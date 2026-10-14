//! Follow up (FR-18/FR-29): continue the *same* session with a new prompt.
//! The tree is kept, the directory lock stays held (P5), and the follow-up
//! count drives the Park & redo nudge in reports.

use std::sync::Arc;

use ocbot_core::{Column, State, WriteValue};

use crate::adapters::now_unix;
use crate::adapters::store::ActionRow;
use crate::app::context::AppContext;
use crate::app::error::AppError;
use crate::app::interaction::{InputStyle, ModalInputSpec, ModalSpec};

/// The decision states a follow-up may start from.
const FOLLOWUP_FROM: [State; 3] = [State::Reported, State::Errored, State::Aborted];

/// The modal plan for `v1:followup:<id>`: opens the same-session prompt,
/// prefilled with the stored prompt (P13).
pub async fn followup_modal(ctx: &Arc<AppContext>, action_id: i64) -> Result<ModalSpec, AppError> {
    let action = require_followup(ctx, action_id).await?;
    Ok(ModalSpec {
        custom_id: format!("v1:followupgo:{action_id}"),
        title: "Follow up — same session".to_owned(),
        inputs: vec![ModalInputSpec {
            id: "prompt".to_owned(),
            label: "Prompt".to_owned(),
            style: InputStyle::Paragraph,
            value: Some(action.prompt_sent.clone()),
            required: true,
        }],
    })
}

/// Execute `v1:followupgo:<id>`: FR-29 ancestor check, CAS back into
/// `running` (followups + 1), and admit the prompt to the same session.
pub async fn execute_followup(
    ctx: &Arc<AppContext>,
    action_id: i64,
    prompt: &str,
    user: &str,
) -> Result<Option<String>, AppError> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err(AppError::Other("a prompt is required".to_owned()));
    }
    let action = require_followup(ctx, action_id).await?;
    let dir = crate::app::handlers::support::directory_for(ctx, &action).await?;

    // FR-29: the run's start must still be an ancestor of HEAD. A reset (or a
    // forced rewrite) invalidates the chain; the operator must re-stage.
    if let Some(base) = action.start_head.clone() {
        let head = ctx.git.head(&dir).await?;
        if !ctx.git.is_ancestor(&dir, &base, &head).await? {
            return Err(AppError::Other(format!(
                "the worktree no longer descends from the run's start ({base}); use Park & redo or /run (FR-29)"
            )));
        }
    }

    let session = action
        .session_id
        .clone()
        .ok_or_else(|| AppError::Other("the action has no session (invariant)".to_owned()))?;

    let writes = vec![
        (Column::FollowUps, WriteValue::Int(action.followups + 1)),
        (Column::PromptSent, WriteValue::Text(prompt.to_owned())),
        (Column::LastEventAt, WriteValue::Int(now_unix())),
        // A fresh follow-up is a fresh report: clear the previous delivery
        // key so the next completion can post (P9's `report_msg_id IS NULL`).
        (Column::ReportMessageId, WriteValue::Null),
    ];
    let started = ctx
        .store
        .cas_state_from_any(action_id, &FOLLOWUP_FROM, State::Running, &writes)
        .await?;
    if !started {
        return Err(AppError::AlreadyHandled);
    }

    if let Err(error) = ctx.opencode.send_prompt(&session, prompt).await {
        // The send failed: leave the action decidable (errored), the lock held
        // until the operator decides (P5).
        ctx.store
            .cas_state(
                action_id,
                State::Running,
                State::Errored,
                &[(Column::LastEventAt, WriteValue::Int(now_unix()))],
            )
            .await?;
        return Err(AppError::OpenCode(error));
    }

    ctx.store
        .audit(user, "followup", Some(action_id), "running")
        .await?;
    Ok(Some(
        "follow-up running; the next report lands when it finishes".to_owned(),
    ))
}

async fn require_followup(ctx: &Arc<AppContext>, action_id: i64) -> Result<ActionRow, AppError> {
    let action = ctx
        .store
        .action(action_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("action {action_id}")))?;
    if !FOLLOWUP_FROM.contains(&action.state) {
        return Err(AppError::AlreadyHandled);
    }
    Ok(action)
}
