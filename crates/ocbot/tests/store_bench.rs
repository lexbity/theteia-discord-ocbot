//! Critical-path benchmark for the typed store actor (SPEC-002 Phase 7).
//!
//! The store's hot paths are a read (`action`), a write (`set_stall_ping`), and
//! a compare-and-set (`cas_state`). This test drives each `N` times through the
//! actor and asserts each stays within a generous wall-clock budget — a
//! catastrophic-regression tripwire (a lock convoy, a per-call allocation
//! storm, or a scheduling pathology blows it). The repository has no committed
//! pre-refactor baseline to diff against, so the budget is absolute rather than
//! a 5% delta.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

use std::time::{Duration, Instant};

use ocbot::adapters::store::StoreHandle;
use ocbot_core::State;

/// Iterations per path.
const N: usize = 2_000;
/// Per-path wall-clock ceiling: the actor does this in milliseconds on any real
/// machine, so this only fires on a pathological regression.
const BUDGET: Duration = Duration::from_secs(30);

#[tokio::test]
async fn store_bench() {
    let store = StoreHandle::open_in_memory().expect("store");
    store
        .upsert_project("/tmp/bench", None, None)
        .await
        .expect("project");
    let run = store.insert_run("/tmp/bench", "free").await.expect("run");
    let action = store
        .insert_action(run, Some(7), "bench", "prompt", 1)
        .await
        .expect("action");

    // Read path.
    let start = Instant::now();
    for _ in 0..N {
        assert!(store.action(action).await.expect("read").is_some());
    }
    let read = start.elapsed();

    // Write path.
    let start = Instant::now();
    for _ in 0..N {
        store.set_stall_ping(action, 1).await.expect("write");
    }
    let write = start.elapsed();

    // CAS path (state-preserving; the row changes on every call).
    let start = Instant::now();
    for _ in 0..N {
        assert!(
            store
                .cas_state(action, State::Staged, State::Staged, &[])
                .await
                .expect("cas")
        );
    }
    let cas = start.elapsed();

    assert!(read < BUDGET, "read path took {read:?} for {N} ops");
    assert!(write < BUDGET, "write path took {write:?} for {N} ops");
    assert!(cas < BUDGET, "cas path took {cas:?} for {N} ops");
}
