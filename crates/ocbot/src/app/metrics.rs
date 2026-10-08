//! Prometheus text exposition on the loopback metrics port (spec NFR-9 /
//! §6.8), hand-rolled so the crate carries no extra HTTP machinery: a tiny
//! tokio TCP server answering `GET /` with the text format.
//!
//! The whole §6.8 family is exposed here: `ocbot_actions_total{state}`,
//! `ocbot_transition_cas_conflicts_total`, `ocbot_gate_requests_total{kind,tool}`,
//! `ocbot_gate_pending`, `ocbot_idle_to_report_seconds` (a proper histogram),
//! `ocbot_opencode_up`, `ocbot_reconcile_repairs_total{kind}`,
//! `ocbot_locks_held`, `ocbot_lock_age_seconds`, `ocbot_log_lag_seconds`, and
//! `ocbot_stream_reconnects_total`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use ocbot_core::State;

/// Shared metric registry. `Arc<Metrics>` is handed to every component.
#[derive(Clone)]
pub struct Metrics {
    inner: Arc<Inner>,
}

struct Inner {
    cas_conflicts: AtomicU64,
    denied: AtomicU64,
    reports_delivered: AtomicU64,
    opencode_up: AtomicU64,
    gate_pending: AtomicU64,
    locks_held: AtomicU64,
    lock_age_seconds: AtomicU64,
    log_lag_seconds: AtomicU64,
    stream_reconnects: AtomicU64,
    repairs_accept: AtomicU64,
    repairs_park: AtomicU64,
    repairs_reset: AtomicU64,
    actions_by_state: [AtomicU64; 10],
    idle_buckets: [AtomicU64; 7],
    idle_count: AtomicU64,
    /// Sum of observed seconds, stored as the bits of an `f64` (updated with a
    /// CAS loop so no lossy float→int cast is needed).
    idle_sum_bits: AtomicU64,
    /// `(kind, tool) -> count` for discovered gate requests. A graph of
    /// dynamic label pairs, so a map (rendered in sorted order).
    gate_requests: Mutex<BTreeMap<(String, String), u64>>,
}

/// Histogram bucket upper bounds (seconds) for idle→report latency (NFR-2).
pub const IDLE_BUCKETS_SECS: [f64; 7] = [1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 600.0];

impl Metrics {
    /// A fresh registry.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                cas_conflicts: AtomicU64::new(0),
                denied: AtomicU64::new(0),
                reports_delivered: AtomicU64::new(0),
                opencode_up: AtomicU64::new(0),
                gate_pending: AtomicU64::new(0),
                locks_held: AtomicU64::new(0),
                lock_age_seconds: AtomicU64::new(0),
                log_lag_seconds: AtomicU64::new(0),
                stream_reconnects: AtomicU64::new(0),
                repairs_accept: AtomicU64::new(0),
                repairs_park: AtomicU64::new(0),
                repairs_reset: AtomicU64::new(0),
                actions_by_state: [const { AtomicU64::new(0) }; 10],
                idle_buckets: [const { AtomicU64::new(0) }; 7],
                idle_count: AtomicU64::new(0),
                idle_sum_bits: AtomicU64::new(0),
                gate_requests: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    /// A CAS lost the race.
    pub fn inc_cas_conflict(&self) {
        self.inner.cas_conflicts.fetch_add(1, Ordering::Relaxed);
    }

    /// A non-allowlisted user was rejected.
    pub fn inc_denied(&self) {
        self.inner.denied.fetch_add(1, Ordering::Relaxed);
    }

    /// A report was delivered (exactly once by the dedupe CAS).
    pub fn inc_reports_delivered(&self) {
        self.inner.reports_delivered.fetch_add(1, Ordering::Relaxed);
    }

    /// opencode reachability gauge (FR-38).
    pub fn set_opencode_up(&self, up: bool) {
        self.inner
            .opencode_up
            .store(u64::from(up), Ordering::Relaxed);
    }

    /// One gate request *discovered* (permission or form; counted once per
    /// unique `ext_id`, with the permission's tool as the second label).
    pub fn inc_gate_request(&self, kind: &str, tool: &str) {
        let mut map = self
            .inner
            .gate_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *map.entry((kind.to_owned(), tool.to_owned())).or_insert(0) += 1;
    }

    /// One interrupted git sequence rolled forward at boot/reconnect.
    pub fn inc_reconcile_repair(&self, kind: &str) {
        let counter = match kind {
            "accept" => &self.inner.repairs_accept,
            "park" => &self.inner.repairs_park,
            "reset" => &self.inner.repairs_reset,
            _ => return,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// An action entered `state` (counter over observed transitions).
    pub fn action_entered(&self, state: State) {
        self.inner.actions_by_state[state_index(state)].fetch_add(1, Ordering::Relaxed);
    }

    /// Absolute action count per state (boot seed).
    pub fn seed_actions(&self, counts: &[(String, i64)]) {
        for (name, count) in counts {
            let Some(state) = parse_state(name) else {
                continue;
            };
            self.inner.actions_by_state[state_index(state)]
                .store(u64::try_from(*count).unwrap_or(0), Ordering::Relaxed);
        }
    }

    /// Observe an idle→report duration (seconds) into the histogram.
    pub fn observe_idle_to_report(&self, seconds: f64) {
        for (index, upper) in IDLE_BUCKETS_SECS.iter().enumerate() {
            if seconds <= *upper {
                self.inner.idle_buckets[index].fetch_add(1, Ordering::Relaxed);
            }
        }
        self.inner.idle_count.fetch_add(1, Ordering::Relaxed);
        if seconds.is_finite() && seconds > 0.0 {
            add_f64(&self.inner.idle_sum_bits, seconds);
        }
    }

    /// Locks-held gauge (driven by the ops tick).
    pub fn set_locks_held(&self, count: u64) {
        self.inner.locks_held.store(count, Ordering::Relaxed);
    }

    /// Age of the oldest held lock (seconds), driven by the ops tick.
    pub fn set_lock_age_seconds(&self, seconds: u64) {
        self.inner
            .lock_age_seconds
            .store(seconds, Ordering::Relaxed);
    }

    /// Observation lag of the newest tracked event (seconds). In the
    /// polling model (S-2) this is `now - last_event_at` for the most recently
    /// active running action.
    pub fn set_log_lag_seconds(&self, seconds: u64) {
        self.inner.log_lag_seconds.store(seconds, Ordering::Relaxed);
    }

    /// One transport failure observed by the watcher (the polling model's
    /// reconnect).
    pub fn inc_stream_reconnect(&self) {
        self.inner.stream_reconnects.fetch_add(1, Ordering::Relaxed);
    }

    /// Pending-gate gauge (driven by the gate loop, P4).
    pub fn set_gate_pending(&self, count: usize) {
        self.inner
            .gate_pending
            .store(u64::try_from(count).unwrap_or(0), Ordering::Relaxed);
    }

    /// Render the Prometheus text format (modelines + family blocks).
    pub fn render_text(&self) -> String {
        let inner = &self.inner;
        let mut out = String::new();
        render_actions(inner, &mut out);
        counter(
            &mut out,
            "ocbot_transition_cas_conflicts_total",
            inner.cas_conflicts.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "ocbot_reports_delivered_total",
            inner.reports_delivered.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "ocbot_denied_interactions_total",
            inner.denied.load(Ordering::Relaxed),
        );
        counter(
            &mut out,
            "ocbot_stream_reconnects_total",
            inner.stream_reconnects.load(Ordering::Relaxed),
        );
        render_gate_requests(inner, &mut out);
        render_repairs(inner, &mut out);
        gauge(
            &mut out,
            "ocbot_opencode_up",
            inner.opencode_up.load(Ordering::Relaxed),
        );
        gauge(
            &mut out,
            "ocbot_gate_pending",
            inner.gate_pending.load(Ordering::Relaxed),
        );
        gauge(
            &mut out,
            "ocbot_locks_held",
            inner.locks_held.load(Ordering::Relaxed),
        );
        gauge(
            &mut out,
            "ocbot_lock_age_seconds",
            inner.lock_age_seconds.load(Ordering::Relaxed),
        );
        gauge(
            &mut out,
            "ocbot_log_lag_seconds",
            inner.log_lag_seconds.load(Ordering::Relaxed),
        );
        render_idle_histogram(inner, &mut out);
        out
    }
}

/// The `ocbot_actions_total{state}` family.
fn render_actions(inner: &Inner, out: &mut String) {
    out.push_str("# HELP ocbot_actions_total Actions observed entering each state.\n");
    out.push_str("# TYPE ocbot_actions_total counter\n");
    for (index, counter) in inner.actions_by_state.iter().enumerate() {
        let value = counter.load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "ocbot_actions_total{{state=\"{}\"}} {value}",
            STATE_NAMES[index]
        );
    }
}

/// The `ocbot_gate_requests_total{kind,tool}` family.
fn render_gate_requests(inner: &Inner, out: &mut String) {
    out.push_str("# HELP ocbot_gate_requests_total Gate requests discovered, by kind and tool.\n");
    out.push_str("# TYPE ocbot_gate_requests_total counter\n");
    let map = inner
        .gate_requests
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for ((kind, tool), count) in map.iter() {
        let _ = writeln!(
            out,
            "ocbot_gate_requests_total{{kind=\"{}\",tool=\"{}\"}} {count}",
            escape_label(kind),
            escape_label(tool)
        );
    }
}

/// The `ocbot_reconcile_repairs_total{kind}` family.
fn render_repairs(inner: &Inner, out: &mut String) {
    out.push_str(
        "# HELP ocbot_reconcile_repairs_total Interrupted git sequences rolled forward.\n",
    );
    out.push_str("# TYPE ocbot_reconcile_repairs_total counter\n");
    for (kind, counter) in [
        ("accept", &inner.repairs_accept),
        ("park", &inner.repairs_park),
        ("reset", &inner.repairs_reset),
    ] {
        let _ = writeln!(
            out,
            "ocbot_reconcile_repairs_total{{kind=\"{kind}\"}} {}",
            counter.load(Ordering::Relaxed)
        );
    }
}

/// The `ocbot_idle_to_report_seconds` histogram family.
fn render_idle_histogram(inner: &Inner, out: &mut String) {
    out.push_str("# HELP ocbot_idle_to_report_seconds Idle to report latency.\n");
    out.push_str("# TYPE ocbot_idle_to_report_seconds histogram\n");
    for (index, upper) in IDLE_BUCKETS_SECS.iter().enumerate() {
        let count = inner.idle_buckets[index].load(Ordering::Relaxed);
        let _ = writeln!(
            out,
            "ocbot_idle_to_report_seconds_bucket{{le=\"{upper}\"}} {count}"
        );
    }
    let count = inner.idle_count.load(Ordering::Relaxed);
    let _ = writeln!(
        out,
        "ocbot_idle_to_report_seconds_bucket{{le=\"+Inf\"}} {count}"
    );
    let _ = writeln!(out, "ocbot_idle_to_report_seconds_count {count}");
    let sum_seconds = f64::from_bits(inner.idle_sum_bits.load(Ordering::Relaxed));
    let _ = writeln!(out, "ocbot_idle_to_report_seconds_sum {sum_seconds}");
}

/// Add `value` to an `f64` stored in an atomic via a CAS loop (no lossy cast).
fn add_f64(cell: &AtomicU64, value: f64) {
    let mut current = cell.load(Ordering::Relaxed);
    loop {
        let updated = f64::from_bits(current) + value;
        match cell.compare_exchange_weak(
            current,
            updated.to_bits(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(observed) => current = observed,
        }
    }
}

/// Append a `# TYPE ... counter` family with a single unlabeled line.
fn counter(out: &mut String, name: &str, value: u64) {
    let _ = writeln!(out, "# TYPE {name} counter");
    let _ = writeln!(out, "{name} {value}");
}

/// Append a `# TYPE ... gauge` family with a single unlabeled line.
fn gauge(out: &mut String, name: &str, value: u64) {
    let _ = writeln!(out, "# TYPE {name} gauge");
    let _ = writeln!(out, "{name} {value}");
}

/// Escape a Prometheus label value (`\`, `"`, newline).
fn escape_label(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

fn state_index(state: State) -> usize {
    match state {
        State::Staged => 0,
        State::Running => 1,
        State::Reported => 2,
        State::Accepting => 3,
        State::Accepted => 4,
        State::Parking => 5,
        State::Parked => 6,
        State::Errored => 7,
        State::Aborted => 8,
        State::Discarded => 9,
    }
}

fn parse_state(name: &str) -> Option<State> {
    ocbot_core::ALL_STATES
        .iter()
        .copied()
        .find(|state| state.as_str() == name)
}

const STATE_NAMES: [&str; 10] = [
    "staged",
    "running",
    "reported",
    "accepting",
    "accepted",
    "parking",
    "parked",
    "errored",
    "aborted",
    "discarded",
];

/// Serve the metrics endpoint on `127.0.0.1:<port>`.
pub async fn serve(bind: &str, metrics: Arc<Metrics>) -> Result<(), std::io::Error> {
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(address = bind, "metrics server listening");
    serve_listener(listener, metrics).await
}

/// Serve an already-bound listener (lets tests bind an ephemeral port and read
/// it back before serving).
pub async fn serve_listener(
    listener: tokio::net::TcpListener,
    metrics: Arc<Metrics>,
) -> Result<(), std::io::Error> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let metrics = metrics.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buffer = [0u8; 2048];
            let read = stream.read(&mut buffer).await.unwrap_or_default();
            let request = String::from_utf8_lossy(&buffer[..read]);
            let (status, body) = if request.starts_with("GET ") {
                ("200 OK", metrics.render_text())
            } else {
                ("404 Not Found", String::new())
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
        });
    }
}
