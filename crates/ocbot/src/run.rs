//! The imperative wiring (spec §6.1): build the secret scrubber and logging
//! first, load config, doctor the server, start the store actor, metrics
//! server, watcher, gates/inbox loops, the ops loop (health + gauges + stall),
//! and the serenity gateway; drain cleanly on SIGINT/SIGTERM.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use opencode_v2_client::Client as OpenCodeClient;

use crate::adapters::clock::SystemClock;
use crate::adapters::discord::{DiscordAdapter, shared_http, start_gateway};
use crate::adapters::fs::FsAdapter;
use crate::adapters::git::GitAdapter;
use crate::adapters::opencode::{OpenCodeAdapter, OpenCodeEventSource};
use crate::adapters::store::StoreHandle;
use crate::app::context::{AppContext, ChatPort, EventSource};
use crate::app::degraded::HealthMonitor;
use crate::app::doctor;
use crate::app::gates_loop::GatesLoop;
use crate::app::guard::Guard;
use crate::app::inbox::Inbox;
use crate::app::logging;
use crate::app::metrics::{self, Metrics};
use crate::app::ops::OpsLoop;
use crate::app::reconcile;
use crate::app::supervisor::Supervisor;
use crate::app::surfaces::{SurfaceRegistry, SurfaceSet};
use crate::app::wake::Dispatcher;
use crate::app::watcher::Watcher;
use crate::config::{Config, Secrets, config_path};
use ocbot_core::Redactor;
use ocbot_core::ports::{Clock, FsPort, GitPort, OpenCodePort, PermissionRule};

/// The tokio entry invoked by `main`. Returns the process exit code.
#[expect(
    clippy::too_many_lines,
    reason = "the bootstrap sequence is a linear I/O chain; per-step extraction would add indirection without reuse"
)]
pub async fn start() -> ExitCode {
    // The shutdown listeners go in before anything else: a SIGTERM that lands
    // *while booting* — between the config load and the surface registry —
    // must still drain (store checkpoint, shard shutdown) instead of killing
    // the process between phases.
    let shutdown_listener = match shutdown_listener() {
        Ok(listener) => listener,
        Err(error) => {
            // Before the scrubber exists, so this goes to stderr unscrubbed:
            // the message is the platform's io error, never a secret.
            use std::io::Write as _;
            let _ = writeln!(
                std::io::stderr(),
                "cannot install the signal handlers: {error}"
            );
            return ExitCode::from(1);
        }
    };
    // The scrubber must exist before the first log line: load the config
    // first, fall back to the built-in patterns if it does not parse, then
    // install the JSON subscriber. Config errors are reported through it.
    let loaded = Config::load(config_path());
    let redactor = Arc::new(match &loaded {
        Ok((config, _)) => {
            Redactor::new(&config.redact_patterns).unwrap_or_else(|_| Redactor::default())
        }
        Err(_) => Redactor::new(&crate::config::default_redact_patterns())
            .unwrap_or_else(|_| Redactor::default()),
    });
    logging::init(redactor.clone());

    let (config, templates) = match loaded {
        Ok(pair) => pair,
        Err(errors) => {
            for error in errors {
                tracing::error!("config error: {error}");
            }
            return ExitCode::from(1);
        }
    };
    // Surface selection (SPEC-004 PH-2/PH-3): resolved before anything else
    // so a signal-only box skips the Discord-only requirements entirely.
    let mut surfaces = match SurfaceSet::from_config(&config) {
        Ok(surfaces) => surfaces,
        Err(errors) => {
            for error in errors {
                tracing::error!("surface set error: {error}");
            }
            return ExitCode::from(1);
        }
    };
    let discord_selected = surfaces.contains(ocbot_core::Surface::Discord);
    let signal_selected = surfaces.contains(ocbot_core::Surface::Signal);

    // SPEC-004 PH-3 doctor gate (FR-114/FR-116): the signal surface must
    // prove a healthy, current daemon or it is refused — the surface only;
    // discord (if selected) keeps booting (D-10/NFR-103). The whole check is
    // budgeted at 2 s, so an absent daemon never stalls the boot. PH-3 ships
    // no transport yet, so even a healthy daemon is dropped with a loud
    // warning (the surface lands in PH-4).
    if signal_selected {
        match doctor::signal_daemon(&config.signal).await {
            Ok(report) => {
                tracing::info!(
                    version = %report.version,
                    accounts = ?report.accounts,
                    "signal-cli daemon healthy (FR-114/FR-116)"
                );
                tracing::warn!(
                    "signal transport lands in SPEC-004 PH-4; refusing the surface until then"
                );
            }
            Err(errors) => {
                for error in &errors {
                    tracing::error!(error = %error, "signal surface refused by the boot doctor");
                }
            }
        }
        surfaces = surfaces.without(ocbot_core::Surface::Signal);
        if surfaces.is_empty() {
            tracing::error!("no chat surface survived the boot doctor — refusing to start");
            return ExitCode::from(3);
        }
    }

    let secrets = match Secrets::from_env() {
        Ok(secrets) => secrets,
        Err(errors) => {
            for error in errors {
                tracing::error!("secrets error: {error}");
            }
            return ExitCode::from(1);
        }
    };
    let Some(password) = secrets.opencode_password else {
        tracing::error!("missing {}", crate::config::ENV_OPENCODE_PASSWORD);
        return ExitCode::from(1);
    };
    // Discord credentials are required iff the discord surface is selected
    // (SPEC-004 D-09: a signal-only box must boot without them).
    let discord_credentials: Option<(String, u64)> = if discord_selected {
        let Some(token) = secrets.discord_token else {
            tracing::error!("missing {}", crate::config::ENV_DISCORD_TOKEN);
            return ExitCode::from(1);
        };
        let Some(operator) = config.resolved_operator() else {
            tracing::error!("missing [discord] allowed_user_id (config or env)");
            return ExitCode::from(1);
        };
        Some((token, operator))
    } else {
        None
    };

    // Doctor, FR-40: refuse wrong server versions; tolerate "down for now".
    let client = match OpenCodeClient::new(&config.opencode_url, &password) {
        Ok(client) => client,
        Err(error) => {
            tracing::error!("{error}");
            return ExitCode::from(1);
        }
    };
    match client.probe().await {
        Ok(info) => {
            tracing::info!(version = %info.version, "opencode v2 confirmed");
        }
        Err(
            opencode_v2_client::OcError::UnsupportedServerVersion { .. }
            | opencode_v2_client::OcError::MissingInfoEndpoint { .. },
        ) => {
            tracing::error!("server is not opencode v2 — refusing to start (FR-40)");
            return ExitCode::from(1);
        }
        Err(error) => {
            tracing::warn!(error = %error, "opencode unreachable; starting degraded");
        }
    }

    let store = match StoreHandle::open(Path::new(&config.db_path)) {
        Ok(store) => store,
        Err(error) => {
            tracing::error!(error = %error, "database open failed");
            return ExitCode::from(1);
        }
    };

    // Doctor, FR-40 part (c): the shipped permission baseline must parse as a
    // v2 ruleset and cover the FR-21 dangerous families as `ask`. A bad
    // baseline is a configuration fault, so it exits 2 (distinct from the
    // version fault in part (b), which exits 1).
    let baseline = match doctor::Baseline::load(Path::new(&config.permissions_file)) {
        Ok(baseline) => baseline,
        Err(errors) => {
            for error in &errors {
                tracing::error!(error = %error, "permission baseline rejected");
            }
            return ExitCode::from(2);
        }
    };
    let permission_ruleset = if config.ruleset_enabled() {
        tracing::info!("FR-42 active: attaching the baseline to created sessions");
        Some(
            baseline
                .rules
                .iter()
                .map(|rule| PermissionRule {
                    action: rule.action.clone(),
                    resource: rule.resource.clone(),
                    effect: rule.effect,
                })
                .collect(),
        )
    } else {
        None
    };

    let metrics = Arc::new(Metrics::new());
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let git: Arc<dyn GitPort> = Arc::new(GitAdapter::with_bin(
        "git",
        Duration::from_secs(config.git_timeout_secs),
    ));
    let fs: Arc<dyn FsPort> = match FsAdapter::new(
        config
            .browse_roots
            .iter()
            .map(Path::new)
            .map(Path::to_path_buf)
            .collect(),
        Path::new(&config.inbox_dir).to_path_buf(),
        config.freeform_paths(),
        clock.clone(),
    ) {
        Ok(adapter) => Arc::new(adapter),
        Err(error) => {
            tracing::error!(error = %error, "filesystem adapter rejected the browse roots");
            return ExitCode::from(1);
        }
    };
    let client = Arc::new(client);
    let opencode: Arc<dyn OpenCodePort> = Arc::new(OpenCodeAdapter::new(client.clone()));
    // Surface registry (SPEC-004 PH-2/PH-3): one instrumented port per
    // surviving surface, handed to the core as the collapsed port.
    // The announce destination: where broadcasts (degraded/stall notices,
    // later reports) land — the inbox channel, falling back to ops (P14).
    let announce_channel = config.discord.inbox_channel.or(config.discord.ops_channel);
    let announce = ocbot_core::Destination::discord(announce_channel.unwrap_or(0));
    let discord_transport: Option<Arc<dyn ChatPort>> =
        discord_credentials
            .as_ref()
            .map(|(token, operator)| -> Arc<dyn ChatPort> {
                Arc::new(
                    DiscordAdapter::new(shared_http(token, *operator), *operator)
                        .with_announce(announce_channel),
                )
            });
    let mut transports: Vec<(
        ocbot_core::Surface,
        ocbot_core::Destination,
        Arc<dyn ChatPort>,
    )> = Vec::new();
    for surface in surfaces.iter() {
        match surface {
            ocbot_core::Surface::Discord => {
                if let Some(port) = &discord_transport {
                    transports.push((surface, announce.clone(), port.clone()));
                } else {
                    // Unreachable: the credentials above are required iff the
                    // surface is selected.
                    tracing::error!("discord selected but no transport was built");
                }
            }
            // Unreachable: the PH-3 doctor gate drops the signal
            // surface before transports are built (the transport is PH-4).
            ocbot_core::Surface::Signal => {
                tracing::error!("no transport available for surface {surface}");
            }
        }
    }
    if transports.is_empty() {
        tracing::error!("no chat surface has a transport — refusing to start");
        return ExitCode::from(3);
    }
    let registry = SurfaceRegistry::build(transports, &metrics);
    tracing::info!(
        surfaces = ?registry.handles().iter().map(|handle| handle.surface).collect::<Vec<_>>(),
        "surface registry built"
    );
    let chat: Arc<dyn ChatPort> = registry.single();
    let guard = Guard {
        // One operator identity per configured surface (D-03). A signal-only
        // box has no Discord identity; PH-4 adds the Signal one (D-11).
        allowed_actors: discord_credentials
            .as_ref()
            .map(|(_, operator)| {
                vec![ocbot_core::ActorId::from_parts(
                    ocbot_core::Surface::Discord,
                    &operator.to_string(),
                )]
            })
            .unwrap_or_default(),
        ops_channel: config.discord.ops_channel,
        inbox_channel: config.discord.inbox_channel,
    };
    let ctx = Arc::new(AppContext {
        store: store.clone(),
        opencode,
        git,
        fs,
        clock,
        abort_timeout_secs: config.abort_timeout_secs,
        permission_ruleset,
        chat,
        metrics: metrics.clone(),
        redactor,
        health: Arc::new(HealthMonitor::new()),
        guard,
        templates,
        plan_heading_min: config.plan_heading_min,
        plan_heading_max: config.plan_heading_max,
        ls_page_size: config.ls_page_size,
    });

    if let Ok(counts) = store.action_counts_by_state().await {
        metrics.seed_actions(&counts);
    }

    // Event-stream supervisor + wake dispatcher (SPEC-003 W3/W4): behind
    // `[events].enabled`; in pure-poll mode neither task exists and no
    // connection is attempted (FR-43). The channel mirrors §4.6's bounded
    // supervisor→dispatcher link.
    let (frame_tx, frame_rx) =
        tokio::sync::mpsc::channel(crate::app::context::FRAME_CHANNEL_CAPACITY);
    let events_source: Arc<dyn EventSource> =
        Arc::new(OpenCodeEventSource::new(client.clone(), metrics.clone()));
    Supervisor::spawn_if_enabled(ctx.clone(), events_source, &config.events, frame_tx);
    Dispatcher::spawn_if_enabled(ctx.clone(), &config.events, frame_rx);

    let metrics_bind = config.metrics_bind.clone();
    {
        let metrics = metrics.clone();
        tokio::spawn(async move {
            if let Err(error) = metrics::serve(&metrics_bind, metrics).await {
                tracing::warn!(error = %error, "metrics server exited");
            }
        });
    }

    let report = reconcile::reconcile(&ctx).await;
    tracing::info!(
        server_up = report.server_up,
        completed = report.completed,
        "boot reconcile done"
    );

    let interval = Duration::from_millis(config.poll_interval_ms.max(100));
    {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            Watcher::new(ctx, interval).run_forever().await;
        });
    }

    // Gate lifecycle loop (P4): sync server gate facts and re-ping unanswered
    // gates on the injected clock.
    let gate_interval = Duration::from_millis(config.gate_poll_interval_ms.max(100));
    let reping_secs =
        i64::try_from(config.perm_reping_minutes.saturating_mul(60)).unwrap_or(i64::MAX);
    {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            GatesLoop::new(ctx, gate_interval, reping_secs)
                .run_forever()
                .await;
        });
    }

    // Inbox loop (P14/S-7): external root sessions are polled every
    // `inbox_poll_interval_ms`; completions post to the inbox channel.
    let inbox_interval = Duration::from_millis(config.inbox_poll_interval_ms.max(100));
    {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            Inbox::new(ctx, inbox_interval).run_forever().await;
        });
    }

    // Ops loop (Phase 7): reachability announcement (FR-38), derived gauges,
    // and the stall watchdog (P3/FR-39).
    let ops_interval = Duration::from_millis(config.ops_poll_interval_ms.max(1_000));
    let stall_secs = i64::try_from(config.stall_minutes.saturating_mul(60)).unwrap_or(i64::MAX);
    {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            OpsLoop::new(ctx, ops_interval, stall_secs)
                .run_forever()
                .await;
        });
    }

    // The serenity gateway exists iff the discord surface does (SPEC-004
    // D-09: a signal-only box has no Discord connection at all).
    let shard_manager = if let Some((token, _)) = &discord_credentials {
        let intents = serenity::all::GatewayIntents::GUILDS
            | serenity::all::GatewayIntents::GUILD_MESSAGES
            | serenity::all::GatewayIntents::DIRECT_MESSAGES
            | serenity::all::GatewayIntents::MESSAGE_CONTENT;
        match start_gateway(token, intents, ctx.clone()).await {
            Ok(manager) => Some(manager),
            Err(error) => {
                tracing::error!(error = %error, "gateway start failed");
                return ExitCode::from(1);
            }
        }
    } else {
        tracing::info!("discord surface not selected: skipping the serenity gateway");
        None
    };

    let signal = shutdown_signal(shutdown_listener).await;
    tracing::info!(signal, "shutdown received; draining");

    if let Some(shard_manager) = shard_manager {
        shard_manager.shutdown_all().await;
    }
    store.shutdown().await;
    tracing::info!("drained; goodbye");
    ExitCode::SUCCESS
}

/// Wait for SIGINT or SIGTERM. `systemd` sends SIGTERM (`KillSignal=SIGTERM`),
/// so both must drain: a bare `ctrl_c()` would terminate on SIGTERM without the
/// store's WAL checkpoint (spec NFR-9). Returns the signal name for the log.
///
/// The listeners are installed by [`shutdown_listener`] at the very top of
/// [`start`] — a signal that arrives *while booting* (between the config load
/// and the registry) must drain cleanly, not kill the process mid-chain.
async fn shutdown_signal(listener: ShutdownListener) -> &'static str {
    listener.wait().await
}

/// The SIGINT/SIGTERM listeners, registered before anything else runs.
struct ShutdownListener {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

impl ShutdownListener {
    /// Await whichever signal arrives first, naming it for the log.
    async fn wait(mut self) -> &'static str {
        tokio::select! {
            _ = self.interrupt.recv() => "SIGINT",
            _ = self.terminate.recv() => "SIGTERM",
        }
    }
}

/// Install the shutdown listeners. Errors only if the platform refuses to
/// register them (a permissions/cgroup oddity); the caller degrades to
/// running without a drain path rather than refusing to boot.
fn shutdown_listener() -> std::io::Result<ShutdownListener> {
    use tokio::signal::unix::{SignalKind, signal};
    Ok(ShutdownListener {
        interrupt: signal(SignalKind::interrupt())?,
        terminate: signal(SignalKind::terminate())?,
    })
}
