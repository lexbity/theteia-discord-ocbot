//! The signal-cli JSON-RPC client and connection machine (SPEC-004 §5.9,
//! D-10).
//!
//! One connection at a time, owned honestly: newline-delimited JSON over a
//! `UnixStream`, requests correlated by a monotonic id (counter, not uuid —
//! one client, one socket, no collision surface), envelopes fanned to the
//! single subscription the machine holds. Any protocol error closes the whole
//! socket: stale-socket recovery without partial-state bugs (§5.9).
//!
//! The machine (`SignalMachine`) is the SPEC-003 supervisor's shape: connect →
//! subscribe → pump until closed → exponential backoff with ±20 % jitter →
//! connect … The backoff knobs come from `[events]` — they are
//! transport-generic and already validated in config.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Mutex, mpsc, oneshot, watch};

use opencode_v2_client::{DECODE_BURST_RECONNECT, DECODE_BURST_WINDOW_MS};

use crate::app::metrics::Metrics;
use ocbot_core::Surface;

use super::envelope::Envelope;

/// Why one RPC call failed.
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    /// The connection ended before (or while) the reply arrived.
    #[error("signal-cli connection closed")]
    Closed,
    /// The daemon answered with a JSON-RPC error object.
    #[error("signal-cli error {code}: {message}")]
    Wire {
        /// The daemon's error code (e.g. `-32601` method not found).
        code: i64,
        /// The daemon's message.
        message: String,
    },
    /// The reply line did not decode.
    #[error("decode failed: {0}")]
    Decode(String),
    /// A socket-level io failure.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// One in-flight request awaiting its reply line.
struct Pending {
    reply: oneshot::Sender<Result<serde_json::Value, RpcError>>,
}

/// Everything one connection shares between its reader task and callers.
struct Shared {
    /// The write half; `None` once the connection is closed.
    writer: Mutex<Option<OwnedWriteHalf>>,
    /// `id →` the awaiting caller.
    pending: Mutex<HashMap<u64, Pending>>,
    /// The id counter (starts at 1: signal-cli ids are 1-based).
    next_id: AtomicU64,
    /// The subscription's envelope sink, set by [`SignalRpc::subscribe`].
    envelope_tx: Mutex<Option<mpsc::Sender<Envelope>>>,
    /// The active subscription's request id (`0` = none).
    subscribe_id: AtomicU64,
    /// Undecodable lines observed on this connection (the drift rule).
    decode_failures: AtomicU64,
    /// `true` while the reader task lives.
    connected: watch::Sender<bool>,
    /// Held for the connection's whole life: `watch` only retains updates
    /// while at least one receiver exists, and [`SignalRpc::is_connected`]
    /// subscribes on demand — without a resident receiver, a `send(false)`
    /// between two queries would be dropped and the flag would read `true`
    /// forever after the socket died.
    connected_rx: watch::Receiver<bool>,
}

/// A cloneable handle to one live connection. Dropping every clone does not
/// close the socket — the reader task owns it until it ends; the machine's
/// [`SignalMachine::run_forever`] is what owns the lifecycle.
#[derive(Clone)]
pub struct SignalRpc {
    shared: Arc<Shared>,
}

impl SignalRpc {
    /// Connect to the daemon's unix socket and start the reader task.
    pub async fn connect(path: &Path) -> Result<Self, RpcError> {
        let stream = tokio::net::UnixStream::connect(path).await?;
        let (read_half, write_half) = stream.into_split();
        let (connected, connected_rx) = watch::channel(true);
        let shared = Arc::new(Shared {
            writer: Mutex::new(Some(write_half)),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            envelope_tx: Mutex::new(None),
            subscribe_id: AtomicU64::new(0),
            decode_failures: AtomicU64::new(0),
            connected,
            connected_rx,
        });
        tokio::spawn(reader_loop(read_half, shared.clone()));
        Ok(Self { shared })
    }

    /// The daemon's version string (`version` RPC).
    pub async fn version(&self) -> Result<String, RpcError> {
        let value = self.request("version", serde_json::json!([])).await?;
        value
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| RpcError::Decode(format!("version result is not a string: {value}")))
    }

    /// The registered accounts (`listAccounts` RPC).
    pub async fn list_accounts(&self) -> Result<Vec<String>, RpcError> {
        let value = self.request("listAccounts", serde_json::json!([])).await?;
        serde_json::from_value(value.clone())
            .map_err(|error| RpcError::Decode(format!("listAccounts result: {error}")))
    }

    /// Send a text message to one recipient; returns the daemon's timestamp
    /// for it (`send` RPC).
    pub async fn send(&self, recipient: &str, text: &str) -> Result<String, RpcError> {
        let params = serde_json::json!([{
            "recipient": recipient,
            "message": text,
        }]);
        let value = self.request("send", params).await?;
        value
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| RpcError::Decode(format!("send result is not a string: {value}")))
    }

    /// Send (or remove) a reaction on a target message (`sendReaction` RPC);
    /// returns the daemon's timestamp for it.
    pub async fn send_reaction(
        &self,
        recipient: &str,
        target_timestamp_ms: u64,
        emoji: &str,
        remove: bool,
    ) -> Result<String, RpcError> {
        let params = serde_json::json!([{
            "recipient": recipient,
            "targetTimestamp": target_timestamp_ms,
            "reaction": emoji,
            "remove": remove,
        }]);
        let value = self.request("sendReaction", params).await?;
        value.as_str().map(str::to_owned).ok_or_else(|| {
            RpcError::Decode(format!("sendReaction result is not a string: {value}"))
        })
    }

    /// Open the inbound subscription: every subsequent envelope line lands on
    /// the returned receiver until the connection ends. One subscription per
    /// connection (§5.9) — a second call replaces the first.
    pub async fn subscribe(&self) -> Result<mpsc::Receiver<Envelope>, RpcError> {
        let (tx, rx) = mpsc::channel(SUBSCRIPTION_CAPACITY);
        let id = self.next_id();
        {
            let mut slot = self.shared.envelope_tx.lock().await;
            *slot = Some(tx);
        }
        self.shared.subscribe_id.store(id, Ordering::Relaxed);
        // The ack (`result: null`) completes the pending entry; envelope lines
        // carrying this id then route to the subscription. A failed subscribe
        // (error reply or closed socket) must not leave the id active.
        let result = self
            .request_with_id(id, "subscribe", serde_json::json!([]))
            .await;
        if result.is_err() {
            self.shared.subscribe_id.store(0, Ordering::Relaxed);
            let mut slot = self.shared.envelope_tx.lock().await;
            *slot = None;
        }
        result.map(|_| rx)
    }

    /// Undecodable lines observed on this connection (the drift metric).
    pub fn decode_failures(&self) -> u64 {
        self.shared.decode_failures.load(Ordering::Relaxed)
    }

    /// Whether the reader task is still alive. The resident receiver is what
    /// makes this authoritative between calls.
    pub fn is_connected(&self) -> bool {
        *self.shared.connected_rx.borrow()
    }

    fn next_id(&self) -> u64 {
        self.shared.next_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Issue one request and await its correlated reply.
    async fn request(
        &self,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        let id = self.next_id();
        self.request_with_id(id, method, params).await
    }

    /// Issue one request under a pre-allocated id (the subscription path).
    async fn request_with_id(
        &self,
        id: u64,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, RpcError> {
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.shared.pending.lock().await;
            pending.insert(id, Pending { reply: tx });
        }
        let line = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Err(error) = self.write_line(&line.to_string()).await {
            self.drop_pending(id).await;
            return Err(error);
        }
        rx.await.unwrap_or(Err(RpcError::Closed))
    }

    async fn write_line(&self, line: &str) -> Result<(), RpcError> {
        let mut writer = self.shared.writer.lock().await;
        let Some(half) = writer.as_mut() else {
            return Err(RpcError::Closed);
        };
        half.write_all(line.as_bytes()).await?;
        half.write_all(b"\n").await?;
        Ok(())
    }

    async fn drop_pending(&self, id: u64) {
        self.shared.pending.lock().await.remove(&id);
    }
}

/// The subscription's envelope buffer (matches the frame-channel idiom).
const SUBSCRIPTION_CAPACITY: usize = 256;

/// The reader task: route reply lines by id, forward subscription lines as
/// envelopes, count decode failures, and end the connection honestly on EOF,
/// io error, or a decode-failure burst (the §5.9 drift rule).
async fn reader_loop(read_half: OwnedReadHalf, shared: Arc<Shared>) {
    let mut lines = BufReader::new(read_half).lines();
    let mut burst: u64 = 0;
    let mut window_start = tokio::time::Instant::now();
    loop {
        let Ok(Some(line)) = lines.next_line().await else {
            break;
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            if decode_burst_exceeded(&shared, &mut burst, &mut window_start) {
                break;
            }
            continue;
        };
        if route_line(&shared, &value).await.is_err()
            && decode_burst_exceeded(&shared, &mut burst, &mut window_start)
        {
            break;
        }
    }
    teardown(&shared).await;
}

/// Route one decoded line: a correlated reply, an envelope, or drift.
async fn route_line(shared: &Shared, value: &serde_json::Value) -> Result<(), ()> {
    let id = value.get("id").and_then(serde_json::Value::as_u64);
    if let Some(id) = id {
        let pending = {
            let mut pending = shared.pending.lock().await;
            pending.remove(&id)
        };
        if let Some(pending) = pending {
            let result = match value.get("error") {
                Some(error) => Err(RpcError::Wire {
                    code: error
                        .get("code")
                        .and_then(serde_json::Value::as_i64)
                        .unwrap_or(0),
                    message: error
                        .get("message")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("unknown error")
                        .to_owned(),
                }),
                None => match value.get("result") {
                    Some(result) => Ok(result.clone()),
                    None => Err(RpcError::Decode(format!(
                        "reply has neither result nor error: {value}"
                    ))),
                },
            };
            let _ = pending.reply.send(result);
            return Ok(());
        }
        if id == shared.subscribe_id.load(Ordering::Relaxed) {
            return forward_envelope(shared, value).await;
        }
        // A reply nobody asked for (a request aborted by a reconnect): drift.
        return Err(());
    }
    // No id: not a correlated reply. In the pinned contract that is drift.
    Err(())
}

/// Decode and forward one subscription line; a decode failure is drift.
async fn forward_envelope(shared: &Shared, value: &serde_json::Value) -> Result<(), ()> {
    let result = value.get("result").ok_or(())?;
    let envelope = Envelope::decode(result).map_err(|_| ())?;
    let tx = {
        let slot = shared.envelope_tx.lock().await;
        slot.clone()
    };
    match tx {
        // A dropped receiver means the consumer is gone; the socket closes
        // wholesale anyway when the machine notices (`None` on next recv).
        Some(tx) => {
            let _ = tx.try_send(envelope);
            Ok(())
        }
        None => Err(()),
    }
}

/// The §5.9 drift rule: count the failure, and end the connection past
/// [`DECODE_BURST_RECONNECT`] within [`DECODE_BURST_WINDOW_MS`].
fn decode_burst_exceeded(
    shared: &Shared,
    burst: &mut u64,
    window_start: &mut tokio::time::Instant,
) -> bool {
    shared.decode_failures.fetch_add(1, Ordering::Relaxed);
    let now = tokio::time::Instant::now();
    if now.duration_since(*window_start) >= Duration::from_millis(DECODE_BURST_WINDOW_MS) {
        *window_start = now;
        *burst = 1;
        return false;
    }
    *burst += 1;
    *burst > u64::from(DECODE_BURST_RECONNECT)
}

/// End the connection: fail every in-flight request, close the writer, flip
/// the connected flag (the `Closed` semantics of context.rs's `StreamHandle`).
async fn teardown(shared: &Shared) {
    let pending: Vec<Pending> = {
        let mut pending = shared.pending.lock().await;
        pending.drain().map(|(_, pending)| pending).collect()
    };
    for pending in pending {
        let _ = pending.reply.send(Err(RpcError::Closed));
    }
    shared.subscribe_id.store(0, Ordering::Relaxed);
    {
        let mut slot = shared.envelope_tx.lock().await;
        *slot = None;
    }
    {
        let mut writer = shared.writer.lock().await;
        if let Some(half) = writer.as_mut() {
            let _ = half.shutdown().await;
        }
        *writer = None;
    }
    let _ = shared.connected.send(false);
}

/// The reconnect machine (§5.9): the socket is owned by this loop alone.
pub struct SignalMachine {
    path: PathBuf,
    backoff_min: Duration,
    backoff_max: Duration,
    metrics: Arc<Metrics>,
    /// Jitter source for the backoff, `[0, 1)`; injected so tests are
    /// deterministic (the supervisor's idiom).
    jitter: Box<dyn Fn() -> f64 + Send + Sync>,
}

impl SignalMachine {
    /// Build the machine over one daemon socket with SPEC-003-style backoff
    /// bounds (min 1 s / max 30 s in production, from `[events]`).
    pub fn new(
        path: PathBuf,
        backoff_min: Duration,
        backoff_max: Duration,
        metrics: Arc<Metrics>,
    ) -> Self {
        Self {
            path,
            backoff_min,
            backoff_max,
            metrics,
            jitter: Box::new(nanos_jitter),
        }
    }

    /// Replace the jitter source (tests inject deterministic values).
    #[must_use]
    pub fn with_jitter(mut self, jitter: Box<dyn Fn() -> f64 + Send + Sync>) -> Self {
        self.jitter = jitter;
        self
    }

    /// Run until the process drains: connect → subscribe → pump envelopes into
    /// `sinks` → on closure, back off and reconnect. The
    /// `ocbot_surface_up{surface="signal"}` gauge tracks subscribe success
    /// (§5.10: 1 = last subscribe ok).
    pub async fn run_forever(self, sinks: mpsc::Sender<Envelope>) {
        let mut attempt: u32 = 0;
        loop {
            // A subscription that ran and ended is a success worth resetting
            // the ladder for (the supervisor's rule); only failures grow it.
            match self.connect_and_subscribe(&sinks).await {
                Ok(()) => attempt = 0,
                Err(error) => {
                    tracing::warn!(error = %error, "signal surface connection ended");
                }
            }
            let delay =
                signal_backoff(attempt, self.backoff_min, self.backoff_max, (self.jitter)());
            tokio::time::sleep(delay).await;
            attempt = attempt.saturating_add(1);
        }
    }

    /// One connection's full life: connect, subscribe, pump until the
    /// subscription closes. `Ok` means a subscription ran and ended cleanly;
    /// `Err` the reason it did not (or ended abruptly).
    async fn connect_and_subscribe(&self, sinks: &mpsc::Sender<Envelope>) -> Result<(), RpcError> {
        let rpc = match SignalRpc::connect(&self.path).await {
            Ok(rpc) => rpc,
            Err(error) => {
                self.metrics.set_surface_up(Surface::Signal, false);
                return Err(error);
            }
        };
        let mut envelopes = match rpc.subscribe().await {
            Ok(rx) => rx,
            Err(error) => {
                self.metrics.set_surface_up(Surface::Signal, false);
                return Err(error);
            }
        };
        self.metrics.set_surface_up(Surface::Signal, true);
        tracing::info!(socket = %self.path.display(), "signal surface subscribed");
        while let Some(envelope) = envelopes.recv().await {
            if sinks.send(envelope).await.is_err() {
                // The consumer is gone; the machine's owner is draining.
                break;
            }
        }
        self.metrics.set_surface_up(Surface::Signal, false);
        // The subscription ran and ended — a clean session (the ladder
        // resets); the next iteration reconnects from the floor.
        Ok(())
    }
}

/// Exponential backoff `min * 2^attempt` capped at `max`, scaled by the
/// jitter factor (±20 %: the injected source yields `[0, 1)` mapped to
/// `[0.8, 1.2)`). The SPEC-003 supervisor's formula, transport-generic.
#[must_use]
pub fn signal_backoff(attempt: u32, min: Duration, max: Duration, jitter: f64) -> Duration {
    let growth = min.saturating_mul(1_u32 << attempt.min(20));
    let capped = growth.min(max);
    let factor = 0.8 + 0.4 * jitter.clamp(0.0, 1.0);
    Duration::from_secs_f64(capped.as_secs_f64() * factor)
}

/// Production jitter: the wall clock's sub-second nanos as `[0, 1)`.
fn nanos_jitter() -> f64 {
    f64::from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos()),
    ) / 1_000_000_000.0
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{nanos_jitter, signal_backoff};

    #[test]
    fn the_production_jitter_source_stays_in_its_band() {
        // `nanos_jitter` is the live source: it must be [0, 1) or the ±20 %
        // scaling would invert.
        for _ in 0..1_000 {
            let value = nanos_jitter();
            assert!((0.0..1.0).contains(&value), "{value} is out of band");
        }
    }

    #[test]
    fn backoff_grows_exponentially_with_jitter() {
        let min = Duration::from_secs(1);
        let max = Duration::from_secs(30);
        // Jitter 0.5 is the factor midpoint: 0.8 + 0.4 * 0.5 = 1.0.
        assert_eq!(signal_backoff(0, min, max, 0.5), Duration::from_secs(1));
        assert_eq!(signal_backoff(1, min, max, 0.5), Duration::from_secs(2));
        assert_eq!(signal_backoff(2, min, max, 0.5), Duration::from_secs(4));
        assert_eq!(signal_backoff(3, min, max, 0.5), Duration::from_secs(8));
        assert_eq!(signal_backoff(4, min, max, 0.5), Duration::from_secs(16));
        assert_eq!(
            signal_backoff(5, min, max, 0.5),
            max,
            "1s * 2^5 = 32s, capped at 30s"
        );
        assert_eq!(signal_backoff(10, min, max, 0.5), max, "capped");
    }

    #[test]
    fn jitter_scales_the_delay_by_up_to_twenty_percent() {
        let min = Duration::from_secs(1);
        let max = Duration::from_secs(30);
        let low = signal_backoff(0, min, max, 0.0);
        let high = signal_backoff(0, min, max, 1.0);
        assert!(low < high);
        assert!(low >= Duration::from_millis(799), "{low:?}");
        assert!(high <= Duration::from_millis(1_201), "{high:?}");
        // Out-of-range jitter is clamped, never amplified.
        assert_eq!(signal_backoff(0, min, max, -1.0), low);
        assert_eq!(signal_backoff(0, min, max, 2.0), high);
    }

    #[test]
    fn huge_attempt_numbers_do_not_shift_into_the_void() {
        let min = Duration::from_secs(1);
        let max = Duration::from_secs(30);
        assert_eq!(signal_backoff(u32::MAX, min, max, 0.5), max);
    }
}
