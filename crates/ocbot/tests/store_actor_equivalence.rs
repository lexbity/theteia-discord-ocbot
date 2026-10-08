//! Store-actor equivalence (SPEC-002 Phase 7): drive a comprehensive typed
//! operation sequence through the actor, then verify the persisted state with
//! an **independent raw connection** — not through the actor's own reads.
//!
//! The typed `StoreRequest` message set replaced the former type-erased `call`.
//! This test pins that the replacement produces exactly the documented SQL
//! effects, so the refactor is behaviour-preserving.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

use ocbot::adapters::store::StoreHandle;
use ocbot_core::State;
use rusqlite::Connection;
use tempfile::TempDir;

/// Read one `T` from `sql` on the raw connection.
fn scalar<T: rusqlite::types::FromSql>(
    connection: &Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> T {
    connection
        .query_row(sql, params, |row| row.get(0))
        .expect("raw scalar")
}

#[expect(
    clippy::too_many_lines,
    reason = "one test drives the whole typed write sequence and then checks every persisted column; splitting would scatter the equivalence proof"
)]
#[tokio::test]
async fn store_actor_equivalence() {
    let tmp = TempDir::new().expect("tmp");
    let db = tmp.path().join("equivalence.db");
    let dir = "/tmp/equivalence";
    let operator = 42_i64;

    // Drive the typed actor through a broad operation sequence.
    let action = {
        let store = StoreHandle::open(&db).expect("store");
        store
            .upsert_project(dir, Some("main"), Some("base"))
            .await
            .expect("project");
        let run = store.insert_run(dir, "free").await.expect("run");
        let action = store
            .insert_action(run, Some(7), "label", "prompt", 1)
            .await
            .expect("action");
        store.set_origin(action, "origin").await.expect("origin");
        store
            .restage_label_prompt(action, "label2", "prompt2")
            .await
            .expect("restage");
        store
            .set_template_prompt(action, "resume", "prompt3")
            .await
            .expect("template");
        assert!(
            store
                .mark_include_existing(action)
                .await
                .expect("include existing")
        );
        store.set_stall_ping(action, 42).await.expect("stall ping");
        assert!(
            store
                .begin_send(action, dir, "ses_eq", Some("head"), true)
                .await
                .expect("begin send")
        );
        assert!(
            store
                .deliver_report(
                    action,
                    State::Reported,
                    "msg",
                    "completed",
                    Some("full"),
                    "{}",
                    100,
                )
                .await
                .expect("deliver report")
        );

        let (gate, inserted) = store
            .upsert_gate(
                "per_eq",
                Some(action),
                Some("ses_eq"),
                Some("ses_eq"),
                "permission",
                "{}",
                true,
                Some(7),
                50,
            )
            .await
            .expect("gate");
        assert!(inserted);
        store
            .set_gate_msg(gate, "gate-msg", Some(7))
            .await
            .expect("gate msg");
        assert!(
            store
                .cas_gate(gate, "pending", "answered", Some("\"once\""))
                .await
                .expect("cas gate")
        );
        assert_eq!(store.bump_gate_pings(gate, 60).await.expect("ping"), 1);

        let intent = store
            .record_reset_intent(dir, "sha", "superseded/1")
            .await
            .expect("intent");
        assert!(store.set_reset_step(intent, 2).await.expect("reset step"));

        store.upsert_seen("ses_eq", Some(dir)).await.expect("seen");
        store.upsert_cursor("ses_eq", 9).await.expect("cursor");
        store.set_ui_cwd(42, "/cwd").await.expect("ui cwd");
        store.set_draft_path(42, "/draft").await.expect("draft");
        store.insert_msg_map("dm", "ses_eq").await.expect("msg map");
        assert!(store.claim_notified("ses_eq", "1").await.expect("claim"));
        store
            .finish_notified("ses_eq", "1", "posted")
            .await
            .expect("finish notified");
        store
            .audit(42, "run", Some(action), "ok")
            .await
            .expect("audit");
        store
            .set_active_session(dir, "ses_active")
            .await
            .expect("active session");
        store.shutdown().await;
        action
    };

    // Verify with an independent connection: the file holds exactly the
    // documented state, no actor read path involved.
    let connection = Connection::open(&db).expect("raw open");
    let integrity: String = scalar(&connection, "PRAGMA integrity_check", &[]);
    assert_eq!(integrity, "ok");

    assert_eq!(
        scalar::<String>(
            &connection,
            "SELECT work_branch FROM project WHERE dir = ?1",
            &[&dir]
        ),
        "main"
    );
    assert_eq!(
        scalar::<String>(
            &connection,
            "SELECT active_session_id FROM project WHERE dir = ?1",
            &[&dir]
        ),
        "ses_active"
    );
    assert_eq!(
        scalar::<String>(
            &connection,
            "SELECT lock_holder FROM project WHERE dir = ?1",
            &[&dir]
        ),
        action.to_string()
    );
    assert_eq!(
        scalar::<String>(&connection, "SELECT kind FROM run WHERE id = 1", &[]),
        "free"
    );

    // One action, every mapped column at its typed value.
    assert_eq!(
        scalar::<String>(
            &connection,
            "SELECT state FROM action WHERE id = ?1",
            &[&action]
        ),
        "reported"
    );
    let (session, origin, label, prompt, template): (String, String, String, String, String) =
        connection
            .query_row(
                "SELECT session_id, origin_msg_id, label, prompt_sent, template
                 FROM action WHERE id = ?1",
                [action],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("action columns");
    assert_eq!(session, "ses_eq");
    assert_eq!(origin, "origin");
    assert_eq!(label, "label2");
    assert_eq!(prompt, "prompt3");
    assert_eq!(template, "resume");
    assert_eq!(
        scalar::<i64>(
            &connection,
            "SELECT include_existing FROM action WHERE id = ?1",
            &[&action]
        ),
        1
    );
    assert_eq!(
        scalar::<i64>(
            &connection,
            "SELECT dirty_at_start FROM action WHERE id = ?1",
            &[&action]
        ),
        1
    );
    assert_eq!(
        scalar::<i64>(
            &connection,
            "SELECT stall_ping_at FROM action WHERE id = ?1",
            &[&action]
        ),
        42
    );
    assert_eq!(
        scalar::<String>(
            &connection,
            "SELECT report_msg_id FROM action WHERE id = ?1",
            &[&action]
        ),
        "msg"
    );
    assert_eq!(
        scalar::<String>(
            &connection,
            "SELECT outcome_kind FROM action WHERE id = ?1",
            &[&action]
        ),
        "completed"
    );
    assert_eq!(
        scalar::<String>(
            &connection,
            "SELECT report_full FROM action WHERE id = ?1",
            &[&action]
        ),
        "full"
    );
    assert_eq!(
        scalar::<i64>(
            &connection,
            "SELECT ended_at FROM action WHERE id = ?1",
            &[&action]
        ),
        100
    );
    assert_eq!(
        scalar::<String>(
            &connection,
            "SELECT start_head FROM action WHERE id = ?1",
            &[&action]
        ),
        "head"
    );

    // Gate, reset intent, cursor, ui state, inbox, audit.
    assert_eq!(
        scalar::<String>(&connection, "SELECT state FROM gate WHERE id = 1", &[]),
        "answered"
    );
    assert_eq!(
        scalar::<String>(&connection, "SELECT answer FROM gate WHERE id = 1", &[]),
        "\"once\""
    );
    assert_eq!(
        scalar::<String>(&connection, "SELECT msg_id FROM gate WHERE id = 1", &[]),
        "gate-msg"
    );
    assert_eq!(
        scalar::<i64>(&connection, "SELECT pings FROM gate WHERE id = 1", &[]),
        1
    );
    assert_eq!(
        scalar::<i64>(
            &connection,
            "SELECT step FROM reset_intent WHERE id = 1",
            &[]
        ),
        2
    );
    assert_eq!(
        scalar::<i64>(
            &connection,
            "SELECT last_seq FROM session_cursor WHERE session_id = ?1",
            &[&"ses_eq"]
        ),
        9
    );
    assert_eq!(
        scalar::<String>(
            &connection,
            "SELECT cwd FROM ui_state WHERE user_id = ?1",
            &[&operator.to_string()]
        ),
        "/cwd"
    );
    assert_eq!(
        scalar::<String>(
            &connection,
            "SELECT draft_path FROM ui_state WHERE user_id = ?1",
            &[&operator.to_string()]
        ),
        "/draft"
    );
    assert_eq!(
        scalar::<String>(
            &connection,
            "SELECT discord_msg_id FROM notified WHERE session_id = ?1 AND seq = ?2",
            &[&"ses_eq", &"1"]
        ),
        "posted"
    );
    assert_eq!(
        scalar::<String>(
            &connection,
            "SELECT dir FROM session_seen WHERE session_id = ?1",
            &[&"ses_eq"]
        ),
        dir
    );
    assert_eq!(
        scalar::<String>(
            &connection,
            "SELECT session_id FROM msg_map WHERE discord_msg_id = ?1",
            &[&"dm"]
        ),
        "ses_eq"
    );
    assert_eq!(
        scalar::<String>(
            &connection,
            "SELECT result FROM audit WHERE action_id = ?1 AND verb = ?2",
            &[&action, &"run"]
        ),
        "ok"
    );
}
