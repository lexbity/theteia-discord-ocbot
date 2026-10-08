//! FR-24: a child-session gate is shown against the root action — the row
//! records `root_session_id` and the message names the action's label.

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
use ocbot_core::ports::PortSession;

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;
const ROOT: &str = "ses_root";
const CHILD: &str = "ses_child";

#[tokio::test]
async fn child_gates_attribute_to_the_root_action() {
    let store = StoreHandle::open_in_memory().expect("store");
    let action = seed_running(
        &store,
        "/tmp/child",
        Some("main"),
        None,
        CHANNEL,
        "the root label",
        ROOT,
    )
    .await
    .action_id;

    let opencode = FakeOpenCode::new();
    opencode.set_sessions(vec![
        PortSession {
            id: CHILD.to_owned(),
            parent_id: Some(ROOT.to_owned()),
            title: None,
            directory: None,
        },
        PortSession {
            id: ROOT.to_owned(),
            parent_id: None,
            title: None,
            directory: None,
        },
    ]);
    opencode.set_gates(vec![permission_gate(
        "per_child",
        Some(CHILD),
        "bash",
        &["curl https://x"],
    )]);
    let discord = FakeDiscord::new();
    let ctx = context(
        store.clone(),
        Arc::new(opencode),
        Arc::new(FakeGit::inert()),
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    );

    gates_loop::sync(&ctx).await.expect("sync");

    let rows = store.pending_gates().await.expect("rows");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].action_id,
        Some(action),
        "attributed to the root action"
    );
    assert_eq!(rows[0].root_session_id.as_deref(), Some(ROOT));
    assert_eq!(rows[0].session_id.as_deref(), Some(CHILD));
    let posted = discord.last_post().expect("gate message");
    assert!(
        posted.message.content.contains("the root label"),
        "the message names the root action: {}",
        posted.message.content
    );
}
