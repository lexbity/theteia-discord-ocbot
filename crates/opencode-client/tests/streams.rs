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
async fn global_event_stream_yields_typed_frames_and_skips_heartbeats() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/event"))
        .respond_with(sse(fixture("event_stream.sse")))
        .mount(&server)
        .await;

    let client = Client::new(&server.uri(), "secret").expect("client");
    let mut stream = client.subscribe_events().await.expect("stream opens");

    let connected = stream.next_frame().await.expect("frame").expect("decodes");
    assert_eq!(connected.event, "server.connected");
    assert_eq!(connected.session_id, None, "global kind is unkeyed");

    // The heartbeat comment frame between events must be skipped.
    let enqueued = stream.next_frame().await.expect("frame").expect("decodes");
    assert_eq!(enqueued.event, "session.inbox.enqueued");
    assert_eq!(
        enqueued.session_id,
        Some("ses_ee76bda2dffexMwBaYTKuT51Me".to_owned())
    );
    // The full lifecycle drains to a clean end, every frame keyed by the
    // session, heartbeats skipped.
    while let Ok(Some(frame)) = stream.next_frame().await {
        assert_eq!(
            frame.session_id,
            Some("ses_ee76bda2dffexMwBaYTKuT51Me".to_owned())
        );
    }
    assert_eq!(stream.decode_failures(), 0, "fixture decodes cleanly");
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
    let mut keyed = 0;
    while let Ok(Some(frame)) = stream.next_frame().await {
        if frame.session_id.is_some() {
            keyed += 1;
        }
        kinds.push(frame.event);
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
    assert!(keyed > 0, "session kinds carry their session key");
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

    let connected = stream.next_frame().await.expect("frame").expect("decodes");
    assert_eq!(connected.event, "server.connected");
    assert!(
        stream.next_frame().await.expect("clean end").is_none(),
        "heartbeats are skipped; the stream ends cleanly"
    );
    assert_eq!(stream.decode_failures(), 0);
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

// --- adversarial battery (SPEC-003 W2, P-24): garbage is skipped or counted,
// never fatal, never a panic. ------------------------------------------------

/// One adversarial body served on `/api/event`; returns what the typed stream
/// yields and how many decode failures it counted.
async fn adversarial_stream_yields(body: String) -> (Vec<opencode_v2_client::EventFrame>, u64) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/event"))
        .respond_with(sse(body))
        .mount(&server)
        .await;
    let client = Client::new(&server.uri(), "secret").expect("client");
    let mut stream = client.subscribe_events().await.expect("stream opens");
    let mut frames = Vec::new();
    while let Ok(Some(frame)) = stream.next_frame().await {
        frames.push(frame);
    }
    (frames, stream.decode_failures())
}

#[tokio::test]
async fn truncated_json_is_counted_and_skipped() {
    let (frames, failures) = adversarial_stream_yields(
        "data: {\"type\":\"session.created\",\"data\":{\"sessionID\":\"ses_a\"}\n\n\
         data: {\"type\":\"session.deleted\",\"data\":{\"sessionID\":\"ses_a\"}}\n\n"
            .to_owned(),
    )
    .await;
    assert_eq!(frames.len(), 1, "only the well-formed frame yields");
    assert_eq!(frames[0].event, "session.deleted");
    assert_eq!(failures, 1, "the truncated frame counted exactly once");
}

#[tokio::test]
async fn data_without_a_discriminator_is_counted_and_skipped() {
    let (frames, failures) = adversarial_stream_yields(
        "data: {\"created\":1791504777105}\n\n\
         data: {\"type\":\"server.connected\",\"data\":{}}\n\n"
            .to_owned(),
    )
    .await;
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].event, "server.connected");
    assert_eq!(failures, 1);
}

#[tokio::test]
async fn an_sse_event_field_substitutes_for_a_missing_discriminator() {
    let (frames, failures) =
        adversarial_stream_yields("event: custom.kind\ndata: {}\n\n".to_owned()).await;
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].event, "custom.kind");
    assert_eq!(failures, 0, "a discriminator via the SSE field is valid");
}

#[tokio::test]
async fn an_event_field_with_no_data_at_all_yields_the_kind() {
    let (frames, failures) = adversarial_stream_yields("event: bare.kind\n\n".to_owned()).await;
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].event, "bare.kind");
    assert_eq!(frames[0].session_id, None);
    assert_eq!(failures, 0);
}

#[tokio::test]
async fn empty_data_lines_are_plain_heartbeats() {
    let (frames, failures) =
        adversarial_stream_yields(": keep-alive\n\ndata: \n\n".to_owned()).await;
    assert!(frames.is_empty());
    assert_eq!(failures, 0, "silence is not corruption");
}

#[tokio::test]
async fn garbage_between_good_frames_never_kills_the_connection() {
    let (frames, failures) = adversarial_stream_yields(
        "data: one\n\ndata: []\n\ndata: {\"type\":\"a\"}\n\ndata: null\n\ndata: {\"type\":\"b\"}\n\n"
            .to_owned(),
    )
    .await;
    assert_eq!(
        frames
            .iter()
            .map(|frame| frame.event.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b"]
    );
    assert_eq!(failures, 3);
}

#[tokio::test]
async fn decode_failures_accumulate_past_the_drift_burst_threshold() {
    let mut body = String::new();
    for _ in 0..40 {
        body.push_str("data: not json\n\n");
    }
    let (frames, failures) = adversarial_stream_yields(body).await;
    assert!(frames.is_empty());
    assert_eq!(failures, 40);
    assert!(
        failures > u64::from(opencode_v2_client::DECODE_BURST_RECONNECT),
        "the burst threshold ({}) is crossed by 40 garbage frames",
        opencode_v2_client::DECODE_BURST_RECONNECT
    );
}
