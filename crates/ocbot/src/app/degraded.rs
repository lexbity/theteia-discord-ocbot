//! Degraded mode (spec FR-38): when the opencode server is unreachable, post
//! **once**; when it recovers, post **once**; never spam on repeated polls.
//!
//! The transition state lives in [`HealthMonitor`] (one per process). A fresh
//! process starts `Unknown`: an initial healthy probe says nothing, while an
//! initial failed probe announces the outage (the operator should know the bot
//! came up blind). Every subsequent change of state is announced exactly once.

use std::sync::Arc;
use std::sync::Mutex;

use ocbot_core::OutgoingMessage;

use crate::app::context::AppContext;
use crate::app::error::AppError;

/// The process-wide reachability transition detector.
#[derive(Debug, Default)]
pub struct HealthMonitor {
    last: Mutex<Option<bool>>,
}

impl HealthMonitor {
    /// A fresh monitor (`Unknown`).
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a probe result; returns `Some(up)` exactly when a *transition*
    /// worth announcing happened.
    pub fn observe(&self, up: bool) -> Option<bool> {
        let mut last = self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let previous = *last;
        *last = Some(up);
        match (previous, up) {
            // The process came up already down, or a healthy service went down:
            // announce the outage once.
            (None | Some(true), false) => Some(false),
            // A down → healthy recovery.
            (Some(false), true) => Some(true),
            // Initial healthy, and any same-state repeat: silent.
            _ => None,
        }
    }
}

/// Broadcast one transition announcement to every configured surface
/// (SPEC-004 PH-2: opencode-health announcements ride the broadcast path).
/// The shared edge-triggered post door (FR-38's reachability and SPEC-003
/// P-28's stream down/recovery both land here — one dedupe pattern).
pub async fn announce(ctx: &Arc<AppContext>, up: bool, down_message: String, up_message: String) {
    let content = if up { up_message } else { down_message };
    let message = OutgoingMessage {
        content,
        attachment: None,
        components: Vec::new(),
        mention_allowlist: ctx.guard.mention_allowlist(),
    };
    let outcome = ctx.chat.broadcast(&message).await;
    if !outcome.any_delivered() {
        // The transition is still remembered by the caller's monitor, so a
        // later recovery does not re-announce.
        tracing::warn!(up, "health transition reached no surface");
    }
}

/// Update the reachability gauge and, on a transition, post a single message to
/// the ops/inbox channel. Returns whether a transition was announced.
pub async fn record(ctx: &Arc<AppContext>, up: bool) -> Result<bool, AppError> {
    ctx.metrics.set_opencode_up(up);
    if ctx.health.observe(up).is_none() {
        return Ok(false);
    }
    announce(
        ctx,
        up,
        "⚠️ opencode is unreachable — new sends are paused until it returns (FR-38)".to_owned(),
        "✅ opencode is reachable again".to_owned(),
    )
    .await;
    tracing::info!(up, "opencode reachability transition announced");
    Ok(true)
}

/// Probe the server and record the result (the ops loop's hop).
pub async fn probe_and_record(ctx: &Arc<AppContext>) -> Result<bool, AppError> {
    let up = ctx.opencode.probe().await.is_ok();
    record(ctx, up).await
}
