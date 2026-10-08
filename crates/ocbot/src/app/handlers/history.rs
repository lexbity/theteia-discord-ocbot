//! `/history` and `/reset` rendering (FR-25): the P8 merge, paged with
//! `v1:page:history:<n>`. `/reset` is the same list with the `Reset here`
//! buttons emphasized (FR-28's "target a live commit").

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use ocbot_core::history::{HistoryAction, merge};
use ocbot_core::{OutgoingMessage, history_page};

use crate::app::context::AppContext;
use crate::app::error::AppError;
use crate::app::handlers::support;
use crate::app::interaction::InteractionSource;

/// Execute `/history` (or `/reset`): post page 0 of the directory's history.
pub async fn execute(
    ctx: &Arc<AppContext>,
    source: &InteractionSource,
    show_reset: bool,
) -> Result<(), AppError> {
    let dir = support::history_dir(ctx, source.user_id).await?;
    ctx.store.set_ui_cwd(source.user_id, &dir).await?;
    let message = build_page(ctx, &dir, 0, show_reset, source.user_id).await?;
    ctx.discord.post(source.channel_id, &message).await?;
    Ok(())
}

/// Execute `v1:page:history:<n>`: rebuild and edit the paging message in place
/// (edits do not notify — P10).
pub async fn page(
    ctx: &Arc<AppContext>,
    source: &InteractionSource,
    page: usize,
) -> Result<(), AppError> {
    let dir = support::history_dir(ctx, source.user_id).await?;
    let message = build_page(ctx, &dir, page, true, source.user_id).await?;
    match source.message_id {
        Some(message_id) => {
            ctx.discord
                .edit(source.channel_id, message_id, &message)
                .await?;
        }
        None => {
            ctx.discord.post(source.channel_id, &message).await?;
        }
    }
    Ok(())
}

/// Build the history page for `dir`: one `git log`, one ancestry probe per
/// distinct accepted sha, then the pure P8 merge.
async fn build_page(
    ctx: &Arc<AppContext>,
    dir: &str,
    page: usize,
    show_reset: bool,
    operator_id: u64,
) -> Result<OutgoingMessage, AppError> {
    let path = Path::new(dir);
    if !support::is_git_dir(ctx, path).await? {
        return Ok(support::simple(
            format!("`{dir}` is not a git repository"),
            operator_id,
        ));
    }
    let base = ctx
        .store
        .project(dir)
        .await?
        .and_then(|project| project.base_sha);
    let log = ctx.git.log(path, base.as_deref()).await?;
    let actions = ctx.store.actions_for_dir(dir).await?;
    let head = ctx.git.head(path).await.unwrap_or_default();

    let mut reachable: HashMap<String, bool> = HashMap::new();
    for action in &actions {
        if let Some(sha) = &action.accept_sha
            && !reachable.contains_key(sha)
        {
            let live = ctx.git.is_ancestor(path, sha, &head).await.unwrap_or(false);
            reachable.insert(sha.clone(), live);
        }
    }

    let history_actions: Vec<HistoryAction> = actions
        .iter()
        .map(|action| HistoryAction {
            id: action.id,
            label: action.label.clone(),
            prompt: action.prompt_sent.clone(),
            session_id: action.session_id.clone(),
            state: action.state,
            accept_sha: action.accept_sha.clone(),
            branch: action.park_branch_name.clone(),
        })
        .collect();

    let rows = merge(&log, &history_actions, &head, |sha| {
        *reachable.get(sha).unwrap_or(&false)
    });
    Ok(history_page(dir, &rows, page, show_reset, operator_id))
}
