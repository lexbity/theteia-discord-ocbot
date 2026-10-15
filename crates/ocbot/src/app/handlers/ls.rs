//! `/ls` and `/cd` (spec FR-34/FR-35): a button-driven directory browser rooted
//! at `BROWSE_ROOTS`. Navigation is Up / Open / Select / Prev / Next; every
//! path is `realpath`'d and containment is re-verified at each step by the fs
//! adapter. A selection only records the path as the user's draft (P11): it
//! never sends anything.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ocbot_core::LsEntry;
use ocbot_core::render::ls_message;

use crate::app::context::AppContext;
use crate::app::error::AppError;
use crate::app::interaction::{InputStyle, InteractionSource, ModalInputSpec, ModalSpec};

/// The `/cd` modal `custom_id`.
pub const CD_MODAL: &str = "v1:cd";

/// Execute `/ls`: browse from the remembered cwd (or the first root).
pub async fn execute_ls(ctx: &Arc<AppContext>, source: &InteractionSource) -> Result<(), AppError> {
    let cwd = start_dir(ctx, &source.user_id).await?;
    let message = render(ctx, &cwd, 0, &source.user_id).await?;
    ctx.chat.post(&source.destination, &message).await?;
    Ok(())
}

/// The `/cd` modal: a single path input (the typed-path entry point).
pub async fn cd_modal(
    ctx: &Arc<AppContext>,
    source: &InteractionSource,
) -> Result<ModalSpec, AppError> {
    let prefill = ctx.store.ui_cwd(&source.user_id).await?;
    Ok(ModalSpec {
        custom_id: CD_MODAL.to_owned(),
        title: "ocbot — cd".to_owned(),
        inputs: vec![ModalInputSpec {
            id: "path".to_owned(),
            label: "Directory".to_owned(),
            style: InputStyle::Short,
            value: prefill,
            required: true,
        }],
    })
}

/// Execute the `/cd` modal: open the browser at the typed path (containment is
/// enforced by the fs adapter; a refusal surfaces as an ephemeral error).
pub async fn execute_cd(
    ctx: &Arc<AppContext>,
    source: &InteractionSource,
) -> Result<Option<String>, AppError> {
    let typed = source.input("path").map(str::trim).unwrap_or_default();
    if typed.is_empty() {
        return Err(AppError::Other("a path is required".to_owned()));
    }
    let resolved = ctx.fs.resolve(Path::new(typed)).await?;
    let message = render(ctx, &resolved, 0, &source.user_id).await?;
    ctx.chat.post(&source.destination, &message).await?;
    Ok(None)
}

/// `v1:ls:up`: ascend one directory (clamped at a browse root).
pub async fn up(ctx: &Arc<AppContext>, source: &InteractionSource) -> Result<(), AppError> {
    let cwd = start_dir(ctx, &source.user_id).await?;
    let parent = cwd
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| cwd.clone(), Path::to_path_buf);
    // Only ascend if the parent still resolves inside the roots; otherwise stay
    // (a root is the boundary).
    let target = match ctx.fs.resolve(&parent).await {
        Ok(resolved) => resolved,
        Err(_) => cwd,
    };
    let message = render(ctx, &target, 0, &source.user_id).await?;
    edit_or_post(ctx, source, &message).await
}

/// `v1:ls:open:<idx>`: descend into the directory at `index`.
pub async fn open(
    ctx: &Arc<AppContext>,
    source: &InteractionSource,
    index: usize,
) -> Result<(), AppError> {
    let cwd = start_dir(ctx, &source.user_id).await?;
    let entry = entry_at(ctx, &cwd, index).await?;
    if !entry.is_dir {
        return Err(AppError::Other(format!(
            "`{}` is not a directory",
            entry.name
        )));
    }
    let target = ctx.fs.resolve(Path::new(&entry.path)).await?;
    let message = render(ctx, &target, 0, &source.user_id).await?;
    edit_or_post(ctx, source, &message).await
}

/// `v1:ls:p:<n>`: re-render the current directory at another page.
pub async fn page(
    ctx: &Arc<AppContext>,
    source: &InteractionSource,
    page: usize,
) -> Result<(), AppError> {
    let cwd = start_dir(ctx, &source.user_id).await?;
    let message = render(ctx, &cwd, page, &source.user_id).await?;
    edit_or_post(ctx, source, &message).await
}

/// `v1:lssel:<idx>`: select a file, recording its path as the draft (P11).
pub async fn select(
    ctx: &Arc<AppContext>,
    source: &InteractionSource,
    index: usize,
) -> Result<Option<String>, AppError> {
    let cwd = start_dir(ctx, &source.user_id).await?;
    let entry = entry_at(ctx, &cwd, index).await?;
    let resolved = ctx.fs.resolve(Path::new(&entry.path)).await?;
    let text = resolved.to_string_lossy().into_owned();
    ctx.store.set_draft_path(&source.user_id, &text).await?;
    ctx.store
        .audit(&source.user_id, "ls", None, "selected")
        .await?;
    Ok(Some(format!("selected `{text}`")))
}

/// `v1:ls:use`: select the current directory itself.
pub async fn use_current(
    ctx: &Arc<AppContext>,
    source: &InteractionSource,
) -> Result<Option<String>, AppError> {
    let cwd = start_dir(ctx, &source.user_id).await?;
    let resolved = ctx.fs.resolve(&cwd).await?;
    let text = resolved.to_string_lossy().into_owned();
    ctx.store.set_draft_path(&source.user_id, &text).await?;
    ctx.store
        .audit(&source.user_id, "ls", None, "selected-dir")
        .await?;
    Ok(Some(format!("selected directory `{text}`")))
}

/// Dispatch a `v1:ls:<rest>` browser button (`up`, `use`, `open:<idx>`,
/// `p:<n>`). Unknown shapes never panic — they surface an error.
pub async fn component(
    ctx: &Arc<AppContext>,
    source: &InteractionSource,
    rest: &str,
) -> Result<Option<String>, AppError> {
    match rest {
        "up" => {
            up(ctx, source).await?;
            Ok(None)
        }
        "use" => use_current(ctx, source).await,
        other => {
            if let Some(index) = other.strip_prefix("open:") {
                open(ctx, source, parse_index(index)?).await?;
                Ok(None)
            } else if let Some(page_number) = other.strip_prefix("p:") {
                page(ctx, source, parse_index(page_number)?).await?;
                Ok(None)
            } else {
                Err(AppError::UnknownCustomId(format!("v1:ls:{other}")))
            }
        }
    }
}

fn parse_index(raw: &str) -> Result<usize, AppError> {
    raw.trim()
        .parse()
        .map_err(|_| AppError::UnknownCustomId(format!("bad ls index {raw:?}")))
}

/// The directory a browser session starts from: the remembered cwd when it is
/// still resolvable, else the first configured root.
async fn start_dir(ctx: &Arc<AppContext>, user_id: &str) -> Result<PathBuf, AppError> {
    if let Some(cwd) = ctx.store.ui_cwd(user_id).await?
        && let Ok(resolved) = ctx.fs.resolve(Path::new(&cwd)).await
    {
        return Ok(resolved);
    }
    ctx.fs
        .roots()
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Other("no browse roots configured".to_owned()))
}

/// Resolve the `index`-th entry of the current directory (the browser lists
/// dirs first, then names, so indices are stable across renders).
async fn entry_at(
    ctx: &Arc<AppContext>,
    cwd: &Path,
    index: usize,
) -> Result<ocbot_core::ports::DirEntry, AppError> {
    let entries = ctx.fs.list(cwd, false).await?;
    entries
        .into_iter()
        .nth(index)
        .ok_or_else(|| AppError::Other(format!("entry {index} is gone; reopen the browser")))
}

/// Render a browser page and remember the browsed directory as the cwd.
async fn render(
    ctx: &Arc<AppContext>,
    cwd: &Path,
    page: usize,
    actor: &str,
) -> Result<ocbot_core::OutgoingMessage, AppError> {
    let entries = ctx.fs.list(cwd, false).await?;
    let ls_entries: Vec<LsEntry> = entries
        .iter()
        .map(|entry| LsEntry {
            name: entry.name.clone(),
            is_dir: entry.is_dir,
        })
        .collect();
    let text = cwd.to_string_lossy().into_owned();
    ctx.store.set_ui_cwd(actor, &text).await?;
    Ok(ls_message(
        &text,
        &ls_entries,
        page,
        ctx.ls_page_size,
        crate::app::handlers::support::operator_u64(actor),
    ))
}

/// Edit the originating browser message in place when there is one (a button
/// click), else post a new one (a slash command).
async fn edit_or_post(
    ctx: &Arc<AppContext>,
    source: &InteractionSource,
    message: &ocbot_core::OutgoingMessage,
) -> Result<(), AppError> {
    match source.message_id {
        Some(message_id) => {
            ctx.chat
                .edit(
                    &crate::app::handlers::support::discord_receipt(message_id),
                    message,
                )
                .await;
        }
        None => {
            ctx.chat.post(&source.destination, message).await?;
        }
    }
    Ok(())
}
