//! The event-stream supervisor (SPEC-003 §4.4, W3): one connection to
//! `/api/event`, owned honestly — idle-deadline reconnects with exponential
//! backoff and jitter, a reconnect ⇒ coalesced status-facts reconcile (P-23),
//! decode-burst drift detection (P-24), and exactly-one down/recovery
//! announcements past the threshold (P-28).
//!
//! **Frames are counted and dropped here — nothing consumes them yet.** The
//! dispatcher lands in W4; this phase is strictly more telemetry, zero
//! behavior change to decisions.
//!
//! P-32: no frame field other than the event kind and session id is ever
//! logged, at any level — and this module logs neither.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use opencode_v2_client::{DECODE_BURST_RECONNECT, DECODE_BURST_WINDOW_MS};

use crate::app::context::{AppContext, EventSource, StreamHandle, StreamItem};
use crate::app::degraded;
use crate::app::reconcile;
use crate::config::EventsConfig;

/// The supervisor's tuning, projected from the `[events]` config block.
#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    /// Frame silence longer than this declares the connection dead (P-22).
    pub idle_deadline: Duration,
    /// Reconnect backoff floor.
    pub backoff_min: Duration,
    /// Reconnect backoff ceiling.
    pub backoff_max: Duration,
    /// Down at least this long ⇒ exactly one Discord post.
    pub announce_after: Duration,
    /// Reconciles are coalesced to at most one per this interval (P-23).
    pub reconcile_min_interval: Duration,
}

impl SupervisorConfig {
    /// Project from the `[events]` config block.
    #[must_use]
    pub fn from_events(events: &EventsConfig) -> Self {
        Self {
            idle_deadline: Duration::from_secs(events.idle_deadline_secs.max(1)),
            backoff_min: Duration::from_millis(events.backoff_min_ms.max(1)),
            backoff_max: Duration::from_millis(events.backoff_max_ms.max(1)),
            announce_after: Duration::from_secs(events.announce_after_secs.max(1)),
            reconcile_min_interval: Duration::from_secs(events.reconcile_min_interval_secs.max(1)),
        }
    }
}

/// Why the previous connection ended (the reconnect metric's label).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectReason {
    /// No bytes within the idle deadline (P-22).
    Idle,
    /// The server closed the stream.
    Eof,
    /// A transport error.
    Error,
    /// More than [`DECODE_BURST_RECONNECT`] header failures within
    /// [`DECODE_BURST_WINDOW_MS`] (P-24).
    Drift,
}

impl DisconnectReason {
    fn label(self) -> &'static str {
        match self {
            DisconnectReason::Idle => "idle",
            DisconnectReason::Eof => "eof",
            DisconnectReason::Error => "error",
            DisconnectReason::Drift => "drift",
        }
    }
}

/// The stream supervisor task. See the module docs for the phase contract.
pub struct Supervisor {
    ctx: Arc<AppContext>,
    source: Arc<dyn EventSource>,
    cfg: SupervisorConfig,
    /// Jitter source for the backoff, `[0, 1)`; injected so paused-time tests
    /// are deterministic. Production derives it from the wall clock's
    /// sub-second nanos.
    jitter: Box<dyn Fn() -> f64 + Send + Sync>,
}

/// A reconcile is worth up to one full server sweep; keep the drift check and
/// the live-time accounting responsive without spinning.
const SUPERVISOR_TICK: Duration = Duration::from_millis(500);

impl Supervisor {
    /// Build a supervisor over an event source.
    pub fn new(ctx: Arc<AppContext>, source: Arc<dyn EventSource>, cfg: SupervisorConfig) -> Self {
        Self {
            ctx,
            source,
            cfg,
            jitter: Box::new(nanos_jitter),
        }
    }

    /// Replace the jitter source (tests inject deterministic values).
    #[must_use]
    pub fn with_jitter(mut self, jitter: Box<dyn Fn() -> f64 + Send + Sync>) -> Self {
        self.jitter = jitter;
        self
    }

    /// Spawn behind `events.enabled`; `None` (and zero connection attempts)
    /// when the stream is disabled — pure-poll mode (FR-43).
    pub fn spawn_if_enabled(
        ctx: Arc<AppContext>,
        source: Arc<dyn EventSource>,
        events: &EventsConfig,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if !events.enabled {
            tracing::info!("event stream disabled ([events].enabled = false): pure-poll mode");
            return None;
        }
        let supervisor = Self::new(ctx, source, SupervisorConfig::from_events(events));
        Some(tokio::spawn(async move {
            supervisor.run_forever().await;
        }))
    }

    /// The §4.4 machine: `Connecting → Live → Backoff → Connecting …` until
    /// the process drains. Announcements are edge-triggered through the
    /// monitor; flaps below `announce_after` never post.
    pub async fn run_forever(self) {
        let health = Arc::new(degraded::HealthMonitor::new());
        let mut attempt: u32 = 0;
        let mut down_since: Option<Instant> = None;
        let mut announced_down = false;
        let mut last_reconcile: Option<Instant> = None;
        loop {
            // --- Connecting ---
            self.ctx.metrics.set_stream_state(1);
            let mut handle = match self.source.connect().await {
                Ok(handle) => handle,
                Err(error) => {
                    tracing::warn!(error = %error, "event stream connect failed");
                    attempt = self
                        .backoff(
                            &health,
                            &mut down_since,
                            &mut announced_down,
                            attempt,
                            DisconnectReason::Error,
                        )
                        .await;
                    continue;
                }
            };

            // --- Live ---
            self.ctx.metrics.set_stream_state(2);
            attempt = 0;
            last_reconcile = self.reconcile_if_due(last_reconcile).await;
            let (reason, saw_frame) = self.live_session(&mut handle).await;
            // A received frame is a genuine recovery: the announcement clock
            // resets, and a pending down state is cleared exactly once
            // (P-28). A connect that never saw a frame is *not* a recovery —
            // the outage clock keeps running.
            if saw_frame && announced_down {
                degraded::announce(
                    &self.ctx,
                    true,
                    DOWN_MESSAGE.to_owned(),
                    UP_MESSAGE.to_owned(),
                )
                .await;
                announced_down = false;
                down_since = None;
            }
            if let Some(reason) = reason {
                attempt = self
                    .backoff(
                        &health,
                        &mut down_since,
                        &mut announced_down,
                        attempt,
                        reason,
                    )
                    .await;
            }
        }
    }

    /// The live phase: consume frames, reset the deadline on each, watch for
    /// silence (P-22), drift (P-24), and closure. Returns the reason the
    /// connection ended (if any) and whether any frame arrived.
    async fn live_session(&self, handle: &mut StreamHandle) -> (Option<DisconnectReason>, bool) {
        let mut deadline = Instant::now() + self.cfg.idle_deadline;
        let mut decode_base = handle.decode_failures();
        let mut decode_window = Instant::now();
        let mut window_failures: u64 = 0;
        let mut last_live_mark = Instant::now();
        let mut saw_frame = false;
        let mut tick = tokio::time::interval(SUPERVISOR_TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                item = handle.next_item() => {
                    match item {
                        Some(StreamItem::Frame(_frame)) => {
                            // Counted at the pump; nothing consumes frames yet (W4).
                            saw_frame = true;
                            deadline = Instant::now() + self.cfg.idle_deadline;
                        }
                        Some(StreamItem::Closed) | None => {
                            return (Some(DisconnectReason::Eof), saw_frame);
                        }
                    }
                }
                () = tokio::time::sleep_until(deadline) => {
                    return (Some(DisconnectReason::Idle), saw_frame);
                }
                _ = tick.tick() => {
                    // Live-time accounting (NFR-15).
                    let now = Instant::now();
                    self.ctx
                        .metrics
                        .add_stream_live_ms(u64::try_from((now - last_live_mark).as_millis()).unwrap_or(0));
                    last_live_mark = now;
                    // Decode-burst drift (P-24): more than the burst
                    // threshold within the window ⇒ the server has drifted.
                    let failures = handle.decode_failures();
                    let delta = failures.saturating_sub(decode_base);
                    if delta > 0 {
                        self.ctx.metrics.inc_frames_decode_failures_by(delta);
                        decode_base = failures;
                        window_failures += delta;
                    }
                    if now.duration_since(decode_window) >= duration_ms(DECODE_BURST_WINDOW_MS) {
                        decode_window = now;
                        window_failures = 0;
                    } else if window_failures > u64::from(DECODE_BURST_RECONNECT) {
                        return (Some(DisconnectReason::Drift), saw_frame);
                    }
                }
            }
        }
    }

    /// Fire the status-facts reconcile at most once per
    /// `reconcile_min_interval`, forced at the first Live entry (P-23/FR-49).
    async fn reconcile_if_due(&self, last: Option<Instant>) -> Option<Instant> {
        let now = Instant::now();
        if let Some(last) = last
            && now - last < self.cfg.reconcile_min_interval
        {
            return Some(last);
        }
        let report = reconcile::reconcile(&self.ctx).await;
        tracing::info!(
            completed = report.completed,
            repaired = report.repaired,
            "event-stream reconcile fired"
        );
        Some(now)
    }

    /// The backoff phase: label and count the reconnect, announce the outage
    /// once past `announce_after`, sleep the exponential delay with jitter,
    /// and return the next attempt number.
    async fn backoff(
        &self,
        health: &Arc<degraded::HealthMonitor>,
        down_since: &mut Option<Instant>,
        announced_down: &mut bool,
        attempt: u32,
        reason: DisconnectReason,
    ) -> u32 {
        self.ctx.metrics.set_stream_state(0);
        self.ctx.metrics.inc_stream_reconnect(reason.label());
        let since = down_since.get_or_insert_with(Instant::now);
        let delay = self.backoff_delay(attempt);
        let mut slept = Duration::ZERO;
        while slept < delay {
            let step = delay.min(SUPERVISOR_TICK);
            tokio::time::sleep(step).await;
            slept += step;
            if !*announced_down
                && Instant::now().saturating_duration_since(*since) >= self.cfg.announce_after
            {
                if health.observe(false).is_some() {
                    degraded::announce(
                        &self.ctx,
                        false,
                        DOWN_MESSAGE.to_owned(),
                        UP_MESSAGE.to_owned(),
                    )
                    .await;
                }
                *announced_down = true;
            }
        }
        tracing::info!(
            reason = reason.label(),
            attempt,
            delay_ms = u64::try_from(delay.as_millis()).unwrap_or(0),
            "event stream down; reconnecting with backoff"
        );
        attempt + 1
    }

    /// Exponential backoff: [`backoff_delay`] with this supervisor's knobs
    /// and injected jitter source.
    fn backoff_delay(&self, attempt: u32) -> Duration {
        backoff_delay(&self.cfg, attempt, (self.jitter)())
    }
}

/// Exponential backoff `min * 2^attempt` capped at `max`, scaled by the
/// jitter factor (±20 %: the injected source yields `[0, 1)` mapped to
/// `[0.8, 1.2)`). Pure; the FR-50 shape is pinned by direct tests.
#[must_use]
pub fn backoff_delay(cfg: &SupervisorConfig, attempt: u32, jitter: f64) -> Duration {
    let growth = cfg.backoff_min.saturating_mul(1_u32 << attempt.min(20));
    let capped = growth.min(cfg.backoff_max);
    let factor = 0.8 + 0.4 * jitter.clamp(0.0, 1.0);
    Duration::from_secs_f64(capped.as_secs_f64() * factor)
}

/// The down/recovery announcement texts (P-28; distinct from FR-38's
/// server-reachability posts).
const DOWN_MESSAGE: &str =
    "⚠️ opencode event stream is down — completions fall back to polling until it recovers";
const UP_MESSAGE: &str = "✅ opencode event stream recovered";

/// Production jitter: the wall clock's sub-second nanos as `[0, 1)`.
fn nanos_jitter() -> f64 {
    f64::from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos()),
    ) / 1_000_000_000.0
}

fn duration_ms(ms: u64) -> Duration {
    Duration::from_millis(ms)
}
