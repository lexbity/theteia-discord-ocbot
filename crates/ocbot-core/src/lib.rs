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
pub mod outcome;
pub mod ports;
pub mod render;
pub mod state;
pub mod templates;

pub use error::{GitError, OcError};
pub use outcome::{AssistantSnapshot, Classification, Outcome, TokenCounts, classify};
pub use render::{
    AttachmentSpec, ButtonKind, ComponentSpec, Footer, OutgoingMessage, component_button,
    fence_neutralize, headline, report, split_excerpt, staged_preview,
};
pub use state::{ALL_STATES, CasIntent, Column, Event, State, WriteValue, transition};
