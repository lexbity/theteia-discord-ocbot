//! The interaction router (spec §6.6): the guarded plan phase for every
//! `custom_id`/command family, plus representative execute passes. This is the
//! chokepoint that decides the immediate ack before any handler work, so its
//! grammar is worth pinning directly.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::sync::Arc;

use common::{FakeChat, FakeGit, FakeOpenCode, context, seed_reported};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::context::AppContext;
use ocbot::app::interaction::{AckPlan, InteractionSource, SourceKind, TextInput};
use ocbot::app::router::{self, PlanOutcome};

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

fn source(kind: SourceKind) -> InteractionSource {
    InteractionSource {
        kind,
        user_id: format!("discord:{OPERATOR}"),
        destination: ocbot_core::Destination::discord(CHANNEL),
        guild_id: Some(1),
        message_id: Some(1),
        is_dm: false,
    }
}

fn slash(name: &str) -> InteractionSource {
    source(SourceKind::SlashCommand {
        name: name.to_owned(),
    })
}

fn component(custom_id: &str) -> InteractionSource {
    source(SourceKind::Component {
        custom_id: custom_id.to_owned(),
    })
}

fn modal(custom_id: &str) -> InteractionSource {
    source(SourceKind::ModalSubmit {
        custom_id: custom_id.to_owned(),
        values: vec![TextInput {
            id: "x".to_owned(),
            value: "y".to_owned(),
        }],
    })
}

fn base_ctx(store: &StoreHandle) -> Arc<AppContext> {
    context(
        store.clone(),
        Arc::new(FakeOpenCode::new()),
        Arc::new(FakeGit::inert()),
        Arc::new(FakeChat::new()),
        OPERATOR,
        Some(CHANNEL),
    )
}

async fn staged_action(store: &StoreHandle, dir: &str) -> i64 {
    store
        .upsert_project(dir, None, None)
        .await
        .expect("project");
    let run = store.insert_run(dir, "free").await.expect("run");
    store
        .insert_action(run, Some("7"), "task", "prompt", 1)
        .await
        .expect("action")
}

#[tokio::test]
async fn guard_runs_before_any_routing() {
    let store = StoreHandle::open_in_memory().expect("store");
    let ctx = base_ctx(&store);

    let mut wrong_user = slash("run");
    wrong_user.user_id = "discord:1".to_owned();
    assert!(matches!(
        router::guarded_plan(&ctx, &wrong_user).await,
        PlanOutcome::Deny
    ));

    let mut foreign = slash("run");
    foreign.destination = ocbot_core::Destination::discord(999);
    assert!(matches!(
        router::guarded_plan(&ctx, &foreign).await,
        PlanOutcome::Ignore
    ));
}

#[tokio::test]
async fn slash_commands_map_to_their_ack_plans() {
    let store = StoreHandle::open_in_memory().expect("store");
    let ctx = base_ctx(&store);

    for name in ["run", "prompt"] {
        assert!(
            matches!(
                router::guarded_plan(&ctx, &slash(name)).await,
                PlanOutcome::Plan(AckPlan::Modal(_))
            ),
            "{name} opens a modal"
        );
    }
    for name in [
        "status", "history", "reset", "release", "abort", "mute",
    ] {
        assert!(
            matches!(
                router::guarded_plan(&ctx, &slash(name)).await,
                PlanOutcome::Plan(AckPlan::DeferPublic)
            ),
            "{name} defers"
        );
    }
    assert!(matches!(
        router::guarded_plan(&ctx, &slash("nonsense")).await,
        PlanOutcome::Failed(_)
    ));
}

#[tokio::test]
async fn staged_and_reported_components_route_by_state() {
    let store = StoreHandle::open_in_memory().expect("store");
    let staged = staged_action(&store, "/tmp/router-staged").await;
    let reported = seed_reported(
        &store,
        "/tmp/router-reported",
        None,
        None,
        CHANNEL,
        "done",
        "prompt",
        "ses_r",
    )
    .await
    .action_id;
    let git_reported = seed_reported(
        &store,
        "/tmp/router-git",
        Some("main"),
        Some(&"a".repeat(40)),
        CHANNEL,
        "done",
        "prompt",
        "ses_g",
    )
    .await
    .action_id;
    let ctx = base_ctx(&store);

    assert!(matches!(
        router::guarded_plan(&ctx, &component(&format!("v1:send:{staged}"))).await,
        PlanOutcome::Plan(AckPlan::DeferUpdate)
    ));
    assert!(matches!(
        router::guarded_plan(&ctx, &component(&format!("v1:edit:{staged}"))).await,
        PlanOutcome::Plan(AckPlan::Modal(_))
    ));
    // A double-tap of Send on an already-running action collapses to an
    // ephemeral deferral rather than a new ack.
    assert!(matches!(
        router::guarded_plan(&ctx, &component(&format!("v1:send:{reported}"))).await,
        PlanOutcome::Plan(AckPlan::DeferEphemeral)
    ));
    // Accept is a modal for git, DB-only (deferred) for non-git.
    assert!(matches!(
        router::guarded_plan(&ctx, &component(&format!("v1:accept:{git_reported}"))).await,
        PlanOutcome::Plan(AckPlan::Modal(_))
    ));
    assert!(matches!(
        router::guarded_plan(&ctx, &component(&format!("v1:accept:{reported}"))).await,
        PlanOutcome::Plan(AckPlan::DeferUpdate)
    ));
}

#[tokio::test]
async fn phase_seven_component_families_plan_a_defer_or_modal() {
    let store = StoreHandle::open_in_memory().expect("store");
    let staged = staged_action(&store, "/tmp/router-families").await;
    let ctx = base_ctx(&store);

    for id in [
        "v1:plan:1:0".to_owned(),
        "v1:planp:1:0".to_owned(),
        "v1:planuse:1".to_owned(),
        format!("v1:tpl:{staged}"),
        "v1:formpick:1:a".to_owned(),
        "v1:page:history:0".to_owned(),
    ] {
        assert!(
            matches!(
                router::guarded_plan(&ctx, &component(&id)).await,
                PlanOutcome::Plan(AckPlan::DeferUpdate)
            ),
            "{id} defers"
        );
    }
    assert!(matches!(
        router::guarded_plan(&ctx, &component("v1:planother:1")).await,
        PlanOutcome::Plan(AckPlan::Modal(_))
    ));
    // A gate button plans a deferral without state.
    assert!(matches!(
        router::guarded_plan(&ctx, &component("v1:gate:5:once")).await,
        PlanOutcome::Plan(AckPlan::DeferUpdate)
    ));
    // An unknown / malformed custom_id fails the plan instead of panicking.
    assert!(matches!(
        router::guarded_plan(&ctx, &component("v1:unknown")).await,
        PlanOutcome::Failed(_)
    ));
    assert!(matches!(
        router::guarded_plan(&ctx, &component("v1:send:not-a-number")).await,
        PlanOutcome::Failed(_)
    ));
}

#[tokio::test]
async fn modal_submits_route_to_their_execute_acks() {
    let store = StoreHandle::open_in_memory().expect("store");
    let ctx = base_ctx(&store);

    for id in ["v1:newrun", "v1:prompt"] {
        assert!(
            matches!(
                router::guarded_plan(&ctx, &modal(id)).await,
                PlanOutcome::Plan(AckPlan::DeferPublic)
            ),
            "{id} defers public"
        );
    }
    for id in [
        "v1:editgo:1",
        "v1:acceptgo:1",
        "v1:followupgo:1",
        "v1:planothergo:1",
    ] {
        assert!(
            matches!(
                router::guarded_plan(&ctx, &modal(id)).await,
                PlanOutcome::Plan(AckPlan::DeferUpdate)
            ),
            "{id} defers update"
        );
    }
    assert!(matches!(
        router::guarded_plan(&ctx, &modal("v1:bogus")).await,
        PlanOutcome::Failed(_)
    ));
}

#[tokio::test]
async fn execute_toggles_the_staged_template() {
    let store = StoreHandle::open_in_memory().expect("store");
    let staged = staged_action(&store, "/tmp/router-tpl").await;
    let ctx = base_ctx(&store);

    let outcome = router::execute(&ctx, &component(&format!("v1:tpl:{staged}")))
        .await
        .expect("tpl executes");
    assert!(outcome.unwrap_or_default().contains("resume"));
    let row = store.action(staged).await.expect("read").expect("row");
    assert_eq!(row.template, "resume");
    assert_eq!(row.state, ocbot_core::State::Staged);
}
