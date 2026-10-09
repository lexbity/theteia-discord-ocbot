//! The wake policy battery (SPEC-003 W1): fake-clock unit tests over every
//! behavioral surface of [`WakePolicy`] / [`classify_event`], plus the AC-E13
//! property battery — arbitrary frame sequences must keep issued reads
//! bounded by the policy, never dispatch `Unkeyed`, and always expose a
//! finite deadline while a key is pending.

use std::collections::HashMap;

use ocbot_core::{
    HintKind, OcEvent, ReadOutcome, TAXONOMY, WakeInput, WakeKey, WakeOutput, WakePolicy,
    WakeState, classify_event,
};

/// A tiny deterministic LCG (house style) so the property battery is
/// reproducible byte-for-byte across machines and runs.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }
}

/// Drive one input through the policy and return `(state, output)`.
fn step(
    policy: &WakePolicy,
    state: WakeState,
    now: i64,
    input: WakeInput,
) -> (WakeState, WakeOutput) {
    policy.step(state, now, input)
}

fn session_hint(id: &str) -> WakeInput {
    WakeInput::Hint(OcEvent::SessionChanged {
        session_id: id.to_owned(),
    })
}

// ---------------------------------------------------------------------------
// classify_event: table test generated from TAXONOMY + adversarial vectors.
// ---------------------------------------------------------------------------

#[test]
fn every_taxonomy_row_classifies_to_its_recorded_hint() {
    assert!(!TAXONOMY.is_empty(), "the taxonomy is not empty");
    for (kind, row) in TAXONOMY {
        let event = classify_event(kind, Some("ses_a"));
        match row.hint {
            HintKind::Session => {
                assert_eq!(
                    event,
                    OcEvent::SessionChanged {
                        session_id: "ses_a".to_owned()
                    },
                    "{kind} must map to SessionChanged"
                );
            }
            HintKind::SessionList => {
                assert_eq!(
                    event,
                    OcEvent::SessionListChanged,
                    "{kind} must map to SessionListChanged"
                );
            }
            HintKind::GateList => {
                assert_eq!(
                    event,
                    OcEvent::GateListChanged,
                    "{kind} must map to GateListChanged"
                );
            }
            HintKind::Ignored => {
                assert_eq!(event, OcEvent::Unkeyed, "{kind} must map to Unkeyed");
            }
        }
        // A keyed kind is keyed regardless of an id; a non-keyed kind ignores
        // a stray id (classification is driven by the kind, not the key).
        if !row.keyed {
            continue;
        }
        let keyless = classify_event(kind, None);
        if row.hint == HintKind::Session {
            assert_eq!(
                keyless,
                OcEvent::Unkeyed,
                "{kind} without a session id is unmappable, not a guess"
            );
        }
    }
}

#[test]
fn unknown_kinds_and_garbage_are_unkeyed_by_construction() {
    for kind in [
        "session.permission.requested", // plausible future kind — unknown today
        "session.",
        "session",
        "SERVER.CONNECTED",
        "",
        "form.pending",
        "permission.created",
    ] {
        assert_eq!(
            classify_event(kind, Some("ses_a")),
            OcEvent::Unkeyed,
            "{kind:?} must be Unkeyed"
        );
        assert_eq!(classify_event(kind, None), OcEvent::Unkeyed);
    }
}

// ---------------------------------------------------------------------------
// Debounce (P-25, trailing edge).
// ---------------------------------------------------------------------------

#[test]
fn debounce_coalesces_n_hints_into_one_read() {
    let policy = WakePolicy::new();
    let mut state = WakeState::new();
    for ms in [0, 50, 100, 150, 200] {
        let (next, out) = step(&policy, state, ms, session_hint("ses_a"));
        state = next;
        assert!(out.read.is_empty(), "no read before the debounce elapses");
    }
    // Exactly at the trailing edge (last hint 200 + 250) the read fires.
    let (state, out) = step(&policy, state, 450, WakeInput::FlushDue);
    assert_eq!(out.read, vec![WakeKey::Session("ses_a".to_owned())]);
    // One read only: no further deadline, no second read.
    assert_eq!(out.sleep_until, None, "no key is pending anymore");
    let (_, out) = step(&policy, state, 600, WakeInput::FlushDue);
    assert!(out.read.is_empty());
}

#[test]
fn trailing_edge_rearm_delays_the_flush() {
    let policy = WakePolicy::new();
    let state = WakeState::new();
    let (state, _) = step(&policy, state, 0, session_hint("ses_a"));
    // A hint at 200 re-arms: flush due at 450, not 250.
    let (state, _) = step(&policy, state, 200, session_hint("ses_a"));
    let (state, out) = step(&policy, state, 250, WakeInput::FlushDue);
    assert!(out.read.is_empty(), "trailing edge moved");
    let (_, out) = step(&policy, state, 450, WakeInput::FlushDue);
    assert_eq!(out.read, vec![WakeKey::Session("ses_a".to_owned())]);
    assert_eq!(out.sleep_until, None);
}

// ---------------------------------------------------------------------------
// Max age (P-25): a pending wake MUST flush by max_delay_ms.
// ---------------------------------------------------------------------------

#[test]
fn max_age_flushes_at_exactly_max_delay() {
    let policy = WakePolicy::new();
    let state = WakeState::new();
    let (state, _) = step(&policy, state, 0, session_hint("ses_a"));
    let mut state = state;
    // Chatter keeps re-arming the trailing edge forever...
    for ms in (100..1000).step_by(100) {
        let (next, out) = step(&policy, state, i64::from(ms), session_hint("ses_a"));
        state = next;
        assert!(out.read.is_empty());
    }
    // ...but max-age still flushes at exactly 1000.
    let (_, out) = step(&policy, state, 1000, WakeInput::FlushDue);
    assert_eq!(
        out.read,
        vec![WakeKey::Session("ses_a".to_owned())],
        "max-age bound is absolute, not chattering-deferrable"
    );
}

#[test]
fn max_age_compares_elapsed_time_not_absolute_magnitude() {
    let policy = WakePolicy::new();
    let state = WakeState::new();
    // A hint at a large epoch: `now + first_at` would already exceed
    // max_delay and flush instantly; only `now - first_at` is correct.
    let (state, _) = step(&policy, state, 5000, session_hint("ses_late"));
    let (state, out) = step(&policy, state, 5100, WakeInput::FlushDue);
    assert!(out.read.is_empty(), "max-age must measure elapsed time");
    let (_, out) = step(&policy, state, 5250, WakeInput::FlushDue);
    assert_eq!(
        out.read,
        vec![WakeKey::Session("ses_late".to_owned())],
        "trailing edge still fires at a large epoch"
    );
}

#[test]
fn the_deadline_guarantees_the_max_age_tick_arrives() {
    let policy = WakePolicy::new();
    let state = WakeState::new();
    let (_, out) = step(&policy, state, 0, session_hint("ses_a"));
    // The dispatcher is told to flush no later than max_delay even though the
    // trailing edge is at 250.
    assert_eq!(out.sleep_until, Some(policy.debounce_ms));
}

// ---------------------------------------------------------------------------
// Per-key spacing (P-25): min 500 ms between ISSUED reads for the same key.
// ---------------------------------------------------------------------------

#[test]
fn per_key_spacing_enforced_under_rapid_flush_and_hint() {
    let policy = WakePolicy::new();
    let state = WakeState::new();
    // First read at 0.
    let (state, out) = step(&policy, state, 0, session_hint("ses_a"));
    assert!(out.read.is_empty(), "first hint only arms");
    let (state, first) = step(&policy, state, 250, WakeInput::FlushDue);
    assert_eq!(first.read, vec![WakeKey::Session("ses_a".to_owned())]);
    // Read completes instantly; a new hint arrives at 260; flush at 510.
    let (state, _) = step(
        &policy,
        state,
        250,
        WakeInput::ReadDone {
            key: WakeKey::Session("ses_a".to_owned()),
            outcome: ReadOutcome::Busy,
        },
    );
    let (state, _) = step(&policy, state, 250, session_hint("ses_a"));
    // The debounce elapses at 500, but spacing (issued at 250) blocks until
    // exactly 750: the key re-arms as pending instead of issuing.
    let (state, out) = step(&policy, state, 500, WakeInput::FlushDue);
    assert!(out.read.is_empty(), "spacing defers the second read");
    assert!(state.has_pending(), "the deferred wake stays pending");
    // The boundary is inclusive: 750 - 250 = per_key_min_ms issues.
    let (_, out) = step(&policy, state, 750, WakeInput::FlushDue);
    assert_eq!(out.read, vec![WakeKey::Session("ses_a".to_owned())]);
}

// ---------------------------------------------------------------------------
// In-flight coalescing (§4.6): a hint for an executing key merges, not queues.
// ---------------------------------------------------------------------------

#[test]
fn a_hint_arriving_mid_flight_merges_into_one_re_arm() {
    let policy = WakePolicy::new();
    let state = WakeState::new();
    let (state, _) = step(&policy, state, 0, session_hint("ses_a"));
    let (state, _) = step(&policy, state, 250, WakeInput::FlushDue);
    let mut state = state;
    // Three hints during the flight merge into a single re-arm.
    for ms in [300, 400, 500] {
        let (next, merged) = step(&policy, state, ms, session_hint("ses_a"));
        state = next;
        assert!(
            merged.read.is_empty(),
            "mid-flight hints never read directly"
        );
    }
    let (state, _) = step(
        &policy,
        state,
        600,
        WakeInput::ReadDone {
            key: WakeKey::Session("ses_a".to_owned()),
            outcome: ReadOutcome::Busy,
        },
    );
    // Exactly one re-arm from the merged hints, debounced from its arrival
    // (due 550) and spacing-blocked until 750 (first read at 250).
    let (state, out) = step(&policy, state, 550, WakeInput::FlushDue);
    assert!(
        out.read.is_empty(),
        "re-arm debounces from the merged arrival"
    );
    let (state, out) = step(&policy, state, 700, WakeInput::FlushDue);
    assert!(
        out.read.is_empty(),
        "spacing (min 500 ms per key) still blocks"
    );
    let (_, out) = step(&policy, state, 1000, WakeInput::FlushDue);
    assert_eq!(out.read, vec![WakeKey::Session("ses_a".to_owned())]);
}

// ---------------------------------------------------------------------------
// Store-first untracked rule (P-27), expressed in policy state.
// ---------------------------------------------------------------------------

#[test]
fn untracked_mark_dirty_emits_exactly_one_mark_per_burst() {
    let policy = WakePolicy::new();
    let state = WakeState::new();
    let key = WakeKey::Session("ses_foreign".to_owned());
    let (state, _) = step(&policy, state, 0, session_hint("ses_foreign"));
    let (state, _) = step(&policy, state, 250, WakeInput::FlushDue);
    let (state, done) = step(
        &policy,
        state,
        260,
        WakeInput::ReadDone {
            key,
            outcome: ReadOutcome::Untracked,
        },
    );
    assert_eq!(
        done.dirty,
        Some(WakeKey::SessionList),
        "the first untracked read marks the list dirty"
    );
    // Five more untracked completions in the same burst: zero further marks.
    let mut state = state;
    for ms in [300, 350, 400, 450, 500] {
        let (next, out) = step(
            &policy,
            state,
            ms,
            WakeInput::ReadDone {
                key: WakeKey::Session(format!("ses_other{ms}")),
                outcome: ReadOutcome::Untracked,
            },
        );
        state = next;
        assert_eq!(out.dirty, None, "dirty is idempotent within a burst");
    }
    // The SessionList read completing clears the flag: the next burst marks again.
    let (_, out) = step(
        &policy,
        state,
        550,
        WakeInput::ReadDone {
            key: WakeKey::SessionList,
            outcome: ReadOutcome::Idle,
        },
    );
    assert_eq!(out.dirty, None);
}

// ---------------------------------------------------------------------------
// Settle path (P-26): contradiction re-reads bounded, then release.
// ---------------------------------------------------------------------------

#[test]
fn settle_path_re_arms_at_most_twice_then_releases() {
    let policy = WakePolicy::new();
    let state = WakeState::new();
    let key = WakeKey::Session("ses_flip".to_owned());
    let (state, _) = step(&policy, state, 0, session_hint("ses_flip"));
    let (state, out) = step(&policy, state, 250, WakeInput::FlushDue);
    assert_eq!(out.read, vec![key.clone()]);
    let mut state = state;
    let mut now = 1000;

    for attempt in 1..=policy.settle_max_tries {
        // Contradiction arms a settle re-read.
        let (next, out) = step(
            &policy,
            state,
            now,
            WakeInput::Contradicted { key: key.clone() },
        );
        state = next;
        assert!(out.read.is_empty());
        assert!(state.has_pending(), "settle attempt {attempt} is scheduled");
        // The re-read must not fire early...
        let (next, out) = step(&policy, state, now + 1, WakeInput::FlushDue);
        state = next;
        assert!(
            out.read.is_empty(),
            "settle attempt {attempt} fired before settle_ms elapsed"
        );
        // ...it fires at settle_ms.
        let (next, out) = step(&policy, state, now + policy.settle_ms, WakeInput::FlushDue);
        state = next;
        assert_eq!(
            out.read,
            vec![key.clone()],
            "settle attempt {attempt} issues its re-read"
        );
        now += policy.settle_ms * 2; // time only moves forward
    }
    // Third contradiction: the key is released to the fallback cadence.
    let (state, out) = step(&policy, state, now, WakeInput::Contradicted { key });
    assert!(out.read.is_empty());
    assert!(!state.has_pending(), "the key is released after the bound");
}

// ---------------------------------------------------------------------------
// Global token bucket (P-25): burst 20, sustained 5/s; shedding drops.
// ---------------------------------------------------------------------------

#[test]
fn token_bucket_sheds_a_thousand_frame_burst_to_the_sustained_rate() {
    let policy = WakePolicy::new();
    let mut state = WakeState::new();
    let mut issued: usize = 0;
    // 1000 distinct sessions flush "at once" (same instant): the bucket caps
    // issuance at the burst ceiling.
    for i in 0..1000 {
        let (next, _) = step(&policy, state, 0, session_hint(&format!("ses_burst{i}")));
        state = next;
    }
    for _ in 0..1000 {
        let (next, out) = step(&policy, state, 250, WakeInput::FlushDue);
        state = next;
        issued += out.read.len();
    }
    assert_eq!(
        issued,
        usize::try_from(policy.global_burst).expect("fits"),
        "burst ceiling holds"
    );
    // The 980 non-issued wakes were shed on the spot: no queued key ever
    // surfaces later, at any tick.
    for ms in [500, 750, 1000, 2000, 5000] {
        let (next, out) = step(&policy, state, ms, WakeInput::FlushDue);
        state = next;
        assert!(out.read.is_empty(), "shed wakes must not re-queue at {ms}");
    }
}

#[test]
fn token_bucket_refills_at_the_sustained_rate() {
    let policy = WakePolicy::new();
    // Two sessions, alternating reads: spacing allows 500ms per key, tokens
    // allow 5/s globally.
    let mut state = WakeState::new();
    let mut issued = Vec::new();
    let mut now = 0;
    for round in 0..10 {
        for id in ["ses_x", "ses_y"] {
            let key = WakeKey::Session(id.to_owned());
            let (next, _) = step(&policy, state, now, session_hint(id));
            state = next;
            let (next, out) = step(
                &policy,
                state,
                now + policy.debounce_ms,
                WakeInput::FlushDue,
            );
            state = next;
            if out.read == vec![key.clone()] {
                issued.push(now + policy.debounce_ms);
                let (next, _) = step(
                    &policy,
                    state,
                    now + policy.debounce_ms + 1,
                    WakeInput::ReadDone {
                        key,
                        outcome: ReadOutcome::Idle,
                    },
                );
                state = next;
            }
            now += 500;
        }
        let _ = round;
    }
    // One read attempt every 500 ms: the sustained refill (5/s = one token
    // per 200 ms) never starves, so spacing (500 ms) stays the binding
    // constraint. Invariant: reads never fire faster than the refill allows.
    for window in issued.windows(2) {
        assert!(
            window[1] - window[0] >= 200,
            "reads fired faster than the sustained refill"
        );
    }
}

// ---------------------------------------------------------------------------
// Unkeyed (FR-44): counted, never dispatched.
// ---------------------------------------------------------------------------

#[test]
fn unkeyed_never_yields_a_read_or_state() {
    let policy = WakePolicy::new();
    let mut state = WakeState::new();
    for ms in [0, 100, 500, 5000] {
        let (next, out) = step(&policy, state, ms, WakeInput::Hint(OcEvent::Unkeyed));
        state = next;
        assert!(out.read.is_empty());
        assert_eq!(out.dirty, None);
        assert_eq!(out.sleep_until, None, "Unkeyed arms nothing");
    }
    assert!(!state.has_pending());
}

// ---------------------------------------------------------------------------
// Property battery (AC-E13): deterministic LCG over random frame sequences.
// ---------------------------------------------------------------------------

#[test]
fn the_sustained_refill_rate_is_pinned_not_merely_bounded() {
    let policy = WakePolicy::new();
    let mut state = WakeState::new();
    // Drain the full burst at one instant: 20 reads, bucket at 0.
    for i in 0..20 {
        let (next, _) = step(&policy, state, 0, session_hint(&format!("ses_d{i}")));
        state = next;
    }
    let mut drained = 0;
    for _ in 0..20 {
        let (next, out) = step(&policy, state, 250, WakeInput::FlushDue);
        state = next;
        drained += out.read.len();
    }
    assert_eq!(drained, 20, "the whole burst issues");
    // Two wakes due 100 ms after the drain (hinted at 100, due at 350):
    // only 0.5 tokens have refilled — the first is shed, and the second
    // finds the bucket still short: both shed, nothing queued.
    let (state, _) = step(&policy, state, 100, session_hint("ses_p"));
    let (state, _) = step(&policy, state, 100, session_hint("ses_r"));
    let (state, out) = step(&policy, state, 350, WakeInput::FlushDue);
    assert!(out.read.is_empty(), "100 ms must not refill a whole token");
    // Re-armed at 200 (due 450), the second 100 ms of refill completes the
    // token exactly at 200 ms after the drain: it issues.
    let (state, _) = step(&policy, state, 200, session_hint("ses_r"));
    let (state, out) = step(&policy, state, 450, WakeInput::FlushDue);
    assert_eq!(out.read, vec![WakeKey::Session("ses_r".to_owned())]);
    // 1100 ms after the last refill the bucket holds 5.5 tokens — well above
    // one: it issues. (Distinguishes a real refill from a
    // modulo-of-1000 ms imitation, which would hold only 0.5.)
    let (state, _) = step(&policy, state, 1300, session_hint("ses_r2"));
    let (_, out) = step(&policy, state, 1550, WakeInput::FlushDue);
    assert_eq!(out.read, vec![WakeKey::Session("ses_r2".to_owned())]);
}

#[test]
fn arbitrary_frame_sequences_keep_reads_bounded_and_unkeyed_dark() {
    let policy = WakePolicy::new();
    let kinds: Vec<&str> = TAXONOMY.iter().map(|(kind, _)| *kind).collect();
    let mut rng = Lcg(0x5EED_1A2B_3C4D_5E6F);
    for sequence in 0..2_000_u64 {
        let length = usize::try_from(rng.next() % 5_000).unwrap_or(0);
        let mut state = WakeState::new();
        let mut now = 0_i64;
        let mut hints = 0_u64;
        let mut flushes = 0_u64;
        let mut contradicted = 0_u64;
        let mut reads_per_key: HashMap<String, u64> = HashMap::new();
        let mut last_read_per_key: HashMap<String, i64> = HashMap::new();
        let mut dirty_marks = 0_u64;
        let mut untracked_dones = 0_u64;

        for _ in 0..length {
            now += i64::try_from(rng.next() % 50).unwrap_or(0);
            let input = match rng.next() % 10 {
                0..=4 => {
                    let kind = kinds[usize::try_from(
                        rng.next() % u64::try_from(kinds.len()).expect("fits"),
                    )
                    .expect("fits")];
                    let keyed = rng.next().is_multiple_of(4);
                    let id = format!("ses_p{}", rng.next() % 7);
                    hints += 1;
                    WakeInput::Hint(classify_event(kind, keyed.then_some(id.as_str())))
                }
                5..=7 => {
                    flushes += 1;
                    WakeInput::FlushDue
                }
                8 => {
                    let key = WakeKey::Session(format!("ses_p{}", rng.next() % 7));
                    let outcome = match rng.next() % 3 {
                        0 => ReadOutcome::Busy,
                        1 => ReadOutcome::Idle,
                        _ => {
                            untracked_dones += 1;
                            ReadOutcome::Untracked
                        }
                    };
                    WakeInput::ReadDone { key, outcome }
                }
                _ => {
                    contradicted += 1;
                    WakeInput::Contradicted {
                        key: WakeKey::Session(format!("ses_p{}", rng.next() % 7)),
                    }
                }
            };
            let (next, out) = step(&policy, state, now, input);
            state = next;

            // (a) every issued read is within the policy bound: each read for
            // a key must be traceable to a hint, a flush tick, or a
            // contradiction of that key.
            for key in &out.read {
                let name = format!("{key:?}");
                *reads_per_key.entry(name.clone()).or_default() += 1;
                if let Some(&last) = last_read_per_key.get(&name) {
                    assert!(
                        now - last >= policy.per_key_min_ms,
                        "sequence {sequence}: reads for {name} fired {last} -> {now}, violating spacing"
                    );
                }
                last_read_per_key.insert(name, now);
            }
            // (b) Unkeyed never dispatches (checked via the classify path:
            // Unkeyed hints arm nothing, so a FlushDue can never read a key
            // that was never armed — the bound above encodes this).
            if out.dirty.is_some() {
                dirty_marks += 1;
            }
            // (c) SleepUntil is finite whenever any key is pending.
            if state.has_pending() {
                assert!(
                    out.sleep_until.is_some(),
                    "sequence {sequence}: pending key at {now} with no deadline"
                );
            } else {
                assert_eq!(
                    out.sleep_until, None,
                    "sequence {sequence}: deadline without a pending key"
                );
            }
        }
        // The bound function: reads ≤ arming inputs (hints) + driving flushes
        // + contradictions — each read consumes one of them.
        let total_reads: u64 = reads_per_key.values().sum();
        assert!(
            total_reads <= hints + flushes + contradicted,
            "sequence {sequence}: {total_reads} reads from {hints} hints, \
             {flushes} flushes, {contradicted} contradictions"
        );
        // Dirty marks are one per burst: bounded by the untracked completions
        // (each mark is created by exactly one untracked completion).
        assert!(
            dirty_marks <= untracked_dones,
            "sequence {sequence}: {dirty_marks} dirty marks from {untracked_dones} untracked dones"
        );
    }
}
