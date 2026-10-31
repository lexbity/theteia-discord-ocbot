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
    DISCORD_MESSAGE_CAP, EXCERPT_TARGET, Footer, OutgoingMessage, REDACTED, Redactor,
    ReportContext, decision_buttons, fence_neutralize, report, split_excerpt, staged_preview,
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
    report(
        label,
        outcome,
        footer,
        context,
        &Redactor::default(),
        None,
        None,
    )
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
    // SPEC-005: the footer is the stats line only — the diff-stat prose
    // ("files changed", "insertions") can no longer be rendered at all.
    assert!(
        !message.content.contains("insertions") && !message.content.contains("files changed"),
        "{}",
        message.content
    );
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
    let preview = staged_preview("poke", 123, 9, "fresh", "user/repo");
    assert!(preview.content.contains("**Ready to run: poke**"));
    assert!(preview.content.contains("_Project: user/repo_"));
    assert!(preview.content.contains("_Template: fresh_"));
    assert_eq!(preview.components.len(), 3);
    assert_eq!(preview.components[0].custom_id, "v1:send:123");
    assert_eq!(preview.components[1].custom_id, "v1:edit:123");
    assert_eq!(preview.components[2].custom_id, "v1:tpl:123");
    assert_eq!(preview.components[2].label, "Use resume template");
    assert_eq!(preview.mention_allowlist, vec![9]);
    let resume = staged_preview("poke", 123, 9, "resume", "user/repo");
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
fn oversized_label_and_header_names_cannot_break_the_cap() {
    // A label far past LABEL_MAX_CHARS and pathological project/plan display
    // names (filesystem-sourced, unbounded length): every dynamic piece is
    // clamped before the budget math, so the cap holds by construction.
    let footer = Footer {
        duration_secs: Some(1.0),
        ..Footer::default()
    };
    let message = report(
        &"label ".repeat(1_000),
        &Outcome::Completed {
            report: "x".repeat(10),
        },
        &footer,
        &done_context(2, true),
        &Redactor::default(),
        Some(&"p".repeat(5_000)),
        Some(&"q".repeat(5_000)),
    );
    assert!(
        message.content.len() <= DISCORD_MESSAGE_CAP,
        "content was {} bytes",
        message.content.len()
    );
    assert!(
        !message.content.contains(&"p".repeat(400)),
        "the project name was clamped, not carried whole"
    );
    assert!(
        !message.content.contains(&"q".repeat(400)),
        "the plan name was clamped, not carried whole"
    );
    assert!(
        !message.content.contains(&"label ".repeat(60)),
        "the label was clamped: {}",
        message.content
    );
}

#[test]
fn header_names_cannot_smuggle_markup() {
    // Display names come from the filesystem: backticks would close the bold
    // span and control characters could break the line structure. Both are
    // neutralized by the shared name sanitizer.
    let message = report(
        "task",
        &Outcome::Completed {
            report: "ok".to_owned(),
        },
        &Footer::default(),
        &done_context(4, false),
        &Redactor::default(),
        Some("weird\nname`x`\u{0007}tail"),
        Some("po`wn\n.md"),
    );
    assert!(
        message.content.contains("**Project: weird name'x' tail**"),
        "{}",
        message.content
    );
    assert!(
        message.content.contains("**Plan: po'wn .md**"),
        "{}",
        message.content
    );
    assert!(
        !message.content.contains("name`x`") && !message.content.contains("po`wn"),
        "no raw backticked name survived the header: {}",
        message.content
    );
    assert!(
        !message.content.contains('\u{0007}'),
        "no control character survived: {}",
        message.content
    );
}

#[test]
fn header_names_pass_through_redaction() {
    // Project and plan display names are filesystem text: a secret-bearing
    // repo or filename must not ride the header verbatim (R-12).
    let redactor = Redactor::new(&["topsecret".to_owned()]).expect("pattern compiles");
    let message = report(
        "task",
        &Outcome::Completed {
            report: "ok".to_owned(),
        },
        &Footer::default(),
        &done_context(9, false),
        &redactor,
        Some("org/topsecret-repo"),
        Some("topsecret-migration.md"),
    );
    assert!(
        !message.content.contains("topsecret"),
        "{}",
        message.content
    );
    assert!(
        message.content.contains("**Project: org/[redacted]-repo**"),
        "{}",
        message.content
    );
    assert!(
        message
            .content
            .contains("**Plan: [redacted]-migration.md**"),
        "{}",
        message.content
    );
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
fn staged_preview_is_bounded_and_carries_no_prompt_chrome() {
    // SPEC-005 G-01: the preview dropped the prompt, so its size is bounded
    // structurally; oversized inputs are clamped and no fenced block ever
    // appears.
    let preview = staged_preview("task", 3, 9, "fresh", &"p".repeat(5_000));
    assert!(
        preview.content.len() < 400,
        "the oversized name was not clamped: {} bytes",
        preview.content.len()
    );
    assert!(preview.content.contains('…'), "{}", preview.content);
    assert!(!preview.content.contains("```"), "{}", preview.content);

    // An oversized label is clamped too, and the template line survives.
    let preview = staged_preview(&"l".repeat(5_000), 3, 9, "resume", "user/repo");
    assert!(
        preview.content.len() <= DISCORD_MESSAGE_CAP,
        "content was {} bytes",
        preview.content.len()
    );
    assert!(preview.content.contains("_Template: resume_"));

    // A control character inside a filesystem-sourced name cannot smuggle a
    // line break into the italic span.
    let preview = staged_preview("task", 3, 9, "fresh", "weird\nname\u{0007}with controls");
    assert!(
        preview
            .content
            .contains("_Project: weird name with controls_"),
        "{}",
        preview.content
    );
}

#[test]
fn aborted_and_empty_headlines_render_with_an_empty_fence() {
    let aborted = plain_report(
        "poke",
        &Outcome::Aborted,
        &Footer::default(),
        &done_context(1, true),
    );
    assert!(aborted.content.contains("⏹ Aborted — **poke**"));
    assert!(
        aborted.content.contains("```\n\n```"),
        "an aborted run has no body to excerpt"
    );
    // Non-completed outcomes carry followup/park/discard (report.rs).
    assert_eq!(aborted.components[2].custom_id, "v1:discard:1");

    let empty = plain_report(
        "poke",
        &Outcome::Empty,
        &Footer::default(),
        &done_context(1, true),
    );
    assert!(empty.content.contains("⬜ Empty — **poke**"));
    assert!(empty.content.contains("```\n\n```"));
}

#[test]
fn footer_duration_scales_from_sub_second_to_hours() {
    let render = |secs: f64| {
        Footer {
            duration_secs: Some(secs),
            ..Footer::default()
        }
        .render()
    };
    assert_eq!(render(0.5), "Duration <1s");
    assert_eq!(render(45.0), "Duration 45s");
    assert_eq!(render(90.0), "Duration 1m 30s");
    assert_eq!(render(120.0), "Duration 2m");
    assert_eq!(render(3_725.0), "Duration 1h 2m");
}

#[test]
fn footer_tokens_and_cost_render_in_both_scales() {
    let render = |input: f64, output: f64, reasoning: f64| {
        Footer {
            tokens: Some(TokenCounts {
                input,
                output,
                reasoning,
            }),
            ..Footer::default()
        }
        .render()
    };
    assert_eq!(render(120.0, 30.0, 0.0), "150 tok");
    assert_eq!(render(1_500.0, 0.0, 0.0), "1.5k tok");
    // A zero total contributes nothing rather than "0 tok".
    assert_eq!(render(0.0, 0.0, 0.0), "Duration n/a");
    let cost = Footer {
        cost: Some(0.126),
        ..Footer::default()
    };
    assert_eq!(cost.render(), "$0.13");
}

#[test]
fn project_centric_report_shows_names_and_stats_only() {
    let footer = Footer {
        duration_secs: Some(90.0),
        tokens: Some(TokenCounts {
            input: 100.0,
            output: 50.0,
            reasoning: 0.0,
        }),
        cost: Some(0.25),
    };
    let message = report(
        "ship",
        &Outcome::Completed {
            report: "did the thing".to_owned(),
        },
        &footer,
        &done_context(9, true),
        &Redactor::default(),
        Some("alpha"),
        Some("login-rework"),
    );
    assert!(message.content.contains("**Project: alpha**"));
    assert!(message.content.contains("**Plan: login-rework**"));
    // The stats survive; the path-bearing diff stat does not (SPEC-005).
    assert!(message.content.contains("Duration 1m 30s"));
    assert!(message.content.contains("150 tok"));
    assert!(message.content.contains("$0.25"));
    assert!(
        !message.content.contains("insertions"),
        "no diff stat inline"
    );
    assert!(!message.content.contains("files changed"));
}

#[test]
fn project_only_report_degrades_to_a_stats_only_footer() {
    let footer = Footer::default();
    let message = report(
        "poke",
        &Outcome::Completed {
            report: "ok".to_owned(),
        },
        &footer,
        &done_context(3, true),
        &Redactor::default(),
        Some("beta"),
        None,
    );
    assert!(message.content.contains("**Project: beta**"));
    assert!(!message.content.contains("**Plan:"), "no plan was given");
    // No duration/tokens/cost known: the stats-only footer still degrades.
    assert!(message.content.contains("Duration n/a"));
    assert!(!message.content.contains("file changed"));
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
        // SPEC-005: cycle every header shape (project+plan, project-only,
        // plan-only, bare) with adversarial filesystem-sourced name lengths;
        // the cap must hold across all of them.
        let name = |value: u64| "n".repeat(1 + fit(value % 500));
        let (project, plan) = match case % 4 {
            0 => (Some(name(next())), Some(name(next()))),
            1 => (Some(name(next())), None),
            2 => (None, Some(name(next()))),
            _ => (None, None),
        };
        let message = report(
            &label,
            &outcome,
            &footer,
            &context,
            &Redactor::default(),
            project.as_deref(),
            plan.as_deref(),
        );
        assert!(
            message.content.len() <= DISCORD_MESSAGE_CAP,
            "case {case}: {} bytes",
            message.content.len()
        );
        if project.is_some() {
            assert!(
                message.content.contains("**Project: ") || message.content.contains("**Project: n"),
                "the project header renders when a name is given"
            );
        }
    }
}

#[test]
fn blank_header_names_render_no_line_at_all() {
    // Empty/whitespace display names are degenerate input: the report must
    // fall back to the bare headline, not render "**Project: **" noise or a
    // stray blank line above the headline.
    let message = report(
        "task",
        &Outcome::Completed {
            report: "ok".to_owned(),
        },
        &Footer::default(),
        &done_context(9, true),
        &Redactor::default(),
        Some(""),
        Some("   "),
    );
    assert!(
        !message.content.contains("**Project:"),
        "{}",
        message.content
    );
    assert!(!message.content.contains("**Plan:"), "{}", message.content);
    assert!(
        message.content.starts_with("✅ Completed — **task**"),
        "no stray blank line above the headline: {}",
        message.content
    );
}

#[test]
fn footer_boundaries_are_exclusive_at_the_right_edges() {
    // The one-second edge: "<1s" is strictly-below, not at-or-below.
    let render = |secs: f64| {
        Footer {
            duration_secs: Some(secs),
            ..Footer::default()
        }
        .render()
    };
    assert_eq!(render(0.999), "Duration <1s");
    assert_eq!(render(1.0), "Duration 1s");
}

#[test]
fn token_sum_includes_reasoning_and_the_k_boundary_is_inclusive() {
    // Non-zero reasoning on every term: a sum that drops a term must change
    // the total. And exactly 1_000 tokens is "k" scale (>=, not >).
    let render = |input: f64, output: f64, reasoning: f64| {
        Footer {
            tokens: Some(TokenCounts {
                input,
                output,
                reasoning,
            }),
            ..Footer::default()
        }
        .render()
    };
    assert_eq!(render(10.0, 5.0, 4.0), "19 tok");
    assert_eq!(render(1_000.0, 0.0, 0.0), "1.0k tok");
    assert_eq!(render(999.0, 0.0, 0.0), "999 tok");
}

#[test]
fn a_budget_over_oversized_body_with_only_a_leading_newline_is_hard_cut() {
    // The newline-at-index-0 case must fall through to the hard cut, not cut
    // the excerpt empty: the guard exists precisely to reject a zero cut.
    let (kept, truncated) = split_excerpt(&format!("\n{}", "x".repeat(50)), 10);
    assert!(truncated);
    assert_eq!(kept, format!("\n{}", "x".repeat(9)));
}

#[test]
fn inline_excerpt_budget_is_the_cap_minus_everything_else_exactly() {
    // The contract of report_excerpt's overhead math, pinned at the boundary:
    // a body up to (cap - assembly - 10 bytes of fences/allowance) renders
    // inline, one byte more moves the full text to the attachment. A padded
    // configuration keeps the budget under EXCERPT_TARGET so the clamp can
    // never mask a mis-counted overhead.
    let redactor = Redactor::new(&[]).expect("empty pattern list");
    let context = ReportContext {
        action_id: 1,
        is_git: false,
        operator_id: 42,
        followups: 3,
    };
    let label = "l".repeat(200);
    let project = "p".repeat(160);
    let plan = "q".repeat(160);
    let body = |text: String| Outcome::Completed { report: text };
    let build = |text: String| {
        report(
            &label,
            &body(text),
            &Footer::default(),
            &context,
            &redactor,
            Some(&project),
            Some(&plan),
        )
    };

    let skeleton = build(String::new());
    assert!(skeleton.attachment.is_none(), "empty body fits");
    // Everything the excerpt does not pay for: the skeleton minus the closing
    // fence's 5 bytes is exactly head+footer+tail in the budget's accounting,
    // which then spends 8 more on fence overhead and 2 on the ellipsis.
    let assembly = skeleton.content.len() - 5;
    let budget = (DISCORD_MESSAGE_CAP - 10 - assembly).min(EXCERPT_TARGET);
    assert!(
        budget < EXCERPT_TARGET,
        "test configuration must leave the clamp inert: budget {budget}"
    );

    let fits = build("z".repeat(budget));
    assert!(
        fits.attachment.is_none(),
        "a body of exactly the budget must render inline"
    );
    assert!(fits.content.len() <= DISCORD_MESSAGE_CAP);
    let over = build("z".repeat(budget + 1));
    assert!(
        over.attachment.is_some(),
        "one byte past the budget must move to the attachment"
    );
    assert!(over.content.len() <= DISCORD_MESSAGE_CAP);
}
