//! Port contracts (spec §6.2). The core declares traits only; the shell wires
//! adapters (opencode client, serenity, git, SQLite) behind them. The core
//! stays dependency-light: these traits are `async` via `async-trait` and use
//! only core-native types.

use std::path::Path;

use async_trait::async_trait;

use crate::error::{GitError, OcError};
use crate::outcome::AssistantSnapshot;

/// Server identity facts from the version probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerFacts {
    /// The reported opencode version.
    pub version: String,
    /// Advertised connection URLs.
    pub urls: Vec<String>,
}

/// A session as seen by the bot through the opencode port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortSession {
    /// The session id.
    pub id: String,
    /// The parent session id, when this is a child session.
    pub parent_id: Option<String>,
    /// The title, when set.
    pub title: Option<String>,
    /// The bound directory, when reported.
    pub directory: Option<String>,
}

/// The result of a `create_session` call.
pub type SessionId = String;

/// A durably admitted prompt (spec P9 — admission, not completion).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptAdmission {
    /// The admitted message id.
    pub id: String,
    /// The owning session id.
    pub session_id: String,
    /// The delivery mode observed (`steer` or `queue`).
    pub delivery: Option<String>,
}

/// Liveness of a session (derived from `session/active`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// No foreground execution.
    Idle,
    /// A foreground execution is owned.
    Busy,
}

/// The concrete nature of a pending gate.
#[derive(Debug, Clone, PartialEq)]
pub enum GateKind {
    /// A permission request naming a tool and its patterns.
    Permission {
        /// The tool / verb, e.g. `bash`.
        action: String,
        /// The resources/patterns requested.
        resources: Vec<String>,
    },
    /// A form asking structured questions.
    Form {
        /// The form title/question.
        title: String,
        /// The number of fields (the bot decides renderability later).
        field_count: usize,
    },
}

/// A pending gate: permissions and forms, unified (spec P4).
#[derive(Debug, Clone, PartialEq)]
pub struct GateRequest {
    /// The server-side gate id.
    pub id: String,
    /// The owning session id, when known.
    pub session_id: Option<String>,
    /// The gate kind.
    pub kind: GateKind,
}

/// A permission decision (wire values pinned in Phase 0: `once`/`always`/`reject`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateDecision {
    /// Allow this once.
    Once,
    /// Remember and always allow.
    Always,
    /// Deny.
    Reject,
}

/// One row of `git log`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRow {
    /// The commit sha.
    pub sha: String,
    /// The commit subject (first line).
    pub subject: String,
    /// Committer/author timestamp (unix seconds).
    pub timestamp: i64,
}

/// A commit that did not land because `git` rejected it (a failing hook, or a
/// commit with nothing to record). This is *not* a transport failure: the
/// repository is intact and the caller surfaces `output` and returns to
/// `reported` (spec P7 accept/park write halves).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookFailure {
    /// The hook/commit output to surface to the operator.
    pub output: String,
}

/// The opencode v2 HTTP surface, normalized for downstream use.
#[async_trait]
pub trait OpenCodePort: Send + Sync {
    /// Probe and version-gate the server (FR-40).
    async fn probe(&self) -> Result<ServerFacts, OcError>;
    /// List sessions; `root_only` filters `parentID=null`.
    async fn list_sessions(&self, root_only: bool) -> Result<Vec<PortSession>, OcError>;
    /// Liveness of one session.
    async fn activity(&self, session_id: &str) -> Result<Activity, OcError>;
    /// Create a session bound to a directory (spec A6).
    async fn create_session(
        &self,
        directory: &Path,
        title: Option<&str>,
    ) -> Result<SessionId, OcError>;
    /// Durably admit a prompt; returns on admission, not completion (P9).
    async fn send_prompt(&self, session_id: &str, text: &str) -> Result<PromptAdmission, OcError>;
    /// The last assistant snapshot (report input, A7).
    async fn last_assistant(&self, session_id: &str) -> Result<Option<AssistantSnapshot>, OcError>;
    /// Interrupt active execution (S-8: no-op when idle).
    async fn interrupt(&self, session_id: &str) -> Result<(), OcError>;
    /// All pending gates (permissions + forms).
    async fn pending_gates(&self) -> Result<Vec<GateRequest>, OcError>;
    /// Reply to a pending permission gate.
    async fn reply_gate(
        &self,
        session_id: &str,
        gate_id: &str,
        decision: GateDecision,
    ) -> Result<(), OcError>;
}

/// The git surface used by the bot. The read half lands in Phase 2; the write
/// verbs (`commit_all`, `branch_exists`, `create_branch_at`, `reset_hard`) and
/// the `untracked_files` helper (park's big-file guard) land in Phase 3 (P7).
#[async_trait]
pub trait GitPort: Send + Sync {
    /// Whether `directory` is inside a git work tree.
    async fn is_repo(&self, directory: &Path) -> Result<bool, GitError>;
    /// The HEAD commit sha.
    async fn head(&self, directory: &Path) -> Result<String, GitError>;
    /// The current branch name (empty when detached).
    async fn branch(&self, directory: &Path) -> Result<String, GitError>;
    /// The porcelain status output (empty when clean).
    async fn status_porcelain(&self, directory: &Path) -> Result<String, GitError>;
    /// The `git diff --stat` text vs `base` (10 s timeout in the adapter).
    async fn diff_stat(&self, directory: &Path, base: &str) -> Result<String, GitError>;
    /// Recent commits on `directory`, optionally from `base` (exclusive).
    async fn log(&self, directory: &Path, base: Option<&str>) -> Result<Vec<CommitRow>, GitError>;
    /// Whether `sha` is an ancestor of `of`.
    async fn is_ancestor(&self, directory: &Path, sha: &str, of: &str) -> Result<bool, GitError>;
    /// Every untracked, unignored path (repo-relative) in `directory`. The park
    /// guard stats these to refuse an oversized attempt (P7).
    async fn untracked_files(&self, directory: &Path) -> Result<Vec<String>, GitError>;
    /// `git add -A` then commit the tree using the message file at
    /// `message_path` (never argv, P7). `no_verify` skips commit hooks (park).
    /// `Ok(Ok(sha))` is a landed commit; `Ok(Err(HookFailure))` is a rejected
    /// commit whose output must be surfaced.
    async fn commit_all(
        &self,
        directory: &Path,
        message_path: &Path,
        no_verify: bool,
    ) -> Result<Result<String, HookFailure>, GitError>;
    /// The tip of `name`, or `None` when the branch does not exist.
    async fn branch_exists(&self, directory: &Path, name: &str)
    -> Result<Option<String>, GitError>;
    /// Create branch `name` at `at` (no checkout).
    async fn create_branch_at(
        &self,
        directory: &Path,
        name: &str,
        at: &str,
    ) -> Result<(), GitError>;
    /// `git reset --hard <sha>`, discarding tracked/index changes (P7).
    async fn reset_hard(&self, directory: &Path, sha: &str) -> Result<(), GitError>;
}
