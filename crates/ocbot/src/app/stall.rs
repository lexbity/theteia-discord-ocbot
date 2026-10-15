//! Stall watchdog (spec P3/FR-39): surface silence, do not diagnose it.
//!
//! The bot has exactly one watchdog. For every `running` action it compares the
//! injected clock against the last observed event (`last_event_at`, falling
//! back to `started_at`). Once a stall has lasted `STALL_MINUTES` it posts a
//! single plain ping and records `stall_ping_at`. It pings **once per stall**:
//! a subsequent scan is silent until a newer event arrives (activity resets the
//! detector). It never guesses at the cause and never aborts anything.

use std::sync::Arc;

use ocbot_core::OutgoingMessage;

use crate::app::context::AppContext;
use crate::app::error::AppError;

/// One stall scan. Returns how many pings were posted.
pub async fn scan(ctx: &Arc<AppContext>, stall_secs: i64) -> Result<usize, AppError> {
    let now = ctx.clock.now();
    let mut pinged = 0;
    for action in ctx.store.running_actions().await? {
        let last = action.last_event_at.or(action.started_at).unwrap_or(now);
        if now.saturating_sub(last) < stall_secs {
            continue;
        }
        // Already pinged for this stall episode (no newer event since).
        if action
            .stall_ping_at
            .is_some_and(|pinged_at| pinged_at >= last)
        {
            continue;
        }
        // The ping goes to the action's own surface (SPEC-004 §5.4), falling
        // back to the announce channel when the row predates destinations.
        let destination = crate::app::destination::about_destination(ctx, Some(&action));
        let minutes = stall_secs / 60;
        let message = OutgoingMessage {
            content: format!(
                "⏳ **{}** — no activity for {minutes}m. Still running; surfacing the silence, not diagnosing it.",
                action.label
            ),
            attachment: None,
            components: Vec::new(),
            mention_allowlist: ctx.guard.discord_operator().into_iter().collect(),
        };
        ctx.chat.post(&destination, &message).await?;
        ctx.store.set_stall_ping(action.id, now).await?;
        tracing::info!(action = action.id, "stall ping posted");
        pinged += 1;
    }
    Ok(pinged)
}
