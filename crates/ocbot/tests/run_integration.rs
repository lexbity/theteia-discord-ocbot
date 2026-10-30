//! SPEC-005 phases 11-12 (run integration + report delivery): the single
//! staging core behind both `/run` entry points, the opaque preview on every
//! re-render path (pick, other-step, plan-use, template toggle), the
//! dirty-tree warning naming the project, never the worktree path, and the
//! completed report heading the project/plan with a stats-only footer.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::sync::Arc;

use common::{FakeChat, FakeGit, FakeOpenCode, append_file, init_repo, write_file};
use ocbot::adapters::projects::{FsProjectAdapter, TomlProjectConfigAdapter};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::error::AppError;
use ocbot::app::handlers::run;
use ocbot::app::interaction::{InteractionSource, SourceKind, TextInput};
use ocbot::app::router;
use ocbot_core::paths::{plan_action_token, project_action_token, project_display_name};
use ocbot_core::ports::{default_plans_dirs, default_readme_files};

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

struct Rig {
    temp_dir: tempfile::TempDir,
    ctx: Arc<ocbot::app::context::AppContext>,
    chat: FakeChat,
    store: StoreHandle,
    opencode: FakeOpenCode,
    project_token: String,
    plan_token: String,
    private_path: String,
}

/// A project root whose plan parses into pickable steps (`##` heading with
/// two list items — `DEFAULT_HEADING_MIN` is 2).
fn rig() -> Rig {
    let temp_dir = tempfile::tempdir().expect("temporary project root");
    let project_dir = temp_dir.path().join("project-alpha");
    let plans_dir = project_dir.join("devdocs").join("plans");
    std::fs::create_dir_all(&plans_dir).expect("plan directory");
    std::fs::write(project_dir.join("README.md"), "# Alpha\nDocs.\n").expect("README");
    std::fs::write(
        plans_dir.join("multi.md"),
        "## Steps\n1. first step\n2. second step\n",
    )
    .expect("plan");

    let store = StoreHandle::open_in_memory().expect("store");
    let chat = FakeChat::new();
    let opencode = FakeOpenCode::new();
    let mut ctx = common::context(
        store.clone(),
        Arc::new(opencode.clone()),
        Arc::new(FakeGit::inert()),
        Arc::new(chat.clone()),
        OPERATOR,
        Some(CHANNEL),
    );
    {
        let context = Arc::get_mut(&mut ctx).expect("exclusive test context");
        context.projects = Arc::new(FsProjectAdapter::new(
            temp_dir.path().to_path_buf(),
            default_plans_dirs(),
            default_readme_files(),
            Arc::new(TomlProjectConfigAdapter),
        ));
    }
    let project_name = ctx
        .projects
        .list_projects()
        .expect("projects")
        .first()
        .expect("one project")
        .name
        .clone();
    Rig {
        private_path: temp_dir.path().to_string_lossy().into_owned(),
        project_token: project_action_token(&project_name),
        plan_token: plan_action_token(&project_name, "multi.md"),
        temp_dir,
        ctx,
        chat,
        store,
        opencode,
    }
}

fn component(custom_id: &str) -> InteractionSource {
    InteractionSource {
        kind: SourceKind::Component {
            custom_id: custom_id.to_owned(),
        },
        user_id: format!("discord:{OPERATOR}"),
        destination: ocbot_core::Destination::discord(CHANNEL),
        guild_id: Some(1),
        message_id: None,
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
        user_id: format!("discord:{OPERATOR}"),
        destination: ocbot_core::Destination::discord(CHANNEL),
        guild_id: Some(1),
        message_id: None,
        is_dm: false,
    }
}

/// No post or edit in the chat log may carry the private root path.
fn assert_no_paths(rig: &Rig) {
    for post in rig.chat.posts() {
        assert!(
            !post.message.content.contains(&rig.private_path),
            "path leaked into {:?}",
            post.message.content
        );
    }
    for (_, _, message) in rig.chat.edits() {
        assert!(
            !message.content.contains(&rig.private_path),
            "path leaked into edit {:?}",
            message.content
        );
    }
}

/// The preview is exactly the opaque trio for the given label and template.
fn assert_preview(
    message: &ocbot_core::OutgoingMessage,
    label: &str,
    template: &str,
    project: &str,
) {
    assert_eq!(
        message.content,
        format!("**Ready to run: {label}**\n_Project: {project}_\n_Template: {template}_"),
    );
}

#[tokio::test]
async fn modal_and_preset_staging_produce_the_same_run() {
    let rig = rig();
    let dir = rig.temp_dir.path().to_path_buf();
    let dir_text = dir.to_string_lossy().into_owned();

    // The typed `/run` modal path.
    run::execute_newrun(
        &rig.ctx,
        &modal(
            run::NEW_RUN_MODAL,
            &[("directory", &dir_text), ("label", "sweep"), ("plan", "")],
        ),
    )
    .await
    .expect("modal staging");
    let modal_action = rig
        .store
        .staged_actions()
        .await
        .expect("staged")
        .pop()
        .expect("modal action");

    // The preset path (what the project flow drives) with the same inputs.
    run::execute_newrun_with_presets(
        &rig.ctx,
        &component("v1:load_plan:synthetic"),
        &dir_text,
        None,
        Some("sweep".to_owned()),
    )
    .await
    .expect("preset staging");
    let actions = rig.store.staged_actions().await.expect("staged");
    assert_eq!(actions.len(), 2, "both entry points staged");
    let preset_action = &actions[1];

    // The shared core makes them byte-identical: same rendered prompt, same
    // stored directory, same opaque preview chrome (up to the row id).
    assert_eq!(preset_action.prompt_sent, modal_action.prompt_sent);
    assert!(
        !modal_action.prompt_sent.is_empty(),
        "the prompt is stored while staying out of the preview"
    );
    let previews: Vec<String> = rig
        .chat
        .posts()
        .into_iter()
        .map(|post| {
            post.message
                .content
                .replace(&modal_action.id.to_string(), "#")
                .replace(&preset_action.id.to_string(), "#")
        })
        .collect();
    assert_eq!(previews.len(), 2);
    assert_eq!(
        previews[0], previews[1],
        "both paths render the same preview"
    );
    let project = project_display_name(&dir);
    assert_preview(
        &rig.chat.last_post().expect("preview").message,
        "sweep",
        "fresh",
        &project,
    );
    assert_no_paths(&rig);
}

/// Drive the project flow through the router up to the plan picker and return
/// the staged action id.
async fn stage_picker_action(rig: &Rig) -> i64 {
    let project = rig.project_token.clone();
    let plan = rig.plan_token.clone();

    // Selecting project + plan through the router...
    router::execute(
        &rig.ctx,
        &component(&format!("v1:project_select:{project}")),
    )
    .await
    .expect("select project");
    router::execute(
        &rig.ctx,
        &component(&format!("v1:plan_select:{project}:{plan}")),
    )
    .await
    .expect("select plan");

    // ...loading the plan with pickable steps lands on the picker, not a preview.
    let outcome = router::execute(
        &rig.ctx,
        &component(&format!("v1:load_plan:{project}:{plan}")),
    )
    .await
    .expect("load plan route")
    .expect("picker hint");
    assert!(outcome.contains("pick a plan step"), "{outcome}");
    let action = rig
        .store
        .staged_actions()
        .await
        .expect("staged")
        .pop()
        .expect("staged action");
    let picker = rig.chat.last_post().expect("picker").message;
    assert!(
        picker
            .components
            .iter()
            .any(|button| button.custom_id == format!("v1:plan:{}:1", action.id)),
        "the picker offers the second step"
    );
    action.id
}

/// The opaque display name of the project the staged action runs in.
async fn staged_project_name(rig: &Rig, action_id: i64) -> String {
    let row = rig
        .store
        .action(action_id)
        .await
        .expect("read")
        .expect("row");
    let run_row = rig.store.run(row.run_id).await.expect("run").expect("row");
    project_display_name(std::path::Path::new(&run_row.dir))
}

#[tokio::test]
async fn a_project_plan_with_steps_lands_on_the_picker_and_picks_a_step() {
    let rig = rig();
    let action_id = stage_picker_action(&rig).await;

    // The run row keeps the plan path internally (never user-facing).
    let row = rig
        .store
        .action(action_id)
        .await
        .expect("read")
        .expect("row");
    let run_row = rig.store.run(row.run_id).await.expect("run").expect("row");
    assert!(
        run_row
            .plan_path
            .as_deref()
            .is_some_and(|path| path.ends_with("devdocs/plans/multi.md")),
        "{:?}",
        run_row.plan_path
    );

    // Picking a step re-renders through the shared preview: the step becomes
    // the label, the project stays opaque.
    let project_name = staged_project_name(&rig, action_id).await;
    router::execute(&rig.ctx, &component(&format!("v1:plan:{action_id}:1")))
        .await
        .expect("pick step");
    let row = rig
        .store
        .action(action_id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(row.label, "second step");
    assert_preview(
        &rig.chat.last_post().expect("pick preview").message,
        "second step",
        "fresh",
        &project_name,
    );

    assert_no_paths(&rig);
}

#[tokio::test]
async fn every_post_pick_re_render_path_shows_the_same_opaque_preview() {
    let rig = rig();
    let action_id = stage_picker_action(&rig).await;
    router::execute(&rig.ctx, &component(&format!("v1:plan:{action_id}:1")))
        .await
        .expect("pick step");
    let project_name = staged_project_name(&rig, action_id).await;

    // "Other" step via its modal: the typed text is the label.
    router::execute(
        &rig.ctx,
        &modal(
            &format!("{}{}", run::PLAN_OTHER_MODAL, action_id),
            &[("step", "hotfix pass")],
        ),
    )
    .await
    .expect("other step");
    assert_preview(
        &rig.chat.last_post().expect("other preview").message,
        "hotfix pass",
        "fresh",
        &project_name,
    );

    // `Use as-is` keeps the current label and re-renders the preview.
    router::execute(&rig.ctx, &component(&format!("v1:planuse:{action_id}")))
        .await
        .expect("plan use");
    assert_preview(
        &rig.chat.last_post().expect("planuse preview").message,
        "hotfix pass",
        "fresh",
        &project_name,
    );

    // The template toggle re-renders through the same opaque path.
    router::execute(&rig.ctx, &component(&format!("v1:tpl:{action_id}")))
        .await
        .expect("toggle template");
    assert_preview(
        &rig.chat.last_post().expect("toggle preview").message,
        "hotfix pass",
        "resume",
        &project_name,
    );

    assert_no_paths(&rig);
}

#[tokio::test]
async fn the_dirty_tree_warning_names_the_project_not_the_path() {
    let temp_dir = tempfile::tempdir().expect("tmp");
    let repo = temp_dir.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    init_repo(&repo);
    write_file(&repo, "README.md", "# work\n");
    common::commit_all(&repo, "initial");
    let private_path = temp_dir.path().to_string_lossy().into_owned();

    let store = StoreHandle::open_in_memory().expect("store");
    let chat = FakeChat::new();
    let ctx = common::context(
        store.clone(),
        Arc::new(FakeOpenCode::new()),
        Arc::new(FakeGit::with_repo(Some(repo.clone()))),
        Arc::new(chat.clone()),
        OPERATOR,
        Some(CHANNEL),
    );

    // Stage against the repo, then make the tree dirty.
    run::execute_newrun(
        &ctx,
        &modal(
            run::NEW_RUN_MODAL,
            &[
                ("directory", &repo.to_string_lossy()),
                ("label", "feature"),
                ("plan", ""),
            ],
        ),
    )
    .await
    .expect("stage");
    let action = store
        .staged_actions()
        .await
        .expect("staged")
        .pop()
        .expect("action");
    append_file(&repo, "README.md", "dirty\n");

    // Send: FR-27 intercepts with the warning — naming the project, not the
    // absolute worktree path.
    let outcome = router::execute(&ctx, &component(&format!("v1:send:{}", action.id)))
        .await
        .expect("send route runs");
    let hint = outcome.clone().unwrap_or_default();
    assert!(hint.contains("dirty"), "{outcome:?}");
    let warning = chat.last_post().expect("warning").message;
    let project = project_display_name(&repo);
    assert!(warning.content.contains(&project), "{}", warning.content);
    assert!(
        !warning.content.contains(&private_path),
        "path leaked: {}",
        warning.content
    );
    assert!(
        warning.content.contains("uncommitted"),
        "{}",
        warning.content
    );
    assert!(
        warning
            .components
            .iter()
            .any(|button| button.custom_id == format!("v1:runany:{}", action.id)),
        "the warning carries the confirm button"
    );
    let row = store.action(action.id).await.expect("read").expect("row");
    assert_eq!(
        row.state,
        ocbot_core::State::Staged,
        "the warning does not send"
    );

    // Confirming "Run anyway" proceeds.
    let sent = router::execute(&ctx, &component(&format!("v1:runany:{}", action.id)))
        .await
        .expect("runany route");
    assert!(sent.is_some(), "the run started: {sent:?}");
}

#[tokio::test]
async fn presets_refuse_an_empty_directory_or_label_without_side_effects() {
    let rig = rig();
    let source = component("v1:load_plan:synthetic");

    let error = run::execute_newrun_with_presets(&rig.ctx, &source, "", None, Some("x".to_owned()))
        .await
        .expect_err("empty directory");
    assert!(matches!(error, AppError::Other(_)), "{error}");
    assert!(error.to_string().contains("directory"), "{error}");

    let error =
        run::execute_newrun_with_presets(&rig.ctx, &source, "/tmp", None, Some("  ".to_owned()))
            .await
            .expect_err("blank label");
    assert!(matches!(error, AppError::Other(_)), "{error}");
    assert!(error.to_string().contains("label"), "{error}");

    assert!(
        rig.store.staged_actions().await.expect("staged").is_empty(),
        "a refused preset stages nothing"
    );
}

/// Stage a preset run, send it through the router, and finalize it with a
/// completed snapshot; returns the delivered report message.
async fn deliver_report(rig: &Rig, dir: &str, plan: Option<&str>) -> ocbot_core::OutgoingMessage {
    use ocbot::app::completion::{DetectedBy, finalize_action};

    let outcome = run::execute_newrun_with_presets(
        &rig.ctx,
        &component("v1:load_plan:synthetic"),
        dir,
        plan,
        Some("solo.md".to_owned()),
    )
    .await
    .expect("stage");
    assert!(
        outcome.unwrap_or_default().contains("staged"),
        "the plan-less-shaped staging previews directly"
    );

    let staged = rig
        .store
        .staged_actions()
        .await
        .expect("staged")
        .pop()
        .expect("action");
    let started = router::execute(&rig.ctx, &component(&format!("v1:send:{}", staged.id)))
        .await
        .expect("send route")
        .expect("started hint");
    assert!(started.contains("started"), "{started}");

    let action = rig
        .store
        .action(staged.id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(action.state, ocbot_core::State::Running);
    let session = action.session_id.clone().expect("session bound");
    rig.opencode
        .set_snapshot(&session, FakeOpenCode::completed_snapshot());

    let delivered = finalize_action(&rig.ctx, &action, false, DetectedBy::Fallback)
        .await
        .expect("finalize");
    assert!(delivered, "the report went out");
    let row = rig
        .store
        .action(staged.id)
        .await
        .expect("read")
        .expect("row");
    assert_eq!(
        row.state,
        ocbot_core::State::Reported,
        "one CAS to reported"
    );

    rig.chat.last_post().expect("report").message
}

#[tokio::test]
async fn a_completed_plan_run_reports_project_plan_and_stats_without_paths() {
    let rig = rig();
    let project_dir = rig.temp_dir.path().join("project-alpha");
    let plans_dir = project_dir.join("devdocs").join("plans");
    std::fs::create_dir_all(&plans_dir).expect("plans dir");
    let plan_path = plans_dir.join("solo.md");
    std::fs::write(&plan_path, "# Solo\n1. only item\n").expect("plan");

    let report = deliver_report(
        &rig,
        &project_dir.to_string_lossy(),
        Some(&plan_path.to_string_lossy()),
    )
    .await;

    // SPEC-005 §flow 6: Project + Plan prominent, full agent response,
    // stats-only footer, no filesystem anywhere.
    assert!(
        report.content.contains(&format!(
            "**Project: {}**",
            project_display_name(&project_dir)
        )),
        "{}",
        report.content
    );
    assert!(
        report.content.contains("**Plan: solo.md**"),
        "{}",
        report.content
    );
    assert!(
        report.content.contains("✅ Completed"),
        "{}",
        report.content
    );
    assert!(report.content.contains("PONG"), "the body survives");
    assert!(report.content.contains("15 tok"), "{}", report.content);
    assert!(report.content.contains("$0.01"), "{}", report.content);
    assert!(
        !report.content.contains(&rig.private_path),
        "absolute paths leaked: {}",
        report.content
    );
    assert!(
        !report.content.contains("devdocs/plans"),
        "the plan's directory leaked: {}",
        report.content
    );
    // The decision buttons are the normal report grammar.
    assert!(
        report
            .components
            .iter()
            .any(|button| button.custom_id.starts_with("v1:followup:")),
        "follow-up is offered"
    );
    assert_no_paths(&rig);
}

#[tokio::test]
async fn a_plan_less_run_reports_the_project_and_no_plan_line() {
    let rig = rig();
    let project_dir = rig.temp_dir.path().join("project-alpha");

    let report = deliver_report(&rig, &project_dir.to_string_lossy(), None).await;

    assert!(
        report.content.contains(&format!(
            "**Project: {}**",
            project_display_name(&project_dir)
        )),
        "{}",
        report.content
    );
    assert!(
        !report.content.contains("**Plan:"),
        "no plan was given: {}",
        report.content
    );
    assert_no_paths(&rig);
}
