//! Port adapters: the opencode client, git (argv-only), SQLite, the clock, and
//! serenity.

pub mod clock;
pub mod discord;
pub mod fs;
pub mod git;
pub mod opencode;
pub mod store;

/// Current wall-clock time as unix seconds, saturating on overflow.
///
/// The single shared clock helper for the adapter and app layers (SPEC-002
/// D-7): [`clock::SystemClock`] and the store actor both derive from it so the
/// epoch conversion exists in exactly one place.
pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}
