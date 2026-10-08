//! The guarded router (spec §6.6): parses the `custom_id`/command grammar,
//! owns the guard (P1/AC-1), and separates the **plan** (an immediate ack:
//! modal or defer, decided without slow work) from the **execute** pass (side
//! effects: session creation, git ops, message posts).

use std::sync::Arc;

use ocbot_core::State;

use crate::adapters::store::ActionRow;
use crate::app::context::AppContext;
use crate::app::error::AppError;
use crate::app::guard::Decision;
use crate::app::handlers;
use crate::app::interaction::{AckPlan, InteractionSource, SourceKind};

/// The result of the guarded plan phase.
#[derive(Debug)]
pub enum PlanOutcome {
    /// Non-allowlisted user (FR-1).
    Deny,
    /// Foreign channel / scope (FR-2).
    Ignore,
    /// Ack plan to send immediately.
    Plan(AckPlan),
    /// The plan phase failed; surface an ephemeral error.
    Failed(String),
}

/// Guard + ack plan. This is the adapter's first phase; the ack is sent from
/// the returned [`AckPlan`] before any handler work runs.
pub async fn guarded_plan(ctx: &Arc<AppContext>, source: &InteractionSource) -> PlanOutcome {
    match ctx.guard.decide(source) {
        Decision::EphemeralDeny => {
            ctx.metrics.inc_denied();
            PlanOutcome::Deny
        }
        Decision::Ignore => PlanOutcome::Ignore,
        Decision::Allow => match plan(ctx, source).await {
            Ok(plan) => PlanOutcome::Plan(plan),
            Err(AppError::AlreadyHandled) => PlanOutcome::Plan(AckPlan::DeferEphemeral),
            Err(error) => {
                tracing::warn!(error = %error, "plan phase failed");
                PlanOutcome::Failed(error.to_string())
            }
        },
    }
}

/// The ack plan for a guarded (allow) interaction. Cheap by construction: the
/// only reads are single SQLite keys (modal prefills / state preflight); no
/// port call happens here (AC-1 leans on this).
async fn plan(ctx: &Arc<AppContext>, source: &InteractionSource) -> Result<AckPlan, AppError> {
    match &source.kind {
        SourceKind::SlashCommand { name } => match name.as_str() {
            "run" => Ok(AckPlan::Modal(
                handlers::run::begin_modal(ctx, source).await?,
            )),
            "status" | "history" | "reset" | "release" => Ok(AckPlan::DeferPublic),
            other => Err(AppError::UnknownCommand(other.to_owned())),
        },
        SourceKind::Component { custom_id } => {
            if let Some(id) = custom_id.strip_prefix("v1:send:") {
                require_staged(ctx, parse_action_id(id)?).await?;
                Ok(AckPlan::DeferUpdate)
            } else if let Some(id) = custom_id.strip_prefix("v1:runany:") {
                require_staged(ctx, parse_action_id(id)?).await?;
                Ok(AckPlan::DeferUpdate)
            } else if let Some(id) = custom_id.strip_prefix("v1:edit:") {
                Ok(AckPlan::Modal(
                    handlers::run::edit_modal(ctx, parse_action_id(id)?).await?,
                ))
            } else if let Some(id) = custom_id.strip_prefix("v1:accept:") {
                let action = require_state(ctx, parse_action_id(id)?, &[State::Reported]).await?;
                let dir = handlers::run::run_directory(ctx, action.run_id).await?;
                let is_git = handlers::support::is_git_project(ctx, &dir.to_string_lossy()).await?;
                if is_git {
                    Ok(AckPlan::Modal(handlers::accept::accept_modal(&action)))
                } else {
                    // Non-git Accept = Mark done (FR-26): no modal, DB-only.
                    Ok(AckPlan::DeferUpdate)
                }
            } else if let Some(id) = custom_id.strip_prefix("v1:park:") {
                require_state(
                    ctx,
                    parse_action_id(id)?,
                    &[State::Reported, State::Errored, State::Aborted],
                )
                .await?;
                Ok(AckPlan::DeferUpdate)
            } else if let Some(id) = custom_id.strip_prefix("v1:discard:") {
                require_state(
                    ctx,
                    parse_action_id(id)?,
                    &[State::Reported, State::Errored, State::Aborted],
                )
                .await?;
                Ok(AckPlan::DeferUpdate)
            } else if is_deferred_action(custom_id) {
                Ok(AckPlan::DeferUpdate)
            } else {
                Err(AppError::UnknownCustomId(custom_id.to_owned()))
            }
        }
        SourceKind::ModalSubmit { custom_id, .. } => {
            if custom_id == handlers::run::NEW_RUN_MODAL {
                Ok(AckPlan::DeferPublic)
            } else if custom_id.starts_with("v1:editgo:") || custom_id.starts_with("v1:acceptgo:") {
                Ok(AckPlan::DeferUpdate)
            } else {
                Err(AppError::UnknownCustomId(custom_id.to_owned()))
            }
        }
    }
}

/// Custom ids whose plan is a plain deferral (all work is in the execute pass).
fn is_deferred_action(custom_id: &str) -> bool {
    ["v1:reset:", "v1:resetgo:", "v1:releaseq:", "v1:release:"]
        .iter()
        .any(|prefix| custom_id.starts_with(prefix))
        || custom_id.starts_with("v1:page:history:")
}

/// The execute pass of a deferred interaction; returns an optional ephemeral
/// followup text (which the adapter posts after the work finishes).
pub async fn execute(
    ctx: &Arc<AppContext>,
    source: &InteractionSource,
) -> Result<Option<String>, AppError> {
    match &source.kind {
        SourceKind::SlashCommand { name } => match name.as_str() {
            "status" => {
                handlers::status::execute(ctx, source).await?;
                Ok(None)
            }
            "history" => {
                handlers::history::execute(ctx, source, false).await?;
                Ok(None)
            }
            "reset" => {
                handlers::history::execute(ctx, source, true).await?;
                Ok(None)
            }
            "release" => {
                handlers::release::list(ctx, source).await?;
                Ok(None)
            }
            other => Err(AppError::UnknownCommand(other.to_owned())),
        },
        SourceKind::Component { custom_id } => {
            if let Some(id) = custom_id.strip_prefix("v1:send:") {
                handlers::run::execute_send(ctx, parse_action_id(id)?, source).await
            } else if let Some(id) = custom_id.strip_prefix("v1:runany:") {
                handlers::run::execute_runany(ctx, parse_action_id(id)?, source).await
            } else if let Some(id) = custom_id.strip_prefix("v1:accept:") {
                handlers::accept::execute_mark_done(ctx, parse_action_id(id)?, source.user_id).await
            } else if let Some(id) = custom_id.strip_prefix("v1:park:") {
                handlers::park::begin_park(ctx, parse_action_id(id)?, source.user_id).await
            } else if let Some(id) = custom_id.strip_prefix("v1:discard:") {
                handlers::discard::execute_discard(ctx, parse_action_id(id)?, source.user_id).await
            } else if let Some(sha) = custom_id.strip_prefix("v1:reset:") {
                handlers::reset::confirm(ctx, source, sha).await
            } else if let Some(sha) = custom_id.strip_prefix("v1:resetgo:") {
                handlers::reset::execute_reset(ctx, source, sha).await
            } else if let Some(hash) = custom_id.strip_prefix("v1:releaseq:") {
                handlers::release::confirm(ctx, source, hash).await
            } else if let Some(hash) = custom_id.strip_prefix("v1:release:") {
                handlers::release::release(ctx, source, hash).await
            } else if let Some(page) = custom_id.strip_prefix("v1:page:history:") {
                handlers::history::page(ctx, source, parse_page(page)?).await?;
                Ok(None)
            } else {
                Err(AppError::UnknownCustomId(custom_id.to_owned()))
            }
        }
        SourceKind::ModalSubmit { custom_id, .. } => {
            if custom_id == handlers::run::NEW_RUN_MODAL {
                handlers::run::execute_newrun(ctx, source).await
            } else if let Some(id) = custom_id.strip_prefix("v1:editgo:") {
                handlers::run::execute_editgo(ctx, parse_action_id(id)?, source).await
            } else if let Some(id) = custom_id.strip_prefix("v1:acceptgo:") {
                let subject = source.input("subject").unwrap_or_default();
                let body = source.input("body").unwrap_or_default();
                handlers::accept::execute_accept(
                    ctx,
                    parse_action_id(id)?,
                    subject,
                    body,
                    source.user_id,
                )
                .await
            } else {
                Err(AppError::UnknownCustomId(custom_id.to_owned()))
            }
        }
    }
}

fn parse_action_id(raw: &str) -> Result<i64, AppError> {
    raw.trim()
        .parse()
        .map_err(|_| AppError::UnknownCustomId(format!("bad action id {raw:?}")))
}

fn parse_page(raw: &str) -> Result<usize, AppError> {
    raw.trim()
        .parse()
        .map_err(|_| AppError::UnknownCustomId(format!("bad page {raw:?}")))
}

/// Load an action and assert it is in one of `allowed`; a state mismatch is a
/// double-tap/replay and maps to `AlreadyHandled`.
async fn require_state(
    ctx: &Arc<AppContext>,
    action_id: i64,
    allowed: &[State],
) -> Result<ActionRow, AppError> {
    let action = ctx
        .store
        .action(action_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("action {action_id}")))?;
    if !allowed.contains(&action.state) {
        return Err(AppError::AlreadyHandled);
    }
    Ok(action)
}

/// A cheap "is this action still staged?" preflight for buttons, so the ack
/// type is decided before any work (double-taps dedupe here too).
async fn require_staged(ctx: &Arc<AppContext>, action_id: i64) -> Result<(), AppError> {
    let action = ctx
        .store
        .action(action_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("action {action_id}")))?;
    if action.state != State::Staged {
        return Err(AppError::AlreadyHandled);
    }
    Ok(())
}
