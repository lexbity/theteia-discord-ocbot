//! Projected session messages ([`Message`] and the assistant snapshot used
//! for outcome classification in the bot's core).

use serde::{Deserialize, Serialize};

use crate::Client;
use crate::error::OcError;
use crate::paths;
use crate::sessions::{Data, ModelRef, TokenUsage};

/// A projected session message, discriminated by the `type` tag.
///
/// The wire carries more variants than the bot needs (`system`, `shell`,
/// `compaction`, `agent-switched`, ...). Those decode into [`Message::Other`]
/// and are ignored downstream — that is the tolerant-serde contract (P16).
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type")]
pub enum Message {
    /// A user message.
    #[serde(rename = "user")]
    User(Box<UserMessage>),
    /// An assistant message (the report candidate).
    #[serde(rename = "assistant")]
    Assistant(Box<AssistantMessage>),
    /// An idle marker carrying the last outcome.
    #[serde(rename = "idle")]
    Idle(Box<IdleMessage>),
    /// Any other message type; tolerated and ignored.
    #[serde(other)]
    Other,
}

/// A user-authored message.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    /// The message id.
    #[serde(default)]
    pub id: String,
    /// Timestamps.
    #[serde(default)]
    pub time: Option<TimeInfo>,
    /// The prompt text.
    #[serde(default)]
    pub text: String,
}

/// An assistant (model) message.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    /// The message id.
    #[serde(default)]
    pub id: String,
    /// Timestamps.
    #[serde(default)]
    pub time: Option<TimeInfo>,
    /// The agent that produced the message.
    #[serde(default)]
    pub agent: Option<String>,
    /// The model that produced the message.
    #[serde(default)]
    pub model: Option<ModelRef>,
    /// Content parts (text / reasoning / tool).
    #[serde(default)]
    pub content: Vec<ContentPart>,
    /// The finish reason.
    #[serde(default)]
    pub finish: Option<Finish>,
    /// Cost in USD.
    #[serde(default)]
    pub cost: Option<f64>,
    /// Token usage.
    #[serde(default)]
    pub tokens: Option<TokenUsage>,
    /// A structured error, if the step errored.
    #[serde(default)]
    pub error: Option<StructuredError>,
    /// The raw provider finish reason.
    #[serde(default)]
    pub raw_finish: Option<String>,
}

impl AssistantMessage {
    /// Concatenated text content (report body candidate).
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<&str>>()
            .join("\n")
    }

    /// Whether the step made any tool calls.
    pub fn has_tool_calls(&self) -> bool {
        self.content
            .iter()
            .any(|part| matches!(part, ContentPart::Tool))
    }

    /// The structured error message, if the step failed.
    pub fn error_message(&self) -> Option<&str> {
        self.error.as_ref().map(|error| error.message.as_str())
    }
}

/// A content part of an assistant message.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    /// A text part.
    Text {
        /// The rendered text.
        #[serde(default)]
        text: String,
    },
    /// A reasoning part (kept for structure; not used in reports).
    Reasoning,
    /// A tool-call part.
    Tool,
    /// An unknown content part; tolerated and ignored.
    #[serde(other)]
    Other,
}

/// The idle marker emitted when a session finishes executing.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IdleMessage {
    /// The message id.
    #[serde(default)]
    pub id: String,
    /// Timestamps.
    #[serde(default)]
    pub time: Option<TimeInfo>,
    /// The terminal outcome: `succeeded`, `failed`, `interrupted`.
    #[serde(default)]
    pub outcome: Option<String>,
}

/// Message timestamps (epoch milliseconds).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TimeInfo {
    /// Creation time.
    #[serde(default)]
    pub created: f64,
    /// Stream start time.
    #[serde(default)]
    pub streamed: Option<f64>,
    /// Completion time.
    #[serde(default)]
    pub completed: Option<f64>,
    /// Idle time.
    #[serde(default)]
    pub idle: Option<f64>,
}

/// The model finish reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Finish {
    /// The model stopped normally.
    Stop,
    /// The model hit its output length limit.
    Length,
    /// The step ended by issuing tool calls.
    ToolCalls,
    /// Provider content filtering stopped the step.
    ContentFilter,
    /// The step ended in an error.
    Error,
    /// Unknown or unsupported finish reason.
    Unknown,
    /// A forward-compatible catch-all.
    #[serde(other)]
    Other,
}

/// A structured error carried on assistant messages.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct StructuredError {
    /// Human-readable error text.
    #[serde(default)]
    pub message: String,
    /// The error kind.
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    /// An optional HTTP-ish status code.
    #[serde(default)]
    pub status: Option<u16>,
}

impl Client {
    /// The projected messages of a session, newest first with the requested
    /// `type` filter (or all types when `None`).
    pub async fn messages(
        &self,
        session_id: &str,
        message_type: Option<&str>,
    ) -> Result<Vec<Message>, OcError> {
        let mut query: Vec<(&str, &str)> = vec![("order", "desc"), ("limit", "100")];
        if let Some(message_type) = message_type {
            query.push(("type", message_type));
        }
        let path = paths::session_message(session_id);
        let response: Data<Vec<Message>> = self.get(&path, &query).await?;
        Ok(response.data)
    }

    /// The most recent assistant message of a session, if any (spec A7: the
    /// report input; tokens/cost/error ride along).
    pub async fn last_assistant(
        &self,
        session_id: &str,
    ) -> Result<Option<AssistantMessage>, OcError> {
        let messages = self.messages(session_id, Some("assistant")).await?;
        Ok(messages.into_iter().find_map(|message| match message {
            Message::Assistant(assistant) => Some(*assistant),
            _ => None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{AssistantMessage, ContentPart, Finish};

    const ASSISTANT_JSON: &str = r#"{
        "id": "msg_x",
        "time": {"created": 1, "streamed": 2, "completed": 3},
        "type": "assistant",
        "agent": "build",
        "model": {"id": "m", "providerID": "p"},
        "content": [
            {"type": "text", "text": "PONG"},
            {"type": "reasoning", "text": "(thinking)"},
            {"type": "tool", "id": "t1", "name": "bash"}
        ],
        "snapshot": {"start": "a", "end": "b", "files": []},
        "finish": "stop",
        "rawFinish": "end_turn",
        "cost": 0.0001,
        "tokens": {"input": 1, "output": 2, "reasoning": 0, "cache": {"read": 0, "write": 0}}
    }"#;

    #[test]
    fn assistant_message_decodes_tolerantly() {
        let message: AssistantMessage = serde_json::from_str(ASSISTANT_JSON).expect("decode");
        assert_eq!(message.text(), "PONG");
        assert_eq!(message.finish, Some(Finish::Stop));
        assert!(message.has_tool_calls());
        assert_eq!(message.error_message(), None);
    }

    #[test]
    fn unknown_content_parts_are_tolerated() {
        let json = r#"{"id":"m","time":{"created":1},"type":"assistant","agent":"a",
            "model":{"id":"m","providerID":"p"},"content":[{"type":"brainwave","amplitude":1}]}"#;
        let message: AssistantMessage = serde_json::from_str(json).expect("decode");
        assert!(matches!(message.content[0], ContentPart::Other));
    }

    #[test]
    fn enums_carry_other_variants() {
        let finish: Finish = serde_json::from_str(r#""pre-empted""#).expect("decode");
        assert_eq!(finish, Finish::Other);
    }
}
