//! SPEC-004 PH-3 §5.9: the connection machine against the pinned fake —
//! closed-socket reconnects, dead-socket recovery, and the surface gauge the
//! subscribe transitions drive.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::signal_fake::{FakeSignal, Script, text_envelope};
use ocbot::adapters::signal::{Envelope, SignalMachine};
use ocbot::app::metrics::Metrics;

/// A machine wired for tests: fast floor/ceiling, deterministic jitter.
fn test_machine(socket: std::path::PathBuf, metrics: Arc<Metrics>) -> SignalMachine {
    SignalMachine::new(
        socket,
        Duration::from_millis(10),
        Duration::from_millis(20),
        metrics,
    )
    .with_jitter(Box::new(|| 0.5))
}

async fn eventually(condition: impl Fn() -> bool, what: &str) {
    for _ in 0..400 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never became true: {what}");
}

fn gauge(metrics: &Metrics, needle: &str) -> bool {
    metrics.render_text().contains(needle)
}

#[tokio::test]
async fn the_machine_reconnects_after_the_daemon_closes_the_subscription() {
    let dir = tempfile::tempdir().expect("tmp");
    let socket = dir.path().join("signal.sock");
    let daemon = FakeSignal::bind(
        socket.clone(),
        Script {
            // Every subscribe is acked, then the socket dies (stale-daemon
            // recovery without partial state — §5.9).
            close_after_subscribe: true,
            ..Script::default()
        },
    );
    let metrics = Arc::new(Metrics::new());
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    tokio::spawn(test_machine(socket, metrics.clone()).run_forever(tx));

    eventually(
        || daemon.connection_count() >= 3,
        "the machine reconnects past two closes",
    )
    .await;
    eventually(
        || gauge(&metrics, "ocbot_surface_up{surface=\"signal\"} 0"),
        "the gauge reflects the closed subscription",
    )
    .await;
}

#[tokio::test]
async fn a_dead_socket_recovers_when_the_daemon_returns() {
    let dir = tempfile::tempdir().expect("tmp");
    let socket = dir.path().join("signal.sock");
    // A stale socket file with no listener: connect is refused.
    {
        let stale = std::os::unix::net::UnixListener::bind(&socket).expect("stale bind");
        drop(stale);
    }
    let metrics = Arc::new(Metrics::new());
    let (tx, mut rx) = tokio::sync::mpsc::channel(16);
    let machine = test_machine(socket.clone(), metrics.clone());
    tokio::spawn(machine.run_forever(tx));

    // While the daemon is absent: no connection, gauge down.
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(
        metrics
            .render_text()
            .matches("ocbot_surface_up{surface=\"signal\"} 0")
            .count(),
        1,
        "the surface is down while the daemon is absent"
    );

    // The daemon appears at the same path: the machine recovers and pumps.
    let daemon = FakeSignal::bind(socket, Script::default());
    eventually(
        || gauge(&metrics, "ocbot_surface_up{surface=\"signal\"} 1"),
        "the gauge flips when the daemon returns",
    )
    .await;
    let id = daemon.subscribe_id();
    let streamed = text_envelope("+15550000005", 11, "after recovery");
    daemon.stream_envelope(id, &streamed);
    let envelope: Envelope = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timed out")
        .expect("an envelope");
    assert_eq!(envelope.text.as_deref(), Some("after recovery"));
}

#[tokio::test]
async fn a_wrong_version_daemon_is_still_a_healthy_connection_for_the_machine() {
    // The version gate is the *doctor's* job (FR-114); the machine owns the
    // socket, not policy. A wrong-version daemon must not flap the machine —
    // this pins the layering.
    let dir = tempfile::tempdir().expect("tmp");
    let socket = dir.path().join("signal.sock");
    let daemon = FakeSignal::bind(
        socket.clone(),
        Script {
            version: "0.11.0".to_owned(),
            ..Script::default()
        },
    );
    let metrics = Arc::new(Metrics::new());
    let (tx, _rx) = tokio::sync::mpsc::channel(16);
    tokio::spawn(test_machine(socket, metrics.clone()).run_forever(tx));

    eventually(
        || gauge(&metrics, "ocbot_surface_up{surface=\"signal\"} 1"),
        "the machine subscribes regardless of version",
    )
    .await;
    eventually(|| daemon.connection_count() >= 1, "one connection").await;
}
