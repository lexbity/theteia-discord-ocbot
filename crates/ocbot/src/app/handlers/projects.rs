//! Project-centric handlers (SPEC-005).
//!
//! This module provides the handlers for the project-centric remote control interface.
//! Users interact with opaque project names and plan names only; all filesystem paths
//! are hidden from user-facing messages.

use std::sync::Arc;

use ocbot_core::render::projects::{
    full_readme_message, plan_preview_message, project_details_message, project_list_message,
};

use crate::app::context::AppContext;
use crate::app::error::AppError;
use crate::app::interaction::InteractionSource;

/// Handle /projects command.
///
/// Lists all available projects under the configured projects_root.
pub async fn list_projects(
    ctx: &Arc<AppContext>,
    source: &InteractionSource,
) -> Result<(), AppError> {
    let projects = ctx.projects.list_projects()
        .map_err(|e| AppError::Other(format!("Failed to list projects: {}", e)))?;

    let message = project_list_message(&projects, ocbot_core::mentionable_id(&source.user_id));
    ctx.chat.post(&source.destination, &message).await?;

    Ok(())
}

/// Handle project selection.
///
/// Shows project details including README preview and available plans.
pub async fn select_project(
    ctx: &Arc<AppContext>,
    project_name: &str,
    source: &InteractionSource,
) -> Result<(), AppError> {
    let details = ctx.projects.get_project_info(project_name)
        .map_err(|e| AppError::Other(format!("Failed to get project info: {}", e)))?;

    let project_dir = ctx.projects.get_project_dir(project_name)?;

    // Store selection in DB
    ctx.store.set_selected_project(
        &source.user_id,
        project_name,
        &project_dir.to_string_lossy().into_owned(),
    ).await?;

    let message = project_details_message(&details, ocbot_core::mentionable_id(&source.user_id));
    ctx.chat.post(&source.destination, &message).await?;

    Ok(())
}

/// Handle full README request.
///
/// Shows the complete README content for a project.
pub async fn show_full_readme(
    ctx: &Arc<AppContext>,
    project_name: &str,
    source: &InteractionSource,
) -> Result<(), AppError> {
    let readme = ctx.projects.get_readme(project_name)
        .map_err(|e| AppError::Other(format!("Failed to get README: {}", e)))?;

    let message = full_readme_message(project_name, readme.as_deref(), ocbot_core::mentionable_id(&source.user_id));
    ctx.chat.post(&source.destination, &message).await?;

    Ok(())
}

/// Handle plan selection.
///
/// Shows a preview of the selected plan file.
pub async fn select_plan(
    ctx: &Arc<AppContext>,
    project_name: &str,
    plan_name: &str,
    source: &InteractionSource,
) -> Result<(), AppError> {
    let content = ctx.projects.get_plan_content(project_name, plan_name)
        .map_err(|e| AppError::Other(format!("Failed to get plan content: {}", e)))?;

    let plan_path = ctx.projects.get_plan_path(project_name, plan_name)?;

    // Store plan selection in DB
    ctx.store.set_selected_plan(
        &source.user_id,
        plan_name,
        &plan_path.to_string_lossy().into_owned(),
    ).await?;

    let message = plan_preview_message(
        project_name,
        plan_name,
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
    _project_name: &str,
    plan_name: &str,
    source: &InteractionSource,
) -> Result<Option<String>, AppError> {
    // Get the stored project and plan paths
    let (_, project_dir, _, plan_path) = ctx.store.get_selected_project_and_plan(&source.user_id).await?
        .ok_or_else(|| AppError::Other("No project or plan selected".to_owned()))?;

    // Use the stored paths (already validated)
    // Import the run handler functions
    super::run::execute_newrun_with_presets(
        ctx,
        source,
        &project_dir,
        plan_path.as_deref(),
        Some(plan_name.to_owned()),
    ).await
}

/// Handle run without plan.
///
/// Stages a new run for the selected project without a plan.
pub async fn run_without_plan(
    ctx: &Arc<AppContext>,
    _project_name: &str,
    source: &InteractionSource,
) -> Result<Option<String>, AppError> {
    let (_, project_dir, ..) = ctx.store.get_selected_project_and_plan(&source.user_id).await?
        .ok_or_else(|| AppError::Other("No project selected".to_owned()))?;

    super::run::execute_newrun_with_presets(
        ctx,
        source,
        &project_dir,
        None,
        None,
    ).await
}
