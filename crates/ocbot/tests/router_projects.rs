//! SPEC-005 phase 10 (router update) behavioural coverage: every project
//! `custom_id` family and the `/projects` slash command driven through the
//! real `router::guarded_plan` / `router::execute` seam against a live
//! filesystem project root.
//!
//! The flow-level tests (`projects_flow.rs`) call the handlers directly; this
//! file pins the dispatch layer that leads to them: the route table, the
//! token/page/pair parsers (`UnknownCustomId` on malformed ids), the fail
//! closed behaviour of stale tokens, and the renderer↔router round trip that
//! keeps any future project control from being rendered without a route.

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
use ocbot::app::interaction::{AckPlan, InteractionSource, SourceKind, TextInput};
use ocbot::app::router::{self, PlanOutcome};
use ocbot_core::error::ProjectError;
use ocbot_core::paths::{plan_action_token, project_action_token};
use ocbot_core::ports::{
    PlanInfo, ProjectDetails, ProjectInfo, ProjectPort, default_plans_dirs, default_readme_files,
};

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

struct Rig {
    _temp_dir: tempfile::TempDir,
    ctx: Arc<ocbot::app::context::AppContext>,
    chat: FakeChat,
    store: StoreHandle,
    project_name: String,
    project_token: String,
    plan_token: String,
    private_path: String,
}

/// A project root in a temp directory holding one project with a long
/// (multi-page) README and one plan file whose single depth-1 heading never
/// triggers the plan picker (`DEFAULT_HEADING_MIN` is 2).
fn rig() -> Rig {
    let temp_dir = tempfile::tempdir().expect("temporary project root");
    let project_dir = temp_dir.path().join("project-alpha");
    let plans_dir = project_dir.join("devdocs").join("plans");
    std::fs::create_dir_all(&plans_dir).expect("plan directory");
    let readme = format!(
        "# Project Alpha\nA sample project.\n{}\nEnd of README.\n",
        "Additional project documentation.\n".repeat(100)
    );
    std::fs::write(project_dir.join("README.md"), readme).expect("README");
    std::fs::write(plans_dir.join("roadmap.md"), "# Roadmap\n\n1. First step\n").expect("plan");

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
            temp_dir.path().to_path_buf(),
            default_plans_dirs(),
            default_readme_files(),
            Arc::new(TomlProjectConfigAdapter),
        ));
        context.projects_root = temp_dir.path().to_path_buf();
    }
    let project_name = ctx
        .projects
        .list_projects()
        .expect("projects")
        .first()
        .expect("one project")
        .name
        .clone();
    let project_token = project_action_token(&project_name);
    let plan_token = plan_action_token(&project_name, "roadmap.md");
    let private_path = temp_dir.path().to_string_lossy().into_owned();
    Rig {
        _temp_dir: temp_dir,
        ctx,
        chat,
        store,
        project_name,
        project_token,
        plan_token,
        private_path,
    }
}

/// Component press. `message_id` stays `None` so previews post a fresh message
/// (a `Some` id edits the interaction message, which the flow tests already
/// pin); every message this file asserts on is observable as a post.
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

fn slash(name: &str) -> InteractionSource {
    InteractionSource {
        kind: SourceKind::SlashCommand {
            name: name.to_owned(),
        },
        user_id: format!("discord:{OPERATOR}"),
        destination: ocbot_core::Destination::discord(CHANNEL),
        guild_id: Some(1),
        message_id: None,
        is_dm: false,
    }
}

fn modal_submit(custom_id: &str, values: &[(&str, &str)]) -> InteractionSource {
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

/// Nothing this file posts may carry the project root's absolute path, in the
/// text or in a button id (SPEC-005 G-01/G-02).
fn assert_no_paths(rig: &Rig) {
    for post in rig.chat.posts() {
        assert!(
            !post.message.content.contains(&rig.private_path),
            "path leaked into {:?}",
            post.message.content
        );
        for button in &post.message.components {
            assert!(
                !button.custom_id.contains(&rig.private_path),
                "path leaked into custom_id {:?}",
                button.custom_id
            );
        }
    }
}

async fn plan_of(rig: &Rig, source: &InteractionSource) -> PlanOutcome {
    router::guarded_plan(&rig.ctx, source).await
}

#[tokio::test]
async fn the_projects_slash_command_plans_a_public_defer_and_lists_projects() {
    let rig = rig();

    assert!(matches!(
        plan_of(&rig, &slash("projects")).await,
        PlanOutcome::Plan(AckPlan::DeferPublic)
    ));
    let outcome = router::execute(&rig.ctx, &slash("projects"))
        .await
        .expect("projects executes");
    assert_eq!(outcome, None, "the slash route posts, it adds no followup");

    let list = rig.chat.last_post().expect("project list").message;
    assert!(list.content.contains("**Projects**"));
    assert!(list.content.contains(&rig.project_name));
    assert_no_paths(&rig);
}

#[tokio::test]
async fn project_navigation_routes_render_the_right_pages() {
    let rig = rig();
    let project = rig.project_token.clone();

    // The "Back to Projects" button replays the list.
    assert!(matches!(
        plan_of(&rig, &component("v1:projects")).await,
        PlanOutcome::Plan(AckPlan::DeferUpdate)
    ));
    assert_eq!(
        router::execute(&rig.ctx, &component("v1:projects"))
            .await
            .expect("list route"),
        None
    );
    assert!(
        rig.chat
            .last_post()
            .expect("project list")
            .message
            .content
            .contains("**Projects**")
    );

    // A page beyond the last one clamps instead of failing (stale pager).
    assert_eq!(
        router::execute(&rig.ctx, &component("v1:projects:p:5"))
            .await
            .expect("paged list route"),
        None
    );
    assert!(
        rig.chat
            .last_post()
            .expect("paged list")
            .message
            .content
            .contains("Page 1 of 1")
    );

    // Project pagination clamps an out-of-range page the same way.
    assert_eq!(
        router::execute(
            &rig.ctx,
            &component(&format!("v1:project_page:{project}:9"))
        )
        .await
        .expect("project page route"),
        None
    );
    assert!(
        rig.chat
            .last_post()
            .expect("project page")
            .message
            .content
            .contains("Plans · page 1 of 1")
    );

    // The full README opens on its first page; page navigation resolves later
    // pages of the same document.
    assert_eq!(
        router::execute(&rig.ctx, &component(&format!("v1:readme_full:{project}")))
            .await
            .expect("readme route"),
        None
    );
    assert!(
        rig.chat
            .last_post()
            .expect("readme page 1")
            .message
            .content
            .contains("Page 1 of")
    );
    assert_eq!(
        router::execute(&rig.ctx, &component(&format!("v1:readme_page:{project}:1")))
            .await
            .expect("readme page route"),
        None
    );
    let second = rig
        .chat
        .last_post()
        .expect("readme page 2")
        .message
        .content
        .clone();
    assert!(second.contains("Page 2 of"), "{second}");
    assert!(
        second.contains("Additional project documentation."),
        "{second}"
    );

    assert_no_paths(&rig);
}

#[tokio::test]
async fn project_selection_and_run_routes_commit_through_the_router() {
    let rig = rig();
    let project = rig.project_token.clone();
    let plan = rig.plan_token.clone();

    // Selecting a project persists the resolved directory and shows details.
    assert_eq!(
        router::execute(
            &rig.ctx,
            &component(&format!("v1:project_select:{project}"))
        )
        .await
        .expect("select route"),
        None
    );
    let details = rig
        .chat
        .last_post()
        .expect("project details")
        .message
        .content
        .clone();
    assert!(details.contains(&rig.project_name), "{details}");
    assert!(details.contains("README Preview"), "{details}");
    let (name, dir, selected_plan, _) = rig
        .store
        .get_selected_project_and_plan(&format!("discord:{OPERATOR}"))
        .await
        .expect("read selection")
        .expect("selection persisted by the route");
    assert_eq!(name, rig.project_name);
    assert!(dir.ends_with("project-alpha"), "{dir}");
    assert!(selected_plan.is_none());

    // Selecting a plan persists it, then the load route stages the run.
    assert_eq!(
        router::execute(
            &rig.ctx,
            &component(&format!("v1:plan_select:{project}:{plan}"))
        )
        .await
        .expect("plan select route"),
        None
    );
    let preview = rig
        .chat
        .last_post()
        .expect("plan preview")
        .message
        .content
        .clone();
    assert!(preview.contains("**Plan: `roadmap.md`**"), "{preview}");
    let (name, _, selected_plan, plan_path) = rig
        .store
        .get_selected_project_and_plan(&format!("discord:{OPERATOR}"))
        .await
        .expect("read selection")
        .expect("still selected");
    assert_eq!(name, rig.project_name);
    assert_eq!(selected_plan.as_deref(), Some("roadmap.md"));
    assert!(plan_path.expect("plan path").ends_with("roadmap.md"));

    assert!(matches!(
        plan_of(&rig, &component(&format!("v1:load_plan:{project}:{plan}"))).await,
        PlanOutcome::Plan(AckPlan::DeferUpdate)
    ));
    let staged = router::execute(
        &rig.ctx,
        &component(&format!("v1:load_plan:{project}:{plan}")),
    )
    .await
    .expect("load plan route")
    .expect("staging returns the next-step hint");
    assert!(staged.contains("staged"), "{staged}");

    // Run Without Plan: the button plans the label modal and is never executed.
    assert!(matches!(
        plan_of(&rig, &component(&format!("v1:run_noplan:{project}"))).await,
        PlanOutcome::Plan(AckPlan::Modal(_))
    ));
    assert!(matches!(
        router::execute(&rig.ctx, &component(&format!("v1:run_noplan:{project}"))).await,
        Err(AppError::AlreadyHandled)
    ));

    assert_no_paths(&rig);
}

#[tokio::test]
async fn malformed_project_custom_ids_plan_a_defer_then_fail_loudly_in_execute() {
    // The plan phase is textual (prefix match), so these still ack with a
    // deferral (AC-1: no port call); the execute phase must then reject them
    // as `UnknownCustomId` — never a silent drop, never a path-bearing error.
    let rig = rig();
    let project = rig.project_token.clone();
    let plan = rig.plan_token.clone();
    let cases: Vec<(String, InteractionSource)> = vec![
        ("v1:projects:p:x".to_owned(), component("v1:projects:p:x")),
        ("v1:projects:p:-1".to_owned(), component("v1:projects:p:-1")),
        (
            "v1:project_select:short".to_owned(),
            component("v1:project_select:short"),
        ),
        (
            "v1:project_select:zzzzzzzzzzzzzzzz".to_owned(),
            component("v1:project_select:zzzzzzzzzzzzzzzz"),
        ),
        (
            format!("v1:project_select:{project}:extra"),
            component(&format!("v1:project_select:{project}:extra")),
        ),
        (
            format!("v1:project_page:{project}"),
            component(&format!("v1:project_page:{project}")),
        ),
        (
            format!("v1:project_page:{project}:x"),
            component(&format!("v1:project_page:{project}:x")),
        ),
        (
            "v1:project_page:short:1".to_owned(),
            component("v1:project_page:short:1"),
        ),
        ("v1:readme_full:".to_owned(), component("v1:readme_full:")),
        (
            format!("v1:readme_full:{project}:more"),
            component(&format!("v1:readme_full:{project}:more")),
        ),
        (
            format!("v1:readme_page:{project}"),
            component(&format!("v1:readme_page:{project}")),
        ),
        (
            format!("v1:readme_page:{project}:y"),
            component(&format!("v1:readme_page:{project}:y")),
        ),
        (
            "v1:readme_page:zzzzzzzzzzzzzzzz:1".to_owned(),
            component("v1:readme_page:zzzzzzzzzzzzzzzz:1"),
        ),
        (
            format!("v1:plan_select:{project}"),
            component(&format!("v1:plan_select:{project}")),
        ),
        (
            format!("v1:plan_select:{project}:short"),
            component(&format!("v1:plan_select:{project}:short")),
        ),
        (
            "v1:plan_select:zzzzzzzzzzzzzzzz:0123456789abcdef".to_owned(),
            component("v1:plan_select:zzzzzzzzzzzzzzzz:0123456789abcdef"),
        ),
        (
            format!("v1:load_plan:{project}"),
            component(&format!("v1:load_plan:{project}")),
        ),
        (
            "v1:load_plan:zz:0123456789abcdef".to_owned(),
            component("v1:load_plan:zz:0123456789abcdef"),
        ),
        (
            format!("v1:run_noplan_go:{plan}x"),
            modal_submit(&format!("v1:run_noplan_go:{plan}x"), &[("label", "sneaky")]),
        ),
    ];
    for (name, source) in cases {
        assert!(
            matches!(
                plan_of(&rig, &source).await,
                PlanOutcome::Plan(AckPlan::DeferUpdate)
            ),
            "{name} must still ack (the plan is textual)"
        );
        let error = router::execute(&rig.ctx, &source)
            .await
            .expect_err("{name} must be rejected by the parser");
        assert!(
            matches!(error, AppError::UnknownCustomId(_)),
            "{name}: {error}"
        );
        assert!(!error.to_string().contains(&rig.private_path), "{error}");
    }

    // Every rejected id still acked, but nothing was ever posted.
    assert!(rig.chat.posts().is_empty(), "no route ran for a bad id");

    // The two final `else` arms are defense-in-depth: the adapter only calls
    // `execute` for interactions whose plan phase passed, so an id that
    // reached here is a bug elsewhere. They must still refuse loudly.
    assert!(matches!(
        router::execute(&rig.ctx, &component("v1:widget:1")).await,
        Err(AppError::UnknownCustomId(_))
    ));
    assert!(matches!(
        router::execute(&rig.ctx, &modal_submit("v1:bogusgo:1", &[("x", "y")])).await,
        Err(AppError::UnknownCustomId(_))
    ));
}

#[tokio::test]
async fn stale_project_tokens_fail_closed_through_the_router() {
    let rig = rig();
    let project = rig.project_token.clone();
    let plan = rig.plan_token.clone();
    let ghost_project = project_action_token("ghost/only-in-a-stale-page");
    let ghost_plan = plan_action_token(&rig.project_name, "removed.md");

    // Loading without any stored selection is refused before any path lookup.
    let error = router::execute(
        &rig.ctx,
        &component(&format!("v1:load_plan:{project}:{plan}")),
    )
    .await
    .expect_err("no selection yet");
    assert!(matches!(error, AppError::Other(_)), "{error}");

    // Right-shaped tokens that resolve to nothing are a plain not-found.
    for id in [
        format!("v1:project_select:{ghost_project}"),
        format!("v1:project_page:{ghost_project}:0"),
        format!("v1:readme_full:{ghost_project}"),
        format!("v1:readme_page:{ghost_project}:0"),
        format!("v1:plan_select:{ghost_project}:{ghost_plan}"),
    ] {
        let error = router::execute(&rig.ctx, &component(&id))
            .await
            .expect_err(&id);
        assert!(matches!(error, AppError::NotFound(_)), "{id}: {error}");
        assert!(
            !error.to_string().contains(&rig.private_path),
            "{id}: {error}"
        );
    }

    // A stale page for the live project naming a plan that is gone: selecting
    // it is a not-found. With a real plan stored, loading the ghost plan (or
    // the real plan under a ghost project) is a replay, never a run.
    router::execute(
        &rig.ctx,
        &component(&format!("v1:project_select:{project}")),
    )
    .await
    .expect("select");
    router::execute(
        &rig.ctx,
        &component(&format!("v1:plan_select:{project}:{plan}")),
    )
    .await
    .expect("select the real plan");
    let error = router::execute(
        &rig.ctx,
        &component(&format!("v1:plan_select:{project}:{ghost_plan}")),
    )
    .await
    .expect_err("ghost plan");
    assert!(matches!(error, AppError::NotFound(_)), "{error}");
    let error = router::execute(
        &rig.ctx,
        &component(&format!("v1:load_plan:{project}:{ghost_plan}")),
    )
    .await
    .expect_err("ghost plan load");
    assert!(matches!(error, AppError::AlreadyHandled), "{error}");
    let error = router::execute(
        &rig.ctx,
        &component(&format!("v1:load_plan:{ghost_project}:{plan}")),
    )
    .await
    .expect_err("ghost project load");
    assert!(matches!(error, AppError::AlreadyHandled), "{error}");

    // A modal submitted for a ghost project against a live selection is a
    // replay, not a run against the stored directory.
    let error = router::execute(
        &rig.ctx,
        &modal_submit(
            &format!("v1:run_noplan_go:{ghost_project}"),
            &[("label", "sneaky")],
        ),
    )
    .await
    .expect_err("ghost project modal");
    assert!(matches!(error, AppError::AlreadyHandled), "{error}");

    assert_no_paths(&rig);
}

#[tokio::test]
async fn run_noplan_modal_submits_stage_a_labelled_run_through_the_router() {
    let rig = rig();
    let project = rig.project_token.clone();
    router::execute(
        &rig.ctx,
        &component(&format!("v1:project_select:{project}")),
    )
    .await
    .expect("select project");

    let submit = modal_submit(
        &format!("v1:run_noplan_go:{project}"),
        &[("label", "  morning sweep  ")],
    );
    assert!(matches!(
        plan_of(&rig, &submit).await,
        PlanOutcome::Plan(AckPlan::DeferUpdate)
    ));
    let staged = router::execute(&rig.ctx, &submit)
        .await
        .expect("labelled run stages")
        .expect("staging hint");
    assert!(staged.contains("staged"), "{staged}");

    let preview = rig.chat.last_post().expect("staged preview").message;
    assert!(
        preview.content.contains("morning sweep"),
        "the label is trimmed and carried: {}",
        preview.content
    );
    assert!(preview.content.contains(&rig.project_name));
    assert_no_paths(&rig);
}

/// Renderer↔router round trip (SPEC-005 phase 10): every `custom_id` template
/// `ocbot-core/src/render/projects.rs` emits must resolve through the real
/// router. A control the router cannot answer is a button that acks and then
/// silently dies — this test makes that a compile-time neighbour instead.
#[tokio::test]
async fn every_project_control_the_renderer_emits_is_routed() {
    let rig = rig();

    // Seed a staged action so the staged-preview templates resolve.
    let dir = "/tmp/router-projects-render";
    rig.store
        .upsert_project(dir, None, None)
        .await
        .expect("project");
    let run = rig.store.insert_run(dir, "free").await.expect("run");
    let staged = rig
        .store
        .insert_action(run, Some("7"), "task", "prompt", 1)
        .await
        .expect("action");

    // The renderer source, minus its own test module, at compile time.
    let renderer = include_str!("../../ocbot-core/src/render/projects.rs");
    let renderer = renderer
        .split("#[cfg(test)]")
        .next()
        .expect("the module has a renderer section");
    let mut templates: Vec<String> = Vec::new();
    let mut unresolved: Vec<String> = Vec::new();
    for literal in renderer
        .split('"')
        .skip(1)
        .step_by(2)
        .filter(|literal| literal.starts_with("v1:"))
    {
        let id = literal
            .replace("{project_ref}", &rig.project_token)
            .replace("{plan_ref}", &rig.plan_token)
            .replace("{target}", "0")
            .replace("{action_id}", &staged.to_string());
        if id.contains('{') || id.ends_with(':') {
            unresolved.push(literal.to_owned());
        } else {
            templates.push(id);
        }
    }
    assert!(
        unresolved.is_empty(),
        "renderer template(s) {unresolved:?} carry a placeholder this test \
         cannot resolve: extend the sample table, then verify the route"
    );
    templates.sort();
    templates.dedup();
    assert!(
        templates.len() >= 9,
        "expected the whole project family, got {templates:?}"
    );

    for id in templates {
        let outcome = plan_of(&rig, &component(&id)).await;
        assert!(
            !matches!(outcome, PlanOutcome::Failed(_)),
            "renderer emitted {id} but the plan phase rejects it"
        );
        // A deferred ack is dispatched in the execute pass; a modal-planned
        // button is never executed by the adapter (its work runs on submit).
        if matches!(outcome, PlanOutcome::Plan(AckPlan::DeferUpdate)) {
            let result = router::execute(&rig.ctx, &component(&id)).await;
            assert!(
                !matches!(result, Err(AppError::UnknownCustomId(_))),
                "renderer emitted {id} but execute has no route for it"
            );
        }
    }
}

/// The tokens the routes carry are opaque: fixed-length hex, never a path and
/// never a filename (SPEC-005 G-01/G-02 underpins every route assertion above).
#[test]
fn project_tokens_used_by_the_routes_are_opaque() {
    let name = "lex/project-alpha";
    let token = project_action_token(name);
    assert_eq!(token.len(), 16, "tokens stay inside the custom_id cap");
    assert!(!token.contains('/'), "{token} must not carry a path");
    let plan = plan_action_token(name, "spec-auth.md");
    assert_eq!(plan.len(), 16);
    assert!(!plan.contains("spec"), "{plan} must not carry a filename");
}

/// A programmable project port: the failure classes below cannot be provoked
/// deliberately through the real filesystem adapter (duplicate hash tokens,
/// a not-configured root), but the router must still route them fail-closed.
/// Pinning them here also proves the route→handler wiring on the error path.
#[derive(Clone)]
struct FakeProjects {
    list: Result<Vec<ProjectInfo>, ProjectError>,
    details: Result<ProjectDetails, ProjectError>,
}

impl ProjectPort for FakeProjects {
    fn list_projects(&self) -> Result<Vec<ProjectInfo>, ProjectError> {
        self.list.clone()
    }
    fn get_project_info(&self, _project_name: &str) -> Result<ProjectDetails, ProjectError> {
        self.details.clone()
    }
    fn get_readme(&self, _project_name: &str) -> Result<Option<String>, ProjectError> {
        Ok(None)
    }
    fn get_plan_content(
        &self,
        _project_name: &str,
        _plan_name: &str,
    ) -> Result<String, ProjectError> {
        Ok(String::new())
    }
    fn get_project_dir(&self, _project_name: &str) -> Result<PathBuf, ProjectError> {
        Ok(PathBuf::from("/private/fake"))
    }
    fn get_plan_path(
        &self,
        _project_name: &str,
        _plan_name: &str,
    ) -> Result<PathBuf, ProjectError> {
        Ok(PathBuf::from("/private/fake/plan.md"))
    }
}

fn install_fake(rig: &mut Rig, fake: FakeProjects) {
    let context = Arc::get_mut(&mut rig.ctx).expect("exclusive test context");
    context.projects = Arc::new(fake);
}

#[tokio::test]
async fn project_root_error_classes_route_fail_closed() {
    let mut rig = rig();

    // Empty root: the NoProjects class maps to the "No projects found." page.
    install_fake(
        &mut rig,
        FakeProjects {
            list: Err(ProjectError::NoProjects("/private/fake-root".to_owned())),
            details: Err(ProjectError::NotConfigured),
        },
    );
    router::execute(&rig.ctx, &slash("projects"))
        .await
        .expect("NoProjects maps to the empty list");
    assert!(
        rig.chat
            .last_post()
            .expect("empty list")
            .message
            .content
            .contains("No projects found")
    );

    // Not configured: the failure *class* surfaces, never the root path.
    install_fake(
        &mut rig,
        FakeProjects {
            list: Err(ProjectError::NotConfigured),
            details: Err(ProjectError::NotConfigured),
        },
    );
    let error = router::execute(&rig.ctx, &slash("projects"))
        .await
        .expect_err("not configured");
    assert!(
        error
            .to_string()
            .contains("projects_root is not configured"),
        "{error}"
    );
    assert!(
        !error.to_string().contains("/private/fake-root"),
        "the fake root path stays log-only: {error}"
    );
}

#[tokio::test]
async fn duplicate_tokens_and_missing_plans_route_fail_closed() {
    let mut rig = rig();
    let dupe = "dupe/alpha";
    let dupe_token = project_action_token(dupe);
    let dupe_plan_token = plan_action_token(dupe, "x.md");
    let single = || {
        Ok(vec![ProjectInfo {
            name: dupe.to_owned(),
            dir: PathBuf::from("/private/a"),
        }])
    };

    // Two projects resolving to one token: selection is ambiguous, not first-match.
    install_fake(
        &mut rig,
        FakeProjects {
            list: Ok(vec![
                ProjectInfo {
                    name: dupe.to_owned(),
                    dir: PathBuf::from("/private/a"),
                },
                ProjectInfo {
                    name: dupe.to_owned(),
                    dir: PathBuf::from("/private/b"),
                },
            ]),
            details: Err(ProjectError::NotFound(dupe.to_owned())),
        },
    );
    let error = router::execute(
        &rig.ctx,
        &component(&format!("v1:project_select:{dupe_token}")),
    )
    .await
    .expect_err("duplicate token");
    assert!(error.to_string().contains("ambiguous project"), "{error}");

    // NoPlans class surfaces through the plan-select route.
    install_fake(
        &mut rig,
        FakeProjects {
            list: single(),
            details: Err(ProjectError::NoPlans(dupe.to_owned())),
        },
    );
    let error = router::execute(
        &rig.ctx,
        &component(&format!("v1:plan_select:{dupe_token}:{dupe_plan_token}")),
    )
    .await
    .expect_err("no plans");
    assert!(
        error.to_string().contains(&format!("no plans in {dupe}")),
        "{error}"
    );

    // Two plans resolving to one token: same fail-closed treatment.
    install_fake(
        &mut rig,
        FakeProjects {
            list: single(),
            details: Ok(ProjectDetails {
                name: dupe.to_owned(),
                readme_preview: None,
                plans: vec![
                    PlanInfo {
                        name: "x.md".to_owned(),
                        path: PathBuf::from("/private/x.md"),
                    },
                    PlanInfo {
                        name: "x.md".to_owned(),
                        path: PathBuf::from("/private/other/x.md"),
                    },
                ],
            }),
        },
    );
    let error = router::execute(
        &rig.ctx,
        &component(&format!("v1:plan_select:{dupe_token}:{dupe_plan_token}")),
    )
    .await
    .expect_err("duplicate plan token");
    assert!(error.to_string().contains("ambiguous plan"), "{error}");
}
