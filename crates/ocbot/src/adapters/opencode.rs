//! The opencode adapter: translates `opencode-v2-client` types onto
//! `ocbot-core::ports::OpenCodePort` (so the port vocabulary is the core's,
//! and replacing the client never touches handlers).

use std::path::Path;

use async_trait::async_trait;
use ocbot_core::error::OcError as CoreOcError;
use ocbot_core::outcome::{AssistantSnapshot, TokenCounts};
use ocbot_core::ports::{
    Activity, GateDecision, GateKind, GateRequest, OpenCodePort, PortSession, PromptAdmission,
    ServerFacts, SessionId,
};
use opencode_v2_client as client;

/// Maps the standalone client onto the core port contract.
#[derive(Debug)]
pub struct OpenCodeAdapter {
    inner: client::Client,
}

impl OpenCodeAdapter {
    /// Wrap a configured client.
    pub fn new(inner: client::Client) -> Self {
        Self { inner }
    }
}

fn map_error(error: client::OcError) -> CoreOcError {
    match error {
        client::OcError::UnsupportedServerVersion { found } => {
            CoreOcError::UnsupportedServerVersion { found }
        }
        client::OcError::MissingInfoEndpoint { status } => {
            CoreOcError::MissingInfoEndpoint { status }
        }
        client::OcError::InvalidUrl(text) => CoreOcError::Transport(text),
        client::OcError::Transport(error) => CoreOcError::Transport(error.to_string()),
        client::OcError::Api { status, message } => CoreOcError::Api { status, message },
        client::OcError::Decode(error) => CoreOcError::Decode(error.to_string()),
        client::OcError::Stream(text) => CoreOcError::Decode(text),
        client::OcError::InvalidId { kind, value } => {
            CoreOcError::Decode(format!("invalid {kind} id {value:?}"))
        }
        other => CoreOcError::Decode(format!("unexpected client error: {other}")),
    }
}

fn to_session(session: &client::SessionInfo) -> PortSession {
    PortSession {
        id: session.id.clone(),
        parent_id: session.parent_id.clone(),
        title: session.title.clone(),
        directory: session.directory().map(str::to_owned),
    }
}

fn to_snapshot(message: &client::AssistantMessage) -> AssistantSnapshot {
    AssistantSnapshot {
        text: message.text(),
        has_tool_calls: message.has_tool_calls(),
        error: message.error_message().map(str::to_owned),
        finish: message
            .finish
            .map(|finish| format!("{finish:?}").to_ascii_lowercase()),
        cost: message.cost,
        tokens: message.tokens.as_ref().map(|tokens| TokenCounts {
            input: tokens.input,
            output: tokens.output,
            reasoning: tokens.reasoning,
        }),
    }
}

fn to_gate(gate: &client::GateRequest) -> GateRequest {
    let kind = match &gate.kind {
        client::GateKind::Permission(request) => GateKind::Permission {
            action: request.action.clone(),
            resources: request.resources.clone(),
        },
        client::GateKind::Form(form) => GateKind::Form {
            title: form.title.clone(),
            field_count: form.fields.len(),
        },
    };
    GateRequest {
        id: gate.id.clone(),
        session_id: gate.session_id.clone(),
        kind,
    }
}

#[async_trait]
impl OpenCodePort for OpenCodeAdapter {
    async fn probe(&self) -> Result<ServerFacts, CoreOcError> {
        let info = self.inner.probe().await.map_err(map_error)?;
        Ok(ServerFacts {
            version: info.version,
            urls: info.urls,
        })
    }

    async fn list_sessions(&self, root_only: bool) -> Result<Vec<PortSession>, CoreOcError> {
        let sessions = self
            .inner
            .list_sessions(root_only, None)
            .await
            .map_err(map_error)?;
        Ok(sessions.iter().map(to_session).collect())
    }

    async fn activity(&self, session_id: &str) -> Result<Activity, CoreOcError> {
        let activity = self.inner.activity(session_id).await.map_err(map_error)?;
        Ok(match activity {
            client::Activity::Busy => Activity::Busy,
            client::Activity::Idle => Activity::Idle,
        })
    }

    async fn create_session(
        &self,
        directory: &Path,
        title: Option<&str>,
    ) -> Result<SessionId, CoreOcError> {
        let session = self
            .inner
            .create_session(Some(&directory.display().to_string()), title)
            .await
            .map_err(map_error)?;
        Ok(session.id)
    }

    async fn send_prompt(
        &self,
        session_id: &str,
        text: &str,
    ) -> Result<PromptAdmission, CoreOcError> {
        let admission = self
            .inner
            .send_prompt(session_id, text)
            .await
            .map_err(map_error)?;
        Ok(PromptAdmission {
            id: admission.id,
            session_id: admission.session_id,
            delivery: admission.delivery,
        })
    }

    async fn last_assistant(
        &self,
        session_id: &str,
    ) -> Result<Option<AssistantSnapshot>, CoreOcError> {
        let message = self
            .inner
            .last_assistant(session_id)
            .await
            .map_err(map_error)?;
        Ok(message.as_ref().map(to_snapshot))
    }

    async fn interrupt(&self, session_id: &str) -> Result<(), CoreOcError> {
        self.inner.interrupt(session_id).await.map_err(map_error)?;
        Ok(())
    }

    async fn pending_gates(&self) -> Result<Vec<GateRequest>, CoreOcError> {
        let gates = self.inner.pending_gates().await.map_err(map_error)?;
        Ok(gates.iter().map(to_gate).collect())
    }

    async fn reply_gate(
        &self,
        session_id: &str,
        gate_id: &str,
        decision: GateDecision,
    ) -> Result<(), CoreOcError> {
        let decision = match decision {
            GateDecision::Once => client::GateDecision::Once,
            GateDecision::Always => client::GateDecision::Always,
            GateDecision::Reject => client::GateDecision::Reject,
        };
        self.inner
            .reply_permission(session_id, gate_id, decision)
            .await
            .map_err(map_error)
    }
}
