//! A thin, tolerant HTTP client for the **opencode v2** API (spec D-R3, P16).
//!
//! This crate maintains only the ~14 endpoints `ocbot` needs. Three properties
//! are normative:
//!
//! 1. **Tolerant deserialization** — every wire type uses `#[serde(default)]`,
//!    ignores unknown fields, and every wire enum carries a `#[serde(other)]`
//!    catch-all. Emitting strict reject-unknown-fields attributes is banned
//!    here by policy (gate G1). The asymmetry with our own config types
//!    (strict) is deliberate (spec P16).
//! 2. **V2 only** — [`Client::probe`] reconciles against `/api/info` and
//!    refuses anything that is not opencode v2 (spec FR-40 / AC-16).
//! 3. **Async, deterministic** — plain `async fn`s over `reqwest`; the caller
//!    brings their own tokio runtime.
//!
//! See `COMPAT.md` for the pinned opencode version ↔ client version table and
//! `docs/compat/SPIKE-0.md` for the Phase 0 spike record.

#![forbid(unsafe_code)]

mod error;
mod gates;
mod info;
mod log;
mod messages;
mod prompt;
mod sessions;

pub use error::OcError;
pub use gates::{
    FormBooleanField, FormExternalField, FormField, FormInfo, FormMultiselectField,
    FormNumberField, FormOption, FormStringField, GateDecision, GateKind, GateRequest,
    PermissionRequest,
};
pub use info::{ServerCapabilities, ServerInfo, ServerPaths, ensure_supported};
pub use log::{Durable, Event, EventStream, LogEntry, LogStream, SseFrame};
pub use messages::{
    AssistantMessage, ContentPart, Finish, IdleMessage, Message, StructuredError, TimeInfo,
    UserMessage,
};
pub use prompt::{Admission, InterruptResult};
pub use sessions::{
    ActiveSession, Activity, CacheUsage, CreateSession, LocationRef, PermissionEffect,
    PermissionRule, SessionInfo, SessionTime, TokenUsage,
};

use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;

/// The username used for basic auth (spec A9: opencode's own password auth).
pub const AUTH_USERNAME: &str = "opencode";

/// Default per-request timeout, in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 10;

/// The opencode server version Phase 0 pinned all fixtures against.
pub const PINNED_SERVER_VERSION: &str = "2.0.24";

/// A thin HTTP client for the opencode v2 API.
///
/// Cheap to clone? It is not `Clone` by design: the `reqwest::Client` inside
/// would clone trivially, but callers should share one instance. Build it
/// once, pass `&Client` everywhere.
#[derive(Debug)]
pub struct Client {
    http: reqwest::Client,
    base: reqwest::Url,
    password: String,
}

impl Client {
    /// Construct a client against `base_url` (e.g. `http://127.0.0.1:8080`)
    /// using the server password. Auth is HTTP basic with username
    /// [`AUTH_USERNAME`]; all calls get a `DEFAULT_TIMEOUT_SECS` timeout.
    pub fn new(base_url: &str, password: &str) -> Result<Self, OcError> {
        let mut normalized = base_url.trim().to_owned();
        if !normalized.ends_with('/') {
            normalized.push('/');
        }
        let base = reqwest::Url::parse(&normalized)
            .map_err(|error| OcError::InvalidUrl(error.to_string()))?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .user_agent(concat!("opencode-v2-client/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            http,
            base,
            password: password.to_owned(),
        })
    }

    /// The normalized base URL (trailing slash trimmed) this client targets.
    pub fn base_url(&self) -> &str {
        self.base.as_str().trim_end_matches('/')
    }

    /// Resolve a relative API path against the client's base URL.
    fn url(&self, path: &str) -> Result<reqwest::Url, OcError> {
        self.base
            .join(path)
            .map_err(|error| OcError::InvalidUrl(format!("{path}: {error}")))
    }

    /// Send a GET and decode the JSON body.
    async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<T, OcError> {
        let request = self
            .http
            .get(self.url(path)?)
            .query(query)
            .basic_auth(AUTH_USERNAME, Some(&self.password));
        let response = request.send().await?;
        self.decode_success(response).await
    }

    /// Send a POST with a JSON body and decode the JSON body.
    async fn post<B: Serialize + Sync + ?Sized, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, OcError> {
        let request = self
            .http
            .post(self.url(path)?)
            .json(body)
            .basic_auth(AUTH_USERNAME, Some(&self.password));
        let response = request.send().await?;
        self.decode_success(response).await
    }

    /// Send a POST with a JSON body and ignore the (normally empty/204) body.
    async fn post_empty<B: Serialize + Sync + ?Sized>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<(), OcError> {
        let request = self
            .http
            .post(self.url(path)?)
            .json(body)
            .basic_auth(AUTH_USERNAME, Some(&self.password));
        let response = request.send().await?;
        let status = response.status();
        if status.is_success() {
            Ok(())
        } else {
            let text = response.text().await.unwrap_or_default();
            Err(Self::api_error(status, &text))
        }
    }

    /// Send a GET that is expected to return a long-lived stream.
    async fn get_stream(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<reqwest::Response, OcError> {
        let request = self
            .http
            .get(self.url(path)?)
            .query(query)
            .basic_auth(AUTH_USERNAME, Some(&self.password));
        let response = request.send().await?;
        let status = response.status();
        if status.is_success() {
            Ok(response)
        } else {
            let text = response.text().await.unwrap_or_default();
            Err(Self::api_error(status, &text))
        }
    }

    /// Turn a success-checked response into a decoded value.
    async fn decode_success<T: DeserializeOwned>(
        &self,
        response: reqwest::Response,
    ) -> Result<T, OcError> {
        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(Self::api_error(status, &text));
        }
        let text = response.text().await?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Build an [`OcError::Api`] from a non-success response, pulling the
    /// server's `message` field when the body is JSON.
    fn api_error(status: reqwest::StatusCode, body: &str) -> OcError {
        let message = serde_json::from_str::<serde_json::Value>(body)
            .ok()
            .and_then(|value| {
                value
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| body.chars().take(200).collect());
        OcError::Api {
            status: status.as_u16(),
            message,
        }
    }
}

/// Path templates the client constructs. The drift test (AC-15) asserts each
/// one exists in the vendored `OpenAPI` spec after parameter-name
/// normalization; keep this list in sync with the methods below.
pub const CONSTRUCTED_PATHS: &[&str] = &[
    "/api/info",
    "/api/session",
    "/api/session/active",
    "/api/session/{session_id}",
    "/api/session/{session_id}/prompt",
    "/api/session/{session_id}/message",
    "/api/session/{session_id}/interrupt",
    "/api/session/{session_id}/permission/{request_id}/reply",
    "/api/session/{session_id}/form/{form_id}/reply",
    "/api/experimental/session/{session_id}/log",
    "/api/permission/request",
    "/api/form",
    "/api/event",
];

/// Paths used by the client, kept together so `CONSTRUCTED_PATHS` cannot
/// silently diverge from the code (the drift test is the tripwire).
pub mod paths {
    /// `GET /api/info` — server identity (version gate).
    pub const INFO: &str = "/api/info";
    /// `GET/POST /api/session` — list/create sessions.
    pub const SESSION: &str = "/api/session";
    /// `GET /api/session/active` — foreground executions.
    pub const SESSION_ACTIVE: &str = "/api/session/active";
    /// `GET /api/permission/request` — pending permission gates.
    pub const PERMISSION_REQUEST: &str = "/api/permission/request";
    /// `GET /api/form` — pending form gates.
    pub const FORM: &str = "/api/form";
    /// `GET /api/event` — global volatile event stream.
    pub const EVENT: &str = "/api/event";

    /// `GET /api/session/{id}` (and `DELETE`/`PATCH`).
    pub fn session(session_id: &str) -> String {
        format!("/api/session/{session_id}")
    }
    /// `POST /api/session/{id}/prompt`.
    pub fn session_prompt(session_id: &str) -> String {
        format!("/api/session/{session_id}/prompt")
    }
    /// `GET /api/session/{id}/message`.
    pub fn session_message(session_id: &str) -> String {
        format!("/api/session/{session_id}/message")
    }
    /// `POST /api/session/{id}/interrupt`.
    pub fn session_interrupt(session_id: &str) -> String {
        format!("/api/session/{session_id}/interrupt")
    }
    /// `POST /api/session/{id}/permission/{request_id}/reply`.
    pub fn session_permission_reply(session_id: &str, request_id: &str) -> String {
        format!("/api/session/{session_id}/permission/{request_id}/reply")
    }
    /// `POST /api/session/{id}/form/{form_id}/reply`.
    pub fn session_form_reply(session_id: &str, form_id: &str) -> String {
        format!("/api/session/{session_id}/form/{form_id}/reply")
    }
    /// `GET /api/experimental/session/{id}/log` — durable session log (SSE).
    pub fn session_log(session_id: &str) -> String {
        format!("/api/experimental/session/{session_id}/log")
    }
}
