//! The global event stream (`/api/event`), typed as tolerant frame headers
//! (SPEC-003 W2).
//!
//! Frames are **untrusted input** (P-32: bodies may carry agent text). Each
//! SSE frame is reduced to an [`EventFrame`] — the event kind and the session
//! key, nothing else; the body is parsed once into a two-field header struct
//! and dropped, never retained. A frame that fails the header parse increments
//! the stream's decode-failure counter and is skipped; the connection survives
//! (P-24). The liveness deadline and the decode-burst reconnect policy are the
//! *consumer's* (P-22, [`DECODE_BURST_RECONNECT`]).
//!
//! The SSE parser here is shared with the durable session log
//! ([`crate::log`]); both streams are volatile-by-contract surfaces where
//! callers reconcile from status facts (spec P9).

use bytes::Bytes;
use futures_util::{StreamExt, stream::BoxStream};
use serde::Deserialize;

use crate::Client;
use crate::error::OcError;
use crate::paths;

/// Decode failures within a 10 s window at which the *consumer* should treat
/// the connection as drifted and reconnect (SPEC-003 P-24; the counter lives
/// on [`EventStream::decode_failures`], the policy lives in the shell).
pub const DECODE_BURST_RECONNECT: u32 = 25;

/// The window, in milliseconds, over which [`DECODE_BURST_RECONNECT`] decode
/// failures mark the connection as drifted.
pub const DECODE_BURST_WINDOW_MS: u64 = 10_000;

/// A tolerant header of one `/api/event` frame: the event kind and the
/// session key it is bound to, nothing else (P-32: the body is never
/// retained).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
pub struct EventFrame {
    /// The event kind (the payload's `type` discriminator, falling back to
    /// the SSE `event:` field).
    pub event: String,
    /// The session key: `data.sessionID`, falling back to the durable
    /// aggregate id. `None` for global (unkeyed) kinds.
    pub session_id: Option<String>,
}

/// Minimal, throwaway header view of a frame payload. Unknown fields are
/// ignored by serde and freed — strict reject-unknown-fields modes are
/// banned in this crate (gate G1).
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct EventHeader {
    #[serde(rename = "type")]
    kind: Option<String>,
    data: HeaderData,
    durable: HeaderDurable,
}

/// The `data` object's session key, if the frame carries one.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct HeaderData {
    #[serde(rename = "sessionID")]
    session_id: Option<String>,
}

/// The durable position's aggregate id (the session id on aggregate events).
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct HeaderDurable {
    #[serde(rename = "aggregateID")]
    aggregate_id: Option<String>,
}

/// A single parsed server-sent event frame (raw fields, before the typed
/// header reduction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseFrame {
    /// The optional SSE `id:` field.
    pub id: Option<String>,
    /// The optional SSE `event:` field.
    pub event: Option<String>,
    /// The joined `data:` lines.
    pub data: String,
}

/// Incremental SSE parser: buffers raw bytes until the next complete frame and
/// decodes it. Tolerates `\n\n` and `\r\n\r\n` separators. Comment/heartbeat
/// frames surface as frames with empty data; consumers skip them.
pub(crate) struct SseReader {
    inner: BoxStream<'static, reqwest::Result<Bytes>>,
    buffer: Vec<u8>,
    closed: bool,
}

impl SseReader {
    pub(crate) fn new(inner: BoxStream<'static, reqwest::Result<Bytes>>) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
            closed: false,
        }
    }

    pub(crate) async fn next_frame(&mut self) -> Option<Result<SseFrame, OcError>> {
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
pub(crate) fn take_frame(buffer: &mut Vec<u8>) -> Option<SseFrame> {
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

/// Reduce one raw SSE frame to a typed header. `None` marks a decode failure:
/// non-JSON data, or no discriminator at all (neither the payload's `type`
/// nor the SSE `event:` field). The body is parsed once into the two-field
/// header and dropped.
pub(crate) fn parse_frame(frame: &SseFrame) -> Option<EventFrame> {
    let header = if frame.data.trim().is_empty() {
        // Comment frame or no body: still valid when the SSE `event:` field
        // carries the kind; otherwise it is a plain heartbeat.
        None
    } else {
        serde_json::from_str::<EventHeader>(&frame.data).ok()
    };
    let kind = match &header {
        Some(header) => header.kind.clone().or_else(|| frame.event.clone()),
        None => frame.event.clone(),
    }?;
    let session_id =
        header.and_then(|header| header.data.session_id.or(header.durable.aggregate_id));
    Some(EventFrame {
        event: kind,
        session_id,
    })
}

/// A subscription to the global (volatile) event stream, yielding typed
/// tolerant headers.
pub struct EventStream {
    reader: SseReader,
    decode_failures: u64,
}

impl EventStream {
    /// Create a stream from an already-constructed server response.
    #[must_use]
    pub fn new(response: reqwest::Response) -> Self {
        Self {
            reader: SseReader::new(response.bytes_stream().boxed()),
            decode_failures: 0,
        }
    }

    /// Decode failures observed so far on this stream (skipped frames, P-24).
    /// The consumer reconnects with `reason=drift` when more than
    /// [`DECODE_BURST_RECONNECT`] accumulate within [`DECODE_BURST_WINDOW_MS`].
    #[must_use]
    pub fn decode_failures(&self) -> u64 {
        self.decode_failures
    }

    /// Await the next event frame. `Ok(None)` means the server closed the
    /// stream. Undecodable frames are counted (see [`Self::decode_failures`])
    /// and skipped; the connection survives (P-24).
    pub async fn next_frame(&mut self) -> Result<Option<EventFrame>, OcError> {
        loop {
            let Some(frame) = self.reader.next_frame().await.transpose()? else {
                return Ok(None);
            };
            match parse_frame(&frame) {
                Some(event) => return Ok(Some(event)),
                None => {
                    if !frame.data.trim().is_empty() {
                        self.decode_failures += 1;
                    }
                }
            }
        }
    }
}

impl Client {
    /// Subscribe to the global event stream (`/api/event`). Volatile by
    /// contract: disconnect means missed events; callers reconcile from
    /// status facts afterwards (spec P9). Frames are typed tolerant headers
    /// ([`EventFrame`]); undecodable frames are counted and skipped (P-24).
    pub async fn subscribe_events(&self) -> Result<EventStream, OcError> {
        let response = self.get_stream(paths::EVENT, &[]).await?;
        Ok(EventStream::new(response))
    }
}

#[cfg(test)]
mod tests {
    use super::{EventFrame, parse_frame, take_frame};

    fn frame(raw: &str) -> super::SseFrame {
        let mut buffer = raw.as_bytes().to_vec();
        take_frame(&mut buffer).expect("one frame")
    }

    #[test]
    fn parses_type_discriminator_and_session_id() {
        let parsed = parse_frame(&frame(
            "data: {\"id\":\"evt_1\",\"type\":\"session.execution.started\",\"data\":{\"sessionID\":\"ses_a\"}}\n\n",
        ))
        .expect("decodes");
        assert_eq!(
            parsed,
            EventFrame {
                event: "session.execution.started".to_owned(),
                session_id: Some("ses_a".to_owned()),
            }
        );
    }

    #[test]
    fn falls_back_to_the_durable_aggregate_id() {
        let parsed = parse_frame(&frame(
            "data: {\"type\":\"session.created\",\"data\":{},\"durable\":{\"aggregateID\":\"ses_b\",\"seq\":1}}\n\n",
        ))
        .expect("decodes");
        assert_eq!(parsed.session_id, Some("ses_b".to_owned()));
    }

    #[test]
    fn global_kinds_are_unkeyed_and_unknown_fields_are_dropped() {
        let parsed = parse_frame(&frame(
            "data: {\"type\":\"server.connected\",\"created\":1,\"data\":{\"anything\":\"dropped\"},\"location\":{\"directory\":\"/x\"}}\n\n",
        ))
        .expect("decodes");
        assert_eq!(parsed.event, "server.connected");
        assert_eq!(parsed.session_id, None);
    }

    #[test]
    fn an_sse_event_field_substitutes_for_a_missing_type() {
        let parsed = parse_frame(&frame("event: custom.kind\ndata: {}\n\n")).expect("decodes");
        assert_eq!(parsed.event, "custom.kind");
        assert_eq!(parsed.session_id, None);
    }

    #[test]
    fn non_json_data_is_a_decode_failure() {
        assert!(parse_frame(&frame("data: this is not json\n\n")).is_none());
    }

    #[test]
    fn data_without_a_discriminator_is_a_decode_failure() {
        assert!(parse_frame(&frame("data: {\"created\":1}\n\n")).is_none());
    }

    #[test]
    fn truncated_json_is_a_decode_failure_never_a_panic() {
        assert!(parse_frame(&frame("data: {\"type\":\"session.\n\n")).is_none());
    }

    #[test]
    fn comment_frames_are_neither_events_nor_failures() {
        assert!(parse_frame(&frame(": heartbeat\n\n")).is_none());
    }

    #[test]
    fn non_utf8_bytes_decode_lossily_and_fail_cleanly() {
        let mut buffer: Vec<u8> = b"data: ".to_vec();
        buffer.extend_from_slice(&[0xFF, 0xFE, 0x00, 0x81]);
        buffer.extend_from_slice(b"\n\n");
        let raw = take_frame(&mut buffer).expect("one frame");
        assert!(parse_frame(&raw).is_none(), "garbage bytes never panic");
    }
}
