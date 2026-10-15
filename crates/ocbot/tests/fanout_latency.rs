//! Broadcast fan-out latency (SPEC-004 NFR-102, PH-6 / AC-5): one surface's
//! slow failure must not delay *delivery* to the others. The healthy
//! surface's post is observed the moment it happens (through the recording
//! fake), not when the merged outcome returns — the fan-out deliberately
//! awaits its slow child so no result is dropped on the floor.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping (C6)"
)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use common::FakeChat;
use ocbot::app::context::{BroadcastOutcome, ChatPort, SendError};
use ocbot::app::surfaces::FanoutPort;
use ocbot_core::OutgoingMessage;
use ocbot_core::surface::{ChatRef, Destination, PostReceipt, Surface};

/// The spec's stall budget for the failing sibling. Shortened from the spec's
/// 5 s to keep the suite fast — the property under test (a slow *failure*
/// not delaying the healthy surface) is identical at any stall above the
/// assertion margin.
const STALL: Duration = Duration::from_millis(1_500);
/// AC-5's generous CI margin for the healthy delivery.
const DELIVERY_BUDGET: Duration = Duration::from_millis(300);

/// Stalls, then fails.
struct StalledFailure;

#[async_trait]
impl ChatPort for StalledFailure {
    async fn post(
        &self,
        destination: &Destination,
        _message: &OutgoingMessage,
    ) -> Result<PostReceipt, SendError> {
        tokio::time::sleep(STALL).await;
        Err(SendError {
            surface: destination.surface,
            message: "slow scripted failure".to_owned(),
        })
    }

    async fn edit(&self, _receipt: &PostReceipt, _replacement: &OutgoingMessage) {}

    async fn react(&self, _receipt: &PostReceipt, _emoji: char) {}

    async fn broadcast(&self, message: &OutgoingMessage) -> BroadcastOutcome {
        let result = self
            .post(
                &Destination {
                    surface: Surface::Signal,
                    chat_ref: ChatRef("+15551234567".to_owned()),
                },
                message,
            )
            .await;
        BroadcastOutcome {
            delivered: Vec::new(),
            failed: vec![(Surface::Signal, result.expect_err("always fails"))],
        }
    }

    fn surfaces(&self) -> Vec<Surface> {
        vec![Surface::Signal]
    }
}

fn report() -> OutgoingMessage {
    OutgoingMessage {
        content: "report".to_owned(),
        attachment: None,
        components: Vec::new(),
        mention_allowlist: Vec::new(),
    }
}

#[tokio::test]
async fn a_slow_failing_surface_does_not_delay_the_healthy_one() {
    let healthy = Arc::new(FakeChat::new().with_announce(7));
    let healthy_port: Arc<dyn ChatPort> = healthy.clone();
    let stalled: Arc<dyn ChatPort> = Arc::new(StalledFailure);
    let port = FanoutPort::new(vec![healthy_port, stalled]);

    let started = Instant::now();
    let broadcast = tokio::spawn(async move { port.broadcast(&report()).await });

    // The healthy delivery lands on the fake long before the stall ends.
    while healthy.posts().is_empty() {
        assert!(
            started.elapsed() < DELIVERY_BUDGET,
            "the healthy surface's delivery took {:?} — the stalled sibling delayed it",
            started.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let delivery_latency = started.elapsed();
    assert!(delivery_latency < DELIVERY_BUDGET);

    // The merged outcome waits for the slow child and names it honestly.
    let outcome = broadcast.await.unwrap();
    assert!(
        started.elapsed() >= STALL,
        "the fan-out awaited its slow child instead of abandoning it"
    );
    assert_eq!(outcome.delivered.len(), 1);
    assert_eq!(outcome.delivered[0].0, Surface::Discord);
    assert_eq!(outcome.failed.len(), 1);
    assert_eq!(outcome.failed[0].0, Surface::Signal);
}
