//! The watcher (spec §6.4): polls session liveness for every `running` action
//! and finalizes idle ones. Phase 0 pinned (S-2) that the per-session durable
//! log does not stream in 2.0.24 and the global `/api/event` bus is volatile,
//! so **status polling is the durable signal** — this is the documented
//! fallback, not a workaround. `/api/event` subscription is a Phase-7 latency
//! optimization and changes nothing here.

use std::sync::Arc;
use std::time::Duration;

use ocbot_core::ports::Activity;

use crate::app::completion::{DetectedBy, finalize_action};
use crate::app::context::AppContext;
use crate::app::error::AppError;

/// The watcher loop. `interval` is `config.poll_interval_ms` (S-2 default 2 s;
/// any given poll ≤ interval + one tick to satisfy NFR-2's polling budget).
pub struct Watcher {
    ctx: Arc<AppContext>,
    interval: Duration,
}

impl Watcher {
    /// Build a watcher from the shared context.
    pub fn new(ctx: Arc<AppContext>, interval: Duration) -> Self {
        Self { ctx, interval }
    }

    /// Run until the task is cancelled (signal drain takes the process down).
    pub async fn run_forever(&self) {
        loop {
            if let Err(error) = self.tick().await {
                tracing::error!(error = %error, "watcher tick failed");
            }
            tokio::time::sleep(self.interval).await;
        }
    }

    /// One pass over all running actions: idle ⇒ finalize, busy ⇒ keep
    /// watching, probe failure ⇒ log and keep watching.
    pub async fn tick(&self) -> Result<(), AppError> {
        let actions = self.ctx.store.running_actions().await?;
        for action in &actions {
            let Some(session_id) = action.session_id.clone() else {
                continue;
            };
            match self.ctx.opencode.activity(&session_id).await {
                Ok(Activity::Idle) => {
                    match finalize_action(&self.ctx, action, false, DetectedBy::Fallback).await {
                        Ok(_) => {}
                        Err(error) => {
                            tracing::error!(action = action.id, error = %error, "finalize failed");
                        }
                    }
                }
                Ok(Activity::Busy) => {}
                Err(error) => {
                    // In the polling model (S-2) a failed probe is retried
                    // next tick; it is counted honestly as a probe failure
                    // (D-11: not as a stream reconnect).
                    self.ctx.metrics.inc_probe_failure("watcher");
                    tracing::warn!(session = %session_id, error = %error, "activity probe failed");
                }
            }
        }
        Ok(())
    }
}
