//! SPEC-004 PH-3: the signal-cli contract layer against the pinned fake
//! daemon (`KNOWN_CAPS.md`). Every client method round-trips, envelopes decode
//! for every captured shape, unknown fields are tolerated, and correlation
//! ids stay correct under interleaving.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::time::Duration;

use common::signal_fake::{FakeSignal, Script, text_envelope};
use ocbot::adapters::signal::rpc::RpcError;
use ocbot::adapters::signal::{Envelope, SignalRpc};

async fn connect(script: Script) -> (tempfile::TempDir, FakeSignal, SignalRpc) {
    let dir = tempfile::tempdir().expect("tmp");
    let socket = dir.path().join("signal.sock");
    let daemon = FakeSignal::bind(socket.clone(), script);
    let rpc = SignalRpc::connect(&socket).await.expect("connect");
    (dir, daemon, rpc)
}

/// Wait until the daemon has accepted the client's connection: streaming a
/// raw line before the accept loop registers it would silently go nowhere.
async fn wait_for_connection(daemon: &FakeSignal) {
    for _ in 0..400 {
        if daemon.connection_count() >= 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the daemon never accepted the connection");
}

/// The id of the first request carrying `method` (read from the daemon's
/// recorded requests), for tests that must reply to an in-flight request.
async fn wait_for_method(daemon: &FakeSignal, method: &str) -> u64 {
    for _ in 0..400 {
        if let Some(recorded) = daemon
            .requests()
            .into_iter()
            .find(|recorded| recorded.method == method)
        {
            return recorded.id;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the daemon never saw a {method} request");
}

/// Wait for the drift counter to reach `want`, naming the offending line.
async fn wait_for_failures(rpc: &SignalRpc, want: u64, what: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while rpc.decode_failures() < want {
        assert!(
            std::time::Instant::now() < deadline,
            "{what} never counted as drift"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// The pinned daemon's death semantics (PH-8's chaos suite drives the client
/// through them): killing the daemon closes every live connection — the client
/// reads EOF — while the listener survives, so a reconnect is served again.
#[tokio::test]
async fn a_killed_daemon_closes_its_live_connections_and_keeps_listening() {
    use tokio::io::AsyncReadExt as _;
    let (dir, daemon, rpc) = connect(Script::default()).await;
    wait_for_connection(&daemon).await;
    let socket = dir.path().join("signal.sock");
    // A second client, so a *new* connection proves the listener survived.
    let mut second = tokio::net::UnixStream::connect(&socket)
        .await
        .expect("a second client connects");
    daemon.kill();
    // The first client sees EOF on its next read.
    let mut line = String::new();
    let read = tokio::time::timeout(Duration::from_secs(2), second.read_to_string(&mut line)).await;
    // Either client sees EOF: the machine's reader ends and it reconnects.
    drop(read);
    let ended = tokio::time::timeout(Duration::from_secs(2), rpc.subscribe())
        .await
        .expect("a dead socket answers promptly");
    assert!(
        matches!(ended, Err(RpcError::Closed)),
        "a killed daemon cannot serve a new subscription: {ended:?}"
    );
    // The listener is still bound, so a fresh client is accepted.
    let fresh = tokio::net::UnixStream::connect(&socket).await;
    assert!(fresh.is_ok(), "the listener survived the kill");
    drop(fresh);
    // A full shutdown unbinds it. The accept loop unwinds asynchronously, so
    // the refusal is polled for: the property under test is that a connect is
    // eventually refused, not that it is refused within one scheduler tick.
    daemon.shutdown();
    let mut refused = false;
    for _ in 0..200 {
        if tokio::net::UnixStream::connect(&socket).await.is_err() {
            refused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(refused, "a shut-down daemon refuses reconnects");
}

#[tokio::test]
async fn version_and_accounts_round_trip() {
    let (_dir, _daemon, rpc) = connect(Script::default()).await;
    assert_eq!(rpc.version().await.expect("version"), "0.15.0");
    assert_eq!(
        rpc.list_accounts().await.expect("accounts"),
        vec!["+15551234567".to_owned()]
    );
    assert!(rpc.is_connected());
}

#[tokio::test]
async fn send_and_reaction_round_trip_with_pinned_shapes() {
    let (_dir, daemon, rpc) = connect(Script::default()).await;
    let timestamp = rpc.send("+15557654321", "status?").await.expect("send");
    assert_eq!(timestamp, "1700000000000");
    let reaction = rpc
        .send_reaction("+15557654321", 1_699_999_999_999, "\u{1F44D}", false)
        .await
        .expect("reaction");
    assert_eq!(reaction, "1700000000001");

    let requests = daemon.requests();
    let send = &requests
        .iter()
        .find(|recorded| recorded.method == "send")
        .expect("a send request");
    assert_eq!(send.params[0]["recipient"], "+15557654321");
    assert_eq!(send.params[0]["message"], "status?");
    let reaction = requests
        .iter()
        .find(|recorded| recorded.method == "sendReaction")
        .expect("a reaction request");
    assert_eq!(reaction.params[0]["targetTimestamp"], 1_699_999_999_999u64);
    assert_eq!(reaction.params[0]["reaction"], "\u{1F44D}");
    assert_eq!(reaction.params[0]["remove"], false);
}

#[tokio::test]
async fn subscribe_streams_decoded_envelopes() {
    let (_dir, daemon, rpc) = connect(Script::default()).await;
    let mut envelopes = rpc.subscribe().await.expect("subscribe");
    let id = daemon.subscribe_id();

    let envelope = text_envelope("+15550000001", 1_700_000_000_000, "status?");
    daemon.stream_envelope(id, &envelope);
    let first = tokio::time::timeout(Duration::from_secs(2), envelopes.recv())
        .await
        .expect("timed out")
        .expect("an envelope");
    assert_eq!(first.sender, "+15550000001");
    assert_eq!(first.text.as_deref(), Some("status?"));
    assert_eq!(first.account.as_deref(), Some("+15551234567"));

    // A reaction envelope decodes through the same stream.
    let reaction = serde_json::json!({
        "envelope": {
            "source": "+15550000002",
            "timestamp": 1_700_000_001_000u64,
            "dataMessage": {
                "reaction": {
                    "emoji": "\u{1F44D}",
                    "targetSentTimestamp": 1_700_000_000_000u64,
                    "isRemove": false
                }
            }
        }
    });
    daemon.stream_envelope(id, &reaction);
    let second = tokio::time::timeout(Duration::from_secs(2), envelopes.recv())
        .await
        .expect("timed out")
        .expect("an envelope");
    assert_eq!(
        second.reaction.expect("a reaction").target_timestamp_ms,
        1_700_000_000_000
    );
}

#[tokio::test]
async fn unknown_fields_do_not_break_the_stream() {
    let (_dir, daemon, rpc) = connect(Script::default()).await;
    let mut envelopes = rpc.subscribe().await.expect("subscribe");
    let future_fields = serde_json::json!({
        "envelope": {
            "source": "+15550000001",
            "timestamp": 5,
            "someFutureField": {"x": 1},
            "dataMessage": {"message": "hi", "newIn0_16": true}
        },
        "anotherFutureField": [1, 2]
    });
    daemon.stream_envelope(daemon.subscribe_id(), &future_fields);
    let envelope = tokio::time::timeout(Duration::from_secs(2), envelopes.recv())
        .await
        .expect("timed out")
        .expect("an envelope");
    assert_eq!(envelope.text.as_deref(), Some("hi"));
    assert_eq!(rpc.decode_failures(), 0, "tolerance is not drift");
}

#[tokio::test]
async fn garbage_lines_count_as_drift_but_do_not_kill_the_stream() {
    let (_dir, daemon, rpc) = connect(Script::default()).await;
    let mut envelopes = rpc.subscribe().await.expect("subscribe");
    let id = daemon.subscribe_id();
    for _ in 0..3 {
        daemon.stream_line("this is not json at all");
    }
    // The failures accrue on this connection…
    for _ in 0..100 {
        if rpc.decode_failures() == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(rpc.decode_failures(), 3);
    // …and a valid envelope still lands (a burst is what reconnects).
    let alive = text_envelope("+15550000009", 7, "still alive");
    daemon.stream_envelope(id, &alive);
    let envelope = tokio::time::timeout(Duration::from_secs(2), envelopes.recv())
        .await
        .expect("timed out")
        .expect("an envelope");
    assert_eq!(envelope.text.as_deref(), Some("still alive"));
}

#[tokio::test]
async fn a_failed_subscribe_leaves_no_subscription_behind() {
    // A daemon that refuses the subscription must not leave the id armed: a
    // later envelope line carrying it is drift, not a phantom subscription.
    let script = Script {
        subscribe_error: Some((-32601, "subscribe unavailable".to_owned())),
        ..Script::default()
    };
    let (_dir, _daemon, rpc) = connect(script).await;
    let error = rpc.subscribe().await.expect_err("the daemon refuses");
    assert!(
        matches!(error, RpcError::Wire { code, .. } if code == -32601),
        "expected the daemon's wire error, got {error:?}"
    );
    // The connection survives: the failure is one request, not the socket.
    assert!(rpc.is_connected());
    assert_eq!(rpc.version().await.expect("version still works"), "0.15.0");
}

#[tokio::test]
async fn a_malformed_reply_errors_the_request_it_belongs_to() {
    // A reply with neither `result` nor `error` is a decode failure on that
    // one request — not a hang, and not a socket teardown. The delayed
    // `version` reply is the handle: the raw line lands while it is pending.
    let script = Script {
        // A silent daemon: the request stays pending (no reply is ever
        // written) while the connection loop stays free to deliver raw lines.
        hang: true,
        ..Script::default()
    };
    let (_dir, daemon, rpc) = connect(script).await;
    let send = tokio::spawn({
        let rpc = rpc.clone();
        async move { rpc.send("+15550000001", "in flight").await }
    });
    let id = wait_for_method(&daemon, "send").await;
    daemon.stream_line(&serde_json::json!({"jsonrpc": "2.0", "id": id}).to_string());
    let error = send
        .await
        .expect("join")
        .expect_err("a malformed reply is an error");
    assert!(
        matches!(error, RpcError::Decode(_)),
        "expected a decode error, got {error:?}"
    );
    // The socket is still the socket: the failure belonged to the request.
    assert!(rpc.is_connected());
}

#[tokio::test]
async fn a_non_string_reaction_reply_is_a_decode_error() {
    // The daemon's `sendReaction` result pinned as a string; an object reply
    // is protocol drift the client must report.
    let script = Script {
        reaction_reply: Some(serde_json::json!({"timestamp": 1_700_000_000_001_u64})),
        ..Script::default()
    };
    let (_dir, _daemon, rpc) = connect(script).await;
    let error = rpc
        .send_reaction("+15550000001", 1_699_999_999_999, "\u{1F44D}", false)
        .await
        .expect_err("drifted reply");
    assert!(
        matches!(error, RpcError::Decode(_)),
        "expected a decode error, got {error:?}"
    );
}

#[tokio::test]
async fn a_reply_nobody_asked_for_counts_as_drift() {
    // A correlated reply for an id with no pending request (an aborted
    // request after a reconnect) is drift: counted, never fatal at one.
    let (_dir, daemon, rpc) = connect(Script::default()).await;
    wait_for_connection(&daemon).await;
    daemon.stream_line(
        &serde_json::json!({"jsonrpc": "2.0", "id": 9_999, "result": null}).to_string(),
    );
    wait_for_failures(&rpc, 1, "the orphan reply").await;
    // A line with no id at all is drift too (nothing to correlate it to).
    daemon.stream_line(&serde_json::json!({"jsonrpc": "2.0", "method": "notice"}).to_string());
    wait_for_failures(&rpc, 2, "the id-less line").await;
}

#[tokio::test]
async fn interleaved_requests_correlate_by_id() {
    let script = Script {
        // The slowest method answers last — ids must not cross.
        version_delay_ms: 80,
        ..Script::default()
    };
    let (_dir, _daemon, rpc) = connect(script).await;
    let (version, send, accounts) = tokio::join!(
        rpc.version(),
        rpc.send("+15550000001", "mid-flight"),
        rpc.list_accounts()
    );
    assert_eq!(version.expect("version"), "0.15.0");
    assert_eq!(send.expect("send"), "1700000000000");
    assert_eq!(accounts.expect("accounts"), vec!["+15551234567".to_owned()]);
}

#[tokio::test]
async fn a_jsonrpc_error_reply_is_a_typed_wire_error() {
    let script = Script {
        send_error: Some((-1, "unregistered user".to_owned())),
        ..Script::default()
    };
    let (_dir, _daemon, rpc) = connect(script).await;
    let error = rpc.send("+15550000001", "hi").await.expect_err("refused");
    match error {
        RpcError::Wire { code, message } => {
            assert_eq!(code, -1);
            assert_eq!(message, "unregistered user");
        }
        other => panic!("expected a wire error, got {other:?}"),
    }
}

#[tokio::test]
async fn a_malformed_result_is_a_decode_error_not_a_panic() {
    let script = Script {
        // The daemon answers `version` with a non-string (protocol drift the
        // client must report, not crash on).
        version_reply: Some(serde_json::json!({"version": "0.15.0"})),
        ..Script::default()
    };
    let (_dir, _daemon, rpc) = connect(script).await;
    let error = rpc.version().await.expect_err("drifted reply");
    assert!(
        matches!(error, RpcError::Decode(_)),
        "expected a decode error, got {error:?}"
    );
}

#[tokio::test]
async fn requests_after_the_socket_dies_fail_as_closed() {
    let script = Script {
        close_after_subscribe: true,
        ..Script::default()
    };
    let (_dir, _daemon, rpc) = connect(script).await;
    let _ack = rpc
        .subscribe()
        .await
        .expect("the ack arrives before the close");
    // The close races the reader task; wait for it to notice (the pending map
    // drains with `Closed`, the writer drops).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while rpc.is_connected() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the reader task never noticed the close"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let error = rpc.version().await.expect_err("the daemon is gone");
    assert!(
        matches!(error, RpcError::Closed),
        "expected Closed, got {error:?}"
    );
    assert!(!rpc.is_connected());
    assert!(rpc.decode_failures() == 0, "a close is not drift");
}

#[tokio::test]
async fn envelope_decode_covers_the_captured_corpus() {
    // The corpus lives in the unit-level shape tests too; this pins the
    // *public* decode entry against the documented subset shapes. Every
    // load-bearing field is asserted: a missing camelCase rename decodes as
    // `None` silently, and an assertion-free corpus test would hide it.
    let text =
        Envelope::decode(&text_envelope("+15551234567", 42, "hello")).expect("text envelope");
    assert_eq!(text.sender, "+15551234567");
    assert_eq!(text.timestamp_ms, 42);
    assert_eq!(text.text.as_deref(), Some("hello"));
    assert_eq!(text.account.as_deref(), Some("+15551234567"));
    assert!(!text.is_group);
    assert!(text.reaction.is_none());
    assert!(text.quote.is_none());
    assert!(text.attachments.is_empty());

    let group = Envelope::decode(&serde_json::json!({
        "envelope": {
            "source": "+15551234567",
            "timestamp": 1,
            "dataMessage": {"message": "hi all", "groupV2": {"groupId": "Zm9v"}}
        }
    }))
    .expect("group envelope");
    assert!(group.is_group, "FR-104's policy input is visible");
    assert_eq!(group.text.as_deref(), Some("hi all"));

    // reaction + quote + attachment localPath: the remaining camelCase names.
    let rich = Envelope::decode(&serde_json::json!({
        "envelope": {
            "source": "+15550000009",
            "timestamp": 5,
            "dataMessage": {
                "message": "see attached",
                "attachments": [{"contentType": "image/png", "filename": "shot.png",
                                 "localPath": "/var/lib/signal-cli/attach/shot.png"}],
                "quote": {"id": 4, "author": "+15550000001", "text": "status?"},
                "reaction": {"emoji": "\u{1F44D}", "targetSentTimestamp": 3,
                             "isRemove": false}
            }
        }
    }))
    .expect("rich envelope");
    assert_eq!(
        rich.attachments[0].local_path.as_deref(),
        Some("/var/lib/signal-cli/attach/shot.png")
    );
    assert_eq!(
        rich.attachments[0].content_type.as_deref(),
        Some("image/png")
    );
    assert_eq!(rich.quote.expect("quote").target_timestamp_ms, 4);
    assert_eq!(rich.reaction.expect("reaction").target_timestamp_ms, 3);
}
