//! Report rendering, truncation, and fence neutralization (spec §6.6 P10).
//!
//! Handlers produce [`OutgoingMessage`] **specs** — pure data, serenity-free.
//! The Discord adapter is the only place that turns a spec into a payload, and
//! its single builder always sets `allowed_mentions { parse: [], users:
//! [OPERATOR] }` (FR-3, structurally enforced).
//!
//! The module is split by concern (SPEC-002 §4.3): `report` renders finished
//! actions and the staged preview, `gate` renders permission/form requests,
//! `browser` renders the `/ls`, plan-picker and history surfaces, `redact`
//! scrubs secrets, `label` holds the shared text helpers, `legend` maps the
//! reaction table for surfaces without buttons, and `flow` holds the
//! pending-input step tables and their prompt texts (a modal's text form).
//! The submodules are private; this facade re-exports the public API so
//! `crate::render::...` imports are unchanged.

mod browser;
mod flow;
mod gate;
mod label;
mod legend;
mod policy;
pub mod projects;
mod redact;
mod report;

use crate::surface::ActorId;

pub use browser::{
    HISTORY_PAGE_SIZE, LS_PAGE_SIZE, LsEntry, history_page, ls_message, plan_picker_message,
};
pub use projects::{
    full_readme_message, plan_preview_message, project_details_message, project_list_message,
    staged_preview_project,
};
pub use flow::{
    CANCEL_WORD, FLOW_MODAL, FLOW_NAMES, FLOW_PROMPT, FLOW_RUN, FLOW_TTL_SECS,
    MODAL_FAMILIES, RUN_STEPS, cancelled, default_hint, draft_echo, expired, field_prompt,
    first_step, flow_steps, form_header, is_cancel, is_modal_id, modal_family_samples, next_step,
    restart_notice, step_field, step_prompt, unknown_flow, with_cancel_hint,
};
pub use gate::gate_message;
pub use legend::{
    ACCEPT_EMOJI, DISCARD_EMOJI, FOLLOWUP_EMOJI, Legend, LegendEntry, PARK_EMOJI,
    emoji_for_custom_id, help_text, keycap, normalize_emoji, render_legend, verb_label,
};
pub use policy::{ReportExcerpt, SUPPRESSED_ATTACHMENT_HINT, SurfacePolicy, apply_surface_policy};
pub use redact::{REDACTED, RedactError, Redactor};
pub use report::{
    DISCORD_MESSAGE_CAP, EXCERPT_TARGET, Footer, LABEL_MAX_CHARS, REPORT_ATTACHMENT_NAME,
    ReportContext, decision_buttons, fence_neutralize, headline, report, split_excerpt,
    staged_preview,
};

pub use label::truncate_chars;

pub(crate) use label::collapse_whitespace;

/// The FR-3 mention allowlist for one actor: the Discord snowflake when the
/// actor is a Discord identity, and empty on every other surface (which has no
/// mention mechanism and ignores the field). The Discord adapter enforces the
/// restriction structurally on top of this — this is the vocabulary, not the
/// guarantee.
#[must_use]
pub fn mention_allowlist(actor: &str) -> Vec<u64> {
    ActorId(actor.to_owned()).discord_id().into_iter().collect()
}

/// The mentionable snowflake behind an actor, or `0` when the actor has none:
/// what the render builders key their FR-3 allowlist on.
#[must_use]
pub fn mentionable_id(actor: &str) -> u64 {
    ActorId(actor.to_owned()).discord_id().unwrap_or(0)
}

/// A message the bot wants to post. Pure data.
#[derive(Debug, Clone, PartialEq)]
pub struct OutgoingMessage {
    /// The message text (concatenated inline content).
    pub content: String,
    /// An optional file attachment (full report when truncated).
    pub attachment: Option<AttachmentSpec>,
    /// Interactive components (buttons).
    pub components: Vec<ComponentSpec>,
    /// Discord-rendered mention allowlist (FR-3): the adapter sets
    /// `allowed_mentions.users` from exactly this list and never the
    /// `everyone`/`here` bits. Other surfaces ignore it.
    pub mention_allowlist: Vec<u64>,
}

/// A file attachment.
#[derive(Debug, Clone, PartialEq)]
pub struct AttachmentSpec {
    /// The file name, e.g. `report.md`.
    pub filename: String,
    /// The file bytes.
    pub bytes: Vec<u8>,
}

/// One button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentSpec {
    /// The button label.
    pub label: String,
    /// The `custom_id` the router will answer (spec §6.6 grammar).
    pub custom_id: String,
    /// The button style.
    pub kind: ButtonKind,
    /// Whether the button is disabled (FR-15: a verb that cannot run here is
    /// shown disabled-with-reason, never as a dead button).
    pub disabled: bool,
}

/// Discord button styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// Blurple.
    Primary,
    /// Grey.
    Secondary,
    /// Red.
    Danger,
    /// Green.
    Success,
}

/// Build a button spec.
pub fn component_button(
    label: impl Into<String>,
    custom_id: impl Into<String>,
    kind: ButtonKind,
) -> ComponentSpec {
    component_button_state(label, custom_id, kind, false)
}

/// Build a button spec with an explicit enabled/disabled state.
pub fn component_button_state(
    label: impl Into<String>,
    custom_id: impl Into<String>,
    kind: ButtonKind,
    disabled: bool,
) -> ComponentSpec {
    ComponentSpec {
        label: label.into(),
        custom_id: custom_id.into(),
        kind,
        disabled,
    }
}
