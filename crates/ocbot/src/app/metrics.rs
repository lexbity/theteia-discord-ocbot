//! Prometheus text exposition on the loopback metrics port (spec NFR-9 /
//! §6.8), hand-rolled so the crate carries no extra HTTP machinery: a tiny
//! tokio TCP server answering `GET /` with the text format.
//!
//! The whole §6.8 family is exposed here, plus the SPEC-003 §4.9 stream
//! family: `ocbot_stream_state`, `ocbot_stream_reconnects_total{reason}`,
//! `ocbot_stream_live_seconds_total`, `ocbot_frames_total{kind,mapped}`,
//! `ocbot_frames_decode_failures_total`, `ocbot_frames_dropped_total`, and the
//! honest `ocbot_probe_failures_total{loop}` (the watcher's probe failures are
//! no longer mislabeled as stream reconnects — D-11).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use ocbot_core::State;
use ocbot_core::Surface;

/// Shared metric registry. `Arc<Metrics>` is handed to every component.
#[derive(Clone)]
pub struct Metrics {
    inner: Arc<Inner>,
}

struct Inner {
    cas_conflicts: AtomicU64,
    denied: AtomicU64,
    /// `surface -> count` of delivered reports (§5.10: labeled by surface).
    reports_delivered: Mutex<BTreeMap<String, u64>>,
    opencode_up: AtomicU64,
    gate_pending: AtomicU64,
    locks_held: AtomicU64,
    lock_age_seconds: AtomicU64,
    log_lag_seconds: AtomicU64,
    stream_state: AtomicU64,
    stream_live_ms: AtomicU64,
    frames_decode_failures: AtomicU64,
    frames_dropped: AtomicU64,
    /// `reason -> count` for stream reconnects (dynamic labels, sorted render).
    stream_reconnects: Mutex<BTreeMap<String, u64>>,
    /// `(kind, mapped) -> count` of observed frames.
    frames_total: Mutex<BTreeMap<(String, bool), u64>>,
    /// `loop -> count` of failed probes per polling loop (D-11).
    probe_failures: Mutex<BTreeMap<String, u64>>,
    /// `source -> count` of detections (event | fallback; P-29, the W5 gate).
    detections: Mutex<BTreeMap<String, u64>>,
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
    /// Per-surface up gauge (1/0).
    surface_up: Mutex<BTreeMap<String, u64>>,
    /// Per-surface, per-op send attempt counters.
    surface_send_total: Mutex<BTreeMap<(String, String), u64>>,
    /// Per-surface, per-op send failure counters.
    surface_send_failures: Mutex<BTreeMap<(String, String), u64>>,
    /// Per-surface deny counters.
    surface_denied: Mutex<BTreeMap<String, u64>>,
    /// Per-surface rate-drop counters.
    surface_rate_dropped: Mutex<BTreeMap<String, u64>>,
    /// Signal redelivery dedupe (D-11.5).
    signal_envelope_duplicates: AtomicU64,
    /// Open pending flows by flow name (D-06).
    pending_flows_open: Mutex<BTreeMap<String, u64>>,
    /// Component-map row count (GC health).
    component_map_rows: AtomicU64,
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
                reports_delivered: Mutex::new(BTreeMap::new()),
                opencode_up: AtomicU64::new(0),
                gate_pending: AtomicU64::new(0),
                locks_held: AtomicU64::new(0),
                lock_age_seconds: AtomicU64::new(0),
                log_lag_seconds: AtomicU64::new(0),
                stream_state: AtomicU64::new(0),
                stream_live_ms: AtomicU64::new(0),
                frames_decode_failures: AtomicU64::new(0),
                frames_dropped: AtomicU64::new(0),
                stream_reconnects: Mutex::new(BTreeMap::new()),
                frames_total: Mutex::new(BTreeMap::new()),
                probe_failures: Mutex::new(BTreeMap::new()),
                detections: Mutex::new(BTreeMap::new()),
                repairs_accept: AtomicU64::new(0),
                repairs_park: AtomicU64::new(0),
                repairs_reset: AtomicU64::new(0),
                actions_by_state: [const { AtomicU64::new(0) }; 10],
                idle_buckets: [const { AtomicU64::new(0) }; 7],
                idle_count: AtomicU64::new(0),
                idle_sum_bits: AtomicU64::new(0),
                gate_requests: Mutex::new(BTreeMap::new()),
                surface_up: Mutex::new(BTreeMap::new()),
                surface_send_total: Mutex::new(BTreeMap::new()),
                surface_send_failures: Mutex::new(BTreeMap::new()),
                surface_denied: Mutex::new(BTreeMap::new()),
                surface_rate_dropped: Mutex::new(BTreeMap::new()),
                signal_envelope_duplicates: AtomicU64::new(0),
                pending_flows_open: Mutex::new(BTreeMap::new()),
                component_map_rows: AtomicU64::new(0),
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

    /// A report was delivered (exactly once by the dedupe CAS), labeled by
    /// the surface it landed on (§5.10).
    pub fn inc_reports_delivered(&self, surface: Surface) {
        let mut map = self
            .inner
            .reports_delivered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *map.entry(surface.as_str().to_owned()).or_insert(0) += 1;
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

    /// The event-stream state gauge (§4.9: 0 down / 1 connecting / 2 live).
    pub fn set_stream_state(&self, state: u8) {
        self.inner
            .stream_state
            .store(u64::from(state), Ordering::Relaxed);
    }

    /// Accumulate live time on the stream (NFR-15 availability math).
    pub fn add_stream_live_ms(&self, millis: u64) {
        self.inner
            .stream_live_ms
            .fetch_add(millis, Ordering::Relaxed);
    }

    /// One reconnect, labeled by reason (idle | error | drift | eof).
    pub fn inc_stream_reconnect(&self, reason: &str) {
        let mut map = self
            .inner
            .stream_reconnects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *map.entry(reason.to_owned()).or_insert(0) += 1;
    }

    /// One frame observed, by kind and whether the taxonomy mapped it.
    pub fn inc_frame(&self, kind: &str, mapped: bool) {
        let mut map = self
            .inner
            .frames_total
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *map.entry((kind.to_owned(), mapped)).or_insert(0) += 1;
    }

    /// `n` frames failed the tolerant header parse (P-24).
    pub fn inc_frames_decode_failures_by(&self, n: u64) {
        self.inner
            .frames_decode_failures
            .fetch_add(n, Ordering::Relaxed);
    }

    /// One frame dropped because the pump channel was full (§4.6).
    pub fn inc_frames_dropped(&self) {
        self.inner.frames_dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// One completion detection attributed to its scheduler (P-29: the W5
    /// flip gate reads the event/fallback split from this counter).
    pub fn inc_detection(&self, source: &str) {
        let mut map = self
            .inner
            .detections
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *map.entry(source.to_owned()).or_insert(0) += 1;
    }

    /// One failed probe by the polling loop named `loop_name` (D-11: the
    /// watcher's failures are no longer mislabeled as stream reconnects).
    pub fn inc_probe_failure(&self, loop_name: &str) {
        let mut map = self
            .inner
            .probe_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *map.entry(loop_name.to_owned()).or_insert(0) += 1;
    }

    /// Pending-gate gauge (driven by the gate loop, P4).
    pub fn set_gate_pending(&self, count: usize) {
        self.inner
            .gate_pending
            .store(u64::try_from(count).unwrap_or(0), Ordering::Relaxed);
    }

    /// Per-surface up gauge (D-07, §5.10).
    pub fn set_surface_up(&self, surface: Surface, up: bool) {
        let mut map = self
            .inner
            .surface_up
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.insert(surface.as_str().to_owned(), u64::from(up));
    }

    /// Per-surface, per-op send attempt counter.
    pub fn inc_surface_send(&self, surface: Surface, op: &str) {
        let mut map = self
            .inner
            .surface_send_total
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = (surface.as_str().to_owned(), op.to_owned());
        *map.entry(key).or_insert(0) += 1;
    }

    /// Per-surface, per-op send failure counter.
    pub fn inc_surface_send_failure(&self, surface: Surface, op: &str) {
        let mut map = self
            .inner
            .surface_send_failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = (surface.as_str().to_owned(), op.to_owned());
        *map.entry(key).or_insert(0) += 1;
    }

    /// Per-surface non-allowlisted sender drop (D-11.1).
    pub fn inc_surface_denied(&self, surface: Surface) {
        let mut map = self
            .inner
            .surface_denied
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *map.entry(surface.as_str().to_owned()).or_insert(0) += 1;
    }

    /// Per-surface rate-bucket drop (D-11.4).
    pub fn inc_surface_rate_dropped(&self, surface: Surface) {
        let mut map = self
            .inner
            .surface_rate_dropped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *map.entry(surface.as_str().to_owned()).or_insert(0) += 1;
    }

    /// Signal envelope-dedupe hit (D-11.5).
    pub fn inc_signal_envelope_duplicates(&self) {
        self.inner
            .signal_envelope_duplicates
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Open pending flows by flow name (D-06).
    pub fn set_pending_flows_open(&self, flow: &str, count: usize) {
        let mut map = self
            .inner
            .pending_flows_open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        map.insert(flow.to_owned(), u64::try_from(count).unwrap_or(0));
    }

    /// Component-map row count (GC health, §5.10).
    pub fn set_component_map_rows(&self, count: usize) {
        self.inner
            .component_map_rows
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
        out.push_str("# TYPE ocbot_reports_delivered_total counter\n");
        let reports = inner
            .reports_delivered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (surface, count) in reports.iter() {
            let _ = writeln!(
                out,
                "ocbot_reports_delivered_total{{surface=\"{surface}\"}} {count}"
            );
        }
        counter(
            &mut out,
            "ocbot_denied_interactions_total",
            inner.denied.load(Ordering::Relaxed),
        );
        render_stream(inner, &mut out);
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
        // --- SPEC-004 §5.10 surface families ------------------------------
        render_surface_gauge(&mut out, "ocbot_surface_up", &inner.surface_up);
        render_surface_counter(
            &mut out,
            "ocbot_surface_denied_total",
            &inner.surface_denied,
        );
        render_surface_counter(
            &mut out,
            "ocbot_surface_rate_dropped_total",
            &inner.surface_rate_dropped,
        );
        render_surface_op_counter(
            &mut out,
            "ocbot_surface_send_total",
            &inner.surface_send_total,
        );
        render_surface_op_counter(
            &mut out,
            "ocbot_surface_send_failures_total",
            &inner.surface_send_failures,
        );
        counter(
            &mut out,
            "ocbot_signal_envelope_duplicates_total",
            inner.signal_envelope_duplicates.load(Ordering::Relaxed),
        );
        out.push_str("# TYPE ocbot_pending_flows_open gauge\n");
        {
            let map = inner
                .pending_flows_open
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (flow, count) in map.iter() {
                let _ = writeln!(out, "ocbot_pending_flows_open{{flow=\"{flow}\"}} {count}");
            }
        }
        gauge(
            &mut out,
            "ocbot_component_map_rows",
            inner.component_map_rows.load(Ordering::Relaxed),
        );
        render_idle_histogram(inner, &mut out);
        out
    }
}

/// The SPEC-003 §4.9 stream families.
fn render_stream(inner: &Inner, out: &mut String) {
    gauge(
        out,
        "ocbot_stream_state",
        inner.stream_state.load(Ordering::Relaxed),
    );
    out.push_str("# TYPE ocbot_stream_reconnects_total counter\n");
    let reconnects = inner
        .stream_reconnects
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (reason, count) in reconnects.iter() {
        let _ = writeln!(
            out,
            "ocbot_stream_reconnects_total{{reason=\"{reason}\"}} {count}"
        );
    }
    let live_seconds = u64_to_f64(inner.stream_live_ms.load(Ordering::Relaxed)) / 1000.0;
    let _ = writeln!(out, "# TYPE ocbot_stream_live_seconds_total counter");
    let _ = writeln!(out, "ocbot_stream_live_seconds_total {live_seconds}");
    out.push_str("# TYPE ocbot_frames_total counter\n");
    let frames = inner
        .frames_total
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for ((kind, mapped), count) in frames.iter() {
        let _ = writeln!(
            out,
            "ocbot_frames_total{{kind=\"{}\",mapped=\"{}\"}} {count}",
            escape_label(kind),
            mapped
        );
    }
    counter(
        out,
        "ocbot_frames_decode_failures_total",
        inner.frames_decode_failures.load(Ordering::Relaxed),
    );
    counter(
        out,
        "ocbot_frames_dropped_total",
        inner.frames_dropped.load(Ordering::Relaxed),
    );
    out.push_str("# TYPE ocbot_detection_total counter\n");
    let detections = inner
        .detections
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (source, count) in detections.iter() {
        let _ = writeln!(out, "ocbot_detection_total{{source=\"{source}\"}} {count}");
    }
    out.push_str("# TYPE ocbot_probe_failures_total counter\n");
    let probes = inner
        .probe_failures
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (loop_name, count) in probes.iter() {
        let _ = writeln!(
            out,
            "ocbot_probe_failures_total{{loop=\"{}\"}} {count}",
            escape_label(loop_name)
        );
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

/// Convert a counter value to `f64` without the banned `as` cast.
fn u64_to_f64(value: u64) -> f64 {
    u32::try_from(value >> 32).map_or(0.0, |high| f64::from(high) * 4_294_967_296.0)
        + f64::from(u32::try_from(value & 0xFFFF_FFFF).unwrap_or(0))
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

/// The `ocbot_surface_up{surface}` gauge family.
fn render_surface_gauge(out: &mut String, name: &str, map: &Mutex<BTreeMap<String, u64>>) {
    out.push_str("# TYPE ");
    out.push_str(name);
    out.push_str(" gauge\n");
    let map = map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (surface, value) in map.iter() {
        let _ = writeln!(out, "{name}{{surface=\"{surface}\"}} {value}");
    }
}

/// A `..._total{surface}` counter family.
fn render_surface_counter(out: &mut String, name: &str, map: &Mutex<BTreeMap<String, u64>>) {
    out.push_str("# TYPE ");
    out.push_str(name);
    out.push_str(" counter\n");
    let map = map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (surface, count) in map.iter() {
        let _ = writeln!(out, "{name}{{surface=\"{surface}\"}} {count}");
    }
}

/// A `..._total{surface,op}` counter family.
fn render_surface_op_counter(
    out: &mut String,
    name: &str,
    map: &Mutex<BTreeMap<(String, String), u64>>,
) {
    out.push_str("# TYPE ");
    out.push_str(name);
    out.push_str(" counter\n");
    let map = map
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for ((surface, op), count) in map.iter() {
        let _ = writeln!(out, "{name}{{surface=\"{surface}\",op=\"{op}\"}} {count}");
    }
}
