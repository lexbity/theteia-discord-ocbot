//! FR-3 structural proof: every Discord payload built from a core
//! [`OutgoingMessage`] carries `allowed_mentions { parse: [], users:
//! [OPERATOR] }` — and nothing else that could ping anyone.

// Integration tests assert via unwrap/panic; production code remains strict
// (workspace lint floor + G3). See common/mod.rs for the shared harness.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use ocbot::adapters::discord::{message_builder, operator_allowed_mentions};
use ocbot_core::render::{
    ButtonKind, Footer, OutgoingMessage, component_button, report, staged_preview,
};
use ocbot_core::{Outcome, TokenCounts};

const OPERATOR: u64 = 42;

fn allowed_mentions_of(message: &serde_json::Value) -> serde_json::Value {
    message
        .get("allowed_mentions")
        .cloned()
        .expect("payload must set allowed_mentions")
}

#[test]
fn every_message_type_carries_operator_only_mentions() {
    let messages: Vec<OutgoingMessage> = vec![
        report(
            "a task",
            &Outcome::Completed {
                report: "done".to_owned(),
            },
            &Footer::default(),
            OPERATOR,
        ),
        staged_preview("label", "prompt text", 3, OPERATOR),
        OutgoingMessage {
            content: "status".to_owned(),
            attachment: None,
            components: vec![component_button("go", "v1:send:3", ButtonKind::Primary)],
            mentioned_users: vec![OPERATOR],
        },
        OutgoingMessage {
            content: "with tokens".to_owned(),
            attachment: None,
            components: Vec::new(),
            mentioned_users: vec![OPERATOR],
        },
    ];

    for message in messages {
        let payload = serde_json::to_value(message_builder(&message, OPERATOR))
            .expect("CreateMessage serializes");
        let allowed = allowed_mentions_of(&payload);
        assert!(
            allowed
                .get("parse")
                .and_then(serde_json::Value::as_array)
                .is_some_and(Vec::is_empty),
            "parse must be the empty set (nothing may ping): {allowed}"
        );
        let users = allowed
            .get("users")
            .and_then(serde_json::Value::as_array)
            .expect("users list");
        assert_eq!(
            users.len(),
            1,
            "only the operator may be mentioned: {allowed}"
        );
        let user = users[0]
            .as_u64()
            .or_else(|| users[0].as_str().and_then(|s| s.parse().ok()));
        assert_eq!(user, Some(OPERATOR));
        assert!(
            payload
                .get("content")
                .and_then(serde_json::Value::as_str)
                .is_some(),
            "content flows through"
        );
    }
}

#[test]
fn mentions_builder_serializes_to_the_fr3_shape() {
    let value = serde_json::to_value(operator_allowed_mentions(OPERATOR)).expect("serialize");
    let parse = value
        .get("parse")
        .and_then(serde_json::Value::as_array)
        .expect("parse");
    assert!(parse.is_empty(), "parse: {value}");
    let users = value
        .get("users")
        .and_then(serde_json::Value::as_array)
        .expect("users");
    let user = users[0]
        .as_u64()
        .or_else(|| users[0].as_str().and_then(|s| s.parse().ok()));
    assert_eq!(user, Some(OPERATOR), "users: {value}");
    // The @everyone-style allow switches never appear.
    let serialized = value.to_string();
    assert!(
        !serialized.contains("everyone"),
        "no everyone allowance: {serialized}"
    );
}

#[test]
fn report_footer_is_inline_and_stays_under_cap() {
    let footer = Footer {
        diff_stat: Some(" 2 files changed, 8 insertions(+), 1 deletion(-)".to_owned()),
        duration_secs: Some(90.0),
        tokens: Some(core_token_counts()),
        cost: Some(0.12),
    };
    let message = report(
        "t",
        &Outcome::Completed {
            report: "body".to_owned(),
        },
        &footer,
        OPERATOR,
    );
    assert!(message.content.len() <= 2_000);
    assert!(message.content.contains("Duration 1m 30s"));
    assert!(message.content.contains("8 insertions(+)"));
    assert!(message.content.contains("$0.12"));
}

fn core_token_counts() -> TokenCounts {
    TokenCounts {
        input: 1_000.0,
        output: 300.0,
        reasoning: 0.0,
    }
}
