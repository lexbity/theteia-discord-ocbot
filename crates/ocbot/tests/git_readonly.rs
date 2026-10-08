//! The read-only git adapter against real repositories (and a scripted fake
//! "git" for the timeout fallback, R-9).

// Integration tests assert via unwrap/panic; production code remains strict
// (workspace lint floor + G3). See common/mod.rs for the shared harness.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::time::Duration;

use ocbot::adapters::git::GitAdapter;
use ocbot_core::GitError;
use ocbot_core::ports::GitPort;
use tempfile::TempDir;

#[tokio::test]
async fn read_only_commands_on_a_real_repo() {
    let tmp = TempDir::new().expect("tmp");
    let repo = tmp.path().join("r");
    std::fs::create_dir_all(&repo).expect("mkdir");
    common::init_repo(&repo);
    common::write_file(&repo, "README.md", "# hello\n");
    let head = common::commit_all(&repo, "initial");

    let git = GitAdapter::new();
    assert!(git.is_repo(&repo).await.expect("is_repo"));
    assert!(
        git.is_ancestor(&repo, &head, &head)
            .await
            .expect("ancestor self")
    );
    assert_eq!(git.head(&repo).await.expect("head"), head);
    assert_eq!(git.branch(&repo).await.expect("branch"), "main");
    assert_eq!(git.status_porcelain(&repo).await.expect("clean"), "");
    let log = git.log(&repo, None).await.expect("log");
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].sha, head);
    assert_eq!(log[0].subject, "initial");
    // Diff against HEAD on a clean tree is empty.
    assert!(git.diff_stat(&repo, &head).await.expect("diff").is_empty());

    // Dirty the tree: an untracked file shows in status, a tracked edit shows
    // in the diff stat.
    common::write_file(&repo, "new.txt", "x\n");
    assert!(!git.status_porcelain(&repo).await.expect("dirty").is_empty());
    common::append_file(&repo, "README.md", "more\n");
    assert!(
        git.diff_stat(&repo, &head)
            .await
            .expect("diff")
            .contains("README.md")
    );

    // An unrelated directory is not a repo.
    let not_repo = tmp.path().join("not-repo");
    std::fs::create_dir_all(&not_repo).expect("mkdir");
    assert!(!git.is_repo(&not_repo).await.expect("is_repo"));
    assert!(git.head(&not_repo).await.is_err());
}

#[tokio::test]
async fn log_from_a_base_range() {
    let tmp = TempDir::new().expect("tmp");
    let repo = tmp.path().join("r2");
    std::fs::create_dir_all(&repo).expect("mkdir");
    common::init_repo(&repo);
    common::write_file(&repo, "a.txt", "a\n");
    let base = common::commit_all(&repo, "base");
    common::write_file(&repo, "b.txt", "b\n");
    let tip = common::commit_all(&repo, "tip");

    let git = GitAdapter::new();
    let rows = git.log(&repo, Some(&base)).await.expect("log range");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].sha, tip);
}

#[tokio::test]
async fn slow_git_times_out_and_gets_killed() {
    let tmp = TempDir::new().expect("tmp");
    // A "git" that sleeps forever (fake, scripted). The adapter must time out
    // and kill it within the configured budget (R-9).
    let fake = tmp.path().join("fake-git");
    std::fs::write(&fake, "#!/bin/sh\nsleep 300\nexit 0\n").expect("write fake git");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&fake).expect("stat").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake, permissions).expect("chmod");
    }

    let slow_git = GitAdapter::with_bin(fake, Duration::from_millis(400));
    let repo = tmp.path().join("r3");
    std::fs::create_dir_all(&repo).expect("mkdir");
    // A freshly written script can transiently fail `exec` with ETXTBSY when
    // test binaries run under heavy parallel load (a harness artifact, not an
    // adapter bug). The claim under test is the *timeout*; a transient spawn
    // collision is retried, never accepted as the outcome.
    let mut attempt = 0;
    loop {
        assert!(attempt < 3, "three transient spawn collisions in a row");
        attempt += 1;
        let error = slow_git
            .diff_stat(&repo, "HEAD")
            .await
            .expect_err("must time out");
        match error {
            GitError::Timeout { .. } => break,
            GitError::Spawn { .. } => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            other @ GitError::Command { .. } => panic!("expected timeout, got {other}"),
        }
    }
}

#[tokio::test]
async fn untracked_status_matches_between_std_and_adapter() {
    let tmp = TempDir::new().expect("tmp");
    let repo = tmp.path().join("r4");
    std::fs::create_dir_all(&repo).expect("mkdir");
    common::init_repo(&repo);
    common::write_file(&repo, "tracked.txt", "t\n");
    common::commit_all(&repo, "seed");
    common::write_file(&repo, "dirty.txt", "d\n");

    let via_std = common::porcelain(&repo);
    assert!(!via_std.is_empty(), "std::process sees the untracked file");
    let via_adapter = GitAdapter::new();
    assert!(
        !via_adapter
            .status_porcelain(&repo)
            .await
            .expect("adapter status")
            .is_empty(),
        "the adapter must see the untracked file"
    );
    let via_fake = common::FakeGit::with_repo(Some(repo.clone()));
    assert!(
        !via_fake
            .status_porcelain(&repo)
            .await
            .expect("fake adapter status")
            .is_empty(),
        "the fake-backed adapter must see the untracked file"
    );
}
