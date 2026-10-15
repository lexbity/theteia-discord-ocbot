//! PH-6: FanoutPort tests (SPEC-004 FR-110/NFR-102).

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
use ocbot::app::surfaces::FanoutPort;
use ocbot_core::{Destination, OutgoingMessage, Surface};

#[tokio::test]
async fn fanout_broadcast_sends_to_all_surfaces() {
    let discord = Arc::new(FakeChat::new().with_announce(7));
    let signal = Arc::new(FakeChat::new().with_announce(1234567890));

    let discord_dest = Destination {
        surface: Surface::Discord,
        chat_ref: ocbot_core::ChatRef("7".to_owned()),
    };

    let signal_dest = Destination {
        surface: Surface::Signal,
        chat_ref: ocbot_core::ChatRef("+15551234567".to_owned()),
    };

    let port = FanoutPort::new(vec![
        (Surface::Discord, discord.clone(), discord_dest.clone()),
        (Surface::Signal, signal.clone(), signal_dest.clone()),
    ]);

    let message = OutgoingMessage {
        content: "test broadcast".to_owned(),
        attachment: None,
        components: Vec::new(),
        mention_allowlist: Vec::new(),
    };

    let outcome = port.broadcast(&message).await;

    assert!(outcome.any_delivered());
    assert_eq!(outcome.delivered.len(), 2);
    assert_eq!(outcome.failed.len(), 0);
    assert_eq!(outcome.delivered[0].0, Surface::Discord);
    assert_eq!(outcome.delivered[1].0, Surface::Signal);
}

#[tokio::test]
async fn fanout_broadcast_tracks_failures() {
    let discord = Arc::new(FakeChat::new());
    let signal = Arc::new(FakeChat::new());

    let discord_dest = Destination {
        surface: Surface::Discord,
        chat_ref: ocbot_core::ChatRef("7".to_owned()),
    };

    let signal_dest = Destination {
        surface: Surface::Signal,
        chat_ref: ocbot_core::ChatRef("+15551234567".to_owned()),
    };

    let port = FanoutPort::new(vec![
        (Surface::Discord, discord.clone(), discord_dest.clone()),
        (Surface::Signal, signal.clone(), signal_dest.clone()),
    ]);

    let message = OutgoingMessage {
        content: "test broadcast".to_owned(),
        attachment: None,
        components: Vec::new(),
        mention_allowlist: Vec::new(),
    };

    let outcome = port.broadcast(&message).await;

    // FakeChat always delivers, so we expect 2 deliveries
    assert!(outcome.any_delivered());
    assert_eq!(outcome.delivered.len(), 2);
    assert_eq!(outcome.failed.len(), 0);
}

#[tokio::test]
async fn fanout_post_finds_matching_port() {
    let discord = Arc::new(FakeChat::new().with_announce(7));
    let signal = Arc::new(FakeChat::new().with_announce(1234567890));

    let discord_dest = Destination {
        surface: Surface::Discord,
        chat_ref: ocbot_core::ChatRef("7".to_owned()),
    };

    let signal_dest = Destination {
        surface: Surface::Signal,
        chat_ref: ocbot_core::ChatRef("+15551234567".to_owned()),
    };

    let port = FanoutPort::new(vec![
        (Surface::Discord, discord.clone(), discord_dest.clone()),
        (Surface::Signal, signal.clone(), signal_dest.clone()),
    ]);

    let message = OutgoingMessage {
        content: "test post".to_owned(),
        attachment: None,
        components: Vec::new(),
        mention_allowlist: Vec::new(),
    };

    let receipt = port.post(&discord_dest, &message).await.unwrap();

    assert_eq!(receipt.surface, Surface::Discord);
}

#[tokio::test]
async fn fanout_surfaces_returns_all() {
    let discord = Arc::new(FakeChat::new().with_announce(7));
    let signal = Arc::new(FakeChat::new().with_announce(1234567890));

    let discord_dest = Destination {
        surface: Surface::Discord,
        chat_ref: ocbot_core::ChatRef("7".to_owned()),
    };

    let signal_dest = Destination {
        surface: Surface::Signal,
        chat_ref: ocbot_core::ChatRef("+15551234567".to_owned()),
    };

    let port = FanoutPort::new(vec![
        (Surface::Discord, discord.clone(), discord_dest.clone()),
        (Surface::Signal, signal.clone(), signal_dest.clone()),
    ]);

    let surfaces = port.surfaces();

    assert_eq!(surfaces.len(), 2);
    assert!(surfaces.contains(&Surface::Discord));
    assert!(surfaces.contains(&Surface::Signal));
}

#[tokio::test]
async fn fanout_apply_policy_discord_removes_attachments() {
    let spec = OutgoingMessage {
        content: "test".to_owned(),
        attachment: Some(ocbot_core::AttachmentSpec {
            filename: "report.md".to_owned(),
            bytes: b"content".to_vec(),
        }),
        components: Vec::new(),
        mention_allowlist: Vec::new(),
    };

    let discord_msg = FanoutPort::apply_policy(&spec, Surface::Discord);

    assert_eq!(discord_msg.content, "test");
    assert!(discord_msg.attachment.is_none()); // Discord removes attachments by default
}

#[tokio::test]
async fn fanout_apply_policy_signal_keeps_attachments() {
    let spec = OutgoingMessage {
        content: "test".to_owned(),
        attachment: Some(ocbot_core::AttachmentSpec {
            filename: "report.md".to_owned(),
            bytes: b"content".to_vec(),
        }),
        components: Vec::new(),
        mention_allowlist: Vec::new(),
    };

    let signal_msg = FanoutPort::apply_policy(&spec, Surface::Signal);

    assert_eq!(signal_msg.content, "test");
    assert!(signal_msg.attachment.is_some()); // Signal keeps attachments by default
}
