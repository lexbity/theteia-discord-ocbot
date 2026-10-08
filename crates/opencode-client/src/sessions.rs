//! Session discovery, creation, and liveness ([`SessionInfo`], [`Activity`]).
//!
//! Tolerant by construction (spec P16): every field except the identity is
//! optional, unknown fields are ignored, and the liveness enum carries a
//! forward-compatible catch-all.

use serde::{Deserialize, Serialize};

/// A public reference to a filesystem location.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct LocationRef {
    /// The absolute directory the session is bound to.
    #[serde(default)]
    pub directory: String,
}

/// A token/cost usage summary attached to a session or message.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct TokenUsage {
    /// Input tokens.
    #[serde(default)]
    pub input: f64,
    /// Output tokens.
    #[serde(default)]
    pub output: f64,
    /// Reasoning tokens.
    #[serde(default)]
    pub reasoning: f64,
    /// Cached token counts.
    #[serde(default)]
    pub cache: Option<CacheUsage>,
}

/// Cache read/write token counts.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct CacheUsage {
    /// Tokens read from cache.
    #[serde(default)]
    pub read: f64,
    /// Tokens written to cache.
    #[serde(default)]
    pub write: f64,
}

/// A model reference as reported on sessions and assistant messages.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelRef {
    /// Model id.
    #[serde(default)]
    pub id: String,
    /// Provider id.
    #[serde(default)]
    pub provider_id: String,
    /// Optional variant id.
    #[serde(default)]
    pub variant: Option<String>,
}

/// A session as returned by `GET /api/session`, `GET /api/session/{id}`, and
/// `POST /api/session`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct SessionInfo {
    /// The session id (wire prefix `ses`).
    pub id: String,
    /// The parent session id for child sessions, absent for roots.
    #[serde(rename = "parentID", default)]
    pub parent_id: Option<String>,
    /// Human-readable title, when set.
    #[serde(default)]
    pub title: Option<String>,
    /// The project id the session belongs to.
    #[serde(rename = "projectID", default)]
    pub project_id: Option<String>,
    /// The bound location (directory).
    #[serde(default)]
    pub location: Option<LocationRef>,
    /// Accumulated cost in USD.
    #[serde(default)]
    pub cost: Option<f64>,
    /// Accumulated token usage.
    #[serde(default)]
    pub tokens: Option<TokenUsage>,
    /// The session outcome, once it has completed at least once.
    #[serde(default)]
    pub outcome: Option<String>,
    /// Timestamps.
    #[serde(default)]
    pub time: Option<SessionTime>,
    /// The model the session is configured with.
    #[serde(default)]
    pub model: Option<ModelRef>,
    /// The agent the session is configured with.
    #[serde(default)]
    pub agent: Option<String>,
}

impl SessionInfo {
    /// The directory the session is bound to, if reported.
    pub fn directory(&self) -> Option<&str> {
        self.location
            .as_ref()
            .map(|location| location.directory.as_str())
            .filter(|directory| !directory.is_empty())
    }

    /// Whether this is a root session (no parent).
    pub fn is_root(&self) -> bool {
        self.parent_id.is_none()
    }
}

/// Session timestamps.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct SessionTime {
    /// Creation time (epoch milliseconds).
    #[serde(default)]
    pub created: f64,
    /// Last update time.
    #[serde(default)]
    pub updated: Option<f64>,
    /// Last idle time.
    #[serde(default)]
    pub idle: Option<f64>,
    /// Last viewed time.
    #[serde(default)]
    pub viewed: Option<f64>,
}

/// Liveness of a session, derived from `GET /api/session/active`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    /// The session has no foreground execution.
    Idle,
    /// The session currently owns a foreground execution.
    Busy,
}

/// One entry of the `GET /api/session/active` map.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ActiveSession {
    /// The active kind; observed value is `"running"`.
    #[serde(rename = "type", default)]
    pub kind: String,
}

/// The request body of `POST /api/session`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CreateSession {
    /// The directory the session is bound to.
    pub location: LocationRef,
    /// Optional title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl CreateSession {
    /// Create a request binding a session to `directory`.
    pub fn in_directory(directory: impl Into<String>) -> Self {
        Self {
            location: LocationRef {
                directory: directory.into(),
            },
            title: None,
        }
    }
}

use crate::Client;
use crate::error::OcError;
use crate::paths;

impl Client {
    /// List sessions. With `root_only`, only sessions whose `parentID` is null
    /// are returned; with `directory`, results are filtered to that location.
    pub async fn list_sessions(
        &self,
        root_only: bool,
        directory: Option<&str>,
    ) -> Result<Vec<SessionInfo>, OcError> {
        let mut query: Vec<(&str, &str)> = vec![("limit", "50"), ("order", "desc")];
        if root_only {
            query.push(("parentID", "null"));
        }
        if let Some(dir) = directory {
            query.push(("directory", dir));
        }
        let response: Data<Vec<SessionInfo>> = self.get(paths::SESSION, &query).await?;
        Ok(response.data)
    }

    /// Fetch a single session by id.
    pub async fn get_session(&self, session_id: &str) -> Result<SessionInfo, OcError> {
        let path = paths::session(session_id);
        let response: Data<SessionInfo> = self.get(&path, &[]).await?;
        Ok(response.data)
    }

    /// Create a session, optionally bound to a directory and titled.
    pub async fn create_session(
        &self,
        directory: Option<&str>,
        title: Option<&str>,
    ) -> Result<SessionInfo, OcError> {
        let mut body = CreateSession::default();
        if let Some(directory) = directory {
            body.location = LocationRef {
                directory: directory.to_owned(),
            };
        }
        body.title = title.map(str::to_owned);
        let response: Data<SessionInfo> = self.post(paths::SESSION, &body).await?;
        Ok(response.data)
    }

    /// The set of sessions currently owning a foreground execution.
    pub async fn active_sessions(
        &self,
    ) -> Result<std::collections::HashMap<String, ActiveSession>, OcError> {
        let response: Data<std::collections::HashMap<String, ActiveSession>> =
            self.get(paths::SESSION_ACTIVE, &[]).await?;
        Ok(response.data)
    }

    /// Liveness of one session.
    pub async fn activity(&self, session_id: &str) -> Result<Activity, OcError> {
        let active = self.active_sessions().await?;
        Ok(if active.contains_key(session_id) {
            Activity::Busy
        } else {
            Activity::Idle
        })
    }
}

/// The universal `{"data": ...}` response envelope. Unknown siblings such as
/// `cursor` are ignored by serde's default (tolerant) behavior.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Data<T> {
    pub(crate) data: T,
}

#[cfg(test)]
mod tests {
    use super::SessionInfo;

    #[test]
    fn unknown_fields_and_missing_optionals_are_tolerated() {
        let json = r#"{
            "id": "ses_x",
            "projectID": "p",
            "cost": 0.1,
            "tokens": {"input": 1, "output": 2, "reasoning": 0, "cache": {"read": 0, "write": 0}},
            "time": {"created": 1.0, "updated": 2.0},
            "location": {"directory": "/tmp/x"},
            "brandNewField": {"nested": true},
            "status": "idle"
        }"#;
        let session: SessionInfo = serde_json::from_str(json).expect("tolerant decode");
        assert_eq!(session.id, "ses_x");
        assert_eq!(session.directory(), Some("/tmp/x"));
        assert!(session.is_root());
    }

    #[test]
    fn missing_identity_fields_default_rather_than_fail() {
        let session: SessionInfo =
            serde_json::from_str(r#"{"id": "ses_z"}"#).expect("tolerant decode");
        assert_eq!(session.id, "ses_z");
        assert_eq!(session.directory(), None);
        assert!(session.is_root());
    }
}
