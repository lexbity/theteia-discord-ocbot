//! The periodic operations loop (Phase 7): one task that owns the derived
//! gauges, the reachability announcement (FR-38), and the stall watchdog
//! (P3/FR-39). Keeping them on one cadence avoids three near-identical timers.

use std::sync::Arc;
use std::time::Duration;

use crate::app::context::AppContext;
use crate::app::error::AppError;
use crate::app::{degraded, stall};

/// The ops cadence: health, gauges, and stall checks.
pub struct OpsLoop {
    ctx: Arc<AppContext>,
    interval: Duration,
    stall_secs: i64,
}

impl OpsLoop {
    /// Build the loop from the shared context.
    pub fn new(ctx: Arc<AppContext>, interval: Duration, stall_secs: i64) -> Self {
        Self {
            ctx,
            interval,
            stall_secs,
        }
    }

    /// Run until cancelled.
    pub async fn run_forever(&self) {
        loop {
            if let Err(error) = tick(&self.ctx, self.stall_secs).await {
                tracing::error!(error = %error, "ops tick failed");
            }
            tokio::time::sleep(self.interval).await;
        }
    }
}

/// One ops pass: announce reachability transitions, refresh derived gauges,
/// then run the stall watchdog.
pub async fn tick(ctx: &Arc<AppContext>, stall_secs: i64) -> Result<(), AppError> {
    degraded::probe_and_record(ctx).await?;
    refresh_gauges(ctx).await?;
    sweep_pending(ctx).await?;
    sweep_maps(ctx).await?;
    stall::scan(ctx, stall_secs).await?;
    Ok(())
}

/// Expire pending-input flows (SPEC-004 D-06/FR-109): a flow older than its
/// TTL is closed with one notice on its own surface, and the gauge recounted.
pub async fn sweep_pending(ctx: &Arc<AppContext>) -> Result<(), AppError> {
    let now = ctx.clock.now();
    for row in ctx.store.pending_expired(now).await? {
        let Some(surface) = ocbot_core::Surface::parse(&row.surface) else {
            continue;
        };
        let destination = ocbot_core::Destination {
            surface,
            chat_ref: ocbot_core::ChatRef(row.chat_ref.clone()),
        };
        let message = ocbot_core::OutgoingMessage {
            content: format!(
                "the `{}` flow expired (10 minutes); send a command to start again",
                row.flow
            ),
            attachment: None,
            components: Vec::new(),
            mention_allowlist: Vec::new(),
        };
        if let Err(error) = ctx.chat.post(&destination, &message).await {
            tracing::warn!(error = %error, "pending-flow expiry notice failed");
        }
        let _ = ctx.store.take_pending(&row.surface, &row.chat_ref).await?;
    }
    recount_pending(ctx).await?;
    Ok(())
}

/// Recount the `ocbot_pending_flows_open` gauge by flow name (D-06).
async fn recount_pending(ctx: &Arc<AppContext>) -> Result<(), AppError> {
    let rows = ctx.store.all_pending().await?;
    let mut counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for row in rows {
        *counts.entry(row.flow).or_insert(0) += 1;
    }
    for flow in ["run", "modal", "prompt", "cd"] {
        ctx.metrics
            .set_pending_flows_open(flow, counts.get(flow).copied().unwrap_or(0));
    }
    Ok(())
}

/// Sweep expired `msg_map` and `component_map` rows (7-day retention).
async fn sweep_maps(ctx: &Arc<AppContext>) -> Result<(), AppError> {
    let now = ctx.clock.now();
    ctx.store.gc_maps(now, 7).await?;
    ctx.metrics.set_component_map_rows(
        usize::try_from(ctx.store.component_map_count().await?).unwrap_or(usize::MAX),
    );
    ctx.metrics
        .set_msg_map_rows(usize::try_from(ctx.store.msg_map_count().await?).unwrap_or(usize::MAX));
    Ok(())
}

/// Refresh the gauges that are derived from the store on each tick.
async fn refresh_gauges(ctx: &Arc<AppContext>) -> Result<(), AppError> {
    let now = ctx.clock.now();
    let locks = ctx.store.locked_projects().await?;
    ctx.metrics
        .set_locks_held(u64::try_from(locks.len()).unwrap_or(u64::MAX));
    let oldest = locks
        .iter()
        .filter_map(|project| project.lock_since)
        .map(|since| now.saturating_sub(since))
        .max()
        .unwrap_or(0);
    ctx.metrics
        .set_lock_age_seconds(u64::try_from(oldest.max(0)).unwrap_or(0));

    let running = ctx.store.running_actions().await?;
    let lag = running
        .iter()
        .filter_map(|action| action.last_event_at.or(action.started_at))
        .map(|last| now.saturating_sub(last))
        .max()
        .unwrap_or(0);
    ctx.metrics
        .set_log_lag_seconds(u64::try_from(lag.max(0)).unwrap_or(0));
    Ok(())
}
