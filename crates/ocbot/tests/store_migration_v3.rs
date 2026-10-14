//! Migration v5 round-trip (SPEC-004 D-08/FR-112 — the spec's "migration v3",
//! renumbered to the store's actual next version): seed a v4-shaped database
//! by hand (`action.channel_id`, `msg_map(discord_msg_id)`,
//! `notified.discord_msg_id`), boot the store, and assert the de-Discord
//! reshape preserved every routing fact and the unclaim semantics.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration tests assert by unwrapping (C6)"
)]

use ocbot::adapters::store::StoreHandle;
use rusqlite::Connection;
use tempfile::TempDir;

#[expect(
    clippy::too_many_lines,
    reason = "one test drives the whole v4→v5 reshape and then checks every preserved fact; splitting would scatter the equivalence proof"
)]
#[tokio::test]
async fn migrates_v4_routing_facts_forward() {
    let tmp = TempDir::new().expect("tmp");
    let path = tmp.path().join("v4.db");
    {
        let store = StoreHandle::open(&path).expect("create v4-shaped db");
        store.shutdown().await;
    }
    // Reshape the fresh (v5) file back into its v4 shape, then seed v4 data.
    {
        let raw = Connection::open(&path).expect("raw open");
        raw.execute_batch(
            "ALTER TABLE action ADD COLUMN channel_id INTEGER;
             UPDATE action SET channel_id = CAST(chat_ref AS INTEGER) WHERE chat_ref IS NOT NULL;
             ALTER TABLE action DROP COLUMN chat_ref;
             ALTER TABLE action DROP COLUMN surface;
             ALTER TABLE notified RENAME COLUMN receipts TO discord_msg_id;
             CREATE TABLE msg_map_v4 (discord_msg_id TEXT PRIMARY KEY, session_id TEXT, action_id INTEGER);
             INSERT INTO msg_map_v4 (discord_msg_id, session_id, action_id)
               SELECT msg_id, session_id, action_id FROM msg_map;
             DROP TABLE msg_map;
             ALTER TABLE msg_map_v4 RENAME TO msg_map;
             DROP TABLE component_map;
             DROP TABLE pending_input;
             DELETE FROM schema_version;
             INSERT INTO schema_version (v) VALUES (4);",
        )
        .expect("reshape to v4");
        raw.execute_batch(
            "INSERT INTO project (dir, work_branch, base_sha) VALUES ('/tmp/mig', 'main', 'base');
             INSERT INTO run (id, dir, kind, created_at) VALUES (1, '/tmp/mig', 'free', 1);
             INSERT INTO action (id, run_id, channel_id, label, prompt_sent, state, attempt)
               VALUES (9, 1, 4242, 'mig', 'prompt', 'reported', 1);
             INSERT INTO msg_map (discord_msg_id, session_id, action_id)
               VALUES ('111222333', 'ses_mig', 9);
             INSERT INTO notified (session_id, seq, discord_msg_id) VALUES ('ses_mig', 5, '');
             INSERT INTO notified (session_id, seq, discord_msg_id) VALUES ('ses_mig', 6, 'posted:1');",
        )
        .expect("seed v4 rows");
    }

    // Boot: the migration runs inside open, transactionally, exactly once.
    let store = StoreHandle::open(&path).expect("v5 upgrade open");

    // msg_map rows survive under the namespaced PK (D-08).
    assert_eq!(
        store
            .lookup_msg_map("discord", "111222333")
            .await
            .expect("lookup"),
        Some(("ses_mig".to_owned(), Some(9)))
    );
    assert!(
        store
            .lookup_msg_map("signal", "111222333")
            .await
            .expect("lookup")
            .is_none(),
        "the namespaced PK does not leak across surfaces"
    );

    // channel_id values were copied into chat_ref (stringified), surface defaults.
    let action = store.action(9).await.expect("read").expect("exists");
    assert_eq!(action.surface, "discord");
    assert_eq!(action.chat_ref.as_deref(), Some("4242"));

    // Unclaim semantics preserved exactly: only empty-receipt rows delete (D-08).
    store
        .unclaim_notified("ses_mig", "5")
        .await
        .expect("unclaim empty");
    store
        .unclaim_notified("ses_mig", "6")
        .await
        .expect("unclaim posted");
    {
        let raw = Connection::open(&path).expect("raw re-open");
        let remaining: i64 = raw
            .query_row(
                "SELECT COUNT(*) FROM notified WHERE session_id = 'ses_mig'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 1, "only the posted claim survives");
        let receipts: String = raw
            .query_row(
                "SELECT receipts FROM notified WHERE session_id = 'ses_mig' AND seq = 6",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(receipts, "posted:1", "the rename carried the value");
    }

    // The new tables exist and their methods work (exercised now, used in
    // later slices; created here so the migration count stays one).
    store
        .insert_component_map(
            "discord",
            "m1",
            &[("✅".to_owned(), "v1:accept:9".to_owned())],
        )
        .await
        .expect("component map insert");
    assert_eq!(
        store
            .lookup_component_map("discord", "m1", "✅")
            .await
            .expect("component map lookup"),
        Some("v1:accept:9".to_owned())
    );
    store
        .put_pending("discord", "4242", "run", "await_prompt", "{}", 600)
        .await
        .expect("pending put");
    let pending = store
        .take_pending("discord", "4242")
        .await
        .expect("pending take")
        .expect("row exists");
    assert_eq!(
        (pending.flow.as_str(), pending.step.as_str()),
        ("run", "await_prompt")
    );
    store
        .put_pending("signal", "+1555", "edit", "await_text", "{}", 600)
        .await
        .expect("pending put 2");
    assert_eq!(
        store
            .pending_expired(i64::MAX / 2)
            .await
            .expect("sweep read")
            .len(),
        1,
        "expiry sweep reads expires_at"
    );
    store.shutdown().await;
}

#[tokio::test]
async fn migration_is_idempotent_under_the_schema_version_guard() {
    let tmp = TempDir::new().expect("tmp");
    let path = tmp.path().join("idempotent.db");
    let first = StoreHandle::open(&path).expect("first open");
    first.shutdown().await;
    // A second open on the already-v5 file is a no-op migration.
    let second = StoreHandle::open(&path).expect("second open");
    second.shutdown().await;
}
