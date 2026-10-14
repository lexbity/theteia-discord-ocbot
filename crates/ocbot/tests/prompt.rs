//! `/prompt` (FR-7): a free-form prompt targets the directory's *active*
//! session (creating one when none), reuses it on the next call, and refuses a
//! busy session (FR-9).

// Integration tests assert via unwrap/panic; production code remains strict
// (workspace lint floor + G3). See common/mod.rs for the shared harness.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{FakeChat, FakeGit, FakeOpenCode, context, init_repo, write_file};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::handlers::prompt;
use ocbot::app::interaction::{InteractionSource, SourceKind, TextInput};
use ocbot_core::ports::Activity;
use tempfile::TempDir;

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

fn prompt_source(dir: &str, text: &str) -> InteractionSource {
    InteractionSource {
        kind: SourceKind::ModalSubmit {
            custom_id: prompt::PROMPT_MODAL.to_owned(),
            values: vec![
                TextInput {
                    id: "directory".to_owned(),
                    value: dir.to_owned(),
                },
                TextInput {
                    id: "prompt".to_owned(),
                    value: text.to_owned(),
                },
            ],
        },
        user_id: format!("discord:{OPERATOR}"),
        channel_id: CHANNEL,
        guild_id: Some(1),
        message_id: None,
        is_dm: false,
    }
}

fn make_repo(tmp: &TempDir) -> (PathBuf, String) {
    let repo = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    init_repo(&repo);
    write_file(&repo, "README.md", "# work\n");
    common::commit_all(&repo, "initial");
    let dir = repo.to_string_lossy().into_owned();
    (repo, dir)
}

fn ctx_for(
    store: &StoreHandle,
    repo: &Path,
    opencode: &FakeOpenCode,
    discord: &FakeChat,
) -> Arc<ocbot::app::context::AppContext> {
    context(
        store.clone(),
        Arc::new(opencode.clone()),
        Arc::new(FakeGit::with_repo(Some(repo.to_path_buf()))),
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    )
}

#[tokio::test]
async fn prompt_creates_and_then_reuses_the_active_session() {
    let tmp = TempDir::new().expect("tmp");
    let (repo, dir) = make_repo(&tmp);
    let store = StoreHandle::open_in_memory().expect("store");
    let opencode = FakeOpenCode::new();
    let discord = FakeChat::new();
    let ctx = ctx_for(&store, &repo, &opencode, &discord);

    prompt::execute_prompt(&ctx, &prompt_source(&dir, "first prompt"))
        .await
        .expect("first prompt");
    let active = store
        .project(&dir)
        .await
        .expect("project")
        .expect("row")
        .active_session_id
        .expect("an active session is created");
    let first = store.running_actions().await.expect("running")[0].id;
    assert_eq!(
        store
            .action(first)
            .await
            .expect("read")
            .expect("row")
            .session_id
            .as_deref(),
        Some(active.as_str())
    );

    // The operator released the directory lock (P5 escape hatch); the active
    // session is still promptable, so the second prompt reuses it — no second
    // session is created.
    store
        .release_lock(&dir, &first.to_string())
        .await
        .expect("release lock");
    prompt::execute_prompt(&ctx, &prompt_source(&dir, "second prompt"))
        .await
        .expect("second prompt");
    assert_eq!(opencode.inner().sessions_created.len(), 1, "session reused");
    let sends: Vec<String> = opencode
        .inner()
        .calls
        .iter()
        .filter_map(|call| match call {
            common::Call::SendPrompt(_, text) => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        sends,
        vec!["first prompt".to_owned(), "second prompt".to_owned()]
    );
    assert_eq!(
        store
            .project(&dir)
            .await
            .expect("project")
            .expect("row")
            .active_session_id
            .as_deref(),
        Some(active.as_str())
    );
}

#[tokio::test]
async fn prompt_refuses_a_busy_active_session() {
    let tmp = TempDir::new().expect("tmp");
    let (_repo, dir) = make_repo(&tmp);
    let store = StoreHandle::open_in_memory().expect("store");
    // A project whose active session is busy (an agent run is in flight in the
    // TUI) and whose directory lock is free — the FR-9 path.
    store
        .upsert_project(&dir, Some("main"), None)
        .await
        .expect("project");
    store
        .set_active_session(&dir, "ses_act")
        .await
        .expect("active session");
    let opencode = FakeOpenCode::new();
    opencode.set_activity("ses_act", Activity::Busy);
    let discord = FakeChat::new();
    let ctx = ctx_for(&store, &PathBuf::from(&dir), &opencode, &discord);

    let outcome = prompt::execute_prompt(&ctx, &prompt_source(&dir, "second"))
        .await
        .expect("busy is a refusal, not an internal error");
    assert!(
        outcome.unwrap_or_default().contains("busy"),
        "FR-9 refusal names the busy session"
    );
    // The refused prompt must not leave a staged action behind.
    assert!(store.staged_actions().await.expect("staged").is_empty());
}
