//! Gate replies (spec FR-20..24, FR-41): exactly-once button answers for
//! permissions, and form answers through a modal or quick choice buttons.
//! The reply POST is retried on transport errors (×3) and a remote 404
//! ("unknown request") is success (FR-23/S-3).

use std::sync::Arc;
use std::time::Duration;

use ocbot_core::gates::{FormFieldKind, FormPlaybook, GateKind, GateRequest, form_playbook};
use ocbot_core::render::OutgoingMessage;

use crate::adapters::store::GateRow;
use crate::app::context::AppContext;
use crate::app::error::AppError;
use crate::app::gates_loop;
use crate::app::interaction::{InputStyle, InteractionSource, ModalInputSpec, ModalSpec};

/// Execute `v1:gate:<rowid>:<once|always|reject>`.
pub async fn execute_permission_reply(
    ctx: &Arc<AppContext>,
    rowid: i64,
    decision: &str,
    operatore_id: u64,
) -> Result<Option<String>, AppError> {
    let decision = match decision {
        "once" => ocbot_core::GateDecision::Once,
        "always" => ocbot_core::GateDecision::Always,
        "reject" => ocbot_core::GateDecision::Reject,
        other => {
            return Err(AppError::UnknownCustomId(format!(
                "bad gate decision {other:?}"
            )));
        }
    };

    let row = ctx
        .store
        .gate(rowid)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("gate {rowid}")))?;
    if row.state != "pending" {
        return Err(AppError::AlreadyHandled);
    }
    let answer = serde_json::json!(decision.as_str()).to_string();
    if !ctx
        .store
        .cas_gate(rowid, "pending", "answered", Some(&answer))
        .await?
    {
        return Err(AppError::AlreadyHandled);
    }
    let row = ctx
        .store
        .gate(rowid)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("gate {rowid}")))?;
    post_with_retry(ctx, &row).await?;
    set_action_clear(ctx, &row).await?;
    edit_gate_message(
        ctx,
        &row,
        &format!("✅ answered: **{}**", decision.as_str()),
    )
    .await?;
    ctx.store
        .audit(operatore_id, "gate-reply", row.action_id, decision.as_str())
        .await?;
    Ok(Some(format!("answered {}.", decision.as_str())))
}

/// The modal plan for `v1:formgo:<rowid>` (the open button). A missing or
/// already-answered gate is `AlreadyHandled` (double-tap-safe); a pending gate
/// that is not modal-renderable (single-choice buttons or surface-only) yields
/// `Ok(None)` — there is no modal to open, and the plan is a no-op.
pub async fn form_modal(ctx: &Arc<AppContext>, rowid: i64) -> Result<Option<ModalSpec>, AppError> {
    let row = match ctx.store.gate(rowid).await? {
        Some(row) if row.state == "pending" => row,
        _ => return Err(AppError::AlreadyHandled),
    };
    let gate = gate_from_row(&row)?;
    let fields = match &gate.kind {
        GateKind::Form { fields, .. }
            if matches!(form_playbook(fields), Some(FormPlaybook::Modal)) =>
        {
            fields
        }
        _ => return Ok(None),
    };
    let GateKind::Form { title, .. } = &gate.kind else {
        return Ok(None);
    };
    let title: String = title.chars().take(45).collect();
    let inputs: Vec<ModalInputSpec> = fields.iter().map(field_input).collect();
    Ok(Some(ModalSpec {
        custom_id: format!("v1:formgo:{rowid}"),
        title: if title.is_empty() {
            "Answer form".to_owned()
        } else {
            title
        },
        inputs,
    }))
}

/// Execute the `v1:formgo:<rowid>` modal submit: collect and coerce every field
/// into the answer map, then commit it exactly once.
pub async fn execute_form_submit(
    ctx: &Arc<AppContext>,
    rowid: i64,
    source: &InteractionSource,
) -> Result<Option<String>, AppError> {
    let row = load_pending(ctx, rowid).await?;
    let gate = gate_from_row(&row)?;
    let fields = match &gate.kind {
        GateKind::Form { fields, .. } => fields,
        GateKind::Permission { .. } => return Err(AppError::AlreadyHandled),
    };
    let mut answer = serde_json::Map::new();
    for field in fields {
        let raw = source.input(&field.key).unwrap_or_default();
        let value = coerce_field_value(field, raw)?;
        answer.insert(field.key.clone(), value);
    }
    commit_form_answer(ctx, row, serde_json::Value::Object(answer), source.user_id).await
}

/// Execute `v1:formpick:<rowid>:<value>`: the one-tap answer of a
/// single-choice form.
pub async fn execute_form_pick(
    ctx: &Arc<AppContext>,
    rowid: i64,
    raw_value: &str,
    user_id: u64,
) -> Result<Option<String>, AppError> {
    let row = load_pending(ctx, rowid).await?;
    let gate = gate_from_row(&row)?;
    let fields = match &gate.kind {
        GateKind::Form { fields, .. } => fields,
        GateKind::Permission { .. } => return Err(AppError::AlreadyHandled),
    };
    if !matches!(form_playbook(fields), Some(FormPlaybook::SingleChoice(_))) {
        return Err(AppError::UnknownCustomId(format!(
            "form {rowid} is not single-choice"
        )));
    }
    let field = &fields[0];
    let value = coerce_field_value(field, raw_value)?;
    let mut answer = serde_json::Map::new();
    answer.insert(field.key.clone(), value);
    commit_form_answer(ctx, row, serde_json::Value::Object(answer), user_id).await
}

/// Shared form-answer commit: CAS → POST (retried) → edit → clear + audit.
async fn commit_form_answer(
    ctx: &Arc<AppContext>,
    row: GateRow,
    answer: serde_json::Value,
    user_id: u64,
) -> Result<Option<String>, AppError> {
    let answer_json = answer.to_string();
    if !ctx
        .store
        .cas_gate(row.id, "pending", "answered", Some(&answer_json))
        .await?
    {
        return Err(AppError::AlreadyHandled);
    }
    let row = ctx
        .store
        .gate(row.id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("gate {}", row.id)))?;
    post_with_retry(ctx, &row).await?;
    set_action_clear(ctx, &row).await?;
    edit_gate_message(ctx, &row, "✅ answered in Discord").await?;
    ctx.store
        .audit(user_id, "form-reply", row.action_id, "answered")
        .await?;
    Ok(Some("form answered.".to_owned()))
}

/// Load a pending gate row (state `pending`).
async fn load_pending(ctx: &Arc<AppContext>, rowid: i64) -> Result<GateRow, AppError> {
    let row = ctx
        .store
        .gate(rowid)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("gate {rowid}")))?;
    if row.state != "pending" {
        return Err(AppError::AlreadyHandled);
    }
    Ok(row)
}

/// Rebuild the core gate request from the row's stored payload.
fn gate_from_row(row: &GateRow) -> Result<GateRequest, AppError> {
    serde_json::from_str(&row.payload_json)
        .map_err(|error| AppError::Other(format!("gate payload decode: {error}")))
}

/// Coerce an operator-supplied value to the field's wire type, rejecting values
/// outside a choice's options (the gate stays pending on a bad input).
fn coerce_field_value(
    field: &ocbot_core::FormField,
    raw: &str,
) -> Result<serde_json::Value, AppError> {
    let raw = raw.trim();
    match &field.kind {
        FormFieldKind::Text => {
            if raw.is_empty() {
                return Err(AppError::Other(format!("{} is required", field.key)));
            }
            Ok(serde_json::Value::String(raw.to_owned()))
        }
        FormFieldKind::Choice(options) => {
            if !options.iter().any(|option| option.value == raw) {
                let choices: Vec<String> =
                    options.iter().map(|option| option.value.clone()).collect();
                return Err(AppError::Other(format!(
                    "{} must be one of {}",
                    field.key,
                    choices.join(", ")
                )));
            }
            Ok(serde_json::Value::String(raw.to_owned()))
        }
        FormFieldKind::Boolean => match raw {
            "yes" | "true" | "1" => Ok(serde_json::Value::Bool(true)),
            "no" | "false" | "0" => Ok(serde_json::Value::Bool(false)),
            _ => Err(AppError::Other(format!("{} must be yes or no", field.key))),
        },
        FormFieldKind::Multiselect(options) => {
            let picked: Vec<String> = raw
                .split([',', '\n'])
                .map(str::trim)
                .map(str::to_owned)
                .collect();
            if picked.is_empty() || picked.iter().any(String::is_empty) {
                return Err(AppError::Other(format!(
                    "{} needs at least one option",
                    field.key
                )));
            }
            for value in &picked {
                if !options.iter().any(|option| option.value == *value) {
                    return Err(AppError::Other(format!(
                        "{} has an invalid option {value:?}",
                        field.key
                    )));
                }
            }
            Ok(serde_json::Value::Array(
                picked.into_iter().map(serde_json::Value::String).collect(),
            ))
        }
        FormFieldKind::External(_) | FormFieldKind::Unknown => Err(AppError::AlreadyHandled),
    }
}

/// The modal spec for a renderable form: one input per field (≤5 by
/// renderability).
fn field_input(field: &ocbot_core::FormField) -> ModalInputSpec {
    let base = field.label.clone().unwrap_or_else(|| field.key.clone());
    let (style, guidance): (InputStyle, Option<String>) = match &field.kind {
        FormFieldKind::Choice(options) => {
            let choices: Vec<String> = options.iter().map(|option| option.value.clone()).collect();
            (
                InputStyle::Short,
                Some(format!("choose: {}", choices.join(", "))),
            )
        }
        FormFieldKind::Boolean => (InputStyle::Short, Some("yes / no".to_owned())),
        FormFieldKind::Multiselect(_) => (
            InputStyle::Paragraph,
            Some("one option per line".to_owned()),
        ),
        FormFieldKind::Text | FormFieldKind::External(_) | FormFieldKind::Unknown => {
            (InputStyle::Short, None)
        }
    };
    let label = match guidance {
        Some(guidance) => format!("{base} ({guidance})"),
        None => base,
    };
    ModalInputSpec {
        id: field.key.clone(),
        label: label.chars().take(45).collect(),
        style,
        value: None,
        required: true,
    }
}

/// POST the recorded answer with a 1s/2s/4s transport retry (FR-23).
async fn post_with_retry(ctx: &Arc<AppContext>, row: &GateRow) -> Result<(), AppError> {
    let mut delay_secs = 1;
    loop {
        match gates_loop::post_answer(ctx, row).await {
            Ok(()) => return Ok(()),
            Err(AppError::OpenCode(ocbot_core::OcError::Transport(_))) if delay_secs <= 4 => {
                tokio::time::sleep(Duration::from_secs(delay_secs)).await;
                delay_secs *= 2;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Clear the action's blocked substatus when this gate is the blocker.
async fn set_action_clear(ctx: &Arc<AppContext>, row: &GateRow) -> Result<(), AppError> {
    if let Some(action_id) = row.action_id {
        ctx.store.clear_action_blocked(action_id, row.id).await?;
    }
    Ok(())
}

/// Edit the gate message in place to show the outcome (P10: edits don't
/// notify, which is what we want for a settled gate).
async fn edit_gate_message(
    ctx: &Arc<AppContext>,
    row: &GateRow,
    summary: &str,
) -> Result<(), AppError> {
    let (Some(msg_id), Some(channel)) = (
        row.msg_id.as_deref(),
        row.channel_id
            .and_then(|channel| u64::try_from(channel).ok()),
    ) else {
        return Ok(());
    };
    let message = OutgoingMessage {
        content: format!("{summary} — gate #{}", row.id),
        attachment: None,
        components: Vec::new(),
        mentioned_users: vec![ctx.guard.allowed_user_id],
    };
    ctx.discord
        .edit(channel, msg_id.parse().unwrap_or_default(), &message)
        .await
        .map_err(AppError::Discord)
}
