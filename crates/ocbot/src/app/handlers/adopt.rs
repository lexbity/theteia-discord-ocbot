//! Adoption (P14 / FR-32 / AC-11): replying to an inbox report continues that
//! session as an adopted action (`run.kind = 'adopted'`), subject to the
//! directory mutex. Adoption reuses the shared launch path, so busy/lock rules
//! apply identically to `/run` and `/prompt`.

use std::path::Path;
use std::sync::Arc;

use ocbot_core::State;

use crate::app::context::AppContext;
use crate::app::error::AppError;
use crate::app::handlers::run;
use crate::app::handlers::support;

/// Adopt `session_id` with `prompt` as the next instruction.
pub async fn adopt(
    ctx: &Arc<AppContext>,
    session_id: &str,
    prompt: &str,
    channel: u64,
    user_id: u64,
) -> Result<(), AppError> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err(AppError::Other(
            "an empty reply cannot adopt a session".to_owned(),
        ));
    }
    let dir = ctx.store.session_dir(session_id).await?.ok_or_else(|| {
        AppError::Other(format!("session {session_id} is not mapped to a directory"))
    })?;

    // The inbox clears the external lock when the session goes idle; drop it
    // here too, then honor the bot mutex (P5).
    ctx.store
        .release_lock(&dir, &format!("external:{session_id}"))
        .await?;
    if let Some(holder) = ctx.store.lock_holder(&dir).await? {
        return Err(AppError::Other(format!(
            "directory is locked by {holder} (P5); /status shows the holder"
        )));
    }

    let canonical = Path::new(&dir);
    let (branch, base) = if ctx.git.is_repo(canonical).await? {
        (
            ctx.git.branch(canonical).await.ok(),
            ctx.git.head(canonical).await.ok(),
        )
    } else {
        (None, None)
    };
    ctx.store
        .upsert_project(&dir, branch.as_deref(), base.as_deref())
        .await?;

    let run_id = ctx.store.insert_run(&dir, "adopted").await?;
    let label: String = prompt.chars().take(40).collect();
    let channel_id = i64::try_from(channel).unwrap_or(i64::MAX);
    let action_id = ctx
        .store
        .insert_action(run_id, Some(channel_id), &label, prompt, 1)
        .await?;
    let action = ctx
        .store
        .action(action_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("action {action_id}")))?;

    let outcome = run::launch(ctx, action, Some(session_id.to_owned())).await?;

    let row = ctx
        .store
        .action(action_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("action {action_id}")))?;
    let state = row.state;
    if state == State::Staged {
        ctx.store
            .cas_state(action_id, State::Staged, State::Discarded, &[])
            .await?;
    }
    ctx.store
        .audit(user_id, "adopt", Some(action_id), state.as_str())
        .await?;

    let message = support::simple(
        format!(
            "adopted `{}` on session `{session_id}` — {}",
            label,
            outcome.unwrap_or_default()
        ),
        user_id,
    );
    ctx.discord.post(channel, &message).await?;
    Ok(())
}
