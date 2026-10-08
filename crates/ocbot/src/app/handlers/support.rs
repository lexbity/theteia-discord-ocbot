//! Shared helpers for the Phase 3 git-truth handlers: directory resolution,
//! temp commit-message files (never argv, P7), and small message builders.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ocbot_core::{ButtonKind, OutgoingMessage, component_button};

use crate::adapters::store::ActionRow;
use crate::app::context::AppContext;
use crate::app::error::AppError;
use crate::app::handlers::run;

/// The directory an action's run targets.
pub(crate) async fn directory_for(
    ctx: &Arc<AppContext>,
    action: &ActionRow,
) -> Result<PathBuf, AppError> {
    run::run_directory(ctx, action.run_id).await
}

/// The directory as the store keys it (lossy is fine: paths come from realpath).
pub(crate) fn dir_text(dir: &Path) -> String {
    dir.to_string_lossy().into_owned()
}

/// Whether `dir` is a git work tree (a real git probe; execute-phase only).
pub(crate) async fn is_git_dir(ctx: &Arc<AppContext>, dir: &Path) -> Result<bool, AppError> {
    Ok(ctx.git.is_repo(dir).await?)
}

/// Whether the project row records a work branch (store-only; safe in the plan
/// phase, where no port may be touched — AC-1).
pub(crate) async fn is_git_project(ctx: &Arc<AppContext>, dir: &str) -> Result<bool, AppError> {
    Ok(ctx
        .store
        .project(dir)
        .await?
        .is_some_and(|project| project.work_branch.is_some()))
}

/// The directory `/history` and `/reset` operate on: the remembered cwd, else
/// the most recent run directory.
pub(crate) async fn history_dir(ctx: &Arc<AppContext>, user_id: u64) -> Result<String, AppError> {
    if let Some(cwd) = ctx.store.ui_cwd(user_id).await? {
        return Ok(cwd);
    }
    ctx.store
        .recent_dirs(1)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| AppError::Other("no directory recorded yet; run something first".to_owned()))
}

/// The channel an action's messages go to.
pub(crate) fn channel_for(action: &ActionRow) -> Result<u64, AppError> {
    action
        .channel_id
        .and_then(|channel| u64::try_from(channel).ok())
        .ok_or_else(|| AppError::Other("action has no channel (invariant)".to_owned()))
}

/// Current unix seconds.
pub(crate) fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

/// A plain (component-less) message mentioning only the operator (FR-3).
pub(crate) fn simple(content: impl Into<String>, operator_id: u64) -> OutgoingMessage {
    OutgoingMessage {
        content: content.into(),
        attachment: None,
        components: Vec::new(),
        mentioned_users: vec![operator_id],
    }
}

/// A one-button message (`custom_id` provided).
pub(crate) fn with_button(
    content: impl Into<String>,
    label: impl Into<String>,
    custom_id: impl Into<String>,
    kind: ButtonKind,
    operator_id: u64,
) -> OutgoingMessage {
    OutgoingMessage {
        content: content.into(),
        attachment: None,
        components: vec![component_button(label, custom_id, kind)],
        mentioned_users: vec![operator_id],
    }
}

/// A temp commit-message file, removed on drop (spec P7: commit messages are
/// passed with `-F <tmpfile>`, never on argv).
pub(crate) struct TempMessage {
    path: PathBuf,
}

impl TempMessage {
    /// Write `text` to a uniquely named temp file.
    pub(crate) fn new(text: &str) -> Result<Self, AppError> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let path =
            std::env::temp_dir().join(format!("ocbot-commit-{}-{nanos}.txt", std::process::id()));
        std::fs::write(&path, text)
            .map_err(|error| AppError::Other(format!("commit message file: {error}")))?;
        Ok(Self { path })
    }

    /// The temp file's path.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempMessage {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
