//! Boot / reconnect reconcile (spec P9): **facts, never the cursor**.
//!
//! Two independent passes:
//! 1. [`reconcile_git_states`] rolls forward the write-ahead intents of
//!    interrupted Accept/Park/Reset sequences (P6/P7) — from DB + git facts,
//!    and *without needing opencode to be up*.
//! 2. The session pass probes the server and, for every `running` action,
//!    finalizes idle ones exactly once (`report_msg_id IS NULL`), keeps watching
//!    busy ones, and leaves everything untouched while the server is down.
//!
//! Plus a third, cheaper pass ([`reconcile_pending_flows`]): a Signal text form
//! that was open when the process died is announced once at boot (D-06).

use std::sync::Arc;

use ocbot_core::State;
use ocbot_core::ports::Activity;

use crate::app::completion::{DetectedBy, finalize_action};
use crate::app::context::AppContext;
use crate::app::gates_loop;
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

    // Gates are server facts that must be mirrored before any completion sweep
    // (P9 step 4): a fresh gate blocks its session from reporting, and a
    // locally-answered gate that is still remote-pending is re-POSTed once per
    // boot (FR-23).
    if let Err(error) = gates_loop::sync(ctx).await {
        tracing::warn!(error = %error, "reconcile: gate sync failed");
    }
    if let Err(error) = gates_loop::repost_answered(ctx).await {
        tracing::warn!(error = %error, "reconcile: gate re-POST failed");
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
            Ok(Activity::Idle) => {
                match finalize_action(ctx, action, false, DetectedBy::Fallback).await {
                    Ok(true) => completed += 1,
                    Ok(false) => {}
                    Err(error) => {
                        tracing::error!(
                            action = action.id,
                            error = %error,
                            "reconcile finalize failed"
                        );
                    }
                }
            }
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

/// Notice every open pending-input flow after a restart (SPEC-004 D-06).
///
/// The draft text survived in SQLite, so the operator is told the flow is
/// still open — reply to continue, or send the verb to restart — instead of
/// typing into a bot that ignores them. The rows are deliberately left in
/// place: resuming a draft is one message, so full resume semantics are not
/// built (the accepted cost in D-06). Surface-blind: each row carries its own
/// destination, so this pass knows nothing about chat platforms.
///
/// Returns the number of flows noticed.
pub async fn reconcile_pending_flows(ctx: &Arc<AppContext>) -> usize {
    let rows = match ctx.store.all_pending().await {
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!(error = %error, "reconcile: pending-flow read failed");
            return 0;
        }
    };
    let mut noticed = 0;
    for row in &rows {
        let Some(surface) = ocbot_core::Surface::parse(&row.surface) else {
            continue;
        };
        let destination = ocbot_core::Destination {
            surface,
            chat_ref: ocbot_core::ChatRef(row.chat_ref.clone()),
        };
        // The wording (and the draft echo) is core vocabulary (PH-7); the pass
        // itself stays surface-blind — it never parses a draft's shape.
        let message = ocbot_core::OutgoingMessage {
            content: ocbot_core::restart_notice(&row.flow, &row.draft),
            attachment: None,
            components: Vec::new(),
            mention_allowlist: Vec::new(),
        };
        if let Err(error) = ctx.chat.post(&destination, &message).await {
            tracing::warn!(
                surface = row.surface,
                error = %error,
                "pending-flow restart notice failed"
            );
            continue;
        }
        noticed += 1;
    }
    if noticed > 0 {
        tracing::info!(noticed, "reconcile announced open pending-input flows");
    }
    noticed
}

/// Roll forward (or conservatively revert) every interrupted Accept/Park/Reset
/// from its write-ahead intent. Returns the number of sequences repaired.
pub async fn reconcile_git_states(ctx: &Arc<AppContext>) -> usize {
    let mut repaired = 0;

    match ctx.store.actions_in_states(&[State::Accepting]).await {
        Ok(actions) => {
            for action in &actions {
                match accept::resume(ctx, action).await {
                    Ok(_) => {
                        repaired += 1;
                        ctx.metrics.inc_reconcile_repair("accept");
                    }
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
                match park::resume(ctx, action, &ctx.guard.operator().0).await {
                    Ok(_) => {
                        repaired += 1;
                        ctx.metrics.inc_reconcile_repair("park");
                    }
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
                match reset::resume(ctx, intent, &ctx.guard.operator().0, None).await {
                    Ok(_) => {
                        repaired += 1;
                        ctx.metrics.inc_reconcile_repair("reset");
                    }
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
