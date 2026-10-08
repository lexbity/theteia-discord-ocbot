//! The `/run` flow (spec FR-6, Phase 2 slice): directory → label → rendered
//! preview → Send/Edit-and-send. No plan picker yet (Phase 6); the dirty-tree
//! warning with its "Run anyway" button (FR-27) IS in this slice.
//!
//! Plan/execute split: the modal-returning plans do one fast SQLite read each;
//! every execute pass happens after a deferral ack.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ocbot_core::templates;
use ocbot_core::{ButtonKind, OutgoingMessage, State, component_button, staged_preview};

use crate::adapters::store::ActionRow;
use crate::app::context::AppContext;
use crate::app::error::AppError;
use crate::app::interaction::{InputStyle, InteractionSource, ModalInputSpec, ModalSpec};

/// The new-run modal `custom_id`.
pub const NEW_RUN_MODAL: &str = "v1:newrun";
/// Maximum git-log rows in the `{git_log}` slot.
const GIT_LOG_ROWS: usize = 15;
/// Maximum git-status lines in the `{git_status}` slot.
const GIT_STATUS_LINES: usize = 15;

/// Plan phase for `/run`: the new-run modal, directory prefilled from history.
pub async fn begin_modal(
    ctx: &Arc<AppContext>,
    source: &InteractionSource,
) -> Result<ModalSpec, AppError> {
    let prefill = match ctx.store.ui_cwd(source.user_id).await? {
        Some(cwd) => cwd,
        None => ctx
            .store
            .recent_dirs(1)
            .await
            .ok()
            .and_then(|dirs| dirs.first().cloned())
            .unwrap_or_default(),
    };
    Ok(ModalSpec {
        custom_id: NEW_RUN_MODAL.to_owned(),
        title: "ocbot — new run".to_owned(),
        inputs: vec![
            ModalInputSpec {
                id: "directory".to_owned(),
                label: "Directory".to_owned(),
                style: InputStyle::Short,
                value: Some(prefill),
                required: false,
            },
            ModalInputSpec {
                id: "label".to_owned(),
                label: "Label".to_owned(),
                style: InputStyle::Short,
                value: None,
                required: true,
            },
        ],
    })
}

/// Execute phase for the `v1:newrun` modal: validate inputs, stage the action,
/// post the preview with Send / Edit-and-send buttons.
pub async fn execute_newrun(
    ctx: &Arc<AppContext>,
    source: &InteractionSource,
) -> Result<Option<String>, AppError> {
    let label = source.input("label").map(str::trim).unwrap_or_default();
    if label.is_empty() {
        return Err(AppError::Other("a label is required".to_owned()));
    }
    let typed = source.input("directory").map(str::trim).unwrap_or_default();
    let directory = if typed.is_empty() {
        ctx.store.ui_cwd(source.user_id).await?.unwrap_or_default()
    } else {
        typed.to_owned()
    };
    if directory.is_empty() {
        return Err(AppError::Other("a directory is required".to_owned()));
    }
    let canonical = realpath(Path::new(&directory))
        .map_err(|error| AppError::Other(format!("directory {directory:?}: {error}")))?;
    let dir_name = canonical.to_string_lossy().into_owned();

    let (branch, base) = if ctx.git.is_repo(&canonical).await? {
        (
            ctx.git.branch(&canonical).await.ok(),
            ctx.git.head(&canonical).await.ok(),
        )
    } else {
        (None, None)
    };
    ctx.store
        .upsert_project(&dir_name, branch.as_deref(), base.as_deref())
        .await?;

    let prompt = prompt_for(ctx, &canonical, label).await?;
    let run_id = ctx.store.insert_run(&dir_name, "free").await?;
    let channel = i64::try_from(source.channel_id).unwrap_or(i64::MAX);
    let action_id = ctx
        .store
        .insert_action(run_id, Some(channel), label, &prompt, 1)
        .await?;
    ctx.store.set_ui_cwd(source.user_id, &dir_name).await?;
    ctx.store
        .audit(source.user_id, "run", Some(action_id), "staged")
        .await?;

    let preview = staged_preview(label, &prompt, action_id, source.user_id);
    let message_id = ctx.discord.post(source.channel_id, &preview).await?;
    ctx.store
        .set_origin(action_id, &message_id.to_string())
        .await?;

    Ok(Some(
        "staged — choose **Send as-is** or **Edit & send**".to_owned(),
    ))
}

/// Plan phase for `v1:edit:<id>`: reopen the editable prompt modal (one fast
/// read).
pub async fn edit_modal(ctx: &Arc<AppContext>, action_id: i64) -> Result<ModalSpec, AppError> {
    let action = load_staged(ctx, action_id).await?;
    Ok(ModalSpec {
        custom_id: format!("v1:editgo:{action_id}"),
        title: "Edit prompt".to_owned(),
        inputs: vec![ModalInputSpec {
            id: "prompt".to_owned(),
            label: "Prompt".to_owned(),
            style: InputStyle::Paragraph,
            value: Some(action.prompt_sent.clone()),
            required: true,
        }],
    })
}

/// Execute phase for `v1:send:<id>`: honor FR-27 before sending; a clean tree
/// sends now, a dirty one gets the "Run anyway" warning button.
pub async fn execute_send(
    ctx: &Arc<AppContext>,
    action_id: i64,
    source: &InteractionSource,
) -> Result<Option<String>, AppError> {
    let action = load_staged(ctx, action_id).await?;
    let directory = run_directory(ctx, action.run_id).await?;

    if tree_dirty(ctx, &directory).await? && !action.include_existing {
        let message = OutgoingMessage {
            content: format!(
                "⚠️ **{}**\nThe worktree at `{}` has uncommitted changes. Running will include them (FR-27).",
                action.label,
                directory.display(),
            ),
            attachment: None,
            components: vec![component_button(
                "Run anyway",
                format!("v1:runany:{action_id}"),
                ButtonKind::Danger,
            )],
            mentioned_users: vec![source.user_id],
        };
        ctx.discord.post(source.channel_id, &message).await?;
        return Ok(Some(
            "worktree is dirty — confirm before running".to_owned(),
        ));
    }
    send_now(ctx, action, source).await
}

/// Execute phase for `v1:runany:<id>`: the explicit dirty-tree confirmation.
pub async fn execute_runany(
    ctx: &Arc<AppContext>,
    action_id: i64,
    source: &InteractionSource,
) -> Result<Option<String>, AppError> {
    let confirmed = ctx.store.mark_include_existing(action_id).await?;
    if !confirmed {
        return Err(AppError::AlreadyHandled);
    }
    let action = load_staged(ctx, action_id).await?;
    send_now(ctx, action, source).await
}

/// Execute phase for `v1:editgo:<id>`: restage the prompt, then send.
pub async fn execute_editgo(
    ctx: &Arc<AppContext>,
    action_id: i64,
    source: &InteractionSource,
) -> Result<Option<String>, AppError> {
    let prompt = source.input("prompt").map(str::trim).unwrap_or_default();
    if prompt.is_empty() {
        return Err(AppError::Other("a prompt is required".to_owned()));
    }
    let restaged = ctx.store.restage_prompt(action_id, prompt).await?;
    if !restaged {
        return Err(AppError::AlreadyHandled);
    }
    let action = load_staged(ctx, action_id).await?;
    send_now(ctx, action, source).await
}

/// Send the staged action: create (or reuse) a session, CAS staged→running
/// with the lock, admit the prompt verbatim (P13), react ⏳ on the preview.
async fn send_now(
    ctx: &Arc<AppContext>,
    action: ActionRow,
    _source: &InteractionSource,
) -> Result<Option<String>, AppError> {
    let directory = run_directory(ctx, action.run_id).await?;
    let dir_text = directory.to_string_lossy().into_owned();

    if let Some(holder) = ctx.store.lock_holder(&dir_text).await?
        && holder != action.id.to_string()
    {
        return Ok(Some(format!(
            "directory is locked by {holder} (P5); /status shows the holder"
        )));
    }

    let session_id = if let Some(session) = action.session_id.clone() {
        session
    } else {
        let session = ctx
            .opencode
            .create_session(
                &directory,
                Some(&action.label),
                ctx.permission_ruleset.as_deref(),
            )
            .await?;
        ctx.store.upsert_seen(&session, Some(&dir_text)).await?;
        session
    };

    let start_head = if ctx.git.is_repo(&directory).await? {
        ctx.git.head(&directory).await.ok()
    } else {
        None
    };
    let dirty =
        action.include_existing || action.dirty_at_start || tree_dirty(ctx, &directory).await?;

    let started = ctx
        .store
        .begin_send(
            action.id,
            &dir_text,
            &session_id,
            start_head.as_deref(),
            dirty,
        )
        .await?;
    if !started {
        ctx.metrics.inc_cas_conflict();
        return Err(AppError::AlreadyHandled);
    }
    ctx.metrics.action_entered(State::Running);

    if let Err(error) = ctx
        .opencode
        .send_prompt(&session_id, &action.prompt_sent)
        .await
    {
        ctx.store
            .fail_send(action.id, &dir_text, &error.to_string())
            .await?;
        ctx.metrics.action_entered(State::Errored);
        return Err(AppError::OpenCode(error));
    }

    if let (Some(origin), Some(channel)) = (action.origin_msg_id.clone(), action.channel_id) {
        let _ = ctx
            .discord
            .react(
                u64::try_from(channel).unwrap_or(u64::MAX),
                origin.parse().unwrap_or_default(),
                '⏳',
            )
            .await;
    }

    Ok(Some(
        "started — the report arrives when it finishes".to_owned(),
    ))
}

/// Load a staged action or report that the state machine already moved.
async fn load_staged(ctx: &Arc<AppContext>, action_id: i64) -> Result<ActionRow, AppError> {
    let action = ctx
        .store
        .action(action_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("action {action_id}")))?;
    if action.state != State::Staged {
        return Err(AppError::AlreadyHandled);
    }
    Ok(action)
}

/// The directory an action's run targets.
pub(crate) async fn run_directory(ctx: &Arc<AppContext>, run_id: i64) -> Result<PathBuf, AppError> {
    let run = ctx
        .store
        .run(run_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("run {run_id}")))?;
    Ok(PathBuf::from(run.dir))
}

/// Render `templates/fresh.md` with the label/git slots (P13).
async fn prompt_for(
    ctx: &Arc<AppContext>,
    directory: &Path,
    label: &str,
) -> Result<String, AppError> {
    let (git_log, git_status) = if ctx.git.is_repo(directory).await? {
        let git_log = match ctx.git.log(directory, None).await {
            Ok(rows) => rows
                .iter()
                .take(GIT_LOG_ROWS)
                .map(|row| {
                    let short = row.sha.chars().take(8).collect::<String>();
                    format!("* {short} {}", row.subject)
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Err(_) => "(git log unavailable)".to_owned(),
        };
        let git_status = match ctx.git.status_porcelain(directory).await {
            Ok(status) if !status.trim().is_empty() => status
                .lines()
                .take(GIT_STATUS_LINES)
                .map(|line| format!("* {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
            _ => "(clean)".to_owned(),
        };
        (git_log, git_status)
    } else {
        ("(not a git repository)".to_owned(), String::new())
    };

    let mut values: HashMap<&'static str, String> = HashMap::new();
    values.insert("label", label.to_owned());
    values.insert("plan_path", String::new());
    values.insert("git_log", git_log);
    values.insert("git_status", git_status);
    templates::render(&ctx.templates.fresh, &values)
        .map_err(|error| AppError::Other(format!("template render: {error}")))
}

/// Whether the worktree has uncommitted changes (FR-27).
pub(crate) async fn tree_dirty(ctx: &Arc<AppContext>, directory: &Path) -> Result<bool, AppError> {
    if !ctx.git.is_repo(directory).await? {
        return Ok(false);
    }
    let status = ctx.git.status_porcelain(directory).await?;
    Ok(!status.trim().is_empty())
}

/// Canonicalize a path (rejects nonexistent or non-absolute inputs).
fn realpath(path: &Path) -> std::io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(std::io::Error::other("path must be absolute"));
    }
    path.canonicalize()
}
