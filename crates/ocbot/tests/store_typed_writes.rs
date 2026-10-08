//! Every store write, exercised through the typed `StoreRequest` message set
//! (SPEC-002 §4.1, Phase 5). Each write round-trips a concrete reply type over
//! its own oneshot channel; there is no type-erased `call`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use common::seed_running;
use ocbot::adapters::store::StoreHandle;
use ocbot_core::{Column, State, WriteValue};

const CHANNEL: u64 = 7;

#[expect(
    clippy::too_many_lines,
    reason = "one test walks the store's entire write message set; splitting it would scatter the coverage and make it easy to drop a write"
)]
#[tokio::test]
async fn store_typed_writes() {
    let store = StoreHandle::open_in_memory().expect("store");

    // --- project / run inserts ---
    store
        .upsert_project("/tmp/w", Some("main"), Some("base"))
        .await
        .expect("upsert project");
    let run = store
        .insert_run("/tmp/w", "free")
        .await
        .expect("insert run");
    let planned = store
        .insert_run_with_plan("/tmp/w", "plan", Some("/tmp/w/plan.md"))
        .await
        .expect("insert planned run");
    assert_eq!(
        store
            .run(planned)
            .await
            .expect("read")
            .expect("row")
            .plan_path
            .as_deref(),
        Some("/tmp/w/plan.md")
    );

    // --- staged action writes ---
    let action = store
        .insert_action(run, Some(7), "lbl", "p0", 1)
        .await
        .expect("insert action");
    assert_eq!(
        store
            .action(action)
            .await
            .expect("read")
            .expect("row")
            .state,
        State::Staged
    );
    assert!(store.restage_prompt(action, "p1").await.expect("prompt"));
    assert!(
        store
            .restage_label_prompt(action, "lbl2", "p2")
            .await
            .expect("label+prompt")
    );
    assert!(
        store
            .set_template_prompt(action, "resume", "p3")
            .await
            .expect("template")
    );
    store.set_stall_ping(action, 55).await.expect("stall ping");
    store
        .set_origin(action, "origin-msg")
        .await
        .expect("origin");
    assert_eq!(
        store
            .action(action)
            .await
            .expect("read")
            .expect("row")
            .origin_msg_id
            .as_deref(),
        Some("origin-msg")
    );

    let child = store
        .insert_staged_child(run, Some(7), "child", "pc", "fresh", 2, action)
        .await
        .expect("staged child");
    let child_row = store.action(child).await.expect("read").expect("row");
    assert_eq!(child_row.parent_action_id, Some(action));
    assert_eq!(child_row.attempt, 2);

    assert!(
        store
            .mark_include_existing(action)
            .await
            .expect("include existing")
    );
    let writes = [(Column::PromptSent, WriteValue::Text("p4".to_owned()))];
    assert!(
        store
            .cas_state_from_any(action, &[State::Staged], State::Staged, &writes)
            .await
            .expect("cas from any")
    );
    assert!(
        !store
            .cas_state_from_any(action, &[], State::Staged, &[])
            .await
            .expect("empty from any")
    );

    // --- send + session lifecycle ---
    assert!(
        store
            .begin_send(action, "/tmp/w", "ses", Some("head"), true)
            .await
            .expect("begin send")
    );
    let running = store.action(action).await.expect("read").expect("row");
    assert_eq!(running.state, State::Running);
    assert_eq!(running.session_id.as_deref(), Some("ses"));
    assert!(running.dirty_at_start);

    store
        .set_active_session("/tmp/w", "ses_active")
        .await
        .expect("active session");
    assert_eq!(
        store
            .project("/tmp/w")
            .await
            .expect("project")
            .expect("row")
            .active_session_id
            .as_deref(),
        Some("ses_active")
    );
    store
        .retire_active_session("/tmp/w")
        .await
        .expect("retire session");
    store
        .set_external_lock("/tmp/w", "ses_ext", 9)
        .await
        .expect("external lock");
    assert_eq!(
        store.lock_holder("/tmp/w").await.expect("lock"),
        Some("external:ses_ext".to_owned())
    );
    assert!(
        store
            .release_lock("/tmp/w", "external:ses_ext")
            .await
            .expect("release lock")
    );
    assert_eq!(store.lock_holder("/tmp/w").await.expect("lock"), None);

    // --- report delivery + failed send ---
    assert!(
        store
            .deliver_report(
                action,
                State::Reported,
                "m1",
                "completed",
                Some("full"),
                "{}",
                9
            )
            .await
            .expect("deliver report")
    );
    assert_eq!(
        store
            .action(action)
            .await
            .expect("read")
            .expect("row")
            .state,
        State::Reported
    );

    let failed = seed_running(&store, "/tmp/w2", None, None, CHANNEL, "work", "ses_w2").await;
    store
        .fail_send(failed.action_id, "/tmp/w2", "boom")
        .await
        .expect("fail send");
    assert_eq!(
        store
            .action(failed.action_id)
            .await
            .expect("read")
            .expect("row")
            .state,
        State::Errored
    );
    assert_eq!(store.lock_holder("/tmp/w2").await.expect("lock"), None);

    // --- gates ---
    let (gate, inserted) = store
        .upsert_gate(
            "per_1",
            Some(action),
            Some("ses"),
            Some("ses"),
            "permission",
            "{}",
            true,
            Some(7),
            100,
        )
        .await
        .expect("upsert gate");
    assert!(inserted);
    store
        .set_gate_msg(gate, "gm", Some(7))
        .await
        .expect("set gate msg");
    assert_eq!(store.bump_gate_pings(gate, 101).await.expect("ping"), 1);
    assert!(
        store
            .cas_gate(gate, "pending", "answered", Some("\"once\""))
            .await
            .expect("cas gate")
    );
    store
        .set_action_blocked(action, gate)
        .await
        .expect("block action");
    store
        .clear_action_blocked(action, gate)
        .await
        .expect("unblock action");

    // --- reset intent ---
    let intent = store
        .record_reset_intent("/tmp/w", "sha", "superseded/1")
        .await
        .expect("record intent");
    assert!(store.set_reset_step(intent, 3).await.expect("reset step"));
    store
        .delete_reset_intent(intent)
        .await
        .expect("delete intent");
    assert!(
        store
            .pending_reset_intents()
            .await
            .expect("pending intents")
            .is_empty()
    );

    // --- audit / ui state / seen / cursor / inbox / mute ---
    store
        .audit(CHANNEL, "run", Some(action), "ok")
        .await
        .expect("audit");
    store.set_ui_cwd(CHANNEL, "/cwd").await.expect("ui cwd");
    store
        .set_draft_path(CHANNEL, "/draft")
        .await
        .expect("set draft");
    assert_eq!(
        store.draft_path(CHANNEL).await.expect("draft"),
        Some("/draft".to_owned())
    );
    store.clear_draft_path(CHANNEL).await.expect("clear draft");
    assert_eq!(store.draft_path(CHANNEL).await.expect("draft"), None);
    store
        .upsert_seen("ses_seen", Some("/tmp/seen"))
        .await
        .expect("upsert seen");
    store.upsert_cursor("ses", 4).await.expect("upsert cursor");
    assert!(store.claim_notified("ses", "1").await.expect("claim"));
    store
        .finish_notified("ses", "1", "msg")
        .await
        .expect("finish notified");
    store
        .unclaim_notified("ses", "1")
        .await
        .expect("unclaim notified");
    store
        .insert_msg_map("dm_1", "ses")
        .await
        .expect("insert msg map");
    assert!(store.toggle_mute("kind", "key").await.expect("mute on"));
    assert!(!store.toggle_mute("kind", "key").await.expect("mute off"));
}
