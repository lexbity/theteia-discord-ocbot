//! Adapter conformance: drive **every** `OpenCodePort` method through the real
//! `OpenCodeAdapter` + client against wiremock fixtures. This is the production
//! path (the e2e tests only exercise a subset), so each method is asserted on
//! its mapped core type.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ocbot::adapters::opencode::OpenCodeAdapter;
use ocbot_core::error::OcError;
use ocbot_core::ports::{Activity, GateDecision, OpenCodePort, PermissionEffect, PermissionRule};
use serde_json::Value;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../opencode-client/fixtures/v2/2.0.24")
}

fn fixture(name: &str) -> Value {
    let text = std::fs::read_to_string(fixtures_dir().join(name)).expect("fixture exists");
    serde_json::from_str(&text).expect("fixture json")
}

struct Responder {
    info: Value,
    sessions: Value,
    create: Value,
    prompt: Value,
    messages: Value,
    interrupt: Value,
    permissions: Value,
    forms: Value,
    /// Force every request to this status (error-path tests).
    fail_status: Option<u16>,
}

impl Respond for Responder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        if let Some(status) = self.fail_status {
            return ResponseTemplate::new(status)
                .set_body_json(serde_json::json!({ "message": "nope" }));
        }
        let method = request.method.to_string();
        let path = request.url.path();
        match (method.as_str(), path) {
            ("GET", "/api/info") => ResponseTemplate::new(200).set_body_json(&self.info),
            ("POST", "/api/session") => ResponseTemplate::new(200).set_body_json(&self.create),
            ("GET", "/api/session") => ResponseTemplate::new(200).set_body_json(&self.sessions),
            ("GET", "/api/session/active") => ResponseTemplate::new(200).set_body_json(
                serde_json::json!({ "data": { "ses_busy": { "type": "running" } } }),
            ),
            ("GET", "/api/permission/request") => {
                ResponseTemplate::new(200).set_body_json(&self.permissions)
            }
            ("GET", "/api/form") => ResponseTemplate::new(200).set_body_json(&self.forms),
            ("POST", p) if p.ends_with("/prompt") => {
                ResponseTemplate::new(200).set_body_json(&self.prompt)
            }
            ("GET", p) if p.ends_with("/message") => {
                ResponseTemplate::new(200).set_body_json(&self.messages)
            }
            ("POST", p) if p.ends_with("/interrupt") => {
                ResponseTemplate::new(200).set_body_json(&self.interrupt)
            }
            ("POST", p) if p.ends_with("/reply") => ResponseTemplate::new(200),
            _ => ResponseTemplate::new(404).set_body_string("unmocked"),
        }
    }
}

fn responder(fail_status: Option<u16>) -> Responder {
    Responder {
        info: fixture("info.json"),
        sessions: fixture("sessions_root.json"),
        create: fixture("session_create.json"),
        prompt: fixture("prompt_admission.json"),
        messages: fixture("messages.json"),
        interrupt: fixture("interrupt_idle.json"),
        permissions: fixture("permission_pending.json"),
        forms: fixture("form_pending.json"),
        fail_status,
    }
}

async fn adapter(fail_status: Option<u16>) -> (MockServer, OpenCodeAdapter) {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(responder(fail_status))
        .mount(&server)
        .await;
    let client = opencode_v2_client::Client::new(&server.uri(), "secret").expect("client");
    (server, OpenCodeAdapter::new(Arc::new(client)))
}

#[tokio::test]
async fn probe_and_sessions_map_to_core_types() {
    let (_server, port) = adapter(None).await;

    let facts = port.probe().await.expect("probe");
    assert_eq!(facts.version, "2.0.24");

    let sessions = port.list_sessions(true).await.expect("list");
    assert_eq!(sessions[0].id, "ses_fixture0000000000000001");

    let busy = port.activity("ses_busy").await.expect("busy");
    assert_eq!(busy, Activity::Busy);
    let idle = port.activity("ses_other").await.expect("idle");
    assert_eq!(idle, Activity::Idle);

    let session = port
        .create_session(Path::new("/tmp/work"), Some("title"), None)
        .await
        .expect("create");
    assert_eq!(session, "ses_ee76bda2dffexMwBaYTKuT51Me");

    let rules = vec![PermissionRule {
        action: "bash".to_owned(),
        resource: "git push*".to_owned(),
        effect: PermissionEffect::Ask,
    }];
    port.create_session(Path::new("/tmp/work"), None, Some(&rules))
        .await
        .expect("create with rules");
}

#[tokio::test]
async fn prompt_snapshot_interrupt_and_gates_map_to_core_types() {
    let (_server, port) = adapter(None).await;

    let admission = port.send_prompt("ses_x", "do it").await.expect("prompt");
    assert!(
        !admission.session_id.is_empty(),
        "admission carries a session"
    );

    let snapshot = port
        .last_assistant("ses_x")
        .await
        .expect("snapshot")
        .expect("present");
    assert_eq!(snapshot.text, "PONG");

    port.interrupt("ses_x").await.expect("interrupt");

    let gates = port.pending_gates().await.expect("gates");
    assert!(gates.iter().any(|gate| matches!(
        &gate.kind,
        ocbot_core::GateKind::Permission { action, .. } if action == "bash"
    )));
    assert!(gates.iter().any(|gate| gate.session_id.is_some()));

    port.reply_gate(
        "ses_fixture0000000000000001",
        "per_fixture00000000000001",
        GateDecision::Once,
    )
    .await
    .expect("reply permission");
    port.reply_form(
        "ses_fixture0000000000000001",
        "frm_fixture00000000000001",
        serde_json::json!({ "approach": "a" }),
    )
    .await
    .expect("reply form");
}

#[tokio::test]
async fn server_errors_map_to_the_api_error_class() {
    let (_server, port) = adapter(Some(500)).await;
    let error = port.probe().await.expect_err("probe fails");
    match error {
        OcError::Api { status, .. } => assert_eq!(status, 500),
        other => panic!("expected Api, got {other:?}"),
    }
}
