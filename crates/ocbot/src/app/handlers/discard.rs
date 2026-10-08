//! Discard (FR-15): seal a finished attempt without committing, and release
//! the directory lock (P5). Available from `reported`, `errored`, `aborted`.

use std::sync::Arc;

use ocbot_core::{Column, State, WriteValue};

use crate::app::context::AppContext;
use crate::app::error::AppError;
use crate::app::handlers::support::{self, dir_text, directory_for};

/// The decision states a Discard may start from.
const DISCARD_FROM: [State; 3] = [State::Reported, State::Errored, State::Aborted];

/// Execute `v1:discard:<id>`.
pub async fn execute_discard(
    ctx: &Arc<AppContext>,
    action_id: i64,
    operator_id: u64,
) -> Result<Option<String>, AppError> {
    let action = ctx
        .store
        .action(action_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("action {action_id}")))?;
    if !DISCARD_FROM.contains(&action.state) {
        return Err(AppError::AlreadyHandled);
    }
    let directory = directory_for(ctx, &action).await?;
    let text = dir_text(&directory);

    let discarded = ctx
        .store
        .cas_state_from_any(
            action_id,
            &DISCARD_FROM,
            State::Discarded,
            &[(Column::EndedAt, WriteValue::Int(support::now_unix()))],
        )
        .await?;
    if !discarded {
        return Err(AppError::AlreadyHandled);
    }
    ctx.store
        .release_lock(&text, &action_id.to_string())
        .await?;
    ctx.store.retire_active_session(&text).await?;
    ctx.store
        .audit(operator_id, "discard", Some(action_id), "discarded")
        .await?;
    Ok(Some(format!("discarded **{}**", action.label)))
}
