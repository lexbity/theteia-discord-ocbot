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
    report(label, outcome, footer, context, &Redactor::default(), None, None)
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
fn split_never_cuts_mid_codepoint() {
    // 2-byte chars: the byte budget lands inside the 4th ü.
    let (excerpt, truncated) = split_excerpt(&"ü".repeat(100), 7);
    assert!(truncated);
    assert_eq!(excerpt.chars().count(), 3);
    // A budget that exactly precedes a codepoint is untouched.
    let (excerpt, _) = split_excerpt(&"ü".repeat(100), 8);
    assert_eq!(excerpt.chars().count(), 4);
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
        None,
        None,
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
    assert_eq!(message.mention_allowlist, vec![42]);
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
    let preview = staged_preview("poke", "do the thing", 123, 9, "fresh", None);
    assert_eq!(preview.components.len(), 3);
    assert_eq!(preview.components[0].custom_id, "v1:send:123");
    assert_eq!(preview.components[1].custom_id, "v1:edit:123");
    assert_eq!(preview.components[2].custom_id, "v1:tpl:123");
    assert_eq!(preview.components[2].label, "Use resume template");
    assert_eq!(preview.mention_allowlist, vec![9]);
    let resume = staged_preview("poke", "do the thing", 123, 9, "resume", None);
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

#[test]
fn notes_are_budgeted_not_appended_past_the_cap() {
    // The cap-overflow regression: both context notes used to be appended
    // *after* the excerpt was budgeted — ~80–155 bytes the excerpt never paid
    // for, pushing the report past 2 000 exactly when it was longest.
    let mut context = done_context(21, false);
    context.followups = 2;
    let message = plain_report(
        "longest",
        &Outcome::Completed {
            report: "line\n".repeat(1_000),
        },
        &Footer::default(),
        &context,
    );
    assert!(
        message.content.len() <= DISCORD_MESSAGE_CAP,
        "content was {} bytes",
        message.content.len()
    );
    assert!(message.content.contains("non-git"));
    assert!(message.content.contains("several follow-ups"));
    assert!(
        message.content.contains("Duration n/a"),
        "footer stays inline"
    );
    assert!(message.attachment.is_some(), "full text attached");
}

#[test]
fn oversized_label_and_footer_cannot_break_the_cap() {
    // A label far past LABEL_MAX_CHARS and a pathological one-line diff stat
    // (no insertion/deletion summary, so the footer falls back to the first
    // line — here 5 000 chars of path).
    let footer = Footer {
        diff_stat: Some(format!("{} | 1000 +\n", "p".repeat(5_000))),
        ..Footer::default()
    };
    let message = plain_report(
        &"label ".repeat(1_000),
        &Outcome::Completed {
            report: "x".repeat(10),
        },
        &footer,
        &done_context(2, true),
    );
    assert!(
        message.content.len() <= DISCORD_MESSAGE_CAP,
        "content was {} bytes",
        message.content.len()
    );
    assert!(
        !message.content.contains(&"p".repeat(400)),
        "the footer line was clamped, not carried whole"
    );
}

#[test]
fn footer_render_clamps_a_pathological_summary() {
    let footer = Footer {
        diff_stat: Some("z".repeat(5_000)),
        ..Footer::default()
    };
    let rendered = footer.render();
    assert!(rendered.chars().count() <= 300, "footer was {rendered}");
    assert!(rendered.ends_with('…'));
}

#[test]
fn redaction_cannot_grow_the_message_past_the_cap() {
    // Scrubbing replaces matches with "[redacted]" — longer than a short
    // match. Redaction runs before the budget math, so the growth is paid
    // for by the excerpt, never by the cap.
    let redactor = Redactor::new(&["ab".to_owned()]).expect("pattern compiles");
    let message = report(
        "leaky",
        &Outcome::Completed {
            report: "ab".repeat(2_000),
        },
        &Footer::default(),
        &done_context(4, true),
        &redactor,
        None,
        None,
    );
    assert!(
        message.content.len() <= DISCORD_MESSAGE_CAP,
        "content was {} bytes",
        message.content.len()
    );
    assert!(!message.content.contains("abab"), "body was scrubbed");
    assert!(message.content.contains(REDACTED));
}

#[test]
fn staged_preview_stays_within_the_cap_and_flags_truncation() {
    // The preview embeds `{git_log}` lines of unbounded subject length.
    let preview = staged_preview("task", &"line of prompt text\n".repeat(500), 3, 9, "fresh", None);
    assert!(
        preview.content.len() <= DISCORD_MESSAGE_CAP,
        "content was {} bytes",
        preview.content.len()
    );
    assert!(
        preview.content.contains("preview truncated"),
        "the clamp is flagged: {}",
        preview.content
    );
    // An oversized label is clamped too.
    let preview = staged_preview(&"l".repeat(5_000), "short", 3, 9, "resume", None);
    assert!(
        preview.content.len() <= DISCORD_MESSAGE_CAP,
        "content was {} bytes",
        preview.content.len()
    );
}

#[test]
fn report_respects_the_cap_across_a_deterministic_corpus() {
    // Deterministic xorshift battery (the client's tolerant-serde house
    // style): adversarial sizes and shapes for label, body, footer, and
    // context flags; the assembled content must always fit the cap.
    let mut state = 0x0C0F_FE12_3456_789Au64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    // The lint floor bans `as` conversions; every narrowing goes through
    // TryFrom/From.
    let fit = |value: u64| usize::try_from(value).expect("usize holds u64");
    for case in 0..512u64 {
        let label = "l".repeat(fit(next() % 600));
        let body_len = fit(next() % 4_000);
        let body = if next() % 3 == 0 {
            "ü".repeat(body_len)
        } else {
            "w".repeat(body_len)
        };
        let outcome = if next() % 5 == 0 {
            Outcome::Errored { error: body }
        } else {
            Outcome::Completed { report: body }
        };
        let small = |value: u64| f64::from(u32::try_from(value).expect("bounded by %"));
        let footer = Footer {
            diff_stat: (next() % 4 == 0).then(|| "f".repeat(fit(next() % 5_000))),
            duration_secs: Some(small(next() % 20_000)),
            tokens: Some(TokenCounts {
                input: small(next() % 100_000),
                output: 0.0,
                reasoning: 0.0,
            }),
            cost: Some(small(next() % 1_000) / 100.0),
        };
        let context = ReportContext {
            action_id: i64::try_from(case).expect("fits"),
            is_git: next() % 2 == 0,
            operator_id: 42,
            followups: i64::try_from(next() % 4).expect("bounded by %"),
        };
        let message = plain_report(&label, &outcome, &footer, &context);
        assert!(
            message.content.len() <= DISCORD_MESSAGE_CAP,
            "case {case}: {} bytes",
            message.content.len()
        );
    }
}
