//! Router-driven interaction e2e: a full `/run` is driven through
//! `router::guarded_plan` + `router::execute` (the same seam the serenity
//! adapter calls), rather than invoking handlers directly. This validates the
//! ack-plan/execute split end to end: stage → send → report.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::sync::Arc;

use common::{FakeChat, FakeGit, FakeOpenCode, context};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::interaction::{AckPlan, InteractionSource, SourceKind, TextInput};
use ocbot::app::router::{self, PlanOutcome};
use ocbot::app::watcher::Watcher;
use ocbot_core::State;

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

fn modal(custom_id: &str, values: &[(&str, &str)]) -> InteractionSource {
    InteractionSource {
        kind: SourceKind::ModalSubmit {
            custom_id: custom_id.to_owned(),
            values: values
                .iter()
                .map(|(id, value)| TextInput {
                    id: (*id).to_owned(),
                    value: (*value).to_owned(),
                })
                .collect(),
        },
        user_id: format!("discord:{OPERATOR}"),
        channel_id: CHANNEL,
        guild_id: Some(1),
        message_id: None,
        is_dm: false,
    }
}

fn button(custom_id: &str) -> InteractionSource {
    InteractionSource {
        kind: SourceKind::Component {
            custom_id: custom_id.to_owned(),
        },
        user_id: format!("discord:{OPERATOR}"),
        channel_id: CHANNEL,
        guild_id: Some(1),
        message_id: Some(1),
        is_dm: false,
    }
}

#[tokio::test]
async fn run_flows_through_the_router_to_a_single_report() {
    let tmp = tempfile::tempdir().expect("tmp");
    let repo = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    common::init_repo(&repo);
    common::write_file(&repo, "README.md", "# work\n");
    common::commit_all(&repo, "initial");

    let store = StoreHandle::open_in_memory().expect("store");
    let opencode = FakeOpenCode::new();
    opencode.set_snapshot("ses_fake_1", FakeOpenCode::completed_snapshot());
    let discord = FakeChat::new();
    let ctx = context(
        store.clone(),
        Arc::new(opencode),
        Arc::new(FakeGit::with_repo(Some(repo.clone()))),
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    );

    // 1. `/run` modal submit: route → execute stages and posts a preview.
    let new_run = modal(
        "v1:newrun",
        &[
            ("directory", &repo.to_string_lossy()),
            ("label", "router task"),
            ("plan", ""),
        ],
    );
    assert!(matches!(
        router::guarded_plan(&ctx, &new_run).await,
        PlanOutcome::Plan(AckPlan::DeferPublic)
    ));
    router::execute(&ctx, &new_run).await.expect("stage");
    let staged = store.staged_actions().await.expect("staged");
    assert_eq!(staged.len(), 1);
    let action_id = staged[0].id;

    // 2. Send button: route → execute launches a session and admits the prompt.
    let send = button(&format!("v1:send:{action_id}"));
    assert!(matches!(
        router::guarded_plan(&ctx, &send).await,
        PlanOutcome::Plan(AckPlan::DeferUpdate)
    ));
    router::execute(&ctx, &send).await.expect("send");
    assert_eq!(
        store.action(action_id).await.unwrap().unwrap().state,
        State::Running
    );

    // 3. The watcher finalizes exactly one report.
    Watcher::new(ctx.clone(), std::time::Duration::from_millis(10))
        .tick()
        .await
        .expect("tick");
    let reports = discord
        .posts()
        .into_iter()
        .filter(|post| post.message.content.contains("✅ Completed"))
        .count();
    assert_eq!(reports, 1, "one report through the router path");
}
