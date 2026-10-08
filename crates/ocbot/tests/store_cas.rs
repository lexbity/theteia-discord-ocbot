//! Store actor semantics (real SQLite): CAS transitions (`rows_changed`),
//! exactly-once report delivery, migration idempotence, and WAL multi-connection
//! access.

// Integration tests assert via unwrap/panic; production code remains strict
// (workspace lint floor + G3). See common/mod.rs for the shared harness.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use ocbot::adapters::store::StoreHandle;
use ocbot_core::{Column, State, WriteValue};
use tempfile::TempDir;

#[tokio::test]
async fn cas_transitions_are_rows_changed_based() {
    let store = StoreHandle::open_in_memory().expect("store");
    common::seed_project(&store, "/tmp/r").await;
    let run = store.insert_run("/tmp/r", "free").await.expect("run");
    let action = store
        .insert_action(run, Some(7), "label", "prompt", 1)
        .await
        .expect("action");

    assert!(
        store
            .cas_state(action, State::Staged, State::Running, &[])
            .await
            .expect("cas"),
        "first CAS wins"
    );
    assert!(
        !store
            .cas_state(action, State::Staged, State::Running, &[])
            .await
            .expect("cas"),
        "second CAS loses on state mismatch purely via rows_changed"
    );

    let row = store.action(action).await.expect("read").expect("exists");
    assert_eq!(row.state, State::Running);
}

#[tokio::test]
async fn cas_writes_columns_atomically() {
    let store = StoreHandle::open_in_memory().expect("store");
    common::seed_project(&store, "/tmp/r").await;
    let run = store.insert_run("/tmp/r", "free").await.expect("run");
    let action = store
        .insert_action(run, None, "label", "prompt", 1)
        .await
        .expect("action");

    let writes = vec![
        (Column::SessionId, WriteValue::Text("ses_atomic".to_owned())),
        (Column::StartedAt, WriteValue::Int(99)),
    ];
    assert!(
        store
            .cas_state(action, State::Staged, State::Running, &writes)
            .await
            .expect("cas")
    );

    let row = store.action(action).await.expect("read").expect("exists");
    assert_eq!(row.session_id.as_deref(), Some("ses_atomic"));
    assert_eq!(row.started_at, Some(99));
}

#[tokio::test]
async fn deliver_report_is_exactly_once() {
    let store = StoreHandle::open_in_memory().expect("store");
    common::seed_project(&store, "/tmp/r").await;
    let run = store.insert_run("/tmp/r", "free").await.expect("run");
    let action = store
        .insert_action(run, Some(3), "label", "prompt", 1)
        .await
        .expect("action");
    let session = "ses_report";
    assert!(
        store
            .begin_send(action, "/tmp/r", session, Some("head1"), false)
            .await
            .expect("send")
    );

    assert!(
        store
            .deliver_report(
                action,
                State::Reported,
                "111",
                "completed",
                Some("PONG"),
                "{}",
                50
            )
            .await
            .expect("deliver #1")
    );
    // A duplicate (crash between Discord POST and this txn is simulated by
    // calling again against the same running→reported position) must lose.
    // The action is no longer running, so report_msg_id's guard is moot; the
    // state guard is what covers repeats.
    assert!(
        !store
            .deliver_report(
                action,
                State::Running,
                "222",
                "completed",
                Some("PONG"),
                "{}",
                51
            )
            .await
            .expect("deliver #2 must lose"),
        "exactly-once: second delivery must not change rows"
    );
    let row = store.action(action).await.expect("read").expect("exists");
    assert_eq!(row.report_msg_id.as_deref(), Some("111"));
}

#[tokio::test]
async fn send_is_atomic_with_lock_and_refuses_held_lock() {
    let store = StoreHandle::open_in_memory().expect("store");
    common::seed_project(&store, "/tmp/same-dir").await;
    let run = store
        .insert_run("/tmp/same-dir", "free")
        .await
        .expect("run");
    let first = store
        .insert_action(run, None, "a", "p1", 1)
        .await
        .expect("action");
    let second = store
        .insert_action(run, None, "b", "p2", 1)
        .await
        .expect("action");

    assert!(
        store
            .begin_send(first, "/tmp/same-dir", "ses_1", None, false)
            .await
            .expect("send")
    );
    assert!(
        !store
            .begin_send(second, "/tmp/same-dir", "ses_2", None, false)
            .await
            .expect("send"),
        "P5: lock is held — the second send must lose atomically"
    );
    let holder = store.lock_holder("/tmp/same-dir").await.expect("holder");
    assert_eq!(holder.as_deref(), Some(first.to_string().as_str()));
    // The loser stays staged.
    let row = store.action(second).await.expect("read").expect("exists");
    assert_eq!(row.state, State::Staged);
}

#[tokio::test]
async fn migration_is_idempotent_across_connections() {
    let tmp = TempDir::new().expect("tmp");
    let path = tmp.path().join("ocbot.db");

    let first = StoreHandle::open(&path).expect("first open");
    common::seed_project(&first, "/tmp/m").await;
    let run = first.insert_run("/tmp/m", "free").await.expect("run");
    let action = first
        .insert_action(run, None, "label", "prompt", 1)
        .await
        .expect("action");

    // Second connection on the same file: WAL + schema_version guard must
    // make the migration a no-op and reads must work.
    let second = StoreHandle::open(&path).expect("second open");
    let row = second.action(action).await.expect("read").expect("exists");
    assert_eq!(row.label, "label");
    let _ = first;
}

#[tokio::test]
async fn migrates_a_v1_database_forward_to_v2() {
    let tmp = TempDir::new().expect("tmp");
    let path = tmp.path().join("ocbot.db");
    {
        let store = StoreHandle::open(&path).expect("first");
        common::seed_project(&store, "/tmp/v1").await;
        store.shutdown().await;
    }
    // Reshape the file into a Phase-2 (v1) database: no `reset_intent`, no v3
    // `gate.channel_id` column, and the recorded schema version back at 1.
    // Reopening must apply V2 and V3 only.
    {
        let raw = rusqlite::Connection::open(&path).expect("raw open");
        raw.execute_batch(
            "DROP TABLE reset_intent;
             ALTER TABLE gate DROP COLUMN channel_id;
             DELETE FROM schema_version;
             INSERT INTO schema_version (v) VALUES (1);",
        )
        .expect("reshape to v1");
    }

    let store = StoreHandle::open(&path).expect("upgrade open");
    let id = store
        .record_reset_intent("/tmp/v1", "deadbeef", "superseded/t")
        .await
        .expect("v2 table exists and works");
    assert_eq!(store.pending_reset_intents().await.expect("read").len(), 1);
    assert!(store.set_reset_step(id, 1).await.expect("step"));
}

#[tokio::test]
async fn begin_send_records_dirty_start_head() {
    let store = StoreHandle::open_in_memory().expect("store");
    common::seed_project(&store, "/tmp/d").await;
    let run = store.insert_run("/tmp/d", "free").await.expect("run");
    let action = store
        .insert_action(run, None, "l", "prompt", 1)
        .await
        .expect("action");

    assert!(
        store
            .begin_send(action, "/tmp/d", "ses_x", Some("deadbeef"), true)
            .await
            .expect("send")
    );
    let row = store.action(action).await.expect("read").expect("exists");
    assert_eq!(row.start_head.as_deref(), Some("deadbeef"));
    assert!(row.dirty_at_start);
    assert_eq!(row.state, State::Running);
}
