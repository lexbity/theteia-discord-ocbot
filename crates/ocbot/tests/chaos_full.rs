//! AC-6 snapshot half — the **full** labelled crash-point matrix. For each
//! labelled point (CP-A0..A3, CP-P0..P4, CP-R0..R2) this constructs the DB +
//! repo state exactly as of that point (using the separately-callable steps) and
//! asserts that `reconcile_git_states` rolls forward (or conservatively
//! reverts) to the §6.4 table outcome. This runs in CI; `chaos_sigkill.rs` adds
//! the real-SIGKILL half.

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
use ocbot::adapters::git::GitAdapter;
use ocbot::adapters::store::StoreHandle;
use ocbot::app::reconcile::reconcile_git_states;
use ocbot_core::ports::GitPort;
use ocbot_core::{Column, State, WriteValue};
use tempfile::TempDir;

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

fn repo_str(repo: &Path) -> String {
    repo.to_string_lossy().into_owned()
}

fn make_repo(tmp: &TempDir) -> (PathBuf, String) {
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

async fn set_started_at(store: &StoreHandle, action_id: i64) {
    // A state-preserving CAS: read the live state so the column write applies
    // without disturbing the transition under test.
    let state = store
        .action(action_id)
        .await
        .expect("read")
        .expect("row")
        .state;
    let writes = [(Column::StartedAt, WriteValue::Int(1))];
    assert!(
        store
            .cas_state(action_id, state, state, &writes)
            .await
            .expect("set started_at"),
        "set_started_at CAS must find the action"
    );
}

/// Put an action into `accepting` with a persisted subject (CP-A0 state).
async fn enter_accepting(store: &StoreHandle, action_id: i64, subject: &str) {
    let writes = vec![(
        Column::PendingCommitSubject,
        WriteValue::Text(subject.to_owned()),
    )];
    assert!(
        store
            .cas_state(action_id, State::Reported, State::Accepting, &writes)
            .await
            .expect("cas accepting")
    );
}

#[tokio::test]
async fn accept_cp_a0_without_a_commit_reverts_to_reported() {
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
    enter_accepting(&store, seeded.action_id, "never happened").await;

    let ctx = ctx_for(&store, &repo, &FakeChat::new());
    let repaired = reconcile_git_states(&ctx).await;
    assert_eq!(repaired, 1);
    assert_eq!(
        store
            .action(seeded.action_id)
            .await
            .expect("read")
            .expect("row")
            .state,
        State::Reported,
        "CP-A0 with no commit is re-offered"
    );
}

#[tokio::test]
async fn accept_cp_a2_adopts_the_landed_commit() {
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
    enter_accepting(&store, seeded.action_id, "recover me").await;
    set_started_at(&store, seeded.action_id).await;
    write_file(&repo, "feature.txt", "f\n");
    let add = common::run_git(&repo, &["add", "-A"]);
    assert!(add.status.success());
    let commit = common::run_git(&repo, &["commit", "-m", "recover me"]);
    assert!(commit.status.success(), "manual commit");
    let head = common::git_stdout(&repo, &["rev-parse", "HEAD"]);

    let ctx = ctx_for(&store, &repo, &FakeChat::new());
    let repaired = reconcile_git_states(&ctx).await;
    assert_eq!(repaired, 1);
    let row = store
        .action(seeded.action_id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(row.state, State::Accepted, "CP-A2 adopts the commit");
    assert_eq!(row.accept_sha.as_deref(), Some(head.as_str()));
    assert_eq!(
        store.lock_holder(&repo_str(&repo)).await.expect("lock"),
        None
    );
}

/// Seed a `parking` action and drive it to just before `step`'s checkpoint.
async fn seed_parking(
    store: &StoreHandle,
    repo: &Path,
    base: &str,
    step: i64,
    marker: &str,
) -> i64 {
    let seeded = common::seed_reported(
        store,
        &repo_str(repo),
        Some("main"),
        Some(base),
        CHANNEL,
        "work",
        "prompt",
        "ses_1",
    )
    .await;
    let writes = vec![
        (
            Column::ParkBranchName,
            WriteValue::Text("rejected/work-1".to_owned()),
        ),
        (Column::ParkStep, WriteValue::Int(0)),
    ];
    assert!(
        store
            .cas_state_from_any(
                seeded.action_id,
                &[State::Reported, State::Errored, State::Aborted],
                State::Parking,
                &writes,
            )
            .await
            .expect("cas parking")
    );
    if step >= 1 {
        let add = common::run_git(repo, &["add", "-A"]);
        assert!(add.status.success());
        let commit = common::run_git(repo, &["commit", "--no-verify", "-m", marker]);
        assert!(commit.status.success(), "park commit");
    }
    let git = GitAdapter::new();
    if step >= 2 {
        let head = git.head(repo).await.expect("head");
        git.create_branch_at(repo, "rejected/work-1", &head)
            .await
            .expect("branch");
    }
    if step >= 3 {
        git.reset_hard(repo, base).await.expect("reset");
    }
    if step > 0 {
        assert!(
            store
                .cas_state(
                    seeded.action_id,
                    State::Parking,
                    State::Parking,
                    &[(Column::ParkStep, WriteValue::Int(step))],
                )
                .await
                .expect("set step")
        );
    }
    seeded.action_id
}

#[tokio::test]
async fn park_cp_p1_commits_resume_branches_and_resets() {
    let tmp = TempDir::new().expect("tmp");
    let (repo, base) = make_repo(&tmp);
    write_file(&repo, "feature.txt", "f\n");
    let store = StoreHandle::open_in_memory().expect("store");
    let action_id = seed_parking(&store, &repo, &base, 1, "park: work (attempt 1)").await;

    let ctx = ctx_for(&store, &repo, &FakeChat::new());
    let repaired = reconcile_git_states(&ctx).await;
    assert_eq!(repaired, 1);
    let row = store.action(action_id).await.expect("read").expect("row");
    assert_eq!(row.state, State::Parked);
    assert_eq!(
        GitAdapter::new().head(&repo).await.expect("head"),
        base,
        "reset back to the pre-attempt HEAD"
    );
    assert!(
        GitAdapter::new()
            .branch_exists(&repo, "rejected/work-1")
            .await
            .expect("branch")
            .is_some()
    );
    assert_eq!(store.staged_actions().await.expect("staged").len(), 1);
}

#[tokio::test]
async fn park_cp_p2_resumes_at_the_reset() {
    let tmp = TempDir::new().expect("tmp");
    let (repo, base) = make_repo(&tmp);
    write_file(&repo, "feature.txt", "f\n");
    let store = StoreHandle::open_in_memory().expect("store");
    let action_id = seed_parking(&store, &repo, &base, 2, "park: work (attempt 1)").await;

    let ctx = ctx_for(&store, &repo, &FakeChat::new());
    reconcile_git_states(&ctx).await;
    assert_eq!(
        store
            .action(action_id)
            .await
            .expect("read")
            .expect("row")
            .state,
        State::Parked
    );
    assert_eq!(GitAdapter::new().head(&repo).await.expect("head"), base);
}

#[tokio::test]
async fn park_cp_p3_finishes() {
    let tmp = TempDir::new().expect("tmp");
    let (repo, base) = make_repo(&tmp);
    write_file(&repo, "feature.txt", "f\n");
    let store = StoreHandle::open_in_memory().expect("store");
    let action_id = seed_parking(&store, &repo, &base, 3, "park: work (attempt 1)").await;

    let ctx = ctx_for(&store, &repo, &FakeChat::new());
    reconcile_git_states(&ctx).await;
    assert_eq!(
        store
            .action(action_id)
            .await
            .expect("read")
            .expect("row")
            .state,
        State::Parked
    );
}

async fn seed_reset(store: &StoreHandle, repo: &Path, first: &str, step: i64) -> i64 {
    let dir = repo_str(repo);
    let id = store
        .record_reset_intent(&dir, first, "superseded/t")
        .await
        .expect("intent");
    let git = GitAdapter::new();
    if step >= 1 {
        let head = git.head(repo).await.expect("head");
        git.create_branch_at(repo, "superseded/t", &head)
            .await
            .expect("branch");
        store.set_reset_step(id, 1).await.expect("step1");
    }
    if step >= 2 {
        git.reset_hard(repo, first).await.expect("reset");
        store.set_reset_step(id, 2).await.expect("step2");
    }
    id
}

#[tokio::test]
async fn reset_cp_r0_creates_the_safety_branch_then_resets() {
    let tmp = TempDir::new().expect("tmp");
    let repo = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    init_repo(&repo);
    write_file(&repo, "a.txt", "a\n");
    let first = common::commit_all(&repo, "first");
    write_file(&repo, "b.txt", "b\n");
    let second = common::commit_all(&repo, "second");
    let store = StoreHandle::open_in_memory().expect("store");
    store
        .upsert_project(&repo_str(&repo), Some("main"), Some(&first))
        .await
        .expect("project");
    seed_reset(&store, &repo, &first, 0).await;

    let ctx = ctx_for(&store, &repo, &FakeChat::new());
    reconcile_git_states(&ctx).await;
    assert!(
        store
            .pending_reset_intents()
            .await
            .expect("intents")
            .is_empty()
    );
    assert_eq!(GitAdapter::new().head(&repo).await.expect("head"), first);
    assert_eq!(
        common::git_stdout(&repo, &["rev-parse", "superseded/t"]),
        second
    );
}

#[tokio::test]
async fn reset_cp_r1_and_r2_resume() {
    for step in [1_i64, 2] {
        let tmp = TempDir::new().expect("tmp");
        let repo = tmp.path().join("work");
        std::fs::create_dir_all(&repo).expect("mkdir");
        init_repo(&repo);
        write_file(&repo, "a.txt", "a\n");
        let first = common::commit_all(&repo, "first");
        write_file(&repo, "b.txt", "b\n");
        common::commit_all(&repo, "second");
        let store = StoreHandle::open_in_memory().expect("store");
        store
            .upsert_project(&repo_str(&repo), Some("main"), Some(&first))
            .await
            .expect("project");
        seed_reset(&store, &repo, &first, step).await;

        let ctx = ctx_for(&store, &repo, &FakeChat::new());
        reconcile_git_states(&ctx).await;
        assert!(
            store
                .pending_reset_intents()
                .await
                .expect("intents")
                .is_empty(),
            "step {step}"
        );
        assert_eq!(
            GitAdapter::new().head(&repo).await.expect("head"),
            first,
            "step {step}"
        );
    }
}

#[tokio::test]
async fn accept_cp_a1_with_a_foreign_commit_reverts_to_reported() {
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
    enter_accepting(&store, seeded.action_id, "the expected subject").await;
    set_started_at(&store, seeded.action_id).await;
    // A commit lands, but it is not the one the accept was going to make.
    write_file(&repo, "foreign.txt", "x\n");
    let add = common::run_git(&repo, &["add", "-A"]);
    assert!(add.status.success());
    let commit = common::run_git(&repo, &["commit", "-m", "a foreign subject"]);
    assert!(commit.status.success());

    let ctx = ctx_for(&store, &repo, &FakeChat::new());
    let repaired = reconcile_git_states(&ctx).await;
    assert_eq!(repaired, 1);
    let row = store
        .action(seeded.action_id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(
        row.state,
        State::Reported,
        "CP-A1: a subject mismatch is re-offered, never silently accepted"
    );
    assert!(row.accept_sha.is_none());
    // `reported` still holds the directory lock (P5).
    assert_eq!(
        store.lock_holder(&repo_str(&repo)).await.expect("lock"),
        Some(seeded.action_id.to_string())
    );
}

#[tokio::test]
async fn accept_cp_a3_accepted_is_terminal_and_untouched() {
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
    enter_accepting(&store, seeded.action_id, "subject").await;
    assert!(
        store
            .cas_state(
                seeded.action_id,
                State::Accepting,
                State::Accepted,
                &[(
                    ocbot_core::Column::AcceptSha,
                    ocbot_core::WriteValue::Text(base.clone()),
                )],
            )
            .await
            .expect("accepted")
    );

    let ctx = ctx_for(&store, &repo, &FakeChat::new());
    assert_eq!(reconcile_git_states(&ctx).await, 0, "Accepted is terminal");
    let row = store
        .action(seeded.action_id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(row.state, State::Accepted);
    assert_eq!(row.accept_sha.as_deref(), Some(base.as_str()));
}

#[tokio::test]
async fn park_cp_p0_commits_from_a_dirty_worktree() {
    let tmp = TempDir::new().expect("tmp");
    let (repo, base) = make_repo(&tmp);
    // The attempt left uncommitted work; CP-P0 has only recorded the intent.
    write_file(&repo, "feature.txt", "f\n");
    let store = StoreHandle::open_in_memory().expect("store");
    let action_id = seed_parking(&store, &repo, &base, 0, "park: work (attempt 1)").await;

    let ctx = ctx_for(&store, &repo, &FakeChat::new());
    assert_eq!(reconcile_git_states(&ctx).await, 1);
    let row = store.action(action_id).await.expect("read").expect("row");
    assert_eq!(row.state, State::Parked, "CP-P0 commits, branches, resets");
    let git = GitAdapter::new();
    assert_eq!(git.head(&repo).await.expect("head"), base);
    assert!(git.status_porcelain(&repo).await.expect("clean").is_empty());
    assert!(
        git.branch_exists(&repo, "rejected/work-1")
            .await
            .expect("branch")
            .is_some()
    );
    assert_eq!(
        common::git_stdout(&repo, &["show", "rejected/work-1:feature.txt"]),
        "f"
    );
}

#[tokio::test]
async fn park_cp_p4_parked_is_untouched_on_a_second_pass() {
    let tmp = TempDir::new().expect("tmp");
    let (repo, base) = make_repo(&tmp);
    write_file(&repo, "feature.txt", "f\n");
    let store = StoreHandle::open_in_memory().expect("store");
    let action_id = seed_parking(&store, &repo, &base, 3, "park: work (attempt 1)").await;

    let ctx = ctx_for(&store, &repo, &FakeChat::new());
    assert_eq!(reconcile_git_states(&ctx).await, 1);
    assert_eq!(
        reconcile_git_states(&ctx).await,
        0,
        "CP-P4: a sealed park is never re-run"
    );
    assert_eq!(
        store
            .action(action_id)
            .await
            .expect("read")
            .expect("row")
            .state,
        State::Parked
    );
}
