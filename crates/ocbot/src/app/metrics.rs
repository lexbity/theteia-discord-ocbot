//! Prometheus text exposition on the loopback metrics port (spec NFR-9 /
//! §6.8), hand-rolled so the crate carries no extra HTTP machinery: a tiny
//! tokio TCP server answering `GET /` with the text format.

use std::fmt::Write as _;
use std::sync::Arc;
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
    locks_held: AtomicU64,
    gate_pending: AtomicU64,
    actions_by_state: [AtomicU64; 10],
    idle_buckets: [AtomicU64; 7],
}

/// Histogram bucket upper bounds (seconds) for idle→report latency (NFR-2).
const IDLE_BUCKETS_SECS: [f64; 7] = [1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 600.0];

impl Metrics {
    /// A fresh registry.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                cas_conflicts: AtomicU64::new(0),
                denied: AtomicU64::new(0),
                reports_delivered: AtomicU64::new(0),
                opencode_up: AtomicU64::new(0),
                locks_held: AtomicU64::new(0),
                gate_pending: AtomicU64::new(0),
                actions_by_state: [const { AtomicU64::new(0) }; 10],
                idle_buckets: [const { AtomicU64::new(0) }; 7],
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

    /// opencode reachability gauge.
    pub fn set_opencode_up(&self, up: bool) {
        self.inner
            .opencode_up
            .store(u64::from(up), Ordering::Relaxed);
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
    }

    /// Locks-held gauge (driven by the status tick).
    pub fn set_locks_held(&self, count: u64) {
        self.inner.locks_held.store(count, Ordering::Relaxed);
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
        let mut counters = Vec::new();
        for (index, counter) in inner.actions_by_state.iter().enumerate() {
            let value = counter.load(Ordering::Relaxed);
            counters.push(format!(
                "ocbot_actions_total{{state=\"{}\"}} {value}",
                STATE_NAMES[index]
            ));
        }
        counters.push(format!(
            "ocbot_transition_cas_conflicts_total {}",
            inner.cas_conflicts.load(Ordering::Relaxed)
        ));
        counters.push(format!(
            "ocbot_reports_delivered_total {}",
            inner.reports_delivered.load(Ordering::Relaxed)
        ));
        counters.push(format!(
            "ocbot_denied_interactions_total {}",
            inner.denied.load(Ordering::Relaxed)
        ));
        counters.push(format!(
            "ocbot_opencode_up {}",
            inner.opencode_up.load(Ordering::Relaxed)
        ));
        counters.push(format!(
            "ocbot_lock_age_seconds {}",
            inner.locks_held.load(Ordering::Relaxed)
        ));
        counters.push(format!(
            "ocbot_gate_pending {}",
            inner.gate_pending.load(Ordering::Relaxed)
        ));
        let mut buckets = String::new();
        for (index, upper) in IDLE_BUCKETS_SECS.iter().enumerate() {
            let count = inner.idle_buckets[index].load(Ordering::Relaxed);
            let _ = writeln!(
                buckets,
                "ocbot_idle_to_report_seconds_bucket{{le=\"{upper}\"}} {count}"
            );
        }
        buckets.push_str("ocbot_idle_to_report_seconds_bucket{le=\"+Inf\"} 0\n");

        format!(
            "# HELP ocbot_actions_total Actions currently in each state.\n\
             # TYPE ocbot_actions_total counter\n{counters}\n\
             # TYPE ocbot_transition_cas_conflicts_total counter\n\
             # TYPE ocbot_reports_delivered_total counter\n\
             # TYPE ocbot_denied_interactions_total counter\n\
             # TYPE ocbot_opencode_up gauge\n\
             # TYPE ocbot_lock_age_seconds gauge\n\
             # TYPE ocbot_gate_pending gauge\n\
             # HELP ocbot_idle_to_report_seconds Idle to report latency.\n\
             # TYPE ocbot_idle_to_report_seconds histogram\n{buckets}",
            counters = counters.join("\n"),
        )
    }
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
