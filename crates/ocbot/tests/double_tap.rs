//! PH-5: Cross-surface CAS race tests (SPEC-004 FR-103).

// Integration tests assert via unwrap/panic; production code remains strict
// (workspace lint floor + G3). See common/mod.rs for the shared harness.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use common::{FakeChat, FakeGit, FakeOpenCode, context, init_repo, write_file};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::handlers::accept;
use tempfile::TempDir;

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;
const TRIALS: usize = 1_000;

/// A tiny deterministic PRNG (LCG) so the interleavings vary but the test is
/// reproducible.
fn next_rand(state: &mut u64) -> u64 {
    *state = state
        .wrapping_mul(6_364_136_223_846_793_005)
        .wrapping_add(1_442_695_040_888_963_407);
    *state >> 33
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn double_tap_accept_commits_exactly_once() {
    let tmp = TempDir::new().expect("tmp");
    let repo: PathBuf = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    init_repo(&repo);
    write_file(&repo, "README.md", "# work\n");
    let base = common::commit_all(&repo, "initial");
    let repo_text = repo.to_string_lossy().into_owned();

    let store = StoreHandle::open_in_memory().expect("store");
    let discord = FakeChat::new();
    let ctx = Arc::new(context(
        store.clone(),
        Arc::new(FakeOpenCode::new()),
        Arc::new(FakeGit::with_repo(Some(repo.clone()))),
        Arc::new(discord),
        OPERATOR,
        Some(CHANNEL),
    ));

    for trial in 0..TRIALS {
        write_file(&repo, &format!("f{trial}.txt"), "x\n");
        let seeded = common::seed_reported(
            &store,
            &repo_text,
            Some("main"),
            Some(&base),
            CHANNEL,
            "work",
            "prompt",
            &format!("ses_{trial}"),
        )
        .await;

        let mut rng = u64::try_from(trial).unwrap_or(0).wrapping_add(1);
        let first_delay = next_rand(&mut rng) % 4;
        let second_delay = next_rand(&mut rng) % 4;
        let first = async {
            for _ in 0..first_delay {
                tokio::task::yield_now().await;
            }
            accept::execute_accept(
                &ctx,
                seeded.action_id,
                "s",
                "",
                &format!("discord:{OPERATOR}"),
            )
            .await
        };
        let second = async {
            for _ in 0..second_delay {
                tokio::task::yield_now().await;
            }
            accept::execute_accept(
                &ctx,
                seeded.action_id,
                "s",
                "",
                &format!("discord:{OPERATOR}"),
            )
            .await
        };
        let (a, b) = tokio::join!(first, second);
        let wins = [a.is_ok(), b.is_ok()].iter().filter(|ok| **ok).count();
        assert_eq!(wins, 1, "exactly one accept wins (trial {trial})");
    }

    let commits = common::git_stdout(&repo, &["rev-list", "--count", "HEAD"])
        .parse::<usize>()
        .expect("commit count");
    assert_eq!(commits, TRIALS + 1, "exactly one commit per trial");
}

/// PH-5: Cross-surface race test (AC-3: same action, accept by Discord button
/// and accept by Signal reaction fired concurrently → exactly one wins).
#[tokio::test]
async fn cross_surface_race_one_wins() {
    let tmp = TempDir::new().expect("tmp");
    let repo: PathBuf = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    init_repo(&repo);
    write_file(&repo, "README.md", "# work\n");
    let base = common::commit_all(&repo, "initial");
    let repo_text = repo.to_string_lossy().into_owned();

    let store = StoreHandle::open_in_memory().expect("store");
    let discord = FakeChat::new();
    // Build a context with Discord only for now
    let ctx = Arc::new(context(
        store.clone(),
        Arc::new(FakeOpenCode::new()),
        Arc::new(FakeGit::with_repo(Some(repo.clone()))),
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    ));

    // Seed a reported action
    let seeded = common::seed_reported(
        &store,
        &repo_text,
        Some("main"),
        Some(&base),
        CHANNEL,
        "work",
        "prompt",
        "ses_cross",
    )
    .await;

    // Simulate accept by Discord button (actor: discord:42)
    let discord_accept = async {
        accept::execute_accept(
            &ctx,
            seeded.action_id,
            "s",
            "",
            &format!("discord:{OPERATOR}"),
        )
        .await
    };

    // Simulate accept by Signal reaction (actor: signal:+15551234567)
    let signal_accept = async {
        accept::execute_accept(&ctx, seeded.action_id, "s", "", "signal:+15551234567").await
    };

    // Fire concurrently
    let (discord_result, signal_result) = tokio::join!(discord_accept, signal_accept);

    // Exactly one should succeed
    let discord_won = discord_result.is_ok();
    let signal_won = signal_result.is_ok();
    let wins = [discord_won, signal_won].iter().filter(|&ok| *ok).count();

    assert_eq!(
        wins, 1,
        "exactly one surface wins cross-surface race (discord={discord_won}, signal={signal_won})"
    );
}
