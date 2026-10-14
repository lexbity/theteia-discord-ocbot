//! Every store read, exercised through the typed `StoreRequest` message set
//! (SPEC-002 §4.1, FR-1/FR-2). The store actor has no type-erased `call`; each
//! read below round-trips a concrete reply type over its own oneshot channel.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use common::{seed_reported, seed_running};
use ocbot::adapters::store::StoreHandle;
use ocbot_core::{Column, State, WriteValue};

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

#[expect(
    clippy::too_many_lines,
    reason = "one test walks the store's entire read message set; splitting it would scatter the coverage and make it easy to drop a read"
)]
#[tokio::test]
async fn store_typed_reads() {
    let store = StoreHandle::open_in_memory().expect("store");
    let running = seed_running(
        &store,
        "/tmp/typed-reads",
        Some("main"),
        Some(&"a".repeat(40)),
        CHANNEL,
        "run",
        "ses_run",
    )
    .await;
    let reported = seed_reported(
        &store,
        "/tmp/typed-reads-reported",
        None,
        None,
        CHANNEL,
        "reported",
        "prompt",
        "ses_reported",
    )
    .await;

    // --- project / run ---
    let project = store
        .project("/tmp/typed-reads")
        .await
        .expect("project")
        .expect("row");
    assert_eq!(project.work_branch.as_deref(), Some("main"));
    assert_eq!(
        store.lock_holder("/tmp/typed-reads").await.expect("lock"),
        Some(running.action_id.to_string()),
        "begin_send recorded the lock holder"
    );
    assert!(
        store
            .locked_projects()
            .await
            .expect("locked")
            .iter()
            .any(|row| row.dir == "/tmp/typed-reads"),
        "the running action holds the lock"
    );
    assert_eq!(
        store
            .run(running.run_id)
            .await
            .expect("run")
            .expect("row")
            .dir,
        "/tmp/typed-reads"
    );

    // --- action reads ---
    let action = store
        .action(running.action_id)
        .await
        .expect("action")
        .expect("row");
    assert_eq!(action.state, State::Running);
    assert_eq!(action.session_id.as_deref(), Some("ses_run"));
    assert!(
        store
            .running_actions()
            .await
            .expect("running")
            .iter()
            .any(|row| row.id == running.action_id)
    );
    assert!(
        store
            .running_for_session("ses_run")
            .await
            .expect("for session")
            .is_some_and(|row| row.id == running.action_id)
    );
    assert!(
        store
            .running_action_for_dir("/tmp/typed-reads")
            .await
            .expect("for dir")
            .is_some_and(|row| row.id == running.action_id)
    );
    assert!(
        store
            .actions_in_states(&[State::Reported])
            .await
            .expect("in states")
            .iter()
            .any(|row| row.id == reported.action_id)
    );
    assert!(
        store
            .actions_for_dir("/tmp/typed-reads")
            .await
            .expect("for dir")
            .iter()
            .any(|row| row.id == running.action_id)
    );

    // A staged action is visible through the staged read.
    let staged = store
        .insert_action(running.run_id, Some("7"), "staged", "p", 1)
        .await
        .expect("insert staged");
    assert!(
        store
            .staged_actions()
            .await
            .expect("staged")
            .iter()
            .any(|row| row.id == staged)
    );

    // `last_accept_sha` moves once an accept sha is recorded.
    assert_eq!(
        store
            .last_accept_sha("/tmp/typed-reads-reported")
            .await
            .expect("sha"),
        None
    );
    let writes = [(Column::AcceptSha, WriteValue::Text("cafe".to_owned()))];
    assert!(
        store
            .cas_state(
                reported.action_id,
                State::Reported,
                State::Reported,
                &writes
            )
            .await
            .expect("record sha")
    );
    assert_eq!(
        store
            .last_accept_sha("/tmp/typed-reads-reported")
            .await
            .expect("sha"),
        Some("cafe".to_owned())
    );

    // --- gates ---
    let (gate_id, inserted) = store
        .upsert_gate(
            "per_typed",
            Some(running.action_id),
            Some("ses_run"),
            Some("ses_run"),
            "permission",
            "{}",
            true,
            Some(7),
            1_000,
        )
        .await
        .expect("upsert gate");
    assert!(inserted);
    assert!(
        store
            .pending_gates()
            .await
            .expect("pending")
            .iter()
            .any(|row| row.id == gate_id)
    );
    assert!(
        store
            .cas_gate(gate_id, "pending", "answered", Some("\"once\""))
            .await
            .expect("cas gate")
    );
    assert_eq!(
        store.gate(gate_id).await.expect("gate").expect("row").state,
        "answered"
    );
    assert!(
        store
            .answered_gates()
            .await
            .expect("answered")
            .iter()
            .any(|row| row.id == gate_id)
    );

    // --- reset intent / cursor ---
    let intent = store
        .record_reset_intent("/tmp/typed-reads", "deadbeef", "superseded/1")
        .await
        .expect("intent");
    assert!(
        store
            .pending_reset_intents()
            .await
            .expect("intents")
            .iter()
            .any(|row| row.id == intent)
    );
    store
        .upsert_cursor("ses_run", 5)
        .await
        .expect("upsert cursor");
    assert_eq!(
        store
            .cursor("ses_run")
            .await
            .expect("cursor")
            .expect("row")
            .last_seq,
        5
    );

    // --- ui state / dirs / counts ---
    store
        .set_ui_cwd(&format!("discord:{OPERATOR}"), "/tmp/cwd")
        .await
        .expect("cwd");
    assert_eq!(
        store
            .ui_cwd(&format!("discord:{OPERATOR}"))
            .await
            .expect("cwd"),
        Some("/tmp/cwd".to_owned())
    );
    store
        .set_draft_path(&format!("discord:{OPERATOR}"), "/tmp/draft")
        .await
        .expect("draft");
    assert_eq!(
        store
            .draft_path(&format!("discord:{OPERATOR}"))
            .await
            .expect("draft"),
        Some("/tmp/draft".to_owned())
    );
    assert!(
        store
            .recent_dirs(10)
            .await
            .expect("recent")
            .contains(&"/tmp/typed-reads".to_owned())
    );
    let counts = store.action_counts_by_state().await.expect("counts");
    assert!(counts.iter().any(|(state, _)| state == "running"));

    // --- session bookkeeping / inbox ---
    store
        .upsert_seen("ses_external", Some("/tmp/seen"))
        .await
        .expect("seen");
    assert_eq!(
        store.session_dir("ses_external").await.expect("dir"),
        Some("/tmp/seen".to_owned())
    );
    assert!(
        store
            .known_bot_sessions()
            .await
            .expect("sessions")
            .contains(&"ses_run".to_owned())
    );
    assert!(store.claim_notified("ses_run", "7").await.expect("claim"));
    store
        .insert_msg_map("discord", "msg_1", "ses_run", None)
        .await
        .expect("msg map");
    assert_eq!(
        store
            .lookup_msg_map("discord", "msg_1")
            .await
            .expect("lookup"),
        Some(("ses_run".to_owned(), None))
    );

    // --- mute toggle (read + write symmetry) ---
    assert!(!store.is_muted("session", "ses_run").await.expect("muted"));
    assert!(
        store
            .toggle_mute("session", "ses_run")
            .await
            .expect("toggle")
    );
    assert!(store.is_muted("session", "ses_run").await.expect("muted"));
    assert!(
        !store
            .toggle_mute("session", "ses_run")
            .await
            .expect("toggle")
    );
    assert!(!store.is_muted("session", "ses_run").await.expect("muted"));
}
