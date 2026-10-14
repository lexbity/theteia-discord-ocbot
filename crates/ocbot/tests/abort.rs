//! `/abort` (FR-10 / S-8): the CAS to `aborted` happens only after the server
//! confirms idle; an already-idle session is immediately aborted; a still-busy
//! session never gets the CAS (no premature abort).

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
use std::time::Duration;

use common::{FakeChat, FakeGit, FakeOpenCode, context_with, seed_running};
use ocbot::adapters::clock::SystemClock;
use ocbot::adapters::store::StoreHandle;
use ocbot::app::handlers::abort;
use ocbot::app::interaction::{InteractionSource, SourceKind};
use ocbot_core::State;
use ocbot_core::ports::Activity;

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;
const DIR: &str = "/tmp/abort";
const SESSION: &str = "ses_abort";

fn slash_abort() -> InteractionSource {
    InteractionSource {
        kind: SourceKind::SlashCommand {
            name: "abort".to_owned(),
        },
        user_id: format!("discord:{OPERATOR}"),
        channel_id: CHANNEL,
        guild_id: Some(1),
        message_id: None,
        is_dm: false,
    }
}

async fn ctx_for(
    store: &StoreHandle,
    opencode: Arc<dyn ocbot_core::ports::OpenCodePort>,
    discord: &FakeChat,
    abort_timeout: u64,
) -> Arc<ocbot::app::context::AppContext> {
    let ctx = context_with(
        store.clone(),
        opencode,
        Arc::new(FakeGit::inert()),
        Arc::new(SystemClock),
        None,
        abort_timeout,
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    );
    store
        .set_ui_cwd(&format!("discord:{OPERATOR}"), DIR)
        .await
        .expect("cwd");
    ctx
}

#[tokio::test]
async fn abort_is_immediate_when_the_session_is_already_idle() {
    let store = StoreHandle::open_in_memory().expect("store");
    let seeded = seed_running(&store, DIR, Some("main"), None, CHANNEL, "poke", SESSION).await;
    let opencode = FakeOpenCode::new();
    opencode.set_activity(SESSION, Activity::Idle);
    let discord = FakeChat::new();
    let handle = opencode.clone();
    let ctx = ctx_for(&store, Arc::new(opencode), &discord, 30).await;

    abort::execute(&ctx, &slash_abort()).await.expect("abort");
    let row = store
        .action(seeded.action_id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(row.state, State::Aborted, "idle session ⇒ immediate abort");
    assert!(
        handle
            .inner()
            .calls
            .iter()
            .any(|call| matches!(call, common::Call::Interrupt(id) if id == SESSION)),
        "the interrupt reached the server"
    );
    assert!(
        discord
            .posts()
            .iter()
            .any(|post| post.message.content.contains("⏹ Aborted")),
        "an aborted report is posted (FR-15 buttons)"
    );
}

#[tokio::test]
async fn abort_waits_for_the_server_to_confirm_idle() {
    let store = StoreHandle::open_in_memory().expect("store");
    let seeded = seed_running(&store, DIR, Some("main"), None, CHANNEL, "poke", SESSION).await;
    let opencode = FakeOpenCode::new();
    opencode.set_activity(SESSION, Activity::Busy);
    let opencode_clone = opencode.clone();
    // The server confirms idle a little while after the interrupt.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(700)).await;
        opencode_clone.set_activity(SESSION, Activity::Idle);
    });
    let discord = FakeChat::new();
    let handle = opencode.clone();
    let ctx = ctx_for(&store, Arc::new(opencode), &discord, 30).await;

    abort::execute(&ctx, &slash_abort())
        .await
        .expect("abort after idle");
    let row = store
        .action(seeded.action_id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(row.state, State::Aborted);
    assert!(
        handle
            .inner()
            .calls
            .iter()
            .filter(|call| matches!(call, common::Call::Interrupt(id) if id == SESSION))
            .count()
            >= 1,
        "the interrupt reached the server"
    );
}

#[tokio::test]
async fn a_still_busy_session_is_never_prematurely_aborted() {
    let store = StoreHandle::open_in_memory().expect("store");
    let seeded = seed_running(&store, DIR, Some("main"), None, CHANNEL, "poke", SESSION).await;
    let opencode = FakeOpenCode::new();
    opencode.set_activity(SESSION, Activity::Busy); // never goes idle
    let discord = FakeChat::new();
    // A tight abort budget so the test does not wait the full default.
    let ctx = ctx_for(&store, Arc::new(opencode), &discord, 1).await;

    let error = abort::execute(&ctx, &slash_abort())
        .await
        .expect_err("still busy must not CAS");
    assert!(error.to_string().contains("still busy"), "error: {error}");
    let row = store
        .action(seeded.action_id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(
        row.state,
        State::Running,
        "no premature CAS on a busy session"
    );
    assert_eq!(
        discord
            .posts()
            .iter()
            .filter(|p| p.message.content.contains("⏹ Aborted"))
            .count(),
        0
    );
}
