//! AC-13: losing the DB loses labels/prompts — not accepted work. After the DB
//! file is deleted and recreated, `/history` must show git commits as "manual".

// Integration tests assert via unwrap/panic; production code remains strict
// (workspace lint floor + G3). See common/mod.rs for the shared harness.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{FakeChat, FakeGit, FakeOpenCode, context, init_repo, write_file};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::handlers::history;
use ocbot::app::interaction::{InteractionSource, SourceKind};
use ocbot_core::{Column, State, WriteValue};
use tempfile::TempDir;

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

fn repo_str(repo: &Path) -> String {
    repo.to_string_lossy().into_owned()
}

fn source() -> InteractionSource {
    InteractionSource {
        kind: SourceKind::SlashCommand {
            name: "history".to_owned(),
        },
        user_id: format!("discord:{OPERATOR}"),
        channel_id: CHANNEL,
        guild_id: Some(1),
        message_id: None,
        is_dm: false,
    }
}

#[tokio::test]
async fn history_renders_manual_rows_after_db_loss() {
    let tmp = TempDir::new().expect("tmp");
    let repo = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    init_repo(&repo);
    write_file(&repo, "a.txt", "a\n");
    let first = common::commit_all(&repo, "first");
    write_file(&repo, "b.txt", "b\n");
    let second = common::commit_all(&repo, "second");
    let text = repo_str(&repo);
    let db = tmp.path().join("ocbot.db");

    {
        let store = StoreHandle::open(&db).expect("store");
        store
            .upsert_project(&text, Some("main"), Some(&first))
            .await
            .expect("project");
        let run = store.insert_run(&text, "free").await.expect("run");
        let action = store
            .insert_action(run, Some(&CHANNEL.to_string()), "my label", "prompt", 1)
            .await
            .expect("action");
        assert!(
            store
                .begin_send(action, &text, "ses_1", Some(&second), false)
                .await
                .expect("send")
        );
        assert!(
            store
                .cas_state(action, State::Running, State::Reported, &[])
                .await
                .expect("reported")
        );
        assert!(
            store
                .cas_state(
                    action,
                    State::Reported,
                    State::Accepted,
                    &[(Column::AcceptSha, WriteValue::Text(second.clone()))],
                )
                .await
                .expect("accepted")
        );
        store.shutdown().await;
    }

    // Losing the DB (P15): labels are gone, the commits remain.
    std::fs::remove_file(&db).expect("remove db");
    let store = StoreHandle::open(&db).expect("reopen");
    store
        .set_ui_cwd(&format!("discord:{OPERATOR}"), &text)
        .await
        .expect("cwd");

    let discord = FakeChat::new();
    let ctx = context(
        store,
        Arc::new(FakeOpenCode::new()),
        Arc::new(FakeGit::with_repo(Some(repo.clone()))),
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    );
    history::execute(&ctx, &source(), false)
        .await
        .expect("history");

    let posted = discord.last_post().expect("history posted");
    assert!(
        posted.message.content.contains("manual"),
        "commits render as manual after DB loss: {}",
        posted.message.content
    );
    assert!(!posted.message.content.contains("my label"));
}
