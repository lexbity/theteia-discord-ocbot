//! The wake dispatcher battery (SPEC-003 W4): storm shedding (AC-E3/NFR-16),
//! store-first dispatch for untracked sessions (FR-45), child→root routing
//! (FR-46), garbage frames never read (FR-44), bounded settle re-reads
//! (AC-E6), and disabled-mode gating.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{FakeChat, FakeGit, FakeOpenCode, seed_running, session_frame};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::wake::Dispatcher;
use ocbot_core::ports::PortSession;

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 999;

/// A running dispatcher over a real store and fake ports; the supervisor is
/// not part of these tests — frames go straight to the dispatcher channel.
struct Dispatch {
    opencode: FakeOpenCode,
    ctx: Arc<ocbot::app::context::AppContext>,
    tx: tokio::sync::mpsc::Sender<opencode_v2_client::EventFrame>,
    task: tokio::task::JoinHandle<()>,
}

impl Dispatch {
    /// Start a dispatcher with a scripted server session list; also returns
    /// the retained fake port and the frame sender the tests ring through.
    fn start_with_sessions(
        sessions: Vec<PortSession>,
    ) -> (
        Self,
        FakeOpenCode,
        tokio::sync::mpsc::Sender<opencode_v2_client::EventFrame>,
    ) {
        let discord = FakeChat::new();
        let opencode = FakeOpenCode::new();
        opencode.set_sessions(sessions);
        let store = StoreHandle::open_in_memory().expect("store");
        let ctx = common::context(
            store,
            Arc::new(opencode.clone()),
            Arc::new(FakeGit::inert()),
            Arc::new(discord),
            OPERATOR,
            Some(CHANNEL),
        );
        let (tx, rx) = tokio::sync::mpsc::channel(1024);
        let cfg = ocbot::config::EventsConfig::spec_defaults();
        let task = Dispatcher::spawn_if_enabled(ctx.clone(), &cfg, rx).expect("dispatcher");
        let dispatch = Self {
            opencode: opencode.clone(),
            ctx,
            tx: tx.clone(),
            task,
        };
        (dispatch, opencode, tx)
    }

    /// Steps of paused time, yielding real time to the store actor.
    async fn steps(&self, count: usize) {
        for _ in 0..count {
            tokio::time::sleep(Duration::from_millis(1100)).await;
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn activity_reads(&self, session: &str) -> usize {
        self.opencode
            .inner()
            .calls
            .iter()
            .filter(|call| matches!(call, common::Call::Activity(id) if id == session))
            .count()
    }

    fn list_reads(&self) -> usize {
        self.opencode
            .inner()
            .calls
            .iter()
            .filter(|call| matches!(call, common::Call::ListSessions(_)))
            .count()
    }
}

/// AC-E3/NFR-16: a 1 000-frame storm on one tracked session in 10 s ⇒ at
/// most 20 session reads (the policy's burst ceiling), no panic.
#[tokio::test(start_paused = true)]
async fn a_thousand_frame_storm_is_shed_to_the_burst_ceiling() {
    let (dispatch, _opencode, _tx) = Dispatch::start_with_sessions(vec![]);
    seed_running(
        &dispatch.ctx.store,
        "/tmp/storm",
        None,
        None,
        CHANNEL,
        "storm",
        "ses_storm",
    )
    .await;
    dispatch
        .opencode
        .set_activity("ses_storm", ocbot_core::ports::Activity::Busy);

    for _ in 0..1_000 {
        let _ = dispatch.tx.send(session_frame("ses_storm")).await;
    }
    // Ten seconds of paused time: enough flush ticks to drain the policy.
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(1100)).await;
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(5));
    }
    let reads = dispatch.activity_reads("ses_storm");
    assert!(
        (1..=20).contains(&reads),
        "the storm coalesces to at least one read and at most the burst \
         ceiling ({reads} reads)"
    );
    // A tracked hit never marks the list dirty: no list reads at all.
    assert_eq!(
        dispatch.list_reads(),
        0,
        "tracked hits cost zero list reads"
    );
    dispatch.task.abort();
}

/// FR-45: an untracked session's wake costs zero session reads and exactly
/// one list read (P-27's dirty-mark path).
#[tokio::test(start_paused = true)]
async fn an_untracked_session_costs_one_list_read_not_session_reads() {
    let (dispatch, _opencode, _tx) = Dispatch::start_with_sessions(vec![]);
    let frame = session_frame("ses_foreign");
    let _ = dispatch.tx.send(frame.clone()).await;
    let _ = dispatch.tx.send(frame).await;
    dispatch.steps(4).await;
    assert_eq!(
        dispatch.activity_reads("ses_foreign"),
        0,
        "untracked ⇒ no session-scoped read (FR-45)"
    );
    // Exactly two list reads: the parent-chain discovery of the store miss,
    // plus the dirty-marked diff (P-27's one list read — not more).
    assert_eq!(
        dispatch.list_reads(),
        2,
        "the dirty-marked diff fired exactly once"
    );
    dispatch.task.abort();
}

/// FR-46: a child-session frame resolves through the parent chain and reads
/// the ROOT; the child is never read independently.
#[tokio::test(start_paused = true)]
async fn a_child_session_frame_reads_the_root() {
    let root = PortSession {
        id: "ses_root".to_owned(),
        parent_id: None,
        title: Some("root".to_owned()),
        directory: None,
    };
    let child = PortSession {
        id: "ses_child".to_owned(),
        parent_id: Some("ses_root".to_owned()),
        title: None,
        directory: None,
    };
    let (dispatch, opencode, tx) = Dispatch::start_with_sessions(vec![root, child]);
    seed_running(
        &dispatch.ctx.store,
        "/tmp/child",
        None,
        None,
        CHANNEL,
        "child",
        "ses_root",
    )
    .await;
    opencode.set_activity("ses_root", ocbot_core::ports::Activity::Busy);

    let _ = tx.send(session_frame("ses_child")).await;
    for _ in 0..4 {
        tokio::time::sleep(Duration::from_millis(1100)).await;
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        dispatch.activity_reads("ses_root"),
        1,
        "the child's wake read the root"
    );
    assert_eq!(
        dispatch.activity_reads("ses_child"),
        0,
        "the child is never read independently (FR-46)"
    );
    dispatch.task.abort();
}

/// FR-44: garbage (unmappable) frames cause zero reads.
#[tokio::test(start_paused = true)]
async fn garbage_frames_never_cause_reads() {
    let (dispatch, _opencode, _tx) = Dispatch::start_with_sessions(vec![]);
    seed_running(
        &dispatch.ctx.store,
        "/tmp/garbage",
        None,
        None,
        CHANNEL,
        "g",
        "ses_g",
    )
    .await;
    for kind in [
        "server.connected",
        "project.updated",
        "mystery.future.kind",
        "session.",
    ] {
        let _ = dispatch
            .tx
            .send(opencode_v2_client::EventFrame {
                event: kind.to_owned(),
                session_id: Some("ses_g".to_owned()),
            })
            .await;
    }
    dispatch.steps(4).await;
    assert_eq!(dispatch.activity_reads("ses_g"), 0, "garbage never reads");
    assert_eq!(dispatch.list_reads(), 0);
    dispatch.task.abort();
}

/// AC-E6: an idle-class hint whose read says busy re-reads at most
/// `settle_max_tries` times, then releases the key (no hot loop).
#[tokio::test(start_paused = true)]
async fn a_contradiction_settles_at_most_twice_then_releases() {
    let (dispatch, _opencode, _tx) = Dispatch::start_with_sessions(vec![]);
    seed_running(
        &dispatch.ctx.store,
        "/tmp/flip",
        None,
        None,
        CHANNEL,
        "flip",
        "ses_flip",
    )
    .await;
    // The server stays busy the whole time: every settle re-read contradicts.
    dispatch
        .opencode
        .set_activity("ses_flip", ocbot_core::ports::Activity::Busy);

    let _ = dispatch
        .tx
        .send(opencode_v2_client::EventFrame {
            event: "session.execution.succeeded".to_owned(),
            session_id: Some("ses_flip".to_owned()),
        })
        .await;
    // Settle budget: 2 re-reads × settle (750 ms) + debounce, with margin.
    dispatch.steps(15).await;

    let reads = dispatch.activity_reads("ses_flip");
    assert!(
        (1..=3).contains(&reads),
        "initial read + at most 2 settle re-reads, got {reads}"
    );
    // The key was released: a long quiet wait produces no further reads.
    let settled = dispatch.activity_reads("ses_flip");
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(1100)).await;
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        dispatch.activity_reads("ses_flip"),
        settled,
        "the key is released to the fallback cadence"
    );
    dispatch.task.abort();
}

/// Disabled mode: the dispatcher task does not exist (AC-E1 discipline).
#[tokio::test]
async fn a_disabled_dispatcher_never_spawns() {
    let events = ocbot::config::EventsConfig {
        enabled: false,
        ..ocbot::config::EventsConfig::spec_defaults()
    };
    let (_tx2, rx) = tokio::sync::mpsc::channel(8);
    assert!(
        Dispatcher::spawn_if_enabled(
            common::context(
                StoreHandle::open_in_memory().expect("store"),
                Arc::new(FakeOpenCode::new()),
                Arc::new(FakeGit::inert()),
                Arc::new(FakeChat::new()),
                OPERATOR,
                None,
            ),
            &events,
            rx,
        )
        .is_none(),
        "pure-poll mode spawns no dispatcher"
    );
}
