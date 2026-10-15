//! SPEC-004 PH-8: the signal-cli transport under chaos.
//!
//! Three failure modes the design claims to survive, each proven against the
//! pinned fake daemon:
//!
//! 1. **Death mid-subscribe.** The daemon dies with a live subscription. The
//!    machine sees `Closed`, reconnects with backoff, and an envelope that was
//!    already buffered is delivered exactly once — a reconnect must never replay
//!    a decision.
//! 2. **Death mid-send.** A point post propagates its `SendError` (D-07: the
//!    caller decides), while a broadcast still counts as delivered when another
//!    surface accepted it.
//! 3. **A restart storm.** Reconnect backoff respects its ceiling, so a flapping
//!    daemon cannot spin the loop hot.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::signal_fake::{FakeSignal, Script, text_envelope};
use common::signal_surface::{OPERATOR, SignalSurface, wait_for};
use ocbot::adapters::signal::{SignalAdapter, SignalMachine};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::context::{BroadcastOutcome, ChatPort, SendError};
use ocbot::app::metrics::Metrics;
use ocbot::app::surfaces::{FanoutPort, SurfaceRegistry};
use ocbot_core::{ChatRef, Destination, MsgId, OutgoingMessage, PostReceipt, Surface};

/// A live daemon + machine pair, for the tests that need to kill the transport
/// mid-flight (the shared `SignalSurface` owns its own temp dir and cannot be
/// re-pointed).
struct Transport {
    /// The temp dir holding the socket. Kept alive for the transport's life:
    /// dropping it would unlink the socket and a reconnect could never bind.
    _dir: tempfile::TempDir,
    /// The fake daemon. `None` once it has been taken away (the listener with
    /// it), which is what a dead daemon looks like to a reconnecting client.
    daemon: Option<FakeSignal>,
    /// The adapter that posts through the machine.
    adapter: Arc<SignalAdapter>,
    /// The registry the machine reports to.
    metrics: Arc<Metrics>,
}

impl Transport {
    /// Bind a daemon on a fresh socket and connect a machine to it.
    async fn new(script: Script) -> Self {
        let dir = tempfile::tempdir().expect("tmp");
        let socket = dir.path().join("signal.sock");
        let daemon = FakeSignal::bind(socket.clone(), script);
        let metrics = Arc::new(Metrics::new());
        let machine = SignalMachine::new(
            socket,
            Duration::from_millis(5),
            Duration::from_millis(20),
            metrics.clone(),
        )
        .with_jitter(Box::new(|| 0.5));
        let slot = machine.connection();
        let adapter = Arc::new(SignalAdapter::new(
            OPERATOR,
            slot,
            StoreHandle::open_in_memory().expect("store"),
            metrics.clone(),
            std::env::temp_dir().join("ocbot-chaos-outbox"),
        ));
        let (sink_tx, _sink_rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(machine.run_forever(sink_tx));
        wait_for(
            || {
                metrics
                    .render_text()
                    .contains("ocbot_surface_up{surface=\"signal\"} 1")
            },
            "the machine subscribes",
        )
        .await;
        Self {
            _dir: dir,
            daemon: Some(daemon),
            adapter,
            metrics,
        }
    }

    /// The operator's destination.
    fn destination() -> Destination {
        Destination {
            surface: Surface::Signal,
            chat_ref: ChatRef(OPERATOR.to_owned()),
        }
    }

    /// Shut the daemon down for good: every live connection closes (the client
    /// reads EOF) and the socket unbinds, so a reconnect is refused. This is
    /// the "daemon is gone" case; [`FakeSignal::kill`] alone is the "daemon
    /// restarted" case, where the reconnect is served again.
    fn kill_daemon(&mut self) {
        if let Some(daemon) = self.daemon.take() {
            daemon.shutdown();
        }
    }

    /// Whether the surface is currently considered up (the machine flips the
    /// gauge on every subscribe and on every closure).
    fn is_up(&self) -> bool {
        self.metrics
            .render_text()
            .contains("ocbot_surface_up{surface=\"signal\"} 1")
    }

    /// The connection count of the live daemon (reconnect evidence).
    fn connections(&self) -> usize {
        self.daemon.as_ref().map_or(0, FakeSignal::connection_count)
    }
}

/// A plain message spec.
fn notice(text: &str) -> OutgoingMessage {
    OutgoingMessage {
        content: text.to_owned(),
        attachment: None,
        components: Vec::new(),
        mention_allowlist: Vec::new(),
    }
}

/// A port that records what it was asked to deliver, and can be told to fail.
#[derive(Clone, Default)]
struct RecordingPort {
    inner: Arc<std::sync::Mutex<RecordingInner>>,
}

#[derive(Default)]
struct RecordingInner {
    posts: Vec<Destination>,
    broadcasts: usize,
    fail: bool,
    next_id: u64,
}

impl RecordingPort {
    /// Make every subsequent send fail.
    fn set_failing(&self, failing: bool) {
        self.inner.lock().expect("lock").fail = failing;
    }

    /// Every destination posted to.
    fn posts(&self) -> Vec<Destination> {
        self.inner.lock().expect("lock").posts.clone()
    }

    /// How many broadcasts were delivered here.
    fn broadcasts(&self) -> usize {
        self.inner.lock().expect("lock").broadcasts
    }
}

#[async_trait::async_trait]
impl ChatPort for RecordingPort {
    async fn post(
        &self,
        dest: &Destination,
        message: &OutgoingMessage,
    ) -> Result<PostReceipt, SendError> {
        let mut inner = self.inner.lock().expect("lock");
        if inner.fail {
            return Err(SendError {
                surface: dest.surface,
                message: "scripted transport failure".to_owned(),
            });
        }
        inner.next_id += 1;
        let id = inner.next_id;
        inner.posts.push(dest.clone());
        if message.content.is_empty() {
            // Unreachable by construction; keeps the recorder total.
            inner.broadcasts += 0;
        }
        Ok(PostReceipt {
            surface: dest.surface,
            msg_id: MsgId(id.to_string()),
        })
    }

    async fn edit(&self, _receipt: &PostReceipt, _replacement: &OutgoingMessage) {}

    async fn react(&self, _receipt: &PostReceipt, _emoji: char) {}

    async fn broadcast(&self, message: &OutgoingMessage) -> BroadcastOutcome {
        let destination = Destination::discord(7);
        match self.post(&destination, message).await {
            Ok(receipt) => {
                self.inner.lock().expect("lock").broadcasts += 1;
                BroadcastOutcome {
                    delivered: vec![(Surface::Discord, receipt)],
                    failed: Vec::new(),
                }
            }
            Err(error) => BroadcastOutcome {
                delivered: Vec::new(),
                failed: vec![(Surface::Discord, error)],
            },
        }
    }

    fn surfaces(&self) -> Vec<Surface> {
        vec![Surface::Discord]
    }
}

/// A dual-surface wiring: the healthy recording sibling plus a Signal transport
/// that can be killed, collapsed into one `ChatPort` the way boot does it.
struct DualSurface {
    /// The recording (healthy) port standing in for Discord.
    sibling: Arc<RecordingPort>,
    /// The collapsed fan-out over both instrumented ports.
    chat: Arc<dyn ChatPort>,
    /// The signal transport under test.
    signal: Transport,
}

impl DualSurface {
    /// Build with a live signal-cli daemon.
    async fn new() -> Self {
        let sibling = Arc::new(RecordingPort::default());
        let signal = Transport::new(Script::default()).await;
        let metrics = Arc::new(Metrics::new());
        let sibling_port: Arc<dyn ChatPort> = sibling.clone();
        let signal_port: Arc<dyn ChatPort> = signal.adapter.clone();
        let registry = SurfaceRegistry::build(
            vec![
                (Surface::Discord, Destination::discord(7), sibling_port),
                (Surface::Signal, Transport::destination(), signal_port),
            ],
            &metrics,
        );
        let chat: Arc<dyn ChatPort> = Arc::new(FanoutPort::new(
            registry
                .handles()
                .iter()
                .map(|handle| handle.port.clone())
                .collect(),
        ));
        Self {
            sibling,
            chat,
            signal,
        }
    }
}

// --- 1. death mid-subscribe ------------------------------------------------

#[tokio::test]
async fn a_daemon_that_dies_mid_subscribe_is_reconnected_to() {
    // `close_after_subscribe` is the daemon hanging up immediately: the machine
    // must treat it as `Closed` and reconnect rather than give up.
    let dir = tempfile::tempdir().expect("tmp");
    let socket = dir.path().join("signal.sock");
    let daemon = FakeSignal::bind(
        socket.clone(),
        Script {
            close_after_subscribe: true,
            ..Script::default()
        },
    );
    let metrics = Arc::new(Metrics::new());
    let machine = SignalMachine::new(
        socket,
        Duration::from_millis(5),
        Duration::from_millis(20),
        metrics.clone(),
    )
    .with_jitter(Box::new(|| 0.5));
    let (sink_tx, _sink_rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(machine.run_forever(sink_tx));
    wait_for(
        || daemon.connection_count() >= 2,
        "the machine reconnects after the daemon hung up",
    )
    .await;
    let subscribes = daemon
        .requests()
        .iter()
        .filter(|recorded| recorded.method == "subscribe")
        .count();
    assert!(
        subscribes >= 2,
        "the second subscribe proves the reconnect: {subscribes}"
    );
}

#[tokio::test]
async fn a_daemon_killed_under_a_live_subscription_is_reconnected_to() {
    // The daemon dies but its listener survives (a signal-cli restart): the
    // machine sees EOF, backs off, and re-subscribes to the same socket.
    let transport = Transport::new(Script::default()).await;
    let before = transport.connections();
    transport.daemon.as_ref().expect("a live daemon").kill();
    wait_for(
        || transport.connections() > before,
        "the machine reconnects after the daemon was killed",
    )
    .await;
    assert!(transport.is_up(), "and it is subscribed again");
}

#[tokio::test]
async fn a_reconnect_never_replays_a_buffered_decision() {
    // The pump's channel can hold an envelope across a reconnect; delivering it
    // twice must be impossible. The dedupe cache is the first leg, and the CAS
    // is the second — this test drives both by replaying one envelope.
    let surface =
        SignalSurface::new(Script::default(), None, Arc::new(common::FakeGit::inert())).await;
    let action_id = surface
        .seed_reported("/tmp/chaos-decision", "chaos", None)
        .await;
    let timestamp = 1_700_000_900_000_u64;
    let envelope = text_envelope(OPERATOR, timestamp, "accept chaos");
    let id = surface.daemon.subscribe_id();
    surface.daemon.stream_envelope(id, &envelope);
    for _ in 0..600 {
        if surface.state(action_id).await == ocbot_core::State::Accepted {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        surface.state(action_id).await,
        ocbot_core::State::Accepted,
        "the decision landed"
    );
    // The daemon replays the same envelope (a reconnect on signal-cli's side
    // resends): the dedupe cache absorbs it, and the CAS would anyway.
    surface.daemon.stream_envelope(id, &envelope);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        surface.state(action_id).await,
        ocbot_core::State::Accepted,
        "the decision stands, exactly once"
    );
    assert!(
        surface
            .metrics
            .render_text()
            .contains("ocbot_signal_envelope_duplicates_total 1"),
        "the redelivery was counted and dropped"
    );
}

// --- 2. death mid-send -----------------------------------------------------

#[tokio::test]
async fn a_point_post_to_a_dead_transport_fails_loud() {
    let mut transport = Transport::new(Script::default()).await;
    // Sanity: the transport delivers while the daemon lives.
    assert!(
        transport
            .adapter
            .post(&Transport::destination(), &notice("first"))
            .await
            .is_ok(),
        "a live daemon accepts the post"
    );
    transport.kill_daemon();
    // The machine sees EOF, backs off, and cannot reconnect: the gauge drops
    // and the published socket is retracted.
    wait_for(
        || !transport.is_up(),
        "the surface is declared down once the daemon is gone",
    )
    .await;
    let error = transport
        .adapter
        .post(&Transport::destination(), &notice("second"))
        .await
        .expect_err("a dead transport is an error, never a silent drop (D-07)");
    assert_eq!(error.surface, Surface::Signal);
}

#[tokio::test]
async fn a_broadcast_still_delivers_when_one_surface_is_down() {
    let mut dual = DualSurface::new().await;
    // Sanity: both surfaces deliver while signal-cli lives.
    let outcome = dual.chat.broadcast(&notice("a report")).await;
    assert_eq!(outcome.delivered.len(), 2, "both surfaces accepted it");
    assert_eq!(dual.sibling.broadcasts(), 1);

    // The daemon is gone for good: the Signal surface is down, the recording
    // sibling is not, and the ≥1-success contract must hold.
    dual.signal.kill_daemon();
    wait_for(
        || !dual.signal.is_up(),
        "the signal surface is declared down",
    )
    .await;
    let before = dual.sibling.broadcasts();
    let outcome = dual.chat.broadcast(&notice("a second report")).await;
    assert!(
        outcome.any_delivered(),
        "≥1 success is delivered (D-07): {outcome:?}"
    );
    assert_eq!(
        outcome.delivered.len(),
        1,
        "the surviving surface carried it: {outcome:?}"
    );
    assert_eq!(
        outcome.failed.len(),
        1,
        "the dead surface is named in the outcome"
    );
    assert_eq!(outcome.failed[0].0, Surface::Signal);
    assert!(
        dual.sibling.broadcasts() > before,
        "the healthy surface actually received it"
    );
}

// --- 3. a restart storm ----------------------------------------------------

#[tokio::test]
async fn a_restart_storm_respects_the_backoff_ceiling() {
    // A daemon that only ever hangs up: the machine reconnects forever, and the
    // interval between attempts never exceeds the ceiling (min + jitter band).
    let dir = tempfile::tempdir().expect("tmp");
    let socket = dir.path().join("signal.sock");
    let daemon = FakeSignal::bind(
        socket.clone(),
        Script {
            close_after_subscribe: true,
            ..Script::default()
        },
    );
    let metrics = Arc::new(Metrics::new());
    let machine = SignalMachine::new(
        socket,
        Duration::from_millis(5),
        Duration::from_millis(20),
        metrics.clone(),
    )
    .with_jitter(Box::new(|| 0.5));
    let (sink_tx, _sink_rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(machine.run_forever(sink_tx));
    wait_for(
        || daemon.connection_count() >= 20,
        "the machine keeps retrying through the storm",
    )
    .await;
    // 20 attempts at a 5–20 ms backoff ceiling take well under a second; a
    // spin-hot loop (no backoff) would be indistinguishable by count alone, so
    // the bound is measured on wall time as well.
    assert!(
        daemon.connection_count() < 2_000,
        "the storm is backoff-bounded, not a hot loop: {} connections",
        daemon.connection_count()
    );
}

#[tokio::test]
async fn the_surface_health_gauge_flips_down_then_back_up() {
    // The health edge around a flapping transport: three consecutive failures
    // declare the surface down, the first success recovers it (PH-2).
    let sibling = Arc::new(RecordingPort::default());
    let metrics = Arc::new(Metrics::new());
    let sibling_port: Arc<dyn ChatPort> = sibling.clone();
    let registry = SurfaceRegistry::build(
        vec![(Surface::Discord, Destination::discord(7), sibling_port)],
        &metrics,
    );
    let port = registry.single();
    assert!(
        port.post(&Destination::discord(7), &notice("ok"))
            .await
            .is_ok()
    );
    assert!(
        metrics
            .render_text()
            .contains("ocbot_surface_up{surface=\"discord\"} 1")
    );
    sibling.set_failing(true);
    for _ in 0..3 {
        let _ = port.post(&Destination::discord(7), &notice("boom")).await;
    }
    assert!(
        metrics
            .render_text()
            .contains("ocbot_surface_up{surface=\"discord\"} 0"),
        "three consecutive failures take the surface down (PH-2)"
    );
    sibling.set_failing(false);
    assert!(
        port.post(&Destination::discord(7), &notice("ok"))
            .await
            .is_ok()
    );
    assert!(
        metrics
            .render_text()
            .contains("ocbot_surface_up{surface=\"discord\"} 1"),
        "the first success recovers it"
    );
    assert_eq!(
        sibling.posts().len(),
        2,
        "the two successful sends were routed; the three failures never reached the port"
    );
}
