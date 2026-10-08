//! The P8 history merge: a `git log` with sticky notes.
//!
//! Facts about *what is committed and in what order* come only from git; the DB
//! supplies what git cannot know (labels, prompts, sessions, decisions). This
//! module is a pure left-join of the two, plus a liveness annotation. It never
//! reads the DB or runs git — the shell supplies both.

use crate::ports::CommitRow;
use crate::state::State;

/// The DB-side half of a history row (git cannot know these fields).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryAction {
    /// The action row id.
    pub id: i64,
    /// The operator label.
    pub label: String,
    /// The stored prompt (rendered verbatim at send, P13).
    pub prompt: String,
    /// The session, when bound.
    pub session_id: Option<String>,
    /// The current (or final) machine state.
    pub state: State,
    /// The accepted commit sha, when the action was accepted.
    pub accept_sha: Option<String>,
    /// A `rejected/*` or `superseded/*` branch associated with the attempt.
    pub branch: Option<String>,
}

/// How a history row is sourced (drives the display and reset eligibility).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryKind {
    /// A commit with a matching accepted action: live and reset-eligible.
    Commit,
    /// A commit with no matching action ("manual").
    Manual,
    /// An accepted action whose sha is no longer reachable from HEAD.
    Orphaned,
    /// A DB-only action with no commit (staged/reported/parked/...).
    DbOnly,
}

/// One rendered history row.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryRow {
    /// The row source.
    pub kind: HistoryKind,
    /// The commit sha, when this row is about a commit.
    pub sha: Option<String>,
    /// The commit subject, otherwise the label.
    pub subject: String,
    /// The commit timestamp, when known.
    pub timestamp: Option<i64>,
    /// The action label, when this row has an action.
    pub label: Option<String>,
    /// The action prompt, when this row has an action.
    pub prompt: Option<String>,
    /// The session id, when this row has an action.
    pub session_id: Option<String>,
    /// The action state, when this row has an action.
    pub state: Option<State>,
    /// Whether the row is live (a legitimate reset target).
    pub live: bool,
    /// The associated rejected/superseded branch, when known.
    pub branch: Option<String>,
}

impl HistoryRow {
    /// A short (7-char) sha for display.
    pub fn short_sha(&self) -> Option<String> {
        self.sha
            .as_deref()
            .map(|sha| sha.chars().take(7).collect::<String>())
    }
}

/// Left-join `log_rows` (newest first, as git emits) with `actions` on
/// `accept_sha`, then append orphaned commits and DB-only rows.
///
/// `is_ancestor(sha)` must answer whether `sha` is reachable from HEAD; the
/// shell pre-computes this with `git merge-base --is-ancestor` so this function
/// stays pure.
pub fn merge(
    log_rows: &[CommitRow],
    actions: &[HistoryAction],
    _head: &str,
    is_ancestor: impl Fn(&str) -> bool,
) -> Vec<HistoryRow> {
    let mut matched = vec![false; actions.len()];
    let mut rows = Vec::with_capacity(log_rows.len() + actions.len());

    for commit in log_rows {
        let found = actions.iter().enumerate().find(|(index, action)| {
            !matched[*index] && action.accept_sha.as_deref() == Some(commit.sha.as_str())
        });
        match found {
            Some((index, action)) => {
                matched[index] = true;
                rows.push(HistoryRow {
                    kind: HistoryKind::Commit,
                    sha: Some(commit.sha.clone()),
                    subject: commit.subject.clone(),
                    timestamp: Some(commit.timestamp),
                    label: Some(action.label.clone()),
                    prompt: Some(action.prompt.clone()),
                    session_id: action.session_id.clone(),
                    state: Some(action.state),
                    live: true,
                    branch: action.branch.clone(),
                });
            }
            None => rows.push(HistoryRow {
                kind: HistoryKind::Manual,
                sha: Some(commit.sha.clone()),
                subject: commit.subject.clone(),
                timestamp: Some(commit.timestamp),
                label: None,
                prompt: None,
                session_id: None,
                state: None,
                live: true,
                branch: None,
            }),
        }
    }

    for (index, action) in actions.iter().enumerate() {
        if matched[index] {
            continue;
        }
        match action.accept_sha.as_deref() {
            Some(sha) => {
                let live = is_ancestor(sha);
                rows.push(HistoryRow {
                    kind: if live {
                        HistoryKind::Commit
                    } else {
                        HistoryKind::Orphaned
                    },
                    sha: Some(sha.to_owned()),
                    subject: action.label.clone(),
                    timestamp: None,
                    label: Some(action.label.clone()),
                    prompt: Some(action.prompt.clone()),
                    session_id: action.session_id.clone(),
                    state: Some(action.state),
                    live,
                    branch: action.branch.clone(),
                });
            }
            None => rows.push(HistoryRow {
                kind: HistoryKind::DbOnly,
                sha: None,
                subject: action.label.clone(),
                timestamp: None,
                label: Some(action.label.clone()),
                prompt: Some(action.prompt.clone()),
                session_id: action.session_id.clone(),
                state: Some(action.state),
                live: false,
                branch: action.branch.clone(),
            }),
        }
    }

    rows
}

#[cfg(test)]
mod tests {
    use super::{HistoryAction, HistoryKind, merge};
    use crate::ports::CommitRow;
    use crate::state::State;

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
    fn commit_with_action_is_live() {
        let rows = merge(
            &[commit("aaa", "feat: x", 10)],
            &[action(1, "label", State::Accepted, Some("aaa"))],
            "aaa",
            |_| true,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, HistoryKind::Commit);
        assert_eq!(rows[0].label.as_deref(), Some("label"));
        assert!(rows[0].live);
    }

    #[test]
    fn commit_without_action_is_manual_and_resettable() {
        let rows = merge(&[commit("aaa", "manual work", 10)], &[], "aaa", |_| true);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, HistoryKind::Manual);
        assert!(rows[0].live);
        assert!(rows[0].label.is_none());
    }

    #[test]
    fn unreachable_sha_is_orphaned() {
        let rows = merge(
            &[],
            &[action(7, "lost", State::Accepted, Some("dead"))],
            "head",
            |_| false,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, HistoryKind::Orphaned);
        assert!(!rows[0].live);
        assert_eq!(rows[0].sha.as_deref(), Some("dead"));
    }

    #[test]
    fn action_without_sha_is_db_only() {
        let rows = merge(
            &[],
            &[action(9, "pending", State::Reported, None)],
            "head",
            |_| true,
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].kind, HistoryKind::DbOnly);
        assert!(!rows[0].live);
        assert!(rows[0].sha.is_none());
    }

    #[test]
    fn mixed_rows_preserve_git_order() {
        let rows = merge(
            &[commit("bbb", "second", 20), commit("aaa", "first", 10)],
            &[
                action(1, "old", State::Accepted, Some("aaa")),
                action(2, "pending", State::Reported, None),
            ],
            "bbb",
            |sha| sha == "aaa",
        );
        // git order first, then the DB-only tail.
        assert_eq!(rows[0].sha.as_deref(), Some("bbb"));
        assert_eq!(rows[0].kind, HistoryKind::Manual);
        assert_eq!(rows[1].sha.as_deref(), Some("aaa"));
        assert_eq!(rows[1].kind, HistoryKind::Commit);
        assert_eq!(rows[2].kind, HistoryKind::DbOnly);
    }

    #[test]
    fn live_sha_not_in_the_log_range_still_renders_as_commit() {
        // An accepted action whose base range excluded it, but which is still
        // reachable from HEAD, is a live commit (reset-eligible).
        let rows = merge(
            &[],
            &[action(3, "older accept", State::Accepted, Some("ccc"))],
            "head",
            |_| true,
        );
        assert_eq!(rows[0].kind, HistoryKind::Commit);
        assert!(rows[0].live);
    }

    #[test]
    fn short_sha_is_the_seven_char_prefix() {
        let rows = merge(
            &[commit("abcdef1234567890", "subject", 1)],
            &[],
            "abcdef1234567890",
            |_| true,
        );
        assert_eq!(rows[0].short_sha().as_deref(), Some("abcdef1"));
        // A DB-only row has no sha to shorten.
        let rows = merge(&[], &[action(1, "x", State::Reported, None)], "h", |_| true);
        assert_eq!(rows[0].short_sha(), None);
    }
}
