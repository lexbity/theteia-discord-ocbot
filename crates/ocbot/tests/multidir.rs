//! Multi-directory (FR-31): two directories run concurrently, each session is
//! created with its own `location.directory`.

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

use common::{FakeDiscord, FakeGit, FakeOpenCode, context, init_repo, write_file};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::handlers::run;
use ocbot::app::interaction::{InteractionSource, SourceKind, TextInput};
use ocbot_core::State;
use tempfile::TempDir;

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

fn modal_source(custom_id: &str, values: Vec<(&str, &str)>) -> InteractionSource {
    InteractionSource {
        kind: SourceKind::ModalSubmit {
            custom_id: custom_id.to_owned(),
            values: values
                .into_iter()
                .map(|(id, value)| TextInput {
                    id: id.to_owned(),
                    value: value.to_owned(),
                })
                .collect(),
        },
        user_id: OPERATOR,
        channel_id: CHANNEL,
        guild_id: Some(1),
        message_id: None,
        is_dm: false,
    }
}

fn button_source(custom_id: &str) -> InteractionSource {
    InteractionSource {
        kind: SourceKind::Component {
            custom_id: custom_id.to_owned(),
        },
        user_id: OPERATOR,
        channel_id: CHANNEL,
        guild_id: Some(1),
        message_id: Some(3),
        is_dm: false,
    }
}

fn make_repo(tmp: &TempDir, name: &str) -> (std::path::PathBuf, String) {
    let repo = tmp.path().join(name);
    std::fs::create_dir_all(&repo).expect("mkdir");
    init_repo(&repo);
    write_file(&repo, "README.md", &format!("# {name}\n"));
    common::commit_all(&repo, "initial");
    let text = repo.to_string_lossy().into_owned();
    (repo, text)
}

#[tokio::test]
async fn two_directories_run_in_parallel_with_location_passed() {
    let tmp = TempDir::new().expect("tmp");
    let (_repo_a, dir_a) = make_repo(&tmp, "alpha");
    let (_repo_b, dir_b) = make_repo(&tmp, "beta");

    let store = StoreHandle::open_in_memory().expect("store");
    let opencode = FakeOpenCode::new();
    let discord = FakeDiscord::new();
    let ctx = context(
        store.clone(),
        Arc::new(opencode.clone()),
        Arc::new(FakeGit::inert()),
        Arc::new(discord),
        OPERATOR,
        Some(CHANNEL),
    );

    for dir in [&dir_a, &dir_b] {
        run::execute_newrun(
            &ctx,
            &modal_source("v1:newrun", vec![("directory", dir), ("label", dir)]),
        )
        .await
        .expect("stage");
    }
    let staged = store.staged_actions().await.expect("staged");
    assert_eq!(staged.len(), 2);
    for action in &staged {
        run::execute_send(
            &ctx,
            action.id,
            &button_source(&format!("v1:send:{}", action.id)),
        )
        .await
        .expect("send");
    }

    let running = store.running_actions().await.expect("running");
    assert_eq!(
        running.len(),
        2,
        "both directories are running concurrently"
    );

    // FR-31: each session was created with its own location.directory.
    let created: Vec<String> = opencode.inner().sessions_created.clone();
    assert_eq!(created.len(), 2);
    let created_dirs: Vec<String> = opencode
        .inner()
        .calls
        .iter()
        .filter_map(|call| match call {
            common::Call::CreateSession(dir, _) => Some(dir.clone()),
            _ => None,
        })
        .collect();
    assert!(
        created_dirs.iter().any(|dir| dir == &dir_a),
        "alpha's directory was passed"
    );
    assert!(
        created_dirs.iter().any(|dir| dir == &dir_b),
        "beta's directory was passed"
    );

    // Both actions are running independently.
    assert!(running.iter().all(|action| action.state == State::Running));
}
