//! The permanent per-session log (`/api/experimental/session/{id}/log`).
//!
//! Phase 0 pinned (SPIKE-0.md, S-2) that in opencode `2.0.24` the durable log
//! endpoint emits at least a `log.synced` head marker and neither replays nor
//! streams deltas; callers treat the durable cursor as an optimization only
//! (spec P9) and reconcile from status facts. The volatile global event
//! stream lives in [`crate::events`]; both parse through the shared
//! [`SseReader`](crate::events::SseReader).

use futures_util::StreamExt;

use crate::Client;
use crate::error::OcError;
use crate::events::SseReader;
use crate::paths;

/// One entry replayed or streamed from the durable session log.
#[derive(Debug, Clone, PartialEq)]
pub struct LogEntry {
    /// The durable sequence number, when the entry carries one.
    pub seq: Option<u64>,
    /// The log event kind (e.g. `log.synced`).
    pub kind: String,
    /// The full decoded payload.
    pub raw: serde_json::Value,
}

/// A durable-log subscription for one session.
pub struct LogStream {
    reader: SseReader,
}

impl LogStream {
    /// Create a stream from an already-constructed server response.
    pub fn new(response: reqwest::Response) -> Self {
        Self {
            reader: SseReader::new(response.bytes_stream().boxed()),
        }
    }

    /// Await the next log entry. `None` means the server closed the stream.
    pub async fn next(&mut self) -> Option<Result<LogEntry, OcError>> {
        loop {
            let frame = self.reader.next_frame().await?;
            let frame = match frame {
                Ok(frame) => frame,
                Err(error) => return Some(Err(error)),
            };
            if frame.data.trim().is_empty() {
                continue; // comment/heartbeat frames carry nothing
            }
            let raw: serde_json::Value = match serde_json::from_str(&frame.data) {
                Ok(raw) => raw,
                Err(error) => return Some(Err(OcError::Decode(error))),
            };
            let kind = raw
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown")
                .to_owned();
            let seq = raw.get("seq").and_then(serde_json::Value::as_u64);
            return Some(Ok(LogEntry { seq, kind, raw }));
        }
    }
}

impl Client {
    /// Subscribe to the durable log of a session at `after` (a sequence
    /// number), optionally following for new entries. Phase 0 pinned that the
    /// endpoint yields the `log.synced` head marker; callers MUST NOT build
    /// correctness guarantees on entry replay (spec P9, A13).
    pub async fn log_after(
        &self,
        session_id: &str,
        after: Option<u64>,
        follow: bool,
    ) -> Result<LogStream, OcError> {
        let mut query: Vec<(String, String)> = Vec::new();
        if let Some(seq) = after {
            query.push(("after".to_owned(), seq.to_string()));
        }
        if follow {
            query.push(("follow".to_owned(), "true".to_owned()));
        }
        let references: Vec<(&str, &str)> = query
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        let response = self
            .get_stream(&paths::session_log(session_id), &references)
            .await?;
        Ok(LogStream::new(response))
    }
}
