//! Finished-action report rendering and the staged preview (spec §6.6 P10,
//! FR-15/FR-18/FR-26).
//!
//! The report is headline + fenced excerpt + always-inline footer, followed by
//! decision buttons. The footer is composed *before* truncation so it can never
//! be cut, and the excerpt is budgeted against the Discord cap.

use crate::outcome::Outcome;
use crate::outcome::TokenCounts;

use super::label::collapse_whitespace;
use super::redact::Redactor;
use super::{
    AttachmentSpec, ButtonKind, ComponentSpec, OutgoingMessage, component_button,
    component_button_state,
};

/// Discord's hard message cap (spec P10).
pub const DISCORD_MESSAGE_CAP: usize = 2_000;
/// Inline excerpt target (spec P10: everyone reads the gist).
pub const EXCERPT_TARGET: usize = 1_500;
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
    /// The single-line footer, always rendered inside the message budget.
    pub fn render(&self) -> String {
        let parts = [self.duration(), self.diff(), self.tokens(), self.cost()];
        let parts: Vec<&str> = parts.iter().filter_map(|part| (*part).as_deref()).collect();
        if parts.is_empty() {
            "Duration n/a".to_owned()
        } else {
            parts.join(" · ")
        }
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
/// newlines is cut hard at the budget.
pub fn split_excerpt(text: &str, budget: usize) -> (String, bool) {
    if text.len() <= budget {
        return (text.to_owned(), false);
    }
    let safe = budget.min(text.len());
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

/// The full report message: headline + fenced excerpt + always-inline footer,
/// followed by the decision buttons (FR-15) and, for a non-git directory, the
/// FR-26 degradation note.
///
/// `budget` is the total allowed size of `content` (default Discord cap); the
/// footer is composed *before* truncation so it can never be cut.
pub fn report(
    label: &str,
    outcome: &Outcome,
    footer: &Footer,
    context: &ReportContext,
    redactor: &Redactor,
) -> OutgoingMessage {
    let label = collapse_whitespace(label);
    let head = headline(outcome, &label);
    let (excerpt, truncated) = report_excerpt(outcome, footer, &head, DISCORD_MESSAGE_CAP);
    let mut message_text = format!("{head}\n```\n{excerpt}\n```\n{}", footer.render());
    if !context.is_git {
        message_text.push_str(
            "\n_non-git directory: **Mark done** records the decision; Park/Reset are disabled._",
        );
    }
    if context.followups >= 2 {
        message_text.push_str(
            "\n_this attempt has had several follow-ups — consider **Park & redo** instead (FR-18)._",
        );
    }
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
        // R-12: only the inline path is scrubbed (see [`Redactor`]).
        content: redactor.scrub(&message_text),
        attachment,
        components: decision_buttons(outcome, context),
        mentioned_users: vec![context.operator_id],
    }
}

/// The excerpt portion budgeted against the headline, footer, and fences.
fn report_excerpt(
    outcome: &Outcome,
    footer: &Footer,
    headline: &str,
    budget: usize,
) -> (String, bool) {
    let body = report_body(outcome);
    let overhead = headline.len() + footer.render().len() + "```\n".len() * 2;
    let available = budget.saturating_sub(overhead).saturating_sub(2);
    let neutralized = fence_neutralize(&body);
    let (excerpt, truncated) = split_excerpt(&neutralized, available.min(EXCERPT_TARGET));
    if truncated {
        (excerpt, true)
    } else {
        (neutralized, false)
    }
}

/// The raw report body (from a completed/errored outcome).
fn report_body(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Completed { report } => report.clone(),
        Outcome::Errored { error } => error.clone(),
        Outcome::Aborted | Outcome::Empty => String::new(),
    }
}

/// A preview spec for a staged action: the prompt text plus the
/// Send / Edit-and-send buttons (`custom_id` grammar `v1:send:<id>` /
/// `v1:edit:<id>`) and the Phase 6 template toggle (`v1:tpl:<id>`).
///
/// `template` is the envelope currently rendered (`"fresh"` or `"resume"`); the
/// toggle button offers the *other* one. Re-rendering happens at review time so
/// `{git_log}`/`{git_status}` are fresh (FR-6/P13).
pub fn staged_preview(
    label: &str,
    prompt: &str,
    action_id: i64,
    operator_id: u64,
    template: &str,
) -> OutgoingMessage {
    let label = collapse_whitespace(label);
    let content = format!(
        "**Ready to run: {label}**\n_template: {template}_\n```\n{}\n```",
        fence_neutralize(prompt)
    );
    let toggle_label = if template == "resume" {
        "Use fresh template"
    } else {
        "Use resume template"
    };
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
        mentioned_users: vec![operator_id],
    }
}
