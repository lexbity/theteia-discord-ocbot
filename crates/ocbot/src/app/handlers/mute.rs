//! `/mute` (FR-33): toggle inbox reporting for a directory.

use std::sync::Arc;

use crate::app::context::AppContext;
use crate::app::error::AppError;
use crate::app::handlers::support;
use crate::app::interaction::InteractionSource;

/// Execute `/mute` on the operator's current directory.
pub async fn execute(ctx: &Arc<AppContext>, source: &InteractionSource) -> Result<(), AppError> {
    let dir = support::history_dir(ctx, source.user_id).await?;
    let muted = ctx.store.toggle_mute("dir", &dir).await?;
    ctx.store.audit(source.user_id, "mute", None, &dir).await?;
    let content = format!(
        "inbox reports for `{dir}` are now {}",
        if muted { "muted" } else { "unmuted" }
    );
    ctx.discord
        .post(source.channel_id, &support::simple(content, source.user_id))
        .await?;
    Ok(())
}
