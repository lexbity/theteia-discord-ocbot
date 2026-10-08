//! Report rendering, truncation, and fence neutralization (spec §6.6 P10).
//!
//! Handlers produce [`OutgoingMessage`] **specs** — pure data, serenity-free.
//! The Discord adapter is the only place that turns a spec into a payload, and
//! its single builder always sets `allowed_mentions { parse: [], users:
//! [OPERATOR] }` (FR-3, structurally enforced).

use std::fmt::Write as _;

use crate::gates::{FormFieldKind, FormPlaybook, GateKind, GateRequest, form_playbook};
use crate::history::{HistoryKind, HistoryRow};
use crate::outcome::Outcome;
use crate::outcome::TokenCounts;
use crate::plan::{OTHER_OPTION_LABEL, PLAN_PAGE_SIZE, PickOption};

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
    let label = sanitize_label(label);
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

/// One browser row (rendered from an fs `DirEntry`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LsEntry {
    /// The base name.
    pub name: String,
    /// Whether opening it descends into a directory.
    pub is_dir: bool,
}

/// Rows per `/ls` page. Leaves room for Up/Select/Prev/Next under Discord's
/// 25-component cap (P10).
pub const LS_PAGE_SIZE: usize = 18;

/// Render the `/ls` browser page (FR-34): an Up button, one Open (directory) or
/// Select (file) button per entry, a "select this directory" action, and paging.
///
/// Selections carry only the global entry *index*; the handler re-lists the
/// directory and resolves it (`v1:ls:open:<idx>` / `v1:lssel:<idx>`), so no UI
/// state lives in memory (D-6).
pub fn ls_message(
    cwd: &str,
    entries: &[LsEntry],
    page: usize,
    page_size: usize,
    operator_id: u64,
) -> OutgoingMessage {
    let page_size = page_size.max(1);
    let total_pages = entries.len().div_ceil(page_size).max(1);
    let page = page.min(total_pages - 1);
    let start = page * page_size;
    let end = (start + page_size).min(entries.len());

    let content = format!(
        "**Browse — `{}`**\n{} entries · page {}/{}\n_open a directory or select a file; its path is kept for the next prompt._",
        inline_safe(cwd),
        entries.len(),
        page + 1,
        total_pages
    );
    let mut components = vec![component_button(
        "⬆ Up",
        "v1:ls:up".to_owned(),
        ButtonKind::Secondary,
    )];
    for (offset, entry) in entries[start..end].iter().enumerate() {
        let index = start + offset;
        let name: String = inline_safe(&entry.name).chars().take(60).collect();
        if entry.is_dir {
            components.push(component_button(
                format!("📁 {name}"),
                format!("v1:ls:open:{index}"),
                ButtonKind::Primary,
            ));
        } else {
            components.push(component_button(
                format!("📄 {name}"),
                format!("v1:lssel:{index}"),
                ButtonKind::Secondary,
            ));
        }
    }
    components.push(component_button(
        "✓ Select this directory",
        "v1:ls:use".to_owned(),
        ButtonKind::Success,
    ));
    if page > 0 {
        components.push(component_button(
            "◀ Prev",
            format!("v1:ls:p:{}", page - 1),
            ButtonKind::Secondary,
        ));
    }
    if page + 1 < total_pages {
        components.push(component_button(
            "Next ▶",
            format!("v1:ls:p:{}", page + 1),
            ButtonKind::Secondary,
        ));
    }

    OutgoingMessage {
        content,
        attachment: None,
        components,
        mentioned_users: vec![operator_id],
    }
}

/// Render the plan-step picker (P12): one button per option on this page, paging,
/// and the always-present "Other: type it" / "Use typed label" affordances.
pub fn plan_picker_message(
    label: &str,
    options: &[PickOption],
    page: usize,
    action_id: i64,
    operator_id: u64,
) -> OutgoingMessage {
    let page_data = crate::plan::plan_page(options, page, PLAN_PAGE_SIZE);
    let mut content = format!(
        "**Plan steps — {}**\n{} option(s) · page {}/{}\n_pick a step to prefill the label; the prompt is still yours to edit._",
        inline_safe(label),
        options.len(),
        page_data.page + 1,
        page_data.total_pages
    );
    let mut components = Vec::new();
    for (offset, option) in page_data.options.iter().enumerate() {
        let index = page_data.page * PLAN_PAGE_SIZE + offset;
        let button_label: String = inline_safe(&option.label).chars().take(60).collect();
        components.push(component_button(
            button_label,
            format!("v1:plan:{action_id}:{index}"),
            ButtonKind::Primary,
        ));
    }
    if page_data.has_prev() {
        components.push(component_button(
            "◀ Prev",
            format!("v1:planp:{action_id}:{}", page_data.page - 1),
            ButtonKind::Secondary,
        ));
    }
    if page_data.has_next() {
        components.push(component_button(
            "Next ▶",
            format!("v1:planp:{action_id}:{}", page_data.page + 1),
            ButtonKind::Secondary,
        ));
    }
    components.push(component_button(
        OTHER_OPTION_LABEL,
        format!("v1:planother:{action_id}"),
        ButtonKind::Secondary,
    ));
    components.push(component_button(
        "Use typed label",
        format!("v1:planuse:{action_id}"),
        ButtonKind::Secondary,
    ));
    // Content lists the section context through the buttons; keep the header
    // short but non-empty (Discord rejects empty content without components).
    content.push_str("\n_options are the plan's list items under the chosen headings._");
    OutgoingMessage {
        content,
        attachment: None,
        components,
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

/// The gate message (FR-20/FR-41): a permission request gets one-tap decision
/// buttons; a renderable form gets an Answer button (plus per-option quick
/// buttons for a single-choice form); anything else renders the *raw payload*
/// verbatim with no buttons (D-R5 surface-only — the bot never guesses at an
/// unknown wire format).
pub fn gate_message(
    label: Option<&str>,
    gate: &GateRequest,
    rowid: i64,
    operator_id: u64,
) -> OutgoingMessage {
    let caller = label.map_or_else(|| "?".to_owned(), inline_safe);
    match &gate.kind {
        GateKind::Permission {
            action,
            resources,
            message,
        } => {
            let mut content = format!("🔒 **{caller}** — permission needed\n");
            let _ = writeln!(content, "tool: `{}`", inline_safe(action));
            for resource in resources {
                let _ = writeln!(content, "  `{}`", inline_safe(resource));
            }
            if let Some(message) = message {
                let _ = write!(content, "_{}_", inline_safe(message));
            }
            OutgoingMessage {
                content,
                attachment: None,
                components: vec![
                    component_button("Once", format!("v1:gate:{rowid}:once"), ButtonKind::Success),
                    component_button(
                        "Always",
                        format!("v1:gate:{rowid}:always"),
                        ButtonKind::Primary,
                    ),
                    component_button(
                        "Reject",
                        format!("v1:gate:{rowid}:reject"),
                        ButtonKind::Danger,
                    ),
                ],
                mentioned_users: vec![operator_id],
            }
        }
        GateKind::Form { title, fields } => {
            if let Some(playbook) = form_playbook(fields) {
                let mut content = format!("📋 **{caller}** — {}\n", inline_safe(title));
                for field in fields {
                    let label = field.label.as_deref().map(inline_safe).unwrap_or_default();
                    let label: String = label.chars().take(80).collect();
                    let _ = writeln!(
                        content,
                        "• `{}` · {} {}",
                        inline_safe(&field.key),
                        describe_kind(&field.kind),
                        label
                    );
                }
                let components = match playbook {
                    FormPlaybook::SingleChoice(options) => options
                        .iter()
                        .map(|option| {
                            component_button_state(
                                inline_safe(&option.label),
                                format!("v1:formpick:{rowid}:{}", option.value),
                                ButtonKind::Secondary,
                                false,
                            )
                        })
                        .collect(),
                    FormPlaybook::Modal => vec![component_button(
                        "✍ Answer form",
                        format!("v1:formgo:{rowid}"),
                        ButtonKind::Primary,
                    )],
                };
                OutgoingMessage {
                    content,
                    attachment: None,
                    components,
                    mentioned_users: vec![operator_id],
                }
            } else {
                let text = serde_json::to_string_pretty(&gate.payload)
                    .unwrap_or_else(|_| gate.payload.to_string());
                OutgoingMessage {
                    content: format!(
                        "📋 **{caller}** — form (answer in the TUI)\n```json\n{}\n```",
                        fence_neutralize(&text)
                    ),
                    attachment: None,
                    components: Vec::new(),
                    mentioned_users: vec![operator_id],
                }
            }
        }
    }
}

/// One-line description of a form field's answer kind (message body text).
fn describe_kind(kind: &FormFieldKind) -> String {
    match kind {
        FormFieldKind::Text => "text".to_owned(),
        FormFieldKind::Choice(options) => {
            let options: Vec<String> = options
                .iter()
                .map(|option| inline_safe(&option.label))
                .collect();
            format!("choice: {}", options.join(", "))
        }
        FormFieldKind::Boolean => "yes/no".to_owned(),
        FormFieldKind::Multiselect(_) => "multi-choice".to_owned(),
        FormFieldKind::External(_) => "external".to_owned(),
        FormFieldKind::Unknown => "unknown".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DISCORD_MESSAGE_CAP, Footer, LS_PAGE_SIZE, Outcome, PLAN_PAGE_SIZE, ReportContext,
        TokenCounts, decision_buttons, fence_neutralize, gate_message, report, split_excerpt,
        staged_preview,
    };
    use crate::gates::{
        FormField, FormFieldKind, FormOption, GateKind, GateRequest, MAX_FORM_FIELDS,
    };

    fn done_context(action_id: i64, is_git: bool) -> ReportContext {
        ReportContext {
            action_id,
            is_git,
            operator_id: 42,
            followups: 0,
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
    fn ls_message_pages_and_uses_global_indices() {
        let entries: Vec<super::LsEntry> = (0..LS_PAGE_SIZE + 2)
            .map(|index| super::LsEntry {
                name: format!("entry{index}"),
                is_dir: index % 2 == 0,
            })
            .collect();
        let first = super::ls_message("/root", &entries, 0, LS_PAGE_SIZE, 7);
        assert_eq!(first.components[0].custom_id, "v1:ls:up");
        assert!(
            first
                .components
                .iter()
                .any(|c| c.custom_id == "v1:ls:open:0")
        );
        assert!(first.components.iter().any(|c| c.custom_id == "v1:lssel:1"));
        assert!(first.components.iter().any(|c| c.custom_id == "v1:ls:p:1"));
        let second = super::ls_message("/root", &entries, 1, LS_PAGE_SIZE, 7);
        // The first entry of page 1 carries the global index LS_PAGE_SIZE.
        assert!(
            second
                .components
                .iter()
                .any(|c| c.custom_id == format!("v1:ls:open:{LS_PAGE_SIZE}")),
            "global index on page 2"
        );
    }

    #[test]
    fn plan_picker_pages_and_always_offers_other() {
        let options: Vec<crate::plan::PickOption> = (0..PLAN_PAGE_SIZE + 5)
            .map(|index| crate::plan::PickOption {
                label: format!("step {index}"),
                section: None,
            })
            .collect();
        let message = super::plan_picker_message("task", &options, 0, 3, 7);
        assert!(
            message
                .components
                .iter()
                .any(|c| c.custom_id == "v1:plan:3:0")
        );
        assert!(
            message
                .components
                .iter()
                .any(|c| c.custom_id == "v1:planp:3:1")
        );
        assert!(
            message
                .components
                .iter()
                .any(|c| c.custom_id == "v1:planother:3")
        );
        assert!(
            message
                .components
                .iter()
                .any(|c| c.custom_id == "v1:planuse:3")
        );
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
        let done = report(
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
        let fresh = report(
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

    fn field(key: &str, kind: FormFieldKind) -> FormField {
        FormField {
            key: key.to_owned(),
            label: None,
            kind,
        }
    }

    fn gate_form(fields: Vec<FormField>) -> GateRequest {
        GateRequest {
            id: "frm_x".to_owned(),
            session_id: Some("ses_x".to_owned()),
            kind: GateKind::Form {
                title: "Which one?".to_owned(),
                fields,
            },
            payload: serde_json::json!({ "id": "frm_x", "title": "Which one?" }),
        }
    }

    #[test]
    fn gate_message_renders_permission_decision_buttons() {
        let gate = GateRequest {
            id: "per_x".to_owned(),
            session_id: Some("ses_x".to_owned()),
            kind: GateKind::Permission {
                action: "bash".to_owned(),
                resources: vec!["rm -rf /tmp/x".to_owned()],
                message: None,
            },
            payload: serde_json::json!({}),
        };
        let message = gate_message(Some("poke"), &gate, 7, 42);
        assert!(message.content.contains("bash"));
        assert_eq!(
            message
                .components
                .iter()
                .map(|component| component.custom_id.as_str())
                .collect::<Vec<_>>(),
            vec!["v1:gate:7:once", "v1:gate:7:always", "v1:gate:7:reject"]
        );
        assert_eq!(message.mentioned_users, vec![42]);
    }

    #[test]
    fn gate_message_renders_renderable_form_modal_button() {
        let gate = gate_form(vec![
            field(
                "approach",
                FormFieldKind::Choice(vec![
                    FormOption {
                        value: "a".to_owned(),
                        label: "A".to_owned(),
                    },
                    FormOption {
                        value: "b".to_owned(),
                        label: "B".to_owned(),
                    },
                ]),
            ),
            field("note", FormFieldKind::Text),
        ]);
        let message = gate_message(Some("poke"), &gate, 8, 42);
        assert!(
            message
                .components
                .iter()
                .any(|component| component.custom_id == "v1:formgo:8"),
            "renderable form carries the answer button"
        );
    }

    #[test]
    fn gate_message_renders_single_choice_form_buttons() {
        let gate = gate_form(vec![field(
            "approach",
            FormFieldKind::Choice(vec![FormOption {
                value: "a".to_owned(),
                label: "A".to_owned(),
            }]),
        )]);
        let message = gate_message(None, &gate, 9, 42);
        assert!(
            message
                .components
                .iter()
                .any(|component| component.custom_id == "v1:formpick:9:a"),
            "single-choice form answers by button"
        );
    }

    #[test]
    fn gate_message_renders_unrenderable_form_verbatim_without_buttons() {
        let fields: Vec<FormField> = (0..=MAX_FORM_FIELDS)
            .map(|index| field(&format!("f{index}"), FormFieldKind::Text))
            .collect();
        let gate = gate_form(fields);
        let message = gate_message(Some("poke"), &gate, 10, 42);
        assert!(message.components.is_empty(), "no buttons for surface-only");
        assert!(message.content.contains("answer in the TUI"));
        assert!(message.content.contains("frm_x"));
    }

    #[test]
    fn gate_message_renders_unknown_field_form_surface_only() {
        let gate = gate_form(vec![field("mystery", FormFieldKind::Unknown)]);
        let message = gate_message(None, &gate, 11, 42);
        assert!(message.components.is_empty());
        assert!(message.content.contains("answer in the TUI"));
    }
}
