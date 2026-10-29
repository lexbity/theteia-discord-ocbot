//! A live Signal surface under test: the pinned fake daemon, a real app
//! context, and the inbound pump running — everything `signal_e2e.rs` and
//! `signal_flows.rs` drive. Extracted from the e2e suite so both share one
//! construction path (PH-7): a surface that is built differently per suite
//! would let the two drift.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::signal_fake::{FakeSignal, Script, text_envelope};
use super::{FakeClock, FakeGit, FakeOpenCode, permissive_fs, workspace_templates};
use ocbot::adapters::signal::{SignalAdapter, SignalInbound, SignalMachine};
use ocbot::adapters::store::StoreHandle;
use ocbot::app::context::{AppContext, ChatPort};
use ocbot::app::guard::Guard;
use ocbot::app::metrics::Metrics;
use ocbot::config::RateLimit;
use ocbot_core::ports::{Clock, GitPort, OpenCodePort};
use ocbot_core::{ActorId, Destination, OutgoingMessage, Surface};

/// The operator's number (the fake daemon's single account).
pub const OPERATOR: &str = "+15551234567";
/// A number that is not the operator (trust-boundary tests).
pub const STRANGER: &str = "+15550000000";
/// The fake daemon's constant `send` reply (the pinned contract).
pub const SENT_TS: u64 = 1_700_000_000_000;

/// A live Signal surface: the daemon, the context, and the running pump.
pub struct SignalSurface {
    /// The temp dir holding the socket and the staged attachments.
    pub tmp: tempfile::TempDir,
    /// The fake signal-cli daemon.
    pub daemon: FakeSignal,
    /// The app context the pump routes through.
    pub ctx: Arc<AppContext>,
    /// The surface's outbound adapter.
    pub adapter: Arc<SignalAdapter>,
    /// The scripted opencode port.
    pub opencode: Arc<FakeOpenCode>,
    /// The store the surface runs on.
    pub store: StoreHandle,
    /// The metrics registry the surface reports to.
    pub metrics: Arc<Metrics>,
    /// The scriptable clock.
    pub clock: Arc<FakeClock>,
    /// The next inbound envelope timestamp.
    next_ts: AtomicU64,
}

impl SignalSurface {
    /// Build a surface over an in-memory store.
    pub async fn new(script: Script, rate: Option<RateLimit>, git: Arc<dyn GitPort>) -> Self {
        let store = StoreHandle::open_in_memory().expect("store");
        Self::with_store(store, script, rate, git).await
    }

    /// Build a surface over a caller-owned store (the crash/restart tests use a
    /// file-backed one so a dropped surface leaves the flow on disk).
    pub async fn with_store(
        store: StoreHandle,
        script: Script,
        rate: Option<RateLimit>,
        git: Arc<dyn GitPort>,
    ) -> Self {
        let dir = tempfile::tempdir().expect("tmp");
        let socket = dir.path().join("signal.sock");
        let daemon = FakeSignal::bind(socket.clone(), script);
        let metrics = Arc::new(Metrics::new());
        let clock = Arc::new(FakeClock::at(1_700_000_000));
        let machine = SignalMachine::new(
            socket,
            Duration::from_millis(10),
            Duration::from_millis(20),
            metrics.clone(),
        )
        .with_jitter(Box::new(|| 0.5));
        let slot = machine.connection();
        let adapter = Arc::new(SignalAdapter::new(
            OPERATOR,
            slot,
            store.clone(),
            metrics.clone(),
            dir.path().join("outbox"),
        ));
        let (sink_tx, sink_rx) = tokio::sync::mpsc::channel(64);
        tokio::spawn(machine.run_forever(sink_tx));
        wait_for(
            || {
                metrics
                    .render_text()
                    .contains("ocbot_surface_up{surface=\"signal\"} 1")
            },
            "the machine subscribes",
        )
        .await;

        let chat: Arc<dyn ChatPort> = adapter.clone();
        let opencode = Arc::new(FakeOpenCode::new());
        let opencode_port: Arc<dyn OpenCodePort> = opencode.clone();
        let clock_port: Arc<dyn Clock> = clock.clone();
        let fs = permissive_fs(clock_port);
        let ctx = Arc::new(AppContext {
            store: store.clone(),
            opencode: opencode_port,
            git,
            fs,
            clock: clock.clone(),
            abort_timeout_secs: 30,
            permission_ruleset: None,
            chat,
            metrics: metrics.clone(),
            redactor: Arc::new(ocbot_core::Redactor::default()),
            health: Arc::new(ocbot::app::degraded::HealthMonitor::new()),
            guard: Guard {
                allowed_actors: vec![ActorId::from_parts(Surface::Signal, OPERATOR)],
                ops_channel: None,
                inbox_channel: None,
            },
            templates: workspace_templates(),
            plan_heading_min: ocbot_core::DEFAULT_HEADING_MIN,
            plan_heading_max: ocbot_core::DEFAULT_HEADING_MAX,
            ls_page_size: ocbot_core::LS_PAGE_SIZE,
            projects: Arc::new(ocbot::adapters::projects::FsProjectAdapter::new(
                std::path::PathBuf::from("/tmp/projects"),
                ocbot_core::ports::default_plans_dirs(),
                ocbot_core::ports::default_readme_files(),
                Arc::new(ocbot::adapters::projects::TomlProjectConfigAdapter),
            )),
            project_config: Arc::new(ocbot::adapters::projects::TomlProjectConfigAdapter),
            projects_root: std::path::PathBuf::from("/tmp/projects"),
        });
        let inbound = Arc::new(SignalInbound::new(
            ctx.clone(),
            adapter.clone(),
            OPERATOR,
            rate.unwrap_or_default(),
        ));
        tokio::spawn(inbound.run(sink_rx));
        Self {
            tmp: dir,
            daemon,
            ctx,
            adapter,
            opencode,
            store,
            metrics,
            clock,
            next_ts: AtomicU64::new(1_700_000_100_000),
        }
    }

    /// The socket path a *second* surface on the same database should use (a
    /// fresh path, so a dropped surface's reconnect loop cannot cross-talk).
    pub fn spare_socket(dir: &std::path::Path) -> std::path::PathBuf {
        dir.join("signal-restart.sock")
    }

    /// Stream a text message from `sender`.
    pub fn send(&self, sender: &str, text: &str) {
        let ts = self.next_ts.fetch_add(1, Ordering::Relaxed);
        let id = self.daemon.subscribe_id();
        self.daemon
            .stream_envelope(id, &text_envelope(sender, ts, text));
    }

    /// Stream a raw envelope result.
    pub fn stream_raw(&self, result: &serde_json::Value) {
        let id = self.daemon.subscribe_id();
        self.daemon.stream_envelope(id, result);
    }

    /// Every text the surface has sent to the operator, in order.
    pub fn sent(&self) -> Vec<String> {
        self.daemon
            .requests()
            .into_iter()
            .filter(|recorded| recorded.method == "send")
            .filter_map(|recorded| recorded.params[0]["message"].as_str().map(str::to_owned))
            .collect()
    }

    /// Wait until a sent message contains `needle`.
    pub async fn wait_sent(&self, needle: &str) {
        for _ in 0..600 {
            if self.sent().iter().any(|text| text.contains(needle)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never sent {needle:?}: {:?}", self.sent());
    }

    /// Seed a `reported` action on the signal surface.
    pub async fn seed_reported(&self, dir: &str, label: &str, work_branch: Option<&str>) -> i64 {
        self.store
            .upsert_project(dir, work_branch, None)
            .await
            .expect("project");
        let run_id = self.store.insert_run(dir, "free").await.expect("run");
        let action_id = self
            .store
            .insert_action(run_id, Some(OPERATOR), label, "prompt", 1)
            .await
            .expect("action");
        self.store
            .set_action_destination(action_id, Surface::Signal.as_str(), OPERATOR)
            .await
            .expect("destination");
        assert!(
            self.store
                .begin_send(action_id, dir, "ses_seed", None, false)
                .await
                .expect("begin_send"),
            "the lock is taken"
        );
        assert!(
            self.store
                .cas_state(
                    action_id,
                    ocbot_core::State::Running,
                    ocbot_core::State::Reported,
                    &[]
                )
                .await
                .expect("cas"),
            "running → reported"
        );
        action_id
    }

    /// Seed a `staged` action on the signal surface.
    pub async fn seed_staged(&self, dir: &str, label: &str) -> i64 {
        self.store
            .upsert_project(dir, None, None)
            .await
            .expect("project");
        let run_id = self.store.insert_run(dir, "free").await.expect("run");
        let action_id = self
            .store
            .insert_action(run_id, Some(OPERATOR), label, "prompt", 1)
            .await
            .expect("action");
        self.store
            .set_action_destination(action_id, Surface::Signal.as_str(), OPERATOR)
            .await
            .expect("destination");
        action_id
    }

    /// Remember `dir` as this chat's cwd.
    pub async fn set_cwd(&self, dir: &str) {
        let actor = ActorId::from_parts(Surface::Signal, OPERATOR).0;
        self.store
            .upsert_project(dir, None, None)
            .await
            .expect("project");
        self.store.set_ui_cwd(&actor, dir).await.expect("cwd");
    }

    /// The state of an action.
    pub async fn state(&self, action_id: i64) -> ocbot_core::State {
        self.store
            .action(action_id)
            .await
            .expect("action read")
            .expect("present")
            .state
    }

    /// The surface's destination.
    pub fn destination(&self) -> Destination {
        self.adapter.destination()
    }
}

/// Poll until `condition` holds (bounded).
pub async fn wait_for(condition: impl Fn() -> bool, what: &str) {
    for _ in 0..600 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never became true: {what}");
}

/// Poll until the action reaches `want`.
pub async fn wait_state(surface: &SignalSurface, action_id: i64, want: ocbot_core::State) {
    for _ in 0..600 {
        if surface.state(action_id).await == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("action {action_id} never reached {want:?}");
}

/// A git port backed by a real repo.
pub fn repo_git(repo: &std::path::Path) -> Arc<dyn GitPort> {
    Arc::new(FakeGit::with_repo(Some(repo.to_path_buf())))
}

/// A plain message spec.
pub fn notice(text: &str) -> OutgoingMessage {
    OutgoingMessage {
        content: text.to_owned(),
        attachment: None,
        components: Vec::new(),
        mention_allowlist: Vec::new(),
    }
}
