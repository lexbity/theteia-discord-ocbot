//! Report rendering tests (SPEC-002 §4.3): the `render::report` concern,
//! exercised through its public API only. Per FR-13 this file imports nothing
//! from `render::gate`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

use ocbot_core::outcome::{Outcome, TokenCounts};
use ocbot_core::render::{
    DISCORD_MESSAGE_CAP, Footer, OutgoingMessage, REDACTED, Redactor, ReportContext,
    decision_buttons, fence_neutralize, report, split_excerpt, staged_preview,
};

fn done_context(action_id: i64, is_git: bool) -> ReportContext {
    ReportContext {
        action_id,
        is_git,
        operator_id: 42,
        followups: 0,
    }
}

/// `report` with the no-op redactor: the tests that do not exercise R-12.
fn plain_report(
    label: &str,
    outcome: &Outcome,
    footer: &Footer,
    context: &ReportContext,
) -> OutgoingMessage {
    report(label, outcome, footer, context, &Redactor::default())
}

#[test]
fn fence_neutralization_never_leaves_triple_backticks() {
    let text = "code:\n```\nlet x = 1;\n```\nthe end ``` more";
    let neutralized = fence_neutralize(text);
    assert!(!neutralized.contains("```"));
    assert!(neutralized.contains("` ``"));
}

#[test]
fn split_respects_budget_and_newline_boundary() {
    // "aaaa\nbbbb\ncccc" is 14 bytes; budget 10 keeps 9 bytes, cutting at
    // the last newline inside the window.
    let (excerpt, truncated) = split_excerpt("aaaa\nbbbb\ncccc", 10);
    assert!(truncated);
    assert_eq!(excerpt, "aaaa\nbbbb");
    let (short, truncated) = split_excerpt("hello", 100);
    assert!(!truncated);
    assert_eq!(short, "hello");
}

#[test]
fn redaction_scrubs_the_inline_path_but_keeps_the_record() {
    let patterns = vec![r"(?i)discord[_-]?token\s*[:=]\s*[A-Za-z0-9._-]{8,}".to_owned()];
    let redactor = Redactor::new(&patterns).expect("pattern compiles");
    assert!(redactor.is_active());
    let secret = "DISCORD_TOKEN=abcdefgh12345678";
    let body = format!("{secret}\n{}", "detail ".repeat(500));
    let message = report(
        "leaky",
        &Outcome::Completed { report: body },
        &Footer::default(),
        &done_context(3, true),
        &redactor,
    );
    assert!(
        !message.content.contains("abcdefgh12345678"),
        "inline content is scrubbed"
    );
    assert!(message.content.contains(REDACTED));
    let attachment = message.attachment.expect("truncated report is attached");
    let full = String::from_utf8(attachment.bytes).expect("utf8");
    assert!(
        full.contains("abcdefgh12345678"),
        "the full record stays verbatim (documented residual, R-12)"
    );
}

#[test]
fn report_stays_under_cap_and_carries_footer() {
    let long = "word ".repeat(2_000);
    let footer = Footer {
        diff_stat: Some(" 3 files changed, 12 insertions(+), 4 deletions(-)".to_owned()),
        duration_secs: Some(252.0),
        tokens: Some(TokenCounts {
            input: 31_000.0,
            output: 400.0,
            reasoning: 0.0,
        }),
        cost: Some(0.41),
    };
    let message = plain_report(
        "big task",
        &Outcome::Completed { report: long },
        &footer,
        &done_context(5, true),
    );
    assert!(
        message.content.len() <= DISCORD_MESSAGE_CAP,
        "content was {} bytes",
        message.content.len()
    );
    assert!(message.content.contains("Duration 4m 12s"));
    assert!(message.content.contains("31.4k tok"));
    assert!(message.content.contains("$0.41"));
    assert!(message.content.contains("12 insertions(+)"));
    assert_eq!(message.mentioned_users, vec![42]);
    let attached = message.attachment.expect("truncated report is attached");
    assert_eq!(attached.filename, "report.md");
    assert!(attached.bytes.len() > message.content.len());
}

#[test]
fn short_report_has_no_attachment() {
    let message = plain_report(
        "hi",
        &Outcome::Completed {
            report: "short".to_owned(),
        },
        &Footer::default(),
        &done_context(7, true),
    );
    assert!(message.attachment.is_none());
    assert!(message.content.contains("Duration n/a"));
}

#[test]
fn preview_carries_send_edit_and_template_buttons() {
    let preview = staged_preview("poke", "do the thing", 123, 9, "fresh");
    assert_eq!(preview.components.len(), 3);
    assert_eq!(preview.components[0].custom_id, "v1:send:123");
    assert_eq!(preview.components[1].custom_id, "v1:edit:123");
    assert_eq!(preview.components[2].custom_id, "v1:tpl:123");
    assert_eq!(preview.components[2].label, "Use resume template");
    assert_eq!(preview.mentioned_users, vec![9]);
    let resume = staged_preview("poke", "do the thing", 123, 9, "resume");
    assert_eq!(resume.components[2].label, "Use fresh template");
}

#[test]
fn label_cannot_inject_markdown() {
    let message = plain_report(
        "a\n```\n@everyone",
        &Outcome::Completed {
            report: "x".to_owned(),
        },
        &Footer::default(),
        &done_context(1, true),
    );
    assert_eq!(
        message.content.lines().next().unwrap(),
        "✅ Completed — **a ``` @everyone**"
    );
}

#[test]
fn completed_git_report_carries_accept_and_park() {
    let buttons = decision_buttons(
        &Outcome::Completed {
            report: "x".to_owned(),
        },
        &done_context(11, true),
    );
    assert_eq!(buttons.len(), 3);
    assert_eq!(buttons[0].custom_id, "v1:accept:11");
    assert_eq!(buttons[0].label, "Accept");
    assert!(!buttons[0].disabled);
    assert_eq!(buttons[1].custom_id, "v1:followup:11");
    assert_eq!(buttons[2].custom_id, "v1:park:11");
    assert!(!buttons[2].disabled);
}

#[test]
fn errored_report_carries_followup_park_and_discard() {
    let buttons = decision_buttons(
        &Outcome::Errored {
            error: "boom".to_owned(),
        },
        &done_context(13, true),
    );
    assert_eq!(buttons.len(), 3);
    assert_eq!(buttons[0].custom_id, "v1:followup:13");
    assert_eq!(buttons[1].custom_id, "v1:park:13");
    assert_eq!(buttons[2].custom_id, "v1:discard:13");
}

#[test]
fn reports_nudge_toward_park_after_two_followups() {
    let done = plain_report(
        "t",
        &Outcome::Completed {
            report: "x".to_owned(),
        },
        &Footer::default(),
        &ReportContext {
            action_id: 1,
            is_git: true,
            operator_id: 42,
            followups: 2,
        },
    );
    assert!(done.content.contains("Park & redo"), "{}", done.content);
    let fresh = plain_report(
        "t",
        &Outcome::Completed {
            report: "x".to_owned(),
        },
        &Footer::default(),
        &ReportContext {
            action_id: 1,
            is_git: true,
            operator_id: 42,
            followups: 1,
        },
    );
    assert!(!fresh.content.contains("several follow-ups"));
}

#[test]
fn non_git_completed_report_marks_done_and_disables_park() {
    let message = plain_report(
        "t",
        &Outcome::Completed {
            report: "x".to_owned(),
        },
        &Footer::default(),
        &done_context(12, false),
    );
    let accept = &message.components[0];
    assert_eq!(accept.label, "Mark done");
    assert_eq!(accept.custom_id, "v1:accept:12");
    assert!(
        message
            .components
            .iter()
            .any(|c| c.label == "Park & redo" && c.disabled),
        "park disabled for non-git"
    );
    assert!(message.content.contains("non-git"));
}
