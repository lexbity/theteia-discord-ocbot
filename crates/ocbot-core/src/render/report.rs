//! Finished-action report rendering and the staged preview (spec §6.6 P10,
//! FR-15/FR-18/FR-26).
//!
//! The report is headline + fenced excerpt + always-inline footer, followed by
//! decision buttons. The footer is composed *before* truncation so it can never
//! be cut, and the excerpt is budgeted against the Discord cap.

use crate::outcome::Outcome;
use crate::outcome::TokenCounts;

use super::label::{collapse_whitespace, short_name, truncate_chars};
use super::redact::Redactor;
use super::{
    AttachmentSpec, ButtonKind, ComponentSpec, OutgoingMessage, component_button,
    component_button_state,
};

/// Discord's hard message cap (spec P10).
pub const DISCORD_MESSAGE_CAP: usize = 2_000;
/// Inline excerpt target (spec P10: everyone reads the gist).
pub const EXCERPT_TARGET: usize = 1_500;
/// The longest label carried inline. Labels are operator text of unbounded
/// length; the headline must always leave room for the excerpt and footer.
pub const LABEL_MAX_CHARS: usize = 200;
/// The longest single-line footer. The diff-stat summary is one line; this
/// guards a pathological summary line so the footer can never crowd out the
/// excerpt entirely.
const FOOTER_MAX_CHARS: usize = 300;
/// The filename used for the full report attachment.
pub const REPORT_ATTACHMENT_NAME: &str = "report.md";

/// The report footer facts (spec P3/P10: git diff stat vs base, duration,
/// tokens/cost — degrading to "n/a" on anything unknown).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Footer {
    /// The `git diff --stat` text vs the action's pre-run state, when
    /// available.
    pub diff_stat: Option<String>,
    /// Wall-clock duration in seconds.
    pub duration_secs: Option<f64>,
    /// Token usage.
    pub tokens: Option<TokenCounts>,
    /// Cost in USD.
    pub cost: Option<f64>,
}

impl Footer {
    /// The single-line footer, always rendered inside the message budget: the
    /// result is clamped to `FOOTER_MAX_CHARS` so the doc's "inside the
    /// budget" claim holds by construction, not by convention.
    pub fn render(&self) -> String {
        let parts = [self.duration(), self.diff(), self.tokens(), self.cost()];
        let parts: Vec<&str> = parts.iter().filter_map(|part| (*part).as_deref()).collect();
        let joined = if parts.is_empty() {
            "Duration n/a".to_owned()
        } else {
            parts.join(" · ")
        };
        truncate_chars(&joined, FOOTER_MAX_CHARS)
    }

    fn duration(&self) -> Option<String> {
        let seconds = self.duration_secs?;
        if seconds < 1.0 {
            return Some("Duration <1s".to_owned());
        }
        let hours = (seconds / 3_600.0).floor();
        let minutes = (seconds % 3_600.0 / 60.0).floor();
        let secs = seconds % 60.0;
        let human = match (hours, minutes, secs) {
            (0.0, 0.0, seconds) => format!("{seconds:.0}s"),
            (0.0, minutes, 0.0) => format!("{minutes:.0}m"),
            (0.0, minutes, seconds) => format!("{minutes:.0}m {seconds:.0}s"),
            (hours, minutes, _) => format!("{hours:.0}h {minutes:.0}m"),
        };
        Some(format!("Duration {human}"))
    }

    fn diff(&self) -> Option<String> {
        let stat = self.diff_stat.as_deref()?;
        let summary = summarize_diff_stat(stat)
            .unwrap_or_else(|| stat.lines().next().unwrap_or_default().trim().to_owned());
        let summary = summary.trim();
        (!summary.is_empty()).then(|| summary.to_owned())
    }

    fn tokens(&self) -> Option<String> {
        let tokens = self.tokens?;
        let total = tokens.input + tokens.output + tokens.reasoning;
        if total <= 0.0 {
            return None;
        }
        let label = if total >= 1_000.0 {
            format!("{:.1}k tok", total / 1_000.0)
        } else {
            format!("{total:.0} tok")
        };
        Some(label)
    }

    fn cost(&self) -> Option<String> {
        let cost = self.cost?;
        Some(format!("${cost:.2}"))
    }
}

/// Extract the compact summary line from `git diff --stat` output, if any
/// (e.g. ` 3 files changed, 12 insertions(+), 4 deletions(-)`).
fn summarize_diff_stat(stat: &str) -> Option<String> {
    stat.lines()
        .rev()
        .find(|line| {
            let lower = line.to_ascii_lowercase();
            lower.contains("insertion") || lower.contains("deletion") || lower.contains("changed")
        })
        .map(|line| line.trim().to_owned())
}

/// Replace every triple-backtick fence inside `text` so the excerpt cannot
/// break the surrounding code block (spec §6.6 "fence-neutralized").
pub fn fence_neutralize(text: &str) -> String {
    text.replace("```", "` ``")
}

/// Split `text` at the last newline inside `budget` bytes. Returns the kept
/// text and whether it was truncated. A text longer than the budget with no
/// newlines is cut hard at the budget — on a char boundary, so multi-byte
/// content can never panic the slice.
pub fn split_excerpt(text: &str, budget: usize) -> (String, bool) {
    if text.len() <= budget {
        return (text.to_owned(), false);
    }
    // The budget is bytes; slide back to the nearest char boundary so a
    // codepoint straddling the cut cannot panic.
    let mut safe = budget.min(text.len());
    while !text.is_char_boundary(safe) {
        safe -= 1;
    }
    let window = &text[..safe];
    let cut = match window.rfind('\n') {
        Some(index) if index > 0 => index,
        _ => safe,
    };
    (text[..cut].to_owned(), true)
}

/// The report headline per outcome (spec §6.6 layout).
pub fn headline(outcome: &Outcome, label: &str) -> String {
    let bare = format!("**{label}**");
    match outcome {
        Outcome::Completed { .. } => format!("✅ Completed — {bare}"),
        Outcome::Errored { .. } => format!("❌ Errored — {bare}"),
        Outcome::Aborted => format!("⏹ Aborted — {bare}"),
        Outcome::Empty => format!("⬜ Empty — {bare}"),
    }
}

/// The context the decision buttons need (spec FR-15/FR-26): the action rowid
/// (for the `custom_id` grammar), whether the directory is a git repo (a
/// non-git action degrades to "Mark done", and Park/Reset are disabled), and
/// the follow-up count (a long chain nudges toward Park & redo, FR-18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportContext {
    /// The action row id.
    pub action_id: i64,
    /// Whether the run's directory is a git work tree.
    pub is_git: bool,
    /// The operator id, the only mentionable user (FR-3).
    pub operator_id: u64,
    /// Follow-ups already sent on this attempt chain.
    pub followups: i64,
}

/// The decision buttons for a finished action (spec FR-15). Buttons for verbs
/// whose handlers exist in this build only — no dead buttons. `Follow up`
/// lands in Phase 5.
pub fn decision_buttons(outcome: &Outcome, context: &ReportContext) -> Vec<ComponentSpec> {
    let id = context.action_id;
    let park = component_button_state(
        "Park & redo",
        format!("v1:park:{id}"),
        ButtonKind::Danger,
        !context.is_git,
    );
    let followup = component_button(
        "Follow up",
        format!("v1:followup:{id}"),
        ButtonKind::Primary,
    );
    match outcome {
        Outcome::Completed { .. } => vec![
            component_button(
                if context.is_git {
                    "Accept"
                } else {
                    "Mark done"
                },
                format!("v1:accept:{id}"),
                ButtonKind::Success,
            ),
            followup,
            park,
        ],
        Outcome::Errored { .. } | Outcome::Aborted | Outcome::Empty => {
            vec![
                followup,
                park,
                component_button("Discard", format!("v1:discard:{id}"), ButtonKind::Secondary),
            ]
        }
    }
}

/// The full report message: headline + fenced excerpt + always-inline footer +
/// context notes, followed by the decision buttons (FR-15).
///
/// The invariant is total: every dynamic piece (label, body, footer) is
/// redacted and clamped *before* the budget math — redaction can lengthen
/// text, so scrubbing first is what makes the byte accounting exact — and the
/// appended notes (`report_tail`) are part of the budget, not an
/// afterthought. Assembled `content` can never exceed the cap (P10).
///
/// For project-centric runs (SPEC-005), the project and plan names are displayed
/// prominently, and the diff stat is omitted (contains filesystem paths).
pub fn report(
    label: &str,
    outcome: &Outcome,
    footer: &Footer,
    context: &ReportContext,
    redactor: &Redactor,
    project: Option<&str>,
    plan: Option<&str>,
) -> OutgoingMessage {
    let label = truncate_chars(&collapse_whitespace(label), LABEL_MAX_CHARS);
    let head = headline(outcome, &redactor.scrub(&label));

    // For project-centric runs, create a footer without diff stat (SPEC-005)
    // The diff stat contains filesystem paths which should be hidden
    let footer_line = if project.is_some() || plan.is_some() {
        // Use stats only (NO diff stat with paths)
        let duration = footer.duration();
        let tokens = footer.tokens();
        let cost = footer.cost();
        let parts: Vec<&str> = vec![duration.as_deref(), tokens.as_deref(), cost.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        let joined = if parts.is_empty() {
            "Duration n/a".to_owned()
        } else {
            parts.join(" · ")
        };
        redactor.scrub(&truncate_chars(&joined, FOOTER_MAX_CHARS))
    } else {
        // `Footer::render` is single-line and self-clamped; scrub it too — the
        // diff-stat summary carries repo file paths (R-12 inline discipline).
        redactor.scrub(&footer.render())
    };
    let tail = report_tail(context);

    // Build the content with project/plan info (SPEC-005)
    let mut message_text = if let (Some(project), Some(plan)) = (project, plan) {
        format!("**Project: {project}**\n**Plan: {plan}**\n\n{head}")
    } else if let Some(project) = project {
        format!("**Project: {project}**\n\n{head}")
    } else {
        head
    };
    message_text.push_str("\n```\n");
    let (excerpt, truncated) = report_excerpt(
        &report_body(outcome),
        &message_text,
        &footer_line,
        &tail,
        redactor,
    );
    message_text.push_str(&excerpt);
    message_text.push_str("\n```\n");
    message_text.push_str(&footer_line);
    message_text.push_str(&tail);

    let attachment = if truncated {
        let full = report_body(outcome);
        Some(AttachmentSpec {
            filename: REPORT_ATTACHMENT_NAME.to_owned(),
            bytes: full.into_bytes(),
        })
    } else {
        None
    };
    OutgoingMessage {
        content: message_text,
        attachment,
        components: decision_buttons(outcome, context),
        mention_allowlist: vec![context.operator_id],
    }
}

/// The context notes appended after the footer: the FR-26 non-git degradation
/// note and the FR-18 follow-up nudge. Composed *before* excerpting so their
/// size is part of the budget — appending them after budgeting was the
/// cap-overflow bug (they are ~80–155 bytes the excerpt did not pay for).
fn report_tail(context: &ReportContext) -> String {
    let mut tail = String::new();
    if !context.is_git {
        tail.push_str(
            "\n_non-git directory: **Mark done** records the decision; Park/Reset are disabled._",
        );
    }
    if context.followups >= 2 {
        tail.push_str(
            "\n_this attempt has had several follow-ups — consider **Park & redo** instead (FR-18)._",
        );
    }
    tail
}

/// The excerpt portion budgeted against every other piece of the message —
/// headline, footer line, notes, and fences — so the assembled content can
/// never exceed the cap.
fn report_excerpt(
    body: &str,
    head: &str,
    footer_line: &str,
    tail: &str,
    redactor: &Redactor,
) -> (String, bool) {
    let overhead = head.len() + footer_line.len() + tail.len() + "```\n".len() * 2;
    let available = DISCORD_MESSAGE_CAP
        .saturating_sub(overhead)
        .saturating_sub(2);
    let neutralized = fence_neutralize(&redactor.scrub(body));
    split_excerpt(&neutralized, available.min(EXCERPT_TARGET))
}

/// The raw report body (from a completed/errored outcome).
fn report_body(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Completed { report } => report.clone(),
        Outcome::Errored { error } => error.clone(),
        Outcome::Aborted | Outcome::Empty => String::new(),
    }
}

/// Longest opaque project name a preview shows; display names are two path
/// components, but a hostile filesystem must not be able to grow the line.
const PROJECT_NAME_MAX_CHARS: usize = 160;

/// A preview spec for a staged action: the label, the opaque project name and
/// the template, plus the Send / Edit-and-send buttons (`custom_id` grammar
/// `v1:send:<id>` / `v1:edit:<id>`) and the template toggle (`v1:tpl:<id>`).
///
/// `template` is the envelope currently rendered (`"fresh"` or `"resume"`); the
/// toggle button offers the *other* one. Re-rendering happens at review time so
/// `{git_log}`/`{git_status}` are fresh (FR-6/P13).
///
/// The prompt is **never shown** (SPEC-005 G-01: paths and prompt bodies stay
/// out of user-facing chrome); the stored prompt — and the Edit modal — keep
/// the full text.
pub fn staged_preview(
    label: &str,
    action_id: i64,
    operator_id: u64,
    template: &str,
    project_name: &str,
) -> OutgoingMessage {
    let label = truncate_chars(&collapse_whitespace(label), LABEL_MAX_CHARS);
    let project = short_name(project_name, PROJECT_NAME_MAX_CHARS);
    let toggle_label = if template == "resume" {
        "Use fresh template"
    } else {
        "Use resume template"
    };
    let content =
        format!("**Ready to run: {label}**\n_Project: {project}_\n_Template: {template}_");
    OutgoingMessage {
        content,
        attachment: None,
        components: vec![
            component_button(
                "Send as-is",
                format!("v1:send:{action_id}"),
                ButtonKind::Primary,
            ),
            component_button(
                "Edit & send",
                format!("v1:edit:{action_id}"),
                ButtonKind::Secondary,
            ),
            component_button(
                toggle_label,
                format!("v1:tpl:{action_id}"),
                ButtonKind::Secondary,
            ),
        ],
        mention_allowlist: vec![operator_id],
    }
}
