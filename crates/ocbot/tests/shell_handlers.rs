//! The shell handlers and ops loop that had no direct test: `/status`, `/mute`,
//! Discard, the `/release` list→confirm→release round-trip, and `ops::tick`
//! (reachability + derived gauges + stall wiring).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::sync::Arc;

use common::{
    FakeClock, FakeDiscord, FakeGit, FakeOpenCode, context_with, seed_reported, seed_running,
};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::handlers::{discard, mute, release, status};
use ocbot::app::interaction::{InteractionSource, SourceKind};
use ocbot::app::ops;
use ocbot_core::ports::OpenCodePort;
use ocbot_core::{Column, State, WriteValue, dir_hash};

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

fn slash(name: &str) -> InteractionSource {
    InteractionSource {
        kind: SourceKind::SlashCommand {
            name: name.to_owned(),
        },
        user_id: OPERATOR,
        channel_id: CHANNEL,
        guild_id: Some(1),
        message_id: None,
        is_dm: false,
    }
}

fn ctx_for(
    store: &StoreHandle,
    opencode: FakeOpenCode,
    discord: &FakeDiscord,
    now: i64,
) -> Arc<ocbot::app::context::AppContext> {
    let opencode_port: Arc<dyn OpenCodePort> = Arc::new(opencode);
    context_with(
        store.clone(),
        opencode_port,
        Arc::new(FakeGit::inert()),
        Arc::new(FakeClock::at(now)),
        None,
        common::DEFAULT_ABORT_TIMEOUT_SECS,
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    )
}

#[tokio::test]
async fn status_posts_a_digest_with_the_running_action() {
    let store = StoreHandle::open_in_memory().expect("store");
    seed_running(
        &store,
        "/tmp/status",
        Some("main"),
        Some(&"a".repeat(40)),
        CHANNEL,
        "digest me",
        "ses_status",
    )
    .await;
    let discord = FakeDiscord::new();
    let ctx = ctx_for(&store, FakeOpenCode::new(), &discord, 1_000);

    status::execute(&ctx, &slash("status"))
        .await
        .expect("status");
    let content = discord.last_post().expect("status posted").message.content;
    assert!(content.contains("ocbot status"), "{content}");
    assert!(content.contains("server: up"), "{content}");
    assert!(content.contains("running actions: 1"), "{content}");
    assert!(content.contains("digest me"), "{content}");
    assert!(content.contains("held locks: 1"), "{content}");
}

#[tokio::test]
async fn mute_toggles_inbox_reporting_for_the_current_directory() {
    let store = StoreHandle::open_in_memory().expect("store");
    store.set_ui_cwd(OPERATOR, "/tmp/mute").await.expect("cwd");
    let discord = FakeDiscord::new();
    let ctx = ctx_for(&store, FakeOpenCode::new(), &discord, 1_000);

    mute::execute(&ctx, &slash("mute")).await.expect("mute");
    assert!(
        discord
            .last_post()
            .unwrap()
            .message
            .content
            .contains("now muted"),
        "first toggle mutes"
    );
    assert!(store.is_muted("dir", "/tmp/mute").await.expect("muted"));
    mute::execute(&ctx, &slash("mute")).await.expect("unmute");
    assert!(
        discord
            .last_post()
            .unwrap()
            .message
            .content
            .contains("now unmuted"),
        "second toggle unmutes"
    );
    assert!(!store.is_muted("dir", "/tmp/mute").await.expect("unmuted"));
}

#[tokio::test]
async fn discard_seals_the_attempt_and_releases_the_lock() {
    let store = StoreHandle::open_in_memory().expect("store");
    let seeded = seed_reported(
        &store,
        "/tmp/discard",
        None,
        None,
        CHANNEL,
        "throwaway",
        "prompt",
        "ses_discard",
    )
    .await;
    let discord = FakeDiscord::new();
    let ctx = ctx_for(&store, FakeOpenCode::new(), &discord, 1_000);

    let outcome = discard::execute_discard(&ctx, seeded.action_id, OPERATOR)
        .await
        .expect("discard")
        .expect("ephemeral");
    assert!(outcome.contains("throwaway"), "{outcome}");
    assert_eq!(
        store
            .action(seeded.action_id)
            .await
            .expect("read")
            .expect("row")
            .state,
        State::Discarded
    );
    assert_eq!(store.lock_holder("/tmp/discard").await.expect("lock"), None);
    // A second discard is a replay, not a panic.
    assert!(
        discard::execute_discard(&ctx, seeded.action_id, OPERATOR)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn release_lists_confirms_and_frees_the_lock() {
    let store = StoreHandle::open_in_memory().expect("store");
    let seeded = seed_reported(
        &store,
        "/tmp/release",
        None,
        None,
        CHANNEL,
        "held",
        "prompt",
        "ses_release",
    )
    .await;
    let discord = FakeDiscord::new();
    let ctx = ctx_for(&store, FakeOpenCode::new(), &discord, 1_000);

    release::list(&ctx, &slash("release")).await.expect("list");
    let listed = discord.last_post().expect("list posted").message;
    assert!(
        listed.content.contains("/tmp/release"),
        "{}",
        listed.content
    );
    assert_eq!(listed.components.len(), 1);

    let hash = dir_hash("/tmp/release");
    release::confirm(&ctx, &slash("release"), &hash)
        .await
        .expect("confirm");
    let confirmation = discord.last_post().expect("confirm posted").message;
    assert!(confirmation.content.contains("Release the lock"));
    assert!(
        confirmation
            .components
            .iter()
            .any(|component| component.label == "Confirm release")
    );

    let outcome = release::release(&ctx, &slash("release"), &hash)
        .await
        .expect("release")
        .expect("ephemeral");
    assert!(outcome.contains("released"), "{outcome}");
    assert_eq!(store.lock_holder("/tmp/release").await.expect("lock"), None);
    // Releasing again is already-handled.
    assert!(
        release::release(&ctx, &slash("release"), &hash)
            .await
            .is_err()
    );
    let _ = seeded;
}

async fn set_times(store: &StoreHandle, action_id: i64, started: i64, last: i64) {
    let writes = [
        (Column::StartedAt, WriteValue::Int(started)),
        (Column::LastEventAt, WriteValue::Int(last)),
    ];
    assert!(
        store
            .cas_state(action_id, State::Running, State::Running, &writes)
            .await
            .expect("set times"),
        "set_times CAS must find a running action"
    );
}

#[tokio::test]
async fn ops_tick_refreshes_gauges_and_announces_health() {
    let store = StoreHandle::open_in_memory().expect("store");
    seed_running(
        &store,
        "/tmp/ops",
        Some("main"),
        Some(&"b".repeat(40)),
        CHANNEL,
        "ops",
        "ses_ops",
    )
    .await;
    let opencode = FakeOpenCode::new();
    let discord = FakeDiscord::new();
    let ctx = ctx_for(&store, opencode.clone(), &discord, 1_000);

    ops::tick(&ctx, 45 * 60).await.expect("tick");
    let metrics = ctx.metrics.render_text();
    assert!(metrics.contains("ocbot_locks_held 1"), "{metrics}");
    assert!(discord.posts().is_empty(), "healthy tick posts nothing");

    opencode.set_unreachable(true);
    ops::tick(&ctx, 45 * 60).await.expect("tick down");
    let posts = discord.posts();
    assert_eq!(posts.len(), 1, "one down post");
    assert!(posts[0].message.content.contains("unreachable"));
}

#[tokio::test]
async fn watcher_skips_busy_sessions_and_counts_probe_failures() {
    use ocbot::app::watcher::Watcher;
    use ocbot_core::ports::Activity;

    let store = StoreHandle::open_in_memory().expect("store");
    let busy = seed_running(
        &store,
        "/tmp/watch_busy",
        Some("main"),
        Some(&"d".repeat(40)),
        CHANNEL,
        "busy",
        "ses_busy",
    )
    .await;
    let down = seed_running(
        &store,
        "/tmp/watch_down",
        Some("main"),
        Some(&"e".repeat(40)),
        CHANNEL,
        "down",
        "ses_down",
    )
    .await;
    let opencode = FakeOpenCode::new();
    opencode.set_activity("ses_busy", Activity::Busy);
    opencode.set_unreachable(true);
    let discord = FakeDiscord::new();
    let ctx = ctx_for(&store, opencode, &discord, 1_000);
    let watcher = Watcher::new(ctx.clone(), std::time::Duration::from_millis(10));

    watcher.tick().await.expect("tick");
    assert_eq!(
        store.action(busy.action_id).await.unwrap().unwrap().state,
        State::Running,
        "a busy session is left alone"
    );
    assert_eq!(
        store.action(down.action_id).await.unwrap().unwrap().state,
        State::Running,
        "a probe failure does not finalize"
    );
    assert!(
        ctx.metrics
            .render_text()
            .contains("ocbot_stream_reconnects_total 1"),
        "one reconnect observed"
    );
    assert!(discord.posts().is_empty(), "nothing posted");
}

#[tokio::test]
async fn ops_tick_runs_the_stall_watchdog() {
    const NOW: i64 = 1_000_000;
    let store = StoreHandle::open_in_memory().expect("store");
    let seeded = seed_running(
        &store,
        "/tmp/ops_stall",
        Some("main"),
        Some(&"c".repeat(40)),
        CHANNEL,
        "stalled",
        "ses_stall_ops",
    )
    .await;
    // 46 minutes of silence on the injected clock (threshold 45 min).
    set_times(&store, seeded.action_id, NOW - 2_760, NOW - 2_760).await;
    let discord = FakeDiscord::new();
    let ctx = ctx_for(&store, FakeOpenCode::new(), &discord, NOW);

    ops::tick(&ctx, 45 * 60).await.expect("tick stall");
    let posts = discord.posts();
    assert_eq!(posts.len(), 1, "one stall ping");
    assert!(posts[0].message.content.contains("no activity"));

    // A second tick is silent: one ping per stall episode.
    ops::tick(&ctx, 45 * 60).await.expect("tick again");
    assert_eq!(discord.posts().len(), 1);
}
