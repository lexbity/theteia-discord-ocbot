//! SPEC-004 PH-3 coverage lift: the Discord adapter's HTTP half against a
//! wiremock stand-in for the Discord API (serenity's `HttpBuilder::proxy`
//! points every request at the mock). This pins the wire shapes the adapter
//! emits — FR-3's structural `allowed_mentions` restriction, receipt
//! correlation, and the quiet-op degrade paths — without a gateway.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

use std::sync::Arc;

use serenity::all::Http;
use serenity::http::HttpBuilder;
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use ocbot::adapters::discord::DiscordAdapter;
use ocbot::app::context::ChatPort;
use ocbot_core::render::{AttachmentSpec, OutgoingMessage};
use ocbot_core::{Destination, PostReceipt, Surface};

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 123;
const MESSAGE: &str = "456";

/// A minimal but complete `Message` object for serenity's model (required
/// fields only; unknown fields would be ignored anyway).
fn message_json() -> serde_json::Value {
    serde_json::json!({
        "id": MESSAGE,
        "channel_id": CHANNEL.to_string(),
        "author": {"id": "999", "username": "ocbot"},
        "content": "posted",
        "timestamp": "2026-01-01T00:00:00Z",
        "tts": false,
        "mention_everyone": false,
        "mentions": [],
        "mention_roles": [],
        "attachments": [],
        "embeds": [],
        "pinned": false,
        "type": 0
    })
}

/// An `Http` whose requests all land on the mock server.
fn proxied_http(server: &MockServer) -> Arc<Http> {
    Arc::new(
        HttpBuilder::new("dummy-token")
            .proxy(server.uri())
            .ratelimiter_disabled(true)
            .build(),
    )
}

fn adapter(server: &MockServer) -> DiscordAdapter {
    DiscordAdapter::new(proxied_http(server), OPERATOR)
}

fn message(content: &str) -> OutgoingMessage {
    OutgoingMessage {
        content: content.to_owned(),
        attachment: None,
        components: Vec::new(),
        mention_allowlist: Vec::new(),
    }
}

/// A 204 responder (reactions).
struct NoContent;

impl Respond for NoContent {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        ResponseTemplate::new(204)
    }
}

async fn mount_message_route(server: &MockServer, route: &str, respond_method: &str) {
    Mock::given(method(respond_method))
        .and(path_regex(route))
        .respond_with(ResponseTemplate::new(200).set_body_json(message_json()))
        .mount(server)
        .await;
}

async fn posted_body(server: &MockServer) -> serde_json::Value {
    let requests = server.received_requests().await.expect("requests recorded");
    let post = requests
        .iter()
        .find(|request| request.method == "POST")
        .expect("a POST");
    serde_json::from_slice(&post.body).expect("json body")
}

#[tokio::test]
async fn post_sends_the_structural_allowed_mentions_restriction() {
    let server = MockServer::start().await;
    mount_message_route(&server, "/api/v10/channels/123/messages", "POST").await;

    let receipt = adapter(&server)
        .post(&Destination::discord(CHANNEL), &message("hello"))
        .await
        .expect("posted");
    assert_eq!(receipt.surface, Surface::Discord);
    assert_eq!(receipt.msg_id.0, MESSAGE);

    // FR-3 over the wire: parse is empty and only the operator is mentionable.
    let body = posted_body(&server).await;
    assert_eq!(body["content"], "hello");
    assert_eq!(body["allowed_mentions"]["parse"], serde_json::json!([]));
    assert_eq!(
        body["allowed_mentions"]["users"],
        serde_json::json!([OPERATOR.to_string()])
    );
}

#[tokio::test]
async fn post_with_an_attachment_rides_multipart() {
    let server = MockServer::start().await;
    mount_message_route(&server, "/api/v10/channels/123/messages", "POST").await;

    let spec = OutgoingMessage {
        content: "full report attached".to_owned(),
        attachment: Some(AttachmentSpec {
            filename: "report.md".to_owned(),
            bytes: b"# report\n".to_vec(),
        }),
        components: Vec::new(),
        mention_allowlist: Vec::new(),
    };
    adapter(&server)
        .post(&Destination::discord(CHANNEL), &spec)
        .await
        .expect("posted");

    let requests = server.received_requests().await.expect("requests recorded");
    let post = requests
        .iter()
        .find(|request| request.method == "POST")
        .expect("a POST");
    let body = String::from_utf8_lossy(&post.body);
    assert!(
        body.contains("report.md"),
        "multipart names the file: {body}"
    );
    assert!(body.contains("full report attached"), "{body}");
    assert!(body.contains("# report"), "the bytes rode along: {body}");
}

#[tokio::test]
async fn edit_patches_the_channel_the_post_remembered() {
    let server = MockServer::start().await;
    mount_message_route(&server, "/api/v10/channels/123/messages", "POST").await;
    mount_message_route(&server, "/api/v10/channels/123/messages/456", "PATCH").await;

    let chat = adapter(&server);
    let receipt = chat
        .post(&Destination::discord(CHANNEL), &message("first"))
        .await
        .expect("posted");
    chat.edit(&receipt, &message("quiet rewrite")).await;

    let requests = server.received_requests().await.expect("requests recorded");
    let patch = requests
        .iter()
        .find(|request| request.method == "PATCH")
        .expect("a PATCH");
    assert!(
        patch.url.path().ends_with("/channels/123/messages/456"),
        "{}",
        patch.url
    );
    let body: serde_json::Value = serde_json::from_slice(&patch.body).expect("json");
    assert_eq!(body["content"], "quiet rewrite");
}

#[tokio::test]
async fn react_puts_a_reaction_on_the_remembered_message() {
    let server = MockServer::start().await;
    mount_message_route(&server, "/api/v10/channels/123/messages", "POST").await;
    Mock::given(method("PUT"))
        .and(path_regex(
            "/api/v10/channels/123/messages/456/reactions/.*/@me",
        ))
        .respond_with(NoContent)
        .mount(&server)
        .await;

    let chat = adapter(&server);
    let receipt = chat
        .post(&Destination::discord(CHANNEL), &message("first"))
        .await
        .expect("posted");
    chat.react(&receipt, '\u{1F44D}').await;

    let requests = server.received_requests().await.expect("requests recorded");
    assert!(
        requests.iter().any(|request| {
            request.method == "PUT" && request.url.path().contains("/reactions/")
        }),
        "a PUT reaction request landed"
    );
}

#[tokio::test]
async fn quiet_ops_degrade_to_warnings_never_errors() {
    let server = MockServer::start().await; // no routes mounted at all
    let chat = adapter(&server);
    // An unknown receipt: no channel remembered, and the 404s that follow
    // are warnings inside the adapter — the caller never sees them.
    let receipt = PostReceipt {
        surface: Surface::Discord,
        msg_id: ocbot_core::MsgId("999".to_owned()),
    };
    chat.edit(&receipt, &message("ignored")).await;
    chat.react(&receipt, 'x').await;
}

#[tokio::test]
async fn a_non_numeric_snowflake_fails_loud() {
    let server = MockServer::start().await;
    let chat = adapter(&server);
    let error = chat
        .post(
            &Destination {
                surface: Surface::Discord,
                chat_ref: ocbot_core::ChatRef("not-a-snowflake".to_owned()),
            },
            &message("x"),
        )
        .await
        .expect_err("D-02: ids are parsed at the boundary");
    assert!(
        error.message.contains("not a numeric snowflake"),
        "{error:?}"
    );

    // Same parse on the quiet path — it warns, and this pins that it does
    // not panic or propagate.
    chat.edit(
        &PostReceipt {
            surface: Surface::Discord,
            msg_id: ocbot_core::MsgId("also-not-a-snowflake".to_owned()),
        },
        &message("x"),
    )
    .await;
}

#[tokio::test]
async fn broadcast_posts_to_the_announce_channel_or_reports_the_miss() {
    let server = MockServer::start().await;
    mount_message_route(&server, "/api/v10/channels/9/messages", "POST").await;

    let with_ops = adapter(&server).with_ops_channel(9);
    let outcome = with_ops.broadcast(&message("degraded notice")).await;
    assert!(outcome.any_delivered(), "{outcome:?}");
    assert_eq!(outcome.delivered[0].0, Surface::Discord);
    assert!(outcome.failed.is_empty());

    let without_ops = adapter(&server);
    let outcome = without_ops.broadcast(&message("nowhere to go")).await;
    assert!(!outcome.any_delivered());
    assert!(
        outcome.failed[0]
            .1
            .message
            .contains("no ops channel configured"),
        "{outcome:?}"
    );

    assert_eq!(adapter(&server).surfaces(), vec![Surface::Discord]);
}
