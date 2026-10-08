//! Report rendering, truncation, and fence neutralization (spec §6.6 P10).
//!
//! Handlers produce [`OutgoingMessage`] **specs** — pure data, serenity-free.
//! The Discord adapter is the only place that turns a spec into a payload, and
//! its single builder always sets `allowed_mentions { parse: [], users:
//! [OPERATOR] }` (FR-3, structurally enforced).

use crate::history::{HistoryKind, HistoryRow};
use crate::outcome::Outcome;
use crate::outcome::TokenCounts;

/// Discord's hard message cap (spec P10).
pub const DISCORD_MESSAGE_CAP: usize = 2_000;
/// Inline excerpt target (spec P10: everyone reads the gist).
pub const EXCERPT_TARGET: usize = 1_500;
/// The filename used for the full report attachment.
pub const REPORT_ATTACHMENT_NAME: &str = "report.md";

/// A message the bot wants to post. Pure data.
#[derive(Debug, Clone, PartialEq)]
pub struct OutgoingMessage {
    /// The message text (concatenated inline content).
    pub content: String,
    /// An optional file attachment (full report when truncated).
    pub attachment: Option<AttachmentSpec>,
    /// Interactive components (buttons).
    pub components: Vec<ComponentSpec>,
    /// Discord user ids that may be mentioned; the adapter sets
    /// `allowed_mentions.users` from exactly this list and never the
    /// `everyone`/`here` bits.
    pub mentioned_users: Vec<u64>,
}

/// A file attachment.
#[derive(Debug, Clone, PartialEq)]
pub struct AttachmentSpec {
    /// The file name, e.g. `report.md`.
    pub filename: String,
    /// The file bytes.
    pub bytes: Vec<u8>,
}

/// One button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentSpec {
    /// The button label.
    pub label: String,
    /// The `custom_id` the router will answer (spec §6.6 grammar).
    pub custom_id: String,
    /// The button style.
    pub kind: ButtonKind,
    /// Whether the button is disabled (FR-15: a verb that cannot run here is
    /// shown disabled-with-reason, never as a dead button).
    pub disabled: bool,
}

/// Discord button styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonKind {
    /// Blurple.
    Primary,
    /// Grey.
    Secondary,
    /// Red.
    Danger,
    /// Green.
    Success,
}

/// Build a button spec.
pub fn component_button(
    label: impl Into<String>,
    custom_id: impl Into<String>,
    kind: ButtonKind,
) -> ComponentSpec {
    component_button_state(label, custom_id, kind, false)
}

/// Build a button spec with an explicit enabled/disabled state.
pub fn component_button_state(
    label: impl Into<String>,
    custom_id: impl Into<String>,
    kind: ButtonKind,
    disabled: bool,
) -> ComponentSpec {
    ComponentSpec {
        label: label.into(),
        custom_id: custom_id.into(),
        kind,
        disabled,
    }
}

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
/// (for the `custom_id` grammar) and whether the directory is a git repo (a
/// non-git action degrades to "Mark done", and Park/Reset are disabled).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportContext {
    /// The action row id.
    pub action_id: i64,
    /// Whether the run's directory is a git work tree.
    pub is_git: bool,
    /// The operator id, the only mentionable user (FR-3).
    pub operator_id: u64,
}

/// The decision buttons for a finished action (spec FR-15). Buttons for verbs
/// whose handlers exist in this build only — no dead buttons. `Follow up`
/// arrives in Phase 5.
pub fn decision_buttons(outcome: &Outcome, context: &ReportContext) -> Vec<ComponentSpec> {
    let id = context.action_id;
    let park = component_button_state(
        "Park & redo",
        format!("v1:park:{id}"),
        ButtonKind::Danger,
        !context.is_git,
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
            park,
        ],
        Outcome::Errored { .. } | Outcome::Aborted | Outcome::Empty => vec![
            park,
            component_button("Discard", format!("v1:discard:{id}"), ButtonKind::Secondary),
        ],
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
) -> OutgoingMessage {
    let label = sanitize_label(label);
    let head = headline(outcome, &label);
    let (excerpt, truncated) = report_excerpt(outcome, footer, &head, DISCORD_MESSAGE_CAP);
    let mut message_text = format!("{head}\n```\n{excerpt}\n```\n{}", footer.render());
    if !context.is_git {
        message_text.push_str(
            "\n_non-git directory: **Mark done** records the decision; Park/Reset are disabled._",
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
        content: message_text,
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

/// Collapse label whitespace/newlines so a label can never inject structural
/// markdown into the headline.
fn sanitize_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut prev_space = false;
    for ch in label.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    out.trim().to_owned()
}

/// A preview spec for a staged action: the prompt text plus the
/// Send / Edit-and-send buttons (`custom_id` grammar `v1:send:<id>` /
/// `v1:edit:<id>`).
pub fn staged_preview(
    label: &str,
    prompt: &str,
    action_id: i64,
    operator_id: u64,
) -> OutgoingMessage {
    let label = sanitize_label(label);
    let content = format!(
        "**Ready to run: {label}**\n```\n{}\n```",
        fence_neutralize(prompt)
    );
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
        ],
        mentioned_users: vec![operator_id],
    }
}

/// Rows per history page. Five keeps the reset buttons plus paging well under
/// Discord's 25-component cap.
pub const HISTORY_PAGE_SIZE: usize = 5;

/// Render one page of the P8 history: live commits are reset-eligible and show
/// a `Reset here` button (`v1:reset:<sha>`); manual commits, orphaned actions
/// and DB-only rows are shown with their provenance (spec P8, FR-25).
pub fn history_page(
    dir: &str,
    rows: &[HistoryRow],
    page: usize,
    show_reset: bool,
    operator_id: u64,
) -> OutgoingMessage {
    let total = rows.len().div_ceil(HISTORY_PAGE_SIZE).max(1);
    let page = page.min(total - 1);
    let start = page * HISTORY_PAGE_SIZE;
    let end = (start + HISTORY_PAGE_SIZE).min(rows.len());

    let mut content = format!(
        "**History — `{}`**  ({} rows, page {}/{})\n",
        inline_safe(dir),
        rows.len(),
        page + 1,
        total
    );
    let mut components = Vec::new();
    for row in &rows[start..end] {
        content.push_str(&format_history_row(row));
        content.push('\n');
        if show_reset
            && row.live
            && let Some(sha) = row.sha.as_deref()
        {
            components.push(component_button(
                format!("Reset {}", row.short_sha().unwrap_or_default()),
                format!("v1:reset:{sha}"),
                ButtonKind::Secondary,
            ));
        }
    }
    if total > 1 {
        if page > 0 {
            components.push(component_button(
                "◀ Prev",
                format!("v1:page:history:{}", page - 1),
                ButtonKind::Secondary,
            ));
        }
        if page + 1 < total {
            components.push(component_button(
                "Next ▶",
                format!("v1:page:history:{}", page + 1),
                ButtonKind::Secondary,
            ));
        }
    }

    OutgoingMessage {
        content,
        attachment: None,
        components,
        mentioned_users: vec![operator_id],
    }
}

fn format_history_row(row: &HistoryRow) -> String {
    let sha = row.short_sha().unwrap_or_else(|| "-------".to_owned());
    match row.kind {
        HistoryKind::Commit => format!(
            "`{sha}` ✅ **{}** — {}",
            inline_safe(row.label.as_deref().unwrap_or("?")),
            inline_safe(&row.subject)
        ),
        HistoryKind::Manual => format!("`{sha}` ✎ manual — {}", inline_safe(&row.subject)),
        HistoryKind::Orphaned => {
            let branch = row.branch.as_deref().map_or(String::new(), |branch| {
                format!(" · branch `{}`", inline_safe(branch))
            });
            format!(
                "`{sha}` ⚠ **{}** (orphaned){branch}",
                inline_safe(row.label.as_deref().unwrap_or("?"))
            )
        }
        HistoryKind::DbOnly => format!(
            "• **{}** ({})",
            inline_safe(row.label.as_deref().unwrap_or("?")),
            row.state.map_or("?", |state| state.as_str())
        ),
    }
}

/// Neutralize a value for inline (non-fenced) markdown: collapse whitespace and
/// drop backticks so a label cannot break the row structure.
fn inline_safe(text: &str) -> String {
    sanitize_label(text).replace('`', "'")
}

#[cfg(test)]
mod tests {
    use super::{
        DISCORD_MESSAGE_CAP, Footer, Outcome, ReportContext, TokenCounts, decision_buttons,
        fence_neutralize, report, split_excerpt, staged_preview,
    };

    fn done_context(action_id: i64, is_git: bool) -> ReportContext {
        ReportContext {
            action_id,
            is_git,
            operator_id: 42,
        }
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
        let message = report(
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
        let message = report(
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
    fn preview_carries_send_and_edit_buttons() {
        let preview = staged_preview("poke", "do the thing", 123, 9);
        assert_eq!(preview.components.len(), 2);
        assert_eq!(preview.components[0].custom_id, "v1:send:123");
        assert_eq!(preview.components[1].custom_id, "v1:edit:123");
        assert_eq!(preview.mentioned_users, vec![9]);
    }

    #[test]
    fn label_cannot_inject_markdown() {
        let message = report(
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
        assert_eq!(buttons.len(), 2);
        assert_eq!(buttons[0].custom_id, "v1:accept:11");
        assert_eq!(buttons[0].label, "Accept");
        assert!(!buttons[0].disabled);
        assert_eq!(buttons[1].custom_id, "v1:park:11");
        assert!(!buttons[1].disabled);
    }

    #[test]
    fn non_git_completed_report_marks_done_and_disables_park() {
        let message = report(
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
    fn errored_report_carries_park_and_discard() {
        let buttons = decision_buttons(
            &Outcome::Errored {
                error: "boom".to_owned(),
            },
            &done_context(13, true),
        );
        assert_eq!(buttons[0].custom_id, "v1:park:13");
        assert_eq!(buttons[1].custom_id, "v1:discard:13");
    }
}
