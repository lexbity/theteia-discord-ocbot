//! The chokepoint (spec P1.1, FR-1/FR-2, AC-1): every interaction passes
//! through exactly one `decide` before any handler logic runs. Exhaustively
//! tested against every handler (tests/guard.rs) with recording fakes.

/// Outcome of the guard for one interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Route to the handler.
    Allow,
    /// Reject with an ephemeral "not authorized" (FR-1).
    EphemeralDeny,
    /// Foreign channel / out of scope: ack with no side effects (FR-2).
    Ignore,
}

use crate::app::interaction::InteractionSource;

/// Tied to the operator and the operator's channel(s).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guard {
    /// The single allowed Discord user id.
    pub allowed_user_id: u64,
    /// The ops channel (DMs with the operator are always allowed).
    pub ops_channel: Option<u64>,
}

impl Guard {
    /// The only guard function. Order matters: user identity first (FR-1),
    /// then location (FR-2).
    pub fn decide(&self, source: &InteractionSource) -> Decision {
        if source.user_id != self.allowed_user_id {
            return Decision::EphemeralDeny;
        }
        if source.is_dm {
            return Decision::Allow;
        }
        if self
            .ops_channel
            .is_some_and(|channel| source.channel_id == channel)
        {
            return Decision::Allow;
        }
        Decision::Ignore
    }
}

#[cfg(test)]
mod tests {
    use super::{Decision, Guard};
    use crate::app::interaction::{InteractionSource, SourceKind};

    fn source(user: u64, channel: u64, is_dm: bool) -> InteractionSource {
        InteractionSource {
            kind: SourceKind::SlashCommand {
                name: "run".to_owned(),
            },
            user_id: user,
            channel_id: channel,
            guild_id: (!is_dm).then_some(1),
            message_id: None,
            is_dm,
        }
    }

    #[test]
    fn operator_in_dm_or_ops_channel_is_allowed() {
        let guard = Guard {
            allowed_user_id: 42,
            ops_channel: Some(7),
        };
        assert_eq!(guard.decide(&source(42, 99, true)), Decision::Allow);
        assert_eq!(guard.decide(&source(42, 7, false)), Decision::Allow);
    }

    #[test]
    fn wrong_user_is_denied_everywhere() {
        let guard = Guard {
            allowed_user_id: 42,
            ops_channel: Some(7),
        };
        assert_eq!(guard.decide(&source(1, 99, true)), Decision::EphemeralDeny);
        assert_eq!(guard.decide(&source(1, 7, false)), Decision::EphemeralDeny);
    }

    #[test]
    fn foreign_channel_is_ignored() {
        let guard = Guard {
            allowed_user_id: 42,
            ops_channel: Some(7),
        };
        assert_eq!(guard.decide(&source(42, 8, false)), Decision::Ignore);
    }

    #[test]
    fn dm_only_mode_allows_dm_and_ignores_channels() {
        let guard = Guard {
            allowed_user_id: 42,
            ops_channel: None,
        };
        assert_eq!(guard.decide(&source(42, 5, true)), Decision::Allow);
        assert_eq!(guard.decide(&source(42, 5, false)), Decision::Ignore);
    }
}
