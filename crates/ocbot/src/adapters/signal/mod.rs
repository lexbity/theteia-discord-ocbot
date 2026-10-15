//! The signal-cli surface adapter (SPEC-004 PH-3): a hand-rolled JSON-RPC
//! client over the daemon's unix socket (D-10), tolerant envelope types, and
//! the connection machine that owns the socket honestly — connect → subscribe
//! → pump → backoff, mirroring SPEC-003's supervisor discipline (§5.9).
//!
//! `rpc` owns the wire, `envelope` the inbound shapes, `adapter` the outbound
//! `ChatPort`, `inbound` the pump that normalizes envelopes into router
//! interactions, and `flows` the pending-input state machine that stands in
//! for Discord modals (D-06/§5.6).
//!
//! The wire shapes this module pins are the documented signal-cli JSON-RPC
//! subset (`version`, `listAccounts`, `subscribe`, `send`, `sendReaction`),
//! captured into `tests/common/signal_fake.rs` — the fake is the pinned
//! contract. `KNOWN_CAPS.md` records what a live capture would supersede.

pub mod adapter;
pub mod envelope;
pub mod flows;
pub mod inbound;
pub mod rpc;

pub use adapter::SignalAdapter;
pub use envelope::Envelope;
pub use flows::{Advance, FlowEngine, FlowField, ModalDraft, RunDraft, advance, run_draft};
pub use inbound::SignalInbound;
pub use rpc::{ConnectionSlot, RpcError, SignalMachine, SignalRpc, signal_backoff};

/// The oldest signal-cli daemon this bot speaks (FR-114).
///
/// No live daemon was available at capture time, so this is pinned to the
/// documented JSON-RPC subset rather than a measured smoke test; the boot
/// doctor enforces it against `version`. See `KNOWN_CAPS.md`.
pub const MIN_SIGNAL_CLI_VERSION: &str = "0.15.0";

/// A parsed `major.minor.patch` triple, ordered for the FR-114 gate. A
/// trailing suffix (`-SNAPSHOT`, `-rc1`) is tolerated and ignored: signal-cli
/// nightlies carry one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SignalVersion {
    /// The major component.
    pub major: u64,
    /// The minor component.
    pub minor: u64,
    /// The patch component.
    pub patch: u64,
}

impl SignalVersion {
    /// Parse the leading semver triple out of a version string; `None` when no
    /// digit-led triple is present.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.trim().split(['.', '-', '+']);
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

impl std::fmt::Display for SignalVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::{MIN_SIGNAL_CLI_VERSION, SignalVersion};

    #[test]
    fn parses_plain_and_suffixed_triples() {
        assert_eq!(
            SignalVersion::parse("0.15.0"),
            Some(SignalVersion {
                major: 0,
                minor: 15,
                patch: 0
            })
        );
        assert_eq!(
            SignalVersion::parse("0.15.2-SNAPSHOT"),
            Some(SignalVersion {
                major: 0,
                minor: 15,
                patch: 2
            })
        );
        assert_eq!(
            SignalVersion::parse(" 1.2.3 "),
            Some(SignalVersion {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
        assert_eq!(SignalVersion::parse("banana"), None);
        assert_eq!(SignalVersion::parse("0.15"), None);
        assert_eq!(SignalVersion::parse(""), None);
    }

    #[test]
    fn orders_by_semver_precedence() {
        let old = SignalVersion::parse("0.13.0").expect("0.13.0");
        let min = SignalVersion::parse(MIN_SIGNAL_CLI_VERSION).expect("the pinned minimum");
        let new = SignalVersion::parse("0.16.0").expect("0.16.0");
        assert!(old < min, "{old} < {min}");
        assert!(min < new, "{min} < {new}");
        assert!(min <= min);
    }

    #[test]
    fn display_round_trips_the_triple() {
        let version = SignalVersion::parse("0.15.7").expect("0.15.7");
        assert_eq!(version.to_string(), "0.15.7");
    }
}
