//! `/status` (spec FR-37, Phase 2 slice: minus pending gates, which land in
//! Phase 4): opencode reachability + version, active actions, held locks.
//! A long result → new channel message (P10).

use std::fmt::Write as _;
use std::sync::Arc;

use ocbot_core::OutgoingMessage;

use crate::adapters::now_unix;
use crate::app::context::AppContext;
use crate::app::error::AppError;
use crate::app::interaction::InteractionSource;

/// Execute phase for `/status`: build and post the status digest.
pub async fn execute(ctx: &Arc<AppContext>, source: &InteractionSource) -> Result<(), AppError> {
    let probe = match ctx.opencode.probe().await {
        Ok(facts) => {
            ctx.metrics.set_opencode_up(true);
            Some(facts)
        }
        Err(error) => {
            ctx.metrics.set_opencode_up(false);
            tracing::warn!(error = %error, "status probe failed");
            None
        }
    };

    let running = ctx.store.running_actions().await?;
    let locks = ctx.store.locked_projects().await?;
    let pending_gates = ctx.store.pending_gates().await?;
    ctx.metrics
        .set_locks_held(u64::try_from(locks.len()).unwrap_or(u64::MAX));
    ctx.metrics.set_gate_pending(pending_gates.len());

    let now = now_unix();
    let mut content = String::from("**ocbot status**\n");
    match &probe {
        Some(facts) => {
            let _ = writeln!(content, "server: up (version {})", facts.version);
        }
        None => content.push_str("server: unreachable\n"),
    }

    let _ = writeln!(content, "running actions: {}", running.len());
    for action in &running {
        let age = action
            .started_at
            .map_or_else(|| "?".to_owned(), |started| format!("{}s", now - started));
        let _ = writeln!(
            content,
            "  · `#{}` {} — session {} — {age}",
            action.id,
            action.label,
            action.session_id.as_deref().unwrap_or("?"),
        );
    }

    let _ = writeln!(content, "held locks: {}", locks.len());
    for lock in &locks {
        let _ = writeln!(
            content,
            "  · {} (holder {}, since {})",
            crate::app::handlers::support::display_name(&lock.dir),
            lock.lock_holder.as_deref().unwrap_or("?"),
            lock.lock_since.unwrap_or_default(),
        );
    }

    let _ = writeln!(content, "pending gates: {}", pending_gates.len());
    for gate in &pending_gates {
        let _ = writeln!(
            content,
            "  · #{} {} — session {}",
            gate.id,
            gate.kind,
            gate.session_id.as_deref().unwrap_or("?"),
        );
    }

    let message = OutgoingMessage {
        content,
        attachment: None,
        components: Vec::new(),
        mention_allowlist: ocbot_core::mention_allowlist(&source.user_id),
    };
    ctx.chat.post(&source.destination, &message).await?;
    Ok(())
}
