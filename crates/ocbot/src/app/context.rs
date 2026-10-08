//! The app context: everything a handler/watcher needs, as injected ports
//! (so tests can substitute fakes freely).

use std::sync::Arc;

use async_trait::async_trait;

use ocbot_core::OutgoingMessage;
use ocbot_core::ports::{GitPort, OpenCodePort};

use crate::adapters::store::StoreHandle;
use crate::app::guard::Guard;
use crate::app::metrics::Metrics;
use crate::config::Templates;

/// A failure posting to Discord.
#[derive(Debug, Clone, thiserror::Error)]
#[error("discord: {0}")]
pub struct SendError(pub String);

/// The message-posting surface handlers use. The serenity adapter is the only
/// production implementation; the e2e suite uses a recording fake. Every
/// payload built from `&OutgoingMessage` carries the structural
/// `allowed_mentions` restriction (FR-3) — this trait is the only door.
#[async_trait]
pub trait DiscordPort: Send + Sync {
    /// Post a new message; returns the Discord message id.
    async fn post(&self, channel: u64, message: &OutgoingMessage) -> Result<u64, SendError>;
    /// Replace a message's content in place (edits do not notify — used for
    /// quiet status, not for reports).
    async fn edit(
        &self,
        channel: u64,
        message: u64,
        replacement: &OutgoingMessage,
    ) -> Result<(), SendError>;
    /// Add a reaction (the quiet status light, P10).
    async fn react(&self, channel: u64, message: u64, emoji: char) -> Result<(), SendError>;
}

/// The shared application context.
pub struct AppContext {
    /// The SQLite actor handle.
    pub store: StoreHandle,
    /// The opencode port (adapter in prod, fake in tests).
    pub opencode: Arc<dyn OpenCodePort>,
    /// The git port (adapter in prod, fake in tests).
    pub git: Arc<dyn GitPort>,
    /// The Discord message surface.
    pub discord: Arc<dyn DiscordPort>,
    /// Metrics registry.
    pub metrics: Arc<Metrics>,
    /// The single chokepoint (P1/AC-1).
    pub guard: Guard,
    /// Load-time validated prompt templates.
    pub templates: Templates,
}
