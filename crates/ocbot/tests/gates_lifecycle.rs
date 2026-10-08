//! The gate lifecycle (spec P4 / FR-20..24 / AC-10 half): upsert idempotence,
//! exactly-once replies, the FR-23 "unknown request" success path, the boot
//! re-POST, and the resolve-elsewhere clear.

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

use common::{FakeDiscord, FakeGit, FakeOpenCode, context, permission_gate, seed_running};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::gates_loop;
use ocbot::app::handlers::gate;

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;
const DIR: &str = "/tmp/gates";
const SESSION: &str = "ses_root";

fn ctx_for(
    store: &StoreHandle,
    opencode: Arc<dyn ocbot_core::ports::OpenCodePort>,
    discord: &FakeDiscord,
) -> Arc<ocbot::app::context::AppContext> {
    context(
        store.clone(),
        opencode,
        Arc::new(FakeGit::inert()),
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    )
}

async fn seed_running_action(store: &StoreHandle) -> i64 {
    seed_running(
        store,
        DIR,
        Some("main"),
        None,
        CHANNEL,
        "my action",
        SESSION,
    )
    .await
    .action_id
}

#[tokio::test]
async fn sync_upserts_idempotently_and_posts_the_gate_message_once() {
    let store = StoreHandle::open_in_memory().expect("store");
    let action = seed_running_action(&store).await;
    let opencode = FakeOpenCode::new();
    opencode.set_gates(vec![permission_gate(
        "per_1",
        Some(SESSION),
        "bash",
        &["echo hi"],
    )]);
    let discord = FakeDiscord::new();
    let ctx = ctx_for(&store, Arc::new(opencode.clone()), &discord);

    gates_loop::sync(&ctx).await.expect("first sync");
    gates_loop::sync(&ctx).await.expect("second sync");

    let rows = store.pending_gates().await.expect("rows");
    assert_eq!(rows.len(), 1, "same fact twice ⇒ one row");
    assert_eq!(rows[0].ext_id.as_deref(), Some("per_1"));
    assert_eq!(rows[0].action_id, Some(action));
    assert_eq!(rows[0].state, "pending");

    let gate_posts = discord
        .posts()
        .iter()
        .filter(|post| post.message.content.contains("permission needed"))
        .count();
    assert_eq!(gate_posts, 1, "the gate message posts exactly once");
    assert!(
        discord
            .posts()
            .iter()
            .flat_map(|post| post.message.components.iter())
            .any(|component| component.custom_id == format!("v1:gate:{}:once", rows[0].id))
    );
    // The blocked substatus is set on the running action (P4).
    assert_eq!(
        store
            .action(action)
            .await
            .expect("read")
            .expect("row")
            .blocked_gate_id,
        Some(rows[0].id)
    );
}

#[tokio::test]
async fn double_reply_commits_exactly_one_post() {
    let store = StoreHandle::open_in_memory().expect("store");
    let action = seed_running_action(&store).await;
    let opencode = FakeOpenCode::new();
    opencode.set_gates(vec![permission_gate(
        "per_2",
        Some(SESSION),
        "bash",
        &["ls"],
    )]);
    let discord = FakeDiscord::new();
    let ctx = ctx_for(&store, Arc::new(opencode.clone()), &discord);
    gates_loop::sync(&ctx).await.expect("sync");

    let rowid = store.pending_gates().await.expect("rows")[0].id;
    for _ in 0..2 {
        let _ = gate::execute_permission_reply(&ctx, rowid, "always", OPERATOR).await;
    }
    let replies = opencode
        .inner()
        .calls
        .iter()
        .filter(|call| matches!(call, common::Call::ReplyGate(_, _, _)))
        .count();
    assert_eq!(replies, 1, "exactly one reply POST (AC-10)");
    let row = store.gate(rowid).await.expect("read").expect("row");
    assert_eq!(row.state, "answered");
    assert_eq!(row.answer.as_deref(), Some("\"always\""));
    assert_eq!(
        store
            .action(action)
            .await
            .expect("read")
            .expect("row")
            .blocked_gate_id,
        None,
        "the blocked substatus clears after an answer"
    );
}

#[tokio::test]
async fn unknown_request_error_is_a_success() {
    let store = StoreHandle::open_in_memory().expect("store");
    seed_running_action(&store).await;
    let opencode = FakeOpenCode::new();
    opencode.set_gates(vec![permission_gate(
        "per_3",
        Some(SESSION),
        "bash",
        &["ls"],
    )]);
    opencode.reply_unknown();
    let discord = FakeDiscord::new();
    let ctx = ctx_for(&store, Arc::new(opencode.clone()), &discord);
    gates_loop::sync(&ctx).await.expect("sync");

    let rowid = store.pending_gates().await.expect("rows")[0].id;
    let outcome = gate::execute_permission_reply(&ctx, rowid, "once", OPERATOR)
        .await
        .expect("404 is success (FR-23)");
    assert!(outcome.unwrap_or_default().contains("answered"));
    assert_eq!(
        store.gate(rowid).await.expect("read").expect("row").state,
        "answered"
    );
}

#[tokio::test]
async fn transport_failures_are_retried() {
    let store = StoreHandle::open_in_memory().expect("store");
    seed_running_action(&store).await;
    let opencode = FakeOpenCode::new();
    opencode.set_gates(vec![permission_gate(
        "per_4",
        Some(SESSION),
        "bash",
        &["ls"],
    )]);
    // Two flaky replies, then success: the ×3 retry must recover without
    // surfacing an error and without a duplicate CAS.
    opencode.fail_next_replies(2);
    let discord = FakeDiscord::new();
    let ctx = ctx_for(&store, Arc::new(opencode.clone()), &discord);
    gates_loop::sync(&ctx).await.expect("sync");

    let rowid = store.pending_gates().await.expect("rows")[0].id;
    gate::execute_permission_reply(&ctx, rowid, "always", OPERATOR)
        .await
        .expect("retry succeeds");
    let replies = opencode
        .inner()
        .calls
        .iter()
        .filter(|call| matches!(call, common::Call::ReplyGate(_, _, _)))
        .count();
    assert_eq!(replies, 3, "two failures + one success");
}

#[tokio::test]
async fn reconcile_reposts_a_locally_answered_remote_pending_gate() {
    let store = StoreHandle::open_in_memory().expect("store");
    seed_running_action(&store).await;
    let opencode = FakeOpenCode::new();
    opencode.set_gates(vec![permission_gate(
        "per_5",
        Some(SESSION),
        "bash",
        &["ls"],
    )]);
    let discord = FakeDiscord::new();
    let ctx = ctx_for(&store, Arc::new(opencode.clone()), &discord);
    gates_loop::sync(&ctx).await.expect("sync");
    let rowid = store.pending_gates().await.expect("rows")[0].id;
    gate::execute_permission_reply(&ctx, rowid, "once", OPERATOR)
        .await
        .expect("reply");
    let reply_call_count = || {
        opencode
            .inner()
            .calls
            .iter()
            .filter(|call| matches!(call, common::Call::ReplyGate(_, _, _)))
            .count()
    };
    let before = reply_call_count();

    // A restart where the remote still lists the gate: boot reconcile re-POSTs
    // the recorded answer (FR-23 idempotent).
    gates_loop::repost_answered(&ctx).await.expect("re-POST");
    assert_eq!(
        reply_call_count(),
        before + 1,
        "one re-POST per boot reconcile"
    );

    // Once the remote stops listing it, the same reconcile pass stops sending.
    opencode.set_gates(Vec::new());
    gates_loop::repost_answered(&ctx)
        .await
        .expect("re-POST resolved");
    assert_eq!(
        reply_call_count(),
        before + 1,
        "no re-POST after remote resolution"
    );
}

#[tokio::test]
async fn a_gate_that_leaves_the_pending_list_is_resolved_elsewhere() {
    let store = StoreHandle::open_in_memory().expect("store");
    let action = seed_running_action(&store).await;
    let opencode = FakeOpenCode::new();
    opencode.set_gates(vec![permission_gate(
        "per_6",
        Some(SESSION),
        "bash",
        &["ls"],
    )]);
    let discord = FakeDiscord::new();
    let ctx = ctx_for(&store, Arc::new(opencode.clone()), &discord);
    gates_loop::sync(&ctx).await.expect("sync");
    let rowid = store.pending_gates().await.expect("rows")[0].id;

    // The operator answered in the TUI: the gate leaves the server's list.
    opencode.set_gates(Vec::new());
    gates_loop::sync(&ctx).await.expect("clear sync");

    let row = store.gate(rowid).await.expect("read").expect("row");
    assert_eq!(row.state, "answered");
    assert_eq!(row.answer.as_deref(), Some("\"resolved-elsewhere\""));
    assert_eq!(
        store
            .action(action)
            .await
            .expect("read")
            .expect("row")
            .blocked_gate_id,
        None,
        "the blocked substatus clears"
    );
}
