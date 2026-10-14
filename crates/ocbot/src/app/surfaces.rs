//! Surface selection, registry, and per-surface health (SPEC-004 PH-2).
//!
//! The supervision core stays surface-blind: it only sees an `Arc<dyn
//! ChatPort>`. Everything in this module is wiring — which surfaces run, how
//! their ports are instrumented, and how (un)healthy transitions are announced
//! to the surviving surfaces (D-07: edge-triggered, exactly one notice per
//! outage and per recovery). It contains no protocol logic.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ocbot_core::{Destination, OutgoingMessage, PostReceipt, Surface};

use crate::app::context::{BroadcastOutcome, ChatPort, SendError};
use crate::app::metrics::Metrics;
use crate::config::Config;

/// Consecutive send failures before a surface is declared down (the PH-2 edge
/// threshold: `N-1` failures stay silent, the `N`-th announces once).
pub const SURFACE_DOWN_FAILURES: u64 = 3;

/// The selected surface set (SPEC-004 D-09): an ordered, duplicate-free list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceSet(Vec<Surface>);

impl SurfaceSet {
    /// Resolve the configured surfaces, collecting every validation problem
    /// instead of failing on the first.
    pub fn from_config(config: &Config) -> Result<Self, Vec<String>> {
        let names = config.channel.names();
        if names.is_empty() {
            return Err(vec!["channel must select at least one surface".to_owned()]);
        }
        let mut surfaces = Vec::with_capacity(names.len());
        let mut errors = Vec::new();
        for name in &names {
            match Surface::parse(name) {
                Some(surface) if surfaces.contains(&surface) => errors.push(format!(
                    "channel lists a surface twice: {name:?} (one entry per surface)"
                )),
                Some(surface) => surfaces.push(surface),
                None => errors.push(format!(
                    "channel lists unknown surface {name:?} (known: discord, signal)"
                )),
            }
        }
        if errors.is_empty() {
            Ok(Self(surfaces))
        } else {
            Err(errors)
        }
    }

    /// The selected surfaces, in configuration order.
    pub fn iter(&self) -> impl Iterator<Item = Surface> + '_ {
        self.0.iter().copied()
    }

    /// How many surfaces are selected.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether no surface is selected (always false for a validated set).
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether `surface` is selected.
    pub fn contains(&self, surface: Surface) -> bool {
        self.0.contains(&surface)
    }

    /// The set without `surface` (the PH-3 boot gate: a surface the doctor
    /// refuses is dropped, and the survivors keep the process alive).
    #[must_use]
    pub fn without(self, surface: Surface) -> Self {
        Self(self.0.into_iter().filter(|kept| *kept != surface).collect())
    }
}

/// Shared per-surface health state (cheap to clone; atomics inside). The
/// down/up edges drive both the `ocbot_surface_up` gauge and the
/// edge-triggered cross-surface notices.
#[derive(Debug, Clone)]
pub struct SurfaceHealth {
    inner: Arc<HealthInner>,
}

#[derive(Debug)]
struct HealthInner {
    consecutive_failures: AtomicU64,
    /// 1 = up, 0 = down; the swap is the edge detector.
    up: AtomicU64,
}

impl SurfaceHealth {
    /// A fresh monitor for one surface (it starts up: a surface that never
    /// sends is not failing).
    pub fn new() -> Self {
        Self {
            inner: Arc::new(HealthInner {
                consecutive_failures: AtomicU64::new(0),
                up: AtomicU64::new(1),
            }),
        }
    }

    /// Whether the surface is currently considered up.
    pub fn is_up(&self) -> bool {
        self.inner.up.load(Ordering::Relaxed) == 1
    }

    /// The current consecutive-failure count.
    pub fn consecutive_failures(&self) -> u64 {
        self.inner.consecutive_failures.load(Ordering::Relaxed)
    }

    /// Record a successful send. Returns `true` exactly when this flipped the
    /// surface from down back to up (a fresh recovery edge).
    pub fn record_success(&self) -> bool {
        self.inner.consecutive_failures.store(0, Ordering::Relaxed);
        self.inner.up.swap(1, Ordering::Relaxed) == 0
    }

    /// Record a failed send. Returns `true` exactly when this failure crossed
    /// [`SURFACE_DOWN_FAILURES`] from below (a fresh down edge). Later
    /// failures of the same outage return `false`.
    pub fn record_failure(&self) -> bool {
        let failures = self
            .inner
            .consecutive_failures
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        if failures == SURFACE_DOWN_FAILURES {
            self.inner.up.store(0, Ordering::Relaxed);
            true
        } else {
            false
        }
    }
}

impl Default for SurfaceHealth {
    fn default() -> Self {
        Self::new()
    }
}

/// An instrumented [`ChatPort`] decorator (D-07/D-14): every send is counted,
/// failures move the health machine, and down/up *edges* post one notice on
/// each surviving surface. The decorator is surface-blind — it never parses
/// platform data, and health notices ride the siblings' raw transports so
/// they cannot recurse through instrumentation.
pub struct InstrumentedPort {
    surface: Surface,
    inner: Arc<dyn ChatPort>,
    health: SurfaceHealth,
    metrics: Arc<Metrics>,
    /// The other surfaces' announce channels, for health notices.
    siblings: Vec<(Destination, Arc<dyn ChatPort>)>,
}

impl InstrumentedPort {
    /// Wrap one surface's transport.
    pub fn new(
        surface: Surface,
        port: Arc<dyn ChatPort>,
        metrics: Arc<Metrics>,
        siblings: Vec<(Destination, Arc<dyn ChatPort>)>,
    ) -> Self {
        Self {
            surface,
            inner: port,
            health: SurfaceHealth::new(),
            metrics,
            siblings,
        }
    }

    /// The live health handle (ops display).
    pub fn health(&self) -> SurfaceHealth {
        self.health.clone()
    }

    /// Fold one send result into metrics + health + notices.
    async fn record(&self, op: &str, failed: bool) {
        if !failed {
            if self.health.record_success() {
                self.metrics.set_surface_up(self.surface, true);
                self.announce(true).await;
            }
            return;
        }
        self.metrics.inc_surface_send_failure(self.surface, op);
        if self.health.record_failure() {
            self.metrics.set_surface_up(self.surface, false);
            self.announce(false).await;
        }
    }

    /// One notice on every sibling surface.
    async fn announce(&self, up: bool) {
        let text = if up {
            format!("✅ the {} surface is back up", self.surface)
        } else {
            format!(
                "⚠️ the {} surface is down — messages will not deliver there until it recovers",
                self.surface
            )
        };
        let notice = OutgoingMessage {
            content: text,
            attachment: None,
            components: Vec::new(),
            mention_allowlist: Vec::new(),
        };
        for (destination, port) in &self.siblings {
            if let Err(error) = port.post(destination, &notice).await {
                tracing::warn!(surface = %self.surface, error = %error, "health notice failed");
            }
        }
    }
}

#[async_trait::async_trait]
impl ChatPort for InstrumentedPort {
    async fn post(
        &self,
        destination: &Destination,
        message: &OutgoingMessage,
    ) -> Result<PostReceipt, SendError> {
        self.metrics.inc_surface_send(self.surface, "post");
        let result = self.inner.post(destination, message).await;
        self.record("post", result.is_err()).await;
        result
    }

    async fn edit(&self, receipt: &PostReceipt, replacement: &OutgoingMessage) {
        self.metrics.inc_surface_send(self.surface, "edit");
        self.inner.edit(receipt, replacement).await;
    }

    async fn react(&self, receipt: &PostReceipt, emoji: char) {
        self.metrics.inc_surface_send(self.surface, "react");
        self.inner.react(receipt, emoji).await;
    }

    async fn broadcast(&self, message: &OutgoingMessage) -> BroadcastOutcome {
        self.metrics.inc_surface_send(self.surface, "broadcast");
        let outcome = self.inner.broadcast(message).await;
        // D-07's ≥1-success contract: any delivery proves the surface alive;
        // only total failure counts against it.
        self.record("broadcast", outcome.delivered.is_empty()).await;
        outcome
    }

    fn surfaces(&self) -> Vec<Surface> {
        self.inner.surfaces()
    }
}

/// One selected surface's wiring unit: the surface and its instrumented port.
#[derive(Clone)]
pub struct PortHandle {
    /// The surface this handle serves.
    pub surface: Surface,
    /// The instrumented port.
    pub port: Arc<dyn ChatPort>,
}

/// The boot-time surface registry: seeds the health gauges and wraps each
/// per-surface transport in an [`InstrumentedPort`] whose siblings are the
/// *other* surfaces' announce channels. It collapses to the single
/// `Arc<dyn ChatPort>` the context carries — with one selected surface (the
/// only reachable shape until PH-4 refuses the second) the registry *is* that
/// port; PH-6's fan-out replaces the collapse without touching callers.
pub struct SurfaceRegistry {
    handles: Vec<PortHandle>,
}

impl SurfaceRegistry {
    /// Build the registry from `(surface, announce destination, transport)`
    /// triples — one per selected surface. Health notices ride the siblings'
    /// raw transports, so they are neither counted against the sibling nor
    /// able to recurse through instrumentation.
    pub fn build(
        transports: Vec<(Surface, Destination, Arc<dyn ChatPort>)>,
        metrics: &Arc<Metrics>,
    ) -> Self {
        let wires: Vec<(Surface, Destination, Arc<dyn ChatPort>)> = transports;
        let handles = wires
            .iter()
            .map(|(surface, _announce_destination, port)| {
                metrics.set_surface_up(*surface, true);
                let siblings = wires
                    .iter()
                    .filter(|(sibling, _, _)| *sibling != *surface)
                    .map(|(_, sibling_destination, sibling_port)| {
                        (sibling_destination.clone(), sibling_port.clone())
                    })
                    .collect();
                PortHandle {
                    surface: *surface,
                    port: Arc::new(InstrumentedPort::new(
                        *surface,
                        port.clone(),
                        metrics.clone(),
                        siblings,
                    )),
                }
            })
            .collect();
        Self { handles }
    }

    /// The port the app context carries. Config validation refuses to select
    /// a second surface until the PH-6 fan-out exists, so the registry always
    /// holds exactly one handle today.
    pub fn single(&self) -> Arc<dyn ChatPort> {
        self.handles[0].port.clone()
    }

    /// Every handle, in selection order (ops display).
    pub fn handles(&self) -> &[PortHandle] {
        &self.handles
    }
}
