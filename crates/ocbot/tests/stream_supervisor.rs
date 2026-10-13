//! The event-stream supervisor battery (SPEC-003 W3, tokio paused time):
//! idle-deadline reconnects (AC-E4), coalesced reconnect reconcile (FR-49),
//! threshold-gated announcements (AC-E7), decode-burst drift (FR-51), the
//! pump's drop-count under a full channel (§4.6), and the backoff shape.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use common::{FakeDiscord, FakeEventSource, FakeGit, FakeOpenCode, session_frame};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::supervisor::{Supervisor, SupervisorConfig};

/// A supervisor config tuned for paused-time tests: fast deadlines, tiny
/// backoff, short announce threshold.
fn test_config() -> SupervisorConfig {
    SupervisorConfig {
        idle_deadline: Duration::from_secs(1),
        backoff_min: Duration::from_millis(100),
        backoff_max: Duration::from_millis(300),
        announce_after: Duration::from_secs(5),
        reconcile_min_interval: Duration::from_secs(5),
    }
}

/// [`ctx_with`] with a retained fake opencode port (reconcile assertions).
fn ctx_with_opencode(
    discord: &FakeDiscord,
    opencode: FakeOpenCode,
) -> Arc<ocbot::app::context::AppContext> {
    let store = StoreHandle::open_in_memory().expect("store");
    common::context(
        store,
        Arc::new(opencode),
        Arc::new(FakeGit::inert()),
        Arc::new(discord.clone()),
        42,
        Some(999),
    )
}

/// The number of full reconcile sweeps the fake port has served (the sweep's
/// first hop is always a probe).
fn probe_count(fake: &FakeOpenCode) -> usize {
    fake.inner()
        .calls
        .iter()
        .filter(|call| **call == common::Call::Probe)
        .count()
}

/// Deterministic jitter: exactly the nominal backoff (factor 1.0).
fn no_jitter() -> Box<dyn Fn() -> f64 + Send + Sync> {
    Box::new(|| 0.5)
}

fn live_ticks(ctx: &Arc<ocbot::app::context::AppContext>) -> bool {
    ctx.metrics.render_text().lines().any(|line| {
        line.starts_with("ocbot_stream_live_seconds_total ")
            && line
                .rsplit(' ')
                .next()
                .and_then(|v| v.parse::<f64>().ok())
                .is_some_and(|seconds| seconds >= 0.5)
    })
}

/// Wait (in paused-time steps) until `cond` holds or the budget expires.
/// Each step parks the main task on a timer (the auto-advance drives the
/// supervisor) and then yields **real** time, so the store actor's OS thread
/// can answer its round-trips even with sibling tests pinning the cores.
async fn wait_until(mut cond: impl FnMut() -> bool, budget_steps: usize) -> bool {
    for _ in 0..budget_steps {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(1100)).await;
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(5));
    }
    cond()
}

/// Run the supervisor under paused time; returns the task handle.
fn spawn_supervisor(
    ctx: Arc<ocbot::app::context::AppContext>,
    source: Arc<FakeEventSource>,
    cfg: SupervisorConfig,
) -> tokio::task::JoinHandle<()> {
    // W3 semantics: the supervisor forwards frames into a channel nobody
    // reads here (the W4 dispatcher owns that side in production).
    let (frame_tx, _frame_rx) = tokio::sync::mpsc::channel::<opencode_v2_client::EventFrame>(1024);
    let supervisor = Supervisor::new(ctx, source, cfg, frame_tx).with_jitter(no_jitter());
    tokio::spawn(async move {
        supervisor.run_forever().await;
    })
}

#[tokio::test(start_paused = true)]
async fn silent_source_is_declared_dead_and_reconcile_coalesces() {
    let discord = FakeDiscord::new();
    let opencode = FakeOpenCode::new();
    let ctx = ctx_with_opencode(&discord, opencode.clone());
    let source = Arc::new(FakeEventSource::new());
    let task = spawn_supervisor(ctx, source.clone(), test_config());

    // The first Live fires the (forced) reconcile: the fake port records the
    // sweep's probe.
    assert!(
        wait_until(|| probe_count(&opencode) == 1, 40).await,
        "the first Live fired the reconcile"
    );
    assert_eq!(source.connect_count(), 1, "one connection so far");

    // Silence past the idle deadline: declared dead and reconnected.
    assert!(
        wait_until(|| source.connect_count() >= 2, 40).await,
        "the silent source was reconnected"
    );

    // Coalescing (FR-49): the second Live within reconcile_min_interval MUST
    // NOT fire a second reconcile — the probe count stays at the first sweep.
    tokio::time::sleep(Duration::from_secs(3)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        probe_count(&opencode),
        1,
        "rapid reconnects coalesce into one reconcile"
    );
    // Announcements: flaps far below the threshold never post (AC-E7).
    assert!(
        discord.posts().is_empty(),
        "flaps below announce_after never post"
    );
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn down_past_threshold_posts_once_and_recovery_once() {
    let discord = FakeDiscord::new();
    let opencode = FakeOpenCode::new();
    let ctx = ctx_with_opencode(&discord, opencode.clone());
    let source = Arc::new(FakeEventSource::new());
    let task = spawn_supervisor(ctx, source.clone(), test_config());
    // The stream goes live first (the forced reconcile completes).
    assert!(wait_until(|| probe_count(&opencode) == 1, 40).await);
    assert!(discord.posts().is_empty(), "live: silent");

    // Down past announce_after (5 s): exactly one post.
    assert!(
        wait_until(
            || discord
                .posts()
                .iter()
                .any(|p| p.message.content.contains("down")),
            60
        )
        .await,
        "the threshold produced exactly the down post"
    );
    tokio::time::sleep(Duration::from_secs(6)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        discord.posts().len(),
        1,
        "the down announcement is never repeated"
    );

    // Recovery: push a frame — the next cycle goes Live and posts once.
    source.push_frame(session_frame("ses_a")).await;
    assert!(
        wait_until(
            || discord
                .posts()
                .iter()
                .any(|p| p.message.content.contains("recovered")),
            40
        )
        .await,
        "the recovery posted once"
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    tokio::task::yield_now().await;
    assert_eq!(discord.posts().len(), 2, "exactly one down + one recovery");
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn garbage_burst_triggers_a_drift_reconnect() {
    let discord = FakeDiscord::new();
    let opencode = FakeOpenCode::new();
    let ctx = ctx_with_opencode(&discord, opencode.clone());
    let source = Arc::new(FakeEventSource::new());
    // A long idle deadline: the bursts must be observed by the *live*
    // connection's ticks, not lost to an idle reconnect of a silently
    // starving one.
    let mut cfg = test_config();
    cfg.idle_deadline = Duration::from_secs(60);
    let task = spawn_supervisor(ctx.clone(), source.clone(), cfg);
    // Wait for the live phase to actually tick: the supervisor snapshots the
    // decode-failure base when it enters the live loop, so the bursts must
    // land while it is watching (the probe fires mid-reconcile, long before).
    assert!(wait_until(|| live_ticks(&ctx), 60).await);
    let connects = source.connect_count();

    // A small burst is tolerated: the connection survives (FR-51).
    source.garbage_burst(5);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        source.connect_count(),
        connects,
        "a few garbage frames never reconnect"
    );

    // Past the burst threshold inside the window: drift reconnect. The
    // reason label is written when the backoff is entered, one cycle after
    // the new connection — wait for the metric itself.
    source.garbage_burst(25);
    assert!(
        wait_until(
            || {
                ctx.metrics
                    .render_text()
                    .contains("ocbot_stream_reconnects_total{reason=\"drift\"} 1")
            },
            40
        )
        .await,
        "drift never labeled"
    );
    task.abort();
}

#[tokio::test(start_paused = true)]
async fn frames_keep_the_connection_alive() {
    let discord = FakeDiscord::new();
    let opencode = FakeOpenCode::new();
    let ctx = ctx_with_opencode(&discord, opencode.clone());
    let source = Arc::new(FakeEventSource::new());
    let task = spawn_supervisor(ctx, source.clone(), test_config());
    // The stream goes live first.
    assert!(wait_until(|| probe_count(&opencode) == 1, 40).await);

    // Steady frames across many idle-deadline windows: no reconnect. Each
    // push is followed by a sub-deadline park, so the deadline never fires.
    for _ in 0..8 {
        source.push_frame(session_frame("ses_live")).await;
        tokio::time::sleep(Duration::from_millis(700)).await;
        tokio::task::yield_now().await;
    }
    assert_eq!(source.connect_count(), 1, "live frames hold the connection");
    assert!(discord.posts().is_empty());
    task.abort();
}

/// The pump's drop-count: a receiver that stops reading must not block the
/// pump (§4.6); frames beyond the channel capacity are counted, and the pump
/// stays alive for later readers. Real pump, real bounded channel.
#[tokio::test]
async fn channel_overflow_drops_and_counts_without_blocking() {
    let uri = wiremock_server_with_many_frames(2_000).await;
    let client = opencode_v2_client::Client::new(&uri, "secret").expect("client");
    let metrics = Arc::new(ocbot::app::metrics::Metrics::new());
    let source =
        ocbot::adapters::opencode::OpenCodeEventSource::new(Arc::new(client), metrics.clone());
    let mut handle = ocbot::app::context::EventSource::connect(&source)
        .await
        .expect("connect");

    // Let the pump run ahead of the (never-read) channel: frames beyond the
    // 1024 capacity are dropped and counted, never blocking the pump.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        handle.dropped_frames() > 0,
        "the flood overflowed the channel ({} dropped)",
        handle.dropped_frames()
    );
    let dropped = handle.dropped_frames();

    // The pump is alive: a reader still receives frames afterwards.
    let mut received = 0;
    while received < 5 {
        match tokio::time::timeout(Duration::from_millis(500), handle.next_item()).await {
            Ok(Some(ocbot::app::context::StreamItem::Frame(_))) => received += 1,
            Ok(Some(ocbot::app::context::StreamItem::Closed) | None) | Err(_) => break,
        }
    }
    assert!(received > 0, "the pump survived the flood");
    assert!(
        handle.dropped_frames() >= dropped,
        "drop counting keeps working"
    );
    let text = metrics.render_text();
    assert!(
        text.contains("ocbot_frames_total{kind=\"server.connected\",mapped=\"false\"}"),
        "{text}"
    );
}

/// Serve `n` frames from a wiremock server (the fixture shape, repeated).
async fn wiremock_server_with_many_frames(n: usize) -> String {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let server = MockServer::start().await;
    let mut body = String::new();
    for i in 0..n {
        let _ = writeln!(
            body,
            "data: {{\"type\":\"server.connected\",\"data\":{{}},\"n\":{i}}}\n\n: heartbeat\n\n"
        );
    }
    Mock::given(method("GET"))
        .and(path("/api/event"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    server.uri()
}

#[test]
fn backoff_grows_exponentially_and_caps_at_max_with_jitter_band() {
    use ocbot::app::supervisor::backoff_delay;
    let cfg = SupervisorConfig {
        idle_deadline: Duration::from_secs(1),
        backoff_min: Duration::from_millis(1_000),
        backoff_max: Duration::from_secs(30),
        announce_after: Duration::from_secs(60),
        reconcile_min_interval: Duration::from_secs(5),
    };
    // Identity jitter: exact doubling, capped at max.
    assert_eq!(backoff_delay(&cfg, 0, 0.5), Duration::from_millis(1_000));
    assert_eq!(backoff_delay(&cfg, 1, 0.5), Duration::from_millis(2_000));
    assert_eq!(backoff_delay(&cfg, 2, 0.5), Duration::from_millis(4_000));
    assert_eq!(
        backoff_delay(&cfg, 20, 0.5),
        Duration::from_secs(30),
        "growth caps at backoff_max"
    );
    // The jitter band is exactly ±20 %: factor 0.0 -> 0.8x, factor 1.0 -> 1.2x.
    assert_eq!(backoff_delay(&cfg, 0, 0.0), Duration::from_millis(800));
    assert_eq!(backoff_delay(&cfg, 0, 1.0), Duration::from_millis(1_200));
    // Out-of-band jitter values clamp into the band.
    assert_eq!(backoff_delay(&cfg, 0, 5.0), Duration::from_millis(1_200));
    assert_eq!(backoff_delay(&cfg, 0, -1.0), Duration::from_millis(800));
}

/// `[events].enabled = false` ⇒ no task, zero connection attempts (FR-43).
#[tokio::test(start_paused = true)]
async fn a_disabled_event_source_never_connects() {
    let discord = FakeDiscord::new();
    let ctx = ctx_with_opencode(&discord, FakeOpenCode::new());
    let source = Arc::new(FakeEventSource::new());
    let events = ocbot::config::EventsConfig {
        enabled: false,
        ..ocbot::config::EventsConfig::spec_defaults()
    };
    let (frame_tx, _frame_rx) = tokio::sync::mpsc::channel::<opencode_v2_client::EventFrame>(1024);
    let handle = Supervisor::spawn_if_enabled(ctx, source.clone(), &events, frame_tx);
    assert!(handle.is_none(), "pure-poll mode spawns nothing");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(source.connect_count(), 0, "zero connection attempts");
}
