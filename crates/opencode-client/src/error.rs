//! Error type for the opencode v2 client.

use thiserror::Error;

/// Every fallible operation in this crate returns [`OcError`].
///
/// The variants are deliberately coarse: callers act on *class* (retry the
/// transport, surface the API message, refuse the version), never on a
/// server-internal error tag.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum OcError {
    /// The server answered, but is not opencode v2. `ocbot` refuses to run
    /// against it (spec FR-40 / AC-16).
    #[error("unsupported opencode server version {found:?}; ocbot requires opencode v2")]
    UnsupportedServerVersion {
        /// The version string the server reported, best-effort.
        found: String,
    },

    /// `/api/info` was absent or not a GET endpoint, which is the v1 shape.
    #[error("server does not expose the v2 /api/info endpoint (HTTP {status})")]
    MissingInfoEndpoint {
        /// The HTTP status observed while probing.
        status: u16,
    },

    /// The configured base URL could not be parsed.
    #[error("invalid base url: {0}")]
    InvalidUrl(String),

    /// A transport-level failure: DNS, connect, TLS, or timeout.
    #[error("transport error: {0}")]
    Transport(#[from] reqwest::Error),

    /// A non-success HTTP status with the server's decoded message when present.
    #[error("opencode API error {status}: {message}")]
    Api {
        /// The HTTP status code returned.
        status: u16,
        /// The server-provided message, or a truncated body.
        message: String,
    },

    /// The response body did not decode into the expected (tolerant) type.
    #[error("failed to decode response: {0}")]
    Decode(#[from] serde_json::Error),

    /// An identifier was used that does not carry the prefix the API requires
    /// (for example a session id not starting with `ses`).
    #[error("invalid {kind} id: {value:?}")]
    InvalidId {
        /// The kind of identifier that failed validation.
        kind: &'static str,
        /// The rejected value.
        value: String,
    },

    /// A streamed response ended before it could be parsed.
    #[error("malformed server-sent event stream: {0}")]
    Stream(String),
}
