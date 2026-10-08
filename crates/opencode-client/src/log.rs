//! Server-sent event (SSE) streams: the permanent per-session log and the
//! volatile global event stream.
//!
//! Phase 0 pinned (SPIKE-0.md, S-2) that in opencode `2.0.24` the durable log
//! endpoint emits at least a `log.synced` head marker; the live event stream
//! is `/api/event` (volatile by contract). Both parse through the same
//! [`SseReader`]; callers treat the durable cursor as an optimization only
//! (spec P9) and reconcile from status facts.

use bytes::Bytes;
use futures_util::{StreamExt, stream::BoxStream};

use crate::Client;
use crate::error::OcError;
use crate::paths;

/// A single parsed server-sent event frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    /// The optional SSE `id:` field.
    pub id: Option<String>,
    /// The optional SSE `event:` field.
    pub event: Option<String>,
    /// The joined `data:` lines.
    pub data: String,
}

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

/// A subscription to the global (volatile) event stream.
pub struct EventStream {
    reader: SseReader,
}

impl EventStream {
    /// Create a stream from an already-constructed server response.
    pub fn new(response: reqwest::Response) -> Self {
        Self {
            reader: SseReader::new(response.bytes_stream().boxed()),
        }
    }

    /// Await the next event. `None` means the server closed the stream.
    pub async fn next(&mut self) -> Option<Result<Event, OcError>> {
        loop {
            let frame = self.reader.next_frame().await?;
            let frame = match frame {
                Ok(frame) => frame,
                Err(error) => return Some(Err(error)),
            };
            if frame.data.trim().is_empty() {
                continue;
            }
            let data: serde_json::Value = match serde_json::from_str(&frame.data) {
                Ok(data) => data,
                Err(error) => return Some(Err(OcError::Decode(error))),
            };
            let kind = data
                .get("type")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown")
                .to_owned();
            let created = data.get("created").and_then(serde_json::Value::as_f64);
            let durable = data.get("durable").and_then(|value| match value {
                serde_json::Value::Object(map) => {
                    let aggregate_id = map
                        .get("aggregateID")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let seq = map.get("seq").and_then(serde_json::Value::as_u64);
                    let version = map.get("version").and_then(serde_json::Value::as_u64);
                    Some(Durable {
                        aggregate_id,
                        seq: seq.unwrap_or_default(),
                        version,
                    })
                }
                _ => None,
            });
            return Some(Ok(Event {
                id: frame.id,
                kind,
                created,
                data,
                durable,
            }));
        }
    }
}

/// One event from the global stream.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// The SSE event id (the `id` field).
    pub id: Option<String>,
    /// The event kind (e.g. `session.execution.started`).
    pub kind: String,
    /// The server `created` timestamp when present.
    pub created: Option<f64>,
    /// The full decoded payload.
    pub data: serde_json::Value,
    /// The durable-log position this event is part of, when the server
    /// attaches one to execution events.
    pub durable: Option<Durable>,
}

/// The `durable` object attached to execution events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Durable {
    /// The aggregate id (the session id).
    pub aggregate_id: String,
    /// The durable sequence number.
    pub seq: u64,
    /// The aggregate version.
    pub version: Option<u64>,
}

/// Incremental SSE parser: buffers raw bytes until the next complete frame and
/// decodes it. Tolerates `\n\n` and `\r\n\r\n` separators.
struct SseReader {
    inner: BoxStream<'static, reqwest::Result<Bytes>>,
    buffer: Vec<u8>,
    closed: bool,
}

impl SseReader {
    fn new(inner: BoxStream<'static, reqwest::Result<Bytes>>) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
            closed: false,
        }
    }

    async fn next_frame(&mut self) -> Option<Result<SseFrame, OcError>> {
        if let Some(frame) = take_frame(&mut self.buffer) {
            return Some(Ok(frame));
        }
        if self.closed {
            return None;
        }
        match self.inner.next().await {
            Some(Ok(chunk)) => {
                self.buffer.extend_from_slice(&chunk);
                if let Some(frame) = take_frame(&mut self.buffer) {
                    return Some(Ok(frame));
                }
                Box::pin(self.next_frame()).await
            }
            Some(Err(error)) => {
                self.closed = true;
                Some(Err(OcError::Transport(error)))
            }
            None => {
                self.closed = true;
                take_frame(&mut self.buffer).map(Ok)
            }
        }
    }
}

/// Split one complete SSE frame (through the blank-line separator) out of the
/// buffer, if present.
fn take_frame(buffer: &mut Vec<u8>) -> Option<SseFrame> {
    let end = find_separator(buffer)?;
    let block: Vec<u8> = buffer.drain(..end).collect();
    let text = String::from_utf8_lossy(&block);
    let mut id = None;
    let mut event = None;
    let mut data_lines: Vec<&str> = Vec::new();
    for mut line in text.lines() {
        if let Some(stripped) = line.strip_suffix('\r') {
            line = stripped;
        }
        if let Some(rest) = line.strip_prefix("id:") {
            id = Some(rest.trim_start().to_owned());
        } else if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim_start().to_owned());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start());
        }
    }
    Some(SseFrame {
        id,
        event,
        data: data_lines.join("\n"),
    })
}

/// Locate the blank-line separator of an SSE frame; returns the index just
/// past it. Handles `\n\n` and `\r\n\r\n`.
fn find_separator(buffer: &[u8]) -> Option<usize> {
    let mut index = 0;
    while index + 1 < buffer.len() {
        if buffer[index] == b'\n' && buffer[index + 1] == b'\n' {
            return Some(index + 2);
        }
        if index + 3 < buffer.len()
            && buffer[index] == b'\r'
            && buffer[index + 1] == b'\n'
            && buffer[index + 2] == b'\r'
            && buffer[index + 3] == b'\n'
        {
            return Some(index + 4);
        }
        index += 1;
    }
    None
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

    /// Subscribe to the global event stream (`/api/event`). Volatile by
    /// contract: disconnect means missed events; callers reconcile from
    /// status facts afterwards (spec P9).
    pub async fn subscribe_events(&self) -> Result<EventStream, OcError> {
        let response = self.get_stream(paths::EVENT, &[]).await?;
        Ok(EventStream::new(response))
    }
}

#[cfg(test)]
mod tests {
    use super::{SseFrame, take_frame};

    #[test]
    fn parses_data_only_frame() {
        let mut buffer = b"data: {\"a\":1}\n\n".to_vec();
        let frame = take_frame(&mut buffer).expect("frame");
        assert_eq!(frame.data, "{\"a\":1}");
        assert!(buffer.is_empty());
    }

    #[test]
    fn parses_id_event_and_multiple_data_lines() {
        let mut buffer = b"id: 1\nevent: x\ndata: a\ndata: b\n\n".to_vec();
        let frame = take_frame(&mut buffer).expect("frame");
        assert_eq!(
            frame,
            SseFrame {
                id: Some("1".to_owned()),
                event: Some("x".to_owned()),
                data: "a\nb".to_owned(),
            }
        );
    }

    #[test]
    fn handles_crlf_separators() {
        let mut buffer = b"data: x\r\n\r\n".to_vec();
        let frame = take_frame(&mut buffer).expect("frame");
        assert_eq!(frame.data, "x");
    }

    #[test]
    fn no_frame_until_blank_line() {
        let mut buffer = b"data: partial".to_vec();
        assert!(take_frame(&mut buffer).is_none());
    }

    #[test]
    fn frames_split_sequentially() {
        let mut buffer = b"data: one\n\ndata: two\n\n".to_vec();
        let one = take_frame(&mut buffer).expect("one");
        let two = take_frame(&mut buffer).expect("two");
        assert_eq!(one.data, "one");
        assert_eq!(two.data, "two");
        assert!(buffer.is_empty());
    }
}
