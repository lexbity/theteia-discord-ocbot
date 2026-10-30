//! Project selection and plan controls resolve opaque tokens without exposing
//! paths in user-facing messages.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use common::{FakeChat, FakeGit, FakeOpenCode};
use ocbot::adapters::projects::{FsProjectAdapter, TomlProjectConfigAdapter};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::error::AppError;
use ocbot::app::handlers::projects;
use ocbot::app::interaction::{InputStyle, InteractionSource, SourceKind};
use ocbot::app::router;
use ocbot_core::paths::{plan_action_token, project_action_token};
use ocbot_core::ports::{default_plans_dirs, default_readme_files};

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

struct ProjectRig {
    temp_dir: tempfile::TempDir,
    ctx: Arc<ocbot::app::context::AppContext>,
    chat: FakeChat,
    store: StoreHandle,
    project_name: String,
    plan_name: String,
    plan_path: PathBuf,
}

fn project_rig() -> ProjectRig {
    let temp_dir = tempfile::tempdir().expect("temporary project root");
    let project_dir = temp_dir.path().join("project-alpha");
    let plans_dir = project_dir.join("devdocs").join("plans");
    std::fs::create_dir_all(&plans_dir).expect("plan directory");
    let readme = format!(
        "# Project Alpha\nA sample project.\n{}\nEnd of README.\n",
        "Additional project documentation.\n".repeat(100)
    );
    std::fs::write(project_dir.join("README.md"), readme).expect("README");
    let plan_name = "roadmap.md".to_owned();
    let plan_path = plans_dir.join(&plan_name);
    std::fs::write(&plan_path, "# Roadmap\n\n1. First step\n").expect("plan");

    let store = StoreHandle::open_in_memory().expect("store");
    let chat = FakeChat::new();
    let mut ctx = common::context(
        store.clone(),
        Arc::new(FakeOpenCode::new()),
        Arc::new(FakeGit::inert()),
        Arc::new(chat.clone()),
        OPERATOR,
        Some(CHANNEL),
    );
    let context = Arc::get_mut(&mut ctx).expect("exclusive test context");
    context.projects = Arc::new(FsProjectAdapter::new(
        temp_dir.path().to_path_buf(),
        default_plans_dirs(),
        default_readme_files(),
        Arc::new(TomlProjectConfigAdapter),
    ));
    let projects = context
        .projects
        .list_projects()
        .expect("project discovered");
    let project_name = projects
        .first()
        .map(|project| project.name.clone())
        .expect("one project");

    ProjectRig {
        temp_dir,
        ctx,
        chat,
        store,
        project_name,
        plan_name,
        plan_path,
    }
}

fn source() -> InteractionSource {
    InteractionSource {
        kind: SourceKind::Component {
            custom_id: "test".to_owned(),
        },
        user_id: format!("discord:{OPERATOR}"),
        destination: ocbot_core::Destination::discord(CHANNEL),
        guild_id: Some(1),
        message_id: Some(10),
        is_dm: false,
    }
}

#[tokio::test]
async fn project_and_plan_tokens_drive_the_selection_flow_without_paths() {
    let rig = project_rig();
    let source = source();
    let project_token = project_action_token(&rig.project_name);
    let plan_token = plan_action_token(&rig.project_name, &rig.plan_name);
    let private_path = rig.temp_dir.path().to_string_lossy().into_owned();

    projects::list_projects(&rig.ctx, 0, &source)
        .await
        .expect("list projects");
    let list = rig.chat.last_post().expect("project list").message;
    assert!(list.content.contains(&rig.project_name));
    assert!(!list.content.contains(&private_path));
    assert_eq!(
        list.components[0].custom_id,
        format!("v1:project_select:{project_token}")
    );

    projects::select_project(&rig.ctx, &project_token, &source)
        .await
        .expect("select project");
    let details = rig.chat.last_post().expect("project details").message;
    assert!(details.content.contains(&rig.project_name));
    assert!(!details.content.contains(&private_path));
    assert!(details.components.iter().any(|button| {
        button.custom_id == format!("v1:plan_select:{project_token}:{plan_token}")
    }));

    projects::show_project_page(&rig.ctx, &project_token, 0, &source)
        .await
        .expect("show project page");
    let page = rig.chat.last_post().expect("project page").message;
    assert!(page.content.contains(&rig.plan_name));
    assert!(!page.content.contains(&private_path));

    projects::show_full_readme(&rig.ctx, &project_token, 0, &source)
        .await
        .expect("show first README page");
    let readme_page = rig.chat.last_post().expect("README page").message;
    assert!(readme_page.content.contains("Project Alpha"));
    assert!(readme_page.content.len() <= ocbot_core::DISCORD_MESSAGE_CAP);
    assert!(
        readme_page
            .components
            .iter()
            .any(|button| { button.custom_id.starts_with("v1:readme_page:") })
    );

    projects::show_full_readme(&rig.ctx, &project_token, 1, &source)
        .await
        .expect("show subsequent README page");
    let next_readme_page = rig.chat.last_post().expect("next README page").message;
    assert!(
        next_readme_page
            .content
            .contains("Additional project documentation.")
    );
    assert!(next_readme_page.content.len() <= ocbot_core::DISCORD_MESSAGE_CAP);
    assert!(!next_readme_page.content.contains(&private_path));

    projects::select_plan(&rig.ctx, &project_token, &plan_token, &source)
        .await
        .expect("select plan");
    let state = rig
        .store
        .get_selected_project_and_plan(&source.user_id)
        .await
        .expect("read selection")
        .expect("selection persisted");
    assert_eq!(state.0, rig.project_name);
    assert_eq!(state.2.as_deref(), Some(rig.plan_name.as_str()));
    assert_eq!(
        state.3.as_deref(),
        Some(rig.plan_path.to_string_lossy().as_ref())
    );

    let preview = rig.chat.last_post().expect("plan preview").message;
    assert!(preview.content.contains(&rig.plan_name));
    assert!(!preview.content.contains(&private_path));
    assert!(preview.components.iter().any(|button| {
        button.custom_id == format!("v1:load_plan:{project_token}:{plan_token}")
    }));

    projects::load_plan_and_run(&rig.ctx, &project_token, &plan_token, &source)
        .await
        .expect("load selected plan and stage run");
    let staged = rig.chat.last_post().expect("staged run").message;
    assert!(staged.content.contains(&rig.project_name));
    assert!(staged.content.contains(&rig.plan_name));
    assert!(!staged.content.contains(&private_path));
}

#[tokio::test]
async fn stale_project_controls_cannot_run_a_plan_for_another_selection() {
    let rig = project_rig();
    let source = source();
    let project_token = project_action_token(&rig.project_name);
    let plan_token = plan_action_token(&rig.project_name, &rig.plan_name);

    projects::select_project(&rig.ctx, &project_token, &source)
        .await
        .expect("select project");
    projects::select_plan(&rig.ctx, &project_token, &plan_token, &source)
        .await
        .expect("select plan");

    let wrong_project_token = project_action_token("another/project");
    let result =
        projects::load_plan_and_run(&rig.ctx, &wrong_project_token, &plan_token, &source).await;
    assert!(matches!(
        result,
        Err(ocbot::app::error::AppError::AlreadyHandled)
    ));
}

/// A modal-submit source for the rig user, mirroring what Discord (or the
/// Signal text form) synthesizes when the label modal is submitted.
fn modal_source(custom_id: &str, values: &[(&str, &str)]) -> InteractionSource {
    InteractionSource {
        kind: SourceKind::ModalSubmit {
            custom_id: custom_id.to_owned(),
            values: values
                .iter()
                .map(|(id, value)| ocbot::app::interaction::TextInput {
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

#[tokio::test]
async fn run_noplan_modal_carries_only_the_label_and_the_project_token() {
    let project_token = project_action_token("lex/project-alpha");
    let spec = projects::run_noplan_modal(&project_token).expect("modal spec");
    assert_eq!(
        spec.custom_id,
        format!("{}{project_token}", projects::RUN_NOPLAN_MODAL)
    );
    assert_eq!(spec.inputs.len(), 1);
    assert_eq!(spec.inputs[0].id, "label");
    assert_eq!(spec.inputs[0].style, InputStyle::Short);
    assert!(spec.inputs[0].required, "flow 7 requires a manual label");
    assert!(spec.inputs[0].value.is_none(), "no plan label exists yet");

    // The modal id is a family prefix plus one opaque token: never a path,
    // never a filename, never a project name.
    assert_eq!(spec.custom_id.matches(':').count(), 2);
    assert!(!spec.custom_id.contains('/'));
    assert!(!spec.custom_id.contains(".md"));

    // A malformed token is rejected at the (pure) plan phase, not deferred.
    assert!(matches!(
        projects::run_noplan_modal("not-a-token"),
        Err(AppError::UnknownCustomId(_))
    ));
}

#[tokio::test]
async fn run_without_plan_modal_submit_stages_a_labelled_run_through_the_router() {
    let rig = project_rig();
    let project_token = project_action_token(&rig.project_name);

    projects::select_project(&rig.ctx, &project_token, &source())
        .await
        .expect("select project");
    let spec = projects::run_noplan_modal(&project_token).expect("modal spec");
    let submit = modal_source(&spec.custom_id, &[("label", "night sweep")]);

    let outcome = router::execute(&rig.ctx, &submit)
        .await
        .expect("run without plan stages")
        .unwrap_or_default();
    assert!(outcome.contains("staged"), "{outcome}");

    let staged = rig.chat.last_post().expect("staged preview").message;
    assert!(staged.content.contains("night sweep"), "{}", staged.content);
    assert!(staged.content.contains(&rig.project_name));
    assert!(
        staged
            .components
            .iter()
            .any(|button| button.custom_id.starts_with("v1:send:")),
        "the staged preview carries the send buttons"
    );

    // No plan was attached: the staged run must not offer plan-step picks.
    assert!(
        !staged
            .components
            .iter()
            .any(|button| button.custom_id.starts_with("v1:plan:")),
        "a plan-less run never reaches the plan picker"
    );

    let private_path = rig.temp_dir.path().to_string_lossy().into_owned();
    for post in rig.chat.posts() {
        assert!(
            !post.message.content.contains(&private_path),
            "path leaked into {:?}",
            post.message.content
        );
    }
}

#[tokio::test]
async fn run_without_plan_refuses_without_a_label_or_a_selection() {
    let rig = project_rig();
    let project_token = project_action_token(&rig.project_name);
    let spec = projects::run_noplan_modal(&project_token).expect("modal spec");

    // Nothing selected yet: a token of the right shape still cannot run.
    let submit = modal_source(&spec.custom_id, &[("label", "first thing")]);
    let error = projects::run_without_plan(&rig.ctx, &project_token, &submit)
        .await
        .expect_err("no selection");
    assert!(matches!(error, AppError::Other(_)), "{error}");

    projects::select_project(&rig.ctx, &project_token, &source())
        .await
        .expect("select project");

    // A blank (or whitespace-only) label never stages a run.
    let blank = modal_source(&spec.custom_id, &[("label", "   ")]);
    let error = projects::run_without_plan(&rig.ctx, &project_token, &blank)
        .await
        .expect_err("blank label");
    assert!(error.to_string().contains("label is required"), "{error}");
}

#[tokio::test]
async fn run_without_plan_rejects_a_stale_project_selection() {
    let rig = project_rig();
    let project_token = project_action_token(&rig.project_name);
    projects::select_project(&rig.ctx, &project_token, &source())
        .await
        .expect("select project");

    // A stale page for another project cannot run against the live selection.
    let other_token = project_action_token("another/project");
    let spec = projects::run_noplan_modal(&other_token).expect("modal spec shape");
    let submit = modal_source(&spec.custom_id, &[("label", "sneaky")]);
    let error = projects::run_without_plan(&rig.ctx, &other_token, &submit)
        .await
        .expect_err("stale token");
    assert!(matches!(error, AppError::AlreadyHandled), "{error}");
}

#[tokio::test]
async fn project_errors_never_put_paths_in_front_of_the_operator() {
    // A rig whose root is a removable subdirectory: each failure class is
    // provoked by mutating the filesystem, and the resulting operator-facing
    // error is checked for path containment (SPEC-005 G-01).
    let temp_dir = tempfile::tempdir().expect("temporary parent");
    let projects_root = temp_dir.path().join("workspace");
    let alpha = projects_root.join("project-alpha");
    let plans = alpha.join("devdocs").join("plans");
    std::fs::create_dir_all(&plans).expect("plan directory");
    std::fs::write(alpha.join("README.md"), "# Alpha\nAlpha docs.\n").expect("README");
    std::fs::write(plans.join("roadmap.md"), "# Roadmap\n\n1. First step\n").expect("plan");
    std::fs::create_dir_all(projects_root.join("project-beta")).expect("second project");

    let store = StoreHandle::open_in_memory().expect("store");
    let chat = FakeChat::new();
    let mut ctx = common::context(
        store.clone(),
        Arc::new(FakeOpenCode::new()),
        Arc::new(FakeGit::inert()),
        Arc::new(chat.clone()),
        OPERATOR,
        Some(CHANNEL),
    );
    {
        let context = Arc::get_mut(&mut ctx).expect("exclusive test context");
        context.projects = Arc::new(FsProjectAdapter::new(
            projects_root.clone(),
            default_plans_dirs(),
            default_readme_files(),
            Arc::new(TomlProjectConfigAdapter),
        ));
    }
    let source = source();
    let private_path = temp_dir.path().to_string_lossy().into_owned();
    let project_name = ctx
        .projects
        .list_projects()
        .expect("projects")
        .into_iter()
        .find(|project| project.name.ends_with("project-alpha"))
        .expect("alpha discovered")
        .name;
    let project_token = project_action_token(&project_name);
    let plan_token = plan_action_token(&project_name, "roadmap.md");

    projects::select_project(&ctx, &project_token, &source)
        .await
        .expect("select project");
    projects::select_plan(&ctx, &project_token, &plan_token, &source)
        .await
        .expect("select plan");

    // ReadError arm: the plan still scans but cannot be read.
    std::fs::remove_file(plans.join("roadmap.md")).expect("remove plan file");
    std::fs::create_dir(plans.join("roadmap.md")).expect("obstructing directory");
    let error = projects::select_plan(&ctx, &project_token, &plan_token, &source)
        .await
        .expect_err("plan read fails");
    let text = error.to_string();
    assert!(text.contains("failed to read plan"), "{text}");
    assert!(!text.contains(&private_path), "{text}");

    // NotFound arm: the stored project no longer exists in the live set.
    std::fs::remove_dir_all(&alpha).expect("remove alpha");
    let error = projects::load_plan_and_run(&ctx, &project_token, &plan_token, &source)
        .await
        .expect_err("project is gone");
    let text = error.to_string();
    assert!(text.contains("not found"), "{text}");
    assert!(!text.contains(&private_path), "{text}");

    // NoProjects arm: the root is readable but holds no projects, so a fresh
    // selection resolves to the failure class only.
    std::fs::remove_dir_all(projects_root.join("project-beta")).expect("remove beta");
    let error = projects::select_project(&ctx, &project_token, &source)
        .await
        .expect_err("no projects left");
    let text = error.to_string();
    assert!(text.contains("no projects are available"), "{text}");
    assert!(!text.contains(&private_path), "{text}");

    // IoError arm: the root itself is gone; only the OS message survives.
    std::fs::remove_dir_all(&projects_root).expect("remove root");
    let error = projects::list_projects(&ctx, 0, &source)
        .await
        .expect_err("root is gone");
    let text = error.to_string();
    assert!(text.contains("failed to list projects"), "{text}");
    assert!(!text.contains(&private_path), "{text}");
    assert!(!text.contains("workspace"), "the root name stays log-only");
}

#[tokio::test]
async fn a_moved_project_tree_invalidates_stored_selections_for_both_run_paths() {
    // The project tree moved to a new absolute location that keeps the same
    // two-component display name (`root/project-alpha` under either parent).
    // The opaque token still matches, so only the live directory cross-check
    // can catch this: both run paths must refuse to stage against the stale
    // stored path.
    let temp_dir = tempfile::tempdir().expect("temporary parent");
    let alpha = |parent: &std::path::Path| {
        let plans = parent
            .join("root")
            .join("project-alpha")
            .join("devdocs")
            .join("plans");
        std::fs::create_dir_all(&plans).expect("plan directory");
        std::fs::write(plans.join("roadmap.md"), "# Roadmap\n\n1. First step\n").expect("plan");
        parent.join("root")
    };
    let old_root = alpha(&temp_dir.path().join("before"));
    let new_root = alpha(&temp_dir.path().join("after"));

    let store = StoreHandle::open_in_memory().expect("store");
    let chat = FakeChat::new();
    let mut ctx = common::context(
        store.clone(),
        Arc::new(FakeOpenCode::new()),
        Arc::new(FakeGit::inert()),
        Arc::new(chat.clone()),
        OPERATOR,
        Some(CHANNEL),
    );
    let install = |ctx: &mut ocbot::app::context::AppContext, root: &std::path::Path| {
        ctx.projects = Arc::new(FsProjectAdapter::new(
            root.to_path_buf(),
            default_plans_dirs(),
            default_readme_files(),
            Arc::new(TomlProjectConfigAdapter),
        ));
    };
    {
        let context = Arc::get_mut(&mut ctx).expect("exclusive test context");
        install(context, &old_root);
    }

    let source = source();
    let project_name = ctx
        .projects
        .list_projects()
        .expect("projects")
        .first()
        .expect("alpha discovered")
        .name
        .clone();
    let project_token = project_action_token(&project_name);
    let plan_token = plan_action_token(&project_name, "roadmap.md");

    projects::select_project(&ctx, &project_token, &source)
        .await
        .expect("select project");
    projects::select_plan(&ctx, &project_token, &plan_token, &source)
        .await
        .expect("select plan");

    // The tree now lives under a different absolute directory.
    {
        let context = Arc::get_mut(&mut ctx).expect("exclusive test context");
        install(context, &new_root);
    }

    let spec = projects::run_noplan_modal(&project_token).expect("modal spec shape");
    let submit = modal_source(&spec.custom_id, &[("label", "after the move")]);
    let error = projects::run_without_plan(&ctx, &project_token, &submit)
        .await
        .expect_err("stored directory is stale");
    assert!(matches!(error, AppError::AlreadyHandled), "{error}");

    let error = projects::load_plan_and_run(&ctx, &project_token, &plan_token, &source)
        .await
        .expect_err("stored plan path is stale");
    assert!(matches!(error, AppError::AlreadyHandled), "{error}");
}

#[tokio::test]
async fn page_and_readme_controls_on_unresolvable_tokens_fail_closed() {
    // Right-shaped tokens that no longer resolve against the live project set
    // are a plain not-found, with a message that carries no path.
    let rig = project_rig();
    let ghost = project_action_token("ghost/only-in-a-stale-page");

    let error = projects::show_project_page(&rig.ctx, &ghost, 0, &source())
        .await
        .expect_err("stale project page");
    let text = error.to_string();
    assert!(text.contains("not found"), "{text}");
    assert!(!text.contains(&rig.temp_dir.path().to_string_lossy().into_owned()));

    let error = projects::show_full_readme(&rig.ctx, &ghost, 0, &source())
        .await
        .expect_err("stale README page");
    assert!(error.to_string().contains("not found"));
}
