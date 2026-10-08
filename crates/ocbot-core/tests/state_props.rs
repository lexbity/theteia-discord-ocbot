//! Property tests for the transition table (spec Phase 1: `state_props.rs`):
//! reachability from `staged`, totality (never panics), and sealed terminals.

use std::collections::{BTreeSet, VecDeque};

use ocbot_core::{ALL_STATES, Event, State, transition};

const ALL_EVENTS: [Event; 11] = [
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
];

fn reachable_from_staged() -> BTreeSet<State> {
    let mut frontier = VecDeque::from([State::Staged]);
    let mut reachable = BTreeSet::from([State::Staged]);
    while let Some(state) = frontier.pop_front() {
        for event in ALL_EVENTS {
            if let Some(next) = transition(state, event)
                && reachable.insert(next)
            {
                frontier.push_back(next);
            }
        }
    }
    reachable
}

#[test]
fn every_state_is_reachable_from_staged() {
    let reachable = reachable_from_staged();
    let expected: BTreeSet<State> = [
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
    ]
    .into_iter()
    .collect();
    assert_eq!(
        reachable, expected,
        "no unreachable dead code in the machine"
    );
}

#[test]
fn the_machine_covers_every_state_event_pair() {
    for state in ALL_STATES {
        for event in ALL_EVENTS {
            let _ = transition(state, event); // must not panic
        }
    }
}

#[test]
fn terminal_states_are_sealed_against_every_event() {
    for terminal in [State::Accepted, State::Parked, State::Discarded] {
        for event in ALL_EVENTS {
            assert!(
                transition(terminal, event).is_none(),
                "{terminal:?} must be sealed against {event:?}"
            );
        }
    }
}

#[test]
fn running_has_no_silent_reports() {
    // The only ways out of `running` are the three idle/abort outcomes and
    // discard — a `running` action can never transition to a fresh `staged`.
    for event in ALL_EVENTS {
        if let Some(next) = transition(State::Running, event) {
            assert_ne!(next, State::Staged);
        }
    }
}
