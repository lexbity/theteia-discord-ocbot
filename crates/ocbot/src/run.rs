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
use crate::adapters::opencode::OpenCodeAdapter;
use crate::adapters::store::StoreHandle;
use crate::app::context::{AppContext, DiscordPort};
use crate::app::degraded::HealthMonitor;
use crate::app::doctor;
use crate::app::gates_loop::GatesLoop;
use crate::app::guard::Guard;
use crate::app::inbox::Inbox;
use crate::app::logging;
use crate::app::metrics::{self, Metrics};
use crate::app::ops::OpsLoop;
use crate::app::reconcile;
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
    let Some(token) = secrets.discord_token else {
        tracing::error!("missing {}", crate::config::ENV_DISCORD_TOKEN);
        return ExitCode::from(1);
    };
    let Some(operator) = config.resolved_operator() else {
        tracing::error!("missing allowed_user_id (config or env)");
        return ExitCode::from(1);
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
    let opencode: Arc<dyn OpenCodePort> = Arc::new(OpenCodeAdapter::new(client));
    let discord_surface: Arc<dyn DiscordPort> =
        Arc::new(DiscordAdapter::new(shared_http(&token, operator), operator));
    let guard = Guard {
        allowed_user_id: operator,
        ops_channel: config.ops_channel,
        inbox_channel: config.inbox_channel,
    };
    let ctx = Arc::new(AppContext {
        store: store.clone(),
        opencode,
        git,
        fs,
        clock,
        abort_timeout_secs: config.abort_timeout_secs,
        permission_ruleset,
        discord: discord_surface,
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

    let intents = serenity::all::GatewayIntents::GUILDS
        | serenity::all::GatewayIntents::GUILD_MESSAGES
        | serenity::all::GatewayIntents::DIRECT_MESSAGES
        | serenity::all::GatewayIntents::MESSAGE_CONTENT;
    let shard_manager = match start_gateway(&token, intents, ctx.clone()).await {
        Ok(manager) => manager,
        Err(error) => {
            tracing::error!(error = %error, "gateway start failed");
            return ExitCode::from(1);
        }
    };

    match tokio::signal::ctrl_c().await {
        Ok(()) => tracing::info!("ctrl-c received; draining"),
        Err(error) => {
            tracing::error!(error = %error, "signal handler failed");
            return ExitCode::from(1);
        }
    }

    shard_manager.shutdown_all().await;
    store.shutdown().await;
    tracing::info!("drained; goodbye");
    ExitCode::SUCCESS
}
