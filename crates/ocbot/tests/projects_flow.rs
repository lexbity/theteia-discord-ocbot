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
use ocbot::app::handlers::projects;
use ocbot::app::interaction::{InteractionSource, SourceKind};
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
    context.projects_root = temp_dir.path().to_path_buf();
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
