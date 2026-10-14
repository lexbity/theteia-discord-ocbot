//! AC-6 real half: spawn the crash-harness as a separate process, let it reach
//! a labelled checkpoint, SIGKILL it, then prove the next boot's
//! `reconcile_git_states` recovers from DB/git facts alone. One labelled point
//! per sequence (accept, park, reset).

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
use std::time::Duration;

use common::{FakeChat, FakeOpenCode, context, init_repo, spawn_harness, write_file};
use ocbot::adapters::git::GitAdapter;
use ocbot::adapters::store::StoreHandle;
use ocbot::app::reconcile::reconcile_git_states;
use ocbot_core::State;
use ocbot_core::ports::GitPort;
use tempfile::TempDir;

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

async fn wait_for_marker(marker: &Path) {
    for _ in 0..400 {
        if marker.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("harness never reached checkpoint {}", marker.display());
}

fn harness_env(
    db: &Path,
    marker_dir: &Path,
    mode: &str,
    crash_after: &str,
    extra: Vec<(&'static str, String)>,
) -> Vec<(&'static str, String)> {
    let mut envs: Vec<(&'static str, String)> = vec![
        ("OCBOT_CRASH_DB", db.to_string_lossy().into_owned()),
        ("OCBOT_CRASH_MODE", mode.to_owned()),
        (
            "OCBOT_TEST_CRASH_DIR",
            marker_dir.to_string_lossy().into_owned(),
        ),
        ("OCBOT_TEST_CRASH_AFTER", crash_after.to_owned()),
        ("OCBOT_CRASH_OPERATOR", OPERATOR.to_string()),
    ];
    envs.extend(extra);
    envs
}

/// Spawn, wait for the checkpoint marker, then SIGKILL.
async fn run_until_killed(envs: &[(&str, String)], marker: &Path) {
    let mut child = spawn_harness(envs);
    wait_for_marker(marker).await;
    child.kill().expect("SIGKILL the harness");
    let _ = child.wait();
}

fn reconcile_ctx(store: &StoreHandle) -> Arc<ocbot::app::context::AppContext> {
    context(
        store.clone(),
        Arc::new(FakeOpenCode::new()),
        Arc::new(GitAdapter::new()),
        Arc::new(FakeChat::new()),
        OPERATOR,
        Some(CHANNEL),
    )
}

#[tokio::test]
async fn sigkill_at_accept_cp_a2_recovers_to_accepted() {
    let tmp = TempDir::new().expect("tmp");
    let repo = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    init_repo(&repo);
    write_file(&repo, "README.md", "# work\n");
    let base = common::commit_all(&repo, "initial");
    let repo_text = repo.to_string_lossy().into_owned();
    let db = tmp.path().join("ocbot.db");
    let markers = tmp.path().join("markers");

    let action = {
        let store = StoreHandle::open(&db).expect("store");
        let seeded = common::seed_reported(
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

    run_until_killed(
        &harness_env(
            &db,
            &markers,
            "accept",
            "CP-A2",
            vec![("OCBOT_CRASH_ACTION", action.to_string())],
        ),
        &markers.join("CP-A2.reached"),
    )
    .await;

    // The commit landed; the sha was not recorded. Boot reconcile adopts it.
    let store = StoreHandle::open(&db).expect("reopen");
    let ctx = reconcile_ctx(&store);
    let repaired = reconcile_git_states(&ctx).await;
    assert_eq!(repaired, 1);
    let row = store.action(action).await.expect("read").expect("row");
    assert_eq!(row.state, State::Accepted);
    assert!(row.accept_sha.is_some());
    assert_eq!(store.lock_holder(&repo_text).await.expect("lock"), None);
}

#[tokio::test]
async fn sigkill_at_park_cp_p2_recovers_to_parked() {
    let tmp = TempDir::new().expect("tmp");
    let repo = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    init_repo(&repo);
    write_file(&repo, "README.md", "# work\n");
    let base = common::commit_all(&repo, "initial");
    let repo_text = repo.to_string_lossy().into_owned();
    let db = tmp.path().join("ocbot.db");
    let markers = tmp.path().join("markers");

    let action = {
        let store = StoreHandle::open(&db).expect("store");
        let seeded = common::seed_reported(
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

    run_until_killed(
        &harness_env(
            &db,
            &markers,
            "park",
            "CP-P2",
            vec![("OCBOT_CRASH_ACTION", action.to_string())],
        ),
        &markers.join("CP-P2.reached"),
    )
    .await;

    let store = StoreHandle::open(&db).expect("reopen");
    let ctx = reconcile_ctx(&store);
    reconcile_git_states(&ctx).await;
    let row = store.action(action).await.expect("read").expect("row");
    assert_eq!(row.state, State::Parked);
    let git = GitAdapter::new();
    assert_eq!(git.head(&repo).await.expect("head"), base);
    assert!(git.status_porcelain(&repo).await.expect("clean").is_empty());
    assert!(
        git.branch_exists(&repo, "rejected/work-1")
            .await
            .expect("branch")
            .is_some()
    );
}

#[tokio::test]
async fn sigkill_at_reset_cp_r1_recovers_the_reset() {
    let tmp = TempDir::new().expect("tmp");
    let repo = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    init_repo(&repo);
    write_file(&repo, "a.txt", "a\n");
    let first = common::commit_all(&repo, "first");
    write_file(&repo, "b.txt", "b\n");
    let second = common::commit_all(&repo, "second");
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

    run_until_killed(
        &harness_env(
            &db,
            &markers,
            "reset",
            "CP-R1",
            vec![
                ("OCBOT_CRASH_DIR", repo_text.clone()),
                ("OCBOT_CRASH_TARGET", first.clone()),
            ],
        ),
        &markers.join("CP-R1.reached"),
    )
    .await;

    let store = StoreHandle::open(&db).expect("reopen");
    let ctx = reconcile_ctx(&store);
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
        common::git_stdout(
            &repo,
            &[
                "for-each-ref",
                "--format=%(objectname)",
                "refs/heads/superseded/",
            ],
        ),
        second
    );
}
