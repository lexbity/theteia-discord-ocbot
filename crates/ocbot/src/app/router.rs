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
            "prompt" => Ok(AckPlan::Modal(
                handlers::prompt::prompt_modal(ctx, source).await?,
            )),
            "status" | "history" | "reset" | "release" | "abort" | "mute" => {
                Ok(AckPlan::DeferPublic)
            }
            "projects" => Ok(AckPlan::DeferPublic),
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
            } else if let Some(id) = custom_id.strip_prefix("v1:formgo:") {
                // The "Answer form" button: the plan is a single store read that
                // decides the modal (a gone/answered gate is a no-op).
                match handlers::gate::form_modal(ctx, parse_action_id(id)?).await? {
                    Some(spec) => Ok(AckPlan::Modal(spec)),
                    None => Ok(AckPlan::DeferEphemeral),
                }
            } else if let Some(rest) = custom_id.strip_prefix("v1:gate:") {
                let _ = split_gate_id(rest)?;
                Ok(AckPlan::DeferUpdate)
            } else if let Some(id) = custom_id.strip_prefix("v1:followup:") {
                require_state(
                    ctx,
                    parse_action_id(id)?,
                    &[State::Reported, State::Errored, State::Aborted],
                )
                .await?;
                Ok(AckPlan::Modal(
                    handlers::followup::followup_modal(ctx, parse_action_id(id)?).await?,
                ))
            } else if let Some(id) = custom_id.strip_prefix("v1:planother:") {
                Ok(AckPlan::Modal(handlers::run::plan_other_modal(
                    parse_action_id(id)?,
                )))
            } else if let Some(project) = custom_id.strip_prefix("v1:run_noplan:") {
                // Run Without Plan needs a label the component cannot carry
                // (SPEC-005 flow 7): the plan is the label modal. The token is
                // checked for shape only, so the plan phase stays I/O-free.
                Ok(AckPlan::Modal(handlers::projects::run_noplan_modal(
                    project,
                )?))
            } else if is_deferred_action(custom_id) {
                Ok(AckPlan::DeferUpdate)
            } else {
                Err(AppError::UnknownCustomId(custom_id.to_owned()))
            }
        }
        SourceKind::ModalSubmit { custom_id, .. } => {
            if EXACT_MODALS.contains(&custom_id.as_str()) {
                Ok(AckPlan::DeferPublic)
            } else if is_prefixed_modal(custom_id) {
                Ok(AckPlan::DeferUpdate)
            } else {
                Err(AppError::UnknownCustomId(custom_id.to_owned()))
            }
        }
    }
}

/// The modal `custom_id`s the plan phase acknowledges publicly: the three bare
/// modals, with no action suffix.
const EXACT_MODALS: &[&str] = &[handlers::run::NEW_RUN_MODAL, handlers::prompt::PROMPT_MODAL];

/// The modal `custom_id` prefixes the plan phase acknowledges as an update:
/// every modal opened from a component, carrying an action or gate rowid.
const PREFIXED_MODALS: &[&str] = &[
    "v1:editgo:",
    "v1:acceptgo:",
    "v1:formgo:",
    "v1:followupgo:",
    handlers::run::PLAN_OTHER_MODAL,
    handlers::projects::RUN_NOPLAN_MODAL,
];

/// Whether a `custom_id` belongs to one of the prefixed modal families.
fn is_prefixed_modal(custom_id: &str) -> bool {
    PREFIXED_MODALS
        .iter()
        .any(|prefix| custom_id.starts_with(prefix))
}

/// Every modal `custom_id` family this router accepts, as one sample id per
/// family (the bare ids plus `<prefix>1`).
///
/// This is the router's own list — the PH-7 drift guard crosses it against the
/// Signal text-form catalog (`ocbot_core::render::is_modal_id`), so a new modal
/// that no adapter can open as a text form fails CI instead of silently
/// degrading to an unanswered form.
#[must_use]
pub fn modal_ids() -> Vec<String> {
    EXACT_MODALS
        .iter()
        .map(|id| (*id).to_owned())
        .chain(PREFIXED_MODALS.iter().map(|prefix| format!("{prefix}1")))
        .collect()
}

/// Custom-id prefixes whose plan is a plain deferral: acknowledged with
/// `DeferUpdate` immediately and fully dispatched in the execute pass.
///
/// This is the single source of truth for the deferred-prefix set (SPEC-002
/// FR-14/FR-15). `is_deferred_action` derives from it, and the `prefix_sync`
/// regression test checks it against the real plan/execute behaviour, so the
/// list cannot drift from the router.
///
/// The state-dependent button families (`v1:send:`, `v1:runany:`, `v1:accept:`,
/// `v1:park:`, `v1:discard:`, `v1:gate:`) have their own preflight branches and
/// are deliberately not plain deferrals.
const EXACT_DEFERRED_IDS: &[&str] = &["v1:projects"];

const DEFERRED_PREFIXES: &[&str] = &[
    "v1:reset:",
    "v1:resetgo:",
    "v1:releaseq:",
    "v1:release:",
    "v1:formpick:",
    "v1:plan:",
    "v1:planp:",
    "v1:planuse:",
    "v1:tpl:",
    "v1:projects:p:",
    "v1:project_select:",
    "v1:project_page:",
    "v1:readme_full:",
    "v1:readme_page:",
    "v1:plan_select:",
    "v1:load_plan:",
];

/// Custom ids whose plan is a plain deferral (all work is in the execute pass).
fn is_deferred_action(custom_id: &str) -> bool {
    EXACT_DEFERRED_IDS.contains(&custom_id)
        || DEFERRED_PREFIXES
            .iter()
            .any(|prefix| custom_id.starts_with(prefix))
        || custom_id.starts_with("v1:page:history:")
}

/// The execute pass of a deferred interaction; returns an optional ephemeral
/// followup text (which the adapter posts after the work finishes).
#[expect(
    clippy::too_many_lines,
    reason = "the router is a flat, exhaustive match over the interaction grammar; splitting it would hide the one-place review of every route"
)]
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
            "abort" => {
                handlers::abort::execute(ctx, source).await?;
                Ok(None)
            }
            "mute" => {
                handlers::mute::execute(ctx, source).await?;
                Ok(None)
            }
            "projects" => {
                handlers::projects::list_projects(ctx, 0, source).await?;
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
                handlers::accept::execute_mark_done(ctx, parse_action_id(id)?, &source.user_id)
                    .await
            } else if let Some(id) = custom_id.strip_prefix("v1:park:") {
                handlers::park::begin_park(ctx, parse_action_id(id)?, &source.user_id).await
            } else if let Some(id) = custom_id.strip_prefix("v1:discard:") {
                handlers::discard::execute_discard(ctx, parse_action_id(id)?, &source.user_id).await
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
            } else if let Some(rest) = custom_id.strip_prefix("v1:gate:") {
                let (id, decision) = split_gate_id(rest)?;
                handlers::gate::execute_permission_reply(ctx, id, decision, &source.user_id).await
            } else if let Some(rest) = custom_id.strip_prefix("v1:formpick:") {
                let (id, value) = parse_form_pick(rest)?;
                handlers::gate::execute_form_pick(ctx, id, &value, &source.user_id).await
            } else if custom_id.starts_with("v1:formgo:") {
                // A Component `v1:formgo` plans a modal and never executes; a
                // stale press is a no-op.
                Err(AppError::AlreadyHandled)
            } else if custom_id.starts_with("v1:followup:") {
                // A Component `v1:followup` plans a modal and never executes.
                Err(AppError::AlreadyHandled)
            } else if let Some(rest) = custom_id.strip_prefix("v1:plan:") {
                let (id, index) = parse_pair(rest)?;
                handlers::run::plan_pick(ctx, id, index, source).await
            } else if let Some(rest) = custom_id.strip_prefix("v1:planp:") {
                let (id, page) = parse_pair(rest)?;
                handlers::run::plan_page_edit(ctx, id, page, source).await?;
                Ok(None)
            } else if let Some(id) = custom_id.strip_prefix("v1:planuse:") {
                handlers::run::plan_use(ctx, parse_action_id(id)?, source).await
            } else if let Some(id) = custom_id.strip_prefix("v1:tpl:") {
                handlers::run::toggle_template(ctx, parse_action_id(id)?, source).await
            } else if custom_id.starts_with("v1:planother:") {
                // A Component `v1:planother` plans a modal and never executes.
                Err(AppError::AlreadyHandled)
            } else if custom_id == "v1:projects" {
                handlers::projects::list_projects(ctx, 0, source).await?;
                Ok(None)
            } else if let Some(page) = custom_id.strip_prefix("v1:projects:p:") {
                handlers::projects::list_projects(ctx, parse_page(page)?, source).await?;
                Ok(None)
            } else if let Some(rest) = custom_id.strip_prefix("v1:project_select:") {
                let project = parse_project_token(rest)?;
                handlers::projects::select_project(ctx, &project, source).await?;
                Ok(None)
            } else if let Some(rest) = custom_id.strip_prefix("v1:project_page:") {
                let (project, page) = parse_project_page(rest)?;
                handlers::projects::show_project_page(ctx, &project, page, source).await?;
                Ok(None)
            } else if let Some(rest) = custom_id.strip_prefix("v1:readme_full:") {
                let project = parse_project_token(rest)?;
                handlers::projects::show_full_readme(ctx, &project, 0, source).await?;
                Ok(None)
            } else if let Some(rest) = custom_id.strip_prefix("v1:readme_page:") {
                let (project, page) = parse_project_page(rest)?;
                handlers::projects::show_full_readme(ctx, &project, page, source).await?;
                Ok(None)
            } else if let Some(rest) = custom_id.strip_prefix("v1:plan_select:") {
                let (project, plan) = parse_project_and_plan_tokens(rest)?;
                handlers::projects::select_plan(ctx, &project, &plan, source).await?;
                Ok(None)
            } else if let Some(rest) = custom_id.strip_prefix("v1:load_plan:") {
                let (project, plan) = parse_project_and_plan_tokens(rest)?;
                handlers::projects::load_plan_and_run(ctx, &project, &plan, source).await
            } else if custom_id.starts_with("v1:run_noplan:") {
                // A Component `v1:run_noplan:` plans the label modal and never
                // executes; a stale press is a no-op (SPEC-005 flow 7).
                Err(AppError::AlreadyHandled)
            } else {
                Err(AppError::UnknownCustomId(custom_id.to_owned()))
            }
        }
        SourceKind::ModalSubmit { custom_id, .. } => {
            if custom_id == handlers::run::NEW_RUN_MODAL {
                handlers::run::execute_newrun(ctx, source).await
            } else if custom_id == handlers::prompt::PROMPT_MODAL {
                handlers::prompt::execute_prompt(ctx, source).await
            } else if let Some(id) = custom_id.strip_prefix(handlers::run::PLAN_OTHER_MODAL) {
                handlers::run::execute_plan_other(ctx, parse_action_id(id)?, source).await
            } else if let Some(id) = custom_id.strip_prefix("v1:editgo:") {
                handlers::run::execute_editgo(ctx, parse_action_id(id)?, source).await
            } else if let Some(id) = custom_id.strip_prefix("v1:followupgo:") {
                let prompt = source.input("prompt").unwrap_or_default();
                handlers::followup::execute_followup(
                    ctx,
                    parse_action_id(id)?,
                    prompt,
                    &source.user_id,
                )
                .await
            } else if let Some(id) = custom_id.strip_prefix("v1:formgo:") {
                handlers::gate::execute_form_submit(ctx, parse_action_id(id)?, source).await
            } else if let Some(id) = custom_id.strip_prefix("v1:acceptgo:") {
                let subject = source.input("subject").unwrap_or_default();
                let body = source.input("body").unwrap_or_default();
                handlers::accept::execute_accept(
                    ctx,
                    parse_action_id(id)?,
                    subject,
                    body,
                    &source.user_id,
                )
                .await
            } else if let Some(rest) = custom_id.strip_prefix(handlers::projects::RUN_NOPLAN_MODAL)
            {
                let project = parse_project_token(rest)?;
                handlers::projects::run_without_plan(ctx, &project, source).await
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

/// Parse a bare usize index.
fn parse_index(raw: &str) -> Result<usize, AppError> {
    raw.trim()
        .parse()
        .map_err(|_| AppError::UnknownCustomId(format!("bad index {raw:?}")))
}

/// Parse `<action_id>:<n>` (plan picks and plan paging).
fn parse_pair(raw: &str) -> Result<(i64, usize), AppError> {
    let (id, value) = raw
        .split_once(':')
        .ok_or_else(|| AppError::UnknownCustomId(format!("bad pair {raw:?}")))?;
    Ok((parse_action_id(id)?, parse_index(value)?))
}

/// Parse `<action_rowid>:<decision>` for a permission-gate button.
fn split_gate_id(raw: &str) -> Result<(i64, &str), AppError> {
    let (id, decision) = raw
        .rsplit_once(':')
        .ok_or_else(|| AppError::UnknownCustomId(format!("bad gate id {raw:?}")))?;
    if !["once", "always", "reject"].contains(&decision) {
        return Err(AppError::UnknownCustomId(format!(
            "bad gate decision {raw:?}"
        )));
    }
    Ok((parse_action_id(id)?, decision))
}

/// Parse `<gate_rowid>:<option-value>` for a single-choice form button.
fn parse_form_pick(raw: &str) -> Result<(i64, String), AppError> {
    let (id, value) = raw
        .split_once(':')
        .ok_or_else(|| AppError::UnknownCustomId(format!("bad form pick {raw:?}")))?;
    Ok((parse_action_id(id)?, value.to_owned()))
}

/// Parse an opaque project token from a `custom_id` segment (SPEC-005).
fn parse_project_token(raw: &str) -> Result<String, AppError> {
    if raw.len() != 16 || !raw.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppError::UnknownCustomId(
            "invalid project token".to_owned(),
        ));
    }
    Ok(raw.to_owned())
}

/// Parse project and plan tokens from a `custom_id` segment (SPEC-005).
/// Format: `project_token:plan_token`.
fn parse_project_and_plan_tokens(raw: &str) -> Result<(String, String), AppError> {
    let (project, plan) = raw
        .split_once(':')
        .ok_or_else(|| AppError::UnknownCustomId(format!("bad project:plan pair {raw:?}")))?;
    Ok((parse_project_token(project)?, parse_project_token(plan)?))
}

/// Parse `<project-token>:<page>` for project and README navigation.
fn parse_project_page(raw: &str) -> Result<(String, usize), AppError> {
    let (project, page) = raw
        .split_once(':')
        .ok_or_else(|| AppError::UnknownCustomId(format!("bad project page {raw:?}")))?;
    Ok((parse_project_token(project)?, parse_page(page)?))
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
