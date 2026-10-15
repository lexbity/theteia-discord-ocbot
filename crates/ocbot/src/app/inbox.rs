//! The inbox (P14 / FR-32 / FR-33 / S-7): polls *external* root sessions (TUI
//! work the bot did not start), mirrors their liveness into the directory
//! mutex (`held(external:<id>)`, P5), and posts completions to the inbox
//! channel — deduped by `(session_id, seq)` through the `notified` table.
//! Replying to an inbox report adopts the session (P14).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use ocbot_core::OutgoingMessage;
use ocbot_core::outcome::{Classification, classify};
use ocbot_core::ports::Activity;
use ocbot_core::render::{fence_neutralize, headline, split_excerpt};

use crate::app::completion::DetectedBy;
use crate::app::context::AppContext;
use crate::app::error::AppError;

/// The inline excerpt budget for inbox reports (P10).
const INBOX_EXCERPT: usize = 1_500;

/// The poll-and-mirror loop task (`inbox_poll_interval_ms`, S-7).
pub struct Inbox {
    ctx: Arc<AppContext>,
    interval: Duration,
}

impl Inbox {
    /// Build the loop from the shared context.
    pub fn new(ctx: Arc<AppContext>, interval: Duration) -> Self {
        Self { ctx, interval }
    }

    /// Run until cancelled.
    pub async fn run_forever(&self) {
        loop {
            if let Err(error) = tick(&self.ctx, DetectedBy::Fallback).await {
                tracing::error!(error = %error, "inbox tick failed");
            }
            tokio::time::sleep(self.interval).await;
        }
    }
}

/// One poll pass over external root sessions. The loop is a thin timer around
/// [`diff_sessions`]: the tick passes [`DetectedBy::Fallback`], the event
/// dispatcher passes [`DetectedBy::Event`] (SPEC-003 W4 — one body, two
/// schedulers).
pub async fn tick(ctx: &Arc<AppContext>, detected_by: DetectedBy) -> Result<(), AppError> {
    diff_sessions(ctx, detected_by).await
}

/// The shared session-list diff: one `list_sessions(root_only)` read, deduped
/// completion posts per external root, external lock sync. `detected_by` is
/// attributed into every completion this pass delivers (P-29).
pub async fn diff_sessions(ctx: &Arc<AppContext>, detected_by: DetectedBy) -> Result<(), AppError> {
    let roots = ctx
        .opencode
        .list_sessions(true)
        .await
        .map_err(AppError::OpenCode)?;
    let bot_sessions: HashSet<String> = ctx.store.known_bot_sessions().await?.into_iter().collect();

    for session in &roots {
        if bot_sessions.contains(&session.id) {
            continue;
        }
        tick_external(ctx, session, detected_by).await?;
    }
    Ok(())
}

/// Mirror one external root session: seen-record, external lock sync, and the
/// deduped completion post.
async fn tick_external(
    ctx: &Arc<AppContext>,
    session: &ocbot_core::ports::PortSession,
    detected_by: DetectedBy,
) -> Result<(), AppError> {
    let dir = session.directory.as_deref();
    ctx.store.upsert_seen(&session.id, dir).await?;

    match ctx.opencode.activity(&session.id).await {
        Ok(Activity::Busy) => {
            // P5: an external session busy in a directory holds it. Never
            // clobber a bot action's lock (P5 owned lock).
            if let Some(dir) = dir {
                let external = format!("external:{}", session.id);
                let holder = ctx.store.lock_holder(dir).await?;
                if holder.is_none_or(|holder| holder == external) {
                    ctx.store
                        .set_external_lock(dir, &session.id, ctx.clock.now())
                        .await?;
                }
            }
        }
        Ok(Activity::Idle) => {
            if let Some(dir) = dir {
                ctx.store
                    .release_lock(dir, &format!("external:{}", session.id))
                    .await?;
            }
            if is_muted(ctx, &session.id, dir).await? {
                return Ok(());
            }
            deliver_completion(ctx, session, dir, detected_by).await?;
        }
        Err(error) => {
            tracing::warn!(session = %session.id, error = %error, "inbox activity probe failed");
        }
    }
    Ok(())
}

/// Post an external completion, deduped by the final assistant message id
/// (the `(session_id, seq)` key). Discord-first, `notified`-second: a crash
/// between them can duplicate but never lose a completion (P9 posture).
async fn deliver_completion(
    ctx: &Arc<AppContext>,
    session: &ocbot_core::ports::PortSession,
    dir: Option<&str>,
    detected_by: DetectedBy,
) -> Result<(), AppError> {
    let Some(snapshot) = ctx.opencode.last_assistant(&session.id).await? else {
        return Ok(()); // freshly created / no assistant message yet
    };
    let Some(seq) = snapshot.id.as_deref() else {
        return Ok(()); // no stable message id to dedupe on
    };

    let message = inbox_message(ctx, session, dir, &snapshot);
    // Claim-then-post: the claim is the dedupe key, so duplicate completions
    // never post a second message (FR-32/P14). A transient post failure is
    // unclaimed so the next tick retries.
    if !ctx.store.claim_notified(&session.id, seq).await? {
        tracing::debug!(session = %session.id, seq, "completion already notified");
        return Ok(());
    }
    // A delivered completion is a won detection (P-29): attributed here, after
    // the dedupe, so the counter is the honest event/fallback numerator.
    ctx.metrics.inc_detection(detected_by.label());
    // Announce to every configured surface (SPEC-004 §5.4): the report is not
    // tied to the channel that started the work. Adopt works from whichever
    // surface carried the message (one `msg_map` row per receipt).
    let outcome = ctx.chat.broadcast(&message).await;
    if outcome.any_delivered() {
        let receipts = outcome
            .delivered
            .iter()
            .map(|(_, receipt)| receipt.to_receipt_string())
            .collect::<Vec<_>>()
            .join("|");
        ctx.store
            .finish_notified(&session.id, seq, &receipts)
            .await?;
        for (surface, receipt) in &outcome.delivered {
            ctx.store
                .insert_msg_map(surface.as_str(), &receipt.msg_id.0, &session.id, None)
                .await?;
        }
        tracing::info!(session = %session.id, seq, "inbox completion posted");
    } else {
        ctx.store.unclaim_notified(&session.id, seq).await?;
        for (surface, error) in &outcome.failed {
            tracing::warn!(session = %session.id, %surface, error = %error, "inbox post failed; retrying next tick");
        }
    }
    Ok(())
}

/// Build the inbox report: outcome headline + excerpt + the reply-to-adopt
/// hint (P14).
fn inbox_message(
    ctx: &Arc<AppContext>,
    session: &ocbot_core::ports::PortSession,
    dir: Option<&str>,
    snapshot: &ocbot_core::AssistantSnapshot,
) -> OutgoingMessage {
    let label = dir.map_or_else(|| session.id.clone(), basename);
    let outcome = match classify(Some(snapshot), false, false) {
        Classification::Blocked => ocbot_core::outcome::Outcome::Empty,
        Classification::Outcome(outcome) => outcome,
    };
    let body = match &outcome {
        ocbot_core::outcome::Outcome::Completed { report } => report.clone(),
        ocbot_core::outcome::Outcome::Errored { error } => error.clone(),
        ocbot_core::outcome::Outcome::Aborted | ocbot_core::outcome::Outcome::Empty => {
            String::new()
        }
    };
    let (excerpt, _) = split_excerpt(&fence_neutralize(&body), INBOX_EXCERPT);
    OutgoingMessage {
        content: format!(
            "{} *(external — reply to continue)*\n```\n{excerpt}\n```",
            headline(&outcome, &label)
        ),
        attachment: None,
        components: Vec::new(),
        mention_allowlist: ctx.guard.mention_allowlist(),
    }
}

/// Whether the directory or the session is muted (FR-33).
async fn is_muted(
    ctx: &Arc<AppContext>,
    session_id: &str,
    dir: Option<&str>,
) -> Result<bool, AppError> {
    if let Some(dir) = dir
        && ctx.store.is_muted("dir", dir).await?
    {
        return Ok(true);
    }
    ctx.store
        .is_muted("session", session_id)
        .await
        .map_err(AppError::from)
}

fn basename(path: &str) -> String {
    path.rsplit('/')
        .next()
        .unwrap_or(path)
        .chars()
        .take(40)
        .collect()
}
