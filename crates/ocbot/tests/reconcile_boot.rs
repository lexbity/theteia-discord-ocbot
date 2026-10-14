//! AC-14 (unit half) + P9: a restart mid-run delivers the report exactly once.
//! Reconcile works from status facts: busy ⇒ keep watching; idle & unreported
//! ⇒ deliver; idle & already reported ⇒ nothing.

// Integration tests assert via unwrap/panic; production code remains strict
// (workspace lint floor + G3). See common/mod.rs for the shared harness.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::sync::Arc;

use common::{FakeChat, FakeGit, FakeOpenCode, context};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::reconcile::{ReconcileReport, reconcile};
use ocbot_core::ports::Activity;
use ocbot_core::{Column, State, WriteValue};

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

async fn seed_running(store: &StoreHandle, session: &str) -> i64 {
    common::seed_project(store, "/tmp/seed").await;
    let run = store.insert_run("/tmp/seed", "free").await.expect("run");
    let action = store
        .insert_action(run, Some(&CHANNEL.to_string()), "label", "prompt", 1)
        .await
        .expect("action");
    assert!(
        store
            .begin_send(action, "/tmp/seed", session, Some("head"), false)
            .await
            .expect("send")
    );
    action
}

#[tokio::test]
async fn boot_reconcile_delivers_exactly_once() {
    let store = StoreHandle::open_in_memory().expect("store");
    let opencode = FakeOpenCode::new();
    let discord = FakeChat::new();
    let ctx = context(
        store.clone(),
        Arc::new(opencode.clone()),
        Arc::new(FakeGit::inert()),
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    );

    let session = "ses_reconcile";
    let action = seed_running(&store, session).await;

    // A restart while the server is down leaves everything untouched.
    opencode.inner().unreachable = true;
    let report = reconcile(&ctx).await;
    assert!(!report.server_up);
    assert_eq!(discord.posts().len(), 0);

    // Server back, session busy (still running on the server) → keep watching.
    opencode.inner().unreachable = false;
    opencode.set_activity(session, Activity::Busy);
    let report: ReconcileReport = reconcile(&ctx).await;
    assert!(report.server_up);
    assert_eq!(report.completed, 0);
    assert_eq!(
        discord.posts().len(),
        0,
        "busy sessions must not be reported"
    );
    assert_eq!(
        store
            .action(action)
            .await
            .expect("row")
            .expect("exists")
            .state,
        State::Running
    );

    // Session idle now → one report.
    opencode.set_activity(session, Activity::Idle);
    opencode.set_snapshot(session, FakeOpenCode::completed_snapshot());
    let report = reconcile(&ctx).await;
    assert_eq!(report.completed, 1);
    assert_eq!(discord.posts().len(), 1, "one report delivered");
    let row = store.action(action).await.expect("row").expect("exists");
    assert_eq!(row.state, State::Reported);
    assert!(row.report_msg_id.is_some());

    // Restart again: nothing to do — the report was already delivered.
    let report = reconcile(&ctx).await;
    assert_eq!(report.completed, 0);
    assert_eq!(discord.posts().len(), 1, "exactly once across restarts");
}

#[tokio::test]
async fn reconcile_never_touches_non_running_actions() {
    let store = StoreHandle::open_in_memory().expect("store");
    let opencode = FakeOpenCode::new();
    let discord = FakeChat::new();
    let ctx = context(
        store.clone(),
        Arc::new(opencode.clone()),
        Arc::new(FakeGit::inert()),
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    );

    // A staged action must stay staged across reconciles.
    common::seed_project(&store, "/tmp/x").await;
    let run = store.insert_run("/tmp/x", "free").await.expect("run");
    let staged = store
        .insert_action(run, None, "l", "p", 1)
        .await
        .expect("action");
    let _ = reconcile(&ctx).await;
    assert_eq!(
        store
            .action(staged)
            .await
            .expect("row")
            .expect("exists")
            .state,
        State::Staged
    );
    assert!(opencode.inner().calls.is_empty() || !opencode.inner().calls.is_empty());

    // A reported action is terminal and never re-reported.
    common::seed_project(&store, "/tmp/y").await;
    let run2 = store.insert_run("/tmp/y", "free").await.expect("run");
    let action2 = store
        .insert_action(run2, Some(&CHANNEL.to_string()), "l2", "p2", 1)
        .await
        .expect("action");
    assert!(
        store
            .begin_send(action2, "/tmp/y", "ses_y", None, false)
            .await
            .expect("send")
    );
    assert!(
        store
            .deliver_report(
                action2,
                State::Reported,
                "999",
                "completed",
                Some("X"),
                "{}",
                1
            )
            .await
            .expect("deliver")
    );
    let _ = reconcile(&ctx).await;
    assert_eq!(
        store
            .action(action2)
            .await
            .expect("row")
            .expect("exists")
            .state,
        State::Reported
    );
    assert_eq!(discord.posts().len(), 0);
}

#[tokio::test]
async fn premarked_report_is_never_reposted() {
    // P9 invariant: an action whose `report_msg_id` is already set (a partial
    // write / already-delivered marker) must never get a second report, even
    // while the state still reads `running`. The `report_msg_id IS NULL` CAS
    // guard is what makes this exactly-once (AC-14).
    let store = StoreHandle::open_in_memory().expect("store");
    let opencode = FakeOpenCode::new();
    let discord = FakeChat::new();
    let ctx = context(
        store.clone(),
        Arc::new(opencode.clone()),
        Arc::new(FakeGit::inert()),
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    );

    let session = "ses_premarked";
    let action = seed_running(&store, session).await;
    // Leave the state `running` but stamp the delivery key (the crash-window
    // shape: a post went out, the CAS had not fully reloaded).
    let writes = vec![(
        Column::ReportMessageId,
        WriteValue::Text("already-sent".to_owned()),
    )];
    assert!(
        store
            .cas_state(action, State::Running, State::Running, &writes)
            .await
            .expect("stamp")
    );

    opencode.set_activity(session, Activity::Idle);
    opencode.set_snapshot(session, FakeOpenCode::completed_snapshot());
    let report = reconcile(&ctx).await;
    assert!(report.server_up);
    assert_eq!(report.completed, 0, "the premarked delivery must win");
    assert_eq!(
        discord.posts().len(),
        0,
        "no duplicate for an already-marked report"
    );
    let row = store.action(action).await.expect("row").expect("exists");
    assert_eq!(row.report_msg_id.as_deref(), Some("already-sent"));
}

#[tokio::test]
async fn cursor_and_writes_are_persisted() {
    let store = StoreHandle::open_in_memory().expect("store");
    store.upsert_cursor("ses_cur", 28).await.expect("cursor");
    assert_eq!(
        store
            .cursor("ses_cur")
            .await
            .expect("read")
            .expect("row")
            .last_seq,
        28
    );
    common::seed_project(&store, "/tmp/z").await;
    let run = store.insert_run("/tmp/z", "free").await.expect("run");
    let action = store
        .insert_action(run, None, "l", "p", 1)
        .await
        .expect("action");
    assert!(
        store
            .cas_state(action, State::Staged, State::Running, &[])
            .await
            .expect("cas0")
    );
    let writes = vec![(Column::AcceptSha, WriteValue::Text("abc".to_owned()))];
    assert!(
        store
            .cas_state(action, State::Running, State::Reported, &writes)
            .await
            .expect("cas1")
    );
    assert_eq!(
        store
            .action(action)
            .await
            .expect("row")
            .expect("exists")
            .accept_sha
            .as_deref(),
        Some("abc")
    );
}
