//! The P8 history merge as table-driven cases, plus the paged renderer.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

use ocbot_core::history::{HistoryAction, HistoryKind, merge};
use ocbot_core::ports::CommitRow;
use ocbot_core::render::HISTORY_PAGE_SIZE;
use ocbot_core::{State, history_page};

fn commit(sha: &str, subject: &str, timestamp: i64) -> CommitRow {
    CommitRow {
        sha: sha.to_owned(),
        subject: subject.to_owned(),
        timestamp,
    }
}

fn action(id: i64, label: &str, state: State, sha: Option<&str>) -> HistoryAction {
    HistoryAction {
        id,
        label: label.to_owned(),
        prompt: format!("prompt {id}"),
        session_id: Some(format!("ses_{id}")),
        state,
        accept_sha: sha.map(str::to_owned),
        branch: None,
    }
}

#[test]
fn commit_action_manual_orphaned_and_db_only_rows_coexist() {
    let log = vec![commit("bbb", "second", 20), commit("aaa", "first", 10)];
    let actions = vec![
        action(1, "old accept", State::Accepted, Some("aaa")),
        action(2, "lost", State::Accepted, Some("dead")),
        action(3, "pending", State::Reported, None),
    ];
    let rows = merge(&log, &actions, "bbb", |sha| sha == "aaa");

    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0].kind, HistoryKind::Manual); // bbb: manual
    assert_eq!(rows[1].kind, HistoryKind::Commit); // aaa: action 1, live
    assert_eq!(rows[2].kind, HistoryKind::Orphaned); // dead
    assert!(!rows[2].live);
    assert_eq!(rows[3].kind, HistoryKind::DbOnly);
    assert_eq!(rows[3].state, Some(State::Reported));
}

#[test]
fn history_page_pages_and_carries_reset_buttons() {
    let log: Vec<CommitRow> = (0..(HISTORY_PAGE_SIZE + 2))
        .map(|index| {
            commit(
                &format!("{index:040}"),
                &format!("commit {index}"),
                10 + i64::try_from(index).unwrap(),
            )
        })
        .collect();
    let rows = merge(&log, &[], "head", |_| true);

    let page0 = history_page("/repo", &rows, 0, true, 42);
    assert!(page0.content.contains("page 1/"));
    assert_eq!(
        page0
            .components
            .iter()
            .filter(|component| component.custom_id.starts_with("v1:reset:"))
            .count(),
        HISTORY_PAGE_SIZE,
        "one reset button per live commit"
    );
    assert!(
        page0
            .components
            .iter()
            .any(|component| component.custom_id == "v1:page:history:1"),
        "next-page button present"
    );

    let page1 = history_page("/repo", &rows, 1, true, 42);
    assert!(page1.content.contains("page 2/"));
    assert!(
        page1
            .components
            .iter()
            .any(|component| component.custom_id == "v1:page:history:0"),
        "prev-page button present"
    );
}
