//! Park & redo (FR-19) against real repositories: the attempt is committed,
//! branched under `rejected/*`, the work branch returns to the pre-attempt
//! HEAD with a clean tree (AC-7), and a retry action is staged.

// Integration tests assert via unwrap/panic; production code remains strict
// (workspace lint floor + G3). See common/mod.rs for the shared harness.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;

use common::{FakeDiscord, FakeGit, FakeOpenCode, append_file, context, init_repo, write_file};
use ocbot::adapters::git::GitAdapter;
use ocbot::adapters::store::StoreHandle;
use ocbot::app::handlers::park;
use ocbot_core::State;
use ocbot_core::ports::GitPort;
use tempfile::TempDir;

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

fn repo_str(repo: &Path) -> String {
    repo.to_string_lossy().into_owned()
}

fn make_repo(tmp: &TempDir) -> (std::path::PathBuf, String) {
    let repo = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    init_repo(&repo);
    write_file(&repo, "README.md", "# work\n");
    let base = common::commit_all(&repo, "initial");
    (repo, base)
}

fn ctx_for(
    store: &StoreHandle,
    repo: &Path,
    discord: &FakeDiscord,
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

#[tokio::test]
async fn park_branches_the_attempt_and_returns_to_a_clean_tree() {
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
        "the original prompt",
        "ses_1",
    )
    .await;
    // A tracked edit and an untracked file (untracked files must join the park
    // commit — AC-7).
    append_file(&repo, "README.md", "edit\n");
    write_file(&repo, "new.txt", "untracked content\n");

    let discord = FakeDiscord::new();
    let ctx = ctx_for(&store, &repo, &discord);
    let outcome = park::begin_park(&ctx, seeded.action_id, OPERATOR)
        .await
        .expect("park");
    assert!(outcome.unwrap_or_default().contains("parked"));

    let row = store
        .action(seeded.action_id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(row.state, State::Parked);
    assert_eq!(row.park_branch_name.as_deref(), Some("rejected/work-1"));

    let git = GitAdapter::new();
    assert_eq!(
        git.head(&repo).await.expect("head"),
        base,
        "clean at last accept"
    );
    assert!(git.status_porcelain(&repo).await.expect("clean").is_empty());
    assert_eq!(
        common::git_stdout(&repo, &["show", "rejected/work-1:new.txt"]),
        "untracked content",
        "the park branch contains the untracked file"
    );
    assert!(
        common::git_stdout(&repo, &["show", "rejected/work-1:README.md"]).contains("edit"),
        "the park branch contains the tracked edit"
    );

    // A retry is staged with the original prompt and a bumped attempt.
    let staged = store.staged_actions().await.expect("staged");
    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0].attempt, 2);
    assert_eq!(staged[0].prompt_sent, "the original prompt");
    assert_eq!(staged[0].parent_action_id, Some(seeded.action_id));

    // The lock released and the retry preview posted.
    assert_eq!(
        store.lock_holder(&repo_str(&repo)).await.expect("lock"),
        None
    );
    assert!(
        discord.posts().iter().any(|post| post
            .message
            .components
            .iter()
            .any(|component| component.custom_id == format!("v1:send:{}", staged[0].id))),
        "retry preview carries Send"
    );
}

#[tokio::test]
async fn park_refuses_an_oversized_untracked_file() {
    let tmp = TempDir::new().expect("tmp");
    let (repo, _base) = make_repo(&tmp);
    let store = StoreHandle::open_in_memory().expect("store");
    let seeded = common::seed_reported(
        &store,
        &repo_str(&repo),
        Some("main"),
        Some(&common::git_stdout(&repo, &["rev-parse", "HEAD"])),
        CHANNEL,
        "work",
        "prompt",
        "ses_1",
    )
    .await;
    // A sparse 51 MB untracked file (no disk cost).
    let big = repo.join("big.bin");
    let file = std::fs::File::create(&big).expect("create");
    file.set_len(51 * 1024 * 1024).expect("set_len");

    let discord = FakeDiscord::new();
    let ctx = ctx_for(&store, &repo, &discord);
    let outcome = park::begin_park(&ctx, seeded.action_id, OPERATOR)
        .await
        .expect("park returns a refusal, not an error");
    assert!(
        outcome.unwrap_or_default().contains("refusing to park"),
        "the oversized file is named"
    );
    assert_eq!(
        store
            .action(seeded.action_id)
            .await
            .expect("read")
            .expect("row")
            .state,
        State::Reported,
        "a refused park leaves the decision state intact"
    );
}

#[tokio::test]
async fn park_of_a_no_change_attempt_still_branches() {
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

    let discord = FakeDiscord::new();
    let ctx = ctx_for(&store, &repo, &discord);
    park::begin_park(&ctx, seeded.action_id, OPERATOR)
        .await
        .expect("park");
    assert_eq!(
        store
            .action(seeded.action_id)
            .await
            .expect("read")
            .expect("row")
            .state,
        State::Parked
    );
    // The rejected branch exists at HEAD even though nothing changed.
    let git = GitAdapter::new();
    assert!(
        git.branch_exists(&repo, "rejected/work-1")
            .await
            .expect("branch")
            .is_some()
    );
    assert_eq!(git.head(&repo).await.expect("head"), base);
}

/// Reject only branch updates that MOVE a branch (`refs/heads/*` with
/// old ≠ new): `--no-verify` skips the pre-commit and commit-msg hooks but
/// **not** the reference-transaction hook, so `git add -A` stages the attempt
/// and the commit's branch move is then rejected — the exact CP-P1 failure
/// window where the index must be repaired. The repair's own `git reset` does
/// not move any branch (HEAD and `refs/heads/*` keep their values; only
/// `ORIG_HEAD` bookkeeping updates with an unspecified old value) and stays
/// allowed.
fn install_failing_ref_hook(repo: &Path) {
    let script = concat!(
        "#!/bin/sh\n",
        "while read old new ref; do\n",
        "  case \"$ref\" in\n",
        "    refs/heads/*) [ \"$old\" = \"$new\" ] || exit 1 ;;\n",
        "  esac\n",
        "done\n",
        "exit 0\n",
    );
    let hook = repo.join(".git/hooks/reference-transaction");
    std::fs::write(&hook, script).expect("write hook");
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).expect("chmod hook");
}

#[tokio::test]
async fn park_commit_failure_unstages_the_index_and_keeps_files() {
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
        "the original prompt",
        "ses_1",
    )
    .await;
    // The attempt: a tracked edit plus an untracked file.
    append_file(&repo, "README.md", "edit\n");
    write_file(&repo, "new.txt", "attempt content\n");
    install_failing_ref_hook(&repo);

    let discord = FakeDiscord::new();
    let ctx = ctx_for(&store, &repo, &discord);
    let outcome = park::begin_park(&ctx, seeded.action_id, OPERATOR)
        .await
        .expect("a failed commit is a handled park failure, not an error");
    assert!(
        outcome
            .as_deref()
            .unwrap_or_default()
            .contains("park failed"),
        "outcome names the failure: {outcome:?}"
    );

    // The action reached `errored` — not stuck in `parking`.
    assert_eq!(
        store
            .action(seeded.action_id)
            .await
            .expect("read")
            .expect("row")
            .state,
        State::Errored
    );

    // CP-P1's repair, verbatim from the spec: "tree unchanged except index →
    // git reset (unstage), keep files".
    assert_eq!(
        common::git_stdout(&repo, &["diff", "--cached", "--name-only"]),
        "",
        "nothing stays staged"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("new.txt")).expect("file kept"),
        "attempt content\n"
    );
    let porcelain = common::porcelain(&repo);
    assert!(
        porcelain.contains("?? new.txt"),
        "untracked file kept: {porcelain}"
    );
    assert!(
        porcelain.contains(" M README.md"),
        "tracked edit kept, unstaged: {porcelain}"
    );

    // The sequence stopped before the branch/reset steps.
    let git = GitAdapter::new();
    assert!(
        git.branch_exists(&repo, "rejected/work-1")
            .await
            .expect("branch")
            .is_none()
    );
    assert_eq!(git.head(&repo).await.expect("head"), base);

    // The failure post fits the Discord cap and reports the tree state.
    let post = discord.last_post().expect("failure posted");
    assert!(post.message.content.contains("Park failed"));
    assert!(
        post.message.content.contains("index unstaged"),
        "{}",
        post.message.content
    );
    assert!(
        post.message.content.len() <= ocbot_core::DISCORD_MESSAGE_CAP,
        "failure post is {} bytes",
        post.message.content.len()
    );
}
