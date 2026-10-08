//! `/prompt` (FR-7): send a free-form prompt to the directory's *active*
//! session (creating one when none exists). The mutex and busy rules apply
//! exactly as `/run` (the launch is shared).

use std::path::Path;
use std::sync::Arc;

use ocbot_core::State;

use crate::app::context::AppContext;
use crate::app::error::AppError;
use crate::app::handlers::run;
use crate::app::interaction::{InputStyle, InteractionSource, ModalInputSpec, ModalSpec};

/// The `/prompt` modal `custom_id`.
pub const PROMPT_MODAL: &str = "v1:prompt";

/// The modal plan for `/prompt`: directory (prefilled) + prompt text.
pub async fn prompt_modal(
    ctx: &Arc<AppContext>,
    source: &InteractionSource,
) -> Result<ModalSpec, AppError> {
    let prefill = match ctx.store.ui_cwd(source.user_id).await? {
        Some(cwd) => cwd,
        None => ctx
            .store
            .recent_dirs(1)
            .await
            .ok()
            .and_then(|dirs| dirs.first().cloned())
            .unwrap_or_default(),
    };
    Ok(ModalSpec {
        custom_id: PROMPT_MODAL.to_owned(),
        title: "ocbot — prompt".to_owned(),
        inputs: vec![
            ModalInputSpec {
                id: "directory".to_owned(),
                label: "Directory".to_owned(),
                style: InputStyle::Short,
                value: Some(prefill),
                required: false,
            },
            ModalInputSpec {
                id: "prompt".to_owned(),
                label: "Prompt".to_owned(),
                style: InputStyle::Paragraph,
                value: None,
                required: true,
            },
        ],
    })
}

/// Execute the `/prompt` modal: stage, bind to the active session (or create
/// one), and launch.
pub async fn execute_prompt(
    ctx: &Arc<AppContext>,
    source: &InteractionSource,
) -> Result<Option<String>, AppError> {
    let prompt = source.input("prompt").map(str::trim).unwrap_or_default();
    if prompt.is_empty() {
        return Err(AppError::Other("a prompt is required".to_owned()));
    }
    let typed = source
        .input("directory")
        .map(str::trim)
        .unwrap_or_default()
        .to_owned();
    let directory = if typed.is_empty() {
        ctx.store.ui_cwd(source.user_id).await?.unwrap_or_default()
    } else {
        typed
    };
    if directory.is_empty() {
        return Err(AppError::Other("a directory is required".to_owned()));
    }
    let canonical = run::realpath(Path::new(&directory))
        .map_err(|error| AppError::Other(format!("directory {directory:?}: {error}")))?;
    let dir_name = canonical.to_string_lossy().into_owned();

    let (branch, base) = if ctx.git.is_repo(&canonical).await? {
        (
            ctx.git.branch(&canonical).await.ok(),
            ctx.git.head(&canonical).await.ok(),
        )
    } else {
        (None, None)
    };
    ctx.store
        .upsert_project(&dir_name, branch.as_deref(), base.as_deref())
        .await?;

    // Mutex (P5): an early refusal before staging, so a refused prompt never
    // leaves a staged row.
    if let Some(holder) = ctx.store.lock_holder(&dir_name).await? {
        return Ok(Some(format!(
            "directory is locked by {holder} (P5); /status shows the holder"
        )));
    }

    // The active session is the promptable one (FR-30); create it when absent.
    let selected = if let Some(session) = ctx
        .store
        .project(&dir_name)
        .await?
        .and_then(|project| project.active_session_id)
    {
        session
    } else {
        let session = ctx
            .opencode
            .create_session(
                &canonical,
                Some("prompt"),
                ctx.permission_ruleset.as_deref(),
            )
            .await?;
        ctx.store.set_active_session(&dir_name, &session).await?;
        ctx.store.upsert_seen(&session, Some(&dir_name)).await?;
        session
    };

    // A draft path (browser selection or uploaded attachment, P11/FR-36) is
    // referenced by absolute path and consumed once.
    let mut prompt_text = prompt.to_owned();
    if let Some(draft) = ctx.store.draft_path(source.user_id).await? {
        if std::path::Path::new(&draft).is_file() {
            prompt_text = format!("{prompt_text}\n\nAttachment: {draft}");
        }
        ctx.store.clear_draft_path(source.user_id).await?;
    }

    let channel = i64::try_from(source.channel_id).unwrap_or(i64::MAX);
    let run_id = ctx.store.insert_run(&dir_name, "free").await?;
    let label: String = prompt_text.chars().take(40).collect();
    let action_id = ctx
        .store
        .insert_action(run_id, Some(channel), &label, &prompt_text, 1)
        .await?;
    ctx.store.set_ui_cwd(source.user_id, &dir_name).await?;

    let action = ctx
        .store
        .action(action_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("action {action_id}")))?;
    let outcome = run::launch(ctx, action, Some(selected)).await?;

    // A refusal (locked/busy) leaves the action staged; don't keep it around.
    let row = ctx
        .store
        .action(action_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("action {action_id}")))?;
    let result = if row.state == State::Staged {
        ctx.store
            .cas_state(action_id, State::Staged, State::Discarded, &[])
            .await?;
        "refused"
    } else {
        "running"
    };
    ctx.store
        .audit(source.user_id, "prompt", Some(action_id), result)
        .await?;
    Ok(outcome)
}
