//! e2e over the real HTTP client: a permission request surfaces as a gate
//! message, the reply POST reaches the server with the exact wire decision, and
//! the gate settles. opencode is served by wiremock over the recorded Phase 0
//! fixtures.

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

use ocbot::adapters::opencode::OpenCodeAdapter;
use ocbot::adapters::store::StoreHandle;
use ocbot::app::gates_loop;
use ocbot::app::handlers::gate;
use ocbot_core::ports::OpenCodePort;
use serde_json::Value;
use wiremock::Request as WireRequest;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

use common::{FakeChat, FakeGit, context, seed_running};

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../opencode-client/fixtures/v2/2.0.24")
}

fn fixture(name: &str) -> Value {
    let text = std::fs::read_to_string(fixtures_dir().join(name)).expect("fixture exists");
    serde_json::from_str(&text).expect("fixture json")
}

/// The gate scenario served over the wire: info, an empty session list,
/// one pending permission, no forms, and a 200 for every reply POST.
struct GateResponder {
    info: Value,
    permission: Value,
    form_empty: Value,
}

impl Respond for GateResponder {
    fn respond(&self, request: &WireRequest) -> ResponseTemplate {
        let method = request.method.to_string();
        let path = request.url.path().to_owned();
        if method == "GET" && path == "/api/info" {
            return ResponseTemplate::new(200).set_body_json(&self.info);
        }
        if method == "GET" && path == "/api/session" {
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": [] }));
        }
        if method == "GET" && path == "/api/permission/request" {
            return ResponseTemplate::new(200).set_body_json(&self.permission);
        }
        if method == "GET" && path == "/api/form" {
            return ResponseTemplate::new(200).set_body_json(&self.form_empty);
        }
        if method == "POST" && path.contains("/permission/") && path.ends_with("/reply") {
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({}));
        }
        ResponseTemplate::new(404).set_body_string("unmocked")
    }
}

#[tokio::test]
async fn permission_gate_round_trips_over_the_real_client() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(GateResponder {
            info: fixture("info.json"),
            permission: fixture("permission_pending.json"),
            form_empty: fixture("form_pending_empty.json"),
        })
        .mount(&server)
        .await;

    // The fixture's session id; a running action is attributed to it.
    let session = fixture("permission_pending.json")["data"][0]["sessionID"]
        .as_str()
        .expect("fixture session")
        .to_owned();

    let store = StoreHandle::open_in_memory().expect("store");
    seed_running(
        &store,
        "/tmp/e2e-gate",
        Some("main"),
        None,
        CHANNEL,
        "poke",
        &session,
    )
    .await;
    let opencode: Arc<dyn OpenCodePort> = Arc::new(OpenCodeAdapter::new(Arc::new(
        opencode_v2_client::Client::new(&server.uri(), "secret").expect("client"),
    )));
    let discord = FakeChat::new();
    let ctx = context(
        store.clone(),
        opencode,
        Arc::new(FakeGit::inert()),
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    );

    gates_loop::sync(&ctx).await.expect("sync");

    let posted = discord.last_post().expect("gate message");
    assert!(posted.message.content.contains("bash"));
    assert!(posted.message.content.contains("printf 'hello'"));

    let rowid = store.pending_gates().await.expect("rows")[0].id;
    gate::execute_permission_reply(&ctx, rowid, "always", &format!("discord:{OPERATOR}"))
        .await
        .expect("reply");

    let received = server.received_requests().await.expect("received");
    let replies: Vec<_> = received
        .iter()
        .filter(|request| {
            request.method == "POST"
                && request
                    .url
                    .path()
                    .ends_with("/permission/per_fixture00000000000001/reply")
        })
        .collect();
    assert_eq!(replies.len(), 1, "the reply POST reached the server once");
    let body: Value = serde_json::from_slice(&replies[0].body).expect("body json");
    assert_eq!(body, serde_json::json!({ "decision": "always" }));
    assert_eq!(
        store.gate(rowid).await.expect("read").expect("row").state,
        "answered"
    );
}
