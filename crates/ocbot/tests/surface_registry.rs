//! Surface selection and registry tests (SPEC-004 PH-2): config → surface
//! set, the instrumented-port metric families, and the registry collapse.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping (C6)"
)]

mod common;

use std::sync::Arc;

use common::FakeChat;
use ocbot::app::context::ChatPort;
use ocbot::app::metrics::Metrics;
use ocbot::app::surfaces::{SURFACE_DOWN_FAILURES, SurfaceRegistry, SurfaceSet};
use ocbot::config::Config;
use ocbot_core::OutgoingMessage;
use ocbot_core::surface::{ChatRef, Destination, MsgId, PostReceipt, Surface};

const BASE: &str = "opencode_url = \"http://127.0.0.1:8080\"\n";

fn config(extra: &str) -> Config {
    Config::parse(&format!("{BASE}{extra}")).expect("config parses")
}

fn discord_dest(channel: u64) -> Destination {
    Destination {
        surface: Surface::Discord,
        chat_ref: ChatRef(channel.to_string()),
    }
}

fn plain_message(text: &str) -> OutgoingMessage {
    OutgoingMessage {
        content: text.to_owned(),
        attachment: None,
        components: Vec::new(),
        mention_allowlist: Vec::new(),
    }
}

#[test]
fn single_selection_resolves_to_one_surface() {
    let set = SurfaceSet::from_config(&config("[discord]\nallowed_user_id = 42\n")).unwrap();
    assert_eq!(set.len(), 1);
    assert!(set.contains(Surface::Discord));
    assert!(!set.contains(Surface::Signal));
    assert_eq!(
        set.iter().collect::<Vec<_>>(),
        vec![Surface::Discord],
        "omitted channel defaults to [\"discord\"]"
    );
}

#[test]
fn list_selection_preserves_order_and_members() {
    let set = SurfaceSet::from_config(&config(
        "channel = [\"discord\", \"signal\"]\n[discord]\nallowed_user_id = 42\n",
    ))
    .unwrap();
    assert_eq!(
        set.iter().collect::<Vec<_>>(),
        vec![Surface::Discord, Surface::Signal]
    );
}

#[test]
fn duplicate_selection_is_an_error() {
    let errors = SurfaceSet::from_config(&config(
        "channel = [\"discord\", \"discord\"]\n[discord]\nallowed_user_id = 42\n",
    ))
    .expect_err("one entry per surface");
    assert!(
        errors.iter().any(|error| error.contains("twice")),
        "{errors:?}"
    );
}

#[test]
fn unknown_surface_is_an_error() {
    let errors = SurfaceSet::from_config(&config(
        "channel = \"telegram\"\n[discord]\nallowed_user_id = 42\n",
    ))
    .expect_err("unknown surface");
    assert!(
        errors.iter().any(|error| error.contains("unknown surface")),
        "{errors:?}"
    );
}

#[tokio::test]
async fn instrumented_round_trip_counts_every_op() {
    let metrics = Arc::new(Metrics::new());
    let port = SurfaceRegistry::build(
        vec![(
            Surface::Discord,
            discord_dest(7),
            Arc::new(FakeChat::new().with_announce(7)),
        )],
        &metrics,
    )
    .single();

    let receipt = port
        .post(&discord_dest(7), &plain_message("hi"))
        .await
        .expect("post");
    assert_eq!(
        receipt,
        PostReceipt {
            surface: Surface::Discord,
            msg_id: MsgId("0".to_owned()),
        }
    );
    port.edit(&receipt, &plain_message("edit")).await;
    port.react(&receipt, '✅').await;
    let outcome = port.broadcast(&plain_message("ops")).await;
    assert_eq!(outcome.delivered.len(), 1, "the single surface delivered");

    let text = metrics.render_text();
    for needle in [
        "ocbot_surface_up{surface=\"discord\"} 1",
        "ocbot_surface_send_total{surface=\"discord\",op=\"post\"} 1",
        "ocbot_surface_send_total{surface=\"discord\",op=\"edit\"} 1",
        "ocbot_surface_send_total{surface=\"discord\",op=\"react\"} 1",
        "ocbot_surface_send_total{surface=\"discord\",op=\"broadcast\"} 1",
    ] {
        assert!(text.contains(needle), "missing {needle} in:\n{text}");
    }
    assert!(
        !text.contains("ocbot_surface_send_failures_total{surface=\"discord\""),
        "no failures happened"
    );
}

/// A chat port whose posts fail while armed and succeed once healed; the
/// down/up edge tests script it.
struct FlakyChat {
    inner: FakeChat,
    fail: std::sync::atomic::AtomicBool,
}

impl FlakyChat {
    fn new() -> Self {
        Self {
            inner: FakeChat::new(),
            fail: std::sync::atomic::AtomicBool::new(true),
        }
    }

    fn heal(&self) {
        self.fail.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ChatPort for FlakyChat {
    async fn post(
        &self,
        destination: &Destination,
        message: &OutgoingMessage,
    ) -> Result<PostReceipt, ocbot::app::context::SendError> {
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ocbot::app::context::SendError {
                surface: destination.surface,
                message: "scripted failure".to_owned(),
            });
        }
        self.inner.post(destination, message).await
    }

    async fn edit(&self, receipt: &PostReceipt, replacement: &OutgoingMessage) {
        self.inner.edit(receipt, replacement).await;
    }

    async fn react(&self, receipt: &PostReceipt, emoji: char) {
        self.inner.react(receipt, emoji).await;
    }

    async fn broadcast(&self, message: &OutgoingMessage) -> ocbot::app::context::BroadcastOutcome {
        self.inner.broadcast(message).await
    }

    fn surfaces(&self) -> Vec<Surface> {
        self.inner.surfaces()
    }
}

#[tokio::test]
async fn failures_flip_the_gauge_only_at_the_threshold() {
    let metrics = Arc::new(Metrics::new());
    let flaky = Arc::new(FlakyChat::new());
    let port = SurfaceRegistry::build(
        vec![(Surface::Discord, discord_dest(7), flaky.clone())],
        &metrics,
    )
    .single();

    // N-1 failures: counted, but the surface is still up.
    for _ in 0..SURFACE_DOWN_FAILURES - 1 {
        assert!(
            port.post(&discord_dest(7), &plain_message("x"))
                .await
                .is_err()
        );
    }
    let text = metrics.render_text();
    assert!(
        text.contains("ocbot_surface_up{surface=\"discord\"} 1"),
        "below the threshold the surface is still up:\n{text}"
    );
    assert!(text.contains(&format!(
        "ocbot_surface_send_failures_total{{surface=\"discord\",op=\"post\"}} {}",
        SURFACE_DOWN_FAILURES - 1
    )));

    // The N-th failure flips the gauge.
    assert!(
        port.post(&discord_dest(7), &plain_message("x"))
            .await
            .is_err()
    );
    assert!(
        metrics
            .render_text()
            .contains("ocbot_surface_up{surface=\"discord\"} 0"),
        "the threshold crossing declares the surface down"
    );

    // Recovery: the first success flips it back.
    flaky.heal();
    assert!(
        port.post(&discord_dest(7), &plain_message("y"))
            .await
            .is_ok()
    );
    assert!(
        metrics
            .render_text()
            .contains("ocbot_surface_up{surface=\"discord\"} 1"),
        "the first success recovers the surface"
    );
}

#[test]
fn registry_exposes_its_handles_for_ops_display() {
    let metrics = Arc::new(Metrics::new());
    let registry = SurfaceRegistry::build(
        vec![(Surface::Discord, discord_dest(7), Arc::new(FakeChat::new()))],
        &metrics,
    );
    assert_eq!(registry.handles().len(), 1);
    assert_eq!(registry.handles()[0].surface, Surface::Discord);
    assert_eq!(registry.single().surfaces(), vec![Surface::Discord]);
}
