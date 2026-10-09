//! The opencode adapter: translates `opencode-v2-client` types onto
//! `ocbot-core::ports::OpenCodePort` (so the port vocabulary is the core's,
//! and replacing the client never touches handlers).

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use ocbot_core::error::OcError as CoreOcError;
use ocbot_core::events::{OcEvent, classify_event};
use ocbot_core::gates::{FormField, FormFieldKind, FormOption};
use ocbot_core::outcome::{AssistantSnapshot, TokenCounts};
use ocbot_core::ports::{
    Activity, GateDecision, GateKind, GateRequest, OpenCodePort, PermissionEffect, PermissionRule,
    PortSession, PromptAdmission, ServerFacts, SessionId,
};
use opencode_v2_client as client;
use tokio::sync::{mpsc, watch};

use crate::app::context::{EventSource, FRAME_CHANNEL_CAPACITY, StreamHandle, StreamItem};
use crate::app::metrics::Metrics;

/// Maps the standalone client onto the core port contract. The client is
/// shared with the event source (one HTTP client, two surfaces).
#[derive(Debug)]
pub struct OpenCodeAdapter {
    inner: Arc<client::Client>,
}

impl OpenCodeAdapter {
    /// Wrap a configured client.
    pub fn new(inner: Arc<client::Client>) -> Self {
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

/// The production [`EventSource`]: opens one `subscribe_events` connection per
/// [`EventSource::connect`] and pumps it into a bounded channel with
/// `try_send` + drop-count (§4.6: a dropped hint is safe by P-19; the pump
/// MUST NOT apply backpressure to the read loop).
pub struct OpenCodeEventSource {
    client: Arc<client::Client>,
    metrics: Arc<Metrics>,
}

impl OpenCodeEventSource {
    /// Build the event source over the shared client.
    pub fn new(client: Arc<client::Client>, metrics: Arc<Metrics>) -> Self {
        Self { client, metrics }
    }
}

#[async_trait]
impl EventSource for OpenCodeEventSource {
    async fn connect(&self) -> Result<StreamHandle, CoreOcError> {
        let mut stream = self.client.subscribe_events().await.map_err(map_error)?;
        let (tx, rx) = mpsc::channel::<StreamItem>(FRAME_CHANNEL_CAPACITY);
        let (stop_tx, stop_rx) = watch::channel(false);
        let handle_stop_rx = stop_rx.clone();
        let decode_failures = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let dropped_frames = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let pump_decode = Arc::clone(&decode_failures);
        let pump_dropped = Arc::clone(&dropped_frames);
        let metrics = self.metrics.clone();
        tokio::spawn(async move {
            let mut stop_rx = stop_rx;
            loop {
                let item = tokio::select! {
                    frame = stream.next_frame() => match frame {
                        Ok(Some(frame)) => frame,
                        Ok(None) => {
                            let _ = tx.send(StreamItem::Closed).await;
                            return;
                        }
                        Err(error) => {
                            tracing::warn!(error = %error, "event stream transport error");
                            let _ = tx.send(StreamItem::Closed).await;
                            return;
                        }
                    },
                    _ = stop_rx.changed() => return,
                };
                pump_decode.store(stream.decode_failures(), Ordering::Relaxed);
                // Frames are counted by kind with their taxonomy mapping
                // (§4.9: `mapped=false` makes taxonomy drift live-visible).
                let mapped =
                    classify_event(&item.event, item.session_id.as_deref()) != OcEvent::Unkeyed;
                metrics.inc_frame(&item.event, mapped);
                match tx.try_send(StreamItem::Frame(item)) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        pump_dropped.fetch_add(1, Ordering::Relaxed);
                        metrics.inc_frames_dropped();
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => return,
                }
            }
        });
        Ok(StreamHandle::new(
            rx,
            stop_tx,
            handle_stop_rx,
            decode_failures,
            dropped_frames,
        ))
    }
}
