//! Project-centric handlers (SPEC-005).
//!
//! This module provides the handlers for the project-centric remote control interface.
//! Users interact with opaque project names and plan names only; all filesystem paths
//! are hidden from user-facing messages.

use std::sync::Arc;

use ocbot_core::error::ProjectError;
use ocbot_core::paths::{is_action_token, plan_action_token, project_action_token};
use ocbot_core::render::projects::{
    full_readme_message, plan_preview_message, project_details_message, project_list_message,
};

use crate::app::context::AppContext;
use crate::app::error::AppError;
use crate::app::interaction::{InputStyle, InteractionSource, ModalInputSpec, ModalSpec};

/// The `custom_id` prefix of the label modal the **Run Without Plan** button
/// plans (SPEC-005 flow 7). Components carry no text inputs, so the required
/// label has to be collected in a modal; the `custom_id` carries the opaque
/// project token and never a path.
pub const RUN_NOPLAN_MODAL: &str = "v1:run_noplan_go:";

/// Plan phase for **Run Without Plan**: the label modal, shaped for one
/// project token.
///
/// Deliberately I/O-free (ack discipline): the token is checked for shape here
/// and resolved against the live project set in [`run_without_plan`].
pub fn run_noplan_modal(project_token: &str) -> Result<ModalSpec, AppError> {
    if !is_action_token(project_token) {
        return Err(AppError::UnknownCustomId(
            "invalid project token".to_owned(),
        ));
    }
    Ok(ModalSpec {
        custom_id: format!("{RUN_NOPLAN_MODAL}{project_token}"),
        title: "Run without plan".to_owned(),
        inputs: vec![ModalInputSpec {
            id: "label".to_owned(),
            label: "Label".to_owned(),
            style: InputStyle::Short,
            value: None,
            required: true,
        }],
    })
}

/// Shape a [`ProjectError`] for the operator (SPEC-005 G-01).
///
/// Execute failures are surfaced verbatim as ephemeral text, and three
/// [`ProjectError`] variants carry an absolute path in their first field —
/// which would put a path in front of the operator. The path still goes to the
/// log; the operator gets the failure *class* only.
fn operator_error(scope: &str, error: &ProjectError) -> AppError {
    tracing::warn!(scope, error = %error, "project operation failed");
    let detail = match error {
        ProjectError::NotConfigured => "projects_root is not configured".to_owned(),
        // A display name, never a path: the operator sees these anyway.
        ProjectError::NotFound(name) => format!("not found: {name}"),
        ProjectError::NoProjects(_) => "no projects are available".to_owned(),
        ProjectError::NoPlans(name) => format!("no plans in {name}"),
        // Field 1 is a bare OS/TOML error string; the adapter never puts a
        // path in it (the path is field 0, which stays log-only).
        ProjectError::IoError(_, message) | ProjectError::ReadError(_, message) => message.clone(),
    };
    AppError::Other(format!("failed to {scope}: {detail}"))
}

/// Handle /projects command.
///
/// Lists all available projects under the configured `projects_root`.
pub async fn list_projects(
    ctx: &Arc<AppContext>,
    page: usize,
    source: &InteractionSource,
) -> Result<(), AppError> {
    let projects = match ctx.projects.list_projects() {
        Ok(projects) => projects,
        Err(ProjectError::NoProjects(_)) => Vec::new(),
        Err(error) => return Err(operator_error("list projects", &error)),
    };

    let message =
        project_list_message(&projects, page, ocbot_core::mentionable_id(&source.user_id));
    ctx.chat.post(&source.destination, &message).await?;

    Ok(())
}

/// Handle project selection.
///
/// Shows project details including README preview and available plans.
pub async fn select_project(
    ctx: &Arc<AppContext>,
    project_token: &str,
    source: &InteractionSource,
) -> Result<(), AppError> {
    let project_name = resolve_project_name(ctx, project_token)?;
    let details = ctx
        .projects
        .get_project_info(&project_name)
        .map_err(|error| operator_error("read project details", &error))?;

    let project_dir = ctx
        .projects
        .get_project_dir(&project_name)
        .map_err(|error| operator_error("resolve project", &error))?;

    // `project_state` is keyed by `ui_state.user_id`, so establish the user's
    // persistent row before saving this selection. The selected project also
    // becomes the prefilled working directory for subsequent runs.
    ctx.store
        .set_ui_cwd(&source.user_id, &project_dir.to_string_lossy())
        .await?;
    ctx.store
        .set_selected_project(
            &source.user_id,
            &project_name,
            &project_dir.to_string_lossy(),
        )
        .await?;

    let message = project_details_message(&details, 0, ocbot_core::mentionable_id(&source.user_id));
    ctx.chat.post(&source.destination, &message).await?;

    Ok(())
}

/// Render a requested page of one project's plans.
pub async fn show_project_page(
    ctx: &Arc<AppContext>,
    project_token: &str,
    page: usize,
    source: &InteractionSource,
) -> Result<(), AppError> {
    let project_name = resolve_project_name(ctx, project_token)?;
    let details = ctx
        .projects
        .get_project_info(&project_name)
        .map_err(|error| operator_error("read project details", &error))?;
    let message =
        project_details_message(&details, page, ocbot_core::mentionable_id(&source.user_id));
    ctx.chat.post(&source.destination, &message).await?;
    Ok(())
}

/// Handle full README request.
///
/// Shows the complete README content for a project.
pub async fn show_full_readme(
    ctx: &Arc<AppContext>,
    project_token: &str,
    page: usize,
    source: &InteractionSource,
) -> Result<(), AppError> {
    let project_name = resolve_project_name(ctx, project_token)?;
    let readme = ctx
        .projects
        .get_readme(&project_name)
        .map_err(|error| operator_error("read README", &error))?;

    let message = full_readme_message(
        &project_name,
        readme.as_deref(),
        page,
        ocbot_core::mentionable_id(&source.user_id),
    );
    ctx.chat.post(&source.destination, &message).await?;

    Ok(())
}

/// Handle plan selection.
///
/// Shows a preview of the selected plan file.
pub async fn select_plan(
    ctx: &Arc<AppContext>,
    project_token: &str,
    plan_token: &str,
    source: &InteractionSource,
) -> Result<(), AppError> {
    let project_name = resolve_project_name(ctx, project_token)?;
    let plan_name = resolve_plan_name(ctx, &project_name, plan_token)?;
    let content = ctx
        .projects
        .get_plan_content(&project_name, &plan_name)
        .map_err(|error| operator_error("read plan", &error))?;

    let plan_path = ctx
        .projects
        .get_plan_path(&project_name, &plan_name)
        .map_err(|error| operator_error("resolve plan", &error))?;
    let project_dir = ctx
        .projects
        .get_project_dir(&project_name)
        .map_err(|error| operator_error("resolve project", &error))?;

    // Keep project and plan state paired even when a stale component is clicked.
    ctx.store
        .set_selected_project(
            &source.user_id,
            &project_name,
            &project_dir.to_string_lossy(),
        )
        .await?;
    ctx.store
        .set_selected_plan(&source.user_id, &plan_name, &plan_path.to_string_lossy())
        .await?;

    let message = plan_preview_message(
        &project_name,
        &plan_name,
        &content,
        ocbot_core::mentionable_id(&source.user_id),
    );
    ctx.chat.post(&source.destination, &message).await?;

    Ok(())
}

/// Handle load plan and stage run.
///
/// Loads the selected plan and stages a new run with it.
pub async fn load_plan_and_run(
    ctx: &Arc<AppContext>,
    project_token: &str,
    plan_token: &str,
    source: &InteractionSource,
) -> Result<Option<String>, AppError> {
    // Get the stored project and plan paths
    let (project_name, project_dir, selected_plan, selected_plan_path) = ctx
        .store
        .get_selected_project_and_plan(&source.user_id)
        .await?
        .ok_or_else(|| AppError::Other("No project or plan selected".to_owned()))?;

    let selected_plan =
        selected_plan.ok_or_else(|| AppError::Other("No plan selected".to_owned()))?;
    if project_action_token(&project_name) != project_token
        || plan_action_token(&project_name, &selected_plan) != plan_token
    {
        return Err(AppError::AlreadyHandled);
    }
    let stored_plan_path = selected_plan_path
        .ok_or_else(|| AppError::Other("Selected plan path is missing".to_owned()))?;
    let current_project_dir = ctx
        .projects
        .get_project_dir(&project_name)
        .map_err(|error| operator_error("resolve project", &error))?;
    let current_plan_path = ctx
        .projects
        .get_plan_path(&project_name, &selected_plan)
        .map_err(|error| operator_error("resolve plan", &error))?;
    if current_project_dir.to_string_lossy() != project_dir
        || current_plan_path.to_string_lossy() != stored_plan_path
    {
        return Err(AppError::AlreadyHandled);
    }

    // Use the stored paths (already validated)
    // Import the run handler functions
    super::run::execute_newrun_with_presets(
        ctx,
        source,
        &project_dir,
        Some(&stored_plan_path),
        Some(selected_plan),
    )
    .await
}

/// Handle the submitted **Run Without Plan** label modal (SPEC-005 flow 7).
///
/// Runs on the modal submit, not the button: components carry no text inputs,
/// so the label the flow requires is collected in [`run_noplan_modal`] and
/// arrives here. The project comes from the stored selection, and the modal's
/// token must match it — a stale page's submit is a replay, not a run.
pub async fn run_without_plan(
    ctx: &Arc<AppContext>,
    project_token: &str,
    source: &InteractionSource,
) -> Result<Option<String>, AppError> {
    let label = source.input("label").map(str::trim).unwrap_or_default();
    if label.is_empty() {
        return Err(AppError::Other("a label is required".to_owned()));
    }
    let (project_name, project_dir, ..) = ctx
        .store
        .get_selected_project_and_plan(&source.user_id)
        .await?
        .ok_or_else(|| AppError::Other("No project selected".to_owned()))?;
    if project_action_token(&project_name) != project_token {
        return Err(AppError::AlreadyHandled);
    }
    // Re-resolve the token against the live project set: a project removed
    // from disk since selection must not stage a run against a stale path.
    let current_project_dir = ctx
        .projects
        .get_project_dir(&project_name)
        .map_err(|error| operator_error("resolve project", &error))?;
    if current_project_dir.to_string_lossy() != project_dir {
        return Err(AppError::AlreadyHandled);
    }

    super::run::execute_newrun_with_presets(ctx, source, &project_dir, None, Some(label.to_owned()))
        .await
}

fn resolve_project_name(ctx: &AppContext, token: &str) -> Result<String, AppError> {
    let projects = ctx
        .projects
        .list_projects()
        .map_err(|error| operator_error("list projects", &error))?;
    let mut matches = projects
        .into_iter()
        .filter(|project| project_action_token(&project.name) == token);
    let Some(project) = matches.next() else {
        return Err(AppError::NotFound("project selection".to_owned()));
    };
    if matches.next().is_some() {
        return Err(AppError::Other("ambiguous project selection".to_owned()));
    }
    Ok(project.name)
}

fn resolve_plan_name(
    ctx: &AppContext,
    project_name: &str,
    token: &str,
) -> Result<String, AppError> {
    let details = ctx
        .projects
        .get_project_info(project_name)
        .map_err(|error| operator_error("read project details", &error))?;
    let mut matches = details
        .plans
        .into_iter()
        .filter(|plan| plan_action_token(project_name, &plan.name) == token);
    let Some(plan) = matches.next() else {
        return Err(AppError::NotFound("plan selection".to_owned()));
    };
    if matches.next().is_some() {
        return Err(AppError::Other("ambiguous plan selection".to_owned()));
    }
    Ok(plan.name)
}
