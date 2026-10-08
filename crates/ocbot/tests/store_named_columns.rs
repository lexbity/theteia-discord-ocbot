//! Named-column mapping regression tests for the store (SPEC-002 §4.2,
//! FR-7/FR-8/FR-9).
//!
//! The store maps `SELECT *` rows into `ProjectRow`/`ActionRow`/`GateRow`. The
//! mappings read every column by name, so a future migration that inserts a
//! column mid-table cannot silently shift values into the wrong fields. These
//! tests build tables whose physical column order is *reversed* (and tables
//! with a dropped column) and prove the mapping still reads the right field, or
//! fails loudly naming the column when it cannot.
//!
//! The `action`/`gate`/`project` column lists below are the test oracle: they
//! mirror the migration schema, not the source under test.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

use std::path::Path;

use ocbot::adapters::store::StoreHandle;
use ocbot_core::State;
use rusqlite::{Connection, params};

/// `(column, declaration)` for `action`, canonical order (migration v1 + v4).
const ACTION_COLUMNS: &[(&str, &str)] = &[
    ("id", "INTEGER PRIMARY KEY"),
    ("run_id", "INTEGER NOT NULL"),
    ("session_id", "TEXT"),
    ("parent_action_id", "INTEGER"),
    ("channel_id", "INTEGER"),
    ("label", "TEXT NOT NULL"),
    ("prompt_sent", "TEXT NOT NULL"),
    ("state", "TEXT NOT NULL"),
    ("blocked_gate_id", "INTEGER"),
    ("outcome_kind", "TEXT"),
    ("report_full", "TEXT"),
    ("footer_json", "TEXT"),
    ("report_msg_id", "TEXT"),
    ("origin_msg_id", "TEXT"),
    ("start_head", "TEXT"),
    ("dirty_at_start", "INTEGER NOT NULL DEFAULT 0"),
    ("include_existing", "INTEGER NOT NULL DEFAULT 0"),
    ("pending_commit_subject", "TEXT"),
    ("pending_commit_body", "TEXT"),
    ("park_branch_name", "TEXT"),
    ("park_step", "INTEGER NOT NULL DEFAULT 0"),
    ("accept_sha", "TEXT"),
    ("attempt", "INTEGER NOT NULL DEFAULT 1"),
    ("followups", "INTEGER NOT NULL DEFAULT 0"),
    ("started_at", "INTEGER"),
    ("ended_at", "INTEGER"),
    ("last_event_at", "INTEGER"),
    ("stall_ping_at", "INTEGER"),
    ("template", "TEXT NOT NULL DEFAULT 'fresh'"),
];

/// `(column, declaration)` for `gate`, canonical order (migration v1 + v3).
const GATE_COLUMNS: &[(&str, &str)] = &[
    ("id", "INTEGER PRIMARY KEY"),
    ("ext_id", "TEXT"),
    ("action_id", "INTEGER"),
    ("session_id", "TEXT"),
    ("root_session_id", "TEXT"),
    ("kind", "TEXT NOT NULL"),
    ("payload_json", "TEXT NOT NULL"),
    ("renderable", "INTEGER NOT NULL DEFAULT 1"),
    ("state", "TEXT NOT NULL"),
    ("answer", "TEXT"),
    ("msg_id", "TEXT"),
    ("pings", "INTEGER NOT NULL DEFAULT 0"),
    ("last_ping_at", "INTEGER"),
    ("channel_id", "INTEGER"),
];

/// `(column, declaration)` for `project`, canonical order (migration v1).
const PROJECT_COLUMNS: &[(&str, &str)] = &[
    ("dir", "TEXT PRIMARY KEY"),
    ("work_branch", "TEXT"),
    ("base_sha", "TEXT"),
    ("active_session_id", "TEXT"),
    ("lock_holder", "TEXT"),
    ("lock_since", "INTEGER"),
];

/// Build a `CREATE TABLE` from a named column list.
fn create_table(conn: &Connection, table: &str, columns: &[(&str, &str)]) {
    let body = columns
        .iter()
        .map(|(name, decl)| format!("{name} {decl}"))
        .collect::<Vec<_>>()
        .join(", ");
    conn.execute_batch(&format!("CREATE TABLE {table} ({body});"))
        .unwrap_or_else(|error| panic!("create {table}: {error}"));
}

/// Insert one fully-populated `action` row (id 7). Names every column, so the
/// table's physical order is irrelevant.
fn insert_action(conn: &Connection) {
    let names = ACTION_COLUMNS
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ");
    let placeholders = (1..=ACTION_COLUMNS.len())
        .map(|position| format!("?{position}"))
        .collect::<Vec<_>>()
        .join(", ");
    conn.execute(
        &format!("INSERT INTO action ({names}) VALUES ({placeholders})"),
        params![
            7,           // id
            11,          // run_id
            "ses_x",     // session_id
            3,           // parent_action_id
            42,          // channel_id
            "lbl",       // label
            "prompt",    // prompt_sent
            "running",   // state
            5,           // blocked_gate_id
            "completed", // outcome_kind
            "full",      // report_full
            "foot",      // footer_json
            "m1",        // report_msg_id
            "m0",        // origin_msg_id
            "head",      // start_head
            1,           // dirty_at_start
            1,           // include_existing
            "subj",      // pending_commit_subject
            "body",      // pending_commit_body
            "park",      // park_branch_name
            2,           // park_step
            "sha",       // accept_sha
            4,           // attempt
            6,           // followups
            100,         // started_at
            200,         // ended_at
            300,         // last_event_at
            400,         // stall_ping_at
            "resume",    // template
        ],
    )
    .expect("insert action");
}

/// Insert one fully-populated `gate` row (id 1).
fn insert_gate(conn: &Connection) {
    let names = GATE_COLUMNS
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>()
        .join(", ");
    let placeholders = (1..=GATE_COLUMNS.len())
        .map(|position| format!("?{position}"))
        .collect::<Vec<_>>()
        .join(", ");
    conn.execute(
        &format!("INSERT INTO gate ({names}) VALUES ({placeholders})"),
        params![
            1,            // id
            "per_1",      // ext_id
            7,            // action_id
            "ses_x",      // session_id
            "ses_root",   // root_session_id
            "permission", // kind
            "{}",         // payload_json
            1,            // renderable
            "pending",    // state
            "once",       // answer
            "gm",         // msg_id
            2,            // pings
            55,           // last_ping_at
            42,           // channel_id
        ],
    )
    .expect("insert gate");
}

/// Insert the `project`/`run`/parent-action/blocking-gate rows that
/// `insert_action`'s foreign keys reference (the canonical schema enforces
/// them).
fn insert_fk_parents(conn: &Connection) {
    conn.execute("INSERT INTO project (dir) VALUES ('/tmp/proj')", [])
        .expect("insert project");
    conn.execute(
        "INSERT INTO run (id, dir, kind, created_at) VALUES (11, '/tmp/proj', 'free', 0)",
        [],
    )
    .expect("insert run");
    conn.execute(
        "INSERT INTO action (id, run_id, label, prompt_sent, state)
         VALUES (3, 11, 'parent', 'p', 'staged')",
        [],
    )
    .expect("insert parent action");
    conn.execute(
        "INSERT INTO gate (id, kind, payload_json, state)
         VALUES (5, 'permission', '{}', 'pending')",
        [],
    )
    .expect("insert blocking gate");
}

/// Insert one fully-populated `project` row.
fn insert_project(conn: &Connection) {
    conn.execute(
        "INSERT INTO project (dir, work_branch, base_sha, active_session_id, lock_holder, lock_since)
         VALUES ('/tmp/proj', 'wt', 'bs', 'ses', 'lock', 9)",
        [],
    )
    .expect("insert project");
}

/// Create a fresh database that already claims schema version 4 (so the store
/// skips its migrations) and contains only `table` in the given column order.
fn seed_custom_db(path: &Path, table: &str, columns: &[(&str, &str)], seed: fn(&Connection)) {
    let conn = Connection::open(path).expect("open raw db");
    conn.execute_batch("CREATE TABLE schema_version (v INTEGER PRIMARY KEY);")
        .expect("schema_version");
    conn.execute("INSERT INTO schema_version (v) VALUES (4)", [])
        .expect("version");
    create_table(&conn, table, columns);
    seed(&conn);
}

/// The same column list in reverse order.
fn reversed<'a>(columns: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
    let mut reversed = columns.to_vec();
    reversed.reverse();
    reversed
}

#[tokio::test]
async fn action_row_named_access() {
    let store = StoreHandle::open_in_memory().expect("store");
    store
        .call(|conn| {
            insert_fk_parents(conn);
            insert_action(conn);
            Ok(())
        })
        .await
        .expect("seed");

    let action = store.action(7).await.expect("read").expect("row");
    assert_eq!(action.id, 7);
    assert_eq!(action.run_id, 11);
    assert_eq!(action.session_id.as_deref(), Some("ses_x"));
    assert_eq!(action.parent_action_id, Some(3));
    assert_eq!(action.channel_id, Some(42));
    assert_eq!(action.label, "lbl");
    assert_eq!(action.prompt_sent, "prompt");
    assert_eq!(action.state, State::Running);
    assert_eq!(action.blocked_gate_id, Some(5));
    assert_eq!(action.outcome_kind.as_deref(), Some("completed"));
    assert_eq!(action.report_full.as_deref(), Some("full"));
    assert_eq!(action.footer_json.as_deref(), Some("foot"));
    assert_eq!(action.report_msg_id.as_deref(), Some("m1"));
    assert_eq!(action.origin_msg_id.as_deref(), Some("m0"));
    assert_eq!(action.start_head.as_deref(), Some("head"));
    assert!(action.dirty_at_start);
    assert!(action.include_existing);
    assert_eq!(action.pending_commit_subject.as_deref(), Some("subj"));
    assert_eq!(action.pending_commit_body.as_deref(), Some("body"));
    assert_eq!(action.park_branch_name.as_deref(), Some("park"));
    assert_eq!(action.park_step, 2);
    assert_eq!(action.accept_sha.as_deref(), Some("sha"));
    assert_eq!(action.attempt, 4);
    assert_eq!(action.followups, 6);
    assert_eq!(action.started_at, Some(100));
    assert_eq!(action.ended_at, Some(200));
    assert_eq!(action.last_event_at, Some(300));
    assert_eq!(action.stall_ping_at, Some(400));
    assert_eq!(action.template, "resume");
}

#[tokio::test]
async fn gate_row_named_access() {
    let store = StoreHandle::open_in_memory().expect("store");
    store
        .call(|conn| {
            insert_fk_parents(conn);
            insert_action(conn);
            insert_gate(conn);
            Ok(())
        })
        .await
        .expect("seed");

    let gate = store.gate(1).await.expect("read").expect("row");
    assert_eq!(gate.id, 1);
    assert_eq!(gate.ext_id.as_deref(), Some("per_1"));
    assert_eq!(gate.action_id, Some(7));
    assert_eq!(gate.session_id.as_deref(), Some("ses_x"));
    assert_eq!(gate.root_session_id.as_deref(), Some("ses_root"));
    assert_eq!(gate.kind, "permission");
    assert_eq!(gate.payload_json, "{}");
    assert!(gate.renderable);
    assert_eq!(gate.state, "pending");
    assert_eq!(gate.answer.as_deref(), Some("once"));
    assert_eq!(gate.msg_id.as_deref(), Some("gm"));
    assert_eq!(gate.pings, 2);
    assert_eq!(gate.last_ping_at, Some(55));
    assert_eq!(gate.channel_id, Some(42));
}

#[tokio::test]
async fn project_row_named_access() {
    let store = StoreHandle::open_in_memory().expect("store");
    store
        .call(|conn| {
            insert_project(conn);
            Ok(())
        })
        .await
        .expect("seed");

    let project = store
        .project("/tmp/proj")
        .await
        .expect("read")
        .expect("row");
    assert_eq!(project.dir, "/tmp/proj");
    assert_eq!(project.work_branch.as_deref(), Some("wt"));
    assert_eq!(project.base_sha.as_deref(), Some("bs"));
    assert_eq!(project.active_session_id.as_deref(), Some("ses"));
    assert_eq!(project.lock_holder.as_deref(), Some("lock"));
    assert_eq!(project.lock_since, Some(9));
}

#[tokio::test]
async fn action_schema_reorder_safety() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("reordered.sqlite");
    let columns = reversed(ACTION_COLUMNS);
    seed_custom_db(&path, "action", &columns, insert_action);

    let store = StoreHandle::open(&path).expect("store");
    let action = store.action(7).await.expect("read").expect("row");
    // Every field is distinct, so a positional mis-read would fail loudly here.
    assert_eq!(action.id, 7);
    assert_eq!(action.run_id, 11);
    assert_eq!(action.state, State::Running);
    assert_eq!(action.template, "resume");
    assert_eq!(action.label, "lbl");
    assert_eq!(action.channel_id, Some(42));
    assert_eq!(action.stall_ping_at, Some(400));
}

#[tokio::test]
async fn gate_schema_reorder_safety() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("reordered.sqlite");
    let columns = reversed(GATE_COLUMNS);
    seed_custom_db(&path, "gate", &columns, insert_gate);

    let store = StoreHandle::open(&path).expect("store");
    let gate = store.gate(1).await.expect("read").expect("row");
    assert_eq!(gate.id, 1);
    assert_eq!(gate.kind, "permission");
    assert_eq!(gate.state, "pending");
    assert_eq!(gate.channel_id, Some(42));
    assert_eq!(gate.pings, 2);
    assert_eq!(gate.last_ping_at, Some(55));
}

#[tokio::test]
async fn project_schema_reorder_safety() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("reordered.sqlite");
    let columns = reversed(PROJECT_COLUMNS);
    seed_custom_db(&path, "project", &columns, insert_project);

    let store = StoreHandle::open(&path).expect("store");
    let project = store
        .project("/tmp/proj")
        .await
        .expect("read")
        .expect("row");
    assert_eq!(project.dir, "/tmp/proj");
    assert_eq!(project.lock_holder.as_deref(), Some("lock"));
    assert_eq!(project.lock_since, Some(9));
}

#[tokio::test]
async fn missing_column_fails_loudly() {
    let store = StoreHandle::open_in_memory().expect("store");
    store
        .call(|conn| {
            insert_fk_parents(conn);
            insert_action(conn);
            // Simulate a failed/partial migration: the mapped column is gone.
            conn.execute_batch("ALTER TABLE action DROP COLUMN template;")?;
            Ok(())
        })
        .await
        .expect("seed");

    let error = store
        .action(7)
        .await
        .expect_err("missing column must error");
    let rendered = error.to_string();
    assert!(
        rendered.contains("template"),
        "the error must name the missing column, got: {rendered}"
    );
}

#[tokio::test]
async fn invalid_state_names_the_column() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("bad_state.sqlite");
    // No CHECK constraint here (it is a store invariant, not a mapping one), so
    // an out-of-vocabulary state can reach the mapper.
    seed_custom_db(&path, "action", ACTION_COLUMNS, insert_action);
    let raw = Connection::open(&path).expect("raw");
    raw.execute("UPDATE action SET state = 'bogus' WHERE id = 7", [])
        .expect("corrupt state");
    drop(raw);

    let store = StoreHandle::open(&path).expect("store");
    let error = store.action(7).await.expect_err("bad state must error");
    let rendered = error.to_string();
    // FR-9: the conversion error names the column, never a positional index.
    assert!(
        rendered.contains("action.state") && rendered.contains("bogus"),
        "the error must name the state column and value, got: {rendered}"
    );
    assert!(
        !rendered.contains("index"),
        "the error must not leak a positional index, got: {rendered}"
    );
}
