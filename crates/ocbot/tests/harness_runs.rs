//! The crash harness's *happy* paths (coverage of `src/harness.rs`): the same
//! binary `chaos_sigkill` SIGKILLs mid-sequence, run to completion. Each mode
//! (accept, park, reset, store) asserts its own durable outcome, so the
//! non-crashing branch of every labelled checkpoint is exercised too — the
//! kill tests only prove the half-sequence was recoverable.

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
use std::time::Duration;

use common::{commit_all, git_stdout, init_repo, seed_reported, spawn_harness, write_file};
use ocbot::adapters::store::StoreHandle;
use ocbot_core::State;

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

/// Run the harness to completion (no `OCBOT_TEST_CRASH_AFTER`: no checkpoint
/// hook fires) and assert it exited cleanly. A nonzero exit is the harness
/// reporting `Err(...)`, which every mode below must avoid.
fn run_to_completion(envs: &[(&str, String)]) {
    let mut child = spawn_harness(envs);
    let status = child.wait().expect("wait for the harness");
    assert!(
        status.success(),
        "the harness failed its sequence: {status:?}"
    );
}

/// Wait for a child to exit; `timeout` guards a hang (the CI signal that a
/// mode blocked rather than finished).
fn run_with_timeout(envs: &[(&str, String)], what: &str) {
    let mut child = spawn_harness(envs);
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        match child.try_wait().expect("wait") {
            Some(status) => {
                assert!(status.success(), "{what} failed: {status:?}");
                return;
            }
            None => assert!(std::time::Instant::now() < deadline, "{what} hung"),
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The shared env prefix every mode needs.
fn base_env(db: &Path, markers: &Path, mode: &str) -> Vec<(&'static str, String)> {
    vec![
        ("OCBOT_CRASH_DB", db.to_string_lossy().into_owned()),
        ("OCBOT_CRASH_MODE", mode.to_owned()),
        (
            "OCBOT_TEST_CRASH_DIR",
            markers.to_string_lossy().into_owned(),
        ),
        ("OCBOT_CRASH_OPERATOR", OPERATOR.to_string()),
    ]
}

#[tokio::test]
async fn the_store_sequence_commits_every_step() {
    let tmp = tempfile::tempdir().expect("tmp");
    let db = tmp.path().join("ocbot.db");
    let markers = tmp.path().join("markers");
    let repo = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");

    let mut envs = base_env(&db, &markers, "store");
    envs.push(("OCBOT_CRASH_DIR", repo.to_string_lossy().into_owned()));
    run_to_completion(&envs);
    // The checkpoint hook is inert without `OCBOT_TEST_CRASH_AFTER`, so no
    // marker file appears — the whole point of the sequence is that every
    // labelled commit landed.
    assert!(
        !markers.join("STORE-PROJECT.reached").exists(),
        "no checkpoint fired without the opt-in env"
    );

    // The store sequence's durable outcome: the project, run, action, send,
    // report and lock release all committed. `action_counts_by_state` is the
    // typed read that names the committed state by label.
    let store = StoreHandle::open(&db).expect("reopen");
    let counts = store.action_counts_by_state().await.expect("counts");
    assert!(
        counts
            .iter()
            .any(|(state, count)| state == "reported" && *count == 1),
        "the report step committed: {counts:?}"
    );
    store.shutdown().await;
}

#[tokio::test]
async fn accept_runs_to_completion_and_lands_the_commit() {
    let tmp = tempfile::tempdir().expect("tmp");
    let repo = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    init_repo(&repo);
    write_file(&repo, "README.md", "# work\n");
    let base = commit_all(&repo, "initial");
    let repo_text = repo.to_string_lossy().into_owned();
    let db = tmp.path().join("ocbot.db");
    let markers = tmp.path().join("markers");

    let action = {
        let store = StoreHandle::open(&db).expect("store");
        let seeded = seed_reported(
            &store,
            &repo_text,
            Some("main"),
            Some(&base),
            CHANNEL,
            "work",
            "prompt",
            "ses_1",
        )
        .await;
        write_file(&repo, "feature.txt", "f\n");
        store.shutdown().await;
        seeded.action_id
    };

    let mut envs = base_env(&db, &markers, "accept");
    envs.push(("OCBOT_CRASH_ACTION", action.to_string()));
    run_with_timeout(&envs, "the accept sequence");

    let store = StoreHandle::open(&db).expect("reopen");
    let row = store.action(action).await.expect("read").expect("row");
    assert_eq!(row.state, State::Accepted);
    assert!(row.accept_sha.is_some(), "the accept sha is recorded");
    let head = git_stdout(&repo, &["rev-parse", "HEAD"]);
    assert_eq!(head, row.accept_sha.expect("sha"), "HEAD is the accept sha");
    assert_eq!(
        store
            .lock_holder(&repo_text)
            .await
            .expect("lock")
            .as_deref(),
        None,
        "the lock was released"
    );
    store.shutdown().await;
}

#[tokio::test]
async fn park_runs_to_completion_and_moves_the_branch() {
    let tmp = tempfile::tempdir().expect("tmp");
    let repo = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    init_repo(&repo);
    write_file(&repo, "README.md", "# work\n");
    let base = commit_all(&repo, "initial");
    let repo_text = repo.to_string_lossy().into_owned();
    let db = tmp.path().join("ocbot.db");
    let markers = tmp.path().join("markers");

    let action = {
        let store = StoreHandle::open(&db).expect("store");
        let seeded = seed_reported(
            &store,
            &repo_text,
            Some("main"),
            Some(&base),
            CHANNEL,
            "work",
            "prompt",
            "ses_1",
        )
        .await;
        write_file(&repo, "feature.txt", "f\n");
        store.shutdown().await;
        seeded.action_id
    };

    let mut envs = base_env(&db, &markers, "park");
    envs.push(("OCBOT_CRASH_ACTION", action.to_string()));
    run_with_timeout(&envs, "the park sequence");

    let store = StoreHandle::open(&db).expect("reopen");
    let row = store.action(action).await.expect("read").expect("row");
    assert_eq!(row.state, State::Parked);
    assert_eq!(
        git_stdout(&repo, &["rev-parse", "HEAD"]),
        base,
        "park resets back to the base"
    );
    assert!(
        git_stdout(&repo, &["status", "--porcelain"]).is_empty(),
        "the working tree is clean after park"
    );
    assert!(
        git_stdout(&repo, &["branch", "--format=%(refname:short)"])
            .lines()
            .any(|branch| branch == "rejected/work-1"),
        "the rejected branch carries the work"
    );
    store.shutdown().await;
}

#[tokio::test]
async fn reset_runs_to_completion_and_keeps_a_superseded_ref() {
    let tmp = tempfile::tempdir().expect("tmp");
    let repo = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    init_repo(&repo);
    write_file(&repo, "a.txt", "a\n");
    let first = commit_all(&repo, "first");
    write_file(&repo, "b.txt", "b\n");
    let second = commit_all(&repo, "second");
    let repo_text = repo.to_string_lossy().into_owned();
    let db = tmp.path().join("ocbot.db");
    let markers = tmp.path().join("markers");

    {
        let store = StoreHandle::open(&db).expect("store");
        store
            .upsert_project(&repo_text, Some("main"), Some(&first))
            .await
            .expect("project");
        store.shutdown().await;
    }

    let mut envs = base_env(&db, &markers, "reset");
    envs.push(("OCBOT_CRASH_DIR", repo_text));
    envs.push(("OCBOT_CRASH_TARGET", first.clone()));
    run_with_timeout(&envs, "the reset sequence");

    assert_eq!(git_stdout(&repo, &["rev-parse", "HEAD"]), first);
    assert_eq!(
        git_stdout(
            &repo,
            &[
                "for-each-ref",
                "--format=%(objectname)",
                "refs/heads/superseded/"
            ],
        ),
        second,
        "the superseded ref still points at the pre-reset head"
    );
}

#[tokio::test]
async fn an_unknown_mode_is_a_loud_failure_not_a_silent_success() {
    // The harness's error arm (`unknown OCBOT_CRASH_MODE`) must fail the
    // process, or a typo'd env var would look like a passing chaos sequence.
    let tmp = tempfile::tempdir().expect("tmp");
    let db = tmp.path().join("ocbot.db");
    let markers = tmp.path().join("markers");
    let mut child = spawn_harness(&base_env(&db, &markers, "teleport"));
    let status = child.wait().expect("wait");
    assert!(!status.success(), "an unknown mode must exit nonzero");
}
