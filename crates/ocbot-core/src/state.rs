//! The action state machine (spec §6.4).
//!
//! The outer `match` in [`transition`] has **no wildcard arm**: adding a
//! `State` variant is a compile error until the transition table is reviewed.
//! Inner arms that fall through to `None` are covered by the property tests in
//! `tests/state_props.rs`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A non-mutable reflection-safe state namespace.
pub const ALL_STATES: [State; 10] = [
    State::Staged,
    State::Running,
    State::Reported,
    State::Accepting,
    State::Accepted,
    State::Parking,
    State::Parked,
    State::Errored,
    State::Aborted,
    State::Discarded,
];

/// The lifecycle of one bot-run action (spec P6 / §6.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
pub enum State {
    /// Prompt drafted, not yet sent.
    Staged,
    /// Prompt admitted to opencode; the agent is executing.
    Running,
    /// Idle; final report delivered. A decision is owed (Phase 3+).
    Reported,
    /// Committing accepted work (write-ahead intent persisted).
    Accepting,
    /// Committed; sealed.
    Accepted,
    /// Parking an attempt (commit → branch → reset).
    Parking,
    /// Parked; sealed.
    Parked,
    /// Idle with a failed outcome; re-decision owed.
    Errored,
    /// Idle after an abort; re-decision owed.
    Aborted,
    /// Sealed without a commit.
    Discarded,
}

impl State {
    /// The wire/db spelling of the state.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Staged => "staged",
            State::Running => "running",
            State::Reported => "reported",
            State::Accepting => "accepting",
            State::Accepted => "accepted",
            State::Parking => "parking",
            State::Parked => "parked",
            State::Errored => "errored",
            State::Aborted => "aborted",
            State::Discarded => "discarded",
        }
    }

    /// Terminal states are sealed: no event may move them (P6).
    pub fn is_terminal(self) -> bool {
        matches!(self, State::Accepted | State::Parked | State::Discarded)
    }
}

impl fmt::Display for State {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for State {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "staged" => Ok(State::Staged),
            "running" => Ok(State::Running),
            "reported" => Ok(State::Reported),
            "accepting" => Ok(State::Accepting),
            "accepted" => Ok(State::Accepted),
            "parking" => Ok(State::Parking),
            "parked" => Ok(State::Parked),
            "errored" => Ok(State::Errored),
            "aborted" => Ok(State::Aborted),
            "discarded" => Ok(State::Discarded),
            other => Err(format!("unknown state {other:?}")),
        }
    }
}

/// Events that move the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum Event {
    /// The operator approved sending the staged prompt.
    Send,
    /// The staged draft was discarded.
    Discard,
    /// The session went idle with a healthy last message.
    IdleOk,
    /// The session went idle with an errored last message.
    IdleErr,
    /// An interrupt was acknowledged by the server.
    AbortOk,
    /// The operator chose to follow up in the same session.
    FollowUp,
    /// The operator chose to accept the work.
    Accept,
    /// The operator chose to park and redo the attempt.
    Park,
    /// The accept commit landed.
    CommitOk,
    /// The accept commit was rejected by a hook; report again.
    CommitHookFail,
    /// The park sequence finished.
    ParkOk,
}

/// The exhaustive transition table (spec §6.4). No wildcard on the outer
/// match — a new `State` fails to compile until reviewed.
pub fn transition(state: State, event: Event) -> Option<State> {
    use Event as E;
    use State as S;
    match state {
        S::Staged => match event {
            E::Send => Some(S::Running),
            E::Discard => Some(S::Discarded),
            _ => None,
        },
        S::Running => match event {
            E::IdleOk => Some(S::Reported),
            E::IdleErr => Some(S::Errored),
            E::AbortOk => Some(S::Aborted),
            E::Discard => Some(S::Discarded),
            _ => None,
        },
        S::Reported => match event {
            E::Accept => Some(S::Accepting),
            E::FollowUp => Some(S::Running),
            E::Park => Some(S::Parking),
            E::Discard => Some(S::Discarded),
            _ => None,
        },
        S::Errored | S::Aborted => match event {
            E::FollowUp => Some(S::Running),
            E::Park => Some(S::Parking),
            E::Discard => Some(S::Discarded),
            _ => None,
        },
        S::Accepting => match event {
            E::CommitOk => Some(S::Accepted),
            E::CommitHookFail => Some(S::Reported),
            _ => None,
        },
        S::Parking => match event {
            E::ParkOk => Some(S::Parked),
            _ => None,
        },
        S::Accepted | S::Parked | S::Discarded => None, // terminal: sealed
    }
}

/// The columns of the `action` row that CAS transitions may write (P6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Column {
    /// Session binding written at send.
    SessionId,
    /// The send time.
    StartedAt,
    /// The HEAD recorded at send.
    StartHead,
    /// Whether the tree was dirty at send.
    DirtyAtStart,
    /// The report message id (dedupe key, P9).
    ReportMessageId,
    /// The full report text.
    ReportFull,
    /// The serialized footer facts.
    FooterJson,
    /// The outcome kind.
    OutcomeKind,
    /// Finish time.
    EndedAt,
    /// Last liveness event.
    LastEventAt,
    /// Commit subject persisted at Accept time.
    PendingCommitSubject,
    /// Commit body persisted at Accept time.
    PendingCommitBody,
    /// The park branch name.
    ParkBranchName,
    /// The park sequence step.
    ParkStep,
    /// The accepted commit sha.
    AcceptSha,
    /// The follow-up count within an attempt chain.
    FollowUps,
    /// The exact prompt stored on the action (follow-up replaces it, P13).
    PromptSent,
}

impl Column {
    /// The SQL column name.
    pub fn sql(self) -> &'static str {
        match self {
            Column::SessionId => "session_id",
            Column::StartedAt => "started_at",
            Column::StartHead => "start_head",
            Column::DirtyAtStart => "dirty_at_start",
            Column::ReportMessageId => "report_msg_id",
            Column::ReportFull => "report_full",
            Column::FooterJson => "footer_json",
            Column::OutcomeKind => "outcome_kind",
            Column::EndedAt => "ended_at",
            Column::LastEventAt => "last_event_at",
            Column::PendingCommitSubject => "pending_commit_subject",
            Column::PendingCommitBody => "pending_commit_body",
            Column::ParkBranchName => "park_branch_name",
            Column::ParkStep => "park_step",
            Column::AcceptSha => "accept_sha",
            Column::FollowUps => "followups",
            Column::PromptSent => "prompt_sent",
        }
    }
}

/// A staged write value for CAS intents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteValue {
    /// SQL NULL.
    Null,
    /// Text.
    Text(String),
    /// Integer.
    Int(i64),
}

/// A compare-and-swap intent: move `action_id` from `from` to `to`, writing
/// `writes` atomically. Zero changed rows means someone else won (P6).
#[derive(Debug, Clone)]
pub struct CasIntent {
    /// Target action row.
    pub action_id: i64,
    /// Expected current state.
    pub from: State,
    /// Destination state.
    pub to: State,
    /// Columns written together with the transition.
    pub writes: Vec<(Column, WriteValue)>,
}

#[cfg(test)]
mod tests {
    use super::{Event, State, transition};

    fn apply(states_events: &[(State, Event)]) -> Option<State> {
        let mut current = states_events.first()?.0;
        for (_, event) in states_events {
            current = transition(current, *event)?;
        }
        Some(current)
    }

    #[test]
    fn canonical_send_idle_report_path() {
        assert_eq!(
            apply(&[
                (State::Staged, Event::Send),
                (State::Running, Event::IdleOk),
            ]),
            Some(State::Reported)
        );
    }

    #[test]
    fn running_can_abort_or_error() {
        assert_eq!(
            transition(State::Running, Event::AbortOk),
            Some(State::Aborted)
        );
        assert_eq!(
            transition(State::Running, Event::IdleErr),
            Some(State::Errored)
        );
    }

    #[test]
    fn decisions_flow_from_reported() {
        assert_eq!(
            transition(State::Reported, Event::Accept),
            Some(State::Accepting)
        );
        assert_eq!(
            transition(State::Reported, Event::FollowUp),
            Some(State::Running)
        );
        assert_eq!(
            transition(State::Reported, Event::Park),
            Some(State::Parking)
        );
    }

    #[test]
    fn accept_commit_flow_with_hook_failure() {
        assert_eq!(
            apply(&[
                (State::Reported, Event::Accept),
                (State::Accepting, Event::CommitOk),
            ]),
            Some(State::Accepted)
        );
        assert_eq!(
            apply(&[
                (State::Reported, Event::Accept),
                (State::Accepting, Event::CommitHookFail),
            ]),
            Some(State::Reported)
        );
    }

    #[test]
    fn terminal_states_are_sealed() {
        for terminal in [State::Accepted, State::Parked, State::Discarded] {
            for event in [
                Event::Send,
                Event::Discard,
                Event::IdleOk,
                Event::IdleErr,
                Event::AbortOk,
                Event::FollowUp,
                Event::Accept,
                Event::Park,
                Event::CommitOk,
                Event::CommitHookFail,
                Event::ParkOk,
            ] {
                assert!(transition(terminal, event).is_none(), "{terminal:?} sealed");
            }
        }
    }

    #[test]
    fn round_trip_through_db_spelling() {
        for state in super::ALL_STATES {
            let parsed: State = state.as_str().parse().expect("parses");
            assert_eq!(parsed, state);
        }
    }
}
