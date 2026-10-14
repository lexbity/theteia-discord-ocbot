//! Accept (FR-16/FR-17) and Mark done (FR-26) against real repositories:
//! the commit lands on the work branch, the lock releases, hooks and policy
//! refusals return the action to `reported`, and the modal prefills from the DB.

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
use ocbot::adapters::git::GitAdapter;
use ocbot::adapters::store::StoreHandle;
use ocbot::app::handlers::accept;
use ocbot_core::ports::GitPort;
use ocbot_core::{Column, State, WriteValue};
use tempfile::TempDir;

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

fn repo_str(repo: &Path) -> String {
    repo.to_string_lossy().into_owned()
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

fn make_repo(tmp: &TempDir) -> (std::path::PathBuf, String) {
    let repo = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    init_repo(&repo);
    write_file(&repo, "README.md", "# work\n");
    let base = common::commit_all(&repo, "initial");
    (repo, base)
}

#[tokio::test]
async fn accept_commits_on_the_work_branch_and_releases_the_lock() {
    let tmp = TempDir::new().expect("tmp");
    let (repo, base) = make_repo(&tmp);
    let store = StoreHandle::open_in_memory().expect("store");
    let seeded = common::seed_reported(
        &store,
        &repo_str(&repo),
        Some("main"),
        Some(&base),
        CHANNEL,
        "work",
        "prompt",
        "ses_1",
    )
    .await;
    write_file(&repo, "feature.txt", "feature\n");
    let discord = FakeChat::new();
    let ctx = ctx_for(&store, &repo, &discord);

    let outcome = accept::execute_accept(
        &ctx,
        seeded.action_id,
        "feat: add",
        "body",
        &format!("discord:{OPERATOR}"),
    )
    .await
    .expect("accept");
    assert!(outcome.unwrap_or_default().contains("accepted"));

    let row = store
        .action(seeded.action_id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(row.state, State::Accepted);
    let accept_sha = row.accept_sha.clone().expect("sha recorded");

    let git = GitAdapter::new();
    let log = git.log(&repo, None).await.expect("log");
    assert_eq!(log[0].subject, "feat: add");
    assert_eq!(log[0].sha, accept_sha);
    assert!(git.status_porcelain(&repo).await.expect("clean").is_empty());
    assert_eq!(
        store.lock_holder(&repo_str(&repo)).await.expect("lock"),
        None
    );
}

#[tokio::test]
async fn failed_hook_returns_to_reported_with_prefill_from_the_db() {
    let tmp = TempDir::new().expect("tmp");
    let (repo, base) = make_repo(&tmp);
    // A failing pre-commit hook.
    let hook = repo.join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\necho 'hook says no' >&2\nexit 1\n").expect("write hook");
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&hook).expect("stat").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&hook, permissions).expect("chmod");
    }

    let store = StoreHandle::open_in_memory().expect("store");
    let seeded = common::seed_reported(
        &store,
        &repo_str(&repo),
        Some("main"),
        Some(&base),
        CHANNEL,
        "work",
        "prompt",
        "ses_1",
    )
    .await;
    write_file(&repo, "feature.txt", "feature\n");
    let discord = FakeChat::new();
    let ctx = ctx_for(&store, &repo, &discord);

    let outcome = accept::execute_accept(
        &ctx,
        seeded.action_id,
        "feat: add",
        "body",
        &format!("discord:{OPERATOR}"),
    )
    .await
    .expect("accept");
    assert!(outcome.unwrap_or_default().contains("failed"));

    let row = store
        .action(seeded.action_id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(
        row.state,
        State::Reported,
        "hook failure returns to reported"
    );
    assert_eq!(row.pending_commit_subject.as_deref(), Some("feat: add"));
    // The lock is still held: the decision is still owed.
    assert_eq!(
        store
            .lock_holder(&repo_str(&repo))
            .await
            .expect("lock")
            .as_deref(),
        Some(seeded.action_id.to_string().as_str())
    );
    // The DB-persisted subject prefills the next modal (P6).
    let prefill = accept::accept_modal(&row);
    assert_eq!(
        prefill.inputs[0].value.as_deref(),
        Some("feat: add"),
        "modal prefilled from DB"
    );
    assert!(
        discord
            .posts()
            .iter()
            .any(|post| post.message.content.contains("hook says no")),
        "hook output surfaced"
    );
}

#[tokio::test]
async fn accept_refuses_when_started_dirty_without_include_existing() {
    let tmp = TempDir::new().expect("tmp");
    let (repo, base) = make_repo(&tmp);
    let store = StoreHandle::open_in_memory().expect("store");
    let seeded = common::seed_reported(
        &store,
        &repo_str(&repo),
        Some("main"),
        Some(&base),
        CHANNEL,
        "work",
        "prompt",
        "ses_1",
    )
    .await;
    // Mark the run as started on a dirty tree that was not included (FR-27).
    // `include_existing` already defaults to 0, so only the dirty flag is set.
    let writes = [(Column::DirtyAtStart, WriteValue::Int(1))];
    assert!(
        store
            .cas_state(seeded.action_id, State::Reported, State::Reported, &writes)
            .await
            .expect("mark dirty"),
        "mark-dirty CAS must find a reported action"
    );
    write_file(&repo, "feature.txt", "f\n");
    let discord = FakeChat::new();
    let ctx = ctx_for(&store, &repo, &discord);

    let error = accept::execute_accept(
        &ctx,
        seeded.action_id,
        "s",
        "",
        &format!("discord:{OPERATOR}"),
    )
    .await
    .expect_err("must refuse");
    assert!(error.to_string().contains("FR-27"), "error: {error}");
    assert_eq!(
        store
            .action(seeded.action_id)
            .await
            .expect("read")
            .expect("row")
            .state,
        State::Reported
    );
}

#[tokio::test]
async fn accept_refuses_on_a_different_branch() {
    let tmp = TempDir::new().expect("tmp");
    let (repo, base) = make_repo(&tmp);
    // Create and switch to another branch.
    let status = common::run_git(&repo, &["checkout", "-b", "other"]);
    assert!(status.status.success());

    let store = StoreHandle::open_in_memory().expect("store");
    let seeded = common::seed_reported(
        &store,
        &repo_str(&repo),
        Some("main"),
        Some(&base),
        CHANNEL,
        "work",
        "prompt",
        "ses_1",
    )
    .await;
    write_file(&repo, "feature.txt", "f\n");
    let discord = FakeChat::new();
    let ctx = ctx_for(&store, &repo, &discord);

    let error = accept::execute_accept(
        &ctx,
        seeded.action_id,
        "s",
        "",
        &format!("discord:{OPERATOR}"),
    )
    .await
    .expect_err("must refuse");
    assert!(error.to_string().contains("FR-17"), "error: {error}");
    assert_eq!(
        store
            .action(seeded.action_id)
            .await
            .expect("read")
            .expect("row")
            .state,
        State::Reported
    );
}

#[tokio::test]
async fn non_git_mark_done_is_db_only() {
    let tmp = TempDir::new().expect("tmp");
    let dir = tmp.path().join("plain");
    std::fs::create_dir_all(&dir).expect("mkdir");
    let store = StoreHandle::open_in_memory().expect("store");
    let seeded = common::seed_reported(
        &store,
        &repo_str(&dir),
        None,
        None,
        CHANNEL,
        "work",
        "prompt",
        "ses_1",
    )
    .await;
    let discord = FakeChat::new();
    let ctx = context(
        store.clone(),
        Arc::new(FakeOpenCode::new()),
        Arc::new(FakeGit::inert()),
        Arc::new(discord),
        OPERATOR,
        Some(CHANNEL),
    );

    let outcome = accept::execute_mark_done(&ctx, seeded.action_id, &format!("discord:{OPERATOR}"))
        .await
        .expect("mark done");
    assert!(outcome.unwrap_or_default().contains("done"));
    let row = store
        .action(seeded.action_id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(row.state, State::Accepted);
    assert!(row.accept_sha.is_none(), "no commit for a non-git action");
}
