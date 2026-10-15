//! The Signal pending-input state machine (SPEC-004 D-06/§5.6, PH-7).
//!
//! A Discord modal becomes a short text conversation here: one row per
//! `(surface, chat_ref)` in `pending_input`, a step table that says what a
//! supplied text fills and what comes next, and a `ModalSubmit` synthesized on
//! completion that is **byte-identical** to the one the Discord modal would
//! have produced — so no handler can tell the surfaces apart (FR-109).
//!
//! The words come from [`ocbot_core::render`] (prompt texts, hints, notices);
//! this module owns the transition and the persistence around it, and the
//! transition itself is a pure function ([`advance`]) so the parity claim is
//! directly testable without a daemon: `tests/signal_flows.rs` builds the
//! Discord-side source with the shared harness and compares.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use ocbot_core::render::{
    FLOW_MODAL, FLOW_RUN, default_hint, field_prompt, first_step, flow_steps, form_header,
    is_cancel, step_field, step_prompt, with_cancel_hint,
};

use crate::adapters::store::{PendingInputRow, StoreHandle};
use crate::app::context::{AppContext, ChatPort};
use crate::app::handlers::{ls, prompt, run};
use crate::app::interaction::ModalSpec;
use crate::app::metrics::Metrics;

use super::adapter::SignalAdapter;

/// The longest derived label a `run` flow can produce from its prompt (the
/// Discord modal asks for one; a text flow derives it from the first line).
const RUN_LABEL_CHARS: usize = 40;

/// One field of a generic modal form, remembered in the modal's own order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlowField {
    /// The field id (the modal input's custom id).
    pub id: String,
    /// The displayed label.
    pub label: String,
    /// The prefilled default, when the modal carried one.
    #[serde(default)]
    pub default: Option<String>,
    /// Whether the operator must supply this field.
    #[serde(default)]
    pub required: bool,
}

/// The persisted draft of a generic modal form.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModalDraft {
    /// The modal `custom_id` the flow submits to.
    pub custom_id: String,
    /// Every field, in the modal's own order (parity with Discord's submit).
    #[serde(default)]
    pub fields: Vec<FlowField>,
    /// The indices into `fields` of the required ones, in order: the only
    /// fields a text form asks for. Optional fields keep their prefill.
    #[serde(default)]
    pub ask: Vec<usize>,
    /// The values collected so far, one per asked field.
    #[serde(default)]
    pub collected: Vec<(String, String)>,
}

impl ModalDraft {
    /// Build the draft for a modal spec: all fields (for the submit values) and
    /// the required ones (for the questions).
    #[must_use]
    pub fn from_spec(custom_id: &str, fields: Vec<FlowField>) -> Self {
        let ask = fields
            .iter()
            .enumerate()
            .filter(|(_, field)| field.required)
            .map(|(index, _)| index)
            .collect();
        Self {
            custom_id: custom_id.to_owned(),
            fields,
            ask,
            collected: Vec::new(),
        }
    }

    /// Whether this form has anything to ask.
    #[must_use]
    pub fn has_questions(&self) -> bool {
        !self.ask.is_empty()
    }

    /// The field the Nth question asks for.
    fn question(&self, index: usize) -> Option<&FlowField> {
        self.ask
            .get(index)
            .and_then(|field| self.fields.get(*field))
    }

    /// The values this draft submits: each field in modal order, the collected
    /// value when one exists and the prefill (or empty) otherwise. This is
    /// exactly what the Discord modal submit carries, in the same order.
    #[must_use]
    pub fn values(&self) -> Vec<(String, String)> {
        self.fields
            .iter()
            .map(|field| {
                let collected = self
                    .collected
                    .iter()
                    .find(|(id, _)| id == &field.id)
                    .map(|(_, value)| value.clone());
                (
                    field.id.clone(),
                    collected.unwrap_or_else(|| field.default.clone().unwrap_or_default()),
                )
            })
            .collect()
    }
}

/// The persisted draft of the named `run` flow.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunDraft {
    /// The directory collected at the first step.
    #[serde(default)]
    pub dir: String,
}

/// What one supplied text did to an open flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Advance {
    /// The flow moved on: persist this draft and post this prompt.
    Next {
        /// The new step name.
        step: String,
        /// The serialized draft to persist.
        draft: String,
        /// The prompt to post.
        prompt: String,
    },
    /// The flow is complete: run this modal submit.
    Submit {
        /// The modal `custom_id` the Discord modal would have carried.
        custom_id: String,
        /// The submitted values, in the modal's own order.
        values: Vec<(String, String)>,
    },
    /// The operator closed the flow.
    Cancelled,
    /// The row names a flow or step this adapter cannot advance; close it.
    Unknown,
}

impl Advance {
    /// The submit this advance completes with, when it completes.
    #[must_use]
    pub fn submit(&self) -> Option<(&str, &[(String, String)])> {
        match self {
            Advance::Submit { custom_id, values } => Some((custom_id.as_str(), values.as_slice())),
            _ => None,
        }
    }
}

/// The label a text `run` derives from its prompt (Discord asks for one).
#[must_use]
pub fn run_label(prompt: &str) -> String {
    prompt
        .lines()
        .next()
        .unwrap_or("run")
        .chars()
        .take(RUN_LABEL_CHARS)
        .collect()
}

/// Serialize the run flow's draft between its two steps.
#[must_use]
pub fn run_draft(dir: &str) -> String {
    serde_json::to_string(&RunDraft {
        dir: dir.to_owned(),
    })
    .unwrap_or_else(|_| "{}".to_owned())
}

/// The pure transition of one supplied text against the open `row`.
///
/// `cwd` is the remembered cwd the caller already read (some flows default
/// their directory field to it). No I/O, no store writes: the caller applies
/// the result, which is what makes the Discord-parity assertion possible.
#[must_use]
pub fn advance(row: &PendingInputRow, text: &str, cwd: &str) -> Advance {
    if is_cancel(text) {
        return Advance::Cancelled;
    }
    match row.flow.as_str() {
        FLOW_RUN => advance_run(&row.step, &row.draft, text),
        "prompt" => advance_named("prompt", &row.step, text, cwd),
        "cd" => advance_named("cd", &row.step, text, cwd),
        FLOW_MODAL => advance_modal(&row.step, &row.draft, text),
        _ => Advance::Unknown,
    }
}

/// The `run` flow: directory, then prompt, then submit.
fn advance_run(step: &str, draft_json: &str, text: &str) -> Advance {
    let Some(current) = flow_steps(FLOW_RUN)
        .iter()
        .find(|candidate| candidate.step == step)
    else {
        return Advance::Unknown;
    };
    if let Some(next) = current.next {
        let draft = serde_json::to_string(&RunDraft {
            dir: text.trim().to_owned(),
        })
        .unwrap_or_else(|_| "{}".to_owned());
        let prompt = step_prompt(FLOW_RUN, next).unwrap_or("Send the next value.");
        Advance::Next {
            step: next.to_owned(),
            draft,
            prompt: prompt.to_owned(),
        }
    } else {
        let draft: RunDraft = serde_json::from_str(draft_json).unwrap_or_default();
        Advance::Submit {
            custom_id: run::NEW_RUN_MODAL.to_owned(),
            values: vec![
                ("directory".to_owned(), draft.dir),
                ("label".to_owned(), run_label(text)),
                ("plan".to_owned(), String::new()),
            ],
        }
    }
}

/// The single-field named flows (`prompt`, `cd`): ask once, then submit.
fn advance_named(flow: &str, step: &str, text: &str, cwd: &str) -> Advance {
    let Some(field) = step_field(flow, step) else {
        return Advance::Unknown;
    };
    let value = text.trim().to_owned();
    if flow == "prompt" {
        Advance::Submit {
            custom_id: prompt::PROMPT_MODAL.to_owned(),
            values: vec![
                ("directory".to_owned(), cwd.to_owned()),
                (field.to_owned(), value),
            ],
        }
    } else {
        Advance::Submit {
            custom_id: ls::CD_MODAL.to_owned(),
            values: vec![(field.to_owned(), value)],
        }
    }
}

/// A generic modal form: ask each required field in order, then submit the
/// collected values in the modal's own order (optional fields keep prefills,
/// so a text form never asks for what the Discord modal left blank).
fn advance_modal(step: &str, draft_json: &str, text: &str) -> Advance {
    let mut draft: ModalDraft = serde_json::from_str(draft_json).unwrap_or_default();
    let index = step.parse::<usize>().unwrap_or(0);
    let Some(current) = draft.question(index).cloned() else {
        return Advance::Submit {
            custom_id: draft.custom_id.clone(),
            values: draft.values(),
        };
    };
    let value = if text.trim().is_empty() {
        current.default.clone().unwrap_or_default()
    } else {
        text.trim().to_owned()
    };
    // Rewriting a question replaces its value; every earlier one is kept.
    draft.collected.truncate(index);
    draft.collected.push((current.id.clone(), value));
    match draft.question(index + 1) {
        Some(next) => {
            let serialized = serde_json::to_string(&draft).unwrap_or_else(|_| "{}".to_owned());
            let mut prompt = field_prompt(&next.id, &next.label, index + 1, draft.ask.len());
            if let Some(default) = &next.default {
                prompt.push_str(&default_hint(default));
            }
            Advance::Next {
                step: (index + 1).to_string(),
                draft: serialized,
                prompt,
            }
        }
        None => Advance::Submit {
            custom_id: draft.custom_id.clone(),
            values: draft.values(),
        },
    }
}

/// The Signal pending-input machine: the effects around [`advance`].
pub struct FlowEngine {
    ctx: Arc<AppContext>,
    adapter: Arc<SignalAdapter>,
    operator: String,
    store: StoreHandle,
    metrics: Arc<Metrics>,
}

impl FlowEngine {
    /// Build the engine over the app context and the surface's adapter.
    pub fn new(
        ctx: Arc<AppContext>,
        adapter: Arc<SignalAdapter>,
        operator: impl Into<String>,
    ) -> Self {
        let store = ctx.store.clone();
        let metrics = ctx.metrics.clone();
        Self {
            ctx,
            adapter,
            operator: operator.into(),
            store,
            metrics,
        }
    }

    /// The engine's destination (the operator chat).
    #[must_use]
    pub fn destination(&self) -> ocbot_core::Destination {
        self.adapter.destination()
    }

    /// The namespaced actor id for the operator on this surface.
    #[must_use]
    pub fn actor(&self) -> String {
        ocbot_core::ActorId::from_parts(ocbot_core::Surface::Signal, &self.operator).0
    }

    /// Open a named flow at its first step (replacing any open flow, D-06).
    pub async fn open(&self, flow: &str) -> bool {
        match first_step(flow) {
            Some(step) => self.open_at(flow, step, "{}").await,
            None => false,
        }
    }

    /// Open a named flow at `step` with the persisted `draft`, replacing any
    /// open flow (D-06: one row per chat). Returns whether a prompt was posted.
    pub async fn open_at(&self, flow: &str, step: &str, draft: &str) -> bool {
        if self.persist(flow, step, draft).await.is_none() {
            return false;
        }
        let Some(prompt) = step_prompt(flow, step) else {
            return false;
        };
        self.post(&with_cancel_hint(prompt)).await;
        true
    }

    /// Open the text form for a Discord-equivalent modal spec (D-04).
    ///
    /// Returns the submit to run when the spec has nothing to ask (every field
    /// optional): the Discord modal would have submitted its prefills. Otherwise
    /// the header and the first field's prompt are posted and `None` is
    /// returned; the caller runs the submit when the flow completes.
    pub async fn open_modal(&self, spec: &ModalSpec) -> Option<Advance> {
        let fields: Vec<FlowField> = spec
            .inputs
            .iter()
            .map(|input| FlowField {
                id: input.id.clone(),
                label: input.label.clone(),
                default: input.value.clone(),
                required: input.required,
            })
            .collect();
        let draft = ModalDraft::from_spec(&spec.custom_id, fields);
        if !draft.has_questions() {
            return Some(Advance::Submit {
                custom_id: spec.custom_id.clone(),
                values: draft.values(),
            });
        }
        self.post(&form_header(&spec.title)).await;
        let first = draft.question(0).cloned()?;
        self.persist(FLOW_MODAL, "0", &serialize(&draft)).await?;
        let mut prompt = field_prompt(&first.id, &first.label, 0, draft.ask.len());
        if let Some(default) = &first.default {
            prompt.push_str(&default_hint(default));
        }
        self.post(&with_cancel_hint(&prompt)).await;
        None
    }

    /// Take the open flow, if any (delete + return).
    pub async fn take(&self) -> Option<PendingInputRow> {
        let row = self
            .store
            .take_pending(ocbot_core::Surface::Signal.as_str(), &self.operator)
            .await
            .ok()
            .flatten();
        if row.is_some() {
            self.refresh_gauge().await;
        }
        row
    }

    /// Persist the next step of a flow (upsert; replaces any open flow).
    pub async fn persist(&self, flow: &str, step: &str, draft: &str) -> Option<()> {
        let result = self
            .store
            .put_pending(
                ocbot_core::Surface::Signal.as_str(),
                &self.operator,
                flow,
                step,
                draft,
                ocbot_core::FLOW_TTL_SECS,
                self.ctx.clock.now(),
            )
            .await;
        match result {
            Ok(()) => {
                self.refresh_gauge().await;
                Some(())
            }
            Err(error) => {
                tracing::warn!(error = %error, "put_pending failed");
                None
            }
        }
    }

    /// Close the open flow with one notice (used for `cancel` and for rows the
    /// adapter cannot advance).
    pub async fn close(&self, notice: &str) {
        let _ = self
            .store
            .take_pending(ocbot_core::Surface::Signal.as_str(), &self.operator)
            .await;
        self.refresh_gauge().await;
        self.post(notice).await;
    }

    /// Recount the `ocbot_pending_flows_open` gauge by flow name (FR-113).
    pub async fn refresh_gauge(&self) {
        let Ok(rows) = self.store.all_pending().await else {
            return;
        };
        let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for row in rows {
            *counts.entry(row.flow).or_insert(0) += 1;
        }
        for flow in ocbot_core::FLOW_NAMES {
            let count = counts.get(flow).copied().unwrap_or(0);
            self.metrics.set_pending_flows_open(flow, count);
        }
    }

    /// Post one plain notice through the adapter (best-effort).
    async fn post(&self, content: &str) {
        let message = ocbot_core::OutgoingMessage {
            content: content.to_owned(),
            attachment: None,
            components: Vec::new(),
            mention_allowlist: Vec::new(),
        };
        if let Err(error) = self.adapter.post(&self.destination(), &message).await {
            tracing::warn!(error = %error, "flow notice post failed");
        }
    }
}

/// Serialize a draft, falling back to an empty object (never a failure path
/// the operator sees).
fn serialize(draft: &ModalDraft) -> String {
    serde_json::to_string(draft).unwrap_or_else(|_| "{}".to_owned())
}

#[cfg(test)]
mod tests {
    use ocbot_core::render::next_step;

    use super::{
        Advance, FLOW_MODAL, FLOW_RUN, FlowField, ModalDraft, RunDraft, advance, advance_modal,
        advance_run,
    };
    use crate::adapters::store::PendingInputRow;

    fn row(flow: &str, step: &str, draft: &str) -> PendingInputRow {
        PendingInputRow {
            surface: "signal".to_owned(),
            chat_ref: "+15551234567".to_owned(),
            flow: flow.to_owned(),
            step: step.to_owned(),
            draft: draft.to_owned(),
            updated_at: 1_700_000_000,
            expires_at: 1_700_000_600,
        }
    }

    fn draft(custom_id: &str, fields: Vec<FlowField>) -> String {
        serde_json::to_string(&ModalDraft::from_spec(custom_id, fields)).expect("serialize")
    }

    fn values(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(id, value)| ((*id).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn cancel_closes_whatever_flow_is_open() {
        let row = row(FLOW_RUN, "await_path", "{}");
        assert_eq!(advance(&row, "cancel", ""), Advance::Cancelled);
        assert_eq!(advance(&row, " CANCEL ", ""), Advance::Cancelled);
    }

    #[test]
    fn the_run_flow_collects_the_directory_then_submits() {
        let first = advance(&row(FLOW_RUN, "await_path", "{}"), "/tmp/x", "");
        assert!(
            matches!(first, Advance::Next { .. }),
            "expected a next step: {first:?}"
        );
        let Advance::Next { step, draft, .. } = first else {
            return;
        };
        assert_eq!(step, "await_prompt");
        let stored: RunDraft = serde_json::from_str(&draft).expect("draft");
        assert_eq!(stored.dir, "/tmp/x");
        let second = advance(&row(FLOW_RUN, &step, &draft), "do the thing", "");
        assert_eq!(
            second.submit(),
            Some((
                "v1:newrun",
                values(&[
                    ("directory", "/tmp/x"),
                    ("label", "do the thing"),
                    ("plan", ""),
                ])
                .as_slice()
            ))
        );
    }

    #[test]
    fn a_run_label_is_the_first_line_truncated() {
        assert_eq!(super::run_label("do the thing"), "do the thing");
        let long: String = "x".repeat(80);
        assert_eq!(super::run_label(&long).chars().count(), 40);
        assert_eq!(super::run_label("first\nsecond"), "first");
    }

    #[test]
    fn the_run_submit_reads_the_directory_from_the_persisted_draft() {
        // The step name alone cannot carry the directory; the draft does.
        let advance = advance_run("await_prompt", r#"{"dir":"/tmp/stored"}"#, "go");
        assert_eq!(
            advance.submit(),
            Some((
                "v1:newrun",
                values(&[("directory", "/tmp/stored"), ("label", "go"), ("plan", ""),]).as_slice()
            ))
        );
    }

    #[test]
    fn the_prompt_flow_submits_the_cwd_and_the_text() {
        let advance = advance(&row("prompt", "await_text", "{}"), "do it", "/tmp/cwd");
        assert_eq!(
            advance.submit(),
            Some((
                "v1:prompt",
                values(&[("directory", "/tmp/cwd"), ("prompt", "do it")]).as_slice()
            ))
        );
    }

    #[test]
    fn the_cd_flow_submits_the_path() {
        let advance = advance(&row("cd", "await_path", "{}"), "/tmp/x", "");
        assert_eq!(
            advance.submit(),
            Some(("v1:cd", values(&[("path", "/tmp/x")]).as_slice()))
        );
    }

    #[test]
    fn a_modal_form_asks_only_required_fields_and_submits_in_modal_order() {
        // subject is required, body is not: a text form asks one question, and
        // the submit still carries both fields in the modal's own order — which
        // is what the Discord modal would have posted.
        let serialized = draft(
            "v1:acceptgo:7",
            vec![
                FlowField {
                    id: "subject".to_owned(),
                    label: "Commit subject".to_owned(),
                    default: Some("prefilled".to_owned()),
                    required: true,
                },
                FlowField {
                    id: "body".to_owned(),
                    label: "Commit body (optional)".to_owned(),
                    default: None,
                    required: false,
                },
            ],
        );
        let advance = advance_modal("0", &serialized, "ship it");
        assert_eq!(
            advance.submit(),
            Some((
                "v1:acceptgo:7",
                values(&[("subject", "ship it"), ("body", "")]).as_slice()
            ))
        );
    }

    #[test]
    fn two_required_fields_are_asked_in_order() {
        let serialized = draft(
            "v1:formgo:9",
            vec![
                FlowField {
                    id: "first".to_owned(),
                    label: "First".to_owned(),
                    default: None,
                    required: true,
                },
                FlowField {
                    id: "second".to_owned(),
                    label: "Second".to_owned(),
                    default: Some("hint".to_owned()),
                    required: true,
                },
            ],
        );
        let first = advance_modal("0", &serialized, "one");
        assert!(
            matches!(first, Advance::Next { .. }),
            "expected a next step: {first:?}"
        );
        let Advance::Next {
            step,
            draft,
            prompt,
        } = first
        else {
            return;
        };
        assert_eq!(step, "1");
        assert!(prompt.contains("2/2"), "{prompt}");
        assert!(prompt.contains("empty keeps: hint"), "{prompt}");
        // An empty reply to the second question keeps its prefill.
        let second = advance_modal(&step, &draft, "");
        assert_eq!(
            second.submit(),
            Some((
                "v1:formgo:9",
                values(&[("first", "one"), ("second", "hint")]).as_slice()
            ))
        );
    }

    #[test]
    fn a_non_empty_default_is_kept_when_the_reply_is_empty() {
        let serialized = draft(
            "v1:editgo:7",
            vec![FlowField {
                id: "prompt".to_owned(),
                label: "Prompt".to_owned(),
                default: Some("the stored prompt".to_owned()),
                required: true,
            }],
        );
        let advance = advance_modal("0", &serialized, "   ");
        assert_eq!(
            advance.submit(),
            Some((
                "v1:editgo:7",
                values(&[("prompt", "the stored prompt")]).as_slice()
            ))
        );
    }

    #[test]
    fn modal_values_walk_the_modal_order_not_the_collection_order() {
        let mut draft = ModalDraft::from_spec(
            "v1:formgo:3",
            vec![
                FlowField {
                    id: "a".to_owned(),
                    label: "A".to_owned(),
                    default: None,
                    required: true,
                },
                FlowField {
                    id: "b".to_owned(),
                    label: "B".to_owned(),
                    default: Some("kept".to_owned()),
                    required: false,
                },
            ],
        );
        draft.collected = vec![("a".to_owned(), "typed".to_owned())];
        assert_eq!(draft.values(), values(&[("a", "typed"), ("b", "kept")]));
    }

    #[test]
    fn a_step_past_the_last_field_submits_what_was_collected() {
        let serialized = draft(
            "v1:planothergo:4",
            vec![FlowField {
                id: "step".to_owned(),
                label: "Step".to_owned(),
                default: None,
                required: true,
            }],
        );
        let advance = advance_modal("9", &serialized, "ignored");
        assert_eq!(
            advance.submit(),
            Some(("v1:planothergo:4", values(&[("step", "")]).as_slice()))
        );
    }

    #[test]
    fn an_unknown_flow_or_step_is_not_advanceable() {
        assert_eq!(
            advance(&row("mystery", "await_path", "{}"), "x", ""),
            Advance::Unknown
        );
        assert_eq!(advance(&row("cd", "nope", "{}"), "x", ""), Advance::Unknown);
        assert_eq!(advance_run("nope", "{}", "x"), Advance::Unknown);
    }

    #[test]
    fn a_broken_draft_json_submits_an_empty_form() {
        let advance = advance_modal("0", "not json", "x");
        assert_eq!(advance.submit(), Some(("", [].as_slice())));
    }

    #[test]
    fn lifecycle_notices_come_from_the_core_vocabulary() {
        // The adapter holds no notice text: every word a flow's lifecycle posts
        // is rendered by ocbot-core (PH-7), so the two cannot drift.
        assert_eq!(
            ocbot_core::unknown_flow(),
            "unknown open flow; it has been cancelled"
        );
        assert!(ocbot_core::restart_notice(FLOW_RUN, "{}").contains("`run` draft"));
        assert!(ocbot_core::restart_notice(FLOW_MODAL, "{}").contains("text form"));
        assert!(ocbot_core::draft_echo(r#"{"dir":"/tmp/x"}"#).contains("dir: /tmp/x"));
        assert!(ocbot_core::expired(FLOW_RUN).contains("`run` flow expired"));
    }

    #[test]
    fn named_flow_steps_resolve_their_successors() {
        assert_eq!(next_step(FLOW_RUN, "await_path"), Some("await_prompt"));
        assert_eq!(next_step(FLOW_RUN, "await_prompt"), None);
    }
}
