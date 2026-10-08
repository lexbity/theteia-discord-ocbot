//! AC-1: a non-allowlisted user (or a foreign channel) must produce **zero**
//! port calls across every handler. `guarded_plan` returns before any handler
//! work; the recording fakes prove nothing happened.

// Integration tests assert via unwrap/panic; production code remains strict
// (workspace lint floor + G3). See common/mod.rs for the shared harness.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use common::{FakeDiscord, FakeGit, FakeOpenCode, context};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::guard::Decision;
use ocbot::app::interaction::{InteractionSource, SourceKind, TextInput};
use ocbot::app::router::{self, PlanOutcome};
use std::sync::Arc;

const OPERATOR: u64 = 42;
const OPS: u64 = 7;

fn slash_run(user: u64, channel: u64, guild: Option<u64>) -> InteractionSource {
    InteractionSource {
        kind: SourceKind::SlashCommand {
            name: "run".to_owned(),
        },
        user_id: user,
        channel_id: channel,
        guild_id: guild,
        message_id: None,
        is_dm: guild.is_none(),
    }
}

fn slash_status(user: u64, channel: u64, guild: Option<u64>) -> InteractionSource {
    InteractionSource {
        kind: SourceKind::SlashCommand {
            name: "status".to_owned(),
        },
        user_id: user,
        channel_id: channel,
        guild_id: guild,
        message_id: None,
        is_dm: guild.is_none(),
    }
}

fn button(
    user: u64,
    channel: u64,
    guild: Option<u64>,
    custom_id: &str,
    message: u64,
) -> InteractionSource {
    InteractionSource {
        kind: SourceKind::Component {
            custom_id: custom_id.to_owned(),
        },
        user_id: user,
        channel_id: channel,
        guild_id: guild,
        message_id: Some(message),
        is_dm: guild.is_none(),
    }
}

fn modal(user: u64, channel: u64, custom_id: &str, values: Vec<(&str, &str)>) -> InteractionSource {
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
        user_id: user,
        channel_id: channel,
        guild_id: Some(OPS),
        message_id: None,
        is_dm: false,
    }
}

/// Every handler the router can dispatch to — as an interaction source.
fn all_handler_sources(channel: u64, guild: Option<u64>) -> Vec<InteractionSource> {
    vec![
        slash_run(OPERATOR, channel, guild),
        slash_status(OPERATOR, channel, guild),
        button(OPERATOR, channel, guild, "v1:send:1", 5),
        button(OPERATOR, channel, guild, "v1:edit:1", 5),
        button(OPERATOR, channel, guild, "v1:runany:1", 5),
        modal(
            OPERATOR,
            channel,
            "v1:newrun",
            vec![("directory", "/tmp/x"), ("label", "l")],
        ),
        modal(OPERATOR, channel, "v1:editgo:1", vec![("prompt", "do it")]),
    ]
}

/// Asserts a source is refused and nothing reached any port.
fn assert_quiet(
    refused: &PlanOutcome,
    opencode: &FakeOpenCode,
    git: &FakeGit,
    discord: &FakeDiscord,
) {
    assert!(
        matches!(refused, PlanOutcome::Deny | PlanOutcome::Ignore),
        "refused: {refused:?}"
    );
    assert!(
        opencode.inner().calls.is_empty(),
        "opencode was called: {:?}",
        opencode.inner().calls
    );
    assert!(git.calls().is_empty(), "git was called");
    assert!(
        discord.posts().is_empty() && discord.reactions().is_empty(),
        "discord was called"
    );
}

#[tokio::test]
async fn wrong_user_is_denied_with_zero_port_calls_for_every_handler() {
    let store = StoreHandle::open_in_memory().expect("store");
    // Seed one staged action so the button plans can `require_staged`.
    common::seed_project(&store, "/tmp/x").await;
    let run = store.insert_run("/tmp/x", "free").await.expect("run");
    store
        .insert_action(run, Some(i64::try_from(OPS).unwrap()), "label", "prompt", 1)
        .await
        .expect("action");

    let opencode = FakeOpenCode::new();
    let git = FakeGit::inert();
    let discord = FakeDiscord::new();
    let ctx = context(
        store,
        Arc::new(opencode.clone()),
        Arc::new(git.clone()),
        Arc::new(discord.clone()),
        OPERATOR,
        Some(OPS),
    );

    for source in all_handler_sources(OPS, Some(OPS)) {
        let mut impostor = source.clone();
        impostor.user_id = OPERATOR + 1;
        let outcome = router::guarded_plan(&ctx, &impostor).await;
        assert_eq!(
            outcome_kind(&outcome),
            "Deny",
            "intruder reached the handler"
        );
        assert_quiet(&outcome, &opencode, &git, &discord);
    }
}

#[tokio::test]
async fn foreign_channel_is_ignored_with_zero_port_calls_for_every_handler() {
    let store = StoreHandle::open_in_memory().expect("store");
    common::seed_project(&store, "/tmp/x").await;
    let run = store.insert_run("/tmp/x", "free").await.expect("run");
    store
        .insert_action(run, Some(i64::try_from(OPS).unwrap()), "label", "prompt", 1)
        .await
        .expect("action");

    let opencode = FakeOpenCode::new();
    let git = FakeGit::inert();
    let discord = FakeDiscord::new();
    let ctx = context(
        store,
        Arc::new(opencode.clone()),
        Arc::new(git.clone()),
        Arc::new(discord.clone()),
        OPERATOR,
        Some(OPS),
    );

    for source in all_handler_sources(OPS, Some(OPS)) {
        let mut foreign = source.clone();
        foreign.channel_id = OPS + 1; // a channel outside the ops channel
        let outcome = router::guarded_plan(&ctx, &foreign).await;
        assert_eq!(
            outcome_kind(&outcome),
            "Ignore",
            "foreign channel reached the handler"
        );
        assert_quiet(&outcome, &opencode, &git, &discord);
    }
}

#[tokio::test]
async fn allowed_operator_plans_without_any_port_calls() {
    let store = StoreHandle::open_in_memory().expect("store");
    common::seed_project(&store, "/tmp/x").await;
    let run = store.insert_run("/tmp/x", "free").await.expect("run");
    store
        .insert_action(run, Some(i64::try_from(OPS).unwrap()), "label", "prompt", 1)
        .await
        .expect("action");

    let opencode = FakeOpenCode::new();
    let git = FakeGit::inert();
    let discord = FakeDiscord::new();
    let ctx = context(
        store,
        Arc::new(opencode.clone()),
        Arc::new(git.clone()),
        Arc::new(discord.clone()),
        OPERATOR,
        Some(OPS),
    );

    for source in all_handler_sources(OPS, Some(OPS)) {
        let outcome = router::guarded_plan(&ctx, &source).await;
        assert!(
            matches!(outcome, PlanOutcome::Plan(_)),
            "allowed operator should reach plan: {outcome:?}"
        );
        // The plan phase must only touch the store (modal prefills / staged
        // preflight); it must not touch any port yet.
        assert!(
            opencode.inner().calls.is_empty(),
            "opencode touched during plan: {:?}",
            opencode.inner().calls
        );
        assert!(git.calls().is_empty(), "git touched during plan");
        assert!(
            discord.posts().is_empty() && discord.reactions().is_empty(),
            "discord touched during plan"
        );
    }
}

#[test]
fn guard_decides_per_fr1_fr2() {
    // The pure part of the chokepoint, independent of routing.
    use ocbot::app::guard::Guard;
    let guard = Guard {
        allowed_user_id: 42,
        ops_channel: Some(7),
    };
    let source = slash_run(1, 7, None); // wrong user, ops channel
    assert_eq!(guard.decide(&source), Decision::EphemeralDeny);
    let source = slash_run(42, 8, None); // right user, unknown dm channel
    assert_eq!(guard.decide(&source), Decision::Allow); // DMs are always allowed
    let source = slash_run(42, 8, Some(9)); // right user, foreign guild channel
    assert_eq!(guard.decide(&source), Decision::Ignore);
}

fn outcome_kind(outcome: &PlanOutcome) -> &'static str {
    match outcome {
        PlanOutcome::Deny => "Deny",
        PlanOutcome::Ignore => "Ignore",
        PlanOutcome::Plan(_) => "Plan",
        PlanOutcome::Failed(_) => "Failed",
    }
}
