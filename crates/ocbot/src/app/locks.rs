//! Directory locks (spec P5). The **acquire** happens atomically with the
//! `staged→running` CAS inside the store's `begin_send` transaction; this
//! module owns the read side (what `/status` and, later, `/release` see).

use crate::adapters::store::{ProjectRow, StoreError, StoreHandle};

/// The derived lock state of a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockState {
    /// No lock held.
    Free,
    /// Held by a bot-run action.
    HeldByAction {
        /// The action rowid.
        action_id: i64,
        /// Unix seconds when the lock was taken.
        since: i64,
    },
    /// Held by an external (TUI) session observed busy in the directory.
    HeldByExternal {
        /// The external session id.
        session: String,
        /// Unix seconds when the lock was taken.
        since: i64,
    },
}

/// Read the lock state of a directory.
pub async fn lock_state(store: &StoreHandle, dir: &str) -> Result<LockState, StoreError> {
    Ok(lock_state_from_row(store.project(dir).await?))
}

fn lock_state_from_row(row: Option<ProjectRow>) -> LockState {
    let Some(row) = row else {
        return LockState::Free;
    };
    let Some(holder) = row.lock_holder else {
        return LockState::Free;
    };
    let since = row.lock_since.unwrap_or_default();
    if let Ok(action_id) = holder.parse::<i64>() {
        return LockState::HeldByAction { action_id, since };
    }
    if let Some(session) = holder.strip_prefix("external:") {
        return LockState::HeldByExternal {
            session: session.to_owned(),
            since,
        };
    }
    LockState::Free
}

/// A single-line human description of a lock for `/status` output.
pub fn describe(lock: &ProjectRow) -> String {
    format!(
        "{} held by {} since {}",
        lock.dir,
        lock.lock_holder.as_deref().unwrap_or("?"),
        lock.lock_since.unwrap_or_default()
    )
}

#[cfg(test)]
mod tests {
    use super::{LockState, lock_state_from_row};
    use crate::adapters::store::ProjectRow;

    fn row() -> ProjectRow {
        ProjectRow {
            dir: "/tmp/x".to_owned(),
            work_branch: Some("main".to_owned()),
            base_sha: None,
            active_session_id: None,
            lock_holder: None,
            lock_since: None,
        }
    }

    #[test]
    fn free_when_no_holder() {
        assert_eq!(lock_state_from_row(Some(row())), LockState::Free);
        assert_eq!(lock_state_from_row(None), LockState::Free);
    }

    #[test]
    fn action_holder_is_extracted() {
        let mut row = row();
        row.lock_holder = Some("17".to_owned());
        row.lock_since = Some(100);
        assert_eq!(
            lock_state_from_row(Some(row)),
            LockState::HeldByAction {
                action_id: 17,
                since: 100
            }
        );
    }

    #[test]
    fn external_holder_is_extracted() {
        let mut row = row();
        row.lock_holder = Some("external:ses_abc".to_owned());
        assert_eq!(
            lock_state_from_row(Some(row)),
            LockState::HeldByExternal {
                session: "ses_abc".to_owned(),
                since: 0
            }
        );
    }
}
