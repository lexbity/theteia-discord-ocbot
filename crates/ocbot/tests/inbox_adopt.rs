//! AC-11 (inbox): an external TUI completion posts once (deduped by
//! `(session_id, seq)`), and replying to that post adopts the session — the
//! adopted action runs in the same session and completes through the normal
//! watcher path.

// Integration tests assert via unwrap/panic; production code remains strict
// (workspace lint floor + G3). See common/mod.rs for the shared harness.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use async_trait::async_trait;

use std::sync::Arc;
use std::time::Duration;

use common::{FakeChat, FakeGit, FakeOpenCode, context};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::completion::DetectedBy;
use ocbot::app::handlers::adopt;
use ocbot::app::inbox;
use ocbot::app::watcher::Watcher;
use ocbot_core::State;
use ocbot_core::outcome::{AssistantSnapshot, TokenCounts};
use ocbot_core::ports::{Activity, PortSession};

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;
const EXT_DIR: &str = "/tmp/ext";
const EXT_SESSION: &str = "ses_ext";

fn ext_snapshot() -> AssistantSnapshot {
    AssistantSnapshot {
        id: Some("msg_ext".to_owned()),
        text: "did the thing in the TUI".to_owned(),
        has_tool_calls: false,
        error: None,
        finish: Some("stop".to_owned()),
        cost: None,
        tokens: Some(TokenCounts {
            input: 1.0,
            output: 1.0,
            reasoning: 0.0,
        }),
    }
}

fn ctx_for(
    store: &StoreHandle,
    opencode: Arc<dyn ocbot_core::ports::OpenCodePort>,
    discord: &FakeChat,
) -> Arc<ocbot::app::context::AppContext> {
    context(
        store.clone(),
        opencode,
        Arc::new(FakeGit::inert()),
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    )
}

fn ext_session() -> PortSession {
    PortSession {
        id: EXT_SESSION.to_owned(),
        parent_id: None,
        title: None,
        directory: Some(EXT_DIR.to_owned()),
    }
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "the DetectedBy threading (SPEC-003 W4) pushed the tick calls past the cap; splitting the assert chain would hide the scenario"
)]
async fn external_completion_posts_once_and_a_reply_adopts() {
    let store = StoreHandle::open_in_memory().expect("store");
    let opencode = FakeOpenCode::new();
    opencode.set_sessions(vec![ext_session()]);
    opencode.set_activity(EXT_SESSION, Activity::Idle);
    opencode.set_snapshot(EXT_SESSION, ext_snapshot());
    let discord = FakeChat::new().with_announce(CHANNEL);
    let ctx = ctx_for(&store, Arc::new(opencode.clone()), &discord);

    // First poll: the external completion lands in the inbox.
    inbox::tick(&ctx, DetectedBy::Fallback)
        .await
        .expect("inbox tick");
    let posts = discord.posts();
    let inbox_posts: Vec<_> = posts
        .iter()
        .filter(|post| {
            post.message
                .content
                .contains("external — reply to continue")
        })
        .collect();
    assert_eq!(inbox_posts.len(), 1, "one inbox post for the completion");
    assert!(
        inbox_posts[0]
            .message
            .content
            .contains("did the thing in the TUI")
    );

    // The dedupe key and the reply map are recorded.
    let msg_id = inbox_posts[0].id;
    assert!(
        store
            .lookup_msg_map("discord", &msg_id.to_string())
            .await
            .expect("msg_map")
            .is_some()
    );

    // A second poll must not duplicate (the (session, seq) key).
    inbox::tick(&ctx, DetectedBy::Fallback)
        .await
        .expect("inbox tick again");
    assert_eq!(
        discord
            .posts()
            .iter()
            .filter(|post| post
                .message
                .content
                .contains("external — reply to continue"))
            .count(),
        1,
        "duplicate completion events post zero extra messages"
    );

    // Replying to the inbox post adopts the session (AC-11).
    adopt::adopt(
        &ctx,
        EXT_SESSION,
        "keep going",
        &ocbot_core::Destination::discord(CHANNEL),
        &format!("discord:{OPERATOR}"),
    )
    .await
    .expect("adopt");
    let adopted = store.running_actions().await.expect("running");
    assert_eq!(adopted.len(), 1);
    let run = store
        .run(adopted[0].run_id)
        .await
        .expect("run")
        .expect("row");
    assert_eq!(run.kind, "adopted");
    assert_eq!(
        adopted[0].session_id.as_deref(),
        Some(EXT_SESSION),
        "same session"
    );
    assert_eq!(
        store
            .action(adopted[0].id)
            .await
            .expect("read")
            .expect("row")
            .prompt_sent,
        "keep going"
    );
    assert!(
        opencode
            .inner()
            .calls
            .iter()
            .any(|call| matches!(call, common::Call::SendPrompt(session, text) if session == EXT_SESSION && text == "keep going")),
        "the adopted prompt is admitted to the external session"
    );

    // The adopted action completes through the normal watcher path.
    opencode.set_snapshot(
        EXT_SESSION,
        AssistantSnapshot {
            id: Some("msg_adopted".to_owned()),
            ..ext_snapshot()
        },
    );
    opencode.set_activity(EXT_SESSION, Activity::Idle);
    Watcher::new(ctx.clone(), Duration::from_millis(10))
        .tick()
        .await
        .expect("tick");
    assert_eq!(
        store
            .action(adopted[0].id)
            .await
            .expect("read")
            .expect("row")
            .state,
        State::Reported
    );
}

#[tokio::test]
async fn a_muted_directory_is_silent() {
    let store = StoreHandle::open_in_memory().expect("store");
    assert!(store.toggle_mute("dir", EXT_DIR).await.expect("mute"));
    let opencode = FakeOpenCode::new();
    opencode.set_sessions(vec![ext_session()]);
    opencode.set_activity(EXT_SESSION, Activity::Idle);
    opencode.set_snapshot(EXT_SESSION, ext_snapshot());
    let discord = FakeChat::new().with_announce(CHANNEL);
    let ctx = ctx_for(&store, Arc::new(opencode), &discord);

    inbox::tick(&ctx, DetectedBy::Fallback)
        .await
        .expect("inbox tick muted");
    assert!(
        discord
            .posts()
            .iter()
            .all(|post| !post.message.content.contains("external")),
        "muted directories produce no inbox posts (FR-33)"
    );
}

/// PH-6 (D-07/§5.4): a dual-surface completion broadcast inserts one
/// `msg_map` row per delivered receipt, and the joined receipts string lands
/// in `notified` — adoption works from whichever surface carried the report.
#[expect(
    clippy::too_many_lines,
    reason = "one test wires the dual-surface fan-out and asserts every receipt mapping; splitting would scatter the proof"
)]
#[tokio::test]
async fn dual_surface_completion_maps_every_receipt() {
    use ocbot::app::surfaces::{FanoutPort, SurfaceRegistry};
    use ocbot_core::surface::{ChatRef, Destination, MsgId, PostReceipt, Surface};

    /// A signal-shaped fake that records posts and answers broadcasts.
    struct SignalChat {
        inner: FakeChat,
    }
    #[async_trait]
    impl ocbot::app::context::ChatPort for SignalChat {
        async fn post(
            &self,
            destination: &Destination,
            message: &ocbot_core::OutgoingMessage,
        ) -> Result<PostReceipt, ocbot::app::context::SendError> {
            self.inner.post(destination, message).await.unwrap();
            // The daemon's timestamps are the surface's message ids.
            Ok(PostReceipt {
                surface: Surface::Signal,
                msg_id: MsgId("1700000000000".to_owned()),
            })
        }
        async fn edit(&self, receipt: &PostReceipt, replacement: &ocbot_core::OutgoingMessage) {
            self.inner.edit(receipt, replacement).await;
        }
        async fn react(&self, receipt: &PostReceipt, emoji: char) {
            self.inner.react(receipt, emoji).await;
        }
        async fn broadcast(
            &self,
            message: &ocbot_core::OutgoingMessage,
        ) -> ocbot::app::context::BroadcastOutcome {
            let receipt = self
                .post(
                    &Destination {
                        surface: Surface::Signal,
                        chat_ref: ChatRef(OPERATOR.to_string()),
                    },
                    message,
                )
                .await
                .unwrap();
            ocbot::app::context::BroadcastOutcome {
                delivered: vec![(Surface::Signal, receipt)],
                failed: Vec::new(),
            }
        }
        fn surfaces(&self) -> Vec<Surface> {
            vec![Surface::Signal]
        }
    }

    let store = StoreHandle::open_in_memory().expect("store");
    let opencode = FakeOpenCode::new();
    opencode.set_sessions(vec![ext_session()]);
    opencode.set_activity(EXT_SESSION, Activity::Idle);
    opencode.set_snapshot(EXT_SESSION, ext_snapshot());
    let discord = FakeChat::new().with_announce(CHANNEL);
    let signal = Arc::new(SignalChat {
        inner: FakeChat::new(),
    });

    let metrics = Arc::new(ocbot::app::metrics::Metrics::new());
    let registry = SurfaceRegistry::build(
        vec![
            (
                Surface::Discord,
                Destination::discord(CHANNEL),
                Arc::new(discord.clone()),
            ),
            (
                Surface::Signal,
                Destination {
                    surface: Surface::Signal,
                    chat_ref: ChatRef("+15551234567".to_owned()),
                },
                signal.clone(),
            ),
        ],
        &metrics,
    );
    let chat: Arc<dyn ocbot::app::context::ChatPort> = Arc::new(FanoutPort::new(
        registry
            .handles()
            .iter()
            .map(|handle| handle.port.clone())
            .collect(),
    ));
    let ctx = context(
        store.clone(),
        Arc::new(opencode.clone()),
        Arc::new(FakeGit::inert()),
        chat,
        OPERATOR,
        Some(CHANNEL),
    );

    inbox::tick(&ctx, DetectedBy::Fallback)
        .await
        .expect("inbox tick");

    // Both surfaces carried the completion; each receipt is its own map row.
    assert!(
        store
            .lookup_msg_map("discord", &discord.posts()[0].id.to_string())
            .await
            .expect("discord msg_map")
            .is_some()
    );
    assert!(
        store
            .lookup_msg_map("signal", "1700000000000")
            .await
            .expect("signal msg_map")
            .is_some()
    );
    // The dual broadcast claimed the completion exactly once: a second
    // claim under the same (session, seq) key is refused — the dedupe held
    // across both deliveries (FR-32).
    let claimed = store
        .claim_notified(EXT_SESSION, "msg_ext")
        .await
        .expect("claim probe");
    assert!(!claimed, "the dual broadcast claimed the completion once");
}
