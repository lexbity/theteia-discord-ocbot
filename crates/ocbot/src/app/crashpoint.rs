//! Debug-only crash checkpoints (AC-6, spec Phase 3).
//!
//! The accept / park / reset sequences call [`checkpoint`] after each labelled
//! step. In a debug build, if `OCBOT_TEST_CRASH_AFTER` names that label, the
//! process writes `<OCBOT_TEST_CRASH_DIR>/<label>.reached` and parks so a test
//! can deliver a genuine SIGKILL at exactly that point; the next boot's
//! reconcile must roll forward from DB/git facts alone. In a release build the
//! function is a no-op, so this cannot arm in production.
//!
//! `std::process::abort` was rejected: SIGKILL is the stronger claim (no
//! unwinding, no destructors, no signal handler) and a test can deliver it with
//! `Child::kill`, so we let the test own the kill and only park here.

/// The checkpoint labels, in sequence order, for the chaos tests.
pub const ACCEPT_CP: [&str; 4] = ["CP-A0", "CP-A1", "CP-A2", "CP-A3"];
/// Park checkpoints (CP-P0..CP-P4).
pub const PARK_CP: [&str; 5] = ["CP-P0", "CP-P1", "CP-P2", "CP-P3", "CP-P4"];
/// Reset checkpoints (CP-R0..CP-R2).
pub const RESET_CP: [&str; 3] = ["CP-R0", "CP-R1", "CP-R2"];

/// Arm the crash hook at `label`, if this is a debug build and the environment
/// asks for exactly this label.
#[cfg(debug_assertions)]
pub fn checkpoint(label: &str) {
    let Ok(target) = std::env::var("OCBOT_TEST_CRASH_AFTER") else {
        return;
    };
    if target != label {
        return;
    }
    let dir = std::env::var("OCBOT_TEST_CRASH_DIR").unwrap_or_else(|_| ".".to_owned());
    let dir = std::path::Path::new(&dir);
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(dir.join(format!("{label}.reached")), b"reached");
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3_600));
    }
}

/// Release builds compile the hook out entirely.
#[cfg(not(debug_assertions))]
pub fn checkpoint(_label: &str) {}
