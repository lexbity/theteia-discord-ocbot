//! The SQLite store actor (spec P15/P18): one connection on a dedicated OS
//! thread behind a message-passing handle. Single writer ⇒ CAS transitions
//! (`rows_changed`) are race-free; the actor abstracts rusqlite so the rest of
//! the bot never holds a lock across an `.await`.
//!
//! Migration v1 creates the full §6.3 schema. One deliberate Phase 2 delta is
//! recorded below: `action.channel_id` — the bot must know where to deliver a
//! report (P10 says reports are new messages in the originating channel).

use std::path::Path;
use std::sync::Arc;

use crossbeam_channel::{Receiver, Select, Sender, unbounded};
use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::oneshot;

use ocbot_core::{Column, State, WriteValue};

use super::now_unix;

/// Errors surfaced by the store actor.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A SQLite error.
    #[error("store: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The actor is shut down.
    #[error("store is closed")]
    Closed,
    /// The actor thread could not be spawned.
    #[error("store worker spawn: {0}")]
    Worker(String),
}

/// A project (one working directory under management).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRow {
    /// Absolute, realpath'd directory.
    pub dir: String,
    /// The work branch, when a git repo.
    pub work_branch: Option<String>,
    /// The base sha (pre-bot history), when a git repo.
    pub base_sha: Option<String>,
    /// The promptable session for the directory (FR-30).
    pub active_session_id: Option<String>,
    /// The lock holder: an action rowid or `external:<session>` (P5).
    pub lock_holder: Option<String>,
    /// When the lock was taken (unix seconds).
    pub lock_since: Option<i64>,
}

/// One action (a bot-run attempt).
#[derive(Debug, Clone, PartialEq)]
pub struct ActionRow {
    /// Row id.
    pub id: i64,
    /// Owning run.
    pub run_id: i64,
    /// The opencode session id, once created.
    pub session_id: Option<String>,
    /// The parent action (park-redo chain), when any.
    pub parent_action_id: Option<i64>,
    /// The surface reports must go to (SPEC-004 D-08).
    pub surface: String,
    /// The surface's conversation reference (NULL only for legacy
    /// non-routed rows).
    pub chat_ref: Option<String>,
    /// The operator label.
    pub label: String,
    /// The exact prompt sent (verbatim, P13).
    pub prompt_sent: String,
    /// The machine state.
    pub state: State,
    /// The blocking gate row, when blocked (Phase 4).
    pub blocked_gate_id: Option<i64>,
    /// The outcome kind (db spelling), when finished.
    pub outcome_kind: Option<String>,
    /// The full report text.
    pub report_full: Option<String>,
    /// The footer json (diff stat text, duration, tokens, cost).
    pub footer_json: Option<String>,
    /// The Discord message id of the delivered report (dedupe key, P9).
    pub report_msg_id: Option<String>,
    /// The Discord message id of the staging preview (origin).
    pub origin_msg_id: Option<String>,
    /// The HEAD at send time.
    pub start_head: Option<String>,
    /// Whether the tree was dirty at send (FR-27).
    pub dirty_at_start: bool,
    /// Whether the operator explicitly included existing changes.
    pub include_existing: bool,
    /// Write-ahead intent columns (P6).
    pub pending_commit_subject: Option<String>,
    /// Write-ahead intent column.
    pub pending_commit_body: Option<String>,
    /// Write-ahead intent column.
    pub park_branch_name: Option<String>,
    /// Write-ahead intent column.
    pub park_step: i64,
    /// The accepted commit sha.
    pub accept_sha: Option<String>,
    /// Attempt number in a park-redo chain.
    pub attempt: i64,
    /// Follow-up count.
    pub followups: i64,
    /// Send time (unix seconds).
    pub started_at: Option<i64>,
    /// Finish time (unix seconds).
    pub ended_at: Option<i64>,
    /// The last watcher-observed liveness event.
    pub last_event_at: Option<i64>,
    /// The last stall ping time (Phase 7).
    pub stall_ping_at: Option<i64>,
    /// The rendered prompt envelope (`fresh` or `resume`, P13). The review-time
    /// toggle flips this and re-renders `prompt_sent`.
    pub template: String,
}

/// A per-session log cursor (P9: cache only, never correctness).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorRow {
    /// The durable sequence.
    pub last_seq: u64,
}

/// A single run (one directory campaign that may spawn several actions).
#[derive(Debug, Clone, PartialEq)]
pub struct RunRow {
    /// Row id.
    pub id: i64,
    /// The directory.
    pub dir: String,
    /// The optional plan path (Phase 6).
    pub plan_path: Option<String>,
    /// The run kind.
    pub kind: String,
}

/// A recorded reset intent (spec §6.4 CP-R0): the write-ahead log for a
/// `reset --hard` whose `superseded/*` safety branch must exist first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResetIntentRow {
    /// Row id.
    pub id: i64,
    /// The directory being reset.
    pub dir: String,
    /// The target sha.
    pub target_sha: String,
    /// The `superseded/<ts>` safety branch.
    pub sup_branch: String,
    /// The last completed step (0 = intent recorded).
    pub step: i64,
}

/// One gate lifecycle row (spec P4): a server-side *fact* (permission or form)
/// mirrored into the DB with its Discord message and answer state. `state` is
/// `pending | answered` — the single shared state machine for both kinds.
#[derive(Debug, Clone, PartialEq)]
pub struct GateRow {
    /// Local rowid (`custom_ids` reference this, never external ids, §6.6).
    pub id: i64,
    /// The server-side gate id (`per_*` / `frm_*`).
    pub ext_id: Option<String>,
    /// The attributed (root) action, when one is running.
    pub action_id: Option<i64>,
    /// The session that owns the gate.
    pub session_id: Option<String>,
    /// The root session the gate was attributed through (FR-24).
    pub root_session_id: Option<String>,
    /// `permission` or `form`.
    pub kind: String,
    /// The verbatim wire payload (D-R5 surface-only rendering).
    pub payload_json: String,
    /// Whether the gate is interactive (renderable forms / permissions).
    pub renderable: bool,
    /// The lifecycle state.
    pub state: String,
    /// The JSON-encoded decision (`"once"`) or form answer object, when set.
    pub answer: Option<String>,
    /// The posted gate-message id (edits don't notify, P10).
    pub msg_id: Option<String>,
    /// The channel the gate message lives in.
    pub channel_id: Option<i64>,
    /// Re-ping count.
    pub pings: i64,
    /// Last re-ping time.
    pub last_ping_at: Option<i64>,
}

/// One open pending-input flow (SPEC-004 D-06): a Signal-side modal
/// replacement state machine, one row per `(surface, chat_ref)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInputRow {
    /// The surface the flow lives on.
    pub surface: String,
    /// The conversation the flow lives in.
    pub chat_ref: String,
    /// The flow name (`run`, `edit`, `cd`).
    pub flow: String,
    /// The current step (`await_prompt`, …).
    pub step: String,
    /// The persisted draft (JSON; the only crash-surviving state).
    pub draft: String,
    /// Last update (unix seconds).
    pub updated_at: i64,
    /// Expiry (unix seconds; the ops sweep consumes past-due rows).
    pub expires_at: i64,
}

/// The typed reply channel for one [`StoreRequest`].
type Reply<T> = oneshot::Sender<Result<T, StoreError>>;

/// Converts a public method argument into the owned value its [`StoreRequest`]
/// variant carries. The actor channel is `Send + 'static`, so borrowed
/// call-site arguments (`&str`, slices) are converted at the method boundary.
trait StoreArg {
    /// The owned type stored in the request variant.
    type Owned;
    /// Convert the call-site argument into its owned form.
    fn into_store_arg(self) -> Self::Owned;
}

impl StoreArg for i64 {
    type Owned = i64;
    fn into_store_arg(self) -> Self::Owned {
        self
    }
}

impl StoreArg for u64 {
    type Owned = u64;
    fn into_store_arg(self) -> Self::Owned {
        self
    }
}

impl StoreArg for bool {
    type Owned = bool;
    fn into_store_arg(self) -> Self::Owned {
        self
    }
}

impl StoreArg for State {
    type Owned = State;
    fn into_store_arg(self) -> Self::Owned {
        self
    }
}

impl StoreArg for &str {
    type Owned = String;
    fn into_store_arg(self) -> Self::Owned {
        self.to_owned()
    }
}

impl StoreArg for Option<&str> {
    type Owned = Option<String>;
    fn into_store_arg(self) -> Self::Owned {
        self.map(str::to_owned)
    }
}

impl StoreArg for Option<i64> {
    type Owned = Option<i64>;
    fn into_store_arg(self) -> Self::Owned {
        self
    }
}

impl StoreArg for &[(String, String)] {
    type Owned = Vec<(String, String)>;
    fn into_store_arg(self) -> Self::Owned {
        self.to_vec()
    }
}

impl StoreArg for &[State] {
    type Owned = Vec<String>;
    fn into_store_arg(self) -> Self::Owned {
        self.iter().map(|state| state.as_str().to_owned()).collect()
    }
}

impl StoreArg for &[(Column, WriteValue)] {
    type Owned = Vec<(Column, WriteValue)>;
    fn into_store_arg(self) -> Self::Owned {
        self.to_vec()
    }
}

/// Declares one store operation and generates its three artifacts in a single
/// place (SPEC-002 Phase 6, FR-6): the [`StoreRequest`] enum variant, its arm in
/// [`StoreRequest::dispatch`], and the public [`StoreHandle`] method. Adding an
/// operation is one declaration.
///
/// The operation body is a closure taking the actor's `&mut Connection`; the
/// actor channel is `Send + 'static`, so borrowed arguments are converted to
/// their owned forms by [`StoreArg`] at the method boundary.
macro_rules! store_method {
    (
        $enum:ident, $handle:ident;
        $(
            $(#[$meta:meta])*
            $variant:ident as $method:ident
                ( $($arg:ident : $public:ty => $owned:ty),* $(,)? )
                -> $reply:ty
                { $body:expr }
                ;
        )*
    ) => {
        /// One typed store request (SPEC-002 §4.1, FR-1/FR-2). Each variant
        /// carries its own response sender, so the actor's message set is
        /// explicit and the reply type is checked at compile time.
        enum $enum {
            $(
                $variant {
                    $($arg: $owned,)*
                    reply: Reply<$reply>,
                },
            )*
        }

        impl $enum {
            /// Dispatch one request on the actor thread and send its typed reply.
            #[allow(
                clippy::too_many_lines,
                reason = "the dispatcher is a flat, exhaustive match over the store's message set; one arm per operation keeps the whole grammar in one place"
            )]
            fn dispatch(self, connection: &mut Connection) {
                match self {
                    $(
                        $enum::$variant { $($arg,)* reply } => {
                            finish(reply, ($body)(connection))
                        }
                    )*
                }
            }
        }

        impl $handle {
            $(
                $(#[$meta])*
                pub async fn $method(
                    &self,
                    $($arg: $public),*
                ) -> Result<$reply, StoreError> {
                    self.request(|reply| $enum::$variant {
                        $($arg: $arg.into_store_arg(),)*
                        reply,
                    })
                    .await
                }
            )*
        }
    };
}

store_method! {
    StoreRequest, StoreHandle;
    /// The lock holder of a directory, if any (P5).
    LockHolder as lock_holder(dir: &str => String) -> Option<String> {
        |connection: &mut Connection| -> rusqlite::Result<Option<String>> {
            connection
                .query_row(
                    "SELECT lock_holder FROM project WHERE dir = ?1",
                    params![dir],
                    |row| row.get::<_, Option<String>>("lock_holder"),
                )
                .optional()
                .map(Option::flatten)
        }
    };

    /// Upsert a project row for `dir` (`work_branch`/`base_sha` are `None` for
    /// non-git directories).
    UpsertProject as upsert_project(dir: &str => String, work_branch: Option<&str> => Option<String>, base_sha: Option<&str> => Option<String>) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            connection
                .execute(
                    "INSERT INTO project (dir, work_branch, base_sha)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(dir) DO UPDATE SET
                        work_branch = excluded.work_branch,
                        base_sha = excluded.base_sha",
                    params![dir, work_branch, base_sha],
                )
                .map(|_| ())
        }
    };

    /// Project by directory.
    Project as project(dir: &str => String) -> Option<ProjectRow> {
        |connection: &mut Connection| -> rusqlite::Result<Option<ProjectRow>> {
            connection
                .query_row(
                    "SELECT dir, work_branch, base_sha, active_session_id, lock_holder, lock_since
                     FROM project WHERE dir = ?1",
                    params![dir],
                    project_row,
                )
                .optional()
        }
    };

    /// Every project that currently holds a lock (for `/status`).
    LockedProjects as locked_projects() -> Vec<ProjectRow> {
        |connection: &mut Connection| -> rusqlite::Result<Vec<ProjectRow>> {
            let mut statement = connection.prepare(
                "SELECT dir, work_branch, base_sha, active_session_id, lock_holder, lock_since
                 FROM project WHERE lock_holder IS NOT NULL ORDER BY dir",
            )?;
            let rows = statement
                .query_map([], project_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        }
    };

    /// Insert a run row; returns its id.
    InsertRun as insert_run(dir: &str => String, kind: &str => String) -> i64 {
        |connection: &mut Connection| -> rusqlite::Result<i64> {
            connection.execute(
                "INSERT INTO run (dir, kind, created_at) VALUES (?1, ?2, ?3)",
                params![dir, kind, now_unix()],
            )?;
            Ok(connection.last_insert_rowid())
        }
    };

    /// Insert a run row carrying an optional plan path (Phase 6).
    InsertRunWithPlan as insert_run_with_plan(dir: &str => String, kind: &str => String, plan_path: Option<&str> => Option<String>) -> i64 {
        |connection: &mut Connection| -> rusqlite::Result<i64> {
            connection.execute(
                "INSERT INTO run (dir, plan_path, kind, created_at) VALUES (?1, ?2, ?3, ?4)",
                params![dir, plan_path, kind, now_unix()],
            )?;
            Ok(connection.last_insert_rowid())
        }
    };

    /// Insert a staged action; returns its id. `chat_ref` is the surface's
    /// conversation reference (SPEC-004 D-08); the surface column keeps its
    /// 'discord' default until a second surface exists.
    InsertAction as insert_action(run_id: i64 => i64, chat_ref: Option<&str> => Option<String>, label: &str => String, prompt_sent: &str => String, attempt: i64 => i64) -> i64 {
        |connection: &mut Connection| -> rusqlite::Result<i64> {
            connection.execute(
                "INSERT INTO action (run_id, chat_ref, label, prompt_sent, state, attempt)
                 VALUES (?1, ?2, ?3, ?4, 'staged', ?5)",
                params![run_id, chat_ref, label, prompt_sent, attempt],
            )?;
            Ok(connection.last_insert_rowid())
        }
    };

    /// Insert the staged successor of a park-redo (CP-P4); returns its id.
    #[expect(
        clippy::too_many_arguments,
        reason = "one column per argument mirrors the insert; a struct would not reduce the call-site clarity"
    )]
    InsertStagedChild as insert_staged_child(run_id: i64 => i64, chat_ref: Option<&str> => Option<String>, label: &str => String, prompt_sent: &str => String, template: &str => String, attempt: i64 => i64, parent_action_id: i64 => i64) -> i64 {
        |connection: &mut Connection| -> rusqlite::Result<i64> {
            connection.execute(
                "INSERT INTO action
                    (run_id, chat_ref, label, prompt_sent, template, state, attempt, parent_action_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'staged', ?6, ?7)",
                params![
                    run_id,
                    chat_ref,
                    label,
                    prompt_sent,
                    template,
                    attempt,
                    parent_action_id
                ],
            )?;
            Ok(connection.last_insert_rowid())
        }
    };

    /// Record where an action's messages go (SPEC-004 §5.7).
    SetActionDestination as set_action_destination(action_id: i64 => i64, surface: &str => String, chat_ref: &str => String) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            connection
                .execute(
                    "UPDATE action SET surface = ?1, chat_ref = ?2 WHERE id = ?3",
                    params![surface, chat_ref, action_id],
                )
                .map(|_| ())
        }
    };

    /// A run row by id.
    Run as run(id: i64 => i64) -> Option<RunRow> {
        |connection: &mut Connection| -> rusqlite::Result<Option<RunRow>> {
            connection
                .query_row(
                    "SELECT id, dir, plan_path, kind FROM run WHERE id = ?1",
                    params![id],
                    |row| {
                        Ok(RunRow {
                            id: row.get("id")?,
                            dir: row.get("dir")?,
                            plan_path: row.get("plan_path")?,
                            kind: row.get("kind")?,
                        })
                    },
                )
                .optional()
        }
    };

    /// An action by id.
    Action as action(id: i64 => i64) -> Option<ActionRow> {
        |connection: &mut Connection| -> rusqlite::Result<Option<ActionRow>> {
            connection
                .query_row("SELECT * FROM action WHERE id = ?1", params![id], action_row)
                .optional()
        }
    };

    /// Actions currently in any of `states` (boot reconcile, P9 step 5).
    ActionsInStates as actions_in_states(states: &[State] => Vec<String>) -> Vec<ActionRow> {
        |connection: &mut Connection| -> rusqlite::Result<Vec<ActionRow>> {
            if states.is_empty() {
                return Ok(Vec::new());
            }
            let placeholders = vec!["?"; states.len()].join(",");
            let sql = format!("SELECT * FROM action WHERE state IN ({placeholders}) ORDER BY id");
            let mut statement = connection.prepare(&sql)?;
            let rows = statement
                .query_map(rusqlite::params_from_iter(states.iter()), action_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        }
    };

    /// Every action belonging to a directory, oldest first (history merge, P8).
    ActionsForDir as actions_for_dir(dir: &str => String) -> Vec<ActionRow> {
        |connection: &mut Connection| -> rusqlite::Result<Vec<ActionRow>> {
            let mut statement = connection.prepare(
                "SELECT action.* FROM action
                 JOIN run ON action.run_id = run.id
                 WHERE run.dir = ?1 ORDER BY action.id",
            )?;
            let rows = statement
                .query_map(params![dir], action_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        }
    };

    /// The most recent accepted sha in a directory, if any (P7 step 3).
    LastAcceptSha as last_accept_sha(dir: &str => String) -> Option<String> {
        |connection: &mut Connection| -> rusqlite::Result<Option<String>> {
            connection
                .query_row(
                    "SELECT action.accept_sha FROM action
                     JOIN run ON action.run_id = run.id
                     WHERE run.dir = ?1 AND action.accept_sha IS NOT NULL
                     ORDER BY action.id DESC LIMIT 1",
                    params![dir],
                    |row| row.get::<_, Option<String>>("accept_sha"),
                )
                .optional()
                .map(Option::flatten)
        }
    };

    /// The running action that owns `session_id`, if any.
    RunningForSession as running_for_session(session_id: &str => String) -> Option<ActionRow> {
        |connection: &mut Connection| -> rusqlite::Result<Option<ActionRow>> {
            connection
                .query_row(
                    "SELECT * FROM action WHERE session_id = ?1 AND state = 'running' ORDER BY id DESC LIMIT 1",
                    params![session_id],
                    action_row,
                )
                .optional()
        }
    };

    /// The running action targeting a directory, if any (the mutex owner).
    RunningActionForDir as running_action_for_dir(dir: &str => String) -> Option<ActionRow> {
        |connection: &mut Connection| -> rusqlite::Result<Option<ActionRow>> {
            connection
                .query_row(
                    "SELECT action.* FROM action
                     JOIN run ON action.run_id = run.id
                     WHERE run.dir = ?1 AND action.state = 'running'
                     ORDER BY action.id DESC LIMIT 1",
                    params![dir],
                    action_row,
                )
                .optional()
        }
    };

    /// Record the staging-preview message id on an action (seeds status origin).
    SetOrigin as set_origin(action_id: i64 => i64, origin: &str => String) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            connection
                .execute(
                    "UPDATE action SET origin_msg_id = ?1 WHERE id = ?2 AND origin_msg_id IS NULL",
                    params![origin, action_id],
                )
                .map(|_| ())
        }
    };

    /// Confirm the operator included existing dirty changes (FR-27).
    MarkIncludeExisting as mark_include_existing(action_id: i64 => i64) -> bool {
        |connection: &mut Connection| -> rusqlite::Result<bool> {
            let changed = connection.execute(
                "UPDATE action SET include_existing = 1 WHERE id = ?1 AND state = 'staged'",
                params![action_id],
            )?;
            Ok(changed == 1)
        }
    };

    /// A generic CAS: `UPDATE ... SET state=:to <writes> WHERE id=:id AND state=:from`.
    CasState as cas_state(id: i64 => i64, from: State => State, to: State => State, writes: &[(Column, WriteValue)] => Vec<(Column, WriteValue)>) -> bool {
        |connection: &mut Connection| -> rusqlite::Result<bool> {
            cas_inner(connection, id, from, to, &writes, None)
        }
    };

    /// A CAS whose `from` may be any of `from_states` (park/discard, §6.4).
    CasStateFromAny as cas_state_from_any(id: i64 => i64, from_states: &[State] => Vec<String>, to: State => State, writes: &[(Column, WriteValue)] => Vec<(Column, WriteValue)>) -> bool {
        |connection: &mut Connection| -> rusqlite::Result<bool> {
            if from_states.is_empty() {
                return Ok(false);
            }
            let placeholders = vec!["?"; from_states.len()].join(",");
            let extra = format!("state IN ({placeholders})");
            cas_inner_any(connection, id, &from_states, to, &writes, &extra)
        }
    };

    /// The report-delivery CAS (exactly-once dedupe key, P9).
    #[expect(
        clippy::too_many_arguments,
        reason = "DB row carriers; each parameter is a column"
    )]
    DeliverReport as deliver_report(id: i64 => i64, to: State => State, report_msg_id: &str => String, outcome_kind: &str => String, report_full: Option<&str> => Option<String>, footer_json: &str => String, ended_at: i64 => i64) -> bool {
        |connection: &mut Connection| -> rusqlite::Result<bool> {
            let writes = vec![
                (Column::ReportMessageId, WriteValue::Text(report_msg_id)),
                (
                    Column::ReportFull,
                    WriteValue::Text(report_full.unwrap_or_default()),
                ),
                (Column::FooterJson, WriteValue::Text(footer_json)),
                (Column::OutcomeKind, WriteValue::Text(outcome_kind)),
                (Column::EndedAt, WriteValue::Int(ended_at)),
                (Column::LastEventAt, WriteValue::Int(ended_at)),
            ];
            cas_inner(
                connection,
                id,
                State::Running,
                to,
                &writes,
                Some("report_msg_id IS NULL"),
            )
        }
    };

    /// The atomic send: staged→running with lock acquisition, session binding,
    /// and the start record all in one transaction (P5).
    BeginSend as begin_send(action_id: i64 => i64, dir: &str => String, session_id: &str => String, start_head: Option<&str> => Option<String>, dirty: bool => bool) -> bool {
        |connection: &mut Connection| -> rusqlite::Result<bool> {
            let transaction = connection.transaction()?;
            let now = now_unix();
            let writes = vec![
                (Column::SessionId, WriteValue::Text(session_id)),
                (Column::StartedAt, WriteValue::Int(now)),
                (Column::LastEventAt, WriteValue::Int(now)),
                (
                    Column::StartHead,
                    start_head.map_or(WriteValue::Null, WriteValue::Text),
                ),
                (Column::DirtyAtStart, WriteValue::Int(i64::from(dirty))),
            ];
            let changed = cas_inner(
                &transaction,
                action_id,
                State::Staged,
                State::Running,
                &writes,
                None,
            )?;
            if changed {
                // Atomic lock acquisition (P5): take the lock only while free
                // or already held by this same action id. If the lock is held
                // by someone else, the whole transition rolls back.
                let holder = action_id.to_string();
                let acquired = transaction.execute(
                    "UPDATE project SET lock_holder = ?1, lock_since = ?2
                     WHERE dir = ?3 AND (lock_holder IS NULL OR lock_holder = ?1)",
                    params![holder, now, dir],
                )?;
                if acquired == 1 {
                    transaction.commit()?;
                    Ok(true)
                } else {
                    transaction.rollback()?;
                    Ok(false)
                }
            } else {
                transaction.rollback()?;
                Ok(false)
            }
        }
    };

    /// Update `prompt_sent` on a staged action (Edit & send path).
    RestagePrompt as restage_prompt(id: i64 => i64, prompt: &str => String) -> bool {
        |connection: &mut Connection| -> rusqlite::Result<bool> {
            let changed = connection.execute(
                "UPDATE action SET prompt_sent = ?1 WHERE id = ?2 AND state = 'staged'",
                params![prompt, id],
            )?;
            Ok(changed == 1)
        }
    };

    /// Update both `label` and `prompt_sent` on a staged action (P12/P13).
    RestageLabelPrompt as restage_label_prompt(id: i64 => i64, label: &str => String, prompt: &str => String) -> bool {
        |connection: &mut Connection| -> rusqlite::Result<bool> {
            let changed = connection.execute(
                "UPDATE action SET label = ?1, prompt_sent = ?2 WHERE id = ?3 AND state = 'staged'",
                params![label, prompt, id],
            )?;
            Ok(changed == 1)
        }
    };

    /// Record the review-time template choice and the re-rendered prompt (FR-6).
    SetTemplatePrompt as set_template_prompt(id: i64 => i64, template: &str => String, prompt: &str => String) -> bool {
        |connection: &mut Connection| -> rusqlite::Result<bool> {
            let changed = connection.execute(
                "UPDATE action SET template = ?1, prompt_sent = ?2 WHERE id = ?3 AND state = 'staged'",
                params![template, prompt, id],
            )?;
            Ok(changed == 1)
        }
    };

    /// Feed the stall watchdog from the event path (SPEC-003 §4.8): a wake
    /// that found the session busy refreshes the action's liveness.
    TouchLastEvent as touch_last_event(id: i64 => i64, at: i64 => i64) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            connection
                .execute(
                    "UPDATE action SET last_event_at = ?2 WHERE id = ?1 AND state = 'running'",
                    params![id, at],
                )
                .map(|_| ())
        }
    };

    /// Record a stall ping time (P3/FR-39: one ping per stall episode).
    SetStallPing as set_stall_ping(id: i64 => i64, ts: i64 => i64) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            connection
                .execute(
                    "UPDATE action SET stall_ping_at = ?1 WHERE id = ?2",
                    params![ts, id],
                )
                .map(|_| ())
        }
    };

    /// Mark a send attempt as failed and release the directory lock.
    FailSend as fail_send(id: i64 => i64, dir: &str => String, reason: &str => String) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            let transaction = connection.transaction()?;
            let changed = transaction.execute(
                "UPDATE action SET state = 'errored', outcome_kind = 'failed-send',
                 report_full = ?1, last_event_at = ?2
                 WHERE id = ?3 AND state = 'running' AND report_msg_id IS NULL",
                params![reason, now_unix(), id],
            )?;
            if changed == 1 {
                transaction.execute(
                    "UPDATE project SET lock_holder = NULL, lock_since = NULL WHERE dir = ?1",
                    params![dir],
                )?;
            }
            transaction.commit()?;
            Ok(())
        }
    };

    /// The last selected/uploaded path for a user (browser + attachment draft).
    /// Keys are namespaced `ActorId`s (SPEC-004 D-03).
    DraftPath as draft_path(user_id: &str => String) -> Option<String> {
        |connection: &mut Connection| -> rusqlite::Result<Option<String>> {
            connection
                .query_row(
                    "SELECT draft_path FROM ui_state WHERE user_id = ?1",
                    params![user_id],
                    |row| row.get::<_, Option<String>>("draft_path"),
                )
                .optional()
                .map(Option::flatten)
        }
    };

    /// Remember the last selected/uploaded path for a user.
    SetDraftPath as set_draft_path(user_id: &str => String, path: &str => String) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            connection
                .execute(
                    "INSERT INTO ui_state (user_id, draft_path) VALUES (?1, ?2)
                     ON CONFLICT(user_id) DO UPDATE SET draft_path = excluded.draft_path",
                    params![user_id, path],
                )
                .map(|_| ())
        }
    };

    /// Forget the draft path (consumed by a prompt).
    ClearDraftPath as clear_draft_path(user_id: &str => String) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            connection
                .execute(
                    "UPDATE ui_state SET draft_path = NULL WHERE user_id = ?1",
                    params![user_id],
                )
                .map(|_| ())
        }
    };

    /// The remembered cwd for a user (modal prefill); a `set_draft_path`-only
    /// row leaves `cwd` NULL, so the read is nullable.
    UiCwd as ui_cwd(user_id: &str => String) -> Option<String> {
        |connection: &mut Connection| -> rusqlite::Result<Option<String>> {
            connection
                .query_row(
                    "SELECT cwd FROM ui_state WHERE user_id = ?1",
                    params![user_id],
                    |row| row.get::<_, Option<String>>("cwd"),
                )
                .optional()
                .map(Option::flatten)
        }
    };

    /// Remember a cwd for a user.
    SetUiCwd as set_ui_cwd(user_id: &str => String, cwd: &str => String) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            connection
                .execute(
                    "INSERT INTO ui_state (user_id, cwd) VALUES (?1, ?2)
                     ON CONFLICT(user_id) DO UPDATE SET cwd = excluded.cwd",
                    params![user_id, cwd],
                )
                .map(|_| ())
        }
    };

    /// Mark a session as seen (Phase 5 inbox bookkeeping, seeded from Phase 2).
    UpsertSeen as upsert_seen(session_id: &str => String, directory: Option<&str> => Option<String>) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            connection
                .execute(
                    "INSERT INTO session_seen (session_id, dir, first_seen) VALUES (?1, ?2, ?3)
                     ON CONFLICT(session_id) DO NOTHING",
                    params![session_id, directory, now_unix()],
                )
                .map(|_| ())
        }
    };

    /// Set the log cursor for a session (P9 cache only).
    UpsertCursor as upsert_cursor(session_id: &str => String, seq: u64 => u64) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            connection
                .execute(
                    "INSERT INTO session_cursor (session_id, last_seq, updated_at) VALUES (?1, ?2, ?3)
                     ON CONFLICT(session_id) DO UPDATE SET last_seq = excluded.last_seq, updated_at = excluded.updated_at",
                    params![session_id, to_i64(seq), now_unix()],
                )
                .map(|_| ())
        }
    };

    /// The logged cursor for a session, if any.
    Cursor as cursor(session_id: &str => String) -> Option<CursorRow> {
        |connection: &mut Connection| -> rusqlite::Result<Option<CursorRow>> {
            connection
                .query_row(
                    "SELECT last_seq FROM session_cursor WHERE session_id = ?1",
                    params![session_id],
                    |row| {
                        row.get::<_, i64>("last_seq").map(|seq| CursorRow {
                            last_seq: to_u64(seq),
                        })
                    },
                )
                .optional()
        }
    };

    /// The most recent directories (for `/run` modal prefill).
    RecentDirs as recent_dirs(limit: u64 => u64) -> Vec<String> {
        |connection: &mut Connection| -> rusqlite::Result<Vec<String>> {
            let mut statement = connection.prepare(
                "SELECT dir FROM project ORDER BY (
                    SELECT MAX(created_at) FROM run WHERE run.dir = project.dir
                 ) DESC LIMIT ?1",
            )?;
            let rows = statement
                .query_map(params![to_i64(limit)], |row| row.get::<_, String>("dir"))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        }
    };

    /// Record an audit line (FR-5). The actor is a namespaced `ActorId` string
    /// (SPEC-004 D-03).
    Audit as audit(user_id: &str => String, verb: &str => String, action_id: Option<i64> => Option<i64>, result: &str => String) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            connection
                .execute(
                    "INSERT INTO audit (ts, user_id, verb, action_id, result) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![now_unix(), user_id, verb, action_id, result],
                )
                .map(|_| ())
        }
    };

    /// Number of actions per state (metrics feed).
    ActionCountsByState as action_counts_by_state() -> Vec<(String, i64)> {
        |connection: &mut Connection| -> rusqlite::Result<Vec<(String, i64)>> {
            let mut statement =
                connection.prepare("SELECT state, COUNT(*) AS n FROM action GROUP BY state")?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>("state")?, row.get::<_, i64>("n")?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        }
    };

    /// Record a reset intent (CP-R0) and return its id.
    RecordResetIntent as record_reset_intent(dir: &str => String, target_sha: &str => String, sup_branch: &str => String) -> i64 {
        |connection: &mut Connection| -> rusqlite::Result<i64> {
            connection.execute(
                "INSERT INTO reset_intent (dir, target_sha, sup_branch, step, created_at)
                 VALUES (?1, ?2, ?3, 0, ?4)",
                params![dir, target_sha, sup_branch, now_unix()],
            )?;
            Ok(connection.last_insert_rowid())
        }
    };

    /// Every unfinished reset intent (boot reconcile, P9 step 5).
    PendingResetIntents as pending_reset_intents() -> Vec<ResetIntentRow> {
        |connection: &mut Connection| -> rusqlite::Result<Vec<ResetIntentRow>> {
            let mut statement = connection.prepare(
                "SELECT id, dir, target_sha, sup_branch, step FROM reset_intent ORDER BY id",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok(ResetIntentRow {
                        id: row.get("id")?,
                        dir: row.get("dir")?,
                        target_sha: row.get("target_sha")?,
                        sup_branch: row.get("sup_branch")?,
                        step: row.get("step")?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        }
    };

    /// Advance a reset intent's step (write-ahead; the intent row survives a
    /// crash so the next boot rolls forward).
    SetResetStep as set_reset_step(id: i64 => i64, step: i64 => i64) -> bool {
        |connection: &mut Connection| -> rusqlite::Result<bool> {
            let changed = connection.execute(
                "UPDATE reset_intent SET step = ?1 WHERE id = ?2",
                params![step, id],
            )?;
            Ok(changed == 1)
        }
    };

    /// Remove a completed reset intent.
    DeleteResetIntent as delete_reset_intent(id: i64 => i64) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            connection
                .execute("DELETE FROM reset_intent WHERE id = ?1", params![id])
                .map(|_| ())
        }
    };

    /// Mirror a server-side gate fact into the DB (idempotent by `ext_id`).
    #[expect(
        clippy::too_many_arguments,
        reason = "gate columns are a fixed set of carriers; a builder would obscure the insert shape"
    )]
    UpsertGate as upsert_gate(ext_id: &str => String, action_id: Option<i64> => Option<i64>, session_id: Option<&str> => Option<String>, root_session_id: Option<&str> => Option<String>, kind: &str => String, payload_json: &str => String, renderable: bool => bool, channel_id: Option<i64> => Option<i64>, now: i64 => i64) -> (i64, bool) {
        |connection: &mut Connection| -> rusqlite::Result<(i64, bool)> {
            let transaction = connection.transaction()?;
            let existing: Option<i64> = transaction
                .query_row(
                    "SELECT id FROM gate WHERE ext_id = ?1",
                    params![ext_id],
                    |row| row.get("id"),
                )
                .optional()?;
            if let Some(id) = existing {
                transaction.commit()?;
                Ok((id, false))
            } else {
                let renderable = i64::from(renderable);
                transaction.execute(
                    "INSERT INTO gate
                            (ext_id, action_id, session_id, root_session_id, kind, payload_json,
                             renderable, state, channel_id, last_ping_at)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8, ?9)",
                    params![
                        ext_id,
                        action_id,
                        session_id,
                        root_session_id,
                        kind,
                        payload_json,
                        renderable,
                        channel_id,
                        now
                    ],
                )?;
                let id = transaction.last_insert_rowid();
                transaction.commit()?;
                Ok((id, true))
            }
        }
    };

    /// A gate row by local id.
    Gate as gate(id: i64 => i64) -> Option<GateRow> {
        |connection: &mut Connection| -> rusqlite::Result<Option<GateRow>> {
            connection
                .query_row("SELECT * FROM gate WHERE id = ?1", params![id], gate_row)
                .optional()
        }
    };

    /// Every pending gate row (the re-ping cursor).
    PendingGates as pending_gates() -> Vec<GateRow> {
        |connection: &mut Connection| -> rusqlite::Result<Vec<GateRow>> {
            let mut statement =
                connection.prepare("SELECT * FROM gate WHERE state = 'pending' ORDER BY id")?;
            let rows = statement
                .query_map([], gate_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        }
    };

    /// The single gate state transition (P4). `answer` is the JSON-encoded
    /// decision/answer.
    CasGate as cas_gate(id: i64 => i64, from: &str => String, to: &str => String, answer: Option<&str> => Option<String>) -> bool {
        |connection: &mut Connection| -> rusqlite::Result<bool> {
            let changed = connection.execute(
                "UPDATE gate SET state = ?1, answer = ?2 WHERE id = ?3 AND state = ?4",
                params![to, answer, id, from],
            )?;
            Ok(changed == 1)
        }
    };

    /// Record the posted gate message (P10: already-posted gates are never
    /// re-posted; the nullable guard is the dedupe).
    SetGateMsg as set_gate_msg(id: i64 => i64, msg_id: &str => String, channel_id: Option<i64> => Option<i64>) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            connection
                .execute(
                    "UPDATE gate SET msg_id = ?1, channel_id = ?2 WHERE id = ?3 AND msg_id IS NULL",
                    params![msg_id, channel_id, id],
                )
                .map(|_| ())
        }
    };

    /// Advance the re-ping counter and stamp the ping time; returns the new
    /// count (the caller decides the message after the 6th ping).
    BumpGatePings as bump_gate_pings(id: i64 => i64, now: i64 => i64) -> i64 {
        |connection: &mut Connection| -> rusqlite::Result<i64> {
            connection.execute(
                "UPDATE gate SET pings = pings + 1, last_ping_at = ?1 WHERE id = ?2",
                params![now, id],
            )?;
            let pings: i64 = connection.query_row(
                "SELECT pings FROM gate WHERE id = ?1",
                params![id],
                |row| row.get("pings"),
            )?;
            Ok(pings)
        }
    };

    /// Every answered gate with a recorded answer (boot reconcile re-POSTs any
    /// that are still pending on the server, FR-23).
    AnsweredGates as answered_gates() -> Vec<GateRow> {
        |connection: &mut Connection| -> rusqlite::Result<Vec<GateRow>> {
            let mut statement = connection.prepare(
                "SELECT * FROM gate WHERE state = 'answered' AND answer IS NOT NULL ORDER BY id",
            )?;
            let rows = statement
                .query_map([], gate_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        }
    };

    /// Set the blocked substatus on an action (P4: `blocked` is a substatus on
    /// `running`, never a state).
    SetActionBlocked as set_action_blocked(action_id: i64 => i64, gate_id: i64 => i64) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            connection
                .execute(
                    "UPDATE action SET blocked_gate_id = ?1 WHERE id = ?2",
                    params![gate_id, action_id],
                )
                .map(|_| ())
        }
    };

    /// Clear the blocked substatus, but only when this gate is the blocker.
    ClearActionBlocked as clear_action_blocked(action_id: i64 => i64, gate_id: i64 => i64) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            connection
                .execute(
                    "UPDATE action SET blocked_gate_id = NULL WHERE id = ?1 AND blocked_gate_id = ?2",
                    params![action_id, gate_id],
                )
                .map(|_| ())
        }
    };

    /// Record the promptable session for a directory (FR-30).
    SetActiveSession as set_active_session(dir: &str => String, session_id: &str => String) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            connection
                .execute(
                    "INSERT INTO project (dir, active_session_id) VALUES (?1, ?2)
                     ON CONFLICT(dir) DO UPDATE SET active_session_id = excluded.active_session_id",
                    params![dir, session_id],
                )
                .map(|_| ())
        }
    };

    /// Take the directory lock on behalf of an *external* (TUI) session (P5).
    SetExternalLock as set_external_lock(dir: &str => String, session_id: &str => String, since: i64 => i64) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            connection
                .execute(
                    "INSERT INTO project (dir, lock_holder, lock_since) VALUES (?1, ?2, ?3)
                     ON CONFLICT(dir) DO UPDATE SET lock_holder = excluded.lock_holder,
                                                    lock_since = excluded.lock_since",
                    params![dir, format!("external:{session_id}"), since],
                )
                .map(|_| ())
        }
    };

    /// Release a directory lock only if `holder` currently owns it (P5).
    ReleaseLock as release_lock(dir: &str => String, holder: &str => String) -> bool {
        |connection: &mut Connection| -> rusqlite::Result<bool> {
            let changed = connection.execute(
                "UPDATE project SET lock_holder = NULL, lock_since = NULL
                 WHERE dir = ?1 AND lock_holder = ?2",
                params![dir, holder],
            )?;
            Ok(changed == 1)
        }
    };

    /// Clear the promptable session for a directory (FR-30).
    RetireActiveSession as retire_active_session(dir: &str => String) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            connection
                .execute(
                    "UPDATE project SET active_session_id = NULL WHERE dir = ?1",
                    params![dir],
                )
                .map(|_| ())
        }
    };

    /// The directory a seen session is bound to (P14).
    SessionDir as session_dir(session_id: &str => String) -> Option<String> {
        |connection: &mut Connection| -> rusqlite::Result<Option<String>> {
            connection
                .query_row(
                    "SELECT dir FROM session_seen WHERE session_id = ?1",
                    params![session_id],
                    |row| row.get("dir"),
                )
                .optional()
        }
    };

    /// Every session the bot owns (bound to an action, or a project's
    /// promptable session) — the complement of `session_seen` is *external*.
    KnownBotSessions as known_bot_sessions() -> Vec<String> {
        |connection: &mut Connection| -> rusqlite::Result<Vec<String>> {
            let mut statement = connection.prepare(
                "SELECT session_id FROM action WHERE session_id IS NOT NULL
                 UNION
                 SELECT active_session_id FROM project WHERE active_session_id IS NOT NULL",
            )?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>("session_id"))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        }
    };

    /// Claim a completion under `(session_id, seq)` *before* posting (FR-32/P14).
    ClaimNotified as claim_notified(session_id: &str => String, seq: &str => String) -> bool {
        |connection: &mut Connection| -> rusqlite::Result<bool> {
            let changed = connection.execute(
                "INSERT OR IGNORE INTO notified (session_id, seq, receipts)
                 VALUES (?1, ?2, '')",
                params![session_id, seq],
            )?;
            Ok(changed == 1)
        }
    };

    /// Record the joined receipts (`discord:1|signal:2`, SPEC-004 D-08) on a
    /// claimed notification. The column is informational; the claim is the
    /// dedupe key and is never parsed.
    FinishNotified as finish_notified(session_id: &str => String, seq: &str => String, receipts: &str => String) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            connection
                .execute(
                    "UPDATE notified SET receipts = ?1 WHERE session_id = ?2 AND seq = ?3",
                    params![receipts, session_id, seq],
                )
                .map(|_| ())
        }
    };

    /// Give a claimed-but-unposted notification back so the next tick retries
    /// (empty receipts = never posted; semantics unchanged from v1).
    UnclaimNotified as unclaim_notified(session_id: &str => String, seq: &str => String) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            connection
                .execute(
                    "DELETE FROM notified WHERE session_id = ?1 AND seq = ?2 AND receipts = ''",
                    params![session_id, seq],
                )
                .map(|_| ())
        }
    };

    /// Record `(surface, msg_id) → session` so a reply can continue that
    /// session (P14 / FR-32; SPEC-004 D-08 namespaced PK).
    InsertMsgMap as insert_msg_map(surface: &str => String, msg_id: &str => String, session_id: &str => String, action_id: Option<i64> => Option<i64>) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            connection
                .execute(
                    "INSERT OR IGNORE INTO msg_map (surface, msg_id, session_id, action_id)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![surface, msg_id, session_id, action_id],
                )
                .map(|_| ())
        }
    };

    /// Resolve a replied-to message (on its surface) to its session.
    LookupMsgMap as lookup_msg_map(surface: &str => String, msg_id: &str => String) -> Option<(String, Option<i64>)> {
        |connection: &mut Connection| -> rusqlite::Result<Option<(String, Option<i64>)>> {
            connection
                .query_row(
                    "SELECT session_id, action_id FROM msg_map WHERE surface = ?1 AND msg_id = ?2",
                    params![surface, msg_id],
                    |row| {
                        Ok((
                            row.get::<_, String>("session_id")?,
                            row.get::<_, Option<i64>>("action_id")?,
                        ))
                    },
                )
                .optional()
        }
    };

    /// Record the reaction→custom-id map for one posted message (SPEC-004
    /// D-05; used by the Signal surface from PH-5, created in migration v5).
    InsertComponentMap as insert_component_map(surface: &str => String, msg_id: &str => String, entries: &[(String, String)] => Vec<(String, String)>) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            let now = now_unix();
            for (emoji, custom_id) in entries {
                connection.execute(
                    "INSERT OR REPLACE INTO component_map (surface, msg_id, emoji, custom_id, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![surface, msg_id, emoji, custom_id, now],
                )?;
            }
            Ok(())
        }
    };

    /// Resolve `(surface, msg_id, emoji)` to the custom id a button would have
    /// carried.
    LookupComponentMap as lookup_component_map(surface: &str => String, msg_id: &str => String, emoji: &str => String) -> Option<String> {
        |connection: &mut Connection| -> rusqlite::Result<Option<String>> {
            connection
                .query_row(
                    "SELECT custom_id FROM component_map
                     WHERE surface = ?1 AND msg_id = ?2 AND emoji = ?3",
                    params![surface, msg_id, emoji],
                    |row| row.get::<_, String>("custom_id"),
                )
                .optional()
        }
    };

    /// Open (or replace) the pending-input flow for one chat (SPEC-004 D-06:
    /// one row per `(surface, chat_ref)`). `now` comes from the app clock so
    /// the TTL is testable against the injected clock and consistent with the
    /// expiry sweep.
    #[expect(
        clippy::too_many_arguments,
        reason = "one column per argument mirrors the insert; a struct would not reduce the call-site clarity"
    )]
    PutPending as put_pending(surface: &str => String, chat_ref: &str => String, flow: &str => String, step: &str => String, draft: &str => String, ttl_secs: i64 => i64, now: i64 => i64) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            connection
                .execute(
                    "INSERT INTO pending_input (surface, chat_ref, flow, step, draft, updated_at, expires_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                     ON CONFLICT(surface, chat_ref) DO UPDATE SET
                        flow = excluded.flow,
                        step = excluded.step,
                        draft = excluded.draft,
                        updated_at = excluded.updated_at,
                        expires_at = excluded.expires_at",
                    params![surface, chat_ref, flow, step, draft, now, now + ttl_secs],
                )
                .map(|_| ())
        }
    };

    /// Consume the pending-input flow for one chat (delete + return).
    TakePending as take_pending(surface: &str => String, chat_ref: &str => String) -> Option<PendingInputRow> {
        |connection: &mut Connection| -> rusqlite::Result<Option<PendingInputRow>> {
            let row = connection
                .query_row(
                    "SELECT surface, chat_ref, flow, step, draft, updated_at, expires_at
                     FROM pending_input WHERE surface = ?1 AND chat_ref = ?2",
                    params![surface, chat_ref],
                    pending_input_row,
                )
                .optional()?;
            connection.execute(
                "DELETE FROM pending_input WHERE surface = ?1 AND chat_ref = ?2",
                params![surface, chat_ref],
            )?;
            Ok(row)
        }
    };

    /// Every pending-input rows past expiry (the ops sweep, FR-113).
    PendingExpired as pending_expired(now: i64 => i64) -> Vec<PendingInputRow> {
        |connection: &mut Connection| -> rusqlite::Result<Vec<PendingInputRow>> {
            let mut statement = connection.prepare(
                "SELECT surface, chat_ref, flow, step, draft, updated_at, expires_at
                 FROM pending_input WHERE expires_at <= ?1 ORDER BY expires_at",
            )?;
            let rows = statement
                .query_map(params![now], pending_input_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        }
    };

    /// The open pending-input flow for one chat, if any (SPEC-004 D-06).
    GetPending as get_pending(surface: &str => String, chat_ref: &str => String) -> Option<PendingInputRow> {
        |connection: &mut Connection| -> rusqlite::Result<Option<PendingInputRow>> {
            connection
                .query_row(
                    "SELECT surface, chat_ref, flow, step, draft, updated_at, expires_at
                     FROM pending_input WHERE surface = ?1 AND chat_ref = ?2",
                    params![surface, chat_ref],
                    pending_input_row,
                )
                .optional()
        }
    };

    /// Every open pending-input flow (the ops gauge, FR-113).
    AllPending as all_pending() -> Vec<PendingInputRow> {
        |connection: &mut Connection| -> rusqlite::Result<Vec<PendingInputRow>> {
            let mut statement = connection.prepare(
                "SELECT surface, chat_ref, flow, step, draft, updated_at, expires_at
                 FROM pending_input ORDER BY updated_at",
            )?;
            let rows = statement
                .query_map([], pending_input_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        }
    };

    /// Sweep expired `component_map` and `msg_map` rows (7-day retention).
    /// GC sweeps happen in the ops loop, not the adapter loop.
    /// `now` and `created_at` are in milliseconds since epoch.
    GcMaps as gc_maps(
        now: i64 => i64,
        days: i64 => i64,
    ) -> () {
        |connection: &mut Connection| -> rusqlite::Result<()> {
            let cutoff = now - (days * 24 * 3600 * 1000); // 7 days in ms
            connection.execute_batch(&format!(
                "DELETE FROM component_map WHERE created_at <= {cutoff};
                 DELETE FROM msg_map WHERE created_at <= {cutoff};"
            ))?;
            Ok(())
        }
    };

    /// The number of `component_map` rows (the GC-health gauge, §5.10).
    ComponentMapCount as component_map_count() -> u64 {
        |connection: &mut Connection| -> rusqlite::Result<u64> {
            let count: i64 = connection.query_row(
                "SELECT COUNT(*) AS n FROM component_map",
                [],
                |row| row.get("n"),
            )?;
            Ok(u64::try_from(count).unwrap_or(0))
        }
    };

    /// The number of `msg_map` rows (the GC-health gauge, §5.10).
    MsgMapCount as msg_map_count() -> u64 {
        |connection: &mut Connection| -> rusqlite::Result<u64> {
            let count: i64 = connection.query_row(
                "SELECT COUNT(*) AS n FROM msg_map",
                [],
                |row| row.get("n"),
            )?;
            Ok(u64::try_from(count).unwrap_or(0))
        }
    };

    /// The number of `action` rows (the GC-health gauge, §5.10).
    ActionCount as action_count() -> u64 {
        |connection: &mut Connection| -> rusqlite::Result<u64> {
            let count: i64 = connection.query_row(
                "SELECT COUNT(*) AS n FROM action",
                [],
                |row| row.get("n"),
            )?;
            Ok(u64::try_from(count).unwrap_or(0))
        }
    };

    /// The number of `notified` rows (the GC-health gauge, §5.10).
    NotifiedCount as notified_count() -> u64 {
        |connection: &mut Connection| -> rusqlite::Result<u64> {
            let count: i64 = connection.query_row(
                "SELECT COUNT(*) AS n FROM notified",
                [],
                |row| row.get("n"),
            )?;
            Ok(u64::try_from(count).unwrap_or(0))
        }
    };

    /// The most recent action in a chat that is in one of `states` — the bare
    /// decision verb's target (SPEC-004 §5.5 / FR-105).
    LatestAction as latest_action(surface: &str => String, chat_ref: &str => String, states: &[State] => Vec<String>) -> Option<ActionRow> {
        |connection: &mut Connection| -> rusqlite::Result<Option<ActionRow>> {
            if states.is_empty() {
                return Ok(None);
            }
            let placeholders = vec!["?"; states.len()].join(",");
            let sql = format!(
                "SELECT * FROM action
                 WHERE surface = ? AND chat_ref = ? AND state IN ({placeholders})
                 ORDER BY id DESC LIMIT 1"
            );
            let mut values: Vec<String> = vec![surface.clone(), chat_ref.clone()];
            values.extend(states.iter().cloned());
            connection
                .query_row(&sql, rusqlite::params_from_iter(values.iter()), action_row)
                .optional()
        }
    };

    /// The most recent action in a chat whose label matches, in one of
    /// `states` (the label-qualified decision form, §5.5).
    ActionByLabel as action_by_label(surface: &str => String, chat_ref: &str => String, label: &str => String, states: &[State] => Vec<String>) -> Option<ActionRow> {
        |connection: &mut Connection| -> rusqlite::Result<Option<ActionRow>> {
            if states.is_empty() {
                return Ok(None);
            }
            let placeholders = vec!["?"; states.len()].join(",");
            let sql = format!(
                "SELECT * FROM action
                 WHERE surface = ? AND chat_ref = ? AND label = ? AND state IN ({placeholders})
                 ORDER BY id DESC LIMIT 1"
            );
            let mut values: Vec<String> = vec![surface.clone(), chat_ref.clone(), label.clone()];
            values.extend(states.iter().cloned());
            connection
                .query_row(&sql, rusqlite::params_from_iter(values.iter()), action_row)
                .optional()
        }
    };

    /// Whether `kind`/`key` is muted (FR-33).
    IsMuted as is_muted(kind: &str => String, key: &str => String) -> bool {
        |connection: &mut Connection| -> rusqlite::Result<bool> {
            let muted = connection
                .query_row(
                    "SELECT 1 FROM mute WHERE kind = ?1 AND key = ?2",
                    params![kind, key],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            Ok(muted)
        }
    };

    /// Toggle a mute; returns the new state (`true` = now muted).
    ToggleMute as toggle_mute(kind: &str => String, key: &str => String) -> bool {
        |connection: &mut Connection| -> rusqlite::Result<bool> {
            let transaction = connection.transaction()?;
            let exists: Option<()> = transaction
                .query_row(
                    "SELECT 1 FROM mute WHERE kind = ?1 AND key = ?2",
                    params![kind, key],
                    |_| Ok(()),
                )
                .optional()?;
            if exists.is_some() {
                transaction.execute(
                    "DELETE FROM mute WHERE kind = ?1 AND key = ?2",
                    params![kind, key],
                )?;
                transaction.commit()?;
                Ok(false)
            } else {
                transaction.execute(
                    "INSERT INTO mute (kind, key) VALUES (?1, ?2)",
                    params![kind, key],
                )?;
                transaction.commit()?;
                Ok(true)
            }
        }
    };
}

impl StoreHandle {
    /// Actions currently in `running`.
    pub async fn running_actions(&self) -> Result<Vec<ActionRow>, StoreError> {
        self.actions_with_state(State::Running).await
    }

    /// Actions currently in `staged`.
    pub async fn staged_actions(&self) -> Result<Vec<ActionRow>, StoreError> {
        self.actions_with_state(State::Staged).await
    }

    async fn actions_with_state(&self, state: State) -> Result<Vec<ActionRow>, StoreError> {
        self.actions_in_states(&[state]).await
    }
}

/// The actor handle: cheap to clone, no direct connection access.
#[derive(Clone)]
pub struct StoreHandle {
    inner: Arc<StoreInner>,
}

struct StoreInner {
    tx: Sender<StoreRequest>,
    shutdown_tx: Sender<()>,
    done_rx: Receiver<()>,
}

impl StoreHandle {
    /// Open (creating if needed) the database at `path` and start the actor.
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let connection = Connection::open(path)?;
        Self::start(connection)
    }

    /// Open an in-memory database (tests, and the actor is thereby testable
    /// without files).
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let connection = Connection::open_in_memory()?;
        Self::start(connection)
    }

    fn start(mut connection: Connection) -> Result<Self, StoreError> {
        configure_and_migrate(&mut connection)?;
        let (tx, rx) = unbounded::<StoreRequest>();
        let (shutdown_tx, shutdown_rx) = unbounded::<()>();
        let (done_tx, done_rx) = unbounded::<()>();
        std::thread::Builder::new()
            .name("ocbot-store".to_owned())
            .spawn(move || worker_loop(&mut connection, &rx, &shutdown_rx, &done_tx))
            .map_err(|error| StoreError::Worker(error.to_string()))?;
        Ok(Self {
            inner: Arc::new(StoreInner {
                tx,
                shutdown_tx,
                done_rx,
            }),
        })
    }

    /// Send one typed request to the actor and await its typed reply. The
    /// request is constructed — with its own response sender — by `make`, so
    /// the reply type is fixed by the enum variant (FR-1/FR-2).
    async fn request<T>(
        &self,
        make: impl FnOnce(Reply<T>) -> StoreRequest,
    ) -> Result<T, StoreError> {
        let (reply, reply_rx) = oneshot::channel();
        self.inner
            .tx
            .send(make(reply))
            .map_err(|_| StoreError::Closed)?;
        reply_rx.await.map_err(|_| StoreError::Closed)?
    }

    /// Graceful drain: ask the worker to checkpoint the WAL and exit, then
    /// wait for it (SIGTERM path).
    pub async fn shutdown(&self) {
        let _ = self.inner.shutdown_tx.send(());
        let done_rx = self.inner.done_rx.clone();
        let _ = tokio::task::spawn_blocking(move || done_rx.recv()).await;
    }
}

/// Worker loop: one connection, serial requests, short transactions.
fn worker_loop(
    connection: &mut Connection,
    rx: &Receiver<StoreRequest>,
    shutdown_rx: &Receiver<()>,
    done_tx: &Sender<()>,
) {
    loop {
        let mut selection = Select::new();
        selection.recv(shutdown_rx);
        selection.recv(rx);
        let operation = selection.select();
        if operation.index() == 0 {
            // Complete the shutdown op before dropping it (crossbeam panics
            // on an uncompleted `SelectedOperation`).
            let _ = operation.recv(shutdown_rx);
            break;
        }
        match operation.recv(rx) {
            Ok(request) => request.dispatch(connection),
            Err(_) => break, // all senders gone
        }
    }
    let _ = connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    let _ = done_tx.send(());
}

/// Send one typed reply; a closed receiver just means the caller moved on.
fn finish<T>(reply: Reply<T>, result: rusqlite::Result<T>) {
    let _ = reply.send(result.map_err(StoreError::from));
}

/// PRAGMAs + schema migrations. Runs exactly once per connection.
fn configure_and_migrate(connection: &mut Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA synchronous=NORMAL;
         PRAGMA foreign_keys=ON;
         PRAGMA busy_timeout=5000;",
    )?;
    migrate(connection)?;
    Ok(())
}

const SCHEMA_VERSION: i64 = 5;

fn migrate(connection: &mut Connection) -> Result<(), StoreError> {
    connection
        .execute_batch("CREATE TABLE IF NOT EXISTS schema_version (v INTEGER PRIMARY KEY);")?;
    let current: Option<i64> = connection
        .query_row(
            "SELECT v FROM schema_version ORDER BY v DESC LIMIT 1",
            [],
            |row| row.get("v"),
        )
        .optional()?;
    let current = current.unwrap_or(0);
    if current >= SCHEMA_VERSION {
        return Ok(());
    }
    // Forward-only, stepwise: an older database gets exactly the migrations it
    // is missing; re-running an already-applied migration would fail on the
    // existing objects.
    let transaction = connection.transaction()?;
    if current < 1 {
        transaction.execute_batch(MIGRATION_V1)?;
    }
    if current < 2 {
        transaction.execute_batch(MIGRATION_V2)?;
    }
    if current < 3 {
        transaction.execute_batch(MIGRATION_V3)?;
    }
    if current < 4 {
        transaction.execute_batch(MIGRATION_V4)?;
    }
    if current < 5 {
        transaction.execute_batch(MIGRATION_V5)?;
    }
    transaction.execute(
        "INSERT INTO schema_version (v) VALUES (?1)",
        params![SCHEMA_VERSION],
    )?;
    transaction.commit()?;
    Ok(())
}

/// v1 — the full §6.3 schema. `channel_id` on `action` is the documented
/// Phase 2 delta (report delivery channel).
const MIGRATION_V1: &str = "
CREATE TABLE project (
  dir TEXT PRIMARY KEY,
  work_branch TEXT,
  base_sha TEXT,
  active_session_id TEXT,
  lock_holder TEXT,
  lock_since INTEGER
);

CREATE TABLE run (
  id INTEGER PRIMARY KEY,
  dir TEXT NOT NULL REFERENCES project(dir),
  plan_path TEXT,
  kind TEXT NOT NULL CHECK (kind IN ('plan','adopted','free')),
  created_at INTEGER NOT NULL
);

CREATE TABLE gate (
  id INTEGER PRIMARY KEY,
  ext_id TEXT,
  action_id INTEGER REFERENCES action(id),
  session_id TEXT,
  root_session_id TEXT,
  kind TEXT NOT NULL CHECK (kind IN ('permission','form')),
  payload_json TEXT NOT NULL,
  renderable INTEGER NOT NULL DEFAULT 1,
  state TEXT NOT NULL CHECK (state IN ('pending','answered')),
  answer TEXT,
  msg_id TEXT,
  pings INTEGER NOT NULL DEFAULT 0,
  last_ping_at INTEGER
);

CREATE TABLE action (
  id INTEGER PRIMARY KEY,
  run_id INTEGER NOT NULL REFERENCES run(id),
  session_id TEXT,
  parent_action_id INTEGER REFERENCES action(id),
  channel_id INTEGER,
  label TEXT NOT NULL,
  prompt_sent TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('staged','running','reported','accepting','accepted','parking','parked','errored','aborted','discarded')),
  blocked_gate_id INTEGER REFERENCES gate(id),
  outcome_kind TEXT,
  report_full TEXT,
  footer_json TEXT,
  report_msg_id TEXT,
  origin_msg_id TEXT,
  start_head TEXT,
  dirty_at_start INTEGER NOT NULL DEFAULT 0,
  include_existing INTEGER NOT NULL DEFAULT 0,
  pending_commit_subject TEXT,
  pending_commit_body TEXT,
  park_branch_name TEXT,
  park_step INTEGER NOT NULL DEFAULT 0,
  accept_sha TEXT,
  attempt INTEGER NOT NULL DEFAULT 1,
  followups INTEGER NOT NULL DEFAULT 0,
  started_at INTEGER,
  ended_at INTEGER,
  last_event_at INTEGER,
  stall_ping_at INTEGER
);
CREATE INDEX action_state ON action(state);
CREATE INDEX action_sha ON action(accept_sha);
CREATE INDEX action_session ON action(session_id);

CREATE TABLE session_cursor (
  session_id TEXT PRIMARY KEY,
  last_seq INTEGER NOT NULL,
  updated_at INTEGER
);

CREATE TABLE session_seen (
  session_id TEXT PRIMARY KEY,
  dir TEXT,
  first_seen INTEGER
);

CREATE TABLE notified (
  session_id TEXT,
  seq INTEGER,
  discord_msg_id TEXT,
  PRIMARY KEY (session_id, seq)
);

CREATE TABLE msg_map (
  discord_msg_id TEXT PRIMARY KEY,
  session_id TEXT,
  action_id INTEGER
);

CREATE TABLE mute (
  kind TEXT,
  key TEXT,
  PRIMARY KEY (kind, key)
);

CREATE TABLE ui_state (
  user_id TEXT PRIMARY KEY,
  cwd TEXT,
  show_hidden INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE audit (
  ts INTEGER,
  user_id TEXT,
  verb TEXT,
  action_id INTEGER,
  result TEXT
);
";

/// v2 — the `reset_intent` write-ahead log (spec §6.4 CP-R0). A reset has no
/// action state to CAS, so its intent lives in its own row: boot reconcile
/// rolls the remaining steps forward from `step` (P6/P7).
const MIGRATION_V2: &str = "
CREATE TABLE reset_intent (
  id INTEGER PRIMARY KEY,
  dir TEXT NOT NULL,
  target_sha TEXT NOT NULL,
  sup_branch TEXT NOT NULL,
  step INTEGER NOT NULL DEFAULT 0,
  created_at INTEGER NOT NULL
);
";

/// v3 — the gate lifecycle (Phase 4) needs to know where a gate's message was
/// posted so replies and re-pings can edit it (P10: edits don't notify).
const MIGRATION_V3: &str = "
ALTER TABLE gate ADD COLUMN channel_id INTEGER;
";

/// v4 — Phase 6 files/plans: the staged action remembers which template
/// envelope it is rendered from (fresh/resume toggle at review), and `ui_state`
/// carries the last selected/uploaded path (the browser and attachment draft).
const MIGRATION_V4: &str = "
ALTER TABLE action ADD COLUMN template TEXT NOT NULL DEFAULT 'fresh';
ALTER TABLE ui_state ADD COLUMN draft_path TEXT;
";

/// v5 — the de-Discord migration (SPEC-004 D-08): destinations become
/// `(surface, chat_ref)`, `msg_map` gets a namespaced PK, `notified`'s message
/// column becomes an informational receipts list, and the `component_map` /
/// `pending_input` tables (Signal, PH-4/5) are created so the migration count
/// stays one. All five parts are one conceptual change and run in the single
/// boot transaction above.
const MIGRATION_V5: &str = "
ALTER TABLE action ADD COLUMN surface TEXT NOT NULL DEFAULT 'discord';
ALTER TABLE action ADD COLUMN chat_ref TEXT;
UPDATE action SET chat_ref = CAST(channel_id AS TEXT) WHERE channel_id IS NOT NULL;
ALTER TABLE action DROP COLUMN channel_id;

CREATE TABLE msg_map_v5 (
  surface TEXT NOT NULL,
  msg_id TEXT NOT NULL,
  session_id TEXT,
  action_id INTEGER,
  created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now') * 1000),
  PRIMARY KEY (surface, msg_id)
);
INSERT INTO msg_map_v5 (surface, msg_id, session_id, action_id, created_at)
  SELECT 'discord', discord_msg_id, session_id, action_id, strftime('%s', 'now') * 1000 FROM msg_map;
DROP TABLE msg_map;
ALTER TABLE msg_map_v5 RENAME TO msg_map;

ALTER TABLE notified RENAME COLUMN discord_msg_id TO receipts;

CREATE TABLE component_map (
  surface TEXT NOT NULL,
  msg_id TEXT NOT NULL,
  emoji TEXT NOT NULL,
  custom_id TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  PRIMARY KEY (surface, msg_id, emoji)
);

CREATE TABLE pending_input (
  surface TEXT NOT NULL,
  chat_ref TEXT NOT NULL,
  flow TEXT NOT NULL,
  step TEXT NOT NULL,
  draft TEXT NOT NULL DEFAULT '{}',
  updated_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  PRIMARY KEY (surface, chat_ref)
);
";

impl std::fmt::Debug for StoreHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("StoreHandle(_)")
    }
}

/// The owned SQL value for a store write. rusqlite's own `Value` type implements
/// `ToSql`, so CAS parameters need neither trait objects nor boxing (SPEC-002
/// D-8).
fn sql_value(value: &WriteValue) -> rusqlite::types::Value {
    match value {
        WriteValue::Null => rusqlite::types::Value::Null,
        WriteValue::Text(text) => rusqlite::types::Value::Text(text.clone()),
        WriteValue::Int(integer) => rusqlite::types::Value::Integer(*integer),
    }
}

/// Shared CAS implementation. `extra_condition` adds a guard to the `WHERE`.
fn cas_inner(
    connection: &Connection,
    id: i64,
    from: State,
    to: State,
    writes: &[(Column, WriteValue)],
    extra_condition: Option<&str>,
) -> rusqlite::Result<bool> {
    use std::fmt::Write as _;
    let mut sql = String::from("UPDATE action SET state = ?1");
    let mut values = vec![rusqlite::types::Value::Text(to.as_str().to_owned())];
    for (column, value) in writes {
        let _ = write!(sql, ", {} = ?{}", column.sql(), values.len() + 1);
        values.push(sql_value(value));
    }
    sql.push_str(" WHERE id = ?");
    values.push(rusqlite::types::Value::Integer(id));
    sql.push_str(" AND state = ?");
    values.push(rusqlite::types::Value::Text(from.as_str().to_owned()));
    if let Some(condition) = extra_condition {
        let _ = write!(sql, " AND {condition}");
    }
    let changed = connection.execute(&sql, rusqlite::params_from_iter(values))?;
    Ok(changed == 1)
}

/// A CAS whose state guard is a caller-supplied `IN (...)` clause (park and
/// discard start from several decision states). `froms` are bound in order
/// after the row id, matching the `?`s in `extra_condition`.
fn cas_inner_any(
    connection: &Connection,
    id: i64,
    froms: &[String],
    to: State,
    writes: &[(Column, WriteValue)],
    extra_condition: &str,
) -> rusqlite::Result<bool> {
    use std::fmt::Write as _;
    let mut sql = String::from("UPDATE action SET state = ?1");
    let mut values = vec![rusqlite::types::Value::Text(to.as_str().to_owned())];
    for (column, value) in writes {
        let _ = write!(sql, ", {} = ?{}", column.sql(), values.len() + 1);
        values.push(sql_value(value));
    }
    sql.push_str(" WHERE id = ?");
    values.push(rusqlite::types::Value::Integer(id));
    for from in froms {
        values.push(rusqlite::types::Value::Text(from.clone()));
    }
    let _ = write!(sql, " AND {extra_condition}");
    let changed = connection.execute(&sql, rusqlite::params_from_iter(values))?;
    Ok(changed == 1)
}

/// rusqlite's private sentinel for "no meaningful column index". When it is the
/// first field of `FromSqlConversionFailure`, rusqlite's `Display` prints only
/// the wrapped error, so a column can be named without leaking a positional
/// index (FR-9).
const UNKNOWN_COLUMN: usize = usize::MAX;

fn project_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectRow> {
    Ok(ProjectRow {
        dir: row.get("dir")?,
        work_branch: row.get("work_branch")?,
        base_sha: row.get("base_sha")?,
        active_session_id: row.get("active_session_id")?,
        lock_holder: row.get("lock_holder")?,
        lock_since: row.get("lock_since")?,
    })
}

/// Parse `action.state` by name, naming the column (not an index) on failure.
fn action_state(row: &rusqlite::Row<'_>) -> rusqlite::Result<State> {
    let raw: String = row.get("state")?;
    raw.parse().map_err(|error: String| {
        rusqlite::Error::FromSqlConversionFailure(
            UNKNOWN_COLUMN,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("action.state = {raw:?}: {error}"),
            )),
        )
    })
}

fn action_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ActionRow> {
    Ok(ActionRow {
        id: row.get("id")?,
        run_id: row.get("run_id")?,
        session_id: row.get("session_id")?,
        parent_action_id: row.get("parent_action_id")?,
        surface: row.get("surface")?,
        chat_ref: row.get("chat_ref")?,
        label: row.get("label")?,
        prompt_sent: row.get("prompt_sent")?,
        state: action_state(row)?,
        blocked_gate_id: row.get("blocked_gate_id")?,
        outcome_kind: row.get("outcome_kind")?,
        report_full: row.get("report_full")?,
        footer_json: row.get("footer_json")?,
        report_msg_id: row.get("report_msg_id")?,
        origin_msg_id: row.get("origin_msg_id")?,
        start_head: row.get("start_head")?,
        dirty_at_start: row.get::<_, i64>("dirty_at_start")? != 0,
        include_existing: row.get::<_, i64>("include_existing")? != 0,
        pending_commit_subject: row.get("pending_commit_subject")?,
        pending_commit_body: row.get("pending_commit_body")?,
        park_branch_name: row.get("park_branch_name")?,
        park_step: row.get("park_step")?,
        accept_sha: row.get("accept_sha")?,
        attempt: row.get("attempt")?,
        followups: row.get("followups")?,
        started_at: row.get("started_at")?,
        ended_at: row.get("ended_at")?,
        last_event_at: row.get("last_event_at")?,
        stall_ping_at: row.get("stall_ping_at")?,
        template: row.get("template")?,
    })
}

fn pending_input_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PendingInputRow> {
    Ok(PendingInputRow {
        surface: row.get("surface")?,
        chat_ref: row.get("chat_ref")?,
        flow: row.get("flow")?,
        step: row.get("step")?,
        draft: row.get("draft")?,
        updated_at: row.get("updated_at")?,
        expires_at: row.get("expires_at")?,
    })
}

fn gate_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<GateRow> {
    Ok(GateRow {
        id: row.get("id")?,
        ext_id: row.get("ext_id")?,
        action_id: row.get("action_id")?,
        session_id: row.get("session_id")?,
        root_session_id: row.get("root_session_id")?,
        kind: row.get("kind")?,
        payload_json: row.get("payload_json")?,
        renderable: row.get::<_, i64>("renderable")? != 0,
        state: row.get("state")?,
        answer: row.get("answer")?,
        msg_id: row.get("msg_id")?,
        pings: row.get("pings")?,
        last_ping_at: row.get("last_ping_at")?,
        channel_id: row.get("channel_id")?,
    })
}

/// Saturating `u64 → i64` for timestamps and cursor seqs (bounded domains).
fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Saturating `i64 → u64` for cursor reads.
fn to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod macro_tests {
    //! Proves `store_method!` generates a working enum variant, dispatcher arm,
    //! and public method from a single declaration (SPEC-002 Phase 6).

    use std::sync::Mutex;

    use rusqlite::{Connection, OptionalExtension, params};
    use tokio::sync::oneshot;

    use super::{Reply, StoreArg, StoreError, finish};

    /// A minimal in-process handle for the codegen test: `request` dispatches
    /// synchronously on the calling thread instead of via the actor loop.
    struct TestHandle {
        connection: Mutex<Connection>,
    }

    impl TestHandle {
        async fn request<T>(
            &self,
            make: impl FnOnce(Reply<T>) -> TestRequest,
        ) -> Result<T, StoreError> {
            let (reply, reply_rx) = oneshot::channel();
            {
                let mut connection = self.connection.lock().expect("lock");
                make(reply).dispatch(&mut connection);
            }
            reply_rx.await.map_err(|_| StoreError::Closed)?
        }
    }

    store_method! {
        TestRequest, TestHandle;
        /// Count the rows in the scratch table (a new write operation).
        CountScratch as count_scratch() -> i64 {
            |connection: &mut Connection| -> rusqlite::Result<i64> {
                connection.query_row("SELECT COUNT(*) FROM scratch", [], |row| {
                    row.get::<_, i64>("COUNT(*)")
                })
            }
        };

        /// Read one scratch value by key (a new read operation).
        ScratchValue as scratch_value(key: &str => String) -> Option<i64> {
            |connection: &mut Connection| -> rusqlite::Result<Option<i64>> {
                connection
                    .query_row(
                        "SELECT value FROM scratch WHERE key = ?1",
                        params![key],
                        |row| row.get::<_, i64>("value"),
                    )
                    .optional()
            }
        };
    }

    fn test_handle() -> TestHandle {
        let connection = Connection::open_in_memory().expect("db");
        connection
            .execute_batch("CREATE TABLE scratch (key TEXT, value INTEGER);")
            .expect("schema");
        TestHandle {
            connection: Mutex::new(connection),
        }
    }

    #[tokio::test]
    async fn store_macro_generated() {
        let handle = test_handle();
        assert_eq!(handle.count_scratch().await.expect("count"), 0);
        handle
            .connection
            .lock()
            .expect("lock")
            .execute("INSERT INTO scratch (key, value) VALUES ('a', 7)", [])
            .expect("insert");
        assert_eq!(handle.count_scratch().await.expect("count"), 1);
    }

    #[tokio::test]
    async fn store_macro_new_operation() {
        let handle = test_handle();
        assert_eq!(handle.scratch_value("a").await.expect("read"), None);
        handle
            .connection
            .lock()
            .expect("lock")
            .execute("INSERT INTO scratch (key, value) VALUES ('a', 7)", [])
            .expect("insert");
        assert_eq!(handle.scratch_value("a").await.expect("read"), Some(7));
    }
}
