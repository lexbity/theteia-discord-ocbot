//! `/status` (spec FR-37, Phase 2 slice: minus pending gates, which land in
//! Phase 4): opencode reachability + version, active actions, held locks.
//! A long result → new channel message (P10).

use std::fmt::Write as _;
use std::sync::Arc;

use ocbot_core::OutgoingMessage;

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
    ctx.metrics
        .set_locks_held(u64::try_from(locks.len()).unwrap_or(u64::MAX));

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
            lock.dir,
            lock.lock_holder.as_deref().unwrap_or("?"),
            lock.lock_since.unwrap_or_default(),
        );
    }

    let message = OutgoingMessage {
        content,
        attachment: None,
        components: Vec::new(),
        mentioned_users: vec![source.user_id],
    };
    ctx.discord.post(source.channel_id, &message).await?;
    Ok(())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}
