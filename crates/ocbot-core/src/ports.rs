//! Port contracts (spec §6.2). The core declares traits only; the shell wires
//! adapters (opencode client, serenity, git, SQLite) behind them. The core
//! stays dependency-light: these traits are `async` via `async-trait` and use
//! only core-native types.

use std::path::Path;

use async_trait::async_trait;

use crate::error::{FsError, GitError, OcError};
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

/// The gate vocabulary lives in [`crate::gates`]; this re-export keeps the
/// port contract spelled the same way it has been since Phase 0.
pub use crate::gates::{GateDecision, GateKind, GateRequest};

/// The effect of one permission rule in an opencode ruleset (wire enum
/// `Permission.Effect`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionEffect {
    /// Allow matching requests without asking.
    Allow,
    /// Deny matching requests.
    Deny,
    /// Ask the operator each time.
    Ask,
}

impl PermissionEffect {
    /// The wire spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionEffect::Allow => "allow",
            PermissionEffect::Deny => "deny",
            PermissionEffect::Ask => "ask",
        }
    }
}

/// One rule of a per-session permission ruleset (spec FR-42; the shape the
/// server's `Session.CreateInput.permissions` expects).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PermissionRule {
    /// The action/tool, e.g. `bash`, `read`, `write`.
    pub action: String,
    /// The resource pattern, e.g. `git push*` or `**.ssh**`.
    pub resource: String,
    /// The effect.
    pub effect: PermissionEffect,
}

/// The injected time source for the re-ping policy (fake-clock-testable, P4).
pub trait Clock: Send + Sync {
    /// Current unix seconds.
    fn now(&self) -> i64;
}

/// One directory entry as shown by the `/ls` browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// The base name.
    pub name: String,
    /// The absolute path (realpath'd, contained).
    pub path: String,
    /// Whether the entry is a directory.
    pub is_dir: bool,
}

/// The filesystem surface (spec P11/FR-34/FR-35/FR-36). The adapter owns
/// `realpath`, the containment re-check at every use (TOCTOU), the browser
/// readdir, and attachment persistence.
#[async_trait]
pub trait FsPort: Send + Sync {
    /// `realpath` `path` and verify it is inside a configured browse root
    /// (unless freeform paths are enabled), re-checking containment at the
    /// moment of use.
    async fn resolve(&self, path: &Path) -> Result<std::path::PathBuf, FsError>;
    /// Read `path` as UTF-8 after a containment re-check (P12: the plan is read
    /// fresh each open).
    async fn read_text(&self, path: &Path) -> Result<String, FsError>;
    /// List `path` (dirs first, then name), hiding dotfiles unless
    /// `show_hidden`.
    async fn list(&self, path: &Path, show_hidden: bool) -> Result<Vec<DirEntry>, FsError>;
    /// Persist an attachment under `INBOX_DIR/<yyyymmdd>/<tag8>-<sanitized>`
    /// and return its absolute path (FR-36).
    async fn save_attachment(
        &self,
        filename: &str,
        bytes: &[u8],
    ) -> Result<std::path::PathBuf, FsError>;
    /// The configured browse roots (realpath'd).
    fn roots(&self) -> Vec<std::path::PathBuf>;
    /// Whether freeform typed paths bypass the browse-root containment check
    /// (`ALLOW_FREEFORM_PATHS`, default false — P11).
    fn allow_freeform(&self) -> bool;
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
    /// Create a session bound to a directory (spec A6). `permissions` attaches a
    /// per-session ruleset at creation (FR-42; `None` when the feature is off).
    async fn create_session(
        &self,
        directory: &Path,
        title: Option<&str>,
        permissions: Option<&[PermissionRule]>,
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
    /// Reply to a pending form with a `{ key: value }` answer map (FR-41).
    async fn reply_form(
        &self,
        session_id: &str,
        form_id: &str,
        answer: serde_json::Value,
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
    /// Mixed-reset the index back to HEAD, leaving the working tree exactly as
    /// it is (spec CP-P1's failure repair: a rejected park commit must not
    /// leave the attempt staged). A no-op when the index already matches HEAD.
    async fn unstage_all(&self, directory: &Path) -> Result<(), GitError>;
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

/// Information about a project (opaque to user, SPEC-005).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectInfo {
    /// Opaque display name derived from directory path (e.g., "lex/project-alpha").
    pub name: String,
    /// Full directory path (internal only, never exposed to user).
    pub dir: std::path::PathBuf,
}

/// Detailed project metadata (SPEC-005).
#[derive(Debug, Clone)]
pub struct ProjectDetails {
    /// Opaque project name.
    pub name: String,
    /// First 5 lines of README (if exists).
    pub readme_preview: Option<String>,
    /// Available plans in this project.
    pub plans: Vec<PlanInfo>,
}

/// Information about a plan (SPEC-005).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanInfo {
    /// Plan filename only (e.g., "spec-auth.md").
    pub name: String,
    /// Full path to plan file (internal only).
    pub path: std::path::PathBuf,
}

/// Default plan directories for project configuration.
pub fn default_plans_dirs() -> Vec<String> {
    vec!["devdocs/plans".to_owned()]
}

/// Default README filename for project configuration.
pub fn default_readme_file() -> String {
    "README.md".to_owned()
}

/// Default README filenames to search for (in order).
pub fn default_readme_files() -> Vec<String> {
    vec!["README.md".to_owned(), "README.txt".to_owned(), "README".to_owned()]
}

/// Per-project configuration loaded from `ocbot.toml` (SPEC-005 Q5).
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ProjectConfig {
    /// Multiple plan directories to search (in order).
    #[serde(default = "default_plans_dirs")]
    pub plans_dirs: Vec<String>,
    /// README filename to look for.
    #[serde(default = "default_readme_file")]
    pub readme_file: String,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            plans_dirs: default_plans_dirs(),
            readme_file: default_readme_file(),
        }
    }
}

/// Port for project scanning and metadata retrieval (SPEC-005).
pub trait ProjectPort: Send + Sync {
    /// List all projects under the configured projects root.
    fn list_projects(&self) -> Result<Vec<ProjectInfo>, crate::error::ProjectError>;

    /// Get project metadata (README preview, available plans).
    fn get_project_info(
        &self,
        project_name: &str,
    ) -> Result<ProjectDetails, crate::error::ProjectError>;

    /// Get full README content for a project.
    fn get_readme(
        &self,
        project_name: &str,
    ) -> Result<Option<String>, crate::error::ProjectError>;

    /// Get the content of a specific plan file.
    fn get_plan_content(
        &self,
        project_name: &str,
        plan_name: &str,
    ) -> Result<String, crate::error::ProjectError>;

    /// Get the full directory path for a project (internal use only).
    fn get_project_dir(&self, project_name: &str) -> Result<std::path::PathBuf, crate::error::ProjectError>;

    /// Get the full path for a plan file (internal use only).
    fn get_plan_path(
        &self,
        project_name: &str,
        plan_name: &str,
    ) -> Result<std::path::PathBuf, crate::error::ProjectError>;
}

/// Port for loading per-project `ocbot.toml` configuration (SPEC-005 Q5).
pub trait ProjectConfigPort: Send + Sync {
    /// Load `ocbot.toml` from a project directory.
    /// Returns `None` if the file doesn't exist.
    fn load_config(
        &self,
        dir: &std::path::Path,
    ) -> Result<Option<ProjectConfig>, crate::error::ProjectConfigError>;
}
