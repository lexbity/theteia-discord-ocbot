//! Browser surfaces: the `/ls` directory browser (FR-34), the plan-step picker
//! (P12), and the P8 history page.

use crate::history::{HistoryKind, HistoryRow};
use crate::plan::{OTHER_OPTION_LABEL, PLAN_PAGE_SIZE, PickOption};

use super::label::inline_safe;
use super::{ButtonKind, OutgoingMessage, component_button};

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
        mention_allowlist: vec![operator_id],
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
        mention_allowlist: vec![operator_id],
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
        mention_allowlist: vec![operator_id],
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

#[cfg(test)]
mod tests {
    use super::{LS_PAGE_SIZE, LsEntry, ls_message, plan_picker_message};
    use crate::plan::PLAN_PAGE_SIZE;

    #[test]
    fn ls_message_pages_and_uses_global_indices() {
        let entries: Vec<LsEntry> = (0..LS_PAGE_SIZE + 2)
            .map(|index| LsEntry {
                name: format!("entry{index}"),
                is_dir: index % 2 == 0,
            })
            .collect();
        let first = ls_message("/root", &entries, 0, LS_PAGE_SIZE, 7);
        assert_eq!(first.components[0].custom_id, "v1:ls:up");
        assert!(
            first
                .components
                .iter()
                .any(|c| c.custom_id == "v1:ls:open:0")
        );
        assert!(first.components.iter().any(|c| c.custom_id == "v1:lssel:1"));
        assert!(first.components.iter().any(|c| c.custom_id == "v1:ls:p:1"));
        let second = ls_message("/root", &entries, 1, LS_PAGE_SIZE, 7);
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
        let message = plan_picker_message("task", &options, 0, 3, 7);
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
}
