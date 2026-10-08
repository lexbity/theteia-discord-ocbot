//! The full Phase 6 `/run` flow (FR-6): upload a file → the path becomes the
//! plan draft → browse/pick a plan step → review with the resume template
//! showing git state → edit & send → report. A real (restricted) fs adapter, a
//! real temp git repo, a recording Discord, and a scripted opencode fake.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::sync::Arc;

use ocbot::adapters::fs::FsAdapter;
use ocbot::adapters::store::StoreHandle;
use ocbot::app::context::{AppContext, DiscordPort};
use ocbot::app::handlers::{files, run};
use ocbot::app::interaction::{InteractionSource, SourceKind, TextInput};
use ocbot::app::watcher::Watcher;
use ocbot_core::State;

use common::{Call, FakeClock, FakeDiscord, FakeGit, FakeOpenCode};

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

fn button(custom_id: &str) -> InteractionSource {
    InteractionSource {
        kind: SourceKind::Component {
            custom_id: custom_id.to_owned(),
        },
        user_id: OPERATOR,
        channel_id: CHANNEL,
        guild_id: Some(1),
        message_id: Some(10),
        is_dm: false,
    }
}

fn modal(custom_id: &str, values: &[(&str, &str)]) -> InteractionSource {
    InteractionSource {
        kind: SourceKind::ModalSubmit {
            custom_id: custom_id.to_owned(),
            values: values
                .iter()
                .map(|(id, value)| TextInput {
                    id: (*id).to_owned(),
                    value: (*value).to_owned(),
                })
                .collect(),
        },
        user_id: OPERATOR,
        channel_id: CHANNEL,
        guild_id: Some(1),
        message_id: None,
        is_dm: false,
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one continuous end-to-end protocol walk; splitting would obscure the ordering"
)]
async fn upload_browse_pick_review_edit_send_report() {
    let tmp = tempfile::tempdir().expect("tmp");
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&root).expect("root");
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).expect("repo");
    common::init_repo(&repo);
    common::write_file(&repo, "README.md", "# repo\n");
    common::commit_all(&repo, "initial");

    let store = StoreHandle::open_in_memory().expect("store");
    let opencode = FakeOpenCode::new();
    opencode.set_snapshot("ses_fake_1", FakeOpenCode::completed_snapshot());
    let clock = Arc::new(FakeClock::at(1_700_000_000));
    let fs =
        FsAdapter::new(vec![root.clone()], root.join("inbox"), false, clock.clone()).expect("fs");
    let discord = FakeDiscord::new();
    let discord_port: Arc<dyn DiscordPort> = Arc::new(discord.clone());
    let ctx: Arc<AppContext> = common::context_with_fs(
        store.clone(),
        Arc::new(opencode.clone()),
        Arc::new(FakeGit::with_repo(Some(repo.clone()))),
        Arc::new(fs),
        clock,
        None,
        common::DEFAULT_ABORT_TIMEOUT_SECS,
        discord_port,
        OPERATOR,
        Some(CHANNEL),
    );

    // 1. "Upload" a plan: the attachment path becomes the draft.
    let plan_markdown = "## Steps\n1. first step\n2. second step\n";
    let plan_path = files::ingest(&ctx, OPERATOR, "plan.md", plan_markdown.as_bytes())
        .await
        .expect("ingest plan");

    // 2. `/run` with the plan; the run records the plan path and a picker posts.
    run::execute_newrun(
        &ctx,
        &modal(
            run::NEW_RUN_MODAL,
            &[
                ("directory", &repo.to_string_lossy()),
                ("label", "implement"),
                ("plan", &plan_path),
            ],
        ),
    )
    .await
    .expect("stage with plan");
    let staged = store.staged_actions().await.expect("staged");
    assert_eq!(staged.len(), 1);
    let action_id = staged[0].id;
    assert_eq!(
        store
            .run(staged[0].run_id)
            .await
            .expect("run")
            .expect("row")
            .plan_path
            .as_deref(),
        Some(plan_path.as_str())
    );
    let picker = discord.last_post().expect("picker posted").message;
    assert!(
        picker
            .components
            .iter()
            .any(|component| component.custom_id == format!("v1:plan:{action_id}:1")),
        "picker offers both steps"
    );

    // 3. Pick the second step: the label and prompt are re-rendered.
    run::plan_pick(
        &ctx,
        action_id,
        1,
        &button(&format!("v1:plan:{action_id}:1")),
    )
    .await
    .expect("pick");
    let action = store.action(action_id).await.expect("read").expect("row");
    assert_eq!(action.label, "second step");
    assert!(
        action.prompt_sent.contains("second step"),
        "{}",
        action.prompt_sent
    );
    assert!(
        action.prompt_sent.contains(&format!("Plan: {plan_path}")),
        "{}",
        action.prompt_sent
    );

    // 4. Toggle to the resume template at review time: the git state is
    //    re-rendered into the stored prompt.
    run::toggle_template(&ctx, action_id, &button(&format!("v1:tpl:{action_id}")))
        .await
        .expect("toggle");
    let action = store.action(action_id).await.expect("read").expect("row");
    assert_eq!(action.template, "resume");
    assert!(
        action
            .prompt_sent
            .contains("Continue the task: second step"),
        "{}",
        action.prompt_sent
    );
    assert!(
        action.prompt_sent.contains("Repository state:") && action.prompt_sent.contains("initial"),
        "git state rendered at review time: {}",
        action.prompt_sent
    );
    let review = discord.edits().pop().expect("review edit").2;
    assert!(review.content.contains("template: resume"));
    assert!(
        review
            .components
            .iter()
            .any(|component| component.label == "Use fresh template")
    );

    // 5. Edit & send: the edited text is admitted verbatim.
    let edited = "EDITED: finish the second step carefully";
    run::execute_editgo(
        &ctx,
        action_id,
        &modal(&format!("v1:editgo:{action_id}"), &[("prompt", edited)]),
    )
    .await
    .expect("edit & send");
    let running = store.action(action_id).await.expect("read").expect("row");
    assert_eq!(running.state, State::Running);
    let sent = opencode
        .inner()
        .calls
        .iter()
        .find_map(|call| match call {
            Call::SendPrompt(_session, text) => Some(text.clone()),
            _ => None,
        })
        .expect("prompt admitted");
    assert_eq!(sent, edited, "the edited prompt is what was sent");

    // 6. Completion → exactly one report.
    let watcher = Watcher::new(ctx.clone(), std::time::Duration::from_millis(10));
    watcher.tick().await.expect("tick");
    let report = discord
        .posts()
        .into_iter()
        .rev()
        .find(|post| post.message.content.contains("✅ Completed"))
        .expect("report posted");
    assert!(report.message.content.contains("PONG"));
    assert_eq!(
        store
            .action(action_id)
            .await
            .expect("read")
            .expect("row")
            .state,
        State::Reported
    );
}
