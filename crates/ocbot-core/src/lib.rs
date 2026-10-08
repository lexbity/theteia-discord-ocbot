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
pub mod gates;
pub mod history;
pub mod outcome;
pub mod paths;
pub mod ports;
pub mod render;
pub mod state;
pub mod templates;

pub use error::{GitError, OcError};
pub use gates::{
    FormField, FormFieldKind, FormOption, FormPlaybook, GateDecision, GateKind, GateRequest,
    MAX_FORM_FIELDS, form_playbook, form_renderable,
};
pub use history::{HistoryAction, HistoryKind, HistoryRow, merge};
pub use outcome::{AssistantSnapshot, Classification, Outcome, TokenCounts, classify};
pub use paths::{dir_hash, slug};
pub use ports::HookFailure;
pub use render::{
    AttachmentSpec, ButtonKind, ComponentSpec, Footer, HISTORY_PAGE_SIZE, OutgoingMessage,
    ReportContext, component_button, component_button_state, decision_buttons, fence_neutralize,
    gate_message, headline, history_page, report, split_excerpt, staged_preview,
};
pub use state::{ALL_STATES, CasIntent, Column, Event, State, WriteValue, transition};
