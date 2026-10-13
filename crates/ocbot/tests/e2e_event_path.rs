//! The event path end to end (SPEC-003 W4, AC-E2): a fake stream delivers an
//! idle-class frame ×3 — the real supervisor, real dispatcher, real policy,
//! and real store produce **exactly one report** through the unmodified
//! completion path, with exactly one `activity` read per debounced wake, and
//! the detection attributed to the event source.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{FakeDiscord, FakeEventSource, FakeGit, FakeOpenCode, session_frame};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::supervisor::Supervisor;
use ocbot::app::wake::Dispatcher;

/// AC-E2: an idle-class frame ×3 ⇒ one report, one `activity` read.
#[tokio::test(start_paused = true)]
async fn triple_idle_frame_produces_exactly_one_report() {
    let discord = FakeDiscord::new();
    let opencode = FakeOpenCode::new();
    let store = StoreHandle::open_in_memory().expect("store");
    common::seed_running(
        &store,
        "/tmp/event-path",
        None,
        None,
        999,
        "label",
        "ses_e2e",
    )
    .await;
    // Busy at boot: the reconcile sweep must leave the action running (only
    // the doorbell may detect this completion).
    opencode.set_activity("ses_e2e", ocbot_core::ports::Activity::Busy);
    opencode.set_snapshot("ses_e2e", FakeOpenCode::completed_snapshot());
    let ctx = common::context(
        store,
        Arc::new(opencode.clone()),
        Arc::new(FakeGit::inert()),
        Arc::new(discord.clone()),
        42,
        Some(999),
    );
    let source = Arc::new(FakeEventSource::new());
    let cfg = ocbot::config::EventsConfig::spec_defaults();
    // A long idle deadline: no reconnect ⇒ exactly one reconcile (at boot,
    // while the session is busy); only the doorbell may detect this
    // completion.
    let mut supervisor_cfg = ocbot::app::supervisor::SupervisorConfig::from_events(&cfg);
    supervisor_cfg.idle_deadline = Duration::from_secs(600);
    let (frame_tx, frame_rx) =
        tokio::sync::mpsc::channel(ocbot::app::context::FRAME_CHANNEL_CAPACITY);
    let supervisor = Supervisor::new(ctx.clone(), source.clone(), supervisor_cfg, frame_tx)
        .with_jitter(Box::new(|| 0.5));
    let ctx_metrics = ctx.metrics.clone();
    tokio::spawn(async move { supervisor.run_forever().await });
    let dispatch = Dispatcher::spawn_if_enabled(ctx.clone(), &cfg, frame_rx).expect("dispatcher");

    // The supervisor must go live first: the boot reconcile sweeps while the
    // session is still busy, then the doorbell takes over.
    let mut reconciled = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(1100)).await;
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(5));
        reconciled = opencode
            .inner()
            .calls
            .iter()
            .any(|call| matches!(call, common::Call::Activity(id) if id == "ses_e2e"));
        if reconciled {
            break;
        }
    }
    assert!(reconciled, "the boot reconcile completed");
    let calls_at_live = opencode.inner().calls.len();

    // Flip the server fact to idle, then ring: the same idle-class hint
    // three times.
    let mut frame = session_frame("ses_e2e");
    frame.event = "session.execution.succeeded".to_owned();
    source.push_frame(frame.clone()).await;
    source.push_frame(frame.clone()).await;
    source.push_frame(frame).await;

    opencode.set_activity("ses_e2e", ocbot_core::ports::Activity::Idle);

    // One debounced wake ⇒ one report through the real completion path.
    let mut reported = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(1100)).await;
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(5));
        reported = !discord.posts().is_empty();
        if reported {
            break;
        }
    }
    assert!(reported, "the event path delivered the report");

    // Exactly one session read for the debounced wake (AC-E2's bound).
    let activity_reads = opencode
        .inner()
        .calls
        .iter()
        .skip(calls_at_live)
        .filter(|call| matches!(call, common::Call::Activity(id) if id == "ses_e2e"))
        .count();
    assert_eq!(activity_reads, 1, "one activity read per debounced wake");
    assert_eq!(discord.posts().len(), 1, "exactly one report");

    // P-29: the detection is attributed to the event source.
    let text = ctx_metrics.render_text();
    assert!(
        text.contains("ocbot_detection_total{source=\"event\"} 1"),
        "the detection is attributed: {text}"
    );
    dispatch.abort();
}
