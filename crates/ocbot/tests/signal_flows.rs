//! SPEC-004 PH-7: modal-flow parity on Signal and crash-reconcile.
//!
//! Every Discord modal (`v1:newrun`, `v1:prompt`, `v1:editgo:*`,
//! `v1:acceptgo:*`, `v1:followupgo:*`, `v1:formgo:*`, `v1:planothergo:*`) has a
//! Signal text form. The proof is [`assert_modal_parity`]: the flow's
//! synthesized `ModalSubmit` must be byte-identical to the one the Discord
//! harness builds for the same operator intent (FR-109), so no handler can tell
//! the surfaces apart.
//!
//! The second half proves the lifecycle survives a crash: a flow open when the
//! process dies is announced once at boot (D-06) and stays closable.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::sync::Arc;

use common::signal_fake::Script;
use common::signal_surface::{OPERATOR, SignalSurface, wait_state};
use ocbot::adapters::signal::flows::{Advance, FlowEngine, RunDraft, advance, run_draft};
use ocbot::adapters::store::{PendingInputRow, StoreHandle};
use ocbot::app::handlers::{accept, followup, prompt, run};
use ocbot::app::interaction::{InputStyle, ModalInputSpec, ModalSpec};
use ocbot::app::reconcile::reconcile_pending_flows;
use ocbot::app::router;
use ocbot_core::ports::GitPort;
use ocbot_core::{ActorId, Destination, State, Surface};

/// An existing directory both phases of a crash test can name.
fn existing_dir() -> String {
    std::env::temp_dir().to_string_lossy().into_owned()
}

/// An inert git port (no repo anywhere).
fn inert_git() -> Arc<dyn GitPort> {
    Arc::new(common::FakeGit::inert())
}

/// A pending row for a named flow, as the store would return it.
fn pending_row(flow: &str, step: &str, draft: &str) -> PendingInputRow {
    PendingInputRow {
        surface: Surface::Signal.as_str().to_owned(),
        chat_ref: OPERATOR.to_owned(),
        flow: flow.to_owned(),
        step: step.to_owned(),
        draft: draft.to_owned(),
        updated_at: 1_700_000_000,
        expires_at: 1_700_000_600,
    }
}

/// The submit a transition completes with, as `(custom_id, values)`.
fn submit(transition: &Advance) -> (&str, &[(String, String)]) {
    transition.submit().expect("the flow completes")
}

/// Drive one modal spec through the engine, answering each question in order,
/// and return the synthesized submit.
async fn flow_submit(
    surface: &SignalSurface,
    spec: &ModalSpec,
    answers: &[&str],
) -> (String, Vec<(String, String)>) {
    let engine = FlowEngine::new(
        surface.ctx.clone(),
        surface.adapter.clone(),
        OPERATOR.to_owned(),
    );
    assert!(
        engine.open_modal(spec).await.is_none(),
        "{} asked a question",
        spec.custom_id
    );
    let mut row = surface
        .store
        .get_pending(Surface::Signal.as_str(), OPERATOR)
        .await
        .expect("get_pending")
        .expect("an open flow");
    let mut transition = advance(&row, answers[0], "");
    for answer in &answers[1..] {
        let Advance::Next { step, draft, .. } = transition else {
            panic!("{} answered more than it asked", spec.custom_id);
        };
        row.step = step;
        row.draft = draft;
        transition = advance(&row, answer, "");
    }
    match transition {
        Advance::Submit { custom_id, values } => (custom_id, values),
        other => panic!("{} never completed: {other:?}", spec.custom_id),
    }
}

// --- parity: the named flows ----------------------------------------------

#[tokio::test]
async fn the_run_flow_synthesizes_the_discord_newrun_submit() {
    // The Discord side: what an operator's submit of the new-run modal carries.
    let discord = common::modal(
        run::NEW_RUN_MODAL,
        &[
            ("directory", "/tmp/x"),
            ("label", "do the thing"),
            ("plan", ""),
        ],
    );
    // The Signal side: directory collected at the first step, prompt at the
    // second, and the label derived by the pure engine.
    let row = pending_row("run", "await_prompt", &run_draft("/tmp/x"));
    let transition = advance(&row, "do the thing", "");
    let (custom_id, values) = submit(&transition);
    common::assert_modal_parity(&discord, custom_id, values);
}

#[tokio::test]
async fn the_prompt_flow_synthesizes_the_discord_prompt_submit() {
    let surface = SignalSurface::new(Script::default(), None, inert_git()).await;
    surface.set_cwd("/tmp/cwd").await;
    let discord = common::modal(
        prompt::PROMPT_MODAL,
        &[("directory", "/tmp/cwd"), ("prompt", "do it now")],
    );
    let transition = advance(
        &pending_row("prompt", "await_text", "{}"),
        "do it now",
        "/tmp/cwd",
    );
    let (custom_id, values) = submit(&transition);
    common::assert_modal_parity(&discord, custom_id, values);
}

// --- parity: every modal family -------------------------------------------

#[tokio::test]
async fn the_accept_form_synthesizes_the_discord_acceptgo_submit() {
    let surface = SignalSurface::new(Script::default(), None, inert_git()).await;
    let action_id = surface
        .seed_reported("/tmp/flow-accept", "feature", None)
        .await;
    let action = surface
        .store
        .action(action_id)
        .await
        .expect("action")
        .expect("row");
    let spec = accept::accept_modal(&action);
    // The Discord side: subject typed, optional body left empty.
    let discord = common::modal(&spec.custom_id, &[("subject", "ship it"), ("body", "")]);
    let (custom_id, values) = flow_submit(&surface, &spec, &["ship it"]).await;
    common::assert_modal_parity(&discord, &custom_id, &values);
}

#[tokio::test]
async fn the_followup_form_synthesizes_the_discord_followupgo_submit() {
    let surface = SignalSurface::new(Script::default(), None, inert_git()).await;
    let action_id = surface
        .seed_reported("/tmp/flow-followup", "feature", None)
        .await;
    let spec = followup::followup_modal(&surface.ctx, action_id)
        .await
        .expect("modal");
    let prefill = spec.inputs[0].value.clone().unwrap_or_default();
    // An empty reply keeps the prefill, which is exactly what the Discord
    // operator submitting the prefilled modal unchanged produces.
    let discord = common::modal(&spec.custom_id, &[("prompt", &prefill)]);
    let (custom_id, values) = flow_submit(&surface, &spec, &[""]).await;
    common::assert_modal_parity(&discord, &custom_id, &values);
}

#[tokio::test]
async fn the_edit_form_synthesizes_the_discord_editgo_submit() {
    let surface = SignalSurface::new(Script::default(), None, inert_git()).await;
    let dir = surface.tmp.path().to_string_lossy().into_owned();
    let action_id = surface.seed_staged(&dir, "staged").await;
    let spec = run::edit_modal(&surface.ctx, action_id)
        .await
        .expect("modal");
    let prefill = spec.inputs[0].value.clone().unwrap_or_default();
    let discord = common::modal(&spec.custom_id, &[("prompt", "the new prompt")]);
    let (custom_id, values) = flow_submit(&surface, &spec, &["the new prompt"]).await;
    common::assert_modal_parity(&discord, &custom_id, &values);
    assert_ne!(
        prefill, "the new prompt",
        "the prefill is the stored prompt, so the answer really was typed"
    );
}

#[tokio::test]
async fn the_plan_other_form_synthesizes_its_modal_submit() {
    let surface = SignalSurface::new(Script::default(), None, inert_git()).await;
    let spec = run::plan_other_modal(7);
    let discord = common::modal(&spec.custom_id, &[("step", "third step")]);
    let (custom_id, values) = flow_submit(&surface, &spec, &["third step"]).await;
    common::assert_modal_parity(&discord, &custom_id, &values);
}

#[tokio::test]
async fn a_two_question_form_asks_both_before_submitting() {
    let surface = SignalSurface::new(Script::default(), None, inert_git()).await;
    // A form-shaped spec with two required text fields (the shape a gate form
    // produces): the id is what the drift guard checks, the fields are what the
    // engine walks.
    let spec = ModalSpec {
        custom_id: "v1:formgo:5".to_owned(),
        title: "Answer form".to_owned(),
        inputs: vec![
            ModalInputSpec {
                id: "first".to_owned(),
                label: "First".to_owned(),
                style: InputStyle::Short,
                value: None,
                required: true,
            },
            ModalInputSpec {
                id: "second".to_owned(),
                label: "Second".to_owned(),
                style: InputStyle::Short,
                value: Some("kept".to_owned()),
                required: true,
            },
        ],
    };
    let discord = common::modal(&spec.custom_id, &[("first", "one"), ("second", "two")]);
    let (custom_id, values) = flow_submit(&surface, &spec, &["one", "two"]).await;
    common::assert_modal_parity(&discord, &custom_id, &values);
    // An empty reply to the second question keeps its prefill instead.
    let discord = common::modal(&spec.custom_id, &[("first", "one"), ("second", "kept")]);
    let (custom_id, values) = flow_submit(&surface, &spec, &["one", ""]).await;
    common::assert_modal_parity(&discord, &custom_id, &values);
}

// --- the drift guard -------------------------------------------------------

#[test]
fn every_modal_id_has_a_signal_text_flow() {
    let router_ids = router::modal_ids();
    assert!(!router_ids.is_empty(), "the router must name its modals");
    for id in &router_ids {
        assert!(
            ocbot_core::is_modal_id(id),
            "the router accepts {id} but no Signal text flow covers it — add it to ocbot_core::render::flow::MODAL_FAMILIES"
        );
    }
    // And the reverse: the core must not claim a flow the router cannot route.
    let mut router_sorted = router_ids;
    router_sorted.sort();
    let mut samples_sorted = ocbot_core::modal_family_samples();
    samples_sorted.sort();
    assert_eq!(
        router_sorted, samples_sorted,
        "the router's modal set and the flow catalog drifted apart"
    );
}

// --- lifecycle: cancel, replace, expire ------------------------------------

#[tokio::test]
async fn cancel_closes_an_open_flow() {
    let surface = SignalSurface::new(Script::default(), None, inert_git()).await;
    surface.send(OPERATOR, "run /tmp/x");
    surface
        .wait_sent("Send the prompt for this run. (or `cancel`)")
        .await;
    surface.send(OPERATOR, "cancel");
    surface.wait_sent("flow cancelled").await;
    assert!(
        surface
            .store
            .get_pending(Surface::Signal.as_str(), OPERATOR)
            .await
            .expect("get_pending")
            .is_none(),
        "the row is gone"
    );
}

#[tokio::test]
async fn an_expired_flow_is_noticed_once_and_cleared() {
    let surface = SignalSurface::new(Script::default(), None, inert_git()).await;
    surface.send(OPERATOR, "run /tmp/x");
    surface.wait_sent("Send the prompt for this run.").await;
    // Ten minutes pass: the flow is past its TTL.
    surface.clock.advance(700);
    ocbot::app::ops::sweep_pending(&surface.ctx)
        .await
        .expect("sweep");
    surface
        .wait_sent("the `run` flow expired (10 minutes)")
        .await;
    assert!(
        surface
            .store
            .get_pending(Surface::Signal.as_str(), OPERATOR)
            .await
            .expect("get_pending")
            .is_none(),
        "the sweep deleted the row"
    );
}

// --- crash and restart (D-06) ----------------------------------------------

#[tokio::test]
async fn a_crash_between_steps_leaves_a_recoverable_draft() {
    let tmp = tempfile::tempdir().expect("tmp");
    let db = tmp.path().join("ocbot.db");
    {
        let store = StoreHandle::open(&db).expect("open");
        let surface = SignalSurface::with_store(store, Script::default(), None, inert_git()).await;
        surface.send(OPERATOR, "run /tmp/x");
        surface.wait_sent("Send the prompt for this run.").await;
        // The crash: the process dies with the flow open. The store file
        // survives; the surface (pump, machine) does not.
        drop(surface);
    }
    {
        let store = StoreHandle::open(&db).expect("reopen");
        let surface =
            SignalSurface::with_store(store.clone(), Script::default(), None, inert_git()).await;
        assert!(
            store
                .get_pending(Surface::Signal.as_str(), OPERATOR)
                .await
                .expect("get_pending")
                .is_some(),
            "the draft survived the crash"
        );
        let noticed = reconcile_pending_flows(&surface.ctx).await;
        assert_eq!(noticed, 1, "one flow is announced");
        surface.wait_sent("a `run` draft was open").await;
        // The notice echoes what was in flight (D-06's draft echo): the
        // directory the first step collected.
        surface.wait_sent("dir: /tmp/x").await;
        // The operator can still close it, or reply to continue it.
        surface.send(OPERATOR, "cancel");
        surface.wait_sent("flow cancelled").await;
        assert!(
            store
                .get_pending(Surface::Signal.as_str(), OPERATOR)
                .await
                .expect("get_pending")
                .is_none(),
            "cancel works after the restart"
        );
    }
}

#[tokio::test]
async fn a_restarted_flow_continues_where_it_left_off() {
    let tmp = tempfile::tempdir().expect("tmp");
    let db = tmp.path().join("ocbot.db");
    {
        let store = StoreHandle::open(&db).expect("open");
        let surface = SignalSurface::with_store(store, Script::default(), None, inert_git()).await;
        surface.send(OPERATOR, "run");
        surface.wait_sent("Send the directory to run in.").await;
        drop(surface);
    }
    {
        let store = StoreHandle::open(&db).expect("reopen");
        let surface =
            SignalSurface::with_store(store.clone(), Script::default(), None, inert_git()).await;
        reconcile_pending_flows(&surface.ctx).await;
        surface.wait_sent("a `run` draft was open").await;
        // Reply to continue: the directory is answered, the prompt is next.
        surface.send(OPERATOR, &existing_dir());
        surface.wait_sent("Send the prompt for this run.").await;
        surface.send(OPERATOR, "do the thing");
        surface.wait_sent("Ready to run").await;
    }
}

#[tokio::test]
async fn a_fresh_flow_completes_after_the_restart_notice() {
    // The accepted cost in D-06: resume is "one message", not a restored draft.
    let tmp = tempfile::tempdir().expect("tmp");
    let db = tmp.path().join("ocbot.db");
    {
        let store = StoreHandle::open(&db).expect("open");
        let surface = SignalSurface::with_store(store, Script::default(), None, inert_git()).await;
        surface.send(OPERATOR, "run /tmp/x");
        surface.wait_sent("Send the prompt for this run.").await;
        drop(surface);
    }
    let store = StoreHandle::open(&db).expect("reopen");
    let surface =
        SignalSurface::with_store(store.clone(), Script::default(), None, inert_git()).await;
    reconcile_pending_flows(&surface.ctx).await;
    surface.wait_sent("a `run` draft was open").await;
    // Starting over means closing the stale draft first (the flow is still open,
    // by design), then one command — which runs end to end.
    surface.send(OPERATOR, "cancel");
    surface.wait_sent("flow cancelled").await;
    let dir = existing_dir();
    surface.send(OPERATOR, &format!("run {dir} | do it fresh"));
    surface.wait_sent("started").await;
}

// --- the named flows end to end --------------------------------------------

#[tokio::test]
async fn the_prompt_flow_runs_end_to_end() {
    let surface = SignalSurface::new(Script::default(), None, inert_git()).await;
    let dir = surface.tmp.path().to_string_lossy().into_owned();
    surface.set_cwd(&dir).await;
    surface.send(OPERATOR, "prompt");
    surface
        .wait_sent("Send the prompt text. (or `cancel`)")
        .await;
    surface.send(OPERATOR, "do a prompt");
    surface.wait_sent("started").await;
    let running = surface
        .store
        .running_actions()
        .await
        .expect("running")
        .into_iter()
        .next()
        .expect("a running action");
    wait_state(&surface, running.id, State::Running).await;
}

// --- the wording is core vocabulary ----------------------------------------

#[tokio::test]
async fn a_form_header_and_prompts_come_from_the_core_vocabulary() {
    let surface = SignalSurface::new(Script::default(), None, inert_git()).await;
    let action_id = surface
        .seed_reported("/tmp/flow-words", "feature", None)
        .await;
    let action = surface
        .store
        .action(action_id)
        .await
        .expect("action")
        .expect("row");
    let spec = accept::accept_modal(&action);
    let engine = FlowEngine::new(
        surface.ctx.clone(),
        surface.adapter.clone(),
        OPERATOR.to_owned(),
    );
    engine.open_modal(&spec).await;
    surface
        .wait_sent("Accept — commit message — text form")
        .await;
    surface
        .wait_sent("1/1 — Commit subject: send the subject")
        .await;
    assert!(
        surface
            .sent()
            .iter()
            .any(|text| text.contains("(or `cancel`)")),
        "every prompt carries the cancel hint"
    );
    // The expiry notice is the same vocabulary the ops sweep uses.
    assert_eq!(
        ocbot_core::expired("run"),
        "the `run` flow expired (10 minutes); send a command to start again"
    );
}

#[tokio::test]
async fn the_pure_transition_needs_no_context_at_all() {
    // A row plus text is enough — this is what makes the parity assertions
    // above possible without a daemon, and what keeps the engine testable.
    let store = StoreHandle::open_in_memory().expect("store");
    let ctx = common::context(
        store.clone(),
        Arc::new(common::FakeOpenCode::new()),
        inert_git(),
        Arc::new(common::FakeChat::new()),
        common::OPERATOR_ID,
        None,
    );
    let engine = FlowEngine::new(
        ctx.clone(),
        Arc::new(ocbot::adapters::signal::SignalAdapter::new(
            OPERATOR,
            ocbot::adapters::signal::ConnectionSlot::new(),
            store,
            Arc::new(ocbot::app::metrics::Metrics::new()),
            std::env::temp_dir().join("ocbot-flow-engine-outbox"),
        )),
        OPERATOR.to_owned(),
    );
    assert_eq!(
        engine.actor(),
        ActorId::from_parts(Surface::Signal, OPERATOR).0
    );
    assert_eq!(
        engine.destination(),
        Destination {
            surface: Surface::Signal,
            chat_ref: ocbot_core::ChatRef(OPERATOR.to_owned()),
        }
    );
    let row = pending_row("run", "await_path", "{}");
    assert!(matches!(advance(&row, "/tmp/x", ""), Advance::Next { .. }));
    assert!(matches!(advance(&row, "cancel", ""), Advance::Cancelled));
    assert!(matches!(
        advance(&pending_row("mystery", "x", "{}"), "y", ""),
        Advance::Unknown
    ));
    // The draft helper round-trips through the persisted JSON.
    let back: RunDraft = serde_json::from_str(&run_draft("/tmp/z")).expect("json");
    assert_eq!(back.dir, "/tmp/z");
}
