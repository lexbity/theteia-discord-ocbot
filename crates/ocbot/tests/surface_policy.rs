//! Per-surface report-policy tests (SPEC-004 D-12/FR-102/FR-107, PH-6): the
//! full/excerpt/off × attachments matrix, applied by the delivery path
//! (`PolicyPort`), from config — handlers stay policy-free.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping (C6)"
)]

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use common::FakeChat;
use ocbot::app::context::{BroadcastOutcome, ChatPort, SendError};
use ocbot::app::surfaces::PolicyPort;
use ocbot::config::Config;
use ocbot_core::surface::{ChatRef, Destination, MsgId, PostReceipt, Surface};
use ocbot_core::{OutgoingMessage, ReportExcerpt, SurfacePolicy, apply_surface_policy};

const REPORT: &str =
    "✅ completed **fix the bug**\n```\nthe long body\nmore body\n```\nΔ +12 −3 · 4.2s";

fn report_spec() -> OutgoingMessage {
    OutgoingMessage {
        content: REPORT.to_owned(),
        attachment: Some(ocbot_core::AttachmentSpec {
            filename: "report.md".to_owned(),
            bytes: b"the long body\nmore body\n".to_vec(),
        }),
        components: Vec::new(),
        mention_allowlist: vec![42],
    }
}

fn policy(excerpt: ReportExcerpt, attachments: bool) -> SurfacePolicy {
    SurfacePolicy {
        excerpt,
        attachments,
    }
}

// --- the pure function (FR-111's round-trip companion) -----------------------

#[test]
fn pure_matrix_full_excerpt_off_by_attachments() {
    for excerpt in [
        ReportExcerpt::Full,
        ReportExcerpt::Excerpt,
        ReportExcerpt::Off,
    ] {
        for attachments in [true, false] {
            let out = apply_surface_policy(&report_spec(), &policy(excerpt, attachments));
            assert_eq!(
                out.attachment.is_some(),
                attachments,
                "{excerpt:?}/{attachments}"
            );
            if excerpt == ReportExcerpt::Off {
                assert!(!out.content.contains("the long body"), "{excerpt:?}");
                assert!(out.content.contains("✅ completed"), "headline kept");
                assert!(out.content.contains("Δ +12 −3"), "verdict kept");
            } else {
                assert!(out.content.contains("the long body"), "{excerpt:?}");
            }
            if !attachments && excerpt != ReportExcerpt::Off {
                assert!(
                    out.content.contains(ocbot_core::SUPPRESSED_ATTACHMENT_HINT),
                    "the hint names where the full report lives ({excerpt:?})"
                );
            }
            assert_eq!(out.mention_allowlist, vec![42], "FR-3 passes through");
        }
    }
}

// --- the defaults resolve from config (D-09/D-12) ----------------------------

#[test]
fn discord_defaults_excerpt_and_no_attachments_signal_defaults_full_and_yes() {
    let config = Config::parse(
        "opencode_url = \"http://127.0.0.1:8080\"\nprojects_root = \"/tmp\"\n[discord]\nallowed_user_id = 42\n[signal]\nsocket = \"/run/signal-cli/socket\"\noperator_number = \"+15551234567\"\n",
    )
    .unwrap();
    assert_eq!(
        config.discord.policy(),
        policy(ReportExcerpt::Excerpt, false)
    );
    assert_eq!(config.signal.policy(), policy(ReportExcerpt::Full, true));
}

#[test]
fn the_discord_opt_back_in_restores_attachments() {
    let config = Config::parse(
        "opencode_url = \"http://127.0.0.1:8080\"\nprojects_root = \"/tmp\"\n[discord]\nallowed_user_id = 42\nreport_attachments = true\n",
    )
    .unwrap();
    assert_eq!(
        config.discord.policy(),
        policy(ReportExcerpt::Excerpt, true),
        "one config line restores today's pre-SPEC-004 behavior exactly"
    );
}

// --- the delivery path enforces it (PolicyPort, FR-107) ----------------------

#[tokio::test]
async fn the_delivery_path_enforces_the_policy() {
    let fake = Arc::new(FakeChat::new().with_announce(7));
    let port = PolicyPort::new(policy(ReportExcerpt::Excerpt, false), fake.clone());
    let destination = Destination {
        surface: Surface::Discord,
        chat_ref: ChatRef("7".to_owned()),
    };

    // A point post is transformed before the transport sees it.
    port.post(&destination, &report_spec()).await.unwrap();
    let posted = fake.posts().remove(0);
    assert!(posted.message.attachment.is_none());
    assert!(
        posted
            .message
            .content
            .contains(ocbot_core::SUPPRESSED_ATTACHMENT_HINT)
    );

    // A broadcast is transformed too.
    let outcome = port.broadcast(&report_spec()).await;
    assert_eq!(outcome.delivered.len(), 1);
    let posted = fake.posts().remove(0);
    assert!(posted.message.attachment.is_none());
}

#[tokio::test]
async fn a_permissive_policy_passes_the_spec_through() {
    let fake = Arc::new(FakeChat::new().with_announce(7));
    let port = PolicyPort::new(policy(ReportExcerpt::Full, true), fake.clone());
    let destination = Destination {
        surface: Surface::Discord,
        chat_ref: ChatRef("7".to_owned()),
    };
    port.post(&destination, &report_spec()).await.unwrap();
    let posted = fake.posts().remove(0);
    assert_eq!(posted.message.content, REPORT);
    assert!(posted.message.attachment.is_some());
    assert_eq!(posted.message.mention_allowlist, vec![42]);
}

/// Quiet ops pass through untouched (status is not a report).
#[tokio::test]
async fn quiet_edits_and_reactions_pass_through() {
    struct RecordingQuiet;
    #[async_trait]
    impl ChatPort for RecordingQuiet {
        async fn post(
            &self,
            _destination: &Destination,
            message: &OutgoingMessage,
        ) -> Result<PostReceipt, SendError> {
            // Quiet ops never arrive here; if they do, they must be untouched.
            assert_eq!(message.content, "quiet status");
            Ok(PostReceipt {
                surface: Surface::Discord,
                msg_id: MsgId("1".to_owned()),
            })
        }
        async fn edit(&self, receipt: &PostReceipt, replacement: &OutgoingMessage) {
            assert_eq!(replacement.content, "quiet status");
            let _ = receipt;
        }
        async fn react(&self, _receipt: &PostReceipt, _emoji: char) {}
        async fn broadcast(&self, _message: &OutgoingMessage) -> BroadcastOutcome {
            BroadcastOutcome::default()
        }
        fn surfaces(&self) -> Vec<Surface> {
            vec![Surface::Discord]
        }
    }
    let port = PolicyPort::new(policy(ReportExcerpt::Off, false), Arc::new(RecordingQuiet));
    port.edit(
        &PostReceipt {
            surface: Surface::Discord,
            msg_id: MsgId("1".to_owned()),
        },
        &OutgoingMessage {
            content: "quiet status".to_owned(),
            attachment: None,
            components: Vec::new(),
            mention_allowlist: Vec::new(),
        },
    )
    .await;
}
