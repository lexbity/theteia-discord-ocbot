//! AC-17 / D-R5: renderable forms answer through buttons + a modal; forms with
//! too many fields (or unknown types) degrade to surface-only, post no buttons,
//! and clear when they leave the pending list.

// Integration tests assert via unwrap/panic; production code remains strict
// (workspace lint floor + G3). See common/mod.rs for the shared harness.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::sync::Arc;

use common::{
    FakeDiscord, FakeGit, FakeOpenCode, boolean_field, choice_field, context, form_gate,
    seed_running, text_field,
};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::gates_loop;
use ocbot::app::handlers::gate;
use ocbot::app::interaction::{InteractionSource, SourceKind, TextInput};
use ocbot_core::ports::OpenCodePort;

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;
const SESSION: &str = "ses_form";

fn ctx_for(
    store: &StoreHandle,
    opencode: Arc<dyn OpenCodePort>,
    discord: &FakeDiscord,
) -> Arc<ocbot::app::context::AppContext> {
    context(
        store.clone(),
        opencode,
        Arc::new(FakeGit::inert()),
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    )
}

async fn seeded_ctx(
    store: &StoreHandle,
    opencode: Arc<dyn OpenCodePort>,
    discord: &FakeDiscord,
) -> Arc<ocbot::app::context::AppContext> {
    seed_running(
        store,
        "/tmp/forms",
        Some("main"),
        None,
        CHANNEL,
        "my action",
        SESSION,
    )
    .await;
    let ctx = ctx_for(store, opencode, discord);
    gates_loop::sync(&ctx).await.expect("sync");
    ctx
}

fn modal_source(custom_id: &str, values: Vec<(&str, &str)>) -> InteractionSource {
    InteractionSource {
        kind: SourceKind::ModalSubmit {
            custom_id: custom_id.to_owned(),
            values: values
                .into_iter()
                .map(|(id, value)| TextInput {
                    id: id.to_owned(),
                    value: value.to_owned(),
                })
                .collect(),
        },
        user_id: OPERATOR,
        channel_id: CHANNEL,
        guild_id: Some(1),
        message_id: None,
        is_dm: false,
    }
}

#[tokio::test]
async fn choice_plus_text_form_renders_a_button_and_a_modal() {
    let store = StoreHandle::open_in_memory().expect("store");
    let opencode = FakeOpenCode::new();
    opencode.set_gates(vec![form_gate(
        "frm_1",
        SESSION,
        "Which approach?",
        vec![
            choice_field("approach", &[("a", "Option A"), ("b", "Option B")]),
            text_field("note", Some("Note")),
        ],
    )]);
    let discord = FakeDiscord::new();
    let ctx = seeded_ctx(&store, Arc::new(opencode.clone()), &discord).await;

    let rowid = store.pending_gates().await.expect("rows")[0].id;
    let posted = discord.last_post().expect("gate message");
    assert!(
        posted
            .message
            .components
            .iter()
            .any(|component| component.custom_id == format!("v1:formgo:{rowid}")),
        "renderable form carries the Answer button"
    );
    assert!(posted.message.content.contains("Which approach?"));

    let modal = gate::form_modal(&ctx, rowid)
        .await
        .expect("modal plan")
        .expect("renderable form has a modal");
    assert_eq!(modal.inputs.len(), 2);
    assert_eq!(modal.inputs[0].id, "approach");
    assert_eq!(modal.inputs[1].id, "note");

    // Submitting the modal replies with the coerced answer map.
    let outcome = gate::execute_form_submit(
        &ctx,
        rowid,
        &modal_source(
            &format!("v1:formgo:{rowid}"),
            vec![("approach", "b"), ("note", "go with B")],
        ),
    )
    .await
    .expect("form submit");
    assert!(outcome.unwrap_or_default().contains("answered"));
    let forms = opencode
        .inner()
        .calls
        .iter()
        .filter_map(|call| match call {
            common::Call::ReplyForm(_, _, answer) => Some(answer.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(forms.len(), 1);
    assert_eq!(
        forms[0],
        serde_json::json!({ "approach": "b", "note": "go with B" })
    );
}

#[tokio::test]
async fn a_bad_choice_keeps_the_gate_pending() {
    let store = StoreHandle::open_in_memory().expect("store");
    let opencode = FakeOpenCode::new();
    opencode.set_gates(vec![form_gate(
        "frm_2",
        SESSION,
        "Pick one",
        vec![choice_field("approach", &[("a", "A"), ("b", "B")])],
    )]);
    let discord = FakeDiscord::new();
    let ctx = seeded_ctx(&store, Arc::new(opencode.clone()), &discord).await;

    let rowid = store.pending_gates().await.expect("rows")[0].id;
    let error = gate::execute_form_submit(
        &ctx,
        rowid,
        &modal_source(&format!("v1:formgo:{rowid}"), vec![("approach", "zzz")]),
    )
    .await
    .expect_err("out-of-domain choice is rejected");
    assert!(error.to_string().contains("one of"), "error: {error}");
    assert_eq!(
        store.gate(rowid).await.expect("read").expect("row").state,
        "pending"
    );
    assert!(
        !opencode
            .inner()
            .calls
            .iter()
            .any(|call| matches!(call, common::Call::ReplyForm(_, _, _))),
        "no reply POST for a rejected answer"
    );
}

#[tokio::test]
async fn single_choice_form_answers_by_button() {
    let store = StoreHandle::open_in_memory().expect("store");
    let opencode = FakeOpenCode::new();
    opencode.set_gates(vec![form_gate(
        "frm_3",
        SESSION,
        "Continue?",
        vec![boolean_field("notify")],
    )]);
    let discord = FakeDiscord::new();
    let ctx = seeded_ctx(&store, Arc::new(opencode.clone()), &discord).await;

    let rowid = store.pending_gates().await.expect("rows")[0].id;
    let posted = discord.last_post().expect("gate message");
    assert!(
        posted
            .message
            .components
            .iter()
            .any(|component| component.custom_id == format!("v1:formpick:{rowid}:true")),
        "yes button"
    );

    gate::execute_form_pick(&ctx, rowid, "true", OPERATOR)
        .await
        .expect("pick");
    assert_eq!(
        store.gate(rowid).await.expect("read").expect("row").state,
        "answered"
    );
    let forms = opencode
        .inner()
        .calls
        .iter()
        .filter_map(|call| match call {
            common::Call::ReplyForm(_, _, answer) => Some(answer.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(forms, vec![serde_json::json!({ "notify": true })]);
}

#[tokio::test]
async fn a_seven_field_form_is_surface_only_and_clears_on_resolve() {
    let store = StoreHandle::open_in_memory().expect("store");
    let opencode = FakeOpenCode::new();
    let fields: Vec<_> = (0..7)
        .map(|index| text_field(&format!("f{index}"), None))
        .collect();
    opencode.set_gates(vec![form_gate("frm_7", SESSION, "Too many", fields)]);
    let discord = FakeDiscord::new();
    let ctx = seeded_ctx(&store, Arc::new(opencode.clone()), &discord).await;

    let rowid = store.pending_gates().await.expect("rows")[0].id;
    let posted = discord.last_post().expect("gate message");
    assert!(
        posted.message.components.is_empty(),
        "an unrenderable form posts no buttons"
    );
    assert!(posted.message.content.contains("answer in the TUI"));
    assert!(posted.message.content.contains("f0"));

    // A stale "Answer" press on a surface-only form is a no-op (no modal).
    assert!(
        gate::form_modal(&ctx, rowid)
            .await
            .expect("no modal")
            .is_none()
    );

    // The operator answers in the TUI; the gate leaves the pending list and is
    // cleared (FR-41).
    opencode.set_gates(Vec::new());
    gates_loop::sync(&ctx).await.expect("clear sync");
    let row = store.gate(rowid).await.expect("read").expect("row");
    assert_eq!(row.state, "answered");
    assert_eq!(row.answer.as_deref(), Some("\"resolved-elsewhere\""));
    assert!(store.pending_gates().await.expect("pending").is_empty());
}
