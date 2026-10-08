//! The completion path shared by the watcher and boot reconcile (spec §6.4:
//! on Idle, classify → footer → render → post → single txn marking the report
//! delivered). Exactly-once is enforced by the `report_msg_id IS NULL` CAS
//! combined with `deliver_report` — the crash gap between the Discord POST and
//! this txn can only ever duplicate a report, never lose one (P9 / NFR-3).

use std::path::Path;
use std::sync::Arc;

use ocbot_core::State;
use ocbot_core::outcome::{self, AssistantSnapshot, Outcome, classify};
use ocbot_core::render::{Footer, ReportContext, report};

use crate::adapters::now_unix;
use crate::adapters::store::ActionRow;
use crate::app::context::AppContext;
use crate::app::error::AppError;

/// Attempt to finalize a running action whose session went idle.
///
/// `abort_requested` forces the `Aborted` classification (FR-10: the abort path
/// finalizes only after the server confirms idle). Returns `true` when a report
/// was delivered, `false` when the action is blocked by a pending gate (and
/// stays `running`).
pub async fn finalize_action(
    ctx: &Arc<AppContext>,
    action: &ActionRow,
    abort_requested: bool,
) -> Result<bool, AppError> {
    let Some(session_id) = action.session_id.clone() else {
        // No session ever got bound; nothing to do.
        return Ok(false);
    };
    // P9: an already-set delivery marker means a report already went out; the
    // `running` state with a marker is the interrupted crash-window shape and
    // must never produce a second report ("if report_msg_id set ⇒ no post").
    if action.report_msg_id.is_some() {
        return Ok(false);
    }
    if session_is_blocked(ctx, &session_id).await {
        tracing::debug!(session = %session_id, "blocked by a pending gate; no report");
        return Ok(false);
    }

    let snapshot = ctx.opencode.last_assistant(&session_id).await?;
    let classification = classify(snapshot.as_ref(), abort_requested, false);
    let (outcome, target): (Outcome, State) = match classification {
        outcome::Classification::Blocked => return Ok(false),
        outcome::Classification::Outcome(outcome) => match outcome {
            Outcome::Completed { .. } | Outcome::Empty => (outcome, State::Reported),
            Outcome::Errored { .. } => (outcome, State::Errored),
            Outcome::Aborted => (outcome, State::Aborted),
        },
    };
    build_and_deliver(ctx, action, &outcome, target, snapshot.as_ref()).await
}

/// True when the session currently has a pending permission/form gate.
/// A probe failure degrades to "false" (the watcher will retry next tick).
async fn session_is_blocked(ctx: &Arc<AppContext>, session_id: &str) -> bool {
    match ctx.opencode.pending_gates().await {
        Ok(gates) => gates
            .iter()
            .any(|gate| gate.session_id.as_deref() == Some(session_id)),
        Err(error) => {
            tracing::warn!(session = %session_id, error = %error, "gate probe failed");
            false
        }
    }
}

/// Render, post, and commit the report. Discord-first, txn-second (P9): a
/// crash between them loses nothing — the next reconcile re-posts, and the
/// CAS rejects any duplicate.
async fn build_and_deliver(
    ctx: &Arc<AppContext>,
    action: &ActionRow,
    outcome: &Outcome,
    target: State,
    snapshot: Option<&AssistantSnapshot>,
) -> Result<bool, AppError> {
    let footer = build_footer(ctx, action, snapshot).await;
    let context = ReportContext {
        action_id: action.id,
        is_git: action_is_git(ctx, action).await,
        operator_id: ctx.guard.allowed_user_id,
        followups: action.followups,
    };
    let message = report(&action.label, outcome, &footer, &context, &ctx.redactor);
    let channel = action.channel_id;

    let message_id = post_report(ctx, channel, &message).await?;

    let outcome_kind = match outcome {
        Outcome::Completed { .. } => "completed",
        Outcome::Errored { .. } => "errored",
        Outcome::Aborted => "aborted",
        Outcome::Empty => "empty",
    };
    let full_body = report_body(outcome);
    let footer_json = serde_json::to_string(&footer)
        .map_err(|error| AppError::Other(format!("footer serialize: {error}")))?;
    let ended_at = now_unix();

    let delivered = ctx
        .store
        .deliver_report(
            action.id,
            target,
            &message_id,
            outcome_kind,
            Some(&full_body),
            &footer_json,
            ended_at,
        )
        .await?;

    if delivered {
        ctx.metrics.inc_reports_delivered();
        ctx.metrics.action_entered(target);
        if let Some(started) = action.started_at {
            ctx.metrics
                .observe_idle_to_report((secs_to_f64(now_unix()) - secs_to_f64(started)).max(0.0));
        }
        if let (Some(origin), Some(channel)) = (action.origin_msg_id.clone(), channel) {
            let _ = ctx
                .discord
                .react(
                    u64::try_from(channel).unwrap_or(u64::MAX),
                    origin.parse().unwrap_or_default(),
                    '✅',
                )
                .await;
        }
        tracing::info!(
            action = action.id,
            outcome = outcome_kind,
            "report delivered"
        );
    } else {
        ctx.metrics.inc_cas_conflict();
        tracing::warn!(
            action = action.id,
            "report already delivered; duplicate suppressed"
        );
    }
    Ok(delivered)
}

/// Post the report where the run originated (P10: reports are new messages).
async fn post_report(
    ctx: &Arc<AppContext>,
    channel: Option<i64>,
    message: &ocbot_core::OutgoingMessage,
) -> Result<String, AppError> {
    match channel {
        Some(channel) => {
            let channel = u64::try_from(channel).unwrap_or(u64::MAX);
            let message_id = ctx.discord.post(channel, message).await?;
            Ok(message_id.to_string())
        }
        None => Err(AppError::Other(
            "action has no channel (invariant)".to_owned(),
        )),
    }
}

fn report_body(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Completed { report } => report.clone(),
        Outcome::Errored { error } => error.clone(),
        Outcome::Aborted | Outcome::Empty => String::new(),
    }
}

/// The ground-truth footer: git diff stat vs the pre-run HEAD, duration,
/// tokens/cost — each degrading independently (A7/S-5 posture).
async fn build_footer(
    ctx: &Arc<AppContext>,
    action: &ActionRow,
    snapshot: Option<&AssistantSnapshot>,
) -> Footer {
    let duration_secs = action
        .started_at
        .map(|started| (secs_to_f64(now_unix()) - secs_to_f64(started)).max(0.0));
    let diff_stat = match action.start_head.clone() {
        Some(start_head) => poll_diff(ctx, action.run_id, &start_head).await,
        None => None,
    };
    Footer {
        diff_stat,
        duration_secs,
        tokens: snapshot.and_then(|snapshot| snapshot.tokens),
        cost: snapshot.and_then(|snapshot| snapshot.cost),
    }
}

/// Whether the action's directory is a git project (store-only; drives the
/// FR-15/FR-26 decision buttons without a git subprocess).
async fn action_is_git(ctx: &Arc<AppContext>, action: &ActionRow) -> bool {
    let Ok(Some(run)) = ctx.store.run(action.run_id).await else {
        return false;
    };
    ctx.store
        .project(&run.dir)
        .await
        .ok()
        .flatten()
        .is_some_and(|project| project.work_branch.is_some())
}

/// Best-effort `git diff --stat` vs a base sha (R-9: the adapter timeout
/// degrades to "n/a").
async fn poll_diff(ctx: &Arc<AppContext>, run_id: i64, base: &str) -> Option<String> {
    let run = ctx.store.run(run_id).await.ok().flatten()?;
    let directory = Path::new(&run.dir);
    if !ctx.git.is_repo(directory).await.ok()? {
        return None;
    }
    match ctx.git.diff_stat(directory, base).await {
        Ok(stat) if !stat.is_empty() => Some(stat),
        _ => None,
    }
}

/// Seconds as f64 without an `as` cast (bounded to u32 → `f64::from`).
fn secs_to_f64(seconds: i64) -> f64 {
    f64::from(u32::try_from(seconds.max(0)).unwrap_or(u32::MAX))
}
