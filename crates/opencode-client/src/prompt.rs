//! Admitted prompts and the interrupt verb.

use serde::{Deserialize, Serialize};

use crate::Client;
use crate::error::OcError;
use crate::paths;

/// Result of a successfully admitted prompt (`POST /api/session/{id}/prompt`).
///
/// The opencode v2 API is **durably admitting, not synchronous**: this type
/// carries the message id and the delivery mode, and the agent-loop execution
/// may still be pending when the call returns.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct Admission {
    /// The admitted user message id (wire prefix `msg`).
    pub id: String,
    /// The session the message was admitted to.
    pub session_id: String,
    /// The delivery mode observed (`steer` or `queue`).
    #[serde(default)]
    pub delivery: Option<String>,
    /// Admission timestamp (epoch milliseconds).
    #[serde(default)]
    pub created: Option<f64>,
}

/// Response of `POST /api/session/{id}/interrupt`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct InterruptResult {
    /// Whether an active execution was actually interrupted.
    pub interrupted: bool,
}

/// Wire envelope for an admitted user inbox item.
#[derive(Debug, Clone, Deserialize)]
struct InboxUser {
    id: String,
    #[serde(rename = "sessionID", default)]
    session_id: String,
    #[serde(default)]
    delivery: Option<String>,
    #[serde(default)]
    time: Option<InboxTime>,
}

#[derive(Debug, Clone, Deserialize)]
struct InboxTime {
    #[serde(default)]
    created: f64,
}

impl Client {
    /// Durably admit a prompt into a session. Returns as soon as the server has
    /// accepted (and scheduled) the message — not when it completes (spec P9).
    pub async fn send_prompt(&self, session_id: &str, text: &str) -> Result<Admission, OcError> {
        let path = paths::session_prompt(session_id);
        let body = serde_json::json!({ "text": text });
        let response: Data<InboxUser> = self.post(&path, &body).await?;
        Ok(Admission {
            id: response.data.id,
            session_id: response.data.session_id,
            delivery: response.data.delivery,
            created: response.data.time.map(|time| time.created),
        })
    }

    /// Interrupt active execution. The server reports `interrupted=false` when
    /// the session was already idle (spec S-8 pinned in Phase 0).
    pub async fn interrupt(&self, session_id: &str) -> Result<InterruptResult, OcError> {
        let path = paths::session_interrupt(session_id);
        self.post(&path, &serde_json::Value::Null).await
    }
}

use crate::sessions::Data;
