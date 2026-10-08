//! Store-actor chaos (SPEC-002 Phase 7): SIGKILL the harness after each typed
//! store commit and prove a freshly reopened actor sees exactly the committed
//! prefix, never a torn transaction, over an integrity-clean database.
//!
//! No clean shutdown, no unwinding, no destructors — `Child::kill` is a real
//! SIGKILL, so durability is carried by SQLite's WAL alone.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::path::Path;
use std::time::Duration;

use common::spawn_harness;
use ocbot::adapters::store::StoreHandle;
use ocbot_core::State;
use rusqlite::Connection;
use tempfile::TempDir;

const OPERATOR: u64 = 42;

/// The store-harness checkpoints, in commit order.
const CHECKPOINTS: [&str; 6] = [
    "STORE-PROJECT",
    "STORE-RUN",
    "STORE-ACTION",
    "STORE-SEND",
    "STORE-REPORT",
    "STORE-RELEASE",
];

async fn wait_for_marker(marker: &Path) {
    for _ in 0..400 {
        if marker.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("harness never reached checkpoint {}", marker.display());
}

fn harness_env(
    db: &Path,
    dir: &str,
    markers: &Path,
    checkpoint: &str,
) -> Vec<(&'static str, String)> {
    vec![
        ("OCBOT_CRASH_DB", db.to_string_lossy().into_owned()),
        ("OCBOT_CRASH_MODE", "store".to_owned()),
        ("OCBOT_CRASH_DIR", dir.to_owned()),
        (
            "OCBOT_TEST_CRASH_DIR",
            markers.to_string_lossy().into_owned(),
        ),
        ("OCBOT_TEST_CRASH_AFTER", checkpoint.to_owned()),
        ("OCBOT_CRASH_OPERATOR", OPERATOR.to_string()),
    ]
}

/// The database must be structurally sound after an abrupt death.
fn assert_integrity(db: &Path) {
    let connection = Connection::open(db).expect("raw open");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("integrity check");
    assert_eq!(result, "ok", "WAL recovery left the database corrupt");
}

#[tokio::test]
async fn store_actor_chaos() {
    for checkpoint in CHECKPOINTS {
        let tmp = TempDir::new().expect("tmp");
        let dir = tmp.path().join("work");
        let dir_text = dir.to_string_lossy().into_owned();
        let db = tmp.path().join("ocbot.db");
        let markers = tmp.path().join("markers");

        // Spawn a store harness, wait until it commits `checkpoint`, then
        // SIGKILL it mid-flight.
        let mut child = spawn_harness(&harness_env(&db, &dir_text, &markers, checkpoint));
        wait_for_marker(&markers.join(format!("{checkpoint}.reached"))).await;
        child.kill().expect("SIGKILL the harness");
        let _ = child.wait();

        // A fresh actor recovers the committed prefix from the WAL.
        let store = StoreHandle::open(&db).expect("reopen");
        assert_integrity(&db);
        let project = store.project(&dir_text).await.expect("project");
        let action = store.action(1).await.expect("action");
        match checkpoint {
            "STORE-PROJECT" => {
                assert!(project.is_some(), "the project commit is durable");
                assert!(store.run(1).await.expect("run").is_none());
                assert!(action.is_none());
            }
            "STORE-RUN" => {
                assert!(store.run(1).await.expect("run").is_some());
                assert!(action.is_none());
            }
            "STORE-ACTION" => {
                assert_eq!(action.expect("action").state, State::Staged);
            }
            "STORE-SEND" => {
                let row = action.expect("action");
                assert_eq!(row.state, State::Running);
                assert_eq!(row.session_id.as_deref(), Some("ses_chaos"));
                assert_eq!(
                    store.lock_holder(&dir_text).await.expect("lock"),
                    Some("1".to_owned()),
                    "the send transaction's lock is durable"
                );
            }
            "STORE-REPORT" => {
                let row = action.expect("action");
                assert_eq!(row.state, State::Reported);
                assert_eq!(row.report_msg_id.as_deref(), Some("m1"));
            }
            "STORE-RELEASE" => {
                assert_eq!(action.expect("action").state, State::Reported);
                assert_eq!(store.lock_holder(&dir_text).await.expect("lock"), None);
            }
            other => panic!("unexpected checkpoint {other}"),
        }
    }
}
