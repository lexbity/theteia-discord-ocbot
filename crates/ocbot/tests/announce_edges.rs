//! Edge-triggered cross-surface announces (SPEC-004 D-07/FR-110, PH-2): a
//! failing surface posts exactly one down notice on its sibling and exactly
//! one recovery notice — never one per failure.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping (C6)"
)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use async_trait::async_trait;
use common::FakeChat;
use ocbot::app::context::{BroadcastOutcome, ChatPort, SendError};
use ocbot::app::metrics::Metrics;
use ocbot::app::surfaces::{SURFACE_DOWN_FAILURES, SurfaceRegistry};
use ocbot_core::OutgoingMessage;
use ocbot_core::surface::{ChatRef, Destination, MsgId, PostReceipt, Surface};

/// The announce destination for one surface in this suite.
fn dest(surface: Surface, chat_ref: &str) -> Destination {
    Destination {
        surface,
        chat_ref: ChatRef(chat_ref.to_owned()),
    }
}

fn plain(text: &str) -> OutgoingMessage {
    OutgoingMessage {
        content: text.to_owned(),
        attachment: None,
        components: Vec::new(),
        mention_allowlist: Vec::new(),
    }
}

/// Fails every post until healed; records nothing itself.
struct FlakyChat {
    fail: AtomicBool,
}

impl FlakyChat {
    fn new() -> Self {
        Self {
            fail: AtomicBool::new(true),
        }
    }

    fn heal(&self) {
        self.fail.store(false, Ordering::SeqCst);
    }

    fn break_(&self) {
        self.fail.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl ChatPort for FlakyChat {
    async fn post(
        &self,
        destination: &Destination,
        _message: &OutgoingMessage,
    ) -> Result<PostReceipt, SendError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(SendError {
                surface: destination.surface,
                message: "scripted outage".to_owned(),
            });
        }
        Ok(PostReceipt {
            surface: destination.surface,
            msg_id: MsgId("recovered".to_owned()),
        })
    }

    async fn edit(&self, _receipt: &PostReceipt, _replacement: &OutgoingMessage) {}

    async fn react(&self, _receipt: &PostReceipt, _emoji: char) {}

    async fn broadcast(&self, _message: &OutgoingMessage) -> BroadcastOutcome {
        BroadcastOutcome::default()
    }

    fn surfaces(&self) -> Vec<Surface> {
        vec![Surface::Discord]
    }
}

/// Two surfaces through one registry: a healthy sibling (a recording fake on
/// "discord") and a scripted flaky surface. The sibling counts notices.
struct NoticeCountingChat {
    inner: FakeChat,
    notices: AtomicU64,
}

impl NoticeCountingChat {
    fn new() -> Self {
        Self {
            inner: FakeChat::new(),
            notices: AtomicU64::new(0),
        }
    }

    fn notices(&self) -> u64 {
        self.notices.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ChatPort for NoticeCountingChat {
    async fn post(
        &self,
        destination: &Destination,
        message: &OutgoingMessage,
    ) -> Result<PostReceipt, SendError> {
        if message.content.contains("surface is down") || message.content.contains("back up") {
            self.notices.fetch_add(1, Ordering::SeqCst);
        }
        self.inner.post(destination, message).await
    }

    async fn edit(&self, receipt: &PostReceipt, replacement: &OutgoingMessage) {
        self.inner.edit(receipt, replacement).await;
    }

    async fn react(&self, receipt: &PostReceipt, emoji: char) {
        self.inner.react(receipt, emoji).await;
    }

    async fn broadcast(&self, message: &OutgoingMessage) -> BroadcastOutcome {
        self.inner.broadcast(message).await
    }

    fn surfaces(&self) -> Vec<Surface> {
        self.inner.surfaces()
    }
}

fn two_surface_registry(
    metrics: &Arc<Metrics>,
    sibling: Arc<NoticeCountingChat>,
    flaky: Arc<FlakyChat>,
) -> (Arc<dyn ChatPort>, Arc<dyn ChatPort>) {
    let registry = SurfaceRegistry::build(
        vec![
            (Surface::Discord, dest(Surface::Discord, "1"), sibling),
            (
                Surface::Signal,
                dest(Surface::Signal, "+15551234567"),
                flaky,
            ),
        ],
        metrics,
    );
    let handles = registry.handles();
    (handles[0].port.clone(), handles[1].port.clone())
}

#[tokio::test]
async fn one_down_notice_per_outage_and_one_recovery_notice() {
    let metrics = Arc::new(Metrics::new());
    let sibling = Arc::new(NoticeCountingChat::new());
    let flaky = Arc::new(FlakyChat::new());
    let (_, signal_port) = two_surface_registry(&metrics, sibling.clone(), flaky.clone());
    let signal_dest = Destination {
        surface: Surface::Signal,
        chat_ref: ChatRef("+15551234567".to_owned()),
    };

    // Fail well past the threshold: the notice lands exactly once.
    for _ in 0..SURFACE_DOWN_FAILURES + 5 {
        assert!(signal_port.post(&signal_dest, &plain("x")).await.is_err());
    }
    assert_eq!(sibling.notices(), 1, "one down notice per outage");
    assert!(
        metrics
            .render_text()
            .contains("ocbot_surface_up{surface=\"signal\"} 0"),
        "the gauge flipped"
    );

    // Further failures of the same outage stay silent.
    for _ in 0..3 {
        assert!(signal_port.post(&signal_dest, &plain("x")).await.is_err());
    }
    assert_eq!(sibling.notices(), 1, "still exactly one down notice");

    // The first success announces the recovery — exactly once.
    flaky.heal();
    assert!(signal_port.post(&signal_dest, &plain("y")).await.is_ok());
    assert_eq!(sibling.notices(), 2, "one recovery notice");
    assert!(
        metrics
            .render_text()
            .contains("ocbot_surface_up{surface=\"signal\"} 1"),
        "the gauge recovered"
    );

    // Steady success never announces again.
    for _ in 0..3 {
        assert!(signal_port.post(&signal_dest, &plain("z")).await.is_ok());
    }
    assert_eq!(sibling.notices(), 2, "no notices for steady state");

    // A fresh outage after recovery is a new edge and announces again.
    flaky.break_();
    for _ in 0..SURFACE_DOWN_FAILURES {
        assert!(signal_port.post(&signal_dest, &plain("x")).await.is_err());
    }
    assert_eq!(sibling.notices(), 3, "the second outage announces once");
}
