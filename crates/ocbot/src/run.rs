//! The imperative wiring (spec Phase 2): load config, doctor the server,
//! start the store actor, metrics server, watcher, and the serenity gateway;
//! drain cleanly on SIGINT/SIGTERM.

use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use opencode_v2_client::Client as OpenCodeClient;

use crate::adapters::discord::{DiscordAdapter, shared_http, start_gateway};
use crate::adapters::git::GitAdapter;
use crate::adapters::opencode::OpenCodeAdapter;
use crate::adapters::store::StoreHandle;
use crate::app::context::{AppContext, DiscordPort};
use crate::app::guard::Guard;
use crate::app::metrics::{self, Metrics};
use crate::app::reconcile;
use crate::app::watcher::Watcher;
use crate::config::{Config, Secrets, config_path};
use ocbot_core::ports::{GitPort, OpenCodePort};

/// The tokio entry invoked by `main`. Returns the process exit code.
#[expect(
    clippy::too_many_lines,
    reason = "the bootstrap sequence is a linear I/O chain; per-step extraction would add indirection without reuse"
)]
pub async fn start() -> ExitCode {
    init_tracing();

    let (config, templates) = match Config::load(config_path()) {
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

    let metrics = Arc::new(Metrics::new());
    let git: Arc<dyn GitPort> = Arc::new(GitAdapter::with_bin(
        "git",
        Duration::from_secs(config.git_timeout_secs),
    ));
    let opencode: Arc<dyn OpenCodePort> = Arc::new(OpenCodeAdapter::new(client));
    let discord_surface: Arc<dyn DiscordPort> =
        Arc::new(DiscordAdapter::new(shared_http(&token, operator), operator));
    let guard = Guard {
        allowed_user_id: operator,
        ops_channel: config.ops_channel,
    };
    let ctx = Arc::new(AppContext {
        store: store.clone(),
        opencode,
        git,
        discord: discord_surface,
        metrics: metrics.clone(),
        guard,
        templates,
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

    let intents = serenity::all::GatewayIntents::GUILDS
        | serenity::all::GatewayIntents::GUILD_MESSAGES
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

/// Structured JSON logging to stderr (NFR-9). Secrets never pass through here
/// (grep gate).
fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .json()
        .with_writer(std::io::stderr)
        .try_init();
}
