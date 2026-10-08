//! Boot / reconnect reconcile (spec P9): **facts, never the cursor**.
//!
//! Two independent passes:
//! 1. [`reconcile_git_states`] rolls forward the write-ahead intents of
//!    interrupted Accept/Park/Reset sequences (P6/P7) — from DB + git facts,
//!    and *without needing opencode to be up*.
//! 2. The session pass probes the server and, for every `running` action,
//!    finalizes idle ones exactly once (`report_msg_id IS NULL`), keeps watching
//!    busy ones, and leaves everything untouched while the server is down.

use std::sync::Arc;

use ocbot_core::State;
use ocbot_core::ports::Activity;

use crate::app::completion::finalize_action;
use crate::app::context::AppContext;
use crate::app::handlers::{accept, park, reset};

/// What the reconcile pass observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Whether the server answered the version probe.
    pub server_up: bool,
    /// How many completions this pass delivered.
    pub completed: usize,
    /// How many interrupted git sequences this pass rolled forward/reverted.
    pub repaired: usize,
}

/// One full reconcile pass: git-state repair first (server-independent), then
/// the session sweep. Never returns an error for a down server; it reports
/// `server_up: false` and leaves every action untouched (`degraded` mode).
pub async fn reconcile(ctx: &Arc<AppContext>) -> ReconcileReport {
    let repaired = reconcile_git_states(ctx).await;

    match ctx.opencode.probe().await {
        Ok(facts) => {
            ctx.metrics.set_opencode_up(true);
            tracing::info!(version = %facts.version, "opencode reachable");
        }
        Err(error) => {
            ctx.metrics.set_opencode_up(false);
            tracing::warn!(error = %error, "opencode unreachable — degraded; retrying via watcher");
            return ReconcileReport {
                server_up: false,
                completed: 0,
                repaired,
            };
        }
    }

    let actions = match ctx.store.running_actions().await {
        Ok(actions) => actions,
        Err(error) => {
            tracing::error!(error = %error, "reconcile: store read failed");
            return ReconcileReport {
                server_up: true,
                completed: 0,
                repaired,
            };
        }
    };

    let mut completed = 0;
    for action in &actions {
        let Some(session_id) = action.session_id.clone() else {
            continue;
        };
        match ctx.opencode.activity(&session_id).await {
            Ok(Activity::Idle) => match finalize_action(ctx, action).await {
                Ok(true) => completed += 1,
                Ok(false) => {}
                Err(error) => {
                    tracing::error!(
                        action = action.id,
                        error = %error,
                        "reconcile finalize failed"
                    );
                }
            },
            Ok(Activity::Busy) => {
                tracing::debug!(session = %session_id, "resuming watch for busy session");
            }
            Err(error) => {
                tracing::warn!(session = %session_id, error = %error, "activity probe failed");
            }
        }
    }
    ReconcileReport {
        server_up: true,
        completed,
        repaired,
    }
}

/// Roll forward (or conservatively revert) every interrupted Accept/Park/Reset
/// from its write-ahead intent. Returns the number of sequences repaired.
pub async fn reconcile_git_states(ctx: &Arc<AppContext>) -> usize {
    let mut repaired = 0;

    match ctx.store.actions_in_states(&[State::Accepting]).await {
        Ok(actions) => {
            for action in &actions {
                match accept::resume(ctx, action).await {
                    Ok(_) => repaired += 1,
                    Err(error) => {
                        tracing::warn!(action = action.id, error = %error, "accept resume failed");
                    }
                }
            }
        }
        Err(error) => tracing::error!(error = %error, "reconcile: accepting read failed"),
    }

    match ctx.store.actions_in_states(&[State::Parking]).await {
        Ok(actions) => {
            for action in &actions {
                match park::resume(ctx, action, ctx.guard.allowed_user_id).await {
                    Ok(_) => repaired += 1,
                    Err(error) => {
                        tracing::warn!(action = action.id, error = %error, "park resume failed");
                    }
                }
            }
        }
        Err(error) => tracing::error!(error = %error, "reconcile: parking read failed"),
    }

    match ctx.store.pending_reset_intents().await {
        Ok(intents) => {
            for intent in &intents {
                match reset::resume(ctx, intent, ctx.guard.allowed_user_id, None).await {
                    Ok(_) => repaired += 1,
                    Err(error) => {
                        tracing::warn!(intent = intent.id, error = %error, "reset resume failed");
                    }
                }
            }
        }
        Err(error) => tracing::error!(error = %error, "reconcile: reset-intent read failed"),
    }

    if repaired > 0 {
        tracing::info!(
            repaired,
            "reconcile rolled forward interrupted git sequences"
        );
    }
    repaired
}
