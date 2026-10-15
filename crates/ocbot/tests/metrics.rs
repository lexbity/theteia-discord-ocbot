//! Metrics completeness (NFR-9 / §6.8): after a scripted scenario the loopback
//! HTTP endpoint exposes every named family, including a real
//! `idle_to_report_seconds` observation taken from a completed report.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::net::SocketAddr;
use std::sync::Arc;

use common::{FakeChat, FakeClock, FakeGit, FakeOpenCode, context_with, seed_running};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::metrics;
use ocbot::app::watcher::Watcher;
use ocbot_core::{Column, State, WriteValue};

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;

async fn set_started_at(store: &StoreHandle, action_id: i64, started: i64) {
    let writes = [
        (Column::StartedAt, WriteValue::Int(started)),
        (Column::LastEventAt, WriteValue::Int(started)),
    ];
    assert!(
        store
            .cas_state(action_id, State::Running, State::Running, &writes)
            .await
            .expect("set started_at"),
        "set_started_at CAS must find a running action"
    );
}

fn system_now() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs(),
    )
    .expect("fits i64")
}

async fn http_get(addr: SocketAddr) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .expect("write");
    let mut buffer = Vec::new();
    stream.read_to_end(&mut buffer).await.expect("read");
    String::from_utf8_lossy(&buffer).into_owned()
}

#[expect(
    clippy::too_many_lines,
    reason = "one test scripts every metric family and asserts the endpoint exposes all of them; splitting would scatter the completeness proof"
)]
#[tokio::test]
async fn endpoint_exposes_every_family_after_a_scripted_scenario() {
    let store = StoreHandle::open_in_memory().expect("store");
    let seeded = seed_running(
        &store,
        "/tmp/metrics",
        Some("main"),
        Some(&"d".repeat(40)),
        CHANNEL,
        "work",
        "ses_m",
    )
    .await;
    let opencode = FakeOpenCode::new();
    opencode.set_snapshot("ses_m", FakeOpenCode::completed_snapshot());
    let discord = FakeChat::new();
    let ctx = context_with(
        store.clone(),
        Arc::new(opencode),
        Arc::new(FakeGit::inert()),
        Arc::new(FakeClock::at(1_000)),
        None,
        common::DEFAULT_ABORT_TIMEOUT_SECS,
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    );

    // A real completed report, started ~3 s ago, observes the histogram.
    set_started_at(&store, seeded.action_id, system_now() - 3).await;
    let watcher = Watcher::new(ctx.clone(), std::time::Duration::from_millis(10));
    watcher.tick().await.expect("tick");

    let registry = ctx.metrics.clone();
    // Script the remaining families.
    registry.inc_cas_conflict();
    registry.inc_denied();
    registry.set_gate_pending(2);
    registry.inc_gate_request("permission", "bash");
    registry.inc_reconcile_repair("accept");
    registry.set_locks_held(1);
    registry.set_lock_age_seconds(42);
    registry.set_log_lag_seconds(7);
    registry.inc_stream_reconnect("idle");
    registry.set_stream_state(2);
    registry.add_stream_live_ms(1500);
    registry.inc_frame("session.reasoning.delta", true);
    registry.inc_frame("mystery.kind", false);
    registry.inc_frames_decode_failures_by(2);
    registry.inc_frames_dropped();
    registry.inc_probe_failure("watcher");
    registry.set_opencode_up(true);
    // SPEC-004 §5.10 surface families.
    registry.set_surface_up(ocbot_core::Surface::Discord, true);
    registry.inc_surface_send(ocbot_core::Surface::Discord, "post");
    registry.inc_surface_send_failure(ocbot_core::Surface::Discord, "post");
    registry.inc_surface_denied(ocbot_core::Surface::Discord);
    registry.inc_surface_rate_dropped(ocbot_core::Surface::Discord);
    registry.inc_signal_envelope_duplicates();
    registry.set_pending_flows_open("run", 1);
    registry.set_component_map_rows(3);
    // §5.10's queue-depth probe: a broadcast in flight on each surface.
    registry.enter_broadcast(ocbot_core::Surface::Discord);
    registry.enter_broadcast(ocbot_core::Surface::Signal);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = metrics::serve_listener(listener, registry).await;
    });

    let response = http_get(addr).await;
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    for needle in [
        "ocbot_actions_total{state=\"running\"}",
        "ocbot_transition_cas_conflicts_total",
        "ocbot_reports_delivered_total{surface=\"discord\"}",
        "ocbot_gate_requests_total{kind=\"permission\",tool=\"bash\"}",
        "ocbot_gate_pending",
        "ocbot_idle_to_report_seconds_bucket{le=\"5\"}",
        "ocbot_idle_to_report_seconds_count",
        "ocbot_idle_to_report_seconds_sum",
        "ocbot_opencode_up",
        "ocbot_reconcile_repairs_total{kind=\"accept\"}",
        "ocbot_locks_held",
        "ocbot_lock_age_seconds",
        "ocbot_log_lag_seconds",
        "ocbot_stream_reconnects_total",
        "ocbot_stream_reconnects_total{reason=\"idle\"}",
        "ocbot_stream_state",
        "ocbot_stream_live_seconds_total",
        "ocbot_frames_total{kind=\"session.reasoning.delta\",mapped=\"true\"}",
        "ocbot_frames_total{kind=\"mystery.kind\",mapped=\"false\"}",
        "ocbot_frames_decode_failures_total",
        "ocbot_frames_dropped_total",
        "ocbot_probe_failures_total{loop=\"watcher\"}",
        "ocbot_surface_up{surface=\"discord\"} 1",
        "ocbot_surface_send_total{surface=\"discord\",op=\"post\"} 1",
        "ocbot_surface_send_failures_total{surface=\"discord\",op=\"post\"} 1",
        "ocbot_surface_denied_total{surface=\"discord\"} 1",
        "ocbot_surface_rate_dropped_total{surface=\"discord\"} 1",
        "ocbot_signal_envelope_duplicates_total 1",
        "ocbot_pending_flows_open{flow=\"run\"} 1",
        "ocbot_component_map_rows 3",
        "ocbot_surface_broadcast_inflight{surface=\"discord\"} 1",
        "ocbot_surface_broadcast_inflight{surface=\"signal\"} 1",
    ] {
        assert!(response.contains(needle), "missing metric family {needle}");
    }
    // The report was observed exactly once, in the ≤5 s bucket, with a sum.
    assert!(
        response.contains("ocbot_idle_to_report_seconds_count 1"),
        "one observation: {response}"
    );
    assert!(
        response.contains("ocbot_idle_to_report_seconds_bucket{le=\"5\"} 1"),
        "within 5 s: {response}"
    );
    assert!(!response.contains("ocbot_idle_to_report_seconds_sum 0\n"));
}

/// The queue-depth gauge is a RAII count: entering leaves it up, leaving brings
/// it back to zero, and it never wraps below.
#[test]
fn the_broadcast_inflight_gauge_returns_to_zero() {
    let registry = ocbot::app::metrics::Metrics::new();
    registry.enter_broadcast(ocbot_core::Surface::Discord);
    registry.enter_broadcast(ocbot_core::Surface::Discord);
    registry.leave_broadcast(ocbot_core::Surface::Discord);
    let text = registry.render_text();
    assert!(
        text.contains("ocbot_surface_broadcast_inflight{surface=\"discord\"} 1"),
        "{text}"
    );
    registry.leave_broadcast(ocbot_core::Surface::Discord);
    // Over-leaving saturates at zero rather than wrapping to u64::MAX.
    registry.leave_broadcast(ocbot_core::Surface::Discord);
    let text = registry.render_text();
    assert!(
        text.contains("ocbot_surface_broadcast_inflight{surface=\"discord\"} 0"),
        "{text}"
    );
}
