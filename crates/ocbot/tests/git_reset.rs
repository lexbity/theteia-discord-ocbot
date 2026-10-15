//! Reset (FR-28) against real repositories: the `superseded/*` branch is
//! created before HEAD moves (AC-8), later actions render orphaned, and dirty /
//! not-live targets are refused.

// Integration tests assert via unwrap/panic; production code remains strict
// (workspace lint floor + G3). See common/mod.rs for the shared harness.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{FakeChat, FakeGit, FakeOpenCode, context, init_repo, write_file};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::handlers::history;
use ocbot::app::interaction::{InteractionSource, SourceKind};
use ocbot_core::ports::GitPort;
use ocbot_core::{Column, State, WriteValue};
use tempfile::TempDir;

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

fn repo_str(repo: &Path) -> String {
    repo.to_string_lossy().into_owned()
}

/// A repo with two commits; returns (path, first, second).
fn two_commit_repo(tmp: &TempDir) -> (std::path::PathBuf, String, String) {
    let repo = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    init_repo(&repo);
    write_file(&repo, "a.txt", "a\n");
    let first = common::commit_all(&repo, "first");
    write_file(&repo, "b.txt", "b\n");
    let second = common::commit_all(&repo, "second");
    (repo, first, second)
}

fn ctx_for(
    store: &StoreHandle,
    repo: &Path,
    discord: &FakeChat,
) -> Arc<ocbot::app::context::AppContext> {
    context(
        store.clone(),
        Arc::new(FakeOpenCode::new()),
        Arc::new(FakeGit::with_repo(Some(repo.to_path_buf()))),
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    )
}

fn history_source() -> InteractionSource {
    InteractionSource {
        kind: SourceKind::SlashCommand {
            name: "history".to_owned(),
        },
        user_id: format!("discord:{OPERATOR}"),
        destination: ocbot_core::Destination::discord(CHANNEL),
        guild_id: Some(1),
        message_id: None,
        is_dm: false,
    }
}

#[tokio::test]
async fn reset_creates_superseded_branch_before_moving_head() {
    let tmp = TempDir::new().expect("tmp");
    let (repo, first, second) = two_commit_repo(&tmp);
    let store = StoreHandle::open_in_memory().expect("store");
    let discord = FakeChat::new();
    let ctx = ctx_for(&store, &repo, &discord);

    let outcome = ocbot::app::handlers::reset::begin_reset(
        &ctx,
        &repo_str(&repo),
        &first,
        &format!("discord:{OPERATOR}"),
        Some(ocbot_core::Destination::discord(CHANNEL)),
    )
    .await
    .expect("reset");
    assert!(outcome.unwrap_or_default().contains("reset"));

    assert_eq!(
        ocbot::adapters::git::GitAdapter::new()
            .head(&repo)
            .await
            .expect("head"),
        first,
        "HEAD moved to the target"
    );
    // The safety branch captured the pre-reset tip.
    let superseded = common::git_stdout(
        &repo,
        &[
            "for-each-ref",
            "--format=%(objectname)",
            "refs/heads/superseded/",
        ],
    );
    assert_eq!(superseded, second, "superseded/<ts> captured HEAD first");
    // No intent left behind.
    assert!(
        store
            .pending_reset_intents()
            .await
            .expect("intents")
            .is_empty()
    );
}

#[tokio::test]
async fn reset_refuses_a_dirty_tree() {
    let tmp = TempDir::new().expect("tmp");
    let (repo, first, second) = two_commit_repo(&tmp);
    write_file(&repo, "dirty.txt", "d\n");
    let store = StoreHandle::open_in_memory().expect("store");
    let discord = FakeChat::new();
    let ctx = ctx_for(&store, &repo, &discord);

    let error = ocbot::app::handlers::reset::begin_reset(
        &ctx,
        &repo_str(&repo),
        &first,
        &format!("discord:{OPERATOR}"),
        Some(ocbot_core::Destination::discord(CHANNEL)),
    )
    .await
    .expect_err("dirty tree refused");
    assert!(error.to_string().contains("dirty"), "error: {error}");
    assert_eq!(
        common::git_stdout(&repo, &["rev-parse", "HEAD"]),
        second,
        "HEAD untouched"
    );
}

#[tokio::test]
async fn reset_refuses_a_non_live_target() {
    let tmp = TempDir::new().expect("tmp");
    let (repo, _first, _second) = two_commit_repo(&tmp);
    let store = StoreHandle::open_in_memory().expect("store");
    let discord = FakeChat::new();
    let ctx = ctx_for(&store, &repo, &discord);

    let error = ocbot::app::handlers::reset::begin_reset(
        &ctx,
        &repo_str(&repo),
        "0123456789abcdef0123456789abcdef01234567",
        &format!("discord:{OPERATOR}"),
        Some(ocbot_core::Destination::discord(CHANNEL)),
    )
    .await
    .expect_err("non-live target refused");
    assert!(error.to_string().contains("live"), "error: {error}");
}

#[tokio::test]
async fn actions_above_a_reset_render_orphaned() {
    let tmp = TempDir::new().expect("tmp");
    let (repo, first, second) = two_commit_repo(&tmp);
    let store = StoreHandle::open_in_memory().expect("store");

    // An accepted action whose commit is the pre-reset tip.
    store
        .upsert_project(&repo_str(&repo), Some("main"), Some(&first))
        .await
        .expect("project");
    let run = store
        .insert_run(&repo_str(&repo), "free")
        .await
        .expect("run");
    let action = store
        .insert_action(
            run,
            Some(&CHANNEL.to_string()),
            "accepted work",
            "prompt",
            1,
        )
        .await
        .expect("action");
    assert!(
        store
            .begin_send(action, &repo_str(&repo), "ses_1", Some(&second), false)
            .await
            .expect("send")
    );
    assert!(
        store
            .cas_state(action, State::Running, State::Reported, &[])
            .await
            .expect("reported")
    );
    assert!(
        store
            .cas_state(
                action,
                State::Reported,
                State::Accepted,
                &[(Column::AcceptSha, WriteValue::Text(second.clone()))],
            )
            .await
            .expect("accepted")
    );
    store
        .release_lock(&repo_str(&repo), &action.to_string())
        .await
        .expect("release");

    let discord = FakeChat::new();
    let ctx = ctx_for(&store, &repo, &discord);
    ocbot::app::handlers::reset::begin_reset(
        &ctx,
        &repo_str(&repo),
        &first,
        &format!("discord:{OPERATOR}"),
        Some(ocbot_core::Destination::discord(CHANNEL)),
    )
    .await
    .expect("reset");

    // History now shows the accepted action as orphaned (AC-8).
    store
        .set_ui_cwd(&format!("discord:{OPERATOR}"), &repo_str(&repo))
        .await
        .expect("cwd");
    history::execute(&ctx, &history_source(), false)
        .await
        .expect("history");
    let posted = discord.last_post().expect("history posted");
    assert!(
        posted.message.content.contains("orphaned"),
        "orphaned row rendered: {}",
        posted.message.content
    );
    assert!(posted.message.content.contains("accepted work"));
}
