//! The git adapter — the **only** place in the codebase that spawns a process
//! (spec P7/D-4/NFR-10, enforced by clippy disallowed-methods and gate G5).
//! Phase 2 ships the read-only half: head, branch, status, diff stat, log,
//! ancestry. All commands are argv-only — a shell can never be involved and
//! `-C <dir>` roots every command in its directory.
//!
//! The two `#![expect(clippy::disallowed_methods)]` sites are this adapter's
//! reason for existing; the `#[expect]` (not `#[allow]`) means a stale ban is
//! itself a compile warning.

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use async_trait::async_trait;

use ocbot_core::error::GitError;
use ocbot_core::ports::{CommitRow, GitPort};

/// A git port bound to a binary path and a per-command timeout.
#[derive(Debug, Clone)]
pub struct GitAdapter {
    git_bin: PathBuf,
    timeout: Duration,
}

impl GitAdapter {
    /// The production adapter: `git` with a 10 s timeout (spec R-9).
    pub fn new() -> Self {
        Self::with_bin("git", Duration::from_secs(10))
    }

    /// A test seam: any executable (including a scripted fake git) with any
    /// timeout. Not called by production wiring.
    pub fn with_bin(git_bin: impl Into<PathBuf>, timeout: Duration) -> Self {
        Self {
            git_bin: git_bin.into(),
            timeout,
        }
    }

    /// Spawn `git -C <dir> <args...>`, wait with the configured timeout.
    ///
    /// Uses `Command::output()` — which pipes stdout/stderr internally — under
    /// a timeout. No `kill_on_drop`: combined with output collection it kills
    /// the child before the pipes drain. On timeout the future is dropped with
    /// its pipes closed; the child dies on its next write (SIGPIPE) — a rare,
    /// bounded path (the timeout protects responsiveness, R-9).
    #[expect(
        clippy::disallowed_methods,
        reason = "the git adapter is the ONLY process spawn site (D-4/NFR-10)"
    )]
    async fn run(
        &self,
        kind: &'static str,
        args: &[&str],
        directory: &Path,
    ) -> Result<Output, GitError> {
        let mut command = tokio::process::Command::new(&self.git_bin);
        command.arg("-C").arg(directory).args(args);
        let output = command.output();
        tokio::time::timeout(self.timeout, output)
            .await
            .map_err(|_| GitError::Timeout { kind })?
            .map_err(|error| GitError::Spawn {
                kind,
                message: error.to_string(),
            })
    }

    /// Run a command and return its trimmed stdout, turning non-zero exits and
    /// spawn/timeout failures into typed errors.
    async fn capture(
        &self,
        kind: &'static str,
        args: &[&str],
        directory: &Path,
    ) -> Result<String, GitError> {
        let output = self.run(kind, args, directory).await?;
        if !output.status.success() {
            return Err(GitError::Command {
                kind,
                code: status_code(output.status),
                stderr: truncate(500, &String::from_utf8_lossy(&output.stderr)),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }

    /// The owned-string variant for commands that build arguments dynamically.
    async fn capture_owned(
        &self,
        kind: &'static str,
        args: &[String],
        directory: &Path,
    ) -> Result<String, GitError> {
        let references: Vec<&str> = args.iter().map(String::as_str).collect();
        self.capture(kind, &references, directory).await
    }
}

impl Default for GitAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl GitPort for GitAdapter {
    async fn is_repo(&self, directory: &Path) -> Result<bool, GitError> {
        let output = self
            .run("rev-parse", &["rev-parse", "--git-dir"], directory)
            .await?;
        Ok(output.status.success())
    }

    async fn head(&self, directory: &Path) -> Result<String, GitError> {
        self.capture("head", &["rev-parse", "HEAD"], directory)
            .await
    }

    async fn branch(&self, directory: &Path) -> Result<String, GitError> {
        self.capture("branch", &["branch", "--show-current"], directory)
            .await
    }

    async fn status_porcelain(&self, directory: &Path) -> Result<String, GitError> {
        self.capture("status", &["status", "--porcelain"], directory)
            .await
    }

    async fn diff_stat(&self, directory: &Path, base: &str) -> Result<String, GitError> {
        self.capture("diff-stat", &["diff", "--stat", base], directory)
            .await
    }

    async fn log(&self, directory: &Path, base: Option<&str>) -> Result<Vec<CommitRow>, GitError> {
        let range = match base {
            Some(base) => format!("{base}..HEAD"),
            None => "HEAD".to_owned(),
        };
        let args = vec![
            "log".to_owned(),
            "--first-parent".to_owned(),
            "-n".to_owned(),
            "200".to_owned(),
            "--format=%H%x1f%s%x1f%ct".to_owned(),
            range,
        ];
        match self.capture_owned("log", &args, directory).await {
            Ok(output) => parse_log(&output),
            // Unborn HEAD (empty repo) has nothing to show.
            Err(_) => Ok(Vec::new()),
        }
    }

    async fn is_ancestor(&self, directory: &Path, sha: &str, of: &str) -> Result<bool, GitError> {
        let output = self
            .run(
                "merge-base",
                &["merge-base", "--is-ancestor", sha, of],
                directory,
            )
            .await?;
        Ok(output.status.success())
    }
}

fn parse_log(output: &str) -> Result<Vec<CommitRow>, GitError> {
    let mut rows = Vec::new();
    for line in output.lines() {
        let mut fields = line.split('\u{1f}');
        let sha = fields.next().unwrap_or_default();
        let subject = fields.next().unwrap_or_default();
        let timestamp = fields.next().unwrap_or_default();
        if sha.is_empty() {
            continue;
        }
        rows.push(CommitRow {
            sha: sha.to_owned(),
            subject: subject.to_owned(),
            timestamp: timestamp.parse().unwrap_or_default(),
        });
    }
    Ok(rows)
}

fn status_code(status: std::process::ExitStatus) -> String {
    status
        .code()
        .map_or_else(|| "killed".to_owned(), |code| code.to_string())
}

fn truncate(max_bytes: usize, text: &str) -> String {
    text.chars().take(max_bytes).collect()
}

#[cfg(test)]
mod tests {
    use super::parse_log;

    #[test]
    fn parses_git_log_rows() {
        let rows = parse_log(
            "abc123\u{1f}initial commit\u{1f}1700000000\n\
             def456\u{1f}add feature\u{1f}1700000100",
        )
        .expect("parse");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].sha, "abc123");
        assert_eq!(rows[0].subject, "initial commit");
        assert_eq!(rows[0].timestamp, 1_700_000_000);
    }

    #[test]
    fn empty_log_is_empty() {
        assert!(parse_log("").expect("empty").is_empty());
    }
}
