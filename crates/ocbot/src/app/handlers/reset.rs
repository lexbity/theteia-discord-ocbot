//! Reset (FR-28) with the CP-R0..R2 write-ahead sequence (§6.4): record the
//! intent, create the `superseded/<ts>` safety branch, then hard-reset. A crash
//! at any point rolls forward from the intent row (P6/P9 step 5).

use std::path::Path;
use std::sync::Arc;

use ocbot_core::ButtonKind;

use crate::adapters::now_unix;
use crate::adapters::store::ResetIntentRow;
use crate::app::context::AppContext;
use crate::app::crashpoint::checkpoint;
use crate::app::error::AppError;
use crate::app::handlers::support;
use crate::app::interaction::InteractionSource;

/// Execute `v1:reset:<sha>`: post the confirmation message (a reset is not
/// reversible once HEAD moves).
pub async fn confirm(
    ctx: &Arc<AppContext>,
    source: &InteractionSource,
    sha: &str,
) -> Result<Option<String>, AppError> {
    let dir = support::history_dir(ctx, source.user_id).await?;
    let content = format!(
        "Reset `{}` to `{}`?\nThis creates `superseded/<ts>` first, then runs `git reset --hard` (FR-28). The tree must be clean.",
        dir,
        short(sha)
    );
    let message = support::with_button(
        content,
        "Confirm reset",
        format!("v1:resetgo:{sha}"),
        ButtonKind::Danger,
        source.user_id,
    );
    ctx.discord.post(source.channel_id, &message).await?;
    Ok(Some("confirm to reset".to_owned()))
}

/// Execute `v1:resetgo:<sha>`.
pub async fn execute_reset(
    ctx: &Arc<AppContext>,
    source: &InteractionSource,
    sha: &str,
) -> Result<Option<String>, AppError> {
    let dir = support::history_dir(ctx, source.user_id).await?;
    begin_reset(ctx, &dir, sha, source.user_id, Some(source.channel_id)).await
}

/// Validate, record the intent, and drive the sequence.
pub async fn begin_reset(
    ctx: &Arc<AppContext>,
    dir_text: &str,
    sha: &str,
    operator_id: u64,
    channel: Option<u64>,
) -> Result<Option<String>, AppError> {
    let dir = Path::new(dir_text);
    if !support::is_git_dir(ctx, dir).await? {
        return Err(AppError::Other("not a git repository".to_owned()));
    }
    if let Some(holder) = ctx.store.lock_holder(dir_text).await? {
        return Err(AppError::Other(format!(
            "directory is locked by {holder}; decide the pending action first"
        )));
    }
    if !ctx.git.status_porcelain(dir).await?.trim().is_empty() {
        return Err(AppError::Other(
            "refusing to reset: the tree is dirty (commit or park first, FR-28)".to_owned(),
        ));
    }
    let head = ctx.git.head(dir).await?;
    if sha == head {
        return Err(AppError::Other("HEAD is already at that commit".to_owned()));
    }
    if !ctx.git.is_ancestor(dir, sha, &head).await? {
        return Err(AppError::Other(
            "refusing to reset: the target is not a live commit (P8)".to_owned(),
        ));
    }

    let sup_branch = format!("superseded/{}", now_unix());
    let id = ctx
        .store
        .record_reset_intent(dir_text, sha, &sup_branch)
        .await?;
    checkpoint("CP-R0");

    let intent = ResetIntentRow {
        id,
        dir: dir_text.to_owned(),
        target_sha: sha.to_owned(),
        sup_branch,
        step: 0,
    };
    resume(ctx, &intent, operator_id, channel).await
}

/// Resume a recorded reset intent from its `step`. Idempotent.
pub async fn resume(
    ctx: &Arc<AppContext>,
    intent: &ResetIntentRow,
    operator_id: u64,
    channel: Option<u64>,
) -> Result<Option<String>, AppError> {
    let dir = Path::new(&intent.dir);

    if intent.step < 1 {
        // The safety branch is created *before* HEAD moves (AC-8).
        if ctx
            .git
            .branch_exists(dir, &intent.sup_branch)
            .await?
            .is_none()
        {
            let head = ctx.git.head(dir).await?;
            ctx.git
                .create_branch_at(dir, &intent.sup_branch, &head)
                .await?;
        }
        ctx.store.set_reset_step(intent.id, 1).await?;
        checkpoint("CP-R1");
    }

    if intent.step < 2 {
        ctx.git.reset_hard(dir, &intent.target_sha).await?;
        ctx.store.set_reset_step(intent.id, 2).await?;
        checkpoint("CP-R2");
    }

    ctx.store.delete_reset_intent(intent.id).await?;
    ctx.store.retire_active_session(&intent.dir).await?;
    ctx.store
        .audit(operator_id, "reset", None, &intent.target_sha)
        .await?;

    let summary = format!(
        "reset to `{}`; actions above that point now render orphaned (P8)",
        short(&intent.target_sha)
    );
    if let Some(channel) = channel {
        ctx.discord
            .post(channel, &support::simple(summary.clone(), operator_id))
            .await?;
    }
    Ok(Some(summary))
}

fn short(sha: &str) -> String {
    sha.chars().take(8).collect()
}
