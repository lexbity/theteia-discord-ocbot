//! Stall watchdog (P3/FR-39, fake clock): silence beyond `STALL_MINUTES` posts
//! exactly one ping; a same-`last_event_at` re-scan stays silent; activity
//! resets the detector so a second stall pings again.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::sync::Arc;

use common::{FakeClock, FakeDiscord, FakeGit, FakeOpenCode, context_with, seed_running};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::stall;
use ocbot_core::ports::Clock;
use ocbot_core::{Column, State, WriteValue};

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;
const STALL_SECS: i64 = 45 * 60;

/// Overwrite the action's activity timestamps with explicit clock values.
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
async fn stall_pings_once_then_resets_with_activity() {
    let store = StoreHandle::open_in_memory().expect("store");
    let opencode: Arc<dyn ocbot_core::ports::OpenCodePort> = Arc::new(FakeOpenCode::new());
    let discord = FakeDiscord::new();
    let clock = Arc::new(FakeClock::at(100_000));
    let ctx = context_with(
        store.clone(),
        opencode,
        Arc::new(FakeGit::inert()),
        clock.clone(),
        None,
        common::DEFAULT_ABORT_TIMEOUT_SECS,
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    );
    let seeded = seed_running(
        &store,
        "/tmp/stall",
        Some("main"),
        Some(&"b".repeat(40)),
        CHANNEL,
        "long task",
        "ses_stall",
    )
    .await;
    set_times(&store, seeded.action_id, 100_000, 100_000).await;

    // 44 minutes: not yet stalled.
    clock.advance(44 * 60);
    assert_eq!(stall::scan(&ctx, STALL_SECS).await.expect("scan"), 0);
    assert!(discord.posts().is_empty());

    // Exactly 45 minutes: the boundary pings (kills the `<`/`<=` mutant).
    clock.advance(60);
    assert_eq!(stall::scan(&ctx, STALL_SECS).await.expect("scan"), 1);
    assert_eq!(stall::scan(&ctx, STALL_SECS).await.expect("scan"), 0);
    let posts = discord.posts();
    assert_eq!(posts.len(), 1, "exactly one stall ping");
    assert!(posts[0].message.content.contains("no activity for 45m"));
    let row = store
        .action(seeded.action_id)
        .await
        .expect("read")
        .expect("row");
    assert!(row.stall_ping_at.is_some(), "ping recorded");
    assert_eq!(
        row.state,
        State::Running,
        "the watchdog never changes state"
    );

    // Activity arrives (a newer `last_event_at`), so the detector resets.
    clock.advance(60);
    set_times(&store, seeded.action_id, 100_000, clock.now()).await;
    assert_eq!(stall::scan(&ctx, STALL_SECS).await.expect("scan"), 0);

    // A second stall pings again (one new ping per episode).
    clock.advance(STALL_SECS + 1);
    assert_eq!(stall::scan(&ctx, STALL_SECS).await.expect("scan"), 1);
    assert_eq!(discord.posts().len(), 2, "one ping per stall episode");
}
