//! The Phase 2 **live-manual** proof (exit criterion: "one manual live run
//! recorded"): the real spine (real opencode client + real git + real store +
//! recording Discord fake) driven headless against a live opencode v2 server.
//!
//! Not run by default: `#![ignore]`. To execute and re-record the transcript:
//!
//! ```sh
//! OPENCODE_URL=http://127.0.0.1:8080 OPENCODE_PASSWORD=<pw> \
//!   cargo test -p ocbot --test live_manual -- --ignored
//! ```
//!
//! The transcript is written to `docs/compat/phase2-live-transcript.txt`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ocbot::adapters::opencode::OpenCodeAdapter;
use ocbot::app::handlers::run;
use ocbot::app::interaction::{InteractionSource, SourceKind, TextInput};
use ocbot::app::reconcile::reconcile;
use ocbot_core::ports::OpenCodePort;
use tempfile::TempDir;

use common::{FakeDiscord, FakeGit, context};

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

#[tokio::test]
#[ignore = "requires a live opencode v2 server (OPENCODE_URL/OPENCODE_PASSWORD)"]
#[expect(
    clippy::too_many_lines,
    reason = "one continuous live-protocol walk whose transcript ordering is the point"
)]
async fn live_spine_against_real_server() {
    let Ok(server_url) = std::env::var("OPENCODE_URL") else {
        tracing::warn!(
            "OPENCODE_URL not set; skipping the live run (this test is #[ignore]d anyway)"
        );
        return;
    };
    let password = std::env::var("OPENCODE_PASSWORD").expect("OPENCODE_PASSWORD set");
    let transcript_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/compat/phase2-live-transcript.txt");
    let mut transcript: Vec<String> = Vec::new();

    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_writer(std::io::stderr)
        .try_init();

    let tmp = TempDir::new().expect("tmp");
    let repo = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    common::init_repo(&repo);
    common::write_file(&repo, "README.md", "# live\n");
    common::commit_all(&repo, "initial");

    let store = ocbot::adapters::store::StoreHandle::open_in_memory().expect("store");
    let opencode: Arc<dyn OpenCodePort> = Arc::new(OpenCodeAdapter::new(
        opencode_v2_client::Client::new(&server_url, &password).expect("client"),
    ));
    let discord = FakeDiscord::new();
    let ctx = context(
        store.clone(),
        opencode.clone(),
        Arc::new(FakeGit::with_repo(Some(repo.clone()))),
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    );

    let facts = opencode.probe().await.expect("v2 probe");
    transcript.push(format!("probe version={}", facts.version));

    let source = InteractionSource {
        kind: SourceKind::ModalSubmit {
            custom_id: "v1:newrun".to_owned(),
            values: vec![
                TextInput {
                    id: "directory".to_owned(),
                    value: repo.display().to_string(),
                },
                TextInput {
                    id: "label".to_owned(),
                    value: "live poke".to_owned(),
                },
            ],
        },
        user_id: OPERATOR,
        channel_id: CHANNEL,
        guild_id: Some(1),
        message_id: None,
        is_dm: false,
    };
    run::execute_newrun(&ctx, &source).await.expect("stage");
    let action = store.staged_actions().await.expect("staged").remove(0);
    transcript.push(format!(
        "staged action#{} prompt=[{}]",
        action.id, action.prompt_sent
    ));

    let button = InteractionSource {
        kind: SourceKind::Component {
            custom_id: format!("v1:send:{}", action.id),
        },
        user_id: OPERATOR,
        channel_id: CHANNEL,
        guild_id: Some(1),
        message_id: Some(1),
        is_dm: false,
    };
    let outcome = run::execute_send(&ctx, action.id, &button)
        .await
        .expect("send");
    transcript.push(format!("send => {}", outcome.expect("ephemeral")));

    let session = store
        .running_actions()
        .await
        .expect("running")
        .remove(0)
        .session_id
        .expect("session bound");
    transcript.push(format!("session={session}"));

    // Follow to idle with the poll+reconcile loop; stop when the report lands.
    let mut polls = 0;
    loop {
        let _report = reconcile(&ctx).await;
        polls += 1;
        let row = store.action(action.id).await.expect("row").expect("exists");
        if row.report_msg_id.is_some() {
            transcript.push(format!(
                "report delivered after {polls} reconcile passes; state={} outcome={:?}",
                row.state, row.outcome_kind
            ));
            break;
        }
        assert!(
            polls <= 120,
            "live run did not finish within 120 polls (session {session})"
        );
        tokio::time::sleep(Duration::from_millis(1500)).await;
    }

    let posted = discord.posts();
    let report_message = posted
        .iter()
        .find(|post| post.message.content.contains("✅"))
        .expect("report message posted");
    transcript.push(format!(
        "report message: {}",
        report_message.message.content
    ));
    assert_eq!(
        posted
            .iter()
            .filter(|p| p.message.content.contains("✅"))
            .count(),
        1,
        "exactly one report in a live run"
    );

    let text = format!("```\n{}\n```\n", transcript.join("\n"));
    std::fs::write(&transcript_path, text).expect("transcript written");
    tracing::info!(path = %transcript_path.display(), "live transcript recorded");
}
