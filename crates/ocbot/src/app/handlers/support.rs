//! Shared helpers for the Phase 3 git-truth handlers: directory resolution,
//! temp commit-message files (never argv, P7), and small message builders.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ocbot_core::{
    ButtonKind, DISCORD_MESSAGE_CAP, Destination, OutgoingMessage, Surface, component_button,
    fence_neutralize, split_excerpt, truncate_chars,
};

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
pub(crate) async fn history_dir(ctx: &Arc<AppContext>, user_id: &str) -> Result<String, AppError> {
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

/// The destination an action's messages go to: the action row's persisted
/// `(surface, chat_ref)` (SPEC-004 §5.4 reply routing).
pub(crate) fn destination_for(action: &ActionRow) -> Result<Destination, AppError> {
    let chat_ref = action
        .chat_ref
        .clone()
        .ok_or_else(|| AppError::Other("action has no chat_ref (invariant)".to_owned()))?;
    let surface = Surface::parse(&action.surface).ok_or_else(|| {
        AppError::Other(format!("action has unknown surface {:?}", action.surface))
    })?;
    Ok(Destination {
        surface,
        chat_ref: ocbot_core::ChatRef(chat_ref),
    })
}

/// The FR-3 mention allowlist for one actor: Discord ids are parsed and
/// allowlisted; other surfaces have no mention mechanism (the field is a
/// Discord-rendered allowlist, SPEC-004 §5.2).
pub(crate) fn mention_allowlist_for(actor: &str) -> Vec<u64> {
    ocbot_core::ActorId(actor.to_owned())
        .discord_id()
        .into_iter()
        .collect()
}

/// The Discord snowflake behind an `ActorId` (render functions key on it); 0
/// when the actor is not a Discord identity (nothing is mentionable then).
pub(crate) fn operator_u64(actor: &str) -> u64 {
    ocbot_core::ActorId(actor.to_owned())
        .discord_id()
        .unwrap_or(0)
}

/// A Discord receipt for a snowflake message id.
pub(crate) fn discord_receipt(message_id: u64) -> ocbot_core::PostReceipt {
    ocbot_core::PostReceipt {
        surface: Surface::Discord,
        msg_id: ocbot_core::MsgId(message_id.to_string()),
    }
}

/// A Discord receipt for an already-stringified message id.
pub(crate) fn discord_receipt_str(message_id: &str) -> ocbot_core::PostReceipt {
    ocbot_core::PostReceipt {
        surface: Surface::Discord,
        msg_id: ocbot_core::MsgId(message_id.to_owned()),
    }
}

/// A plain (component-less) message mentioning only the operator (FR-3).
pub(crate) fn simple(content: impl Into<String>, actor: &str) -> OutgoingMessage {
    OutgoingMessage {
        content: content.into(),
        attachment: None,
        components: Vec::new(),
        mention_allowlist: mention_allowlist_for(actor),
    }
}

/// The marker appended inside the fence when [`failure_post`] clamps output.
const OUTPUT_CLAMPED: &str = "… (output truncated)";

/// A failure post carrying command output in a fenced block: `title`, the
/// clamped and fence-neutralized `output`, and an optional trailing `note`
/// (e.g. what happened to the tree).
///
/// Hook output arrives already cut to 2 000 bytes by the adapter — on its own
/// that overflows the Discord cap once framed, so the block is budgeted
/// against title + note + fences and the whole message can never exceed the
/// cap (P10), whatever the output or label size.
pub(crate) fn failure_post(
    title: impl AsRef<str>,
    output: &str,
    note: &str,
    actor: &str,
) -> OutgoingMessage {
    // Belt: callers clamp labels, but this builder's cap invariant must not
    // depend on them.
    let title = truncate_chars(title.as_ref(), 300);
    let note_bytes = if note.is_empty() { 0 } else { note.len() + 1 };
    let fixed = title.len() + note_bytes + "```\n".len() * 2 + 2;
    let capacity = DISCORD_MESSAGE_CAP.saturating_sub(fixed);
    let neutralized = fence_neutralize(output);
    let (block, truncated) = split_excerpt(&neutralized, capacity);
    let block = if truncated && capacity > OUTPUT_CLAMPED.len() + 1 {
        // Re-split with the marker's bytes reserved so the marker itself
        // cannot push the message past the cap (skipped when the budget is
        // too small to ever fit it — then the bare clamp already suffices).
        let (clamped, _) = split_excerpt(
            &neutralized,
            capacity.saturating_sub(OUTPUT_CLAMPED.len() + 1),
        );
        format!("{clamped}\n{OUTPUT_CLAMPED}")
    } else {
        block
    };
    let mut content = format!("{title}\n```\n{block}\n```");
    if !note.is_empty() {
        content.push('\n');
        content.push_str(note);
    }
    simple(content, actor)
}

/// A one-button message (`custom_id` provided).
pub(crate) fn with_button(
    content: impl Into<String>,
    label: impl Into<String>,
    custom_id: impl Into<String>,
    kind: ButtonKind,
    actor: &str,
) -> OutgoingMessage {
    OutgoingMessage {
        content: content.into(),
        attachment: None,
        components: vec![component_button(label, custom_id, kind)],
        mention_allowlist: mention_allowlist_for(actor),
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
