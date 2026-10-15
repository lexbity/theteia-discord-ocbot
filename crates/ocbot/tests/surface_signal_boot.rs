//! SPEC-004 PH-4: the Signal surface's boot wiring and per-surface health —
//! the live-connection slot, the `ocbot_surface_up` gauge, and the send
//! counters the instrumented decorator records.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::signal_fake::{FakeSignal, Script};
use ocbot::adapters::signal::{SignalAdapter, SignalMachine};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::context::ChatPort;
use ocbot::app::metrics::Metrics;
use ocbot::app::surfaces::{InstrumentedPort, SURFACE_DOWN_FAILURES};
use ocbot_core::{Destination, OutgoingMessage, Surface};

const OPERATOR: &str = "+15551234567";

fn notice(text: &str) -> OutgoingMessage {
    OutgoingMessage {
        content: text.to_owned(),
        attachment: None,
        components: Vec::new(),
        mention_allowlist: Vec::new(),
    }
}

/// Poll the rendered metrics for `needle`.
async fn wait_metric(metrics: &Metrics, needle: &str) {
    for _ in 0..600 {
        if metrics.render_text().contains(needle) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("metric never appeared: {needle}\n{}", metrics.render_text());
}

/// A machine + adapter under a live fake daemon.
fn surface(
    script: Script,
) -> (
    tempfile::TempDir,
    FakeSignal,
    Arc<Metrics>,
    Arc<SignalAdapter>,
) {
    let dir = tempfile::tempdir().expect("tmp");
    let socket = dir.path().join("signal.sock");
    let daemon = FakeSignal::bind(socket.clone(), script);
    let metrics = Arc::new(Metrics::new());
    let store = StoreHandle::open_in_memory().expect("store");
    let machine = SignalMachine::new(
        socket,
        Duration::from_millis(10),
        Duration::from_millis(20),
        metrics.clone(),
    )
    .with_jitter(Box::new(|| 0.5));
    let slot = machine.connection();
    let adapter = Arc::new(SignalAdapter::new(
        OPERATOR,
        slot,
        store,
        metrics.clone(),
        dir.path().join("outbox"),
    ));
    let (sink_tx, _sink_rx) = tokio::sync::mpsc::channel(16);
    tokio::spawn(machine.run_forever(sink_tx));
    (dir, daemon, metrics, adapter)
}

#[tokio::test]
async fn the_signal_surface_reports_up_and_counts_sends() {
    let (_dir, _daemon, metrics, adapter) = surface(Script::default());
    wait_metric(&metrics, "ocbot_surface_up{surface=\"signal\"} 1").await;

    let instrumented = InstrumentedPort::new(
        Surface::Signal,
        adapter.clone(),
        metrics.clone(),
        Vec::new(),
    );
    let receipt = instrumented
        .post(&adapter.destination(), &notice("hello"))
        .await
        .expect("post");
    assert_eq!(receipt.surface, Surface::Signal);
    wait_metric(
        &metrics,
        "ocbot_surface_send_total{surface=\"signal\",op=\"post\"} 1",
    )
    .await;
    assert!(
        metrics
            .render_text()
            .contains("ocbot_surface_up{surface=\"signal\"} 1"),
        "a successful send keeps the surface up"
    );
}

#[tokio::test]
async fn a_closed_subscription_flips_the_gauge_down() {
    let (_dir, _daemon, metrics, _adapter) = surface(Script {
        close_after_subscribe: true,
        ..Script::default()
    });
    wait_metric(&metrics, "ocbot_surface_up{surface=\"signal\"} 0").await;
}

#[tokio::test]
async fn repeated_send_failures_flip_the_health_gauge() {
    let (_dir, _daemon, metrics, adapter) = surface(Script {
        send_error: Some((-1, "unregistered".to_owned())),
        ..Script::default()
    });
    wait_metric(&metrics, "ocbot_surface_up{surface=\"signal\"} 1").await;

    let instrumented = InstrumentedPort::new(
        Surface::Signal,
        adapter.clone(),
        metrics.clone(),
        Vec::new(),
    );
    for _ in 0..SURFACE_DOWN_FAILURES {
        let error = instrumented
            .post(&adapter.destination(), &notice("fail"))
            .await
            .expect_err("the daemon refuses the send");
        assert_eq!(error.surface, Surface::Signal);
    }
    wait_metric(&metrics, "ocbot_surface_up{surface=\"signal\"} 0").await;
    wait_metric(
        &metrics,
        &format!(
            "ocbot_surface_send_failures_total{{surface=\"signal\",op=\"post\"}} {SURFACE_DOWN_FAILURES}"
        ),
    )
    .await;
}

#[tokio::test]
async fn a_post_with_no_live_connection_fails_loud() {
    // Never start a machine: the slot is empty, so the surface is down and a
    // send must fail (never a silent drop).
    let dir = tempfile::tempdir().expect("tmp");
    let metrics = Arc::new(Metrics::new());
    let store = StoreHandle::open_in_memory().expect("store");
    let machine = SignalMachine::new(
        dir.path().join("absent.sock"),
        Duration::from_millis(10),
        Duration::from_millis(20),
        metrics.clone(),
    );
    let adapter = SignalAdapter::new(
        OPERATOR,
        machine.connection(),
        store,
        metrics.clone(),
        dir.path().join("outbox"),
    );
    let error = adapter
        .post(
            &Destination {
                surface: Surface::Signal,
                chat_ref: ocbot_core::ChatRef(OPERATOR.to_owned()),
            },
            &notice("gone"),
        )
        .await
        .expect_err("no connection");
    assert_eq!(error.surface, Surface::Signal);
    assert!(error.message.contains("no live"), "{}", error.message);
}
