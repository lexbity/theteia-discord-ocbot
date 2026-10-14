//! Degraded mode (FR-38): one "server down" post, one recovery post, and no
//! spam across repeated polls or a double flap. Actions are never touched while
//! the server is down.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::sync::Arc;

use common::{FakeChat, FakeClock, FakeGit, FakeOpenCode, context_with, seed_running};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::degraded;
use ocbot_core::State;
use ocbot_core::ports::OpenCodePort;

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

#[tokio::test]
async fn announces_each_transition_once_and_never_touches_actions() {
    let store = StoreHandle::open_in_memory().expect("store");
    let seeded = seed_running(
        &store,
        "/tmp/degraded",
        Some("main"),
        Some(&"c".repeat(40)),
        CHANNEL,
        "work",
        "ses_deg",
    )
    .await;
    let opencode = FakeOpenCode::new();
    let opencode_port: Arc<dyn OpenCodePort> = Arc::new(opencode.clone());
    let discord = FakeChat::new().with_announce(CHANNEL);
    let ctx = context_with(
        store.clone(),
        opencode_port,
        Arc::new(FakeGit::inert()),
        Arc::new(FakeClock::at(1_000)),
        None,
        common::DEFAULT_ABORT_TIMEOUT_SECS,
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    );

    // 1. Boot healthy: silent.
    assert!(!degraded::probe_and_record(&ctx).await.expect("probe"));
    assert!(discord.posts().is_empty(), "healthy boot posts nothing");

    // 2. Down: exactly one post, and repeated polls stay quiet.
    opencode.set_unreachable(true);
    assert!(degraded::probe_and_record(&ctx).await.expect("probe"));
    for _ in 0..3 {
        assert!(!degraded::probe_and_record(&ctx).await.expect("probe"));
    }
    assert_eq!(discord.posts().len(), 1, "one down post");
    assert!(
        discord
            .last_post()
            .unwrap()
            .message
            .content
            .contains("unreachable")
    );

    // The running action was not disturbed by the outage.
    let row = store
        .action(seeded.action_id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(row.state, State::Running);
    assert!(row.report_msg_id.is_none());

    // 3. Recovery: exactly one post, then quiet.
    opencode.set_unreachable(false);
    assert!(degraded::probe_and_record(&ctx).await.expect("probe"));
    assert!(!degraded::probe_and_record(&ctx).await.expect("probe"));
    assert_eq!(discord.posts().len(), 2);
    assert!(
        discord
            .last_post()
            .unwrap()
            .message
            .content
            .contains("reachable")
    );

    // 4. A double flap announces each edge once (no spam).
    for expected in [3_usize, 4, 5, 6] {
        opencode.set_unreachable(expected % 2 == 1);
        assert!(degraded::probe_and_record(&ctx).await.expect("probe"));
        assert_eq!(discord.posts().len(), expected);
    }
    // Repeating the final state is silent.
    assert!(!degraded::probe_and_record(&ctx).await.expect("probe"));
    assert_eq!(discord.posts().len(), 6);
}
