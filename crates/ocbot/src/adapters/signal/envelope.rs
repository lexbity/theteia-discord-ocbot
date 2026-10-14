//! Tolerant envelope types for the signal-cli receive stream (SPEC-004 §5.9,
//! D-02/D-03 at the Signal boundary).
//!
//! The wire half (`Wire*` types) decodes with the opencode-client's
//! tolerant-serde discipline: unknown fields are *ignored*, never rejected —
//! signal-cli's schema is advisory, not contractual, and a daemon upgrade
//! adding a field must not take the surface down (the drift rule counts
//! undecodable envelopes, not unknown keys).
//!
//! The normalized half ([`Envelope`]) is what the app layer sees: one sender,
//! one timestamp, optional text/reaction/quote/attachments, and the group
//! flag FR-104's policy rejects later (parsed now, decided in PH-4).

use serde::Deserialize;

/// One line from the `subscribe` stream, decoded tolerantly. Built only via
/// [`Envelope::decode`] — the wire shape is never deserialized directly.
#[derive(Debug, Clone)]
pub struct Envelope {
    /// The sender's E.164 number (`sourceNumber`, falling back to `source`).
    pub sender: String,
    /// The envelope timestamp in unix **milliseconds**.
    pub timestamp_ms: u64,
    /// The message text, when a data message carried one.
    pub text: Option<String>,
    /// The reaction this envelope represents, when it is one.
    pub reaction: Option<Reaction>,
    /// The quoted message, when replying to one.
    pub quote: Option<Quote>,
    /// Attachments (local paths after the daemon's download, when it did).
    pub attachments: Vec<Attachment>,
    /// Whether the envelope names a group conversation (FR-104: policy
    /// rejects it later; the parse keeps it visible).
    pub is_group: bool,
    /// The receiving account signal-cli attributed the envelope to.
    pub account: Option<String>,
}

/// A reaction (or reaction removal) on a target message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reaction {
    /// The reaction emoji.
    pub emoji: String,
    /// The sent timestamp of the target message (unix milliseconds).
    pub target_timestamp_ms: u64,
    /// Whether this removes a reaction instead of adding one.
    pub remove: bool,
}

/// A quote of an earlier message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quote {
    /// The target message's sent timestamp (unix milliseconds).
    pub target_timestamp_ms: u64,
    /// The quoted author, when the daemon reported one.
    pub author: Option<String>,
    /// The quoted text, when the daemon reported it.
    pub text: Option<String>,
}

/// One attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// The original filename, when known.
    pub filename: Option<String>,
    /// The path the daemon downloaded it to, when it did.
    pub local_path: Option<String>,
    /// The content type, when known.
    pub content_type: Option<String>,
}

/// The wire wrapper: the `result` of a `subscribe`d envelope
/// (`{ "envelope": {...}, "account": "+" }` in the pinned contract).
#[derive(Debug, Deserialize)]
struct Streamed {
    envelope: Option<WireEnvelope>,
    account: Option<String>,
}

/// The wire envelope itself. Wire names are camelCase, so every field that
/// crosses the boundary is renamed explicitly — a missing rename decodes as
/// `None` silently, which is exactly the failure mode this module exists to
/// pin (the corpus test asserts every field, not just the easy ones).
#[derive(Debug, Deserialize)]
struct WireEnvelope {
    source: Option<String>,
    #[serde(rename = "sourceNumber")]
    source_number: Option<String>,
    timestamp: Option<u64>,
    #[serde(rename = "dataMessage")]
    data_message: Option<WireDataMessage>,
    /// Group markers can sit at the envelope level or on the data message;
    /// both are kept visible for FR-104's policy decision.
    group: Option<serde_json::Value>,
    #[serde(rename = "groupV2")]
    group_v2: Option<serde_json::Value>,
    // Other message kinds signal-cli may stream (sync/receipt/typing) are
    // *not* declared: tolerant serde ignores unknown keys, which is exactly
    // the drift behaviour we want — a daemon that adds a kind decodes fine and
    // the envelope simply carries no text/reaction (PH-4's router ignores it).
}

/// The wire data message.
#[derive(Debug, Deserialize)]
struct WireDataMessage {
    timestamp: Option<u64>,
    message: Option<String>,
    #[serde(default)]
    attachments: Vec<WireAttachment>,
    quote: Option<WireQuote>,
    reaction: Option<WireReaction>,
    group: Option<serde_json::Value>,
    #[serde(rename = "groupV2")]
    group_v2: Option<serde_json::Value>,
}

/// One wire attachment. signal-cli downloads attachments for the JSON-RPC
/// daemon and names the local path it wrote (`localPath`).
#[derive(Debug, Deserialize)]
struct WireAttachment {
    #[serde(rename = "contentType")]
    content_type: Option<String>,
    filename: Option<String>,
    #[serde(rename = "localPath")]
    local_path: Option<String>,
}

/// The wire quote.
#[derive(Debug, Deserialize)]
struct WireQuote {
    id: Option<u64>,
    author: Option<String>,
    text: Option<String>,
}

/// The wire reaction (signal-cli ≥ 0.13 carries reactions as data messages).
#[derive(Debug, Deserialize)]
struct WireReaction {
    emoji: Option<String>,
    #[serde(rename = "targetSentTimestamp")]
    target_sent_timestamp: Option<u64>,
    #[serde(rename = "isRemove")]
    is_remove: Option<bool>,
}

impl Envelope {
    /// Decode one `subscribe` result value, collecting the reasons a line is
    /// not an envelope (they feed the drift rule as decode failures).
    pub fn decode(value: &serde_json::Value) -> Result<Self, String> {
        // The pinned wrapper shape; a bare envelope is tolerated too (a daemon
        // that streams the envelope object directly still decodes).
        let (wire, account) = match serde_json::from_value::<Streamed>(value.clone()) {
            Ok(streamed) => match streamed.envelope {
                Some(wire) => (wire, streamed.account),
                None => (bare(value)?, streamed.account),
            },
            Err(_) => (bare(value)?, None),
        };
        let sender = wire
            .source_number
            .or(wire.source)
            .filter(|sender| !sender.is_empty())
            .ok_or("envelope has no source number")?;
        let timestamp_ms = wire
            .data_message
            .as_ref()
            .and_then(|data| data.timestamp)
            .or(wire.timestamp)
            .ok_or("envelope has no timestamp")?;
        let (text, reaction, quote, attachments, group) = match wire.data_message {
            Some(data) => (
                data.message,
                data.reaction.and_then(decode_reaction),
                data.quote.and_then(decode_quote),
                data.attachments
                    .into_iter()
                    .map(decode_attachment)
                    .collect(),
                data.group.is_some() || data.group_v2.is_some(),
            ),
            None => (None, None, None, Vec::new(), false),
        };
        let is_group = group || wire.group.is_some() || wire.group_v2.is_some();
        Ok(Self {
            sender,
            timestamp_ms,
            text,
            reaction,
            quote,
            attachments,
            is_group,
            account,
        })
    }
}

/// Decode a bare wire envelope (no `{envelope, account}` wrapper).
fn bare(value: &serde_json::Value) -> Result<WireEnvelope, String> {
    serde_json::from_value(value.clone()).map_err(|error| format!("not an envelope: {error}"))
}

/// A reaction without an emoji or a target is not a reaction.
fn decode_reaction(wire: WireReaction) -> Option<Reaction> {
    Some(Reaction {
        emoji: wire.emoji?,
        target_timestamp_ms: wire.target_sent_timestamp?,
        remove: wire.is_remove.unwrap_or(false),
    })
}

/// A quote without a target timestamp is not a quote.
fn decode_quote(wire: WireQuote) -> Option<Quote> {
    Some(Quote {
        target_timestamp_ms: wire.id?,
        author: wire.author,
        text: wire.text,
    })
}

fn decode_attachment(wire: WireAttachment) -> Attachment {
    Attachment {
        filename: wire.filename,
        local_path: wire.local_path,
        content_type: wire.content_type,
    }
}

#[cfg(test)]
mod tests {
    use super::Envelope;

    #[test]
    fn plain_text_message_decodes() {
        let value = serde_json::json!({
            "envelope": {
                "source": "+15550000001",
                "sourceNumber": "+15550000001",
                "timestamp": 1_700_000_000_000_u64,
                "dataMessage": {
                    "timestamp": 1_700_000_000_000_u64,
                    "message": "status?"
                }
            },
            "account": "+15551234567"
        });
        let envelope = Envelope::decode(&value).expect("decodes");
        assert_eq!(envelope.sender, "+15550000001");
        assert_eq!(envelope.timestamp_ms, 1_700_000_000_000u64);
        assert_eq!(envelope.text.as_deref(), Some("status?"));
        assert_eq!(envelope.account.as_deref(), Some("+15551234567"));
        assert!(!envelope.is_group);
        assert!(envelope.reaction.is_none());
    }

    #[test]
    fn junk_fields_are_ignored_not_rejected() {
        let value = serde_json::json!({
            "envelope": {
                "source": "+15550000001",
                "timestamp": 1,
                "futureField": {"nested": true},
                "dataMessage": {"message": "hi", "brandNewKey": [1, 2, 3]}
            },
            "somethingElse": "drift"
        });
        assert_eq!(
            Envelope::decode(&value).expect("tolerant").text.as_deref(),
            Some("hi")
        );
    }

    #[test]
    fn undecodable_lines_name_the_reason() {
        let missing_sender = serde_json::json!({"envelope": {"timestamp": 1}});
        assert!(
            Envelope::decode(&missing_sender)
                .expect_err("no sender")
                .contains("source")
        );
        let missing_timestamp = serde_json::json!({"envelope": {"source": "+1"}});
        assert!(
            Envelope::decode(&missing_timestamp)
                .expect_err("no timestamp")
                .contains("timestamp")
        );
        let not_an_envelope = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": 7});
        assert!(Envelope::decode(&not_an_envelope).is_err());
    }

    #[test]
    fn group_flags_are_parsed_for_policy() {
        let wrapped = serde_json::json!({
            "envelope": {
                "source": "+15550000001",
                "timestamp": 1,
                "type": "GROUP",
                "dataMessage": {"message": "hello group", "groupV2": {"groupId": "Zm9v"}}
            }
        });
        assert!(Envelope::decode(&wrapped).expect("group decodes").is_group);

        let bare = serde_json::json!({
            "source": "+15550000001",
            "timestamp": 1,
            "group": {"id": "abc"},
            "dataMessage": {"message": "legacy group"}
        });
        assert!(
            Envelope::decode(&bare)
                .expect("bare group decodes")
                .is_group
        );
    }

    #[test]
    fn attachments_and_quotes_decode() {
        let value = serde_json::json!({
            "envelope": {
                "source": "+15550000002",
                "timestamp": 1_700_000_001_000_u64,
                "dataMessage": {
                    "message": "see attached",
                    "attachments": [
                        {"contentType": "image/png", "filename": "shot.png",
                         "id": "1", "localPath": "/var/lib/signal-cli/attach/shot.png"},
                        {"id": "2"}
                    ],
                    "quote": {"id": 1_699_999_999_999_u64, "author": "+15550000001",
                              "text": "status?"}
                }
            }
        });
        let envelope = Envelope::decode(&value).expect("decodes");
        assert_eq!(envelope.attachments.len(), 2);
        assert_eq!(
            envelope.attachments[0],
            super::Attachment {
                filename: Some("shot.png".to_owned()),
                local_path: Some("/var/lib/signal-cli/attach/shot.png".to_owned()),
                content_type: Some("image/png".to_owned()),
            }
        );
        let quote = envelope.quote.expect("quote");
        assert_eq!(quote.target_timestamp_ms, 1_699_999_999_999);
        assert_eq!(quote.author.as_deref(), Some("+15550000001"));
        assert_eq!(quote.text.as_deref(), Some("status?"));
    }

    #[test]
    fn reactions_decode_including_removals() {
        let add = serde_json::json!({
            "envelope": {
                "source": "+15550000003",
                "timestamp": 1_700_000_002_000_u64,
                "dataMessage": {
                    "reaction": {
                        "emoji": "\u{1F44D}",
                        "targetSentTimestamp": 1_699_999_999_999_u64,
                        "isRemove": false
                    }
                }
            }
        });
        let reaction = Envelope::decode(&add)
            .expect("decodes")
            .reaction
            .expect("reaction");
        assert_eq!(reaction.emoji, "\u{1F44D}");
        assert_eq!(reaction.target_timestamp_ms, 1_699_999_999_999);
        assert!(!reaction.remove);

        let remove = serde_json::json!({
            "envelope": {
                "source": "+15550000003",
                "timestamp": 3,
                "dataMessage": {
                    "reaction": {
                        "emoji": "\u{1F44D}",
                        "targetSentTimestamp": 1_699_999_999_999_u64,
                        "isRemove": true
                    }
                }
            }
        });
        assert!(
            Envelope::decode(&remove)
                .expect("decodes")
                .reaction
                .expect("removal")
                .remove
        );

        // A reaction without a target is not a reaction — the field degrades.
        let targetless = serde_json::json!({
            "envelope": {
                "source": "+15550000003",
                "timestamp": 3,
                "dataMessage": {"reaction": {"emoji": "x"}}
            }
        });
        assert!(
            Envelope::decode(&targetless)
                .expect("still decodes")
                .reaction
                .is_none()
        );
    }

    #[test]
    fn non_data_envelopes_decode_as_bare_facts() {
        let typing = serde_json::json!({
            "envelope": {
                "source": "+15550000004",
                "timestamp": 9,
                "typingMessage": {"action": "started"}
            }
        });
        let envelope = Envelope::decode(&typing).expect("decodes");
        assert!(envelope.text.is_none());
        assert!(!envelope.is_group);
    }
}
