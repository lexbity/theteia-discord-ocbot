//! ocbot-core — the deterministic brain of `ocbot` (spec §6.1).
//!
//! This crate is **pure and synchronous**: it declares no I/O dependencies and
//! only defines what the bot *decides*. The shell (`crates/ocbot`) wires
//! adapters to the port contracts in [`ports`]; the state machine, outcome
//! classification, report rendering, and template engine all live here so the
//! riskiest decisions are exhaustively testable (NFR-8) and structurally
//! isolated from any I/O (NFR-11).
//!
//! Nothing in this crate opens a file, speaks a network protocol, or touches
//! git — by dependency graph, not by discipline.

pub mod error;
pub mod events;
pub mod gates;
pub mod grammar;
pub mod history;
pub mod outcome;
pub mod paths;
pub mod plan;
pub mod ports;
pub mod render;
pub mod state;
pub mod surface;
pub mod templates;

pub use error::{FsError, GitError, OcError};
pub use events::{
    HintKind, OcEvent, ReadOutcome, TAXONOMY, TaxonomyRow, WakeInput, WakeKey, WakeOutput,
    WakePolicy, WakeState, classify_event,
};
pub use gates::{
    FormField, FormFieldKind, FormOption, FormPlaybook, GateDecision, GateKind, GateRequest,
    MAX_FORM_FIELDS, form_playbook, form_renderable,
};
pub use grammar::{DecisionVerb, GrammarError, Intent, parse as parse_intent};
pub use history::{HistoryAction, HistoryKind, HistoryRow, merge};
pub use outcome::{AssistantSnapshot, Classification, Outcome, TokenCounts, classify};
pub use paths::{content_tag8, dir_hash, is_inside, sanitize_name, slug, yyyymmdd};
pub use plan::{
    DEFAULT_HEADING_MAX, DEFAULT_HEADING_MIN, OTHER_OPTION_LABEL, PLAN_PAGE_SIZE, PickOption,
    PlanPage, parse_plan, plan_page,
};
pub use ports::HookFailure;
pub use render::{
    AttachmentSpec, ButtonKind, ComponentSpec, DISCORD_MESSAGE_CAP, Footer, HISTORY_PAGE_SIZE,
    LABEL_MAX_CHARS, LS_PAGE_SIZE, Legend, LegendEntry, LsEntry, OutgoingMessage, REDACTED,
    RedactError, Redactor, ReportContext, component_button, component_button_state,
    decision_buttons, fence_neutralize, gate_message, headline, help_text, history_page,
    ls_message, normalize_emoji, plan_picker_message, render_legend, report, split_excerpt,
    staged_preview, truncate_chars,
};
pub use state::{ALL_STATES, CasIntent, Column, Event, State, WriteValue, transition};
pub use surface::{ActorId, ChatRef, Destination, MsgId, PostReceipt, Surface};
