//! The interaction vocabulary the router reasons about (serenity-free).
//! The adapter converts raw Discord interactions into [`InteractionSource`]s;
//! routing is two-phase: an immediate [`AckPlan`] (modal or defer, decided
//! before any slow work — the 3 s rule) and an [`execute`](crate::app::router::execute)
//! pass that owns the side effects.

use ocbot_core::{Destination, MsgId, PostReceipt};

/// A normalized interaction.
#[derive(Debug, Clone)]
pub struct InteractionSource {
    /// What kind of interaction this is.
    pub kind: SourceKind,
    /// The invoking user, as a namespaced `ActorId` string (`discord:42`,
    /// `signal:+1555…` — SPEC-004 D-03; adapters namespace at build time).
    pub user_id: String,
    /// Where this interaction's reply belongs: the surface and chat it
    /// happened in (SPEC-004 §5.2 — never a Discord snowflake in the core).
    pub destination: Destination,
    /// The guild, when not a DM (Discord-only; `None` elsewhere).
    pub guild_id: Option<u64>,
    /// The message the component is attached to, when the surface addresses
    /// messages by snowflake (Discord buttons); `None` on surfaces that do not.
    pub message_id: Option<u64>,
    /// Whether this is a direct conversation with the operator (always true on
    /// surfaces with a single 1:1 chat; computed from `guild_id` on Discord).
    pub is_dm: bool,
}

/// The interaction shape the router understands.
#[derive(Debug, Clone)]
pub enum SourceKind {
    /// A slash command.
    SlashCommand {
        /// The command name (`run`, `status`).
        name: String,
    },
    /// A button click.
    Component {
        /// The `custom_id` (spec §6.6 grammar).
        custom_id: String,
    },
    /// A modal submit.
    ModalSubmit {
        /// The modal `custom_id`.
        custom_id: String,
        /// The submitted text inputs, in order.
        values: Vec<TextInput>,
    },
}

/// One modal text input value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextInput {
    /// The input's custom id.
    pub id: String,
    /// The submitted text.
    pub value: String,
}

impl InteractionSource {
    /// The receipt of the message this interaction came from, when the surface
    /// addresses messages by id (a Discord button's origin message). `None`
    /// elsewhere — and the surface comes from this source's own destination, so
    /// a quiet edit always addresses the chat the interaction happened in.
    #[must_use]
    pub fn receipt(&self) -> Option<PostReceipt> {
        self.message_id.map(|message_id| PostReceipt {
            surface: self.destination.surface,
            msg_id: MsgId(message_id.to_string()),
        })
    }

    /// Look up a modal input value by id.
    pub fn input(&self, id: &str) -> Option<&str> {
        match &self.kind {
            SourceKind::ModalSubmit { values, .. } => values
                .iter()
                .find(|input| input.id == id)
                .map(|input| input.value.as_str()),
            _ => None,
        }
    }
}

/// The immediate acknowledgement the adapter must send, decided before any
/// slow work (ack discipline, §6.6).
#[derive(Debug, Clone)]
pub enum AckPlan {
    /// Answer with a modal right now (slash commands and edit buttons).
    Modal(ModalSpec),
    /// `DEFERRED_CHANNEL_MESSAGE_WITH_SOURCE`, public — long results arrive as
    /// new channel messages.
    DeferPublic,
    /// `DEFERRED_CHANNEL_MESSAGE_WITH_SOURCE`, ephemeral.
    DeferEphemeral,
    /// `DEFERRED_UPDATE_MESSAGE` — button clicked, content comes later.
    DeferUpdate,
}

/// A modal to present.
#[derive(Debug, Clone)]
pub struct ModalSpec {
    /// The modal `custom_id` (the router recognizes `v1:*` ids only).
    pub custom_id: String,
    /// The modal title.
    pub title: String,
    /// The inputs (≤ 5).
    pub inputs: Vec<ModalInputSpec>,
}

/// One modal input.
#[derive(Debug, Clone)]
pub struct ModalInputSpec {
    /// The input id (must not collide).
    pub id: String,
    /// The displayed label.
    pub label: String,
    /// Short or paragraph.
    pub style: InputStyle,
    /// Prefilled value.
    pub value: Option<String>,
    /// Whether the input is required.
    pub required: bool,
}

/// Modal input styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputStyle {
    /// Single-line.
    Short,
    /// Multi-line.
    Paragraph,
}

/// The guarded routing result the adapter executes.
#[derive(Debug)]
pub enum Guarded {
    /// Non-allowlisted user: ack ephemeral "not authorized".
    Denied,
    /// Foreign scope: ack with nothing.
    Ignored,
    /// Route to the handler (plan decided, work pending).
    Routed(AckPlan),
    /// The plan phase failed; the adapter surfaces an ephemeral error.
    Failed(String),
}
