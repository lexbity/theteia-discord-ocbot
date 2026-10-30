//! Park & redo (FR-19), the Rev B sequence (spec P7 / §6.4 CP-P0..CP-P4):
//! commit the attempt on the work branch, branch it, then reset — each step
//! idempotent and individually resumable from the `park_step` write-ahead log.

use std::path::Path;
use std::sync::Arc;

use ocbot_core::{
    Column, LABEL_MAX_CHARS, State, WriteValue, slug, staged_preview, truncate_chars,
};

use crate::adapters::now_unix;
use crate::adapters::store::ActionRow;
use crate::app::context::AppContext;
use crate::app::crashpoint::checkpoint;
use crate::app::error::AppError;
use crate::app::handlers::support::{TempMessage, dir_text, directory_for};

/// Refuse to park when an untracked, unignored file exceeds this size (P7).
pub const MAX_PARK_FILE_MB: u64 = 50;

/// The states a park may start from (spec CP-P0).
const PARK_FROM: [State; 3] = [State::Reported, State::Errored, State::Aborted];

/// The park-marker prefix used to recognize a resumed commit (P7/reconcile).
pub const PARK_MARKER: &str = "park: ";

/// Execute `v1:park:<id>`: guard, CAS into `parking`, then run the sequence.
pub async fn begin_park(
    ctx: &Arc<AppContext>,
    action_id: i64,
    operator: &str,
) -> Result<Option<String>, AppError> {
    let action = ctx
        .store
        .action(action_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("action {action_id}")))?;
    if !PARK_FROM.contains(&action.state) {
        return Err(AppError::AlreadyHandled);
    }
    let dir = directory_for(ctx, &action).await?;

    // Big-file guard before the CAS so a refused park leaves the action in its
    // decision state rather than stuck in `parking`.
    if let Some(name) = oversized_untracked(ctx, &dir).await? {
        return Ok(Some(format!(
            "refusing to park: untracked file **{name}** exceeds {MAX_PARK_FILE_MB} MB"
        )));
    }

    let branch = format!("rejected/{}-{}", slug(&action.label), action.attempt);
    let writes = vec![
        (Column::ParkBranchName, WriteValue::Text(branch)),
        (Column::ParkStep, WriteValue::Int(0)),
    ];
    let started = ctx
        .store
        .cas_state_from_any(action_id, &PARK_FROM, State::Parking, &writes)
        .await?;
    if !started {
        return Err(AppError::AlreadyHandled);
    }
    checkpoint("CP-P0");

    let action = ctx
        .store
        .action(action_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("action {action_id}")))?;
    resume(ctx, &action, operator).await
}

/// Resume an action already in `parking` from its recorded `park_step`. Safe to
/// call repeatedly (boot reconcile calls it after every crash).
#[expect(
    clippy::too_many_lines,
    reason = "the Rev B park sequence is a linear crash-labelled protocol; splitting hides the step ordering"
)]
pub async fn resume(
    ctx: &Arc<AppContext>,
    action: &ActionRow,
    operator: &str,
) -> Result<Option<String>, AppError> {
    if action.state != State::Parking {
        return Err(AppError::AlreadyHandled);
    }
    let dir = directory_for(ctx, action).await?;
    let dir_text = dir_text(&dir);
    let dest = crate::app::destination::destination_for(action)?;
    let branch = action
        .park_branch_name
        .clone()
        .ok_or_else(|| AppError::Other("parking action has no branch name".to_owned()))?;

    if action.park_step < 1 {
        // CP-P1: commit the attempt on the work branch (hooks skipped — this is
        // a safety snapshot, not a release).
        let marker = format!("{PARK_MARKER}{} (attempt {})", action.label, action.attempt);
        let temp = TempMessage::new(&marker)?;
        match ctx.git.commit_all(&dir, temp.path(), true).await {
            Ok(Ok(_sha)) => {}
            // A park of an attempt that changed nothing is still a park: there
            // is nothing to snapshot, so branch at the current HEAD (below).
            Ok(Err(hook)) if is_nothing_to_commit(&hook.output) => {}
            Ok(Err(hook)) => {
                // CP-P1 failure: nothing landed. Unstage while `park_step` is
                // still 0 — reconcile replays step 1 (which re-adds), so a
                // crash mid-repair converges; after the terminal `errored`
                // CAS nothing would come back for the index. Mixed reset: the
                // files stay, only the staging is undone.
                let unstaged = ctx.git.unstage_all(&dir).await;
                ctx.store
                    .cas_state(action.id, State::Parking, State::Errored, &[])
                    .await?;
                let note = match &unstaged {
                    Ok(()) => "Files are untouched (index unstaged).".to_owned(),
                    Err(error) => {
                        format!("⚠️ files kept, but the index could not be unstaged: {error}")
                    }
                };
                let message = crate::app::handlers::support::failure_post(
                    format!(
                        "❌ **Park failed for {}**",
                        truncate_chars(&action.label, LABEL_MAX_CHARS)
                    ),
                    &hook.output,
                    &note,
                    operator,
                );
                ctx.chat.post(&dest, &message).await?;
                return Ok(Some("park failed — see the output".to_owned()));
            }
            Err(error) => {
                // Infra failure (spawn/timeout): the add half of `commit_all`
                // may have staged before the commit broke. Same repair, best
                // effort — git itself is misbehaving, and the original error
                // is the one that must propagate.
                let _ = ctx.git.unstage_all(&dir).await;
                ctx.store
                    .cas_state(action.id, State::Parking, State::Errored, &[])
                    .await?;
                return Err(AppError::Git(error));
            }
        }
        ctx.store
            .cas_state(
                action.id,
                State::Parking,
                State::Parking,
                &[(Column::ParkStep, WriteValue::Int(1))],
            )
            .await?;
        checkpoint("CP-P1");
    }

    if action.park_step < 2 {
        // CP-P2: create the rejected branch at the attempt commit. Idempotent:
        // an existing branch at the same tip is skipped; a different tip aborts.
        let head = ctx.git.head(&dir).await?;
        match ctx.git.branch_exists(&dir, &branch).await? {
            Some(tip) if tip == head => {}
            Some(tip) => {
                return Err(AppError::Other(format!(
                    "rejected branch {branch} already exists at {tip}, expected {head}"
                )));
            }
            None => {
                ctx.git.create_branch_at(&dir, &branch, &head).await?;
            }
        }
        ctx.store
            .cas_state(
                action.id,
                State::Parking,
                State::Parking,
                &[(Column::ParkStep, WriteValue::Int(2))],
            )
            .await?;
        checkpoint("CP-P2");
    }

    if action.park_step < 3 {
        // CP-P3: reset the work branch to the pre-attempt state. `start_head`
        // is the recorded pre-run HEAD (equal to the last accept in the normal
        // flow); fall back to the last accepted sha, then the run base (P7).
        let target = reset_target(ctx, action, &dir_text).await?;
        ctx.git.reset_hard(&dir, &target).await?;
        ctx.store
            .cas_state(
                action.id,
                State::Parking,
                State::Parking,
                &[(Column::ParkStep, WriteValue::Int(3))],
            )
            .await?;
        checkpoint("CP-P3");
    }

    // CP-P4: seal, release the lock, retire the session, stage the retry.
    let parked = ctx
        .store
        .cas_state(
            action.id,
            State::Parking,
            State::Parked,
            &[(Column::EndedAt, WriteValue::Int(now_unix()))],
        )
        .await?;
    if !parked {
        return Err(AppError::AlreadyHandled);
    }
    ctx.store
        .release_lock(&dir_text, &action.id.to_string())
        .await?;
    ctx.store.retire_active_session(&dir_text).await?;
    ctx.store
        .audit(operator, "park", Some(action.id), &branch)
        .await?;
    let child = ctx
        .store
        .insert_staged_child(
            action.run_id,
            action.chat_ref.as_deref(),
            &action.label,
            &action.prompt_sent,
            &action.template,
            action.attempt + 1,
            action.id,
        )
        .await?;
    // SPEC-005 G-01: the parked-retry preview carries the opaque project name;
    // the stored prompt (re-staged verbatim into the child) stays out of the
    // message chrome.
    let preview = staged_preview(
        &action.label,
        child,
        ocbot_core::mentionable_id(operator),
        &action.template,
        &ocbot_core::paths::project_display_name(&dir),
    );
    ctx.chat.post(&dest, &preview).await?;
    checkpoint("CP-P4");
    Ok(Some(format!(
        "parked on `{branch}`; retry staged (attempt {})",
        action.attempt + 1
    )))
}

/// The reset target for step 3: the recorded pre-run HEAD, else the last
/// accepted sha, else the project base.
async fn reset_target(
    ctx: &Arc<AppContext>,
    action: &ActionRow,
    dir_text: &str,
) -> Result<String, AppError> {
    if let Some(head) = &action.start_head {
        return Ok(head.clone());
    }
    if let Some(sha) = ctx.store.last_accept_sha(dir_text).await? {
        return Ok(sha);
    }
    ctx.store
        .project(dir_text)
        .await?
        .and_then(|project| project.base_sha)
        .ok_or_else(|| {
            AppError::Other("no reset target: no start head, accept, or base sha".to_owned())
        })
}

/// Whether a rejected `git commit` means the tree had nothing to record.
fn is_nothing_to_commit(output: &str) -> bool {
    output.contains("nothing to commit") || output.contains("no changes added")
}

/// The first untracked file larger than [`MAX_PARK_FILE_MB`], if any.
async fn oversized_untracked(
    ctx: &Arc<AppContext>,
    dir: &Path,
) -> Result<Option<String>, AppError> {
    let limit = MAX_PARK_FILE_MB.saturating_mul(1024 * 1024);
    for file in ctx.git.untracked_files(dir).await? {
        let path = dir.join(&file);
        if let Ok(metadata) = std::fs::metadata(&path)
            && metadata.is_file()
            && metadata.len() > limit
        {
            return Ok(Some(file));
        }
    }
    Ok(None)
}
