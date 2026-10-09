//! The app context: everything a handler/watcher needs, as injected ports
//! (so tests can substitute fakes freely).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};

use ocbot_core::error::OcError as CoreOcError;
use ocbot_core::ports::{Clock, FsPort, GitPort, OpenCodePort, PermissionRule};
use ocbot_core::{OutgoingMessage, Redactor};
use opencode_v2_client::EventFrame;

use crate::adapters::store::StoreHandle;
use crate::app::degraded::HealthMonitor;
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

/// One item handed from an event-source pump to its consumer (SPEC-003 §4.6:
/// the supervisor and dispatcher communicate over a bounded channel; the pump
/// never applies backpressure to the read loop).
#[derive(Debug, Clone)]
pub enum StreamItem {
    /// A typed tolerant event frame (P-32: kind and session key only).
    Frame(EventFrame),
    /// The connection ended (server closed or transport error); the consumer
    /// reconnects with backoff. The two causes are not distinguished here —
    /// both are `Backoff` inputs in the §4.4 machine.
    Closed,
}

/// The bound capacity of an event-source pump channel (§4.6).
pub const FRAME_CHANNEL_CAPACITY: usize = 1024;

/// One live connection to the event stream. The pump inside the
/// [`EventSource`] feeds it; the consumer owns the lifecycle via [`stop`](Self::stop).
pub struct StreamHandle {
    rx: mpsc::Receiver<StreamItem>,
    stop_tx: watch::Sender<bool>,
    stop_rx: watch::Receiver<bool>,
    decode_failures: Arc<AtomicU64>,
    dropped_frames: Arc<AtomicU64>,
}

impl StreamHandle {
    /// Build a handle over a pump's channel side.
    pub fn new(
        rx: mpsc::Receiver<StreamItem>,
        stop_tx: watch::Sender<bool>,
        stop_rx: watch::Receiver<bool>,
        decode_failures: Arc<AtomicU64>,
        dropped_frames: Arc<AtomicU64>,
    ) -> Self {
        Self {
            rx,
            stop_tx,
            stop_rx,
            decode_failures,
            dropped_frames,
        }
    }

    /// Await the next item. `None` means the pump is gone (stopped or the
    /// connection was replaced); the consumer reconnects either way.
    pub async fn next_item(&mut self) -> Option<StreamItem> {
        tokio::select! {
            item = self.rx.recv() => item,
            _ = self.stop_rx.changed() => None,
        }
    }

    /// Undecodable frames observed so far on this connection (P-24); the
    /// consumer reconnects with `reason=drift` past
    /// [`DECODE_BURST_RECONNECT`](opencode_v2_client::DECODE_BURST_RECONNECT)
    /// within its window.
    pub fn decode_failures(&self) -> u64 {
        self.decode_failures.load(Ordering::Relaxed)
    }

    /// Frames dropped because the channel was full (§4.6): safe by P-19,
    /// ticks are their deadline.
    pub fn dropped_frames(&self) -> u64 {
        self.dropped_frames.load(Ordering::Relaxed)
    }

    /// Stop the pump and close the stream.
    pub fn stop(&mut self) {
        let _ = self.stop_tx.send(true);
    }
}

/// The event-stream transport port (SPEC-003 P-21): one connection at a time,
/// owned by the supervisor through [`StreamHandle`]s. The production
/// implementation wraps the client in `adapters::opencode`; tests fake it.
#[async_trait]
pub trait EventSource: Send + Sync {
    /// Open one connection to `/api/event`.
    async fn connect(&self) -> Result<StreamHandle, CoreOcError>;
}

/// The shared application context.
pub struct AppContext {
    /// The SQLite actor handle.
    pub store: StoreHandle,
    /// The opencode port (adapter in prod, fake in tests).
    pub opencode: Arc<dyn OpenCodePort>,
    /// The git port (adapter in prod, fake in tests).
    pub git: Arc<dyn GitPort>,
    /// The filesystem port (browse containment + attachments, P11/FR-34..36).
    pub fs: Arc<dyn FsPort>,
    /// The injected time source (P4: the re-ping policy is fake-clock tested).
    pub clock: Arc<dyn Clock>,
    /// How long `/abort` waits for the server to confirm idle before giving up
    /// (FR-10).
    pub abort_timeout_secs: u64,
    /// The permission baseline, attached to created sessions only when FR-42
    /// is enabled (D-R6: off by default).
    pub permission_ruleset: Option<Vec<PermissionRule>>,
    /// The Discord message surface.
    pub discord: Arc<dyn DiscordPort>,
    /// Metrics registry.
    pub metrics: Arc<Metrics>,
    /// Inline-report secret scrubber (R-12).
    pub redactor: Arc<Redactor>,
    /// opencode reachability transition state (FR-38).
    pub health: Arc<HealthMonitor>,
    /// The single chokepoint (P1/AC-1).
    pub guard: Guard,
    /// Load-time validated prompt templates.
    pub templates: Templates,
    /// Plan-picker heading depth range (P12).
    pub plan_heading_min: u32,
    /// Plan-picker heading depth range (P12).
    pub plan_heading_max: u32,
    /// Browser entries per page (P10).
    pub ls_page_size: usize,
}
