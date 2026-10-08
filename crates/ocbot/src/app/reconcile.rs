//! Boot / reconnect reconcile (spec P9 steps 1–3): **status facts, never the
//! cursor**. For every action in `running`: if its session is idle and
//! `report_msg_id` is NULL ⇒ run the completion path; if set ⇒ already
//! delivered; if busy ⇒ keep watching. Inbox/`session_seen` bookkeeping
//! (P9 step 6) and write-git roll-forward (step 5) land in Phases 5 and 3
//! respectively — the states those concern cannot exist yet.

use std::sync::Arc;

use ocbot_core::ports::Activity;

use crate::app::completion::finalize_action;
use crate::app::context::AppContext;

/// What the reconcile pass observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Whether the server answered the version probe.
    pub server_up: bool,
    /// How many completions this pass delivered.
    pub completed: usize,
}

/// One reconcile pass. Never returns an error for a down server; it reports
/// `server_up: false` and leaves every action untouched (`degraded` mode).
pub async fn reconcile(ctx: &Arc<AppContext>) -> ReconcileReport {
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
    }
}
