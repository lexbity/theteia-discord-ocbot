//! Version gate (AC-16) and HTTP-level contract tests against a mock server.
//!
//! These tests prove the client treats the server as untrusted-wire: it knows
//! only what `/api/info` reports, refuses v1, authenticates with
//! `Authorization: Basic` (username `opencode`), and hits exactly the paths
//! the vendored spec pins.
//!
//! Integration-test crates assert by unwrapping/panicking; the workspace
//! `panic`/`unwrap`/`expect` bans exist to keep *production* code honest
//! (spec C6, NFR-10).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use opencode_v2_client::{Activity, Client, GateKind, OcError, ServerInfo, SessionInfo};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// base64("opencode:secret")
const AUTH_HEADER: &str = "Basic b3BlbmNvZGU6c2VjcmV0";

fn mock_client(server: &MockServer) -> Client {
    Client::new(&server.uri(), "secret").expect("client builds")
}

#[tokio::test]
async fn probe_accepts_v2_info_and_checks_auth() {
    let server = MockServer::start().await;
    let info = common::load_fixture("info.json").expect("fixture info.json loads");
    Mock::given(method("GET"))
        .and(path("/api/info"))
        .and(header("authorization", AUTH_HEADER))
        .respond_with(ResponseTemplate::new(200).set_body_json(&info))
        .mount(&server)
        .await;

    let client = mock_client(&server);
    let info: ServerInfo = client.info().await.expect("v2 info decodes");
    assert!(info.is_v2());
    let probed = client.probe().await.expect("probe passes for v2");
    assert_eq!(probed.version, info.version);
}

#[tokio::test]
async fn probe_refuses_v1_shaped_info() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            common::load_fixture("v1_info.json").expect("fixture v1_info.json loads"),
        ))
        .mount(&server)
        .await;

    let client = mock_client(&server);
    let error = client.probe().await.expect_err("v1 must be refused");
    assert!(
        matches!(error,
            OcError::UnsupportedServerVersion { ref found } if found == "1.24.0"),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn probe_reports_missing_info_endpoint_as_v1_signature() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/info"))
        .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
        .mount(&server)
        .await;

    let client = mock_client(&server);
    let error = client
        .probe()
        .await
        .expect_err("404 must be a version signal");
    assert!(
        matches!(
            error,
            OcError::MissingInfoEndpoint { status: 404 } | OcError::Api { status: 404, .. }
        ),
        "unexpected error: {error}"
    );
    // The specific variant is asserted by the unit fixture test in src/info.rs;
    // structurally a 404 must surface as a refusal.
}

#[tokio::test]
async fn no_credentials_are_rejected_cleanly() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/info"))
        .respond_with(ResponseTemplate::new(401).set_body_json(
            common::load_fixture("error_401.json").expect("fixture error_401.json loads"),
        ))
        .mount(&server)
        .await;

    let client = Client::new(&server.uri(), "wrong").expect("client builds");
    let error = client.info().await.expect_err("401 must surface");
    match error {
        OcError::Api { status, message } => {
            assert_eq!(status, 401);
            assert!(message.contains("Authentication"), "message: {message}");
        }
        other => panic!("expected Api error, got {other}"),
    }
}

#[tokio::test]
async fn api_error_decodes_server_message() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/session/ses_doesnotexist000000000000"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(
                common::load_fixture("error_404_session.json")
                    .expect("fixture error_404_session.json loads"),
            ),
        )
        .mount(&server)
        .await;

    let client = mock_client(&server);
    let error = client
        .get_session("ses_doesnotexist000000000000")
        .await
        .expect_err("404 must surface");
    match error {
        OcError::Api { status, message } => {
            assert_eq!(status, 404);
            assert!(
                message.contains("not found"),
                "server message should be surfaced: {message}"
            );
        }
        other => panic!("expected Api error, got {other}"),
    }
}

#[tokio::test]
async fn list_create_activity_prompt_interrupt_messages_crud() {
    let server = MockServer::start().await;
    let session_id = "ses_mock000000000000000001";

    Mock::given(method("GET"))
        .and(path("/api/session"))
        .and(header("authorization", AUTH_HEADER))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            common::load_fixture("sessions_root.json").expect("fixture sessions_root.json loads"),
        ))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/api/session/active"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            common::load_fixture("session_active.json").expect("fixture session_active.json loads"),
        ))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/api/session"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            common::load_fixture("session_create.json").expect("fixture session_create.json loads"),
        ))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/api/session/{session_id}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            common::load_fixture("session_get.json").expect("fixture session_get.json loads"),
        ))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path(format!("/api/session/{session_id}/prompt")))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                common::load_fixture("prompt_admission.json")
                    .expect("fixture prompt_admission.json loads"),
            ),
        )
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path(format!("/api/session/{session_id}/interrupt")))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            common::load_fixture("interrupt_idle.json").expect("fixture interrupt_idle.json loads"),
        ))
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/api/session/{session_id}/message")))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            common::load_fixture("messages.json").expect("fixture messages.json loads"),
        ))
        .mount(&server)
        .await;

    let client = mock_client(&server);

    let roots = client.list_sessions(true, None).await.expect("list roots");
    // Root-only filtering was requested; every returned session must be a root.
    assert!(roots.iter().all(SessionInfo::is_root));

    let activity = client
        .activity(session_id)
        .await
        .expect("activity derives from active map");
    assert_eq!(activity, Activity::Idle);

    let created = client
        .create_session(Some("/tmp/scratch"), Some("label"))
        .await
        .expect("create session");
    assert_eq!(
        created.id,
        common::load_fixture("session_create.json").expect("fixture session_create.json loads")["data"]
            ["id"]
    );

    let admission = client
        .send_prompt(session_id, "Reply with PONG")
        .await
        .expect("prompt admitted");
    assert_eq!(admission.delivery.as_deref(), Some("steer"));

    let interrupt = client.interrupt(session_id).await.expect("interrupt");
    assert!(!interrupt.interrupted);

    let assistant = client
        .last_assistant(session_id)
        .await
        .expect("last assistant")
        .expect("an assistant message exists");
    assert_eq!(assistant.text(), "PONG");
}

#[tokio::test]
async fn pending_gates_unify_permissions_and_forms() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/permission/request"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                common::load_fixture("permission_pending.json")
                    .expect("fixture permission_pending.json loads"),
            ),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/form"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            common::load_fixture("form_pending.json").expect("fixture form_pending.json loads"),
        ))
        .mount(&server)
        .await;

    let client = mock_client(&server);
    let gates = client.pending_gates().await.expect("gate list");
    assert_eq!(gates.len(), 2);
    assert!(matches!(gates[0].kind, GateKind::Permission(_)));
    assert!(matches!(gates[1].kind, GateKind::Form(_)));
}

#[tokio::test]
async fn gate_replies_post_to_session_scoped_paths() {
    let server = MockServer::start().await;
    let session_id = "ses_mock000000000000000001";

    Mock::given(method("POST"))
        .and(path(format!(
            "/api/session/{session_id}/permission/per_x/reply"
        )))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/api/session/{session_id}/form/frm_x/reply")))
        .respond_with(ResponseTemplate::new(204))
        .mount(&server)
        .await;

    let client = mock_client(&server);
    client
        .reply_permission(
            session_id,
            "per_x",
            opencode_v2_client::GateDecision::Always,
        )
        .await
        .expect("permission reply accepted");
    client
        .reply_form(
            session_id,
            "frm_x",
            serde_json::json!({ "approach": "a", "notify": true }),
        )
        .await
        .expect("form reply accepted");
}
