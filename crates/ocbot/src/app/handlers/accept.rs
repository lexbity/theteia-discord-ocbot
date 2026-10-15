//! Accept (FR-16/FR-17) and the non-git "Mark done" degrade (FR-26), with the
//! §6.4 CP-A0..A3 crash points.
//!
//! The commit subject/body are persisted at the CP-A0 CAS *before* any git
//! work, so a hook failure or a crash can prefill the modal from the DB rather
//! than from in-memory retry state (P6).

use std::sync::Arc;

use ocbot_core::{Column, LABEL_MAX_CHARS, State, WriteValue, truncate_chars};

use crate::adapters::now_unix;
use crate::adapters::store::ActionRow;
use crate::app::context::AppContext;
use crate::app::crashpoint::checkpoint;
use crate::app::error::AppError;
use crate::app::handlers::support::TempMessage;
use crate::app::interaction::{InputStyle, ModalInputSpec, ModalSpec};

/// The decision states an Accept may start from (a completed report).
const ACCEPT_FROM: [State; 1] = [State::Reported];

/// Build the Accept modal, prefilled from the DB (P6) and the action label.
pub fn accept_modal(action: &ActionRow) -> ModalSpec {
    let subject = action
        .pending_commit_subject
        .clone()
        .unwrap_or_else(|| truncate(&action.label, 72));
    ModalSpec {
        custom_id: format!("v1:acceptgo:{}", action.id),
        title: "Accept — commit message".to_owned(),
        inputs: vec![
            ModalInputSpec {
                id: "subject".to_owned(),
                label: "Commit subject".to_owned(),
                style: InputStyle::Short,
                value: Some(subject),
                required: true,
            },
            ModalInputSpec {
                id: "body".to_owned(),
                label: "Commit body (optional)".to_owned(),
                style: InputStyle::Paragraph,
                value: action.pending_commit_body.clone(),
                required: false,
            },
        ],
    }
}

/// Execute the non-git Accept: record the decision in the DB only (FR-26).
/// Called for `v1:accept:<id>` when the directory is not a git repo.
pub async fn execute_mark_done(
    ctx: &Arc<AppContext>,
    action_id: i64,
    operator: &str,
) -> Result<Option<String>, AppError> {
    let action = ctx
        .store
        .action(action_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("action {action_id}")))?;
    if !ACCEPT_FROM.contains(&action.state) {
        return Err(AppError::AlreadyHandled);
    }
    let dir = crate::app::handlers::support::directory_for(ctx, &action).await?;
    let dir_text = crate::app::handlers::support::dir_text(&dir);

    let accepted = ctx
        .store
        .cas_state_from_any(action_id, &ACCEPT_FROM, State::Accepted, &[])
        .await?;
    if !accepted {
        return Err(AppError::AlreadyHandled);
    }
    ctx.store
        .release_lock(&dir_text, &action_id.to_string())
        .await?;
    ctx.store.retire_active_session(&dir_text).await?;
    ctx.store
        .audit(operator, "markdone", Some(action_id), "accepted")
        .await?;
    Ok(Some(format!("marked **{}** done", action.label)))
}

/// Execute `v1:acceptgo:<id>`: commit the attempt on the work branch (§6.4).
#[expect(
    clippy::too_many_lines,
    reason = "the §6.4 accept sequence is a linear crash-labelled protocol; splitting hides the CP ordering"
)]
pub async fn execute_accept(
    ctx: &Arc<AppContext>,
    action_id: i64,
    subject: &str,
    body: &str,
    operator: &str,
) -> Result<Option<String>, AppError> {
    let subject = subject.trim();
    if subject.is_empty() {
        return Err(AppError::Other("a commit subject is required".to_owned()));
    }

    // CP-A0: write-ahead the intent (state + message) atomically.
    let intent = vec![
        (
            Column::PendingCommitSubject,
            WriteValue::Text(subject.to_owned()),
        ),
        (Column::PendingCommitBody, WriteValue::Text(body.to_owned())),
    ];
    let started = ctx
        .store
        .cas_state_from_any(action_id, &ACCEPT_FROM, State::Accepting, &intent)
        .await?;
    if !started {
        return Err(AppError::AlreadyHandled);
    }
    checkpoint("CP-A0");

    let action = ctx
        .store
        .action(action_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("action {action_id}")))?;
    let dir = crate::app::handlers::support::directory_for(ctx, &action).await?;
    let dir_text = crate::app::handlers::support::dir_text(&dir);
    let project = ctx.store.project(&dir_text).await?;

    // Non-git fallback: the router routes non-git to mark_done, but a direct
    // caller (crash harness) may reach here; treat it as a DB-only accept.
    let Some(work_branch) = project.and_then(|project| project.work_branch) else {
        return finish_non_git(ctx, action_id, &dir_text, operator).await;
    };

    // CP-A1: the branch must still be the run's work branch (FR-17) and the
    // FR-27 dirty policy must allow the commit.
    let current = ctx.git.branch(&dir).await?;
    if current != work_branch {
        ctx.store
            .cas_state(action_id, State::Accepting, State::Reported, &[])
            .await?;
        return Err(AppError::Other(format!(
            "refusing to accept: on branch {current:?}, expected {work_branch:?} (FR-17)"
        )));
    }
    if action.dirty_at_start && !action.include_existing {
        ctx.store
            .cas_state(action_id, State::Accepting, State::Reported, &[])
            .await?;
        return Err(AppError::Other(
            "refusing to accept: the run started on a dirty tree that was not included (FR-27)"
                .to_owned(),
        ));
    }
    checkpoint("CP-A1");

    // CP-A2: commit. The message goes via -F tempfile, never argv (P7).
    let message = if body.trim().is_empty() {
        subject.to_owned()
    } else {
        format!("{subject}\n\n{body}")
    };
    let temp = TempMessage::new(&message)?;
    let sha = match ctx.git.commit_all(&dir, temp.path(), false).await {
        Ok(Ok(sha)) => sha,
        Ok(Err(hook)) => {
            // Hook / rejected commit: return to `reported` and surface output.
            // The staged index stays (unlike park's CP-P1 failure): the action
            // is re-offered with the tree still in play, and a retried Accept
            // re-adds anyway.
            ctx.store
                .cas_state(action_id, State::Accepting, State::Reported, &[])
                .await?;
            let message = crate::app::handlers::support::failure_post(
                format!(
                    "❌ **Accept failed for {}**",
                    truncate_chars(&action.label, LABEL_MAX_CHARS)
                ),
                &hook.output,
                "",
                operator,
            );
            ctx.chat
                .post(
                    &crate::app::destination::destination_for(&action)?,
                    &message,
                )
                .await?;
            return Ok(Some("accept failed — see the hook output".to_owned()));
        }
        Err(error) => {
            ctx.store
                .cas_state(action_id, State::Accepting, State::Reported, &[])
                .await?;
            return Err(AppError::Git(error));
        }
    };
    checkpoint("CP-A2");

    // CP-A3: record the sha, seal, release the lock, confirm.
    let writes = vec![
        (Column::AcceptSha, WriteValue::Text(sha.clone())),
        (Column::EndedAt, WriteValue::Int(now_unix())),
    ];
    let accepted = ctx
        .store
        .cas_state(action_id, State::Accepting, State::Accepted, &writes)
        .await?;
    if !accepted {
        return Err(AppError::AlreadyHandled);
    }
    ctx.store
        .release_lock(&dir_text, &action_id.to_string())
        .await?;
    ctx.store.retire_active_session(&dir_text).await?;
    ctx.store
        .audit(operator, "accept", Some(action_id), &sha)
        .await?;
    let short: String = sha.chars().take(8).collect();
    let message = crate::app::handlers::support::simple(
        format!("✅ Accepted **{}** as `{short}`", action.label),
        operator,
    );
    ctx.chat
        .post(
            &crate::app::destination::destination_for(&action)?,
            &message,
        )
        .await?;
    checkpoint("CP-A3");
    Ok(Some(format!("accepted as {short}")))
}

/// Resume (or roll back) an action stopped in `accepting` after a crash
/// (spec §6.4 reconcile table). The adopt rule is conservative: HEAD's subject
/// must equal the persisted subject **and** the commit time must be at or after
/// the send time; otherwise the action returns to `reported` and is re-offered.
pub async fn resume(ctx: &Arc<AppContext>, action: &ActionRow) -> Result<Option<String>, AppError> {
    if action.state != State::Accepting {
        return Err(AppError::AlreadyHandled);
    }
    let dir = crate::app::handlers::support::directory_for(ctx, action).await?;
    let dir_text = crate::app::handlers::support::dir_text(&dir);
    let head = ctx.git.head(&dir).await?;

    let matches = match action.pending_commit_subject.as_deref() {
        Some(subject) => {
            let log = ctx.git.log(&dir, None).await?;
            log.first().is_some_and(|row| {
                row.sha == head
                    && row.subject == subject
                    && action
                        .started_at
                        .is_none_or(|started| row.timestamp >= started)
            })
        }
        None => false,
    };

    if matches {
        let writes = vec![
            (Column::AcceptSha, WriteValue::Text(head.clone())),
            (Column::EndedAt, WriteValue::Int(now_unix())),
        ];
        let adopted = ctx
            .store
            .cas_state(action.id, State::Accepting, State::Accepted, &writes)
            .await?;
        if !adopted {
            return Err(AppError::AlreadyHandled);
        }
        ctx.store
            .release_lock(&dir_text, &action.id.to_string())
            .await?;
        ctx.store.retire_active_session(&dir_text).await?;
        tracing::info!(action = action.id, "reconcile adopted a landed accept");
        return Ok(Some("accepted (recovered)".to_owned()));
    }

    let reverted = ctx
        .store
        .cas_state(action.id, State::Accepting, State::Reported, &[])
        .await?;
    if reverted {
        tracing::info!(
            action = action.id,
            "reconcile re-offered an interrupted accept"
        );
        Ok(Some("re-offered".to_owned()))
    } else {
        Err(AppError::AlreadyHandled)
    }
}

async fn finish_non_git(
    ctx: &Arc<AppContext>,
    action_id: i64,
    dir_text: &str,
    operator: &str,
) -> Result<Option<String>, AppError> {
    let accepted = ctx
        .store
        .cas_state_from_any(action_id, &[State::Accepting], State::Accepted, &[])
        .await?;
    if !accepted {
        return Err(AppError::AlreadyHandled);
    }
    ctx.store
        .release_lock(dir_text, &action_id.to_string())
        .await?;
    ctx.store.retire_active_session(dir_text).await?;
    ctx.store
        .audit(operator, "markdone", Some(action_id), "accepted")
        .await?;
    Ok(Some("marked done".to_owned()))
}

fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}
