//! Event-stream vocabulary and the pure wake policy (SPEC-003 W1, D-12).
//!
//! [`OcEvent`] is a change *hint* reduced from an opencode event frame; hints
//! carry no authority (P-19): they select fact reads, nothing else. The
//! classifier's truth is [`TAXONOMY`], populated from the recorded evidence in
//! `devdocs/compat/EVENTS.md` and nothing else — unknown kinds are
//! [`OcEvent::Unkeyed`] by construction. [`WakePolicy`] is a total, panic-free
//! state machine over `(state, now, input)`; it carries no clock and no I/O —
//! the dispatcher executes its decisions.
//!
//! The module imports nothing beyond `std` (NFR-11/P-21): the async machinery
//! lives in the shell.

use std::collections::BTreeMap;

/// A change HINT parsed from an opencode event frame. Hints carry no
/// authority (P-19): they select fact reads, nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OcEvent {
    /// Something about this session changed (activity, messages, gates).
    SessionChanged {
        /// The session the frame was keyed by.
        session_id: String,
    },
    /// The permission/form pending lists may have changed.
    GateListChanged,
    /// The set of sessions may have changed (created/deleted).
    SessionListChanged,
    /// A frame we could not map to a key. Counted, never dispatched.
    Unkeyed,
}

/// The debounce key a hint schedules under.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WakeKey {
    /// One session's activity surface.
    Session(String),
    /// The unified pending-gate list.
    Gates,
    /// The root-session list.
    SessionList,
}

/// What the frame kind means for wake purposes, per the recorded taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HintKind {
    /// The frame is keyed by a session whose activity may have changed.
    Session,
    /// The frame signals possible pending-gate-list churn.
    GateList,
    /// The frame signals possible root-session-list churn.
    SessionList,
    /// A known kind with no wake meaning; it classifies to [`OcEvent::Unkeyed`].
    Ignored,
}

/// One [`TAXONOMY`] row: whether the kind carries a session id, and what the
/// kind means. Evidence, not aspiration — rows come from
/// `devdocs/compat/EVENTS.md` (SPEC-003 W0) and nowhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TaxonomyRow {
    /// Whether a conforming frame of this kind carries a session key.
    pub keyed: bool,
    /// The wake meaning of the kind.
    pub hint: HintKind,
}

/// The frame-kind taxonomy, transcribed from `devdocs/compat/EVENTS.md`
/// (S-10 evidence against the pinned 2.0.24 server). This table is the single
/// place frame-kind knowledge lives; unknown kinds are [`OcEvent::Unkeyed`]
/// by construction. There are deliberately no `GateList` rows: S-10 pinned
/// AE-2 false (no permission/form frame kinds exist in 2.0.24).
pub const TAXONOMY: &[(&str, TaxonomyRow)] = &[
    (
        "server.connected",
        TaxonomyRow {
            keyed: false,
            hint: HintKind::Ignored,
        },
    ),
    (
        "session.created",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::SessionList,
        },
    ),
    (
        "session.deleted",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::SessionList,
        },
    ),
    (
        "session.inbox.enqueued",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::Session,
        },
    ),
    (
        "session.inbox.delivered",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::Session,
        },
    ),
    (
        "session.execution.started",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::Session,
        },
    ),
    (
        "session.execution.failed",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::Session,
        },
    ),
    (
        "session.instructions.updated",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::Session,
        },
    ),
    (
        "session.reasoning.started",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::Session,
        },
    ),
    (
        "session.reasoning.delta",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::Session,
        },
    ),
    (
        "session.reasoning.ended",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::Session,
        },
    ),
    (
        "session.text.started",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::Session,
        },
    ),
    (
        "session.text.delta",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::Session,
        },
    ),
    (
        "session.text.ended",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::Session,
        },
    ),
    (
        "session.step.started",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::Session,
        },
    ),
    (
        "session.step.streamed",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::Session,
        },
    ),
    (
        "session.step.ended",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::Session,
        },
    ),
    (
        "session.step.failed",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::Session,
        },
    ),
    (
        "session.retry.scheduled",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::Session,
        },
    ),
    (
        "session.tool.called",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::Session,
        },
    ),
    (
        "session.tool.input.started",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::Session,
        },
    ),
    (
        "session.tool.input.ended",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::Session,
        },
    ),
    (
        "session.tool.progress",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::Session,
        },
    ),
    (
        "session.tool.success",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::Session,
        },
    ),
    (
        "session.usage.updated",
        TaxonomyRow {
            keyed: true,
            hint: HintKind::Session,
        },
    ),
    (
        "project.updated",
        TaxonomyRow {
            keyed: false,
            hint: HintKind::Ignored,
        },
    ),
    (
        "agent.updated",
        TaxonomyRow {
            keyed: false,
            hint: HintKind::Ignored,
        },
    ),
    (
        "command.updated",
        TaxonomyRow {
            keyed: false,
            hint: HintKind::Ignored,
        },
    ),
    (
        "integration.updated",
        TaxonomyRow {
            keyed: false,
            hint: HintKind::Ignored,
        },
    ),
    (
        "model.updated",
        TaxonomyRow {
            keyed: false,
            hint: HintKind::Ignored,
        },
    ),
    (
        "plugin.updated",
        TaxonomyRow {
            keyed: false,
            hint: HintKind::Ignored,
        },
    ),
    (
        "provider.updated",
        TaxonomyRow {
            keyed: false,
            hint: HintKind::Ignored,
        },
    ),
    (
        "reference.updated",
        TaxonomyRow {
            keyed: false,
            hint: HintKind::Ignored,
        },
    ),
    (
        "skill.updated",
        TaxonomyRow {
            keyed: false,
            hint: HintKind::Ignored,
        },
    ),
    (
        "websearch.updated",
        TaxonomyRow {
            keyed: false,
            hint: HintKind::Ignored,
        },
    ),
    (
        "shell.created",
        TaxonomyRow {
            keyed: false,
            hint: HintKind::Ignored,
        },
    ),
    (
        "shell.exited",
        TaxonomyRow {
            keyed: false,
            hint: HintKind::Ignored,
        },
    ),
    (
        "shell.deleted",
        TaxonomyRow {
            keyed: false,
            hint: HintKind::Ignored,
        },
    ),
];

/// Map a frame header to a hint. Pure over strings; data-driven from
/// [`TAXONOMY`]. Unknown kinds are [`OcEvent::Unkeyed`] by construction, and
/// so is a *keyed* kind arriving without a session id — a keyless keyed-frame
/// is unmappable, not a [`OcEvent::GateListChanged`] guess (SPEC-003 W1).
#[must_use]
pub fn classify_event(kind: &str, session_id: Option<&str>) -> OcEvent {
    let Some(row) = TAXONOMY
        .iter()
        .find(|(name, _)| *name == kind)
        .map(|(_, row)| row)
    else {
        return OcEvent::Unkeyed;
    };
    match row.hint {
        HintKind::Session => match session_id {
            Some(id) => OcEvent::SessionChanged {
                session_id: id.to_owned(),
            },
            None => OcEvent::Unkeyed,
        },
        HintKind::GateList => OcEvent::GateListChanged,
        HintKind::SessionList => OcEvent::SessionListChanged,
        HintKind::Ignored => OcEvent::Unkeyed,
    }
}

/// The result of the fact read a dispatched wake caused, fed back into the
/// policy so it can coalesce, mark dirty, or settle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOutcome {
    /// The session (or its root) is still running.
    Busy,
    /// The session reached idle; the completion path ran.
    Idle,
    /// The session has no running action in the store (untracked, P-27).
    Untracked,
}

/// One input to the policy. Time never enters except as `now`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WakeInput {
    /// A classified hint arrived from the stream.
    Hint(OcEvent),
    /// The dispatcher's deadline tick fired (trailing edge, max age, settle).
    FlushDue,
    /// The read issued for `key` completed. When the read contradicted an
    /// idle-class hint's expectation (the frame said idle, `activity` says
    /// busy), the dispatcher reports [`WakeInput::Contradicted`] *instead* —
    /// the key is still in flight at that moment.
    ReadDone {
        /// The key the read belonged to.
        key: WakeKey,
        /// What the read found.
        outcome: ReadOutcome,
    },
    /// The read contradicted the hint's expectation (hint said idle-class,
    /// the read says busy): arm a bounded settle re-read (P-26). Reported
    /// in place of [`WakeInput::ReadDone`] for that read.
    Contradicted {
        /// The key whose read contradicted.
        key: WakeKey,
    },
}

/// What the dispatcher must execute after one step. A single `FlushDue` tick
/// may fire several due keys at once (§4.6); each entry is one fact read
/// (P-27 governs which read). `sleep_until` is the next absolute deadline the
/// dispatcher must guarantee (`FlushDue` arrives no later than it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WakeOutput {
    /// Issue one fact read per entry (P-27 governs which read).
    pub read: Vec<WakeKey>,
    /// The session-list key became dirty; the inbox diff picks it up with
    /// one list read, deduped by `notified` (P-27).
    pub dirty: Option<WakeKey>,
    /// The next deadline, if any key is pending or settling.
    pub sleep_until: Option<i64>,
}

impl WakeOutput {
    /// No side effects (deadlines may still be set).
    #[must_use]
    pub fn none() -> Self {
        Self {
            read: Vec::new(),
            dirty: None,
            sleep_until: None,
        }
    }
}

/// Per-key lifecycle inside [`WakeState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyState {
    /// Debounce armed: flush no earlier than `last_at + debounce_ms`, and no
    /// later than `first_at + max_delay_ms` (P-25).
    Pending {
        /// Arrival of the first coalesced hint.
        first_at: i64,
        /// Arrival of the most recent coalesced hint (trailing edge).
        last_at: i64,
    },
    /// A read is executing. `rearm_at` merges hints that arrived mid-flight
    /// into a single re-arm, not a queue (§4.6); `settle_tries` survives the
    /// flight so contradiction re-reads stay bounded (P-26).
    InFlight {
        /// Arrival of a merged mid-flight hint, if any.
        rearm_at: Option<i64>,
        /// Contradiction re-reads already consumed by this wake.
        settle_tries: u8,
    },
    /// A contradiction re-read is scheduled (P-26).
    Settle { due_at: i64, tries: u8 },
}

/// The tunable policy numbers (§4.7 `[wake]`; defaults per P-25).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WakePolicy {
    /// Trailing-edge coalesce per key.
    pub debounce_ms: i64,
    /// A pending wake MUST flush by now.
    pub max_delay_ms: i64,
    /// Minimum spacing between ISSUED reads per key.
    pub per_key_min_ms: i64,
    /// Reads/s burst ceiling of the global token bucket.
    pub global_burst: u32,
    /// Reads/s sustained refill rate of the global token bucket.
    pub global_sustained: f64,
    /// Contradiction re-read delay (P-26).
    pub settle_ms: i64,
    /// Maximum contradiction re-reads per wake (P-26).
    pub settle_max_tries: u8,
}

impl Default for WakePolicy {
    fn default() -> Self {
        Self {
            debounce_ms: 250,
            max_delay_ms: 1000,
            per_key_min_ms: 500,
            global_burst: 20,
            global_sustained: 5.0,
            settle_ms: 750,
            settle_max_tries: 2,
        }
    }
}

impl WakePolicy {
    /// The §4.7 defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Pure transition: `(policy, state, now, input) -> (state', output)`.
    /// Carries no clock, no I/O; the dispatcher executes. `now` is
    /// milliseconds on a monotonic-ish scale (the [`i64`] of the shell's
    /// clock; only differences matter).
    #[must_use]
    pub fn step(
        &self,
        mut state: WakeState,
        now: i64,
        input: WakeInput,
    ) -> (WakeState, WakeOutput) {
        let mut out = WakeOutput::none();
        match input {
            WakeInput::Hint(event) => match event {
                OcEvent::SessionChanged { session_id } => {
                    Self::arm(&mut state, WakeKey::Session(session_id), now);
                }
                OcEvent::GateListChanged => {
                    Self::arm(&mut state, WakeKey::Gates, now);
                }
                OcEvent::SessionListChanged => {
                    Self::arm(&mut state, WakeKey::SessionList, now);
                }
                OcEvent::Unkeyed => {}
            },
            WakeInput::FlushDue => {
                // Collect due keys first: `issue` mutates the map.
                let due: Vec<WakeKey> = state
                    .keys
                    .iter()
                    .filter(|(_, key_state)| match key_state {
                        KeyState::Pending { first_at, last_at } => {
                            now - *last_at >= self.debounce_ms
                                || now - *first_at >= self.max_delay_ms
                        }
                        KeyState::Settle { due_at, .. } => now >= *due_at,
                        KeyState::InFlight { .. } => false,
                    })
                    .map(|(key, _)| key.clone())
                    .collect();
                for key in due {
                    self.issue(&mut state, key, now, &mut out);
                }
            }
            WakeInput::ReadDone { key, outcome } => {
                if key == WakeKey::SessionList {
                    state.session_list_dirty = false;
                }
                if let Some(KeyState::InFlight {
                    rearm_at: Some(at), ..
                }) = state.keys.remove(&key)
                {
                    state.keys.insert(
                        key,
                        KeyState::Pending {
                            first_at: at,
                            last_at: at,
                        },
                    );
                }
                if outcome == ReadOutcome::Untracked && !state.session_list_dirty {
                    state.session_list_dirty = true;
                    out.dirty = Some(WakeKey::SessionList);
                }
            }
            WakeInput::Contradicted { key } => {
                if let Some(KeyState::InFlight { settle_tries, .. }) = state.keys.get(&key).copied()
                {
                    let tries = settle_tries + 1;
                    if tries <= self.settle_max_tries {
                        state.keys.insert(
                            key,
                            KeyState::Settle {
                                due_at: now + self.settle_ms,
                                tries,
                            },
                        );
                    } else {
                        // Released to the fallback cadence (FR-48).
                        state.keys.remove(&key);
                    }
                }
            }
        }
        out.sleep_until = self.next_deadline(&state);
        (state, out)
    }

    /// The earliest pending/settle deadline, if any key is pending or
    /// settling: trailing-edge flush at `last_at + debounce_ms`, max-age
    /// flush at `first_at + max_delay_ms`, settle re-read at `due_at`. The
    /// dispatcher must deliver `FlushDue` no later than this instant.
    #[must_use]
    pub fn next_deadline(&self, state: &WakeState) -> Option<i64> {
        state
            .keys
            .values()
            .filter_map(|key_state| match *key_state {
                KeyState::Pending { first_at, last_at } => {
                    Some((last_at + self.debounce_ms).min(first_at + self.max_delay_ms))
                }
                KeyState::Settle { due_at, .. } => Some(due_at),
                KeyState::InFlight { .. } => None,
            })
            .min()
    }

    /// Arm (or trailing-edge re-arm) a key's debounce window.
    fn arm(state: &mut WakeState, key: WakeKey, now: i64) {
        match state.keys.get_mut(&key) {
            Some(KeyState::Pending { last_at, .. }) => {
                *last_at = now;
            }
            Some(KeyState::InFlight { rearm_at, .. }) => {
                if rearm_at.is_none() {
                    *rearm_at = Some(now);
                }
            }
            Some(KeyState::Settle { .. }) => {}
            None => {
                state.keys.insert(
                    key,
                    KeyState::Pending {
                        first_at: now,
                        last_at: now,
                    },
                );
            }
        }
    }

    /// Try to issue the read for a due key: per-key spacing first, then the
    /// global token bucket. A spaced-out key re-arms as pending; a shed key
    /// is dropped, not queued — the fallback cadence is its deadline (P-25).
    fn issue(&self, state: &mut WakeState, key: WakeKey, now: i64, out: &mut WakeOutput) {
        if let Some(&issued_at) = state.last_issued_ms.get(&key)
            && now - issued_at < self.per_key_min_ms
        {
            state.keys.insert(
                key,
                KeyState::Pending {
                    first_at: now,
                    last_at: now,
                },
            );
            return;
        }
        Self::refill(state, now, self.global_burst, self.global_sustained);
        if state.tokens < 1.0 {
            // Shed: counted by the dispatcher, never queued (P-25).
            state.keys.remove(&key);
            return;
        }
        state.tokens -= 1.0;
        let settle_tries = match state.keys.get(&key) {
            Some(KeyState::Settle { tries, .. }) => *tries,
            _ => 0,
        };
        state.last_issued_ms.insert(key.clone(), now);
        state.keys.insert(
            key.clone(),
            KeyState::InFlight {
                rearm_at: None,
                settle_tries,
            },
        );
        out.read.push(key);
    }

    /// Refill the global token bucket up to the burst ceiling. The bucket
    /// starts full (classic capacity-burst semantics: the first burst of
    /// reads is bounded by the burst ceiling, not by prior refill history).
    /// The elapsed window is capped at an hour so a pathological clock jump
    /// cannot mint tokens; a backward jump refills nothing.
    fn refill(state: &mut WakeState, now: i64, burst: u32, sustained: f64) {
        if state.last_refill_ms == 0 && state.tokens == 0.0 {
            state.tokens = f64::from(burst);
            state.last_refill_ms = now;
            return;
        }
        let elapsed = (now - state.last_refill_ms).min(3_600_000);
        if elapsed > 0 {
            let millis = f64::from(i32::try_from(elapsed).unwrap_or(0));
            state.tokens = f64::from(burst).min(state.tokens + millis / 1000.0 * sustained);
            state.last_refill_ms = now;
        }
    }
}

/// The policy's mutable state, threaded through [`WakePolicy::step`] by value.
#[derive(Debug, Clone, Default)]
pub struct WakeState {
    keys: BTreeMap<WakeKey, KeyState>,
    last_issued_ms: BTreeMap<WakeKey, i64>,
    tokens: f64,
    last_refill_ms: i64,
    session_list_dirty: bool,
}

impl WakeState {
    /// A fresh, empty state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether any key is awaiting a [`WakeInput::FlushDue`] tick (debounce
    /// pending or settle re-read scheduled). Keys whose read is merely in
    /// flight do not count: they need no timer.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.keys.values().any(|key_state| {
            matches!(
                key_state,
                KeyState::Pending { .. } | KeyState::Settle { .. }
            )
        })
    }
}
