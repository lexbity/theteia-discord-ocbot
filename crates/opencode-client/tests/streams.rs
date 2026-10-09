//! The SSE streaming surface (`log_after` / `subscribe_events`): frame parsing,
//! the durable-log head marker, the global event stream's `durable` object,
//! comment/heartbeat skipping, decode errors, and non-success status handling.
//! Served by wiremock from the recorded Phase 0 fixtures.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

use std::path::{Path, PathBuf};

use opencode_v2_client::{Client, OcError};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn fixture(name: &str) -> String {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/v2/2.0.24")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("fixture {name}: {error}"))
}

fn sse(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

#[tokio::test]
async fn durable_log_stream_parses_the_head_marker_then_ends() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/experimental/session/ses_x/log"))
        .respond_with(sse(fixture("log_synced.sse")))
        .mount(&server)
        .await;

    let client = Client::new(&server.uri(), "secret").expect("client");
    let mut stream = client
        .log_after("ses_x", Some(1), true)
        .await
        .expect("stream opens");
    let entry = stream.next().await.expect("one entry").expect("decodes");
    assert_eq!(entry.kind, "log.synced");
    assert_eq!(entry.seq, Some(28));
    assert_eq!(
        entry.raw.get("aggregateID").and_then(|v| v.as_str()),
        Some("ses_ee76bda2dffexMwBaYTKuT51Me")
    );
    assert!(stream.next().await.is_none(), "the stream ends cleanly");
}

#[tokio::test]
async fn global_event_stream_parses_durable_positions_and_skips_heartbeats() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/event"))
        .respond_with(sse(fixture("event_stream.sse")))
        .mount(&server)
        .await;

    let client = Client::new(&server.uri(), "secret").expect("client");
    let mut stream = client.subscribe_events().await.expect("stream opens");

    let connected = stream.next().await.expect("event").expect("decodes");
    assert_eq!(connected.kind, "server.connected");

    // The heartbeat comment frame between events must be skipped.
    let enqueued = stream.next().await.expect("event").expect("decodes");
    assert_eq!(enqueued.kind, "session.inbox.enqueued");
    assert!(enqueued.created.is_some(), "created timestamp parsed");
    let durable = enqueued.durable.expect("durable position");
    assert_eq!(durable.aggregate_id, "ses_ee76bda2dffexMwBaYTKuT51Me");
    assert_eq!(durable.seq, 11);
    assert_eq!(durable.version, Some(1));
}

/// S-10 fixture (SPEC-003 W0): every frame kind of the scripted spike run
/// decodes with its kind, key, and `durable` position; comment heartbeats are
/// skipped; the stream ends cleanly. This pins the live-recorded taxonomy —
/// see `devdocs/compat/EVENTS.md`.
#[tokio::test]
async fn scripted_event_fixture_decodes_every_observed_kind() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/event"))
        .respond_with(sse(fixture("events_scripted.sse")))
        .mount(&server)
        .await;

    let client = Client::new(&server.uri(), "secret").expect("client");
    let mut stream = client.subscribe_events().await.expect("stream opens");

    let mut kinds = Vec::new();
    while let Some(Ok(event)) = stream.next().await {
        kinds.push(event.kind);
    }
    assert_eq!(kinds.first().map(String::as_str), Some("server.connected"));
    for expected in [
        "session.created",
        "session.inbox.enqueued",
        "session.execution.started",
        "session.step.started",
        "session.step.failed",
        "session.execution.failed",
        "session.deleted",
    ] {
        assert!(
            kinds.iter().any(|kind| kind == expected),
            "events_scripted.sse missing {expected}"
        );
    }
}

/// S-10 quiet fixture: a connect frame followed only by heartbeat comments
/// — the parser must skip them and report a clean end of stream (AE-4
/// heartbeat evidence, pinned deterministically).
#[tokio::test]
async fn quiet_event_fixture_yields_only_the_connect_frame() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/event"))
        .respond_with(sse(fixture("events_quiet.sse")))
        .mount(&server)
        .await;

    let client = Client::new(&server.uri(), "secret").expect("client");
    let mut stream = client.subscribe_events().await.expect("stream opens");

    let connected = stream.next().await.expect("event").expect("decodes");
    assert_eq!(connected.kind, "server.connected");
    assert!(
        stream.next().await.is_none(),
        "heartbeats are skipped; the stream ends cleanly"
    );
}

#[tokio::test]
async fn log_stream_surfaces_a_decode_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/experimental/session/ses_bad/log"))
        .respond_with(sse("data: this is not json\n\n".to_owned()))
        .mount(&server)
        .await;

    let client = Client::new(&server.uri(), "secret").expect("client");
    let mut stream = client
        .log_after("ses_bad", None, false)
        .await
        .expect("stream opens");
    let error = stream
        .next()
        .await
        .expect("frame")
        .expect_err("decode error");
    assert!(matches!(error, OcError::Decode(_)), "got {error:?}");
}

#[tokio::test]
async fn log_after_reports_a_non_success_status_with_the_server_message() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/experimental/session/ses_down/log"))
        .respond_with(
            ResponseTemplate::new(500).set_body_json(serde_json::json!({ "message": "boom" })),
        )
        .mount(&server)
        .await;

    let client = Client::new(&server.uri(), "secret").expect("client");
    let Err(error) = client.log_after("ses_down", None, false).await else {
        panic!("expected an API error");
    };
    match error {
        OcError::Api { status, message } => {
            assert_eq!(status, 500);
            assert_eq!(message, "boom");
        }
        other => panic!("expected Api, got {other:?}"),
    }
}

#[tokio::test]
async fn event_stream_reports_a_non_success_status_with_a_non_json_body() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/event"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
        .mount(&server)
        .await;

    let client = Client::new(&server.uri(), "secret").expect("client");
    let Err(error) = client.subscribe_events().await else {
        panic!("expected an API error");
    };
    match error {
        OcError::Api { status, message } => {
            assert_eq!(status, 503);
            assert_eq!(message, "upstream down");
        }
        other => panic!("expected Api, got {other:?}"),
    }
}
