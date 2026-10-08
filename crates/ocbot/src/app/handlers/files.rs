//! Attachment ingestion (spec P11/FR-36). A phone upload is persisted under
//! `INBOX_DIR/<yyyymmdd>/<tag8>-<sanitized>` and its absolute path becomes the
//! user's draft, which the next `/prompt` (or `/run` plan field) can reference.
//! The bot never executes an attachment.

use std::sync::Arc;

use crate::app::context::AppContext;
use crate::app::error::AppError;

/// Persist `bytes` as `filename` and remember the resulting path as `user_id`'s
/// draft. Returns the absolute path.
pub async fn ingest(
    ctx: &Arc<AppContext>,
    user_id: u64,
    filename: &str,
    bytes: &[u8],
) -> Result<String, AppError> {
    let path = ctx.fs.save_attachment(filename, bytes).await?;
    let text = path.to_string_lossy().into_owned();
    ctx.store.set_draft_path(user_id, &text).await?;
    ctx.store.audit(user_id, "attach", None, "saved").await?;
    Ok(text)
}

/// The operator's current draft path, if any (used to prefill prompts).
pub async fn draft(ctx: &Arc<AppContext>, user_id: u64) -> Result<Option<String>, AppError> {
    Ok(ctx.store.draft_path(user_id).await?)
}
