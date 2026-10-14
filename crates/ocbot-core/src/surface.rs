//! The surface vocabulary (SPEC-004 §5.2): the core addresses chat platforms
//! through opaque, namespaced strings instead of Discord snowflakes. Adapters
//! parse/serialize at their boundary (D-02); the persisted columns were
//! already `TEXT`, so the DB gets more type-honest, not less.

use serde::{Deserialize, Serialize};

/// A chat platform adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Surface {
    /// Discord, via serenity.
    Discord,
    /// Signal, via the signal-cli JSON-RPC daemon.
    Signal,
}

impl Surface {
    /// Parse a config/receipt spelling.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "discord" => Some(Surface::Discord),
            "signal" => Some(Surface::Signal),
            _ => None,
        }
    }

    /// The canonical spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Surface::Discord => "discord",
            Surface::Signal => "signal",
        }
    }
}

impl std::fmt::Display for Surface {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Where a message goes: on Discord a channel snowflake, on Signal the
/// operator's E.164 number.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChatRef(pub String);

impl ChatRef {
    /// Render as `surface:ref` (the receipt spelling, D-08).
    pub fn prefixed(self, surface: Surface) -> String {
        format!("{}:{}", surface.as_str(), self.0)
    }
}

impl std::fmt::Display for ChatRef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A delivered message's id: a snowflake on Discord, a timestamp on Signal.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MsgId(pub String);

impl std::fmt::Display for MsgId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A namespaced identity string (`discord:42`, `signal:+1555…`) — the only
/// user-identity shape the core sees (D-03).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ActorId(pub String);

impl ActorId {
    /// Build from parts.
    pub fn from_parts(surface: Surface, id: &str) -> Self {
        ActorId(format!("{}:{id}", surface.as_str()))
    }

    /// The surface namespace, when the string is well-formed.
    pub fn surface(&self) -> Option<Surface> {
        self.0
            .split_once(':')
            .and_then(|(surface, _)| Surface::parse(surface))
    }

    /// The id without its namespace prefix.
    pub fn local(&self) -> &str {
        self.0.split_once(':').map_or(&self.0, |(_, id)| id)
    }

    /// The Discord snowflake, when this is a Discord actor.
    pub fn discord_id(&self) -> Option<u64> {
        match self.surface() {
            Some(Surface::Discord) => self.local().parse().ok(),
            _ => None,
        }
    }
}

impl std::fmt::Display for ActorId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One delivery target.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Destination {
    /// The surface the chat lives on.
    pub surface: Surface,
    /// The platform's conversation reference (opaque to the core, D-02).
    pub chat_ref: ChatRef,
}

impl Destination {
    /// A Discord channel destination.
    pub fn discord(channel: u64) -> Self {
        Destination {
            surface: Surface::Discord,
            chat_ref: ChatRef(channel.to_string()),
        }
    }
}

/// Proof of one delivery, used by `msg_map`/`notified`/`component_map`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PostReceipt {
    /// The surface the message landed on.
    pub surface: Surface,
    /// The platform's message id.
    pub msg_id: MsgId,
}

impl PostReceipt {
    /// The `surface:id` receipt spelling (D-08's `notified.receipts` format).
    pub fn to_receipt_string(&self) -> String {
        format!("{}:{}", self.surface.as_str(), self.msg_id.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{ActorId, Destination, MsgId, PostReceipt, Surface};

    #[test]
    fn surface_round_trips() {
        for surface in [Surface::Discord, Surface::Signal] {
            assert_eq!(Surface::parse(surface.as_str()), Some(surface));
        }
        assert_eq!(Surface::parse("telegram"), None);
    }

    #[test]
    fn actor_id_namespacing() {
        let actor = ActorId::from_parts(Surface::Discord, "42");
        assert_eq!(actor.0, "discord:42");
        assert_eq!(actor.surface(), Some(Surface::Discord));
        assert_eq!(actor.local(), "42");
        assert_eq!(actor.discord_id(), Some(42));

        let actor = ActorId::from_parts(Surface::Signal, "+15551234567");
        assert_eq!(actor.discord_id(), None);
        assert_eq!(actor.surface(), Some(Surface::Signal));
    }

    #[test]
    fn destination_and_receipt_spellings() {
        let dest = Destination::discord(123);
        assert_eq!(dest.chat_ref.0, "123");
        assert_eq!(
            dest.chat_ref.clone().prefixed(Surface::Discord),
            "discord:123"
        );

        let receipt = PostReceipt {
            surface: Surface::Discord,
            msg_id: MsgId("456".to_owned()),
        };
        assert_eq!(receipt.to_receipt_string(), "discord:456");
    }
}
