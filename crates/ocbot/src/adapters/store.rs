//! The SQLite store actor (spec P15/P18): one connection on a dedicated OS
//! thread behind a message-passing handle. Single writer ⇒ CAS transitions
//! (`rows_changed`) are race-free; the actor abstracts rusqlite so the rest of
//! the bot never holds a lock across an `.await`.
//!
//! Migration v1 creates the full §6.3 schema. One deliberate Phase 2 delta is
//! recorded below: `action.channel_id` — the bot must know where to deliver a
//! report (P10 says reports are new messages in the originating channel).

use std::any::Any;
use std::path::Path;
use std::sync::Arc;

use crossbeam_channel::{Receiver, Select, Sender, unbounded};
use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::oneshot;

use ocbot_core::{Column, State, WriteValue};

/// Errors surfaced by the store actor.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A SQLite error.
    #[error("store: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The actor is shut down.
    #[error("store is closed")]
    Closed,
    /// A reply was not of the expected type (internal invariant).
    #[error("store reply type mismatch")]
    TypeMismatch,
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

type Job = Box<dyn FnOnce(&mut Connection) -> rusqlite::Result<Box<dyn Any + Send>> + Send>;
type Request = (
    Job,
    oneshot::Sender<Result<Box<dyn Any + Send>, StoreError>>,
);

/// The actor handle: cheap to clone, no direct connection access.
#[derive(Clone)]
pub struct StoreHandle {
    inner: Arc<StoreInner>,
}

struct StoreInner {
    tx: Sender<Request>,
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
        let (tx, rx) = unbounded::<Request>();
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
        let dir = dir.to_owned();
        self.call(move |connection| {
            connection
                .query_row(
                    "SELECT lock_holder FROM project WHERE dir = ?1",
                    params![dir],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
                .map(Option::flatten)
        })
        .await
    }

    /// Run a closure against the actor's connection. The closure is moved to
    /// the actor thread and results are awaited.
    pub async fn call<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        let job: Job = Box::new(move |connection| {
            f(connection).map(|value| {
                let boxed: Box<dyn Any + Send> = Box::new(value);
                boxed
            })
        });
        self.inner
            .tx
            .send((job, reply_tx))
            .map_err(|_| StoreError::Closed)?;
        let reply = reply_rx.await.map_err(|_| StoreError::Closed)?;
        let value = reply?;
        value
            .downcast::<T>()
            .map(|boxed| *boxed)
            .map_err(|_| StoreError::TypeMismatch)
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
    rx: &Receiver<Request>,
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
            Ok((job, reply)) => {
                let result = job(connection).map_err(StoreError::from);
                // A failed oneshot just means the caller lost interest.
                let _ = reply.send(result);
            }
            Err(_) => break, // all senders gone
        }
    }
    let _ = connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
    let _ = done_tx.send(());
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

const SCHEMA_VERSION: i64 = 3;

fn migrate(connection: &mut Connection) -> Result<(), StoreError> {
    connection
        .execute_batch("CREATE TABLE IF NOT EXISTS schema_version (v INTEGER PRIMARY KEY);")?;
    let current: Option<i64> = connection
        .query_row(
            "SELECT v FROM schema_version ORDER BY v DESC LIMIT 1",
            [],
            |row| row.get(0),
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
        let dir = dir.to_owned();
        let work_branch = work_branch.map(str::to_owned);
        let base_sha = base_sha.map(str::to_owned);
        self.call(move |connection| {
            connection.execute(
                "INSERT INTO project (dir, work_branch, base_sha)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(dir) DO UPDATE SET
                    work_branch = excluded.work_branch,
                    base_sha = excluded.base_sha",
                params![dir, work_branch, base_sha],
            )?;
            Ok(())
        })
        .await
    }

    /// Project by directory.
    pub async fn project(&self, dir: &str) -> Result<Option<ProjectRow>, StoreError> {
        let dir = dir.to_owned();
        self.call(move |connection| {
            connection
                .query_row(
                    "SELECT dir, work_branch, base_sha, active_session_id, lock_holder, lock_since
                     FROM project WHERE dir = ?1",
                    params![dir],
                    project_row,
                )
                .optional()
        })
        .await
    }

    /// Every project that currently holds a lock (for `/status`).
    pub async fn locked_projects(&self) -> Result<Vec<ProjectRow>, StoreError> {
        self.call(|connection| {
            let mut statement = connection.prepare(
                "SELECT dir, work_branch, base_sha, active_session_id, lock_holder, lock_since
                 FROM project WHERE lock_holder IS NOT NULL ORDER BY dir",
            )?;
            let rows = statement
                .query_map([], project_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    /// Insert a run row; returns its id.
    pub async fn insert_run(&self, dir: &str, kind: &str) -> Result<i64, StoreError> {
        let dir = dir.to_owned();
        let kind = kind.to_owned();
        let now = now_unix();
        self.call(move |connection| {
            connection.execute(
                "INSERT INTO run (dir, kind, created_at) VALUES (?1, ?2, ?3)",
                params![dir, kind, now],
            )?;
            Ok(connection.last_insert_rowid())
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
        let label = label.to_owned();
        let prompt_sent = prompt_sent.to_owned();
        self.call(move |connection| {
            connection.execute(
                "INSERT INTO action (run_id, channel_id, label, prompt_sent, state, attempt)
                 VALUES (?1, ?2, ?3, ?4, 'staged', ?5)",
                params![run_id, channel_id, label, prompt_sent, attempt],
            )?;
            Ok(connection.last_insert_rowid())
        })
        .await
    }

    /// A run row by id.
    pub async fn run(&self, id: i64) -> Result<Option<RunRow>, StoreError> {
        self.call(move |connection| {
            connection
                .query_row(
                    "SELECT id, dir, plan_path, kind FROM run WHERE id = ?1",
                    params![id],
                    |row| {
                        Ok(RunRow {
                            id: row.get(0)?,
                            dir: row.get(1)?,
                            plan_path: row.get(2)?,
                            kind: row.get(3)?,
                        })
                    },
                )
                .optional()
        })
        .await
    }

    /// Record the staging-preview message id on an action (also seeds the
    /// origin for the quiet status reactions).
    pub async fn set_origin(&self, action_id: i64, origin: &str) -> Result<(), StoreError> {
        let origin = origin.to_owned();
        self.call(move |connection| {
            connection.execute(
                "UPDATE action SET origin_msg_id = ?1 WHERE id = ?2 AND origin_msg_id IS NULL",
                params![origin, action_id],
            )?;
            Ok(())
        })
        .await
    }

    /// Mark a session as seen (Phase 5 inbox bookkeeping, seeded from Phase 2).
    pub async fn upsert_seen(
        &self,
        session_id: &str,
        directory: Option<&str>,
    ) -> Result<(), StoreError> {
        let session_id = session_id.to_owned();
        let directory = directory.map(str::to_owned);
        self.call(move |connection| {
            connection.execute(
                "INSERT INTO session_seen (session_id, dir, first_seen) VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id) DO NOTHING",
                params![session_id, directory, now_unix()],
            )?;
            Ok(())
        })
        .await
    }

    /// Confirm the operator chose to include existing dirty changes (FR-27).
    pub async fn mark_include_existing(&self, action_id: i64) -> Result<bool, StoreError> {
        self.call(move |connection| {
            let changed = connection.execute(
                "UPDATE action SET include_existing = 1 WHERE id = ?1 AND state = 'staged'",
                params![action_id],
            )?;
            Ok(changed == 1)
        })
        .await
    }

    /// An action by id.
    pub async fn action(&self, id: i64) -> Result<Option<ActionRow>, StoreError> {
        self.call(move |connection| {
            connection
                .query_row(
                    "SELECT * FROM action WHERE id = ?1",
                    params![id],
                    action_row,
                )
                .optional()
        })
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
        let names: Vec<String> = states
            .iter()
            .map(|state| state.as_str().to_owned())
            .collect();
        self.call(move |connection| {
            if names.is_empty() {
                return Ok(Vec::new());
            }
            let placeholders = vec!["?"; names.len()].join(",");
            let sql = format!("SELECT * FROM action WHERE state IN ({placeholders}) ORDER BY id");
            let mut statement = connection.prepare(&sql)?;
            let rows = statement
                .query_map(rusqlite::params_from_iter(names.iter()), action_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    /// Every action belonging to a directory, oldest first (history merge, P8).
    pub async fn actions_for_dir(&self, dir: &str) -> Result<Vec<ActionRow>, StoreError> {
        let dir = dir.to_owned();
        self.call(move |connection| {
            let mut statement = connection.prepare(
                "SELECT action.* FROM action
                 JOIN run ON action.run_id = run.id
                 WHERE run.dir = ?1 ORDER BY action.id",
            )?;
            let rows = statement
                .query_map(params![dir], action_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    /// The most recent accepted sha in a directory, if any (park's fallback
    /// reset target, spec P7 step 3).
    pub async fn last_accept_sha(&self, dir: &str) -> Result<Option<String>, StoreError> {
        let dir = dir.to_owned();
        self.call(move |connection| {
            connection
                .query_row(
                    "SELECT action.accept_sha FROM action
                     JOIN run ON action.run_id = run.id
                     WHERE run.dir = ?1 AND action.accept_sha IS NOT NULL
                     ORDER BY action.id DESC LIMIT 1",
                    params![dir],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
                .map(Option::flatten)
        })
        .await
    }

    /// The running action that owns `session_id`, if any.
    pub async fn running_for_session(
        &self,
        session_id: &str,
    ) -> Result<Option<ActionRow>, StoreError> {
        let session_id = session_id.to_owned();
        self.call(move |connection| {
            connection
                .query_row(
                    "SELECT * FROM action WHERE session_id = ?1 AND state = 'running' ORDER BY id DESC LIMIT 1",
                    params![session_id],
                    action_row,
                )
                .optional()
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
        let writes = writes.to_vec();
        self.call(move |connection| cas_inner(connection, id, from, to, &writes, None))
            .await
    }

    /// A CAS whose `from` may be any of `froms` (park starts from reported,
    /// errored, or aborted; discard likewise — spec §6.4). Exactly one row
    /// changes or the caller lost the race.
    pub async fn cas_state_from_any(
        &self,
        id: i64,
        froms: &[State],
        to: State,
        writes: &[(Column, WriteValue)],
    ) -> Result<bool, StoreError> {
        let writes = writes.to_vec();
        let froms: Vec<String> = froms
            .iter()
            .map(|state| state.as_str().to_owned())
            .collect();
        self.call(move |connection| {
            if froms.is_empty() {
                return Ok(false);
            }
            let placeholders = vec!["?"; froms.len()].join(",");
            let extra = format!("state IN ({placeholders})");
            cas_inner_any(connection, id, &froms, to, &writes, &extra)
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
        self.call(move |connection| {
            cas_inner(
                connection,
                id,
                State::Running,
                to,
                &writes,
                Some("report_msg_id IS NULL"),
            )
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
        let dir = dir.to_owned();
        let session_id = session_id.to_owned();
        let start_head = start_head.map(str::to_owned);
        let now = now_unix();
        self.call(move |connection| {
            let transaction = connection.transaction()?;
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
                // by someone else, the whole transition must roll back.
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
        })
        .await
    }

    /// Update `prompt_sent` on a staged action (Edit & send path).
    pub async fn restage_prompt(&self, id: i64, prompt: &str) -> Result<bool, StoreError> {
        let prompt = prompt.to_owned();
        self.call(move |connection| {
            let changed = connection.execute(
                "UPDATE action SET prompt_sent = ?1 WHERE id = ?2 AND state = 'staged'",
                params![prompt, id],
            )?;
            Ok(changed == 1)
        })
        .await
    }

    /// Mark a send attempt as failed: leave `running` without a report and
    /// release the directory lock.
    pub async fn fail_send(&self, id: i64, dir: &str, reason: &str) -> Result<(), StoreError> {
        let dir = dir.to_owned();
        let reason = reason.to_owned();
        self.call(move |connection| {
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
        })
        .await
    }

    /// Set the log cursor for a session (P9 cache only).
    pub async fn upsert_cursor(&self, session_id: &str, seq: u64) -> Result<(), StoreError> {
        let session_id = session_id.to_owned();
        let now = now_unix();
        self.call(move |connection| {
            connection.execute(
                "INSERT INTO session_cursor (session_id, last_seq, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id) DO UPDATE SET last_seq = excluded.last_seq, updated_at = excluded.updated_at",
                params![session_id, to_i64(seq), now],
            )?;
            Ok(())
        })
        .await
    }

    /// The logged cursor for a session, if any.
    pub async fn cursor(&self, session_id: &str) -> Result<Option<CursorRow>, StoreError> {
        let session_id = session_id.to_owned();
        self.call(move |connection| {
            connection
                .query_row(
                    "SELECT last_seq FROM session_cursor WHERE session_id = ?1",
                    params![session_id],
                    |row| {
                        row.get::<_, i64>(0).map(|seq| CursorRow {
                            last_seq: to_u64(seq),
                        })
                    },
                )
                .optional()
        })
        .await
    }

    /// The most recent directories (for `/run` modal prefill).
    pub async fn recent_dirs(&self, limit: u64) -> Result<Vec<String>, StoreError> {
        self.call(move |connection| {
            let mut statement = connection.prepare(
                "SELECT dir FROM project ORDER BY (
                    SELECT MAX(created_at) FROM run WHERE run.dir = project.dir
                 ) DESC LIMIT ?1",
            )?;
            let rows = statement
                .query_map(params![to_i64(limit)], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    /// The remembered cwd for a user (modal prefill).
    pub async fn ui_cwd(&self, user_id: u64) -> Result<Option<String>, StoreError> {
        self.call(move |connection| {
            connection
                .query_row(
                    "SELECT cwd FROM ui_state WHERE user_id = ?1",
                    params![user_id.to_string()],
                    |row| row.get(0),
                )
                .optional()
        })
        .await
    }

    /// Remember a cwd for a user.
    pub async fn set_ui_cwd(&self, user_id: u64, cwd: &str) -> Result<(), StoreError> {
        let cwd = cwd.to_owned();
        self.call(move |connection| {
            connection.execute(
                "INSERT INTO ui_state (user_id, cwd) VALUES (?1, ?2)
                 ON CONFLICT(user_id) DO UPDATE SET cwd = excluded.cwd",
                params![user_id.to_string(), cwd],
            )?;
            Ok(())
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
        let verb = verb.to_owned();
        let result = result.to_owned();
        self.call(move |connection| {
            connection.execute(
                "INSERT INTO audit (ts, user_id, verb, action_id, result) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![now_unix(), user_id.to_string(), verb, action_id, result],
            )?;
            Ok(())
        })
        .await
    }

    /// Number of actions per state (metrics feed).
    pub async fn action_counts_by_state(&self) -> Result<Vec<(String, i64)>, StoreError> {
        self.call(|connection| {
            let mut statement =
                connection.prepare("SELECT state, COUNT(*) FROM action GROUP BY state")?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    /// Insert the staged successor action of a park-redo (spec CP-P4): same
    /// run, same stored prompt, incremented attempt, parented to the parked
    /// action. Returns the new id.
    pub async fn insert_staged_child(
        &self,
        run_id: i64,
        channel_id: Option<i64>,
        label: &str,
        prompt_sent: &str,
        attempt: i64,
        parent_action_id: i64,
    ) -> Result<i64, StoreError> {
        let label = label.to_owned();
        let prompt_sent = prompt_sent.to_owned();
        self.call(move |connection| {
            connection.execute(
                "INSERT INTO action
                    (run_id, channel_id, label, prompt_sent, state, attempt, parent_action_id)
                 VALUES (?1, ?2, ?3, ?4, 'staged', ?5, ?6)",
                params![
                    run_id,
                    channel_id,
                    label,
                    prompt_sent,
                    attempt,
                    parent_action_id
                ],
            )?;
            Ok(connection.last_insert_rowid())
        })
        .await
    }

    /// Release a directory lock only if `holder` currently owns it (P5). A
    /// stale holder (another action already took over) is a no-op.
    pub async fn release_lock(&self, dir: &str, holder: &str) -> Result<bool, StoreError> {
        let dir = dir.to_owned();
        let holder = holder.to_owned();
        self.call(move |connection| {
            let changed = connection.execute(
                "UPDATE project SET lock_holder = NULL, lock_since = NULL
                 WHERE dir = ?1 AND lock_holder = ?2",
                params![dir, holder],
            )?;
            Ok(changed == 1)
        })
        .await
    }

    /// Clear the promptable session for a directory (FR-30). Park and reset
    /// retire the session so a stale session is readable but never prompted.
    pub async fn retire_active_session(&self, dir: &str) -> Result<(), StoreError> {
        let dir = dir.to_owned();
        self.call(move |connection| {
            connection.execute(
                "UPDATE project SET active_session_id = NULL WHERE dir = ?1",
                params![dir],
            )?;
            Ok(())
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
        let dir = dir.to_owned();
        let target_sha = target_sha.to_owned();
        let sup_branch = sup_branch.to_owned();
        let now = now_unix();
        self.call(move |connection| {
            connection.execute(
                "INSERT INTO reset_intent (dir, target_sha, sup_branch, step, created_at)
                 VALUES (?1, ?2, ?3, 0, ?4)",
                params![dir, target_sha, sup_branch, now],
            )?;
            Ok(connection.last_insert_rowid())
        })
        .await
    }

    /// Every unfinished reset intent (boot reconcile, P9 step 5).
    pub async fn pending_reset_intents(&self) -> Result<Vec<ResetIntentRow>, StoreError> {
        self.call(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, dir, target_sha, sup_branch, step FROM reset_intent ORDER BY id",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok(ResetIntentRow {
                        id: row.get(0)?,
                        dir: row.get(1)?,
                        target_sha: row.get(2)?,
                        sup_branch: row.get(3)?,
                        step: row.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    /// Advance a reset intent's step (write-ahead; the intent row survives a
    /// crash so the next boot rolls forward).
    pub async fn set_reset_step(&self, id: i64, step: i64) -> Result<bool, StoreError> {
        self.call(move |connection| {
            let changed = connection.execute(
                "UPDATE reset_intent SET step = ?1 WHERE id = ?2",
                params![step, id],
            )?;
            Ok(changed == 1)
        })
        .await
    }

    /// Remove a completed reset intent.
    pub async fn delete_reset_intent(&self, id: i64) -> Result<(), StoreError> {
        self.call(move |connection| {
            connection.execute("DELETE FROM reset_intent WHERE id = ?1", params![id])?;
            Ok(())
        })
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
        let ext = ext_id.to_owned();
        let kind = kind.to_owned();
        let payload = payload_json.to_owned();
        let session = session_id.map(str::to_owned);
        let root = root_session_id.map(str::to_owned);
        self.call(move |connection| {
            let transaction = connection.transaction()?;
            let existing: Option<i64> = transaction
                .query_row(
                    "SELECT id FROM gate WHERE ext_id = ?1",
                    params![ext],
                    |row| row.get(0),
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
                        ext, action_id, session, root, kind, payload, renderable, channel_id, now
                    ],
                )?;
                let id = transaction.last_insert_rowid();
                transaction.commit()?;
                Ok((id, true))
            }
        })
        .await
    }

    /// A gate row by local id.
    pub async fn gate(&self, id: i64) -> Result<Option<GateRow>, StoreError> {
        self.call(move |connection| {
            connection
                .query_row("SELECT * FROM gate WHERE id = ?1", params![id], gate_row)
                .optional()
        })
        .await
    }

    /// Every pending gate row (the re-ping cursor).
    pub async fn pending_gates(&self) -> Result<Vec<GateRow>, StoreError> {
        self.call(|connection| {
            let mut statement =
                connection.prepare("SELECT * FROM gate WHERE state = 'pending' ORDER BY id")?;
            let rows = statement
                .query_map([], gate_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
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
        let from = from.to_owned();
        let to = to.to_owned();
        let answer = answer.map(str::to_owned);
        self.call(move |connection| {
            let changed = connection.execute(
                "UPDATE gate SET state = ?1, answer = ?2 WHERE id = ?3 AND state = ?4",
                params![to, answer, id, from],
            )?;
            Ok(changed == 1)
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
        let msg_id = msg_id.to_owned();
        self.call(move |connection| {
            connection.execute(
                "UPDATE gate SET msg_id = ?1, channel_id = ?2 WHERE id = ?3 AND msg_id IS NULL",
                params![msg_id, channel_id, id],
            )?;
            Ok(())
        })
        .await
    }

    /// Advance the re-ping counter and stamp the ping time; returns the new
    /// count (the caller decides the message after the 6th ping).
    pub async fn bump_gate_pings(&self, id: i64, now: i64) -> Result<i64, StoreError> {
        self.call(move |connection| {
            connection.execute(
                "UPDATE gate SET pings = pings + 1, last_ping_at = ?1 WHERE id = ?2",
                params![now, id],
            )?;
            let pings: i64 = connection.query_row(
                "SELECT pings FROM gate WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )?;
            Ok(pings)
        })
        .await
    }

    /// Every answered gate with a recorded answer (boot reconcile re-POSTs any
    /// that are still pending on the server, FR-23).
    pub async fn answered_gates(&self) -> Result<Vec<GateRow>, StoreError> {
        self.call(|connection| {
            let mut statement = connection.prepare(
                "SELECT * FROM gate WHERE state = 'answered' AND answer IS NOT NULL ORDER BY id",
            )?;
            let rows = statement
                .query_map([], gate_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
    }

    /// Set the blocked substatus on an action (P4: `blocked` is a substatus on
    /// `running`, never a state).
    pub async fn set_action_blocked(&self, action_id: i64, gate_id: i64) -> Result<(), StoreError> {
        self.call(move |connection| {
            connection.execute(
                "UPDATE action SET blocked_gate_id = ?1 WHERE id = ?2",
                params![gate_id, action_id],
            )?;
            Ok(())
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
        self.call(move |connection| {
            connection.execute(
                "UPDATE action SET blocked_gate_id = NULL WHERE id = ?1 AND blocked_gate_id = ?2",
                params![action_id, gate_id],
            )?;
            Ok(())
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

fn project_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProjectRow> {
    Ok(ProjectRow {
        dir: row.get(0)?,
        work_branch: row.get(1)?,
        base_sha: row.get(2)?,
        active_session_id: row.get(3)?,
        lock_holder: row.get(4)?,
        lock_since: row.get(5)?,
    })
}

fn action_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ActionRow> {
    let state: String = row.get(7)?;
    let state: State = state.parse().map_err(|error: String| {
        rusqlite::Error::FromSqlConversionFailure(
            7,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(error)),
        )
    })?;
    Ok(ActionRow {
        id: row.get(0)?,
        run_id: row.get(1)?,
        session_id: row.get(2)?,
        parent_action_id: row.get(3)?,
        channel_id: row.get(4)?,
        label: row.get(5)?,
        prompt_sent: row.get(6)?,
        state,
        blocked_gate_id: row.get(8)?,
        outcome_kind: row.get(9)?,
        report_full: row.get(10)?,
        footer_json: row.get(11)?,
        report_msg_id: row.get(12)?,
        origin_msg_id: row.get(13)?,
        start_head: row.get(14)?,
        dirty_at_start: row.get::<_, i64>(15)? != 0,
        include_existing: row.get::<_, i64>(16)? != 0,
        pending_commit_subject: row.get(17)?,
        pending_commit_body: row.get(18)?,
        park_branch_name: row.get(19)?,
        park_step: row.get(20)?,
        accept_sha: row.get(21)?,
        attempt: row.get(22)?,
        followups: row.get(23)?,
        started_at: row.get(24)?,
        ended_at: row.get(25)?,
        last_event_at: row.get(26)?,
        stall_ping_at: row.get(27)?,
    })
}

fn gate_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<GateRow> {
    Ok(GateRow {
        id: row.get(0)?,
        ext_id: row.get(1)?,
        action_id: row.get(2)?,
        session_id: row.get(3)?,
        root_session_id: row.get(4)?,
        kind: row.get(5)?,
        payload_json: row.get(6)?,
        renderable: row.get::<_, i64>(7)? != 0,
        state: row.get(8)?,
        answer: row.get(9)?,
        msg_id: row.get(10)?,
        pings: row.get(11)?,
        last_ping_at: row.get(12)?,
        channel_id: row.get(13)?,
    })
}

fn now_unix() -> i64 {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    to_i64(seconds)
}

/// Saturating `u64 → i64` for timestamps and cursor seqs (bounded domains).
fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Saturating `i64 → u64` for cursor reads.
fn to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}
