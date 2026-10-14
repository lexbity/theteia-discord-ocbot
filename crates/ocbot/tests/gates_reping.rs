//! AC-10: unanswered gates re-ping on the injected clock, stop after six, and
//! end with a "still blocked" post.

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

use common::{
    FakeChat, FakeClock, FakeGit, FakeOpenCode, context_with, permission_gate, seed_running,
};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::gates_loop;

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;
const REPING_SECS: i64 = 600; // PERM_REPING_MINUTES = 10

#[tokio::test]
async fn reping_runs_on_the_cadence_and_stops_after_six() {
    let store = StoreHandle::open_in_memory().expect("store");
    seed_running(
        &store,
        "/tmp/reping",
        Some("main"),
        None,
        CHANNEL,
        "my action",
        "ses_root",
    )
    .await;
    let opencode = FakeOpenCode::new();
    opencode.set_gates(vec![permission_gate(
        "per_reping",
        Some("ses_root"),
        "bash",
        &["ls"],
    )]);
    let discord = FakeChat::new();
    let clock = FakeClock::at(1_000_000);
    let ctx = context_with(
        store.clone(),
        Arc::new(opencode),
        Arc::new(FakeGit::inert()),
        Arc::new(clock.clone()),
        None,
        30,
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    );

    gates_loop::sync(&ctx).await.expect("sync");
    let rowid = store.pending_gates().await.expect("rows")[0].id;
    let posts_after_sync = discord.posts().len();

    // Not due yet: 599 of 600 seconds elapsed.
    clock.advance(599);
    gates_loop::reping(&ctx, REPING_SECS)
        .await
        .expect("reping not due");
    assert_eq!(discord.posts().len(), posts_after_sync, "not due");

    // One more second crosses the cadence: first re-ping.
    clock.advance(1);
    gates_loop::reping(&ctx, REPING_SECS)
        .await
        .expect("first reping");
    assert_eq!(discord.posts().len(), posts_after_sync + 1);
    assert!(
        discord
            .last_post()
            .unwrap()
            .message
            .content
            .contains("still waiting")
    );

    // Five more re-pings on the cadence = six total; the sixth says blocked.
    for expected in 2..=6_i64 {
        clock.advance(REPING_SECS);
        gates_loop::reping(&ctx, REPING_SECS).await.expect("reping");
        assert_eq!(
            discord.posts().len(),
            posts_after_sync + usize::try_from(expected).unwrap(),
            "ping {expected}"
        );
    }
    assert!(
        discord
            .last_post()
            .unwrap()
            .message
            .content
            .contains("still blocked"),
        "sixth ping is the still-blocked post"
    );

    // The counter is exhausted: further time passes with no more posts.
    clock.advance(REPING_SECS);
    gates_loop::reping(&ctx, REPING_SECS)
        .await
        .expect("reping stops");
    assert_eq!(discord.posts().len(), posts_after_sync + 6);
    let row = store.gate(rowid).await.expect("read").expect("row");
    assert_eq!(row.pings, 6);
    assert_eq!(
        row.state, "pending",
        "the gate stays pending until answered"
    );
}
