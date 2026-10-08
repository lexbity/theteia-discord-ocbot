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
    /// The Discord channel reports must go to (Phase 2 delta).
    pub channel_id: Option<i64>,
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

/// The typed reply channel for one [`StoreRequest`].
type Reply<T> = oneshot::Sender<Result<T, StoreError>>;

/// One typed store request (SPEC-002 §4.1, FR-1/FR-2). Each variant carries its
/// own response sender, so the actor's message set is explicit and the reply
/// type is checked at compile time — no type erasure, no runtime downcast.
enum StoreRequest {
    // --- project / run ---
    LockHolder {
        dir: String,
        reply: Reply<Option<String>>,
    },
    UpsertProject {
        dir: String,
        work_branch: Option<String>,
        base_sha: Option<String>,
        reply: Reply<()>,
    },
    Project {
        dir: String,
        reply: Reply<Option<ProjectRow>>,
    },
    LockedProjects {
        reply: Reply<Vec<ProjectRow>>,
    },
    InsertRun {
        dir: String,
        kind: String,
        reply: Reply<i64>,
    },
    InsertRunWithPlan {
        dir: String,
        kind: String,
        plan_path: Option<String>,
        reply: Reply<i64>,
    },
    // --- action ---
    InsertAction {
        run_id: i64,
        channel_id: Option<i64>,
        label: String,
        prompt_sent: String,
        attempt: i64,
        reply: Reply<i64>,
    },
    InsertStagedChild {
        run_id: i64,
        channel_id: Option<i64>,
        label: String,
        prompt_sent: String,
        template: String,
        attempt: i64,
        parent_action_id: i64,
        reply: Reply<i64>,
    },
    Run {
        id: i64,
        reply: Reply<Option<RunRow>>,
    },
    Action {
        id: i64,
        reply: Reply<Option<ActionRow>>,
    },
    ActionsInStates {
        states: Vec<String>,
        reply: Reply<Vec<ActionRow>>,
    },
    ActionsForDir {
        dir: String,
        reply: Reply<Vec<ActionRow>>,
    },
    LastAcceptSha {
        dir: String,
        reply: Reply<Option<String>>,
    },
    RunningForSession {
        session_id: String,
        reply: Reply<Option<ActionRow>>,
    },
    RunningActionForDir {
        dir: String,
        reply: Reply<Option<ActionRow>>,
    },
    SetOrigin {
        action_id: i64,
        origin: String,
        reply: Reply<()>,
    },
    MarkIncludeExisting {
        action_id: i64,
        reply: Reply<bool>,
    },
    // --- transitions ---
    CasState {
        id: i64,
        from: State,
        to: State,
        writes: Vec<(Column, WriteValue)>,
        reply: Reply<bool>,
    },
    CasStateFromAny {
        id: i64,
        from_states: Vec<String>,
        to: State,
        writes: Vec<(Column, WriteValue)>,
        reply: Reply<bool>,
    },
    DeliverReport {
        id: i64,
        to: State,
        writes: Vec<(Column, WriteValue)>,
        reply: Reply<bool>,
    },
    BeginSend {
        action_id: i64,
        dir: String,
        session_id: String,
        start_head: Option<String>,
        dirty: bool,
        reply: Reply<bool>,
    },
    RestagePrompt {
        id: i64,
        prompt: String,
        reply: Reply<bool>,
    },
    RestageLabelPrompt {
        id: i64,
        label: String,
        prompt: String,
        reply: Reply<bool>,
    },
    SetTemplatePrompt {
        id: i64,
        template: String,
        prompt: String,
        reply: Reply<bool>,
    },
    SetStallPing {
        id: i64,
        ts: i64,
        reply: Reply<()>,
    },
    FailSend {
        id: i64,
        dir: String,
        reason: String,
        reply: Reply<()>,
    },
    // --- ui state / audit / cursor ---
    DraftPath {
        user_id: u64,
        reply: Reply<Option<String>>,
    },
    SetDraftPath {
        user_id: u64,
        path: String,
        reply: Reply<()>,
    },
    ClearDraftPath {
        user_id: u64,
        reply: Reply<()>,
    },
    UiCwd {
        user_id: u64,
        reply: Reply<Option<String>>,
    },
    SetUiCwd {
        user_id: u64,
        cwd: String,
        reply: Reply<()>,
    },
    UpsertSeen {
        session_id: String,
        directory: Option<String>,
        reply: Reply<()>,
    },
    UpsertCursor {
        session_id: String,
        seq: u64,
        reply: Reply<()>,
    },
    Cursor {
        session_id: String,
        reply: Reply<Option<CursorRow>>,
    },
    RecentDirs {
        limit: u64,
        reply: Reply<Vec<String>>,
    },
    Audit {
        user_id: u64,
        verb: String,
        action_id: Option<i64>,
        result: String,
        reply: Reply<()>,
    },
    ActionCountsByState {
        reply: Reply<Vec<(String, i64)>>,
    },
    // --- reset intent ---
    RecordResetIntent {
        dir: String,
        target_sha: String,
        sup_branch: String,
        reply: Reply<i64>,
    },
    PendingResetIntents {
        reply: Reply<Vec<ResetIntentRow>>,
    },
    SetResetStep {
        id: i64,
        step: i64,
        reply: Reply<bool>,
    },
    DeleteResetIntent {
        id: i64,
        reply: Reply<()>,
    },
    // --- gate lifecycle ---
    UpsertGate {
        ext_id: String,
        action_id: Option<i64>,
        session_id: Option<String>,
        root_session_id: Option<String>,
        kind: String,
        payload_json: String,
        renderable: bool,
        channel_id: Option<i64>,
        now: i64,
        reply: Reply<(i64, bool)>,
    },
    Gate {
        id: i64,
        reply: Reply<Option<GateRow>>,
    },
    PendingGates {
        reply: Reply<Vec<GateRow>>,
    },
    CasGate {
        id: i64,
        from: String,
        to: String,
        answer: Option<String>,
        reply: Reply<bool>,
    },
    SetGateMsg {
        id: i64,
        msg_id: String,
        channel_id: Option<i64>,
        reply: Reply<()>,
    },
    BumpGatePings {
        id: i64,
        now: i64,
        reply: Reply<i64>,
    },
    AnsweredGates {
        reply: Reply<Vec<GateRow>>,
    },
    SetActionBlocked {
        action_id: i64,
        gate_id: i64,
        reply: Reply<()>,
    },
    ClearActionBlocked {
        action_id: i64,
        gate_id: i64,
        reply: Reply<()>,
    },
    // --- session lifecycle ---
    SetActiveSession {
        dir: String,
        session_id: String,
        reply: Reply<()>,
    },
    SetExternalLock {
        dir: String,
        session_id: String,
        since: i64,
        reply: Reply<()>,
    },
    ReleaseLock {
        dir: String,
        holder: String,
        reply: Reply<bool>,
    },
    RetireActiveSession {
        dir: String,
        reply: Reply<()>,
    },
    SessionDir {
        session_id: String,
        reply: Reply<Option<String>>,
    },
    KnownBotSessions {
        reply: Reply<Vec<String>>,
    },
    ClaimNotified {
        session_id: String,
        seq: String,
        reply: Reply<bool>,
    },
    FinishNotified {
        session_id: String,
        seq: String,
        discord_msg_id: String,
        reply: Reply<()>,
    },
    UnclaimNotified {
        session_id: String,
        seq: String,
        reply: Reply<()>,
    },
    InsertMsgMap {
        discord_msg_id: String,
        session_id: String,
        reply: Reply<()>,
    },
    LookupMsgMap {
        discord_msg_id: String,
        reply: Reply<Option<(String, Option<i64>)>>,
    },
    IsMuted {
        kind: String,
        key: String,
        reply: Reply<bool>,
    },
    ToggleMute {
        kind: String,
        key: String,
        reply: Reply<bool>,
    },
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

    /// The lock holder of a directory, if any (P5).
    pub async fn lock_holder(&self, dir: &str) -> Result<Option<String>, StoreError> {
        self.request(|reply| StoreRequest::LockHolder {
            dir: dir.to_owned(),
            reply,
        })
        .await
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

impl StoreRequest {
    /// Dispatch one request on the actor thread and send its typed reply
    /// (FR-2/FR-6: every operation is one enum variant and one match arm).
    #[expect(
        clippy::too_many_lines,
        reason = "the dispatcher is a flat, exhaustive match over the store's message set; one arm per operation keeps the whole grammar in one place"
    )]
    fn dispatch(self, connection: &mut Connection) {
        match self {
            StoreRequest::LockHolder { dir, reply } => finish(
                reply,
                connection
                    .query_row(
                        "SELECT lock_holder FROM project WHERE dir = ?1",
                        params![dir],
                        |row| row.get::<_, Option<String>>("lock_holder"),
                    )
                    .optional()
                    .map(Option::flatten),
            ),
            StoreRequest::UpsertProject {
                dir,
                work_branch,
                base_sha,
                reply,
            } => finish(
                reply,
                connection
                    .execute(
                        "INSERT INTO project (dir, work_branch, base_sha)
                         VALUES (?1, ?2, ?3)
                         ON CONFLICT(dir) DO UPDATE SET
                            work_branch = excluded.work_branch,
                            base_sha = excluded.base_sha",
                        params![dir, work_branch, base_sha],
                    )
                    .map(|_| ()),
            ),
            StoreRequest::Project { dir, reply } => finish(
                reply,
                connection
                    .query_row(
                        "SELECT dir, work_branch, base_sha, active_session_id, lock_holder, lock_since
                         FROM project WHERE dir = ?1",
                        params![dir],
                        project_row,
                    )
                    .optional(),
            ),
            StoreRequest::LockedProjects { reply } => finish(
                reply,
                (|| -> rusqlite::Result<Vec<ProjectRow>> {
                    let mut statement = connection.prepare(
                        "SELECT dir, work_branch, base_sha, active_session_id, lock_holder, lock_since
                         FROM project WHERE lock_holder IS NOT NULL ORDER BY dir",
                    )?;
                    let rows = statement
                        .query_map([], project_row)?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(rows)
                })(),
            ),
            StoreRequest::InsertRun { dir, kind, reply } => finish(
                reply,
                (|| -> rusqlite::Result<i64> {
                    connection.execute(
                        "INSERT INTO run (dir, kind, created_at) VALUES (?1, ?2, ?3)",
                        params![dir, kind, now_unix()],
                    )?;
                    Ok(connection.last_insert_rowid())
                })(),
            ),
            StoreRequest::InsertRunWithPlan {
                dir,
                kind,
                plan_path,
                reply,
            } => finish(
                reply,
                (|| -> rusqlite::Result<i64> {
                    connection.execute(
                        "INSERT INTO run (dir, plan_path, kind, created_at) VALUES (?1, ?2, ?3, ?4)",
                        params![dir, plan_path, kind, now_unix()],
                    )?;
                    Ok(connection.last_insert_rowid())
                })(),
            ),
            StoreRequest::InsertAction {
                run_id,
                channel_id,
                label,
                prompt_sent,
                attempt,
                reply,
            } => finish(
                reply,
                (|| -> rusqlite::Result<i64> {
                    connection.execute(
                        "INSERT INTO action (run_id, channel_id, label, prompt_sent, state, attempt)
                         VALUES (?1, ?2, ?3, ?4, 'staged', ?5)",
                        params![run_id, channel_id, label, prompt_sent, attempt],
                    )?;
                    Ok(connection.last_insert_rowid())
                })(),
            ),
            StoreRequest::InsertStagedChild {
                run_id,
                channel_id,
                label,
                prompt_sent,
                template,
                attempt,
                parent_action_id,
                reply,
            } => finish(
                reply,
                (|| -> rusqlite::Result<i64> {
                    connection.execute(
                        "INSERT INTO action
                            (run_id, channel_id, label, prompt_sent, template, state, attempt, parent_action_id)
                         VALUES (?1, ?2, ?3, ?4, ?5, 'staged', ?6, ?7)",
                        params![
                            run_id,
                            channel_id,
                            label,
                            prompt_sent,
                            template,
                            attempt,
                            parent_action_id
                        ],
                    )?;
                    Ok(connection.last_insert_rowid())
                })(),
            ),
            StoreRequest::Run { id, reply } => finish(
                reply,
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
                    .optional(),
            ),
            StoreRequest::Action { id, reply } => finish(
                reply,
                connection
                    .query_row(
                        "SELECT * FROM action WHERE id = ?1",
                        params![id],
                        action_row,
                    )
                    .optional(),
            ),
            StoreRequest::ActionsInStates { states, reply } => finish(
                reply,
                (|| -> rusqlite::Result<Vec<ActionRow>> {
                    if states.is_empty() {
                        return Ok(Vec::new());
                    }
                    let placeholders = vec!["?"; states.len()].join(",");
                    let sql =
                        format!("SELECT * FROM action WHERE state IN ({placeholders}) ORDER BY id");
                    let mut statement = connection.prepare(&sql)?;
                    let rows = statement
                        .query_map(rusqlite::params_from_iter(states.iter()), action_row)?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(rows)
                })(),
            ),
            StoreRequest::ActionsForDir { dir, reply } => finish(
                reply,
                (|| -> rusqlite::Result<Vec<ActionRow>> {
                    let mut statement = connection.prepare(
                        "SELECT action.* FROM action
                         JOIN run ON action.run_id = run.id
                         WHERE run.dir = ?1 ORDER BY action.id",
                    )?;
                    let rows = statement
                        .query_map(params![dir], action_row)?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(rows)
                })(),
            ),
            StoreRequest::LastAcceptSha { dir, reply } => finish(
                reply,
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
                    .map(Option::flatten),
            ),
            StoreRequest::RunningForSession { session_id, reply } => finish(
                reply,
                connection
                    .query_row(
                        "SELECT * FROM action WHERE session_id = ?1 AND state = 'running' ORDER BY id DESC LIMIT 1",
                        params![session_id],
                        action_row,
                    )
                    .optional(),
            ),
            StoreRequest::RunningActionForDir { dir, reply } => finish(
                reply,
                connection
                    .query_row(
                        "SELECT action.* FROM action
                         JOIN run ON action.run_id = run.id
                         WHERE run.dir = ?1 AND action.state = 'running'
                         ORDER BY action.id DESC LIMIT 1",
                        params![dir],
                        action_row,
                    )
                    .optional(),
            ),
            StoreRequest::SetOrigin {
                action_id,
                origin,
                reply,
            } => finish(
                reply,
                connection
                    .execute(
                        "UPDATE action SET origin_msg_id = ?1 WHERE id = ?2 AND origin_msg_id IS NULL",
                        params![origin, action_id],
                    )
                    .map(|_| ()),
            ),
            StoreRequest::MarkIncludeExisting { action_id, reply } => finish(
                reply,
                (|| -> rusqlite::Result<bool> {
                    let changed = connection.execute(
                        "UPDATE action SET include_existing = 1 WHERE id = ?1 AND state = 'staged'",
                        params![action_id],
                    )?;
                    Ok(changed == 1)
                })(),
            ),
            StoreRequest::CasState {
                id,
                from,
                to,
                writes,
                reply,
            } => finish(reply, cas_inner(connection, id, from, to, &writes, None)),
            StoreRequest::CasStateFromAny {
                id,
                from_states,
                to,
                writes,
                reply,
            } => finish(
                reply,
                (|| -> rusqlite::Result<bool> {
                    if from_states.is_empty() {
                        return Ok(false);
                    }
                    let placeholders = vec!["?"; from_states.len()].join(",");
                    let extra = format!("state IN ({placeholders})");
                    cas_inner_any(connection, id, &from_states, to, &writes, &extra)
                })(),
            ),
            StoreRequest::DeliverReport { id, to, writes, reply } => finish(
                reply,
                cas_inner(
                    connection,
                    id,
                    State::Running,
                    to,
                    &writes,
                    Some("report_msg_id IS NULL"),
                ),
            ),
            StoreRequest::BeginSend {
                action_id,
                dir,
                session_id,
                start_head,
                dirty,
                reply,
            } => finish(
                reply,
                (|| -> rusqlite::Result<bool> {
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
                        // Atomic lock acquisition (P5): take the lock only while
                        // free or already held by this same action id. If the
                        // lock is held by someone else, the transition rolls back.
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
                })(),
            ),
            StoreRequest::RestagePrompt { id, prompt, reply } => finish(
                reply,
                (|| -> rusqlite::Result<bool> {
                    let changed = connection.execute(
                        "UPDATE action SET prompt_sent = ?1 WHERE id = ?2 AND state = 'staged'",
                        params![prompt, id],
                    )?;
                    Ok(changed == 1)
                })(),
            ),
            StoreRequest::RestageLabelPrompt {
                id,
                label,
                prompt,
                reply,
            } => finish(
                reply,
                (|| -> rusqlite::Result<bool> {
                    let changed = connection.execute(
                        "UPDATE action SET label = ?1, prompt_sent = ?2 WHERE id = ?3 AND state = 'staged'",
                        params![label, prompt, id],
                    )?;
                    Ok(changed == 1)
                })(),
            ),
            StoreRequest::SetTemplatePrompt {
                id,
                template,
                prompt,
                reply,
            } => finish(
                reply,
                (|| -> rusqlite::Result<bool> {
                    let changed = connection.execute(
                        "UPDATE action SET template = ?1, prompt_sent = ?2 WHERE id = ?3 AND state = 'staged'",
                        params![template, prompt, id],
                    )?;
                    Ok(changed == 1)
                })(),
            ),
            StoreRequest::SetStallPing { id, ts, reply } => finish(
                reply,
                connection
                    .execute(
                        "UPDATE action SET stall_ping_at = ?1 WHERE id = ?2",
                        params![ts, id],
                    )
                    .map(|_| ()),
            ),
            StoreRequest::FailSend { id, dir, reason, reply } => finish(
                reply,
                (|| -> rusqlite::Result<()> {
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
                })(),
            ),
            StoreRequest::DraftPath { user_id, reply } => finish(
                reply,
                connection
                    .query_row(
                        "SELECT draft_path FROM ui_state WHERE user_id = ?1",
                        params![user_id.to_string()],
                        |row| row.get::<_, Option<String>>("draft_path"),
                    )
                    .optional()
                    .map(Option::flatten),
            ),
            StoreRequest::SetDraftPath {
                user_id,
                path,
                reply,
            } => finish(
                reply,
                connection
                    .execute(
                        "INSERT INTO ui_state (user_id, draft_path) VALUES (?1, ?2)
                         ON CONFLICT(user_id) DO UPDATE SET draft_path = excluded.draft_path",
                        params![user_id.to_string(), path],
                    )
                    .map(|_| ()),
            ),
            StoreRequest::ClearDraftPath { user_id, reply } => finish(
                reply,
                connection
                    .execute(
                        "UPDATE ui_state SET draft_path = NULL WHERE user_id = ?1",
                        params![user_id.to_string()],
                    )
                    .map(|_| ()),
            ),
            StoreRequest::UiCwd { user_id, reply } => finish(
                reply,
                connection
                    .query_row(
                        "SELECT cwd FROM ui_state WHERE user_id = ?1",
                        params![user_id.to_string()],
                        |row| row.get::<_, Option<String>>("cwd"),
                    )
                    .optional()
                    .map(Option::flatten),
            ),
            StoreRequest::SetUiCwd { user_id, cwd, reply } => finish(
                reply,
                connection
                    .execute(
                        "INSERT INTO ui_state (user_id, cwd) VALUES (?1, ?2)
                         ON CONFLICT(user_id) DO UPDATE SET cwd = excluded.cwd",
                        params![user_id.to_string(), cwd],
                    )
                    .map(|_| ()),
            ),
            StoreRequest::UpsertSeen {
                session_id,
                directory,
                reply,
            } => finish(
                reply,
                connection
                    .execute(
                        "INSERT INTO session_seen (session_id, dir, first_seen) VALUES (?1, ?2, ?3)
                         ON CONFLICT(session_id) DO NOTHING",
                        params![session_id, directory, now_unix()],
                    )
                    .map(|_| ()),
            ),
            StoreRequest::UpsertCursor {
                session_id,
                seq,
                reply,
            } => finish(
                reply,
                connection
                    .execute(
                        "INSERT INTO session_cursor (session_id, last_seq, updated_at) VALUES (?1, ?2, ?3)
                         ON CONFLICT(session_id) DO UPDATE SET last_seq = excluded.last_seq, updated_at = excluded.updated_at",
                        params![session_id, to_i64(seq), now_unix()],
                    )
                    .map(|_| ()),
            ),
            StoreRequest::Cursor { session_id, reply } => finish(
                reply,
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
                    .optional(),
            ),
            StoreRequest::RecentDirs { limit, reply } => finish(
                reply,
                (|| -> rusqlite::Result<Vec<String>> {
                    let mut statement = connection.prepare(
                        "SELECT dir FROM project ORDER BY (
                            SELECT MAX(created_at) FROM run WHERE run.dir = project.dir
                         ) DESC LIMIT ?1",
                    )?;
                    let rows = statement
                        .query_map(params![to_i64(limit)], |row| row.get::<_, String>("dir"))?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(rows)
                })(),
            ),
            StoreRequest::Audit {
                user_id,
                verb,
                action_id,
                result,
                reply,
            } => finish(
                reply,
                connection
                    .execute(
                        "INSERT INTO audit (ts, user_id, verb, action_id, result) VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![now_unix(), user_id.to_string(), verb, action_id, result],
                    )
                    .map(|_| ()),
            ),
            StoreRequest::ActionCountsByState { reply } => finish(
                reply,
                (|| -> rusqlite::Result<Vec<(String, i64)>> {
                    let mut statement =
                        connection.prepare("SELECT state, COUNT(*) AS n FROM action GROUP BY state")?;
                    let rows = statement
                        .query_map([], |row| {
                            Ok((row.get::<_, String>("state")?, row.get::<_, i64>("n")?))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(rows)
                })(),
            ),
            StoreRequest::RecordResetIntent {
                dir,
                target_sha,
                sup_branch,
                reply,
            } => finish(
                reply,
                (|| -> rusqlite::Result<i64> {
                    connection.execute(
                        "INSERT INTO reset_intent (dir, target_sha, sup_branch, step, created_at)
                         VALUES (?1, ?2, ?3, 0, ?4)",
                        params![dir, target_sha, sup_branch, now_unix()],
                    )?;
                    Ok(connection.last_insert_rowid())
                })(),
            ),
            StoreRequest::PendingResetIntents { reply } => finish(
                reply,
                (|| -> rusqlite::Result<Vec<ResetIntentRow>> {
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
                })(),
            ),
            StoreRequest::SetResetStep { id, step, reply } => finish(
                reply,
                (|| -> rusqlite::Result<bool> {
                    let changed = connection.execute(
                        "UPDATE reset_intent SET step = ?1 WHERE id = ?2",
                        params![step, id],
                    )?;
                    Ok(changed == 1)
                })(),
            ),
            StoreRequest::DeleteResetIntent { id, reply } => finish(
                reply,
                connection
                    .execute("DELETE FROM reset_intent WHERE id = ?1", params![id])
                    .map(|_| ()),
            ),
            StoreRequest::UpsertGate {
                ext_id,
                action_id,
                session_id,
                root_session_id,
                kind,
                payload_json,
                renderable,
                channel_id,
                now,
                reply,
            } => finish(
                reply,
                (|| -> rusqlite::Result<(i64, bool)> {
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
                })(),
            ),
            StoreRequest::Gate { id, reply } => finish(
                reply,
                connection
                    .query_row("SELECT * FROM gate WHERE id = ?1", params![id], gate_row)
                    .optional(),
            ),
            StoreRequest::PendingGates { reply } => finish(
                reply,
                (|| -> rusqlite::Result<Vec<GateRow>> {
                    let mut statement = connection
                        .prepare("SELECT * FROM gate WHERE state = 'pending' ORDER BY id")?;
                    let rows = statement
                        .query_map([], gate_row)?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(rows)
                })(),
            ),
            StoreRequest::CasGate {
                id,
                from,
                to,
                answer,
                reply,
            } => finish(
                reply,
                (|| -> rusqlite::Result<bool> {
                    let changed = connection.execute(
                        "UPDATE gate SET state = ?1, answer = ?2 WHERE id = ?3 AND state = ?4",
                        params![to, answer, id, from],
                    )?;
                    Ok(changed == 1)
                })(),
            ),
            StoreRequest::SetGateMsg {
                id,
                msg_id,
                channel_id,
                reply,
            } => finish(
                reply,
                connection
                    .execute(
                        "UPDATE gate SET msg_id = ?1, channel_id = ?2 WHERE id = ?3 AND msg_id IS NULL",
                        params![msg_id, channel_id, id],
                    )
                    .map(|_| ()),
            ),
            StoreRequest::BumpGatePings { id, now, reply } => finish(
                reply,
                (|| -> rusqlite::Result<i64> {
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
                })(),
            ),
            StoreRequest::AnsweredGates { reply } => finish(
                reply,
                (|| -> rusqlite::Result<Vec<GateRow>> {
                    let mut statement = connection.prepare(
                        "SELECT * FROM gate WHERE state = 'answered' AND answer IS NOT NULL ORDER BY id",
                    )?;
                    let rows = statement
                        .query_map([], gate_row)?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(rows)
                })(),
            ),
            StoreRequest::SetActionBlocked {
                action_id,
                gate_id,
                reply,
            } => finish(
                reply,
                connection
                    .execute(
                        "UPDATE action SET blocked_gate_id = ?1 WHERE id = ?2",
                        params![gate_id, action_id],
                    )
                    .map(|_| ()),
            ),
            StoreRequest::ClearActionBlocked {
                action_id,
                gate_id,
                reply,
            } => finish(
                reply,
                connection
                    .execute(
                        "UPDATE action SET blocked_gate_id = NULL WHERE id = ?1 AND blocked_gate_id = ?2",
                        params![action_id, gate_id],
                    )
                    .map(|_| ()),
            ),
            StoreRequest::SetActiveSession {
                dir,
                session_id,
                reply,
            } => finish(
                reply,
                connection
                    .execute(
                        "INSERT INTO project (dir, active_session_id) VALUES (?1, ?2)
                         ON CONFLICT(dir) DO UPDATE SET active_session_id = excluded.active_session_id",
                        params![dir, session_id],
                    )
                    .map(|_| ()),
            ),
            StoreRequest::SetExternalLock {
                dir,
                session_id,
                since,
                reply,
            } => finish(
                reply,
                connection
                    .execute(
                        "INSERT INTO project (dir, lock_holder, lock_since) VALUES (?1, ?2, ?3)
                         ON CONFLICT(dir) DO UPDATE SET lock_holder = excluded.lock_holder,
                                                        lock_since = excluded.lock_since",
                        params![dir, format!("external:{session_id}"), since],
                    )
                    .map(|_| ()),
            ),
            StoreRequest::ReleaseLock { dir, holder, reply } => finish(
                reply,
                (|| -> rusqlite::Result<bool> {
                    let changed = connection.execute(
                        "UPDATE project SET lock_holder = NULL, lock_since = NULL
                         WHERE dir = ?1 AND lock_holder = ?2",
                        params![dir, holder],
                    )?;
                    Ok(changed == 1)
                })(),
            ),
            StoreRequest::RetireActiveSession { dir, reply } => finish(
                reply,
                connection
                    .execute(
                        "UPDATE project SET active_session_id = NULL WHERE dir = ?1",
                        params![dir],
                    )
                    .map(|_| ()),
            ),
            StoreRequest::SessionDir { session_id, reply } => finish(
                reply,
                connection
                    .query_row(
                        "SELECT dir FROM session_seen WHERE session_id = ?1",
                        params![session_id],
                        |row| row.get("dir"),
                    )
                    .optional(),
            ),
            StoreRequest::KnownBotSessions { reply } => finish(
                reply,
                (|| -> rusqlite::Result<Vec<String>> {
                    let mut statement = connection.prepare(
                        "SELECT session_id FROM action WHERE session_id IS NOT NULL
                         UNION
                         SELECT active_session_id FROM project WHERE active_session_id IS NOT NULL",
                    )?;
                    let rows = statement
                        .query_map([], |row| row.get::<_, String>("session_id"))?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(rows)
                })(),
            ),
            StoreRequest::ClaimNotified {
                session_id,
                seq,
                reply,
            } => finish(
                reply,
                (|| -> rusqlite::Result<bool> {
                    let changed = connection.execute(
                        "INSERT OR IGNORE INTO notified (session_id, seq, discord_msg_id)
                         VALUES (?1, ?2, '')",
                        params![session_id, seq],
                    )?;
                    Ok(changed == 1)
                })(),
            ),
            StoreRequest::FinishNotified {
                session_id,
                seq,
                discord_msg_id,
                reply,
            } => finish(
                reply,
                connection
                    .execute(
                        "UPDATE notified SET discord_msg_id = ?1 WHERE session_id = ?2 AND seq = ?3",
                        params![discord_msg_id, session_id, seq],
                    )
                    .map(|_| ()),
            ),
            StoreRequest::UnclaimNotified {
                session_id,
                seq,
                reply,
            } => finish(
                reply,
                connection
                    .execute(
                        "DELETE FROM notified WHERE session_id = ?1 AND seq = ?2 AND discord_msg_id = ''",
                        params![session_id, seq],
                    )
                    .map(|_| ()),
            ),
            StoreRequest::InsertMsgMap {
                discord_msg_id,
                session_id,
                reply,
            } => finish(
                reply,
                connection
                    .execute(
                        "INSERT OR IGNORE INTO msg_map (discord_msg_id, session_id) VALUES (?1, ?2)",
                        params![discord_msg_id, session_id],
                    )
                    .map(|_| ()),
            ),
            StoreRequest::LookupMsgMap {
                discord_msg_id,
                reply,
            } => finish(
                reply,
                connection
                    .query_row(
                        "SELECT session_id, action_id FROM msg_map WHERE discord_msg_id = ?1",
                        params![discord_msg_id],
                        |row| {
                            Ok((
                                row.get::<_, String>("session_id")?,
                                row.get::<_, Option<i64>>("action_id")?,
                            ))
                        },
                    )
                    .optional(),
            ),
            StoreRequest::IsMuted { kind, key, reply } => finish(
                reply,
                (|| -> rusqlite::Result<bool> {
                    let muted = connection
                        .query_row(
                            "SELECT 1 FROM mute WHERE kind = ?1 AND key = ?2",
                            params![kind, key],
                            |_| Ok(()),
                        )
                        .optional()?
                        .is_some();
                    Ok(muted)
                })(),
            ),
            StoreRequest::ToggleMute { kind, key, reply } => finish(
                reply,
                (|| -> rusqlite::Result<bool> {
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
                })(),
            ),
        }
    }
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

const SCHEMA_VERSION: i64 = 4;

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

impl std::fmt::Debug for StoreHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("StoreHandle(_)")
    }
}

// == typed operations =======================================================

impl StoreHandle {
    /// Upsert a project row for `dir`. `work_branch`/`base_sha` are `None` for
    /// non-git directories.
    pub async fn upsert_project(
        &self,
        dir: &str,
        work_branch: Option<&str>,
        base_sha: Option<&str>,
    ) -> Result<(), StoreError> {
        self.request(|reply| StoreRequest::UpsertProject {
            dir: dir.to_owned(),
            work_branch: work_branch.map(str::to_owned),
            base_sha: base_sha.map(str::to_owned),
            reply,
        })
        .await
    }

    /// Project by directory.
    pub async fn project(&self, dir: &str) -> Result<Option<ProjectRow>, StoreError> {
        self.request(|reply| StoreRequest::Project {
            dir: dir.to_owned(),
            reply,
        })
        .await
    }

    /// Every project that currently holds a lock (for `/status`).
    pub async fn locked_projects(&self) -> Result<Vec<ProjectRow>, StoreError> {
        self.request(|reply| StoreRequest::LockedProjects { reply })
            .await
    }

    /// Insert a run row; returns its id.
    pub async fn insert_run(&self, dir: &str, kind: &str) -> Result<i64, StoreError> {
        self.request(|reply| StoreRequest::InsertRun {
            dir: dir.to_owned(),
            kind: kind.to_owned(),
            reply,
        })
        .await
    }

    /// Insert a staged action; returns its id.
    pub async fn insert_action(
        &self,
        run_id: i64,
        channel_id: Option<i64>,
        label: &str,
        prompt_sent: &str,
        attempt: i64,
    ) -> Result<i64, StoreError> {
        self.request(|reply| StoreRequest::InsertAction {
            run_id,
            channel_id,
            label: label.to_owned(),
            prompt_sent: prompt_sent.to_owned(),
            attempt,
            reply,
        })
        .await
    }

    /// A run row by id.
    pub async fn run(&self, id: i64) -> Result<Option<RunRow>, StoreError> {
        self.request(|reply| StoreRequest::Run { id, reply }).await
    }

    /// Record the staging-preview message id on an action (also seeds the
    /// origin for the quiet status reactions).
    pub async fn set_origin(&self, action_id: i64, origin: &str) -> Result<(), StoreError> {
        self.request(|reply| StoreRequest::SetOrigin {
            action_id,
            origin: origin.to_owned(),
            reply,
        })
        .await
    }

    /// Mark a session as seen (Phase 5 inbox bookkeeping, seeded from Phase 2).
    pub async fn upsert_seen(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> Result<(), StoreError> {
        self.request(|reply| StoreRequest::UpsertSeen {
            session_id: session_id.to_owned(),
            directory: directory.map(str::to_owned),
            reply,
        })
        .await
    }

    /// Confirm the operator chose to include existing dirty changes (FR-27).
    pub async fn mark_include_existing(&self, action_id: i64) -> Result<bool, StoreError> {
        self.request(|reply| StoreRequest::MarkIncludeExisting { action_id, reply })
            .await
    }

    /// An action by id.
    pub async fn action(&self, id: i64) -> Result<Option<ActionRow>, StoreError> {
        self.request(|reply| StoreRequest::Action { id, reply })
            .await
    }

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

    /// Actions currently in any of `states` (boot reconcile over intermediate
    /// states uses this — spec P9 step 5).
    pub async fn actions_in_states(&self, states: &[State]) -> Result<Vec<ActionRow>, StoreError> {
        let states: Vec<String> = states
            .iter()
            .map(|state| state.as_str().to_owned())
            .collect();
        self.request(|reply| StoreRequest::ActionsInStates { states, reply })
            .await
    }

    /// Every action belonging to a directory, oldest first (history merge, P8).
    pub async fn actions_for_dir(&self, dir: &str) -> Result<Vec<ActionRow>, StoreError> {
        self.request(|reply| StoreRequest::ActionsForDir {
            dir: dir.to_owned(),
            reply,
        })
        .await
    }

    /// The most recent accepted sha in a directory, if any (park's fallback
    /// reset target, spec P7 step 3).
    pub async fn last_accept_sha(&self, dir: &str) -> Result<Option<String>, StoreError> {
        self.request(|reply| StoreRequest::LastAcceptSha {
            dir: dir.to_owned(),
            reply,
        })
        .await
    }

    /// The running action that owns `session_id`, if any.
    pub async fn running_for_session(
        &self,
        session_id: &str,
    ) -> Result<Option<ActionRow>, StoreError> {
        self.request(|reply| StoreRequest::RunningForSession {
            session_id: session_id.to_owned(),
            reply,
        })
        .await
    }

    /// A generic CAS: `UPDATE ... SET state=:to <writes> WHERE id=:id AND
    /// state=:from`. Returns whether exactly one row changed (P6).
    pub async fn cas_state(
        &self,
        id: i64,
        from: State,
        to: State,
        writes: &[(Column, WriteValue)],
    ) -> Result<bool, StoreError> {
        self.request(|reply| StoreRequest::CasState {
            id,
            from,
            to,
            writes: writes.to_vec(),
            reply,
        })
        .await
    }

    /// A CAS whose `from` may be any of `from_states` (park starts from
    /// reported, errored, or aborted; discard likewise — spec §6.4). Exactly
    /// one row changes or the caller lost the race.
    pub async fn cas_state_from_any(
        &self,
        id: i64,
        from_states: &[State],
        to: State,
        writes: &[(Column, WriteValue)],
    ) -> Result<bool, StoreError> {
        self.request(|reply| StoreRequest::CasStateFromAny {
            id,
            from_states: from_states
                .iter()
                .map(|state| state.as_str().to_owned())
                .collect(),
            to,
            writes: writes.to_vec(),
            reply,
        })
        .await
    }

    /// The report-delivery CAS: `running → reported/errored/aborted` **only
    /// while `report_msg_id IS NULL`** — the exactly-once dedupe key surviving
    /// the crash gap between Discord POST and this txn (P9).
    #[expect(
        clippy::too_many_arguments,
        reason = "DB row carriers; each parameter is a column"
    )]
    pub async fn deliver_report(
        &self,
        id: i64,
        to: State,
        report_msg_id: &str,
        outcome_kind: &str,
        report_full: Option<&str>,
        footer_json: &str,
        ended_at: i64,
    ) -> Result<bool, StoreError> {
        let writes = vec![
            (
                Column::ReportMessageId,
                WriteValue::Text(report_msg_id.to_owned()),
            ),
            (
                Column::ReportFull,
                WriteValue::Text(report_full.unwrap_or_default().to_owned()),
            ),
            (Column::FooterJson, WriteValue::Text(footer_json.to_owned())),
            (
                Column::OutcomeKind,
                WriteValue::Text(outcome_kind.to_owned()),
            ),
            (Column::EndedAt, WriteValue::Int(ended_at)),
            (Column::LastEventAt, WriteValue::Int(ended_at)),
        ];
        self.request(|reply| StoreRequest::DeliverReport {
            id,
            to,
            writes,
            reply,
        })
        .await
    }

    /// The atomic send: staged→running with the lock acquisition, session
    /// binding, and the start record all in one transaction (spec P5).
    pub async fn begin_send(
        &self,
        action_id: i64,
        dir: &str,
        session_id: &str,
        start_head: Option<&str>,
        dirty: bool,
    ) -> Result<bool, StoreError> {
        self.request(|reply| StoreRequest::BeginSend {
            action_id,
            dir: dir.to_owned(),
            session_id: session_id.to_owned(),
            start_head: start_head.map(str::to_owned),
            dirty,
            reply,
        })
        .await
    }

    /// Update `prompt_sent` on a staged action (Edit & send path).
    pub async fn restage_prompt(&self, id: i64, prompt: &str) -> Result<bool, StoreError> {
        self.request(|reply| StoreRequest::RestagePrompt {
            id,
            prompt: prompt.to_owned(),
            reply,
        })
        .await
    }

    /// Update both `label` and `prompt_sent` on a staged action (the plan pick
    /// prefills the label, then re-renders the envelope — P12/P13).
    pub async fn restage_label_prompt(
        &self,
        id: i64,
        label: &str,
        prompt: &str,
    ) -> Result<bool, StoreError> {
        self.request(|reply| StoreRequest::RestageLabelPrompt {
            id,
            label: label.to_owned(),
            prompt: prompt.to_owned(),
            reply,
        })
        .await
    }

    /// Record the review-time template choice and the re-rendered prompt
    /// (`fresh`/`resume` toggle, FR-6/P13).
    pub async fn set_template_prompt(
        &self,
        id: i64,
        template: &str,
        prompt: &str,
    ) -> Result<bool, StoreError> {
        self.request(|reply| StoreRequest::SetTemplatePrompt {
            id,
            template: template.to_owned(),
            prompt: prompt.to_owned(),
            reply,
        })
        .await
    }

    /// Record a stall ping time (P3/FR-39: one ping per stall episode).
    pub async fn set_stall_ping(&self, id: i64, ts: i64) -> Result<(), StoreError> {
        self.request(|reply| StoreRequest::SetStallPing { id, ts, reply })
            .await
    }

    /// Insert a run row carrying an optional plan path (Phase 6; the P12 plan is
    /// input, recorded on the run, never tracked).
    pub async fn insert_run_with_plan(
        &self,
        dir: &str,
        kind: &str,
        plan_path: Option<&str>,
    ) -> Result<i64, StoreError> {
        self.request(|reply| StoreRequest::InsertRunWithPlan {
            dir: dir.to_owned(),
            kind: kind.to_owned(),
            plan_path: plan_path.map(str::to_owned),
            reply,
        })
        .await
    }

    /// The last selected/uploaded path for a user (browser + attachment
    /// draft).
    pub async fn draft_path(&self, user_id: u64) -> Result<Option<String>, StoreError> {
        self.request(|reply| StoreRequest::DraftPath { user_id, reply })
            .await
    }

    /// Remember the last selected/uploaded path for a user.
    pub async fn set_draft_path(&self, user_id: u64, path: &str) -> Result<(), StoreError> {
        self.request(|reply| StoreRequest::SetDraftPath {
            user_id,
            path: path.to_owned(),
            reply,
        })
        .await
    }

    /// Forget the draft path (consumed by a prompt).
    pub async fn clear_draft_path(&self, user_id: u64) -> Result<(), StoreError> {
        self.request(|reply| StoreRequest::ClearDraftPath { user_id, reply })
            .await
    }

    /// Mark a send attempt as failed: leave `running` without a report and
    /// release the directory lock.
    pub async fn fail_send(&self, id: i64, dir: &str, reason: &str) -> Result<(), StoreError> {
        self.request(|reply| StoreRequest::FailSend {
            id,
            dir: dir.to_owned(),
            reason: reason.to_owned(),
            reply,
        })
        .await
    }

    /// Set the log cursor for a session (P9 cache only).
    pub async fn upsert_cursor(&self, session_id: &str, seq: u64) -> Result<(), StoreError> {
        self.request(|reply| StoreRequest::UpsertCursor {
            session_id: session_id.to_owned(),
            seq,
            reply,
        })
        .await
    }

    /// The logged cursor for a session, if any.
    pub async fn cursor(&self, session_id: &str) -> Result<Option<CursorRow>, StoreError> {
        self.request(|reply| StoreRequest::Cursor {
            session_id: session_id.to_owned(),
            reply,
        })
        .await
    }

    /// The most recent directories (for `/run` modal prefill).
    pub async fn recent_dirs(&self, limit: u64) -> Result<Vec<String>, StoreError> {
        self.request(|reply| StoreRequest::RecentDirs { limit, reply })
            .await
    }

    /// The remembered cwd for a user (modal prefill). A row created by
    /// `set_draft_path` alone leaves `cwd` NULL, so the read is nullable.
    pub async fn ui_cwd(&self, user_id: u64) -> Result<Option<String>, StoreError> {
        self.request(|reply| StoreRequest::UiCwd { user_id, reply })
            .await
    }

    /// Remember a cwd for a user.
    pub async fn set_ui_cwd(&self, user_id: u64, cwd: &str) -> Result<(), StoreError> {
        self.request(|reply| StoreRequest::SetUiCwd {
            user_id,
            cwd: cwd.to_owned(),
            reply,
        })
        .await
    }

    /// Record an audit line (FR-5).
    pub async fn audit(
        &self,
        user_id: u64,
        verb: &str,
        action_id: Option<i64>,
        result: &str,
    ) -> Result<(), StoreError> {
        self.request(|reply| StoreRequest::Audit {
            user_id,
            verb: verb.to_owned(),
            action_id,
            result: result.to_owned(),
            reply,
        })
        .await
    }

    /// Number of actions per state (metrics feed).
    pub async fn action_counts_by_state(&self) -> Result<Vec<(String, i64)>, StoreError> {
        self.request(|reply| StoreRequest::ActionCountsByState { reply })
            .await
    }

    /// Insert the staged successor action of a park-redo (spec CP-P4): same
    /// run, same stored prompt, incremented attempt, parented to the parked
    /// action. Returns the new id.
    #[expect(
        clippy::too_many_arguments,
        reason = "one column per argument mirrors the insert; a struct would not reduce the call-site clarity"
    )]
    pub async fn insert_staged_child(
        &self,
        run_id: i64,
        channel_id: Option<i64>,
        label: &str,
        prompt_sent: &str,
        template: &str,
        attempt: i64,
        parent_action_id: i64,
    ) -> Result<i64, StoreError> {
        self.request(|reply| StoreRequest::InsertStagedChild {
            run_id,
            channel_id,
            label: label.to_owned(),
            prompt_sent: prompt_sent.to_owned(),
            template: template.to_owned(),
            attempt,
            parent_action_id,
            reply,
        })
        .await
    }

    /// Release a directory lock only if `holder` currently owns it (P5). A
    /// stale holder (another action already took over) is a no-op.
    pub async fn release_lock(&self, dir: &str, holder: &str) -> Result<bool, StoreError> {
        self.request(|reply| StoreRequest::ReleaseLock {
            dir: dir.to_owned(),
            holder: holder.to_owned(),
            reply,
        })
        .await
    }

    /// Clear the promptable session for a directory (FR-30). Park and reset
    /// retire the session so a stale session is readable but never prompted.
    pub async fn retire_active_session(&self, dir: &str) -> Result<(), StoreError> {
        self.request(|reply| StoreRequest::RetireActiveSession {
            dir: dir.to_owned(),
            reply,
        })
        .await
    }

    /// Record a reset intent (CP-R0) and return its id.
    pub async fn record_reset_intent(
        &self,
        dir: &str,
        target_sha: &str,
        sup_branch: &str,
    ) -> Result<i64, StoreError> {
        self.request(|reply| StoreRequest::RecordResetIntent {
            dir: dir.to_owned(),
            target_sha: target_sha.to_owned(),
            sup_branch: sup_branch.to_owned(),
            reply,
        })
        .await
    }

    /// Every unfinished reset intent (boot reconcile, P9 step 5).
    pub async fn pending_reset_intents(&self) -> Result<Vec<ResetIntentRow>, StoreError> {
        self.request(|reply| StoreRequest::PendingResetIntents { reply })
            .await
    }

    /// Advance a reset intent's step (write-ahead; the intent row survives a
    /// crash so the next boot rolls forward).
    pub async fn set_reset_step(&self, id: i64, step: i64) -> Result<bool, StoreError> {
        self.request(|reply| StoreRequest::SetResetStep { id, step, reply })
            .await
    }

    /// Remove a completed reset intent.
    pub async fn delete_reset_intent(&self, id: i64) -> Result<(), StoreError> {
        self.request(|reply| StoreRequest::DeleteResetIntent { id, reply })
            .await
    }

    // == gate lifecycle (Phase 4, spec P4) =====================================

    /// Mirror a server-side gate fact into the DB. Returns the local rowid and
    /// whether the row was inserted (idempotent by `ext_id`; same fact twice ⇒
    /// one row, one message, AC-10/FR-23). `now` seeds the re-ping clock so the
    /// first re-ping is due only after the full interval.
    #[expect(
        clippy::too_many_arguments,
        reason = "gate columns are a fixed set of carriers; a builder would obscure the insert shape"
    )]
    pub async fn upsert_gate(
        &self,
        ext_id: &str,
        action_id: Option<i64>,
        session_id: Option<&str>,
        root_session_id: Option<&str>,
        kind: &str,
        payload_json: &str,
        renderable: bool,
        channel_id: Option<i64>,
        now: i64,
    ) -> Result<(i64, bool), StoreError> {
        self.request(|reply| StoreRequest::UpsertGate {
            ext_id: ext_id.to_owned(),
            action_id,
            session_id: session_id.map(str::to_owned),
            root_session_id: root_session_id.map(str::to_owned),
            kind: kind.to_owned(),
            payload_json: payload_json.to_owned(),
            renderable,
            channel_id,
            now,
            reply,
        })
        .await
    }

    /// A gate row by local id.
    pub async fn gate(&self, id: i64) -> Result<Option<GateRow>, StoreError> {
        self.request(|reply| StoreRequest::Gate { id, reply }).await
    }

    /// Every pending gate row (the re-ping cursor).
    pub async fn pending_gates(&self) -> Result<Vec<GateRow>, StoreError> {
        self.request(|reply| StoreRequest::PendingGates { reply })
            .await
    }

    /// The single gate state transition (P4: one lifecycle, one exactly-once
    /// reply rule). `answer` is the JSON-encoded decision/answer.
    pub async fn cas_gate(
        &self,
        id: i64,
        from: &str,
        to: &str,
        answer: Option<&str>,
    ) -> Result<bool, StoreError> {
        self.request(|reply| StoreRequest::CasGate {
            id,
            from: from.to_owned(),
            to: to.to_owned(),
            answer: answer.map(str::to_owned),
            reply,
        })
        .await
    }

    /// Record the posted gate message (P10: already-posted gates are never
    /// re-posted; the nullable guard is the dedupe).
    pub async fn set_gate_msg(
        &self,
        id: i64,
        msg_id: &str,
        channel_id: Option<i64>,
    ) -> Result<(), StoreError> {
        self.request(|reply| StoreRequest::SetGateMsg {
            id,
            msg_id: msg_id.to_owned(),
            channel_id,
            reply,
        })
        .await
    }

    /// Advance the re-ping counter and stamp the ping time; returns the new
    /// count (the caller decides the message after the 6th ping).
    pub async fn bump_gate_pings(&self, id: i64, now: i64) -> Result<i64, StoreError> {
        self.request(|reply| StoreRequest::BumpGatePings { id, now, reply })
            .await
    }

    /// Every answered gate with a recorded answer (boot reconcile re-POSTs any
    /// that are still pending on the server, FR-23).
    pub async fn answered_gates(&self) -> Result<Vec<GateRow>, StoreError> {
        self.request(|reply| StoreRequest::AnsweredGates { reply })
            .await
    }

    /// Set the blocked substatus on an action (P4: `blocked` is a substatus on
    /// `running`, never a state).
    pub async fn set_action_blocked(&self, action_id: i64, gate_id: i64) -> Result<(), StoreError> {
        self.request(|reply| StoreRequest::SetActionBlocked {
            action_id,
            gate_id,
            reply,
        })
        .await
    }

    /// Clear the blocked substatus, but only when this gate is the blocker
    /// (a concurrent gate on the same action keeps its block).
    pub async fn clear_action_blocked(
        &self,
        action_id: i64,
        gate_id: i64,
    ) -> Result<(), StoreError> {
        self.request(|reply| StoreRequest::ClearActionBlocked {
            action_id,
            gate_id,
            reply,
        })
        .await
    }

    // == session lifecycle (Phase 5, spec P5/P14/FR-30) =======================

    /// Record the promptable session for a directory (FR-30). `/run` seeds it
    /// when creating a session; park/reset retire it.
    pub async fn set_active_session(&self, dir: &str, session_id: &str) -> Result<(), StoreError> {
        self.request(|reply| StoreRequest::SetActiveSession {
            dir: dir.to_owned(),
            session_id: session_id.to_owned(),
            reply,
        })
        .await
    }

    /// Take the directory lock on behalf of an *external* (TUI) session
    /// observed busy in the directory (P5 `held(external:<id>)`).
    pub async fn set_external_lock(
        &self,
        dir: &str,
        session_id: &str,
        since: i64,
    ) -> Result<(), StoreError> {
        self.request(|reply| StoreRequest::SetExternalLock {
            dir: dir.to_owned(),
            session_id: session_id.to_owned(),
            since,
            reply,
        })
        .await
    }

    /// The running action targeting a directory, if any (the mutex owner).
    pub async fn running_action_for_dir(&self, dir: &str) -> Result<Option<ActionRow>, StoreError> {
        self.request(|reply| StoreRequest::RunningActionForDir {
            dir: dir.to_owned(),
            reply,
        })
        .await
    }

    /// The directory a seen session is bound to (inbox adoption resolves the
    /// worktree this way, P14).
    pub async fn session_dir(&self, session_id: &str) -> Result<Option<String>, StoreError> {
        self.request(|reply| StoreRequest::SessionDir {
            session_id: session_id.to_owned(),
            reply,
        })
        .await
    }

    /// Every session the bot owns (bound to an action, or the promptable
    /// session of a project) — the complement of `session_seen` is *external*.
    pub async fn known_bot_sessions(&self) -> Result<Vec<String>, StoreError> {
        self.request(|reply| StoreRequest::KnownBotSessions { reply })
            .await
    }

    /// Claim a completion under `(session_id, seq)` *before* posting. Returns
    /// `false` when that exact completion was already claimed (the inbox dedupe
    /// key, FR-32/P14) — the caller must not post. A transient post failure can
    /// `unclaim` so the next tick retries (never duplicates, retries on loss).
    pub async fn claim_notified(&self, session_id: &str, seq: &str) -> Result<bool, StoreError> {
        self.request(|reply| StoreRequest::ClaimNotified {
            session_id: session_id.to_owned(),
            seq: seq.to_owned(),
            reply,
        })
        .await
    }

    /// Record the posted message id on a claimed notification.
    pub async fn finish_notified(
        &self,
        session_id: &str,
        seq: &str,
        discord_msg_id: &str,
    ) -> Result<(), StoreError> {
        self.request(|reply| StoreRequest::FinishNotified {
            session_id: session_id.to_owned(),
            seq: seq.to_owned(),
            discord_msg_id: discord_msg_id.to_owned(),
            reply,
        })
        .await
    }

    /// Give a claimed-but-unposted notification back so the next tick retries.
    pub async fn unclaim_notified(&self, session_id: &str, seq: &str) -> Result<(), StoreError> {
        self.request(|reply| StoreRequest::UnclaimNotified {
            session_id: session_id.to_owned(),
            seq: seq.to_owned(),
            reply,
        })
        .await
    }

    /// Record `discord_msg_id → session` so a reply to an inbox report can
    /// continue that session (P14 / FR-32).
    pub async fn insert_msg_map(
        &self,
        discord_msg_id: &str,
        session_id: &str,
    ) -> Result<(), StoreError> {
        self.request(|reply| StoreRequest::InsertMsgMap {
            discord_msg_id: discord_msg_id.to_owned(),
            session_id: session_id.to_owned(),
            reply,
        })
        .await
    }

    /// Resolve a replied-to Discord message to its session.
    pub async fn lookup_msg_map(
        &self,
        discord_msg_id: &str,
    ) -> Result<Option<(String, Option<i64>)>, StoreError> {
        self.request(|reply| StoreRequest::LookupMsgMap {
            discord_msg_id: discord_msg_id.to_owned(),
            reply,
        })
        .await
    }

    /// Whether `kind`/`key` is muted (FR-33).
    pub async fn is_muted(&self, kind: &str, key: &str) -> Result<bool, StoreError> {
        self.request(|reply| StoreRequest::IsMuted {
            kind: kind.to_owned(),
            key: key.to_owned(),
            reply,
        })
        .await
    }

    /// Toggle a mute; returns the new state (`true` = now muted).
    pub async fn toggle_mute(&self, kind: &str, key: &str) -> Result<bool, StoreError> {
        self.request(|reply| StoreRequest::ToggleMute {
            kind: kind.to_owned(),
            key: key.to_owned(),
            reply,
        })
        .await
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
    let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    values.push(Box::new(to.as_str()));
    for (column, value) in writes {
        let _ = write!(sql, ", {} = ?{}", column.sql(), values.len() + 1);
        match value {
            WriteValue::Null => values.push(Box::new(Option::<String>::None)),
            WriteValue::Text(text) => values.push(Box::new(text.clone())),
            WriteValue::Int(integer) => values.push(Box::new(integer.to_owned())),
        }
    }
    sql.push_str(" WHERE id = ?");
    values.push(Box::new(id));
    sql.push_str(" AND state = ?");
    values.push(Box::new(from.as_str()));
    if let Some(condition) = extra_condition {
        let _ = write!(sql, " AND {condition}");
    }
    let references: Vec<&dyn rusqlite::ToSql> = values.iter().map(AsRef::as_ref).collect();
    let params = rusqlite::params_from_iter(references.iter().copied());
    let changed = connection.execute(&sql, params)?;
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
    let mut values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
    values.push(Box::new(to.as_str()));
    for (column, value) in writes {
        let _ = write!(sql, ", {} = ?{}", column.sql(), values.len() + 1);
        match value {
            WriteValue::Null => values.push(Box::new(Option::<String>::None)),
            WriteValue::Text(text) => values.push(Box::new(text.clone())),
            WriteValue::Int(integer) => values.push(Box::new(integer.to_owned())),
        }
    }
    sql.push_str(" WHERE id = ?");
    values.push(Box::new(id));
    for from in froms {
        values.push(Box::new(from.clone()));
    }
    let _ = write!(sql, " AND {extra_condition}");
    let references: Vec<&dyn rusqlite::ToSql> = values.iter().map(AsRef::as_ref).collect();
    let params = rusqlite::params_from_iter(references.iter().copied());
    let changed = connection.execute(&sql, params)?;
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
        channel_id: row.get("channel_id")?,
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
