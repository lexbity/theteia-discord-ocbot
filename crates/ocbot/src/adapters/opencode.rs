//! The opencode adapter: translates `opencode-v2-client` types onto
//! `ocbot-core::ports::OpenCodePort` (so the port vocabulary is the core's,
//! and replacing the client never touches handlers).

use std::path::Path;

use async_trait::async_trait;
use ocbot_core::error::OcError as CoreOcError;
use ocbot_core::gates::{FormField, FormFieldKind, FormOption};
use ocbot_core::outcome::{AssistantSnapshot, TokenCounts};
use ocbot_core::ports::{
    Activity, GateDecision, GateKind, GateRequest, OpenCodePort, PermissionEffect, PermissionRule,
    PortSession, PromptAdmission, ServerFacts, SessionId,
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
        id: (!message.id.is_empty()).then(|| message.id.clone()),
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
            message: request.message.clone(),
        },
        client::GateKind::Form(form) => GateKind::Form {
            title: form.title.clone(),
            fields: form.fields.iter().map(to_field).collect(),
        },
    };
    let payload = match &gate.kind {
        client::GateKind::Permission(request) => serde_json::to_value(request),
        client::GateKind::Form(form) => serde_json::to_value(form),
    };
    GateRequest {
        id: gate.id.clone(),
        session_id: gate.session_id.clone(),
        kind,
        payload: payload.unwrap_or(serde_json::Value::Null),
    }
}

/// Map a wire form field onto the core vocabulary; the `Other` catch-all keeps
/// an unknown field from ever panicking deserialization (P16).
fn to_field(field: &client::FormField) -> FormField {
    let (key, label, kind) = match field {
        client::FormField::String(value) if !value.options.is_empty() => (
            value.key.clone(),
            value.title.clone(),
            FormFieldKind::Choice(to_options(&value.options)),
        ),
        client::FormField::String(value) => {
            (value.key.clone(), value.title.clone(), FormFieldKind::Text)
        }
        client::FormField::Number(value) | client::FormField::Integer(value) => {
            (value.key.clone(), value.title.clone(), FormFieldKind::Text)
        }
        client::FormField::Boolean(value) => (
            value.key.clone(),
            value.title.clone(),
            FormFieldKind::Boolean,
        ),
        client::FormField::Multiselect(value) => (
            value.key.clone(),
            value.title.clone(),
            FormFieldKind::Multiselect(to_options(&value.options)),
        ),
        client::FormField::External(value) => (
            value.key.clone(),
            None,
            FormFieldKind::External(value.url.clone()),
        ),
        client::FormField::Other => (String::new(), None, FormFieldKind::Unknown),
    };
    FormField { key, label, kind }
}

fn to_options(options: &[client::FormOption]) -> Vec<FormOption> {
    options
        .iter()
        .map(|option| FormOption {
            value: option.value.clone(),
            label: option.label.clone(),
        })
        .collect()
}

fn to_effect(effect: PermissionEffect) -> client::PermissionEffect {
    match effect {
        PermissionEffect::Allow => client::PermissionEffect::Allow,
        PermissionEffect::Deny => client::PermissionEffect::Deny,
        PermissionEffect::Ask => client::PermissionEffect::Ask,
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
        permissions: Option<&[PermissionRule]>,
    ) -> Result<SessionId, CoreOcError> {
        let rules: Option<Vec<client::PermissionRule>> = permissions.map(|rules| {
            rules
                .iter()
                .map(|rule| client::PermissionRule {
                    action: rule.action.clone(),
                    resource: rule.resource.clone(),
                    effect: to_effect(rule.effect),
                })
                .collect()
        });
        let session = self
            .inner
            .create_session_with(
                Some(&directory.display().to_string()),
                title,
                rules.as_deref(),
            )
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

    async fn reply_form(
        &self,
        session_id: &str,
        form_id: &str,
        answer: serde_json::Value,
    ) -> Result<(), CoreOcError> {
        self.inner
            .reply_form(session_id, form_id, answer)
            .await
            .map_err(map_error)
    }
}
