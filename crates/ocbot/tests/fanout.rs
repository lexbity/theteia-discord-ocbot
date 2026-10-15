//! PH-6: `FanoutPort` tests (SPEC-004 FR-110/D-01/D-07): concurrent broadcast,
//! honest per-surface failure attribution, and surface-routed point posts.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping (C6)"
)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use common::FakeChat;
use ocbot::app::context::{BroadcastOutcome, ChatPort, SendError};
use ocbot::app::surfaces::FanoutPort;
use ocbot_core::OutgoingMessage;
use ocbot_core::surface::{ChatRef, Destination, MsgId, PostReceipt, Surface};

/// A recording fake tagged with the surface it serves (the shared `FakeChat`
/// is Discord-shaped; the fan-out routes by `surfaces()`).
struct TaggedChat {
    inner: FakeChat,
    surface: Surface,
    fail: AtomicBool,
}

impl TaggedChat {
    fn up(surface: Surface, announce: Option<u64>) -> Arc<Self> {
        let inner = match announce {
            Some(channel) => FakeChat::new().with_announce(channel),
            None => FakeChat::new(),
        };
        Arc::new(Self {
            inner,
            surface,
            fail: AtomicBool::new(false),
        })
    }

    fn failing(surface: Surface) -> Arc<Self> {
        let port = Self::up(surface, None);
        port.fail.store(true, Ordering::SeqCst);
        port
    }

    fn posts(&self) -> Vec<(u64, String)> {
        self.inner
            .posts()
            .into_iter()
            .map(|posted| (posted.channel, posted.message.content))
            .collect()
    }
}

#[async_trait]
impl ChatPort for TaggedChat {
    async fn post(
        &self,
        destination: &Destination,
        message: &OutgoingMessage,
    ) -> Result<PostReceipt, SendError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(SendError {
                surface: self.surface,
                message: "scripted outage".to_owned(),
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

    async fn broadcast(&self, message: &OutgoingMessage) -> BroadcastOutcome {
        if self.fail.load(Ordering::SeqCst) {
            return BroadcastOutcome {
                delivered: Vec::new(),
                failed: vec![(
                    self.surface,
                    SendError {
                        surface: self.surface,
                        message: "scripted outage".to_owned(),
                    },
                )],
            };
        }
        self.inner.broadcast(message).await
    }

    fn surfaces(&self) -> Vec<Surface> {
        vec![self.surface]
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

fn dest(surface: Surface, chat_ref: &str) -> Destination {
    Destination {
        surface,
        chat_ref: ChatRef(chat_ref.to_owned()),
    }
}

#[tokio::test]
async fn broadcast_reaches_every_surface_through_its_own_announce_path() {
    let discord = TaggedChat::up(Surface::Discord, Some(7));
    let port = FanoutPort::new(vec![
        discord.clone(),
        signal_announcing(Surface::Signal, "+15551234567"),
    ]);
    let outcome = port.broadcast(&plain("ops heartbeat")).await;
    assert_eq!(outcome.delivered.len(), 2, "both surfaces delivered");
    assert!(outcome.failed.is_empty());
    let delivered_surfaces: Vec<Surface> = outcome.delivered.iter().map(|(s, _)| *s).collect();
    assert!(delivered_surfaces.contains(&Surface::Discord));
    assert!(delivered_surfaces.contains(&Surface::Signal));
    assert!(discord.posts()[0].1.contains("ops heartbeat"));
}

/// A signal-shaped fake whose broadcast delivers to the operator chat.
fn signal_announcing(surface: Surface, operator: &str) -> Arc<dyn ChatPort> {
    struct OperatorChat {
        inner: FakeChat,
        operator: String,
        surface: Surface,
    }
    #[async_trait]
    impl ChatPort for OperatorChat {
        async fn post(
            &self,
            destination: &Destination,
            message: &OutgoingMessage,
        ) -> Result<PostReceipt, SendError> {
            self.inner.post(destination, message).await
        }
        async fn edit(&self, receipt: &PostReceipt, replacement: &OutgoingMessage) {
            self.inner.edit(receipt, replacement).await;
        }
        async fn react(&self, receipt: &PostReceipt, emoji: char) {
            self.inner.react(receipt, emoji).await;
        }
        async fn broadcast(&self, message: &OutgoingMessage) -> BroadcastOutcome {
            let receipt = PostReceipt {
                surface: self.surface,
                msg_id: MsgId("1700000000000".to_owned()),
            };
            let _ = self
                .inner
                .post(&dest(self.surface, &self.operator), message)
                .await;
            BroadcastOutcome {
                delivered: vec![(self.surface, receipt)],
                failed: Vec::new(),
            }
        }
        fn surfaces(&self) -> Vec<Surface> {
            vec![self.surface]
        }
    }
    Arc::new(OperatorChat {
        inner: FakeChat::new(),
        operator: operator.to_owned(),
        surface,
    })
}

#[tokio::test]
async fn failed_surfaces_are_named_by_their_own_error() {
    let discord = TaggedChat::up(Surface::Discord, Some(7));
    let signal = TaggedChat::failing(Surface::Signal);
    let port = FanoutPort::new(vec![discord.clone(), signal]);
    let outcome = port.broadcast(&plain("report")).await;
    assert_eq!(outcome.delivered.len(), 1, "the healthy surface delivered");
    assert_eq!(outcome.failed.len(), 1);
    // AC-5: the failure names the stalled surface — never a placeholder.
    assert_eq!(outcome.failed[0].0, Surface::Signal);
    assert_eq!(outcome.failed[0].1.surface, Surface::Signal);
}

#[tokio::test]
async fn point_posts_route_by_surface_to_any_destination() {
    // The action's channel is *not* the announce channel: reply routing must
    // find its surface's port regardless of destination.
    let discord = TaggedChat::up(Surface::Discord, Some(7));
    let port = FanoutPort::new(vec![discord.clone()]);
    let action_channel = dest(Surface::Discord, "999");
    let receipt = port.post(&action_channel, &plain("report")).await.unwrap();
    assert_eq!(receipt.surface, Surface::Discord);
    assert_eq!(discord.posts()[0].0, 999);

    let unknown = port
        .post(&dest(Surface::Signal, "+1555"), &plain("x"))
        .await;
    assert!(unknown.is_err(), "a surface with no port fails loud");
}

#[tokio::test]
async fn edit_and_react_route_by_receipt_surface() {
    let discord = TaggedChat::up(Surface::Discord, None);
    let signal = signal_announcing(Surface::Signal, "+1555");
    let port = FanoutPort::new(vec![discord.clone(), signal]);
    port.edit(
        &PostReceipt {
            surface: Surface::Discord,
            msg_id: MsgId("1".to_owned()),
        },
        &plain("quiet"),
    )
    .await;
    port.react(
        &PostReceipt {
            surface: Surface::Discord,
            msg_id: MsgId("1".to_owned()),
        },
        '✅',
    )
    .await;
    assert_eq!(discord.inner.edits().len(), 1);
    assert_eq!(discord.inner.reactions().len(), 1);
}

#[tokio::test]
async fn surfaces_is_the_union_of_children() {
    let port = FanoutPort::new(vec![
        TaggedChat::up(Surface::Discord, None),
        signal_announcing(Surface::Signal, "+1555"),
    ]);
    let mut surfaces = port.surfaces();
    surfaces.sort_by_key(|surface| surface.as_str().to_owned());
    assert_eq!(surfaces, vec![Surface::Discord, Surface::Signal]);
}

/// Dual-surface mode must run over the registry's *instrumented* ports: the
/// fan-out inherits per-surface counters and health (the PH-6 wiring gap this
/// guards against is "fan-out over raw transports").
#[tokio::test]
async fn dual_mode_keeps_per_surface_instrumentation() {
    use ocbot::app::metrics::Metrics;
    use ocbot::app::surfaces::SurfaceRegistry;

    let metrics = Arc::new(Metrics::new());
    let registry = SurfaceRegistry::build(
        vec![
            (
                Surface::Discord,
                dest(Surface::Discord, "7"),
                TaggedChat::up(Surface::Discord, Some(7)),
            ),
            (
                Surface::Signal,
                dest(Surface::Signal, "+15551234567"),
                signal_announcing(Surface::Signal, "+15551234567"),
            ),
        ],
        &metrics,
    );
    let fanout = FanoutPort::new(
        registry
            .handles()
            .iter()
            .map(|handle| handle.port.clone())
            .collect(),
    );

    fanout.broadcast(&plain("report")).await;
    fanout
        .post(&dest(Surface::Discord, "999"), &plain("reply"))
        .await
        .unwrap();

    let text = metrics.render_text();
    assert!(
        text.contains("ocbot_surface_send_total{surface=\"discord\",op=\"broadcast\"} 1"),
        "the discord child was instrumented through the fan-out:\n{text}"
    );
    assert!(
        text.contains("ocbot_surface_send_total{surface=\"signal\",op=\"broadcast\"} 1"),
        "the signal child was instrumented through the fan-out:\n{text}"
    );
    assert!(
        text.contains("ocbot_surface_send_total{surface=\"discord\",op=\"post\"} 1"),
        "point posts route through instrumentation too:\n{text}"
    );
}
