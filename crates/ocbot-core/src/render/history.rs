//! The P8 history page: commits with provenance and reset affordances. The
//! header names the project by its opaque display name only (SPEC-005 G-01 —
//! the `/ls`-era raw directory was removed from user-facing chrome).

use std::path::Path;

use crate::history::{HistoryKind, HistoryRow};
use crate::paths::project_display_name;

use super::label::inline_safe;
use super::{ButtonKind, OutgoingMessage, component_button};

/// Rows per history page. Five keeps the reset buttons plus paging well under
/// Discord's 25-component cap.
pub const HISTORY_PAGE_SIZE: usize = 5;

/// Render one page of the P8 history: live commits are reset-eligible and show
/// a `Reset here` button (`v1:reset:<sha>`); manual commits, orphaned actions
/// and DB-only rows are shown with their provenance (spec P8, FR-25). `dir`
/// is internal — only its display name reaches the message.
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
        project_display_name(Path::new(dir)),
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
    // No outer `total > 1` guard: each arrow is already conditioned by its
    // own bounds, so a single page renders neither.
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
    use super::{HISTORY_PAGE_SIZE, OutgoingMessage, history_page};
    use crate::State;
    use crate::history::{HistoryKind, HistoryRow};
    use crate::paths::project_display_name;

    fn commit(index: usize) -> HistoryRow {
        HistoryRow {
            kind: HistoryKind::Commit,
            sha: Some(format!("{index:040}")),
            subject: format!("commit {index}"),
            timestamp: Some(1_700_000_000),
            label: Some(format!("task {index}")),
            prompt: None,
            session_id: Some(format!("ses_{index}")),
            state: Some(State::Reported),
            live: true,
            branch: None,
        }
    }

    fn button_ids(message: &OutgoingMessage) -> Vec<String> {
        message
            .components
            .iter()
            .map(|component| component.custom_id.clone())
            .collect()
    }

    #[test]
    fn history_header_names_the_project_not_its_path() {
        let rows: Vec<HistoryRow> = (0..HISTORY_PAGE_SIZE + 2).map(commit).collect();
        let message = history_page("/home/lex/secret-workspace/repo", &rows, 0, true, 7);
        assert!(
            message
                .content
                .contains("**History — `secret-workspace/repo`**"),
            "{}",
            message.content
        );
        assert!(
            !message.content.contains("/home/lex"),
            "the absolute path stayed out: {}",
            message.content
        );
        assert!(
            message.content.contains(&format!("{} rows", rows.len())),
            "{}",
            message.content
        );
        assert!(
            button_ids(&message)
                .iter()
                .any(|id| id.starts_with("v1:reset:")),
            "live commits offer reset"
        );
        assert!(
            button_ids(&message).contains(&"v1:page:history:1".to_owned()),
            "a second page is offered"
        );
    }

    #[test]
    fn every_provenance_kind_renders_its_exact_row() {
        let rows = vec![
            HistoryRow {
                kind: HistoryKind::Manual,
                sha: Some("abcdef1234567890".to_owned()),
                subject: "hand edit".to_owned(),
                ..commit(9)
            },
            HistoryRow {
                kind: HistoryKind::Orphaned,
                sha: Some("beef000000000000".to_owned()),
                subject: "lost".to_owned(),
                label: Some("task-c".to_owned()),
                branch: Some("work/lost".to_owned()),
                live: false,
                ..commit(8)
            },
            HistoryRow {
                kind: HistoryKind::DbOnly,
                sha: None,
                subject: String::new(),
                label: Some("queued".to_owned()),
                state: Some(State::Staged),
                live: false,
                branch: None,
                timestamp: None,
                prompt: None,
                session_id: None,
            },
            commit(0),
        ];
        let message = history_page("/srv/team/repo", &rows, 0, true, 7);
        let content = &message.content;
        assert!(
            content.contains("`abcdef1` ✎ manual — hand edit"),
            "{content}"
        );
        assert!(
            content.contains("`beef000` ⚠ **task-c** (orphaned) · branch `work/lost`"),
            "{content}"
        );
        assert!(content.contains("• **queued** (staged)"), "{content}");
        assert!(
            content.contains("`0000000` ✅ **task 0** — commit 0"),
            "{content}"
        );
        // Only live rows are reset-eligible: the Commit and the (live-by-
        // construction Manual) row — never the orphaned or DB-only ones.
        let ids = button_ids(&message);
        assert_eq!(
            ids.iter().filter(|id| id.starts_with("v1:reset:")).count(),
            2,
            "commit + manual only: {ids:?}"
        );
        // Four rows fit one page: a single page renders neither arrow (this
        // is why the paging block needs no `total > 1` guard).
        assert!(
            ids.iter().all(|id| !id.starts_with("v1:page:history:")),
            "single page offers no arrows: {ids:?}"
        );
    }

    #[test]
    fn paging_walks_pages_sharp_and_clamps_past_the_end() {
        // 12 rows at HISTORY_PAGE_SIZE=5 => pages of 5, 5, 2.
        let rows: Vec<HistoryRow> = (0..12).map(commit).collect();

        // First page: next only, exactly the first five subjects, no reset
        // chrome when show_reset is off.
        let first = history_page("/x/repo", &rows, 0, false, 7);
        assert!(
            first.content.contains("(12 rows, page 1/3)"),
            "{}",
            first.content
        );
        for index in 0..5 {
            assert!(
                first.content.contains(&format!("commit {index}")),
                "page 1 {index}"
            );
        }
        assert!(!first.content.contains("commit 5"), "page 1 overran");
        let ids = button_ids(&first);
        assert!(ids.contains(&"v1:page:history:1".to_owned()), "{ids:?}");
        assert!(
            !ids.iter()
                .any(|id| id.starts_with("v1:page:history:") && id != "v1:page:history:1"),
            "no prev on the first page: {ids:?}"
        );
        assert!(
            !first.content.contains("Reset"),
            "show_reset=false renders none"
        );

        // Middle page: both arrows, correct slice 5..10.
        let middle = history_page("/x/repo", &rows, 1, true, 7);
        assert!(middle.content.contains("page 2/3"), "{}", middle.content);
        assert!(middle.content.contains("commit 5"), "start arithmetic");
        assert!(middle.content.contains("commit 9"), "end arithmetic");
        assert!(!middle.content.contains("commit 10"), "end overran");
        assert!(!middle.content.contains("commit 4\n"), "start under-ran");
        let ids = button_ids(&middle);
        assert!(
            ids.contains(&"v1:page:history:0".to_owned())
                && ids.contains(&"v1:page:history:2".to_owned()),
            "{ids:?}"
        );
        assert!(
            ids.iter()
                .any(|id| id == "v1:reset:0000000000000000000000000000000000000005"),
            "the page's commit offers reset: {ids:?}"
        );

        // Last page: prev only, final two rows.
        let last = history_page("/x/repo", &rows, 2, false, 7);
        assert!(last.content.contains("page 3/3"), "{}", last.content);
        assert!(last.content.contains("commit 10") && last.content.contains("commit 11"));
        assert!(
            !last.content.contains("commit 9"),
            "last page started too early"
        );
        let ids = button_ids(&last);
        assert!(ids.contains(&"v1:page:history:1".to_owned()), "{ids:?}");
        assert!(
            !ids.contains(&"v1:page:history:3".to_owned()),
            "no next past the last page: {ids:?}"
        );

        // A page past the end clamps to the last page rather than panicking
        // or rendering empty.
        let clamped = history_page("/x/repo", &rows, 99, false, 7);
        assert_eq!(
            clamped.content, last.content,
            "page 99 must clamp to the last page"
        );
    }

    #[test]
    fn deep_paths_collapse_to_two_components() {
        let rows = vec![commit(0)];
        let message = history_page(
            &std::path::Path::new("/srv")
                .join("teams")
                .join("alpha")
                .join("repo")
                .to_string_lossy(),
            &rows,
            0,
            false,
            7,
        );
        assert_eq!(
            message
                .content
                .strip_prefix("**History — `")
                .and_then(|rest| rest.split('`').next())
                .unwrap_or_default(),
            project_display_name(std::path::Path::new("/srv/teams/alpha/repo"))
        );
        assert!(
            button_ids(&message)
                .iter()
                .all(|id| !id.starts_with("v1:reset:")),
            "show_reset=false renders no reset buttons"
        );
    }
}
