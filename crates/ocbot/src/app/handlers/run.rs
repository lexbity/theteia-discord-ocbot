//! The `/run` flow (spec FR-6, completed in Phase 6): directory → optional plan
//! → item picker → rendered-prompt review (Edit & send / Send) → staged action.
//! The plan is read-only input parsed fresh at each open (P12); a pick only
//! prefills the label. The dirty-tree warning with its "Run anyway" button
//! (FR-27) and the review-time fresh/resume template toggle (P13) live here too.
//!
//! Plan/execute split: the modal-returning plans do one fast SQLite read each;
//! every execute pass happens after a deferral ack.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ocbot_core::plan::{PickOption, parse_plan};
use ocbot_core::ports::Activity;
use ocbot_core::render::plan_picker_message;
use ocbot_core::templates;
use ocbot_core::{ButtonKind, OutgoingMessage, State, component_button};

use crate::adapters::store::ActionRow;
use crate::app::context::AppContext;
use crate::app::error::AppError;
use crate::app::interaction::{InputStyle, InteractionSource, ModalInputSpec, ModalSpec};

/// The new-run modal `custom_id`.
pub const NEW_RUN_MODAL: &str = "v1:newrun";
/// The plan "Other" modal `custom_id` prefix.
pub const PLAN_OTHER_MODAL: &str = "v1:planothergo:";
/// Maximum git-log rows in the `{git_log}` slot.
const GIT_LOG_ROWS: usize = 15;
/// Maximum git-status lines in the `{git_status}` slot.
const GIT_STATUS_LINES: usize = 15;

/// Plan phase for `/run`: the new-run modal, directory prefilled from history
/// and the plan field from the user's draft path (a browser selection or an
/// uploaded attachment, P11).
pub async fn begin_modal(
    ctx: &Arc<AppContext>,
    source: &InteractionSource,
) -> Result<ModalSpec, AppError> {
    let prefill = match ctx.store.ui_cwd(&source.user_id).await? {
        Some(cwd) => cwd,
        None => ctx
            .store
            .recent_dirs(1)
            .await?
            .first()
            .cloned()
            .unwrap_or_default(),
    };
    // Only a *file* draft (an upload or a selected file) is a plausible plan;
    // a directory selection is meant for the plan field only after `/ls` drills
    // in, so it is not prefilled here.
    let plan_prefill = ctx
        .store
        .draft_path(&source.user_id)
        .await?
        .filter(|path| Path::new(path).is_file());
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
            ModalInputSpec {
                id: "plan".to_owned(),
                label: "Plan (optional .md)".to_owned(),
                style: InputStyle::Short,
                value: plan_prefill,
                required: false,
            },
        ],
    })
}

/// Execute phase for the `v1:newrun` modal: validate inputs, resolve a plan when
/// given (containment re-checked at use, FR-35), stage the action, and either
/// post the plan picker or the rendered preview.
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
        ctx.store.ui_cwd(&source.user_id).await?.unwrap_or_default()
    } else {
        typed.to_owned()
    };
    if directory.is_empty() {
        return Err(AppError::Other("a directory is required".to_owned()));
    }
    let canonical = ctx.fs.resolve(Path::new(&directory)).await?;
    let dir_name = canonical.to_string_lossy().into_owned();

    // The plan path is optional and strictly resolved through the fs port, so a
    // refusal happens before any DB write (zero side effects).
    let plan_input = source.input("plan").map(str::trim).unwrap_or_default();
    let plan_path = if plan_input.is_empty() {
        None
    } else {
        Some(
            ctx.fs
                .resolve(Path::new(plan_input))
                .await?
                .to_string_lossy()
                .into_owned(),
        )
    };
    // Read and parse the plan *before* any DB write: a containment/read failure
    // must leave zero side effects (FR-35).
    let plan_options = match &plan_path {
        Some(path) => load_plan(ctx, path).await?,
        None => Vec::new(),
    };

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

    let prompt = prompt_for(ctx, &canonical, label, plan_path.as_deref(), "fresh").await?;
    let run_id = ctx
        .store
        .insert_run_with_plan(&dir_name, "free", plan_path.as_deref())
        .await?;
    let chat_ref = source.destination.chat_ref.0.clone();
    let action_id = ctx
        .store
        .insert_action(run_id, Some(&chat_ref), label, &prompt, 1)
        .await?;
    ctx.store
        .set_action_destination(action_id, source.destination.surface.as_str(), &chat_ref)
        .await?;
    ctx.store.set_ui_cwd(&source.user_id, &dir_name).await?;
    ctx.store
        .audit(&source.user_id, "run", Some(action_id), "staged")
        .await?;

    if plan_path.is_some() && !plan_options.is_empty() {
        let picker = plan_picker_message(
            label,
            &plan_options,
            0,
            action_id,
            ocbot_core::mentionable_id(&source.user_id),
        );
        let receipt = ctx.chat.post(&source.destination, &picker).await?;
        ctx.store.set_origin(action_id, &receipt.msg_id.0).await?;
        return Ok(Some("pick a plan step (or Other: type it)".to_owned()));
    }

    show_preview(ctx, source, action_id, label, &prompt, "fresh", &canonical).await?;
    Ok(Some(
        "staged — choose **Send as-is** or **Edit & send**".to_owned(),
    ))
}

/// Execute a new run with pre-filled directory and plan path (SPEC-005).
/// This is used by the project-centric interface where the user selects a
/// project and plan from the project browser, not from modal inputs.
pub async fn execute_newrun_with_presets(
    ctx: &Arc<AppContext>,
    source: &InteractionSource,
    directory: &str,
    plan_path: Option<&str>,
    label_override: Option<String>,
) -> Result<Option<String>, AppError> {
    // Use the pre-filled label if provided, otherwise get from modal input
    let label = label_override.unwrap_or_else(|| {
        source
            .input("label")
            .map(str::trim)
            .unwrap_or_default()
            .to_owned()
    });

    if label.is_empty() {
        return Err(AppError::Other("a label is required".to_owned()));
    }

    if directory.is_empty() {
        return Err(AppError::Other("a directory is required".to_owned()));
    }

    // Use the pre-filled directory (already validated by project selection)
    let canonical = ctx.fs.resolve(Path::new(directory)).await?;
    let dir_name = canonical.to_string_lossy().into_owned();

    // Use the pre-filled plan path if provided
    let plan_path_resolved = if let Some(path) = plan_path {
        // The plan path from project selection is already validated
        Some(
            ctx.fs
                .resolve(Path::new(path))
                .await?
                .to_string_lossy()
                .into_owned(),
        )
    } else {
        None
    };

    // Read and parse the plan *before* any DB write: a containment/read failure
    // must leave zero side effects (FR-35).
    let plan_options = match &plan_path_resolved {
        Some(path) => load_plan(ctx, path).await?,
        None => Vec::new(),
    };

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

    let prompt = prompt_for(
        ctx,
        &canonical,
        &label,
        plan_path_resolved.as_deref(),
        "fresh",
    )
    .await?;
    let run_id = ctx
        .store
        .insert_run_with_plan(&dir_name, "free", plan_path_resolved.as_deref())
        .await?;
    let chat_ref = source.destination.chat_ref.0.clone();
    let action_id = ctx
        .store
        .insert_action(run_id, Some(&chat_ref), &label, &prompt, 1)
        .await?;
    ctx.store
        .set_action_destination(action_id, source.destination.surface.as_str(), &chat_ref)
        .await?;
    ctx.store.set_ui_cwd(&source.user_id, &dir_name).await?;
    ctx.store
        .audit(&source.user_id, "run", Some(action_id), "staged")
        .await?;

    if plan_path_resolved.is_some() && !plan_options.is_empty() {
        let picker = plan_picker_message(
            &label,
            &plan_options,
            0,
            action_id,
            ocbot_core::mentionable_id(&source.user_id),
        );
        let receipt = ctx.chat.post(&source.destination, &picker).await?;
        ctx.store.set_origin(action_id, &receipt.msg_id.0).await?;
        return Ok(Some("pick a plan step (or Other: type it)".to_owned()));
    }

    show_preview(ctx, source, action_id, &label, &prompt, "fresh", &canonical).await?;
    Ok(Some(
        "staged — choose **Send as-is** or **Edit & send**".to_owned(),
    ))
}

/// Execute `v1:plan:<id>:<idx>`: prefill the label from a parsed plan item and
/// re-render the preview (the plan is re-read and re-checked at this use, P12).
pub async fn plan_pick(
    ctx: &Arc<AppContext>,
    action_id: i64,
    index: usize,
    source: &InteractionSource,
) -> Result<Option<String>, AppError> {
    let action = load_staged(ctx, action_id).await?;
    let (dir, plan_path) = plan_context(ctx, &action).await?;
    let dir_path = Path::new(&dir);
    let options = load_plan(ctx, &plan_path).await?;
    let option: &PickOption = options
        .get(index)
        .ok_or_else(|| AppError::Other(format!("plan item {index} is gone")))?;
    let prompt = prompt_for(
        ctx,
        dir_path,
        &option.label,
        Some(&plan_path),
        &action.template,
    )
    .await?;
    if !ctx
        .store
        .restage_label_prompt(action_id, &option.label, &prompt)
        .await?
    {
        return Err(AppError::AlreadyHandled);
    }
    show_preview(
        ctx,
        source,
        action_id,
        &option.label,
        &prompt,
        &action.template,
        dir_path,
    )
    .await?;
    Ok(Some("step selected — review and send".to_owned()))
}

/// Execute `v1:planp:<id>:<n>`: re-render the picker at another page (edits do
/// not notify, P10).
pub async fn plan_page_edit(
    ctx: &Arc<AppContext>,
    action_id: i64,
    page: usize,
    source: &InteractionSource,
) -> Result<(), AppError> {
    let action = load_staged(ctx, action_id).await?;
    let (_, plan_path) = plan_context(ctx, &action).await?;
    let options = load_plan(ctx, &plan_path).await?;
    let picker = plan_picker_message(
        &action.label,
        &options,
        page,
        action_id,
        ocbot_core::mentionable_id(&source.user_id),
    );
    if let Some(receipt) = source.receipt() {
        ctx.chat.edit(&receipt, &picker).await;
    } else {
        ctx.chat.post(&source.destination, &picker).await?;
    }
    Ok(())
}

/// Plan phase for `v1:planother:<id>`: a modal to type the step by hand.
pub fn plan_other_modal(action_id: i64) -> ModalSpec {
    ModalSpec {
        custom_id: format!("{PLAN_OTHER_MODAL}{action_id}"),
        title: "Plan step".to_owned(),
        inputs: vec![ModalInputSpec {
            id: "step".to_owned(),
            label: "Step".to_owned(),
            style: InputStyle::Short,
            value: None,
            required: true,
        }],
    }
}

/// Execute the "Other" plan modal: use the typed text as the label.
pub async fn execute_plan_other(
    ctx: &Arc<AppContext>,
    action_id: i64,
    source: &InteractionSource,
) -> Result<Option<String>, AppError> {
    let step = source.input("step").map(str::trim).unwrap_or_default();
    if step.is_empty() {
        return Err(AppError::Other("a step is required".to_owned()));
    }
    let action = load_staged(ctx, action_id).await?;
    let (dir, plan_path) = plan_context(ctx, &action).await?;
    let dir_path = Path::new(&dir);
    let prompt = prompt_for(ctx, dir_path, step, Some(&plan_path), &action.template).await?;
    if !ctx
        .store
        .restage_label_prompt(action_id, step, &prompt)
        .await?
    {
        return Err(AppError::AlreadyHandled);
    }
    show_preview(
        ctx,
        source,
        action_id,
        step,
        &prompt,
        &action.template,
        dir_path,
    )
    .await?;
    Ok(Some("step set — review and send".to_owned()))
}

/// Execute `v1:planuse:<id>`: keep the typed label and show the preview.
pub async fn plan_use(
    ctx: &Arc<AppContext>,
    action_id: i64,
    source: &InteractionSource,
) -> Result<Option<String>, AppError> {
    let action = load_staged(ctx, action_id).await?;
    let dir = run_directory(ctx, action.run_id).await?;
    let dir_path = Path::new(&dir);
    show_preview(
        ctx,
        source,
        action_id,
        &action.label,
        &action.prompt_sent,
        &action.template,
        dir_path,
    )
    .await?;
    Ok(Some("staged — choose Send as-is or Edit & send".to_owned()))
}

/// Execute `v1:tpl:<id>`: flip the prompt envelope between fresh and resume,
/// re-rendering the git slots at review time (P13).
pub async fn toggle_template(
    ctx: &Arc<AppContext>,
    action_id: i64,
    source: &InteractionSource,
) -> Result<Option<String>, AppError> {
    let action = load_staged(ctx, action_id).await?;
    let next = if action.template == "resume" {
        "fresh"
    } else {
        "resume"
    };
    let dir = run_directory(ctx, action.run_id).await?;
    let dir_path = Path::new(&dir);
    let run = ctx.store.run(action.run_id).await?;
    let plan_path = run.and_then(|run| run.plan_path);
    let prompt = prompt_for(ctx, dir_path, &action.label, plan_path.as_deref(), next).await?;
    if !ctx
        .store
        .set_template_prompt(action_id, next, &prompt)
        .await?
    {
        return Err(AppError::AlreadyHandled);
    }
    show_preview(
        ctx,
        source,
        action_id,
        &action.label,
        &prompt,
        next,
        dir_path,
    )
    .await?;
    Ok(Some(format!("template: {next}")))
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
            mention_allowlist: ocbot_core::mention_allowlist(&source.user_id),
        };
        ctx.chat.post(&source.destination, &message).await?;
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
    launch(ctx, action, None).await
}

/// The shared launch: bind the action to a session (creating one when
/// `session_override` and the action's own session are absent, FR-31 passes
/// `location.directory` at create), take the directory lock atomically (P5),
/// and admit the prompt.
pub async fn launch(
    ctx: &Arc<AppContext>,
    action: ActionRow,
    session_override: Option<String>,
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

    let has_bound_session = action.session_id.is_some();
    let reused_session = session_override.is_some() || has_bound_session;
    let session_id = if let Some(session) = session_override.or_else(|| action.session_id.clone()) {
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
        ctx.store.set_active_session(&dir_text, &session).await?;
        session
    };

    // FR-9: never admit into a session that is busy *reusing* it (/prompt,
    // adoption). A freshly created session is idle by construction, so the
    // check only guards reused sessions.
    if reused_session && ctx.opencode.activity(&session_id).await? == Activity::Busy {
        return Ok(Some(format!(
            "session {session_id} is busy (FR-9); /abort to stop it or wait"
        )));
    }

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

    if let Some(origin) = action.origin_msg_id.clone() {
        ctx.chat
            .react(
                &crate::app::destination::receipt_for(&action, &origin)?,
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

/// The run directory and plan path for a staged plan action.
async fn plan_context(
    ctx: &Arc<AppContext>,
    action: &ActionRow,
) -> Result<(PathBuf, String), AppError> {
    let dir = run_directory(ctx, action.run_id).await?;
    let run = ctx
        .store
        .run(action.run_id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("run {}", action.run_id)))?;
    let plan_path = run
        .plan_path
        .ok_or_else(|| AppError::Other("this run has no plan".to_owned()))?;
    Ok((dir, plan_path))
}

/// Resolve, read, and parse a plan (containment re-checked at use, FR-35/P12).
async fn load_plan(ctx: &Arc<AppContext>, plan_path: &str) -> Result<Vec<PickOption>, AppError> {
    let resolved = ctx.fs.resolve(Path::new(plan_path)).await?;
    let text = ctx.fs.read_text(&resolved).await?;
    Ok(parse_plan(
        &text,
        ctx.plan_heading_min,
        ctx.plan_heading_max,
    ))
}

/// Post (or edit in place) the staged preview and remember its origin.
/// For project-centric runs, this uses the project name in the preview.
async fn show_preview(
    ctx: &Arc<AppContext>,
    source: &InteractionSource,
    action_id: i64,
    label: &str,
    prompt: &str,
    template: &str,
    directory: &Path,
) -> Result<(), AppError> {
    // Check if this directory is under projects_root (SPEC-005)
    let is_project_run = directory.starts_with(&ctx.projects_root);

    let preview = if is_project_run {
        // Project-centric preview: don't show prompt, show project name
        let project_name = ocbot_core::paths::project_display_name(directory);
        ocbot_core::render::projects::staged_preview_project(
            label,
            prompt,
            action_id,
            ocbot_core::mentionable_id(&source.user_id),
            template,
            &project_name,
        )
    } else {
        // Legacy preview: show prompt in code block
        ocbot_core::render::staged_preview(
            label,
            prompt,
            action_id,
            ocbot_core::mentionable_id(&source.user_id),
            template,
            None,
        )
    };

    if let Some(receipt) = source.receipt() {
        ctx.chat.edit(&receipt, &preview).await;
    } else {
        let receipt = ctx.chat.post(&source.destination, &preview).await?;
        ctx.store.set_origin(action_id, &receipt.msg_id.0).await?;
    }
    Ok(())
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

/// Render a prompt envelope (`fresh.md` or `resume.md`) with the label, plan
/// path, and git slots (P13).
async fn prompt_for(
    ctx: &Arc<AppContext>,
    directory: &Path,
    label: &str,
    plan_path: Option<&str>,
    template: &str,
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
    values.insert(
        "plan_path",
        plan_path.map_or_else(String::new, |path| format!("Plan: {path}")),
    );
    values.insert("git_log", git_log);
    values.insert("git_status", git_status);
    let source = if template == "resume" {
        &ctx.templates.resume
    } else {
        &ctx.templates.fresh
    };
    templates::render(source, &values)
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
pub(crate) fn realpath(path: &Path) -> std::io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(std::io::Error::other("path must be absolute"));
    }
    path.canonicalize()
}
