//! The chokepoint (spec P1.1, FR-1/FR-2, AC-1): every interaction passes
//! through exactly one `decide` before any handler logic runs. Exhaustively
//! tested against every handler (tests/guard.rs) with recording fakes.

use ocbot_core::{ActorId, Surface};

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

/// Tied to the operator and the operator's channel(s). Identity is a set of
/// namespaced `ActorId`s (SPEC-004 D-03): one operator identity per surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Guard {
    /// The allowed `ActorId`s (`discord:42`, `signal:+1555…`), built at boot
    /// from config.
    pub allowed_actors: Vec<ActorId>,
    /// The ops channel (DMs with the operator are always allowed).
    pub ops_channel: Option<u64>,
    /// The inbox channel for external completions; also an allowed location
    /// (P14). Falls back to `ops_channel` when unset.
    pub inbox_channel: Option<u64>,
}

impl Guard {
    /// The only guard function. Order matters: user identity first (FR-1),
    /// then location (FR-2; DMs, the ops channel, and the inbox channel are
    /// the allowed surfaces).
    pub fn decide(&self, source: &InteractionSource) -> Decision {
        if !self.allows(&source.user_id) {
            return Decision::EphemeralDeny;
        }
        if source.is_dm {
            return Decision::Allow;
        }
        // Location is compared on the opaque chat reference (SPEC-004 D-02):
        // the ops/inbox channels are Discord snowflakes, so a non-Discord
        // surface (always `is_dm`, handled above) never reaches here.
        let chat_ref = source.destination.chat_ref.0.as_str();
        if self
            .ops_channel
            .is_some_and(|channel| channel.to_string() == chat_ref)
        {
            return Decision::Allow;
        }
        if self
            .inbox_channel
            .is_some_and(|channel| channel.to_string() == chat_ref)
        {
            return Decision::Allow;
        }
        Decision::Ignore
    }

    /// Whether `actor` is an authorized operator identity.
    pub fn allows(&self, actor: &str) -> bool {
        self.allowed_actors.iter().any(|allowed| allowed.0 == actor)
    }

    /// The single configured operator's `ActorId` (D-03: one operator per
    /// surface; a discord-only deployment has exactly one). Unreachable in
    /// configured deployments — a guard with no actors denies everything, and
    /// the placeholder never authorizes (`allows` matches nothing).
    pub fn operator(&self) -> ActorId {
        self.allowed_actors
            .first()
            .cloned()
            .unwrap_or_else(|| ActorId::from_parts(Surface::Discord, "0"))
    }

    /// The FR-3 mention allowlist for this guard's configured identities:
    /// every configured Discord snowflake, and nothing on any other surface
    /// (which has no mention mechanism). The Discord adapter enforces the
    /// restriction structurally on top of this.
    pub fn mention_allowlist(&self) -> Vec<u64> {
        self.allowed_actors
            .iter()
            .filter_map(ocbot_core::ActorId::discord_id)
            .collect()
    }

    /// The Discord snowflake of the operator, when a Discord identity is
    /// configured (the adapter's payload lock).
    pub fn discord_operator(&self) -> Option<u64> {
        self.mention_allowlist().into_iter().next()
    }

    /// The channel external completions post to.
    pub fn inbox(&self) -> Option<u64> {
        self.inbox_channel.or(self.ops_channel)
    }
}

#[cfg(test)]
mod tests {
    use super::{Decision, Guard};
    use crate::app::interaction::{InteractionSource, SourceKind};
    use ocbot_core::{ActorId, Surface};

    fn source(user: &str, channel: u64, is_dm: bool) -> InteractionSource {
        InteractionSource {
            kind: SourceKind::SlashCommand {
                name: "run".to_owned(),
            },
            user_id: user.to_owned(),
            destination: ocbot_core::Destination::discord(channel),
            guild_id: (!is_dm).then_some(1),
            message_id: None,
            is_dm,
        }
    }

    fn guard() -> Guard {
        Guard {
            allowed_actors: vec![ActorId::from_parts(Surface::Discord, "42")],
            ops_channel: Some(7),
            inbox_channel: None,
        }
    }

    #[test]
    fn operator_in_dm_or_ops_channel_is_allowed() {
        let guard = guard();
        assert_eq!(
            guard.decide(&source("discord:42", 99, true)),
            Decision::Allow
        );
        assert_eq!(
            guard.decide(&source("discord:42", 7, false)),
            Decision::Allow
        );
    }

    #[test]
    fn wrong_user_is_denied_everywhere() {
        let guard = guard();
        assert_eq!(
            guard.decide(&source("discord:1", 99, true)),
            Decision::EphemeralDeny
        );
        assert_eq!(
            guard.decide(&source("discord:1", 7, false)),
            Decision::EphemeralDeny
        );
    }

    #[test]
    fn foreign_channel_is_ignored() {
        let guard = guard();
        assert_eq!(
            guard.decide(&source("discord:42", 8, false)),
            Decision::Ignore
        );
    }

    #[test]
    fn dm_only_mode_allows_dm_and_ignores_channels() {
        let guard = Guard {
            allowed_actors: vec![ActorId::from_parts(Surface::Discord, "42")],
            ops_channel: None,
            inbox_channel: None,
        };
        assert_eq!(
            guard.decide(&source("discord:42", 5, true)),
            Decision::Allow
        );
        assert_eq!(
            guard.decide(&source("discord:42", 5, false)),
            Decision::Ignore
        );
    }

    #[test]
    fn identity_matrix_covers_both_namespaces() {
        // FR-101: a signal actor is a distinct identity; only the configured
        // set authorizes, regardless of namespace.
        let guard = Guard {
            allowed_actors: vec![
                ActorId::from_parts(Surface::Discord, "42"),
                ActorId::from_parts(Surface::Signal, "+15551234567"),
            ],
            ops_channel: None,
            inbox_channel: None,
        };
        assert!(guard.allows("discord:42"));
        assert!(guard.allows("signal:+15551234567"));
        assert!(!guard.allows("signal:+15550000000"));
        assert!(!guard.allows("42"));
        assert_eq!(guard.discord_operator(), Some(42));
        assert_eq!(
            guard.operator(),
            ActorId::from_parts(Surface::Discord, "42")
        );
    }
}
