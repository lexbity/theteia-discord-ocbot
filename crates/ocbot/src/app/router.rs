//! The guarded router (spec §6.6): parses the `custom_id`/command grammar,
//! owns the guard (P1/AC-1), and separates the **plan** (an immediate ack:
//! modal or defer, decided without slow work) from the **execute** pass (side
//! effects: session creation, git ops, message posts).

use std::sync::Arc;

use ocbot_core::State;

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
/// only reads are the two modal prefills (a single SQLite key each).
async fn plan(ctx: &Arc<AppContext>, source: &InteractionSource) -> Result<AckPlan, AppError> {
    match &source.kind {
        SourceKind::SlashCommand { name } => match name.as_str() {
            "run" => Ok(AckPlan::Modal(
                handlers::run::begin_modal(ctx, source).await?,
            )),
            "status" => Ok(AckPlan::DeferPublic),
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
            } else {
                Err(AppError::UnknownCustomId(custom_id.to_owned()))
            }
        }
        SourceKind::ModalSubmit { custom_id, .. } => {
            if custom_id == handlers::run::NEW_RUN_MODAL {
                Ok(AckPlan::DeferPublic)
            } else if custom_id.starts_with("v1:editgo:") {
                Ok(AckPlan::DeferUpdate)
            } else {
                Err(AppError::UnknownCustomId(custom_id.to_owned()))
            }
        }
    }
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
            other => Err(AppError::UnknownCommand(other.to_owned())),
        },
        SourceKind::Component { custom_id } => {
            if let Some(id) = custom_id.strip_prefix("v1:send:") {
                handlers::run::execute_send(ctx, parse_action_id(id)?, source).await
            } else if let Some(id) = custom_id.strip_prefix("v1:runany:") {
                handlers::run::execute_runany(ctx, parse_action_id(id)?, source).await
            } else {
                Err(AppError::UnknownCustomId(custom_id.to_owned()))
            }
        }
        SourceKind::ModalSubmit { custom_id, .. } => {
            if custom_id == handlers::run::NEW_RUN_MODAL {
                handlers::run::execute_newrun(ctx, source).await
            } else if let Some(id) = custom_id.strip_prefix("v1:editgo:") {
                handlers::run::execute_editgo(ctx, parse_action_id(id)?, source).await
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
