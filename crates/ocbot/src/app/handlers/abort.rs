//! `/abort` (FR-10): interrupt the running action's session, then finalize as
//! `Aborted` **only after the server confirms idle**. An already-idle session
//! is treated as immediately aborted (S-8).

use std::sync::Arc;
use std::time::Duration;

use ocbot_core::ports::Activity;

use crate::app::completion::{DetectedBy, finalize_action};
use crate::app::context::AppContext;
use crate::app::error::AppError;
use crate::app::interaction::InteractionSource;

/// How often the idle confirmation polls.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Execute `/abort`: the running action in the operator's current directory.
pub async fn execute(ctx: &Arc<AppContext>, source: &InteractionSource) -> Result<(), AppError> {
    let dir = crate::app::handlers::support::history_dir(ctx, &source.user_id).await?;
    let action = ctx
        .store
        .running_action_for_dir(&dir)
        .await?
        .ok_or_else(|| {
            AppError::Other(format!(
                "no running action in `{}`",
                crate::app::handlers::support::display_name(&dir)
            ))
        })?;
    let session_id = action.session_id.clone().ok_or_else(|| {
        AppError::Other("the running action has no session (invariant)".to_owned())
    })?;

    ctx.opencode.interrupt(&session_id).await?;

    // FR-10: the CAS to `aborted` only after opencode confirms idle. The
    // interrupt is a no-op when already idle (S-8), which is an immediate
    // abort.
    let mut idle = ctx.opencode.activity(&session_id).await? == Activity::Idle;
    if !idle {
        let deadline = Duration::from_secs(ctx.abort_timeout_secs);
        let waited = wait_until_idle(ctx, &session_id, deadline).await;
        idle = waited;
    }
    if !idle {
        return Err(AppError::Other(
            "abort was sent but the session is still busy; leaving the action running".to_owned(),
        ));
    }

    finalize_action(ctx, &action, true, DetectedBy::Fallback).await?;
    ctx.store
        .audit(&source.user_id, "abort", Some(action.id), "aborted")
        .await?;
    Ok(())
}

/// Poll the session until idle or `deadline` elapses.
async fn wait_until_idle(ctx: &Arc<AppContext>, session_id: &str, deadline: Duration) -> bool {
    let started = tokio::time::Instant::now();
    while started.elapsed() < deadline {
        tokio::time::sleep(POLL_INTERVAL).await;
        match ctx.opencode.activity(session_id).await {
            Ok(Activity::Idle) => return true,
            Ok(Activity::Busy) => {}
            Err(error) => {
                tracing::warn!(session = session_id, error = %error, "abort idle probe failed");
            }
        }
    }
    false
}
