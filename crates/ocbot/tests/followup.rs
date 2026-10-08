//! Follow up (FR-18/FR-29): the same session is reused, the follow-up counter
//! drives the Park & redo nudge in reports, and a reset tree refuses the
//! follow-up (FR-29).

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

use common::{FakeDiscord, FakeGit, FakeOpenCode, context, init_repo, write_file};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::handlers::followup;
use ocbot::app::watcher::Watcher;
use ocbot_core::ports::Activity;
use ocbot_core::{State, ports::OpenCodePort};
use tempfile::TempDir;

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

fn repo_str(repo: &Path) -> String {
    repo.to_string_lossy().into_owned()
}

fn ctx_for(
    store: &StoreHandle,
    repo: &Path,
    opencode: Arc<dyn OpenCodePort>,
    discord: &FakeDiscord,
) -> Arc<ocbot::app::context::AppContext> {
    context(
        store.clone(),
        opencode,
        Arc::new(FakeGit::with_repo(Some(repo.to_path_buf()))),
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    )
}

#[tokio::test]
async fn followup_reuses_the_same_session_and_increments() {
    let tmp = TempDir::new().expect("tmp");
    let repo = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    init_repo(&repo);
    write_file(&repo, "README.md", "# work\n");
    let base = common::commit_all(&repo, "initial");
    let dir = repo_str(&repo);

    let store = StoreHandle::open_in_memory().expect("store");
    let seeded = common::seed_reported(
        &store,
        &dir,
        Some("main"),
        Some(&base),
        CHANNEL,
        "poke",
        "prompt",
        "ses_fu",
    )
    .await;
    let opencode = FakeOpenCode::new();
    let discord = FakeDiscord::new();
    let handle = opencode.clone();
    let ctx = ctx_for(&store, &repo, Arc::new(opencode), &discord);

    let outcome = followup::execute_followup(&ctx, seeded.action_id, "keep going", OPERATOR)
        .await
        .expect("follow up");
    assert!(outcome.unwrap_or_default().contains("follow-up"));

    let row = store
        .action(seeded.action_id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(row.state, State::Running);
    assert_eq!(row.followups, 1);
    assert_eq!(row.session_id.as_deref(), Some("ses_fu"), "session reused");
    // The follow-up prompt replaced the stored prompt (P13) and was admitted
    // to the *same* session — no new session was created.
    assert_eq!(row.prompt_sent, "keep going");
    assert!(handle.inner().sessions_created.is_empty());
    assert!(
        handle
            .inner()
            .calls
            .iter()
            .any(|call| matches!(call, common::Call::SendPrompt(session, text) if session == "ses_fu" && text == "keep going"))
    );
}

#[tokio::test]
async fn reports_nudge_after_two_followups() {
    let tmp = TempDir::new().expect("tmp");
    let repo = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    init_repo(&repo);
    write_file(&repo, "README.md", "# work\n");
    let base = common::commit_all(&repo, "initial");
    let dir = repo_str(&repo);

    let store = StoreHandle::open_in_memory().expect("store");
    let seeded = common::seed_reported(
        &store,
        &dir,
        Some("main"),
        Some(&base),
        CHANNEL,
        "poke",
        "prompt",
        "ses_fu2",
    )
    .await;
    common::write_file(&repo, "feature.txt", "f\n");
    let opencode = FakeOpenCode::new();
    let discord = FakeDiscord::new();
    let ctx = ctx_for(&store, &repo, Arc::new(opencode.clone()), &discord);

    for _ in 0..2 {
        followup::execute_followup(&ctx, seeded.action_id, "again", OPERATOR)
            .await
            .expect("follow up");
        // The follow-up completes.
        opencode.set_activity("ses_fu2", Activity::Idle);
        opencode.set_snapshot("ses_fu2", FakeOpenCode::completed_snapshot());
        Watcher::new(ctx.clone(), std::time::Duration::from_millis(10))
            .tick()
            .await
            .expect("tick");
        opencode.set_activity("ses_fu2", Activity::Busy);
    }
    let row = store
        .action(seeded.action_id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(row.state, State::Reported);
    assert_eq!(row.followups, 2);

    let posts = discord.posts();
    let reports: Vec<_> = posts
        .iter()
        .filter(|post| post.message.content.contains("✅ Completed"))
        .collect();
    assert_eq!(reports.len(), 2);
    assert!(
        reports[1].message.content.contains("Park & redo"),
        "the second follow-up's report nudges: {}",
        reports[1].message.content
    );
    assert!(!reports[0].message.content.contains("several follow-ups"));
}

#[tokio::test]
async fn followup_refuses_after_a_reset_fr29() {
    let tmp = TempDir::new().expect("tmp");
    let repo = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    init_repo(&repo);
    write_file(&repo, "README.md", "# work\n");
    let base = common::commit_all(&repo, "initial");
    write_file(&repo, "feature.txt", "f\n");
    let start = common::commit_all(&repo, "work");

    let store = StoreHandle::open_in_memory().expect("store");
    let seeded = common::seed_reported(
        &store,
        &repo_str(&repo),
        Some("main"),
        Some(&start),
        CHANNEL,
        "poke",
        "prompt",
        "ses_fu3",
    )
    .await;

    // The tree was reset below the run's start.
    let status = common::run_git(&repo, &["reset", "--hard", &base]);
    assert!(status.status.success());
    assert_ne!(common::git_stdout(&repo, &["rev-parse", "HEAD"]), start);

    let opencode = FakeOpenCode::new();
    let discord = FakeDiscord::new();
    let ctx = ctx_for(&store, &repo, Arc::new(opencode), &discord);

    let error = followup::execute_followup(&ctx, seeded.action_id, "again", OPERATOR)
        .await
        .expect_err("FR-29 refusal");
    assert!(error.to_string().contains("FR-29"), "error: {error}");
    assert_eq!(
        store
            .action(seeded.action_id)
            .await
            .expect("read")
            .expect("row")
            .state,
        State::Reported,
        "the action stays decidable"
    );
}
