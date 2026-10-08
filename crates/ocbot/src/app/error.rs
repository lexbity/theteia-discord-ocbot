//! App errors.

use crate::adapters::store::StoreError;
use ocbot_core::{GitError, OcError};

/// The app-level error used by handlers, the watcher, and reconcile.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// Store actor failure.
    #[error("store: {0}")]
    Store(#[from] StoreError),
    /// opencode port failure.
    #[error("opencode: {0}")]
    OpenCode(#[from] OcError),
    /// git port failure.
    #[error("git: {0}")]
    Git(#[from] GitError),
    /// Discord send failure.
    #[error("discord: {0}")]
    Discord(#[from] crate::app::context::SendError),
    /// A referenced row/session no longer exists.
    #[error("not found: {0}")]
    NotFound(String),
    /// The state machine refused a transition (double-tap / replay).
    #[error("already handled")]
    AlreadyHandled,
    /// An unrecognized `custom_id`.
    #[error("unknown interaction: {0}")]
    UnknownCustomId(String),
    /// An unrecognized slash command.
    #[error("unknown command: {0}")]
    UnknownCommand(String),
    /// Everything else.
    #[error("{0}")]
    Other(String),
}
