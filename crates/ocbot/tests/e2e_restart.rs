//! AC-14 (real restart half): a run that is `running` when the process dies is
//! reported exactly once after the store is reopened from disk. This uses a
//! **file-backed** database across separate connections plus the real opencode
//! client against wiremock — the closest offline approximation of a restart.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use common::{FakeDiscord, FakeGit, context};
use ocbot::adapters::opencode::OpenCodeAdapter;
use ocbot::adapters::store::StoreHandle;
use ocbot::app::reconcile::reconcile;
use ocbot_core::State;
use serde_json::Value;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;
const SESSION: &str = "ses_ee76bda2dffexMwBaYTKuT51Me";

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../opencode-client/fixtures/v2/2.0.24")
}

fn fixture(name: &str) -> Value {
    let text = std::fs::read_to_string(fixtures_dir().join(name)).expect("fixture exists");
    serde_json::from_str(&text).expect("fixture json")
}

/// A stateless responder: the session is idle (empty active map) and its last
/// assistant message is the recorded completed PONG.
struct RestartResponder {
    info: Value,
    messages: Value,
    permission_empty: Value,
    form_empty: Value,
}

impl Respond for RestartResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let method = request.method.to_string();
        let path = request.url.path();
        if method == "GET" && path == "/api/info" {
            return ResponseTemplate::new(200).set_body_json(&self.info);
        }
        if method == "GET" && path == "/api/session/active" {
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": {} }));
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

#[tokio::test]
async fn a_restart_delivers_the_report_exactly_once() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(RestartResponder {
            info: fixture("info.json"),
            messages: fixture("messages.json"),
            permission_empty: fixture("permission_pending_empty.json"),
            form_empty: fixture("form_pending_empty.json"),
        })
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().expect("tmp");
    let db = tmp.path().join("ocbot.db");

    // Phase A: seed a running action and simulate the process dying (shutdown).
    {
        let store = StoreHandle::open(&db).expect("open A");
        let seeded = common::seed_running(
            &store,
            "/tmp/restart",
            None,
            None,
            CHANNEL,
            "interrupted",
            SESSION,
        )
        .await;
        assert_eq!(
            store.action(seeded.action_id).await.unwrap().unwrap().state,
            State::Running
        );
        store.shutdown().await;
    }

    // Phase B: a fresh process reopens the file and reconciles → one report.
    let first_report;
    {
        let store = StoreHandle::open(&db).expect("open B");
        let opencode = OpenCodeAdapter::new(
            opencode_v2_client::Client::new(&server.uri(), "secret").expect("client"),
        );
        let discord = FakeDiscord::new();
        let ctx = context(
            store.clone(),
            Arc::new(opencode),
            Arc::new(FakeGit::inert()),
            Arc::new(discord.clone()),
            OPERATOR,
            Some(CHANNEL),
        );
        let report = reconcile(&ctx).await;
        assert!(report.server_up);
        assert_eq!(report.completed, 1, "the interrupted run is delivered");
        let post = discord
            .posts()
            .into_iter()
            .find(|post| post.message.content.contains("✅ Completed"))
            .expect("report posted");
        first_report = post.message.content;
        assert!(first_report.contains("PONG"));
        store.shutdown().await;
    }

    // Phase C: another restart finds the delivery marker and posts nothing.
    {
        let store = StoreHandle::open(&db).expect("open C");
        let opencode = OpenCodeAdapter::new(
            opencode_v2_client::Client::new(&server.uri(), "secret").expect("client"),
        );
        let discord = FakeDiscord::new();
        let ctx = context(
            store.clone(),
            Arc::new(opencode),
            Arc::new(FakeGit::inert()),
            Arc::new(discord.clone()),
            OPERATOR,
            Some(CHANNEL),
        );
        let report = reconcile(&ctx).await;
        assert_eq!(report.completed, 0);
        assert!(discord.posts().is_empty(), "exactly once across restarts");
        store.shutdown().await;
    }
}
