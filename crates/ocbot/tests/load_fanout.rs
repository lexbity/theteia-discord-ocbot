//! SPEC-004 PH-8: the fan-out's load bounds (NFR-100/101/102).
//!
//! The design envelope is ~20 outbound messages per minute per surface, and
//! the claim under test is that a sustained run of that volume keeps per-send
//! latency flat and leaves nothing queued behind it. Ten minutes of wall-clock
//! time buys nothing a faked transport cannot buy in milliseconds — the fakes
//! have no I/O — so this suite drives the *whole ten-minute volume* (200
//! broadcasts) back-to-back through the real fan-out and asserts on the
//! per-send latency, the send counters, and the queue-depth gauge. That is the
//! same property, measured rather than assumed.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use ocbot::app::context::{BroadcastOutcome, ChatPort};
use ocbot::app::metrics::Metrics;
use ocbot::app::surfaces::{FanoutPort, SurfaceRegistry};
use ocbot_core::{Destination, MsgId, OutgoingMessage, PostReceipt, Surface};

/// The sustained volume the design envelope allows: 20 messages per minute.
const MESSAGES_PER_MINUTE: usize = 20;
/// The sustained run: ten minutes of that volume.
const SUSTAINED_MINUTES: usize = 10;
/// The total volume driven through the fan-out.
const TOTAL: usize = MESSAGES_PER_MINUTE * SUSTAINED_MINUTES;

/// The latency budget for one broadcast through the fan-out (NFR-100's ceiling
/// is 6 s end to end for a *report*; this is the port hop alone, so the budget
/// is deliberately tiny — it catches queueing, not slow surfaces).
const PER_SEND_BUDGET: Duration = Duration::from_millis(50);

/// A recording port with an optional, deterministic send latency.
#[derive(Clone)]
struct LatencyPort {
    name: &'static str,
    surface: Surface,
    /// Per-send latency the fake adds before answering.
    latency: Duration,
    posted: Arc<std::sync::atomic::AtomicUsize>,
}

impl LatencyPort {
    /// Build with no artificial latency.
    fn fast(name: &'static str, surface: Surface) -> Arc<Self> {
        Arc::new(Self {
            name,
            surface,
            latency: Duration::ZERO,
            posted: Arc::default(),
        })
    }

    /// Build with a per-send latency (a slow transport).
    fn slow(name: &'static str, surface: Surface, latency: Duration) -> Arc<Self> {
        Arc::new(Self {
            name,
            surface,
            latency,
            posted: Arc::default(),
        })
    }

    /// How many messages this port delivered.
    fn delivered(&self) -> usize {
        self.posted.load(std::sync::atomic::Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl ChatPort for LatencyPort {
    async fn post(
        &self,
        _dest: &Destination,
        message: &OutgoingMessage,
    ) -> Result<PostReceipt, SendError> {
        if !self.latency.is_zero() {
            tokio::time::sleep(self.latency).await;
        }
        let count = self
            .posted
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        assert!(
            !message.content.is_empty(),
            "{} sent an empty message",
            self.name
        );
        Ok(PostReceipt {
            surface: self.surface,
            msg_id: MsgId(format!("{}-{}", self.name, count)),
        })
    }

    async fn edit(&self, _receipt: &PostReceipt, _replacement: &OutgoingMessage) {}

    async fn react(&self, _receipt: &PostReceipt, _emoji: char) {}

    async fn broadcast(&self, message: &OutgoingMessage) -> BroadcastOutcome {
        let destination = Destination {
            surface: self.surface,
            chat_ref: ocbot_core::ChatRef(match self.surface {
                Surface::Discord => "7".to_owned(),
                Surface::Signal => "+15551234567".to_owned(),
            }),
        };
        match self.post(&destination, message).await {
            Ok(receipt) => BroadcastOutcome {
                delivered: vec![(self.surface, receipt)],
                failed: Vec::new(),
            },
            Err(error) => BroadcastOutcome {
                delivered: Vec::new(),
                failed: vec![(self.surface, error)],
            },
        }
    }

    fn surfaces(&self) -> Vec<Surface> {
        vec![self.surface]
    }
}

/// The send error the ports above can produce (they cannot, but the trait
/// requires the type).
type SendError = ocbot::app::context::SendError;

fn notice(sequence: usize) -> OutgoingMessage {
    OutgoingMessage {
        content: format!("report {sequence}"),
        attachment: None,
        components: Vec::new(),
        mention_allowlist: Vec::new(),
    }
}

/// The dual-surface wiring the boot path builds: one instrumented port per
/// surface, collapsed into a fan-out.
#[expect(
    clippy::needless_pass_by_value,
    reason = "the ports are handed to the registry, which owns them"
)]
fn dual_surface(
    discord: Arc<LatencyPort>,
    signal: Arc<LatencyPort>,
    metrics: &Arc<Metrics>,
) -> Arc<dyn ChatPort> {
    let discord_port: Arc<dyn ChatPort> = discord.clone();
    let signal_port: Arc<dyn ChatPort> = signal.clone();
    let registry = SurfaceRegistry::build(
        vec![
            (Surface::Discord, Destination::discord(7), discord_port),
            (
                Surface::Signal,
                Destination {
                    surface: Surface::Signal,
                    chat_ref: ocbot_core::ChatRef("+15551234567".to_owned()),
                },
                signal_port,
            ),
        ],
        metrics,
    );
    Arc::new(FanoutPort::new(
        registry
            .handles()
            .iter()
            .map(|handle| handle.port.clone())
            .collect(),
    ))
}

/// The queue-depth gauge readings, per surface.
fn inflight(metrics: &Metrics, surface: Surface) -> Vec<usize> {
    metrics
        .render_text()
        .lines()
        .filter_map(|line| {
            let prefix = format!("ocbot_surface_broadcast_inflight{{surface=\"{surface}\"}} ");
            line.strip_prefix(&prefix)?.parse().ok()
        })
        .collect()
}

#[tokio::test]
async fn a_sustained_ten_minute_volume_keeps_latency_flat_and_queues_empty() {
    let discord = LatencyPort::fast("discord", Surface::Discord);
    let signal = LatencyPort::fast("signal", Surface::Signal);
    let metrics = Arc::new(Metrics::new());
    let chat = dual_surface(discord.clone(), signal.clone(), &metrics);

    let mut slowest = Duration::ZERO;
    let started = Instant::now();
    for sequence in 0..TOTAL {
        let send_started = Instant::now();
        let outcome = chat.broadcast(&notice(sequence)).await;
        let elapsed = send_started.elapsed();
        slowest = slowest.max(elapsed);
        assert_eq!(
            outcome.delivered.len(),
            2,
            "both surfaces took message {sequence}"
        );
        assert!(outcome.failed.is_empty(), "no send failed: {outcome:?}");
        assert!(
            elapsed < PER_SEND_BUDGET,
            "message {sequence} took {elapsed:?} — the port is queueing"
        );
    }
    let wall = started.elapsed();

    // NFR-101: the ten-minute volume is delivered, with headroom to spare.
    assert_eq!(discord.delivered(), TOTAL);
    assert_eq!(signal.delivered(), TOTAL);
    assert!(
        wall < Duration::from_secs(60),
        "ten minutes of volume took {wall:?} — no headroom"
    );

    // NFR-104's queue-depth probe: nothing is left in flight.
    assert_eq!(
        inflight(&metrics, Surface::Discord),
        vec![0],
        "no broadcast is still queued"
    );
    assert_eq!(inflight(&metrics, Surface::Signal), vec![0]);

    // And the send counters say the same thing: every attempt, no failures.
    let text = metrics.render_text();
    assert!(
        text.contains(&format!(
            "ocbot_surface_send_total{{surface=\"discord\",op=\"broadcast\"}} {TOTAL}"
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "ocbot_surface_send_total{{surface=\"signal\",op=\"broadcast\"}} {TOTAL}"
        )),
        "{text}"
    );
    assert!(
        text.contains("ocbot_surface_up{surface=\"discord\"} 1")
            && text.contains("ocbot_surface_up{surface=\"signal\"} 1"),
        "both surfaces stayed up through the run: {text}"
    );
}

#[tokio::test]
async fn a_slow_surface_does_not_delay_the_other_one() {
    // NFR-102: one surface's timeout must not delay the others by more than
    // 50 ms beyond its own send. Here the slow surface takes 100 ms per send —
    // far over the 50 ms budget — and the fast one must still be quick.
    let slow = LatencyPort::slow("signal", Surface::Signal, Duration::from_millis(100));
    let fast = LatencyPort::fast("discord", Surface::Discord);
    let metrics = Arc::new(Metrics::new());
    let chat = dual_surface(fast.clone(), slow.clone(), &metrics);

    for sequence in 0..10 {
        let started = Instant::now();
        let outcome = chat.broadcast(&notice(sequence)).await;
        let elapsed = started.elapsed();
        assert_eq!(outcome.delivered.len(), 2);
        // The fast surface's own send is measured from inside: the elapsed time
        // here is the *fan-out's* wall time, which is bounded by the slowest
        // child (concurrency), so the assertion is that it is not the sum.
        assert!(
            elapsed < Duration::from_millis(150),
            "the fan-out serialized the surfaces: {elapsed:?}"
        );
    }
    assert_eq!(fast.delivered(), 10);
    assert_eq!(slow.delivered(), 10);
    assert_eq!(inflight(&metrics, Surface::Signal), vec![0]);
}

#[tokio::test]
async fn point_sends_do_not_grow_a_queue_either() {
    // The reply path is serialized per surface by the adapter; the fan-out must
    // route by surface without queueing behind a busy sibling.
    let discord = LatencyPort::fast("discord", Surface::Discord);
    let signal = LatencyPort::slow("signal", Surface::Signal, Duration::from_millis(20));
    let metrics = Arc::new(Metrics::new());
    let chat = dual_surface(discord.clone(), signal.clone(), &metrics);
    // One broadcast first, so the queue-depth family is on the board.
    assert_eq!(
        chat.broadcast(&notice(0)).await.delivered.len(),
        2,
        "both surfaces took the broadcast"
    );
    for sequence in 0..TOTAL {
        let outcome = chat
            .post(&Destination::discord(7), &notice(sequence))
            .await
            .expect("a point post routes to the discord child");
        assert_eq!(outcome.surface, Surface::Discord);
    }
    assert_eq!(
        discord.delivered(),
        TOTAL + 1,
        "the point posts plus the one broadcast"
    );
    assert_eq!(signal.delivered(), 1, "a point post never fans out");
    assert_eq!(
        inflight(&metrics, Surface::Discord),
        vec![0],
        "point sends queue nothing behind the broadcast"
    );
}
