//! Error vocabulary for the port contracts and the pure core.

/// A failure talking to the opencode server, from the core's point of view.
/// The adapters translate concrete client/transport errors into these classes
/// so handlers act on *class*, never on server-internal tags.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum OcError {
    /// The server answered but is not opencode v2 (FR-40).
    #[error("unsupported opencode server version {found:?}; ocbot requires opencode v2")]
    UnsupportedServerVersion {
        /// The version string the server reported.
        found: String,
    },
    /// `/api/info` absent — the v1 signature.
    #[error("server does not expose the v2 /api/info endpoint (HTTP {status})")]
    MissingInfoEndpoint {
        /// The observed HTTP status.
        status: u16,
    },
    /// The server could not be reached at all.
    #[error("opencode unreachable: {0}")]
    Transport(String),
    /// A non-success HTTP status with the server's message.
    #[error("opencode API error {status}: {message}")]
    Api {
        /// The HTTP status.
        status: u16,
        /// The server-provided message.
        message: String,
    },
    /// A message/stream could not be decoded.
    #[error("bad server payload: {0}")]
    Decode(String),
}

/// A failure in the fs adapter (containment, attachment persistence). FR-35
/// requires containment to be re-verified at every use, so the adapter is
/// the only place filesystem paths become real.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum FsError {
    /// The path (after realpath) is not inside any configured containment root.
    #[error("path {path:?} is outside the allowed roots (FR-34/FR-35)")]
    OutsideRoots {
        /// The offending path as given.
        path: String,
    },
    /// The path does not exist or cannot be canonicalized.
    #[error("path not found: {0}")]
    NotFound(String),
    /// Any other filesystem error.
    #[error("fs: {0}")]
    Io(String),
}

/// A failure running a git command. The read-only half of the git adapter
/// (spec P7: argv-only; the adapter is the only spawn site).
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum GitError {
    /// The command exited non-zero.
    #[error("git {kind} failed ({code}): {stderr}")]
    Command {
        /// A short description of the command.
        kind: &'static str,
        /// The exit status.
        code: String,
        /// Captured stderr (truncated).
        stderr: String,
    },
    /// The command could not be spawned.
    #[error("could not run git {kind}: {message}")]
    Spawn {
        /// A short description of the command.
        kind: &'static str,
        /// The io error text.
        message: String,
    },
    /// The command exceeded its timeout and was killed.
    #[error("git {kind} timed out")]
    Timeout {
        /// A short description of the command.
        kind: &'static str,
    },
}

/// A failure in project operations (SPEC-005).
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ProjectError {
    /// The projects root is not configured.
    #[error("projects root not configured")]
    NotConfigured,
    /// The project was not found.
    #[error("project not found: {0}")]
    NotFound(String),
    /// No projects exist in the configured root.
    #[error("no projects found in {0}")]
    NoProjects(String),
    /// No plans exist in the project.
    #[error("no plans found in project: {0}")]
    NoPlans(String),
    /// Failed to read a directory.
    #[error("failed to read directory {0}: {1}")]
    IoError(String, String),
    /// Failed to read a file.
    #[error("failed to read file {0}: {1}")]
    ReadError(String, String),
}

/// A failure loading per-project configuration.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum ProjectConfigError {
    /// Invalid TOML syntax in the config file.
    #[error("invalid TOML in {0}: {1}")]
    ParseError(String, String),
    /// Failed to read the config file.
    #[error("failed to read config {0}: {1}")]
    IoError(String, String),
}
