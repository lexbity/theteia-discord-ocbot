//! E2E spine: `/run` → preview → send (lock + prompt) → busy → idle → exactly
//! one report with a real git footer. opencode is served by wiremock over the
//! recorded Phase 0 fixtures (the real client runs), Discord is a recording
//! fake, and git is real on a temp repository.

// Integration tests assert via unwrap/panic; production code remains strict
// (workspace lint floor + G3). See common/mod.rs for the shared harness.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ocbot::adapters::opencode::OpenCodeAdapter;
use ocbot::app::handlers::run;
use ocbot::app::interaction::{InteractionSource, SourceKind, TextInput};
use ocbot::app::watcher::Watcher;
use ocbot_core::ports::OpenCodePort;
use serde_json::Value;
use tempfile::TempDir;
use wiremock::Request as WireRequest;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

use common::{FakeDiscord, FakeGit, context};

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../opencode-client/fixtures/v2/2.0.24")
}

fn fixture(name: &str) -> Value {
    let text = std::fs::read_to_string(fixtures_dir().join(name)).expect("fixture exists");
    serde_json::from_str(&text).expect("fixture json")
}

/// A stateful responder over the recorded fixtures. The first `/active` poll
/// reports the freshly created session as busy; afterwards the session is
/// idle (the fixture captures a real completed run).
struct SpineResponder {
    info: Value,
    session_create: Value,
    prompt: Value,
    messages: Value,
    permission_empty: Value,
    form_empty: Value,
    busy_session: String,
    active_calls: AtomicU64,
}

impl SpineResponder {
    fn new() -> Self {
        let session_create = fixture("session_create.json");
        let busy_session = session_create["data"]["id"]
            .as_str()
            .expect("fixture session id")
            .to_owned();
        Self {
            info: fixture("info.json"),
            session_create,
            prompt: fixture("prompt_admission.json"),
            messages: fixture("messages.json"),
            permission_empty: fixture("permission_pending_empty.json"),
            form_empty: fixture("form_pending_empty.json"),
            busy_session,
            active_calls: AtomicU64::new(0),
        }
    }
}

impl Respond for SpineResponder {
    fn respond(&self, request: &WireRequest) -> ResponseTemplate {
        let method = request.method.to_string();
        let path = request.url.path().to_owned();
        if method == "GET" && path == "/api/info" {
            return ResponseTemplate::new(200).set_body_json(&self.info);
        }
        if method == "POST" && path == "/api/session" {
            return ResponseTemplate::new(200).set_body_json(&self.session_create);
        }
        if method == "GET" && path == "/api/session/active" {
            let busy = self.active_calls.fetch_add(1, Ordering::SeqCst) == 0;
            return if busy {
                let body = serde_json::json!({ "data": { self.busy_session.clone(): { "type": "running" } } });
                ResponseTemplate::new(200).set_body_json(&body)
            } else {
                let body = serde_json::json!({ "data": {} });
                ResponseTemplate::new(200).set_body_json(&body)
            };
        }
        if method == "POST" && path.ends_with("/prompt") {
            return ResponseTemplate::new(200).set_body_json(&self.prompt);
        }
        if method == "GET" && path.contains("/message") {
            return ResponseTemplate::new(200).set_body_json(&self.messages);
        }
        if method == "GET" && path == "/api/permission/request" {
            return ResponseTemplate::new(200).set_body_json(&self.permission_empty);
        }
        if method == "GET" && path == "/api/form" {
            return ResponseTemplate::new(200).set_body_json(&self.form_empty);
        }
        ResponseTemplate::new(404).set_body_string("unmocked")
    }
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

fn button_source(custom_id: &str) -> InteractionSource {
    InteractionSource {
        kind: SourceKind::Component {
            custom_id: custom_id.to_owned(),
        },
        user_id: OPERATOR,
        channel_id: CHANNEL,
        guild_id: Some(1),
        message_id: Some(10),
        is_dm: false,
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one continuous end-to-end protocol walk; splitting it would obscure the ordering guarantees"
)]
async fn run_to_report_end_to_end() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(SpineResponder::new())
        .mount(&server)
        .await;

    // A real, clean git repo.
    let tmp = TempDir::new().expect("tmp");
    let repo = tmp.path().join("work");
    std::fs::create_dir_all(&repo).expect("mkdir");
    common::init_repo(&repo);
    common::write_file(&repo, "README.md", "# work\n");
    let head = common::commit_all(&repo, "initial");

    let store = ocbot::adapters::store::StoreHandle::open_in_memory().expect("store");
    let opencode: Arc<dyn OpenCodePort> = Arc::new(OpenCodeAdapter::new(Arc::new(
        opencode_v2_client::Client::new(&server.uri(), "secret").expect("client"),
    )));
    let discord = FakeDiscord::new();
    let ctx = context(
        store.clone(),
        opencode,
        Arc::new(FakeGit::with_repo(Some(repo.clone()))),
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    );

    // 1. `/run` modal submit → staged preview.
    run::execute_newrun(
        &ctx,
        &modal_source(
            "v1:newrun",
            vec![
                ("directory", &repo.display().to_string()),
                ("label", "poke"),
            ],
        ),
    )
    .await
    .expect("stage");
    let staged = store.staged_actions().await.expect("staged");
    assert_eq!(staged.len(), 1, "exactly one staged action");
    let action_id = staged[0].id;
    assert!(
        staged[0].prompt_sent.contains("Task: poke"),
        "prompt_sent renders the template: {}",
        staged[0].prompt_sent
    );
    let preview = discord.last_post().expect("preview posted");
    assert!(
        preview
            .message
            .components
            .iter()
            .any(|c| c.custom_id == format!("v1:send:{action_id}"))
    );
    assert!(
        preview
            .message
            .components
            .iter()
            .any(|c| c.custom_id == format!("v1:edit:{action_id}"))
    );

    // 2. Send: clean tree → session created, prompt admitted, lock taken.
    let outcome = run::execute_send(
        &ctx,
        action_id,
        &button_source(&format!("v1:send:{action_id}")),
    )
    .await
    .expect("send");
    assert!(outcome.unwrap().contains("started"), "ephemeral confirm");
    let running = store.running_actions().await.expect("running");
    assert_eq!(running.len(), 1);
    let session_id = running[0].session_id.clone().expect("session bound");
    assert_eq!(
        session_id,
        fixture("session_create.json")["data"]["id"]
            .as_str()
            .expect("id")
    );

    // 3. FR-27: a second staged run on a dirty tree warns and does not send.
    let _second_outcome = run::execute_newrun(
        &ctx,
        &modal_source(
            "v1:newrun",
            vec![
                ("directory", &repo.display().to_string()),
                ("label", "poke2"),
            ],
        ),
    )
    .await
    .expect("stage second");
    common::write_file(&repo, "dirty.txt", "dirty\n");
    let raw_status = common::porcelain(&repo);
    let via_ctx_git = ctx
        .git
        .status_porcelain(&repo)
        .await
        .expect("ctx git status");
    let second_action = store
        .staged_actions()
        .await
        .expect("staged")
        .pop()
        .expect("second action")
        .id;
    let outcome = run::execute_send(
        &ctx,
        second_action,
        &button_source(&format!("v1:send:{second_action}")),
    )
    .await
    .expect("dirty warn");
    assert!(
        outcome.clone().unwrap_or_default().contains("dirty"),
        "FR-27 warning surfaced; got: {outcome:?} (raw {raw_status:?}, via ctx.git {via_ctx_git:?})"
    );
    assert!(
        store
            .action(second_action)
            .await
            .expect("read")
            .expect("row")
            .state
            == ocbot_core::State::Staged,
        "dirty run must stay staged"
    );

    // 4. The first run completes: first poll busy, then idle → report.
    let watcher = Watcher::new(ctx.clone(), std::time::Duration::from_millis(10));
    watcher.tick().await.expect("tick busy");
    assert!(
        !discord.last_post().unwrap().message.content.contains("✅"),
        "no report while busy"
    );
    watcher.tick().await.expect("tick idle");

    let posts = discord.posts();
    let report = posts
        .iter()
        .rev()
        .find(|post| post.message.content.contains("✅ Completed"))
        .expect("report posted");
    assert!(
        report.message.content.contains("PONG"),
        "report body from last assistant"
    );
    assert!(report.message.content.contains("Duration"), "footer inline");

    let recorded = store.action(action_id).await.expect("read").expect("row");
    assert_eq!(recorded.state, ocbot_core::State::Reported);
    assert!(recorded.report_msg_id.is_some(), "exactly-once key set");
    assert_eq!(recorded.outcome_kind.as_deref(), Some("completed"));
    assert_eq!(recorded.report_full.as_deref(), Some("PONG"));

    // 5. Restart / duplicate: another tick posts nothing.
    watcher.tick().await.expect("tick report");
    let reports = discord
        .posts()
        .iter()
        .filter(|post| post.message.content.contains("✅ Completed"))
        .count();
    assert_eq!(reports, 1, "exactly one report");

    // 6. Lock is held through `reported` (P5) — the dirty second action stays
    // staged; an explicit run-anyway is refused by the held lock.
    let outcome = run::execute_runany(
        &ctx,
        second_action,
        &button_source(&format!("v1:runany:{second_action}")),
    )
    .await
    .expect("runany refused");
    assert!(
        outcome.unwrap().contains("locked"),
        "P5 refusal names the lock"
    );
    assert_eq!(
        store
            .action(second_action)
            .await
            .expect("read")
            .expect("row")
            .state,
        ocbot_core::State::Staged
    );
    assert_eq!(
        store
            .lock_holder(&repo.display().to_string())
            .await
            .expect("lock")
            .as_deref(),
        Some(action_id.to_string().as_str())
    );
    let _ = head;
}
