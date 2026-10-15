//! The directory mutex (P5): two runs in one directory serialize (the loser is
//! refused naming the holder), the lock is held through `reported`,
//! `/release` frees it, and an external (TUI) holder is named too.

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

use common::{FakeChat, FakeGit, FakeOpenCode, context, init_repo, write_file};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::handlers::run;
use ocbot::app::interaction::{InteractionSource, SourceKind, TextInput};
use ocbot_core::{State, dir_hash};
use tempfile::TempDir;

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

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
        user_id: format!("discord:{OPERATOR}"),
        destination: ocbot_core::Destination::discord(CHANNEL),
        guild_id: Some(1),
        message_id: None,
        is_dm: false,
    }
}

fn button_source(custom_id: &str) -> InteractionSource {
    InteractionSource {
        kind: SourceKind::Component {
            custom_id: custom_id.to_owned(),
        },
        user_id: format!("discord:{OPERATOR}"),
        destination: ocbot_core::Destination::discord(CHANNEL),
        guild_id: Some(1),
        message_id: Some(9),
        is_dm: false,
    }
}

async fn stage_run(ctx: &Arc<ocbot::app::context::AppContext>, dir: &str, label: &str) -> i64 {
    run::execute_newrun(
        ctx,
        &modal_source("v1:newrun", vec![("directory", dir), ("label", label)]),
    )
    .await
    .expect("stage");
    let staged = ctx.store.staged_actions().await.expect("staged");
    staged.last().expect("just staged").id
}

#[tokio::test]
async fn same_directory_runs_serialize_through_the_lock() {
    let tmp = TempDir::new().expect("tmp");
    let repo = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    init_repo(&repo);
    write_file(&repo, "README.md", "# work\n");
    common::commit_all(&repo, "initial");
    let dir = repo.to_string_lossy().into_owned();

    let store = StoreHandle::open_in_memory().expect("store");
    let opencode = FakeOpenCode::new();
    let discord = FakeChat::new();
    let ctx = context(
        store.clone(),
        Arc::new(opencode),
        Arc::new(FakeGit::with_repo(Some(repo))),
        Arc::new(discord),
        OPERATOR,
        Some(CHANNEL),
    );

    let first = stage_run(&ctx, &dir, "a").await;
    let second = stage_run(&ctx, &dir, "b").await;

    let outcome = run::execute_send(&ctx, first, &button_source(&format!("v1:send:{first}")))
        .await
        .expect("first send");
    assert!(
        outcome.unwrap_or_default().contains("started"),
        "first runs"
    );

    // Second: refused, naming the holder (P5).
    let outcome = run::execute_send(&ctx, second, &button_source(&format!("v1:send:{second}")))
        .await
        .expect("second send refuses");
    assert!(
        outcome.unwrap_or_default().contains("locked"),
        "P5 refusal names the holder"
    );
    assert_eq!(
        store.lock_holder(&dir).await.expect("lock").as_deref(),
        Some(first.to_string().as_str())
    );
    assert_eq!(
        store
            .action(second)
            .await
            .expect("read")
            .expect("row")
            .state,
        State::Staged
    );

    // A third run is refused too, and first stays running.
    let third = stage_run(&ctx, &dir, "c").await;
    assert!(
        run::execute_send(&ctx, third, &button_source(&format!("v1:send:{third}")))
            .await
            .expect("third refuses")
            .unwrap_or_default()
            .contains("locked")
    );
    assert_eq!(
        store.action(first).await.expect("read").expect("row").state,
        State::Running
    );

    // `/release` frees the lock (P5 escape hatch), audited.
    let hash = dir_hash(&dir);
    let outcome = ocbot::app::handlers::release::release(
        &ctx,
        &button_source(&format!("v1:release:{hash}")),
        &hash,
    )
    .await
    .expect("release");
    assert!(outcome.unwrap_or_default().contains("released"));
    assert_eq!(store.lock_holder(&dir).await.expect("lock"), None);
}

#[tokio::test]
async fn an_external_holder_is_named_and_releasable() {
    let tmp = TempDir::new().expect("tmp");
    let repo = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    init_repo(&repo);
    write_file(&repo, "README.md", "# work\n");
    common::commit_all(&repo, "initial");
    let dir = repo.to_string_lossy().into_owned();

    let store = StoreHandle::open_in_memory().expect("store");
    // A TUI session busy in the directory takes the external lock (P5).
    store
        .set_external_lock(&dir, "ses_tui", 1000)
        .await
        .expect("external lock");
    let discord = FakeChat::new();
    let ctx = context(
        store.clone(),
        Arc::new(FakeOpenCode::new()),
        Arc::new(FakeGit::with_repo(Some(repo))),
        Arc::new(discord),
        OPERATOR,
        Some(CHANNEL),
    );

    let action = stage_run(&ctx, &dir, "poke").await;
    let outcome = run::execute_send(&ctx, action, &button_source(&format!("v1:send:{action}")))
        .await
        .expect("refused");
    assert!(
        outcome.unwrap_or_default().contains("external:ses_tui"),
        "the external holder is named"
    );
    assert_eq!(
        store
            .action(action)
            .await
            .expect("read")
            .expect("row")
            .state,
        State::Staged
    );

    // `/release` also releases an external holder.
    let hash = dir_hash(&dir);
    ocbot::app::handlers::release::release(
        &ctx,
        &button_source(&format!("v1:release:{hash}")),
        &hash,
    )
    .await
    .expect("release external");
    assert_eq!(store.lock_holder(&dir).await.expect("lock"), None);
}
