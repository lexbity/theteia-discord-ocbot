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
use ocbot_core::paths::{plan_action_token, project_action_token};

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
    for name in ["status", "history", "reset", "release", "abort", "mute"] {
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
async fn project_renderer_controls_are_acknowledged_as_deferred_updates() {
    let store = StoreHandle::open_in_memory().expect("store");
    let ctx = base_ctx(&store);
    let project = project_action_token("user/project");
    let plan = plan_action_token("user/project", "plan.md");

    for id in [
        "v1:projects".to_owned(),
        "v1:projects:p:1".to_owned(),
        format!("v1:project_select:{project}"),
        format!("v1:project_page:{project}:1"),
        format!("v1:readme_full:{project}"),
        format!("v1:readme_page:{project}:1"),
        format!("v1:plan_select:{project}:{plan}"),
        format!("v1:load_plan:{project}:{plan}"),
    ] {
        assert!(
            matches!(
                router::guarded_plan(&ctx, &component(&id)).await,
                PlanOutcome::Plan(AckPlan::DeferUpdate)
            ),
            "{id} defers"
        );
    }
    // Run Without Plan collects its label in a modal (SPEC-005 flow 7): the
    // button plans the modal, and the plan stays I/O-free (AC-1) — a token of
    // the right shape against an unreachable `/tmp/projects` root still gets a
    // modal, never a deferral into a path-based failure.
    assert!(
        matches!(
            router::guarded_plan(&ctx, &component(&format!("v1:run_noplan:{project}"))).await,
            PlanOutcome::Plan(AckPlan::Modal(_))
        ),
        "run_noplan plans the label modal"
    );
    assert!(
        matches!(
            router::guarded_plan(&ctx, &component("v1:run_noplan:not-a-token")).await,
            PlanOutcome::Failed(_)
        ),
        "a malformed project token is rejected, not silently deferred"
    );
    // And the button is never executed: the work runs on the modal submit.
    assert!(matches!(
        router::execute(&ctx, &component(&format!("v1:run_noplan:{project}"))).await,
        Err(ocbot::app::error::AppError::AlreadyHandled)
    ));
}

#[tokio::test]
async fn modal_submits_route_to_their_execute_acks() {
    let store = StoreHandle::open_in_memory().expect("store");
    let ctx = base_ctx(&store);
    let project = project_action_token("user/project");

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
        "v1:editgo:1".to_owned(),
        "v1:acceptgo:1".to_owned(),
        "v1:followupgo:1".to_owned(),
        "v1:planothergo:1".to_owned(),
        format!("v1:run_noplan_go:{project}"),
    ] {
        assert!(
            matches!(
                router::guarded_plan(&ctx, &modal(&id)).await,
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

/// The slice of the router source between a function's slash-command match
/// head and its component arm, as `&str`.
///
/// `plan` is the first `SourceKind::SlashCommand` in the file, `execute` the
/// last; the verbs are the quoted lowercase-letter literals in that segment.
/// This mirrors how `router_prefix_sync` reads the router (SPEC-002 §4.4) so
/// the slash grammar cannot drift from the adapter command table unnoticed.
fn slash_segment(function: &str) -> String {
    const MARKER: &str = "SourceKind::SlashCommand { name } => match name.as_str()";
    let source = include_str!("../src/app/router.rs");
    let start = match function {
        "plan" => source.find(MARKER),
        "execute" => source.rfind(MARKER),
        other => panic!("unknown router function {other}"),
    }
    .expect("both plan and execute dispatch slash commands by name");
    let rest = &source[start..];
    let end = rest
        .find("SourceKind::Component")
        .expect("the component arm follows the slash arm");
    rest[..end].to_owned()
}

/// The slash verbs the router dispatches in `plan` / `execute`, sorted.
fn slash_verbs(function: &str) -> Vec<String> {
    let mut verbs: Vec<String> = slash_segment(function)
        .split('"')
        .skip(1)
        .step_by(2)
        .filter(|literal| !literal.is_empty() && literal.chars().all(|ch| ch.is_ascii_alphabetic()))
        .map(str::to_owned)
        .collect();
    verbs.sort();
    verbs
}

/// SPEC-005 phase 10 drift guard: the Discord application-command table and
/// the router's slash grammar are one contract. A command registered but not
/// routed is a dead picker entry (the former `/ls` and `/cd`); a command
/// routed but not registered is unreachable (`/projects` was, briefly).
#[tokio::test]
async fn the_discord_command_table_matches_the_router_slash_grammar() {
    let mut registered: Vec<String> = ocbot::adapters::discord::SLASH_COMMANDS
        .iter()
        .map(|(name, _description)| (*name).to_owned())
        .collect();
    registered.sort();
    let mut unique = registered.clone();
    unique.dedup();
    assert_eq!(
        unique.len(),
        registered.len(),
        "SLASH_COMMANDS contains a duplicate entry"
    );

    assert_eq!(
        slash_verbs("plan"),
        registered,
        "every router verb must be registered with Discord and vice versa"
    );
    // Modal-led commands (`run`, `prompt`) have no execute slash arm: their
    // work runs on the modal submit. The difference is pinned exactly — a new
    // modal-led verb or an accidentally double-routed verb both fail here.
    let plan_verbs = slash_verbs("plan");
    let execute_verbs = slash_verbs("execute");
    let modal_led: Vec<String> = plan_verbs
        .iter()
        .filter(|verb| !execute_verbs.contains(verb))
        .cloned()
        .collect();
    assert_eq!(
        modal_led,
        vec!["prompt".to_owned(), "run".to_owned()],
        "only the modal-led commands may plan without an execute slash arm"
    );
    for verb in &execute_verbs {
        assert!(
            plan_verbs.contains(verb),
            "execute dispatches `{verb}` but plan does not know it"
        );
    }

    // Behavioural half: every registered name must survive the real plan
    // phase (the textual match arms and the router cannot disagree).
    let store = StoreHandle::open_in_memory().expect("store");
    let ctx = base_ctx(&store);
    for name in &registered {
        assert!(
            !matches!(
                router::guarded_plan(&ctx, &slash(name)).await,
                PlanOutcome::Failed(_)
            ),
            "{name} is registered but the router cannot plan it"
        );
    }
    // And the SPEC-005 entry point defers publicly like the other readers.
    assert!(matches!(
        router::guarded_plan(&ctx, &slash("projects")).await,
        PlanOutcome::Plan(AckPlan::DeferPublic)
    ));
    // The mirror image: a command Discord would never send (or a stale client
    // inventing one) is refused loudly by both phases, never silently routed.
    // `ls` and `cd` are pinned explicitly: SPEC-005 removed them, and they must
    // stay removed from both the plan and the execute grammar.
    for name in ["ls", "cd", "nonsense"] {
        assert!(
            matches!(
                router::guarded_plan(&ctx, &slash(name)).await,
                PlanOutcome::Failed(_)
            ),
            "{name} must fail the plan phase"
        );
        let result = router::execute(&ctx, &slash(name)).await;
        assert!(
            matches!(result, Err(ocbot::app::error::AppError::UnknownCommand(_))),
            "{name} must fail the execute phase: {result:?}"
        );
    }
}
