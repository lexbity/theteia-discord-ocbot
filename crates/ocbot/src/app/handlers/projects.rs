//! Project-centric handlers (SPEC-005).
//!
//! This module provides the handlers for the project-centric remote control interface.
//! Users interact with opaque project names and plan names only; all filesystem paths
//! are hidden from user-facing messages.

use std::sync::Arc;

use ocbot_core::error::ProjectError;
use ocbot_core::paths::{plan_action_token, project_action_token};
use ocbot_core::render::projects::{
    full_readme_message, plan_preview_message, project_details_message, project_list_message,
};

use crate::app::context::AppContext;
use crate::app::error::AppError;
use crate::app::interaction::InteractionSource;

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
        Err(error) => return Err(AppError::Other(format!("Failed to list projects: {error}"))),
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
        .map_err(|e| AppError::Other(format!("Failed to get project info: {e}")))?;

    let project_dir = ctx.projects.get_project_dir(&project_name)?;

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
    let details = ctx.projects.get_project_info(&project_name)?;
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
        .map_err(|e| AppError::Other(format!("Failed to get README: {e}")))?;

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
        .map_err(|e| AppError::Other(format!("Failed to get plan content: {e}")))?;

    let plan_path = ctx.projects.get_plan_path(&project_name, &plan_name)?;
    let project_dir = ctx.projects.get_project_dir(&project_name)?;

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
    let current_project_dir = ctx.projects.get_project_dir(&project_name)?;
    let current_plan_path = ctx.projects.get_plan_path(&project_name, &selected_plan)?;
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

/// Handle run without plan.
///
/// Stages a new run for the selected project without a plan.
pub async fn run_without_plan(
    ctx: &Arc<AppContext>,
    project_token: &str,
    source: &InteractionSource,
) -> Result<Option<String>, AppError> {
    let (project_name, project_dir, ..) = ctx
        .store
        .get_selected_project_and_plan(&source.user_id)
        .await?
        .ok_or_else(|| AppError::Other("No project selected".to_owned()))?;
    if project_action_token(&project_name) != project_token {
        return Err(AppError::AlreadyHandled);
    }

    super::run::execute_newrun_with_presets(ctx, source, &project_dir, None, None).await
}

fn resolve_project_name(ctx: &AppContext, token: &str) -> Result<String, AppError> {
    let projects = ctx.projects.list_projects()?;
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
    let details = ctx.projects.get_project_info(project_name)?;
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
