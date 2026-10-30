//! R-12: a `DISCORD_TOKEN`-shaped secret in a report is scrubbed from the
//! inline Discord message (while the DB record keeps the raw text), and the
//! logging writer scrubs secrets from captured log lines.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::io;
use std::sync::{Arc, Mutex};

use ocbot::adapters::store::StoreHandle;
use ocbot::app::context::{AppContext, ChatPort};
use ocbot::app::degraded::HealthMonitor;
use ocbot::app::guard::Guard;
use ocbot::app::logging;
use ocbot::app::metrics::Metrics;
use ocbot::app::watcher::Watcher;
use ocbot_core::Redactor;
use ocbot_core::ports::{Clock, OpenCodePort};

use common::{FakeChat, FakeClock, FakeGit, FakeOpenCode, seed_running};

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;
const SECRET: &str = "DISCORD_TOKEN=abcdefgh12345678";

fn redactor() -> Arc<Redactor> {
    let patterns = vec![r"(?i)discord[_-]?token\s*[:=]\s*[A-Za-z0-9._\-]{8,}".to_owned()];
    Arc::new(Redactor::new(&patterns).expect("pattern compiles"))
}

fn redacting_ctx(
    store: StoreHandle,
    opencode: Arc<dyn OpenCodePort>,
    discord: Arc<dyn ChatPort>,
) -> Arc<AppContext> {
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::at(1_000));
    Arc::new(AppContext {
        store,
        opencode,
        git: Arc::new(FakeGit::inert()),
        fs: common::permissive_fs(clock.clone()),
        clock,
        abort_timeout_secs: common::DEFAULT_ABORT_TIMEOUT_SECS,
        permission_ruleset: None,
        chat: discord,
        metrics: Arc::new(Metrics::new()),
        redactor: redactor(),
        health: Arc::new(HealthMonitor::new()),
        guard: Guard {
            allowed_actors: vec![ocbot_core::ActorId::from_parts(
                ocbot_core::Surface::Discord,
                &OPERATOR.to_string(),
            )],
            ops_channel: Some(CHANNEL),
            inbox_channel: None,
        },
        templates: common::workspace_templates(),
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
    })
}

#[tokio::test]
async fn a_secret_is_scrubbed_inline_but_recorded_verbatim() {
    let store = StoreHandle::open_in_memory().expect("store");
    let seeded = seed_running(
        &store,
        "/tmp/redact",
        Some("main"),
        Some(&"e".repeat(40)),
        CHANNEL,
        "leaky",
        "ses_red",
    )
    .await;
    let opencode = FakeOpenCode::new();
    let mut snapshot = FakeOpenCode::completed_snapshot();
    snapshot.text = format!("{SECRET}\n{}", "detail ".repeat(500));
    opencode.set_snapshot("ses_red", snapshot);
    let discord = FakeChat::new();
    let ctx = redacting_ctx(store.clone(), Arc::new(opencode), Arc::new(discord.clone()));

    Watcher::new(ctx.clone(), std::time::Duration::from_millis(10))
        .tick()
        .await
        .expect("tick");

    let report = discord
        .posts()
        .into_iter()
        .rev()
        .find(|post| post.message.content.contains("Completed"))
        .expect("report posted");
    assert!(
        !report.message.content.contains("abcdefgh12345678"),
        "inline content must be scrubbed"
    );
    assert!(report.message.content.contains("[redacted]"));

    let row = store
        .action(seeded.action_id)
        .await
        .expect("read")
        .expect("row");
    assert!(
        row.report_full
            .as_deref()
            .is_some_and(|body| body.contains("abcdefgh12345678")),
        "the DB record keeps the raw agent text (documented residual)"
    );
}

/// A recording `MakeWriter` for the log-scrub assertion.
#[derive(Clone)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buffer {
    type Writer = BufferWriter;

    fn make_writer(&'a self) -> Self::Writer {
        BufferWriter(self.0.clone())
    }
}

struct BufferWriter(Arc<Mutex<Vec<u8>>>);

impl io::Write for BufferWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("buffer lock").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn logging_writer_scrubs_captured_lines() {
    use tracing_subscriber::layer::SubscriberExt as _;

    let buffer = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .json()
            .with_writer(logging::scrub_writer(Buffer(buffer.clone()), redactor())),
    );

    tracing::subscriber::with_default(subscriber, || {
        tracing::error!(detail = SECRET, "a leaky log line");
    });

    let captured = String::from_utf8(buffer.lock().expect("lock").clone()).expect("utf8");
    assert!(
        !captured.contains("abcdefgh12345678"),
        "secret reached the log writer: {captured}"
    );
    assert!(captured.contains("[redacted]"), "{captured}");
    assert!(captured.contains("a leaky log line"), "{captured}");
}

#[test]
fn logging_writer_passes_lines_through_when_disarmed() {
    use tracing_subscriber::layer::SubscriberExt as _;

    let buffer = Arc::new(Mutex::new(Vec::new()));
    let subscriber =
        tracing_subscriber::registry().with(tracing_subscriber::fmt::layer().json().with_writer(
            logging::scrub_writer(Buffer(buffer.clone()), Arc::new(Redactor::default())),
        ));

    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(detail = "nothing sensitive", "plain line");
    });

    let captured = String::from_utf8(buffer.lock().expect("lock").clone()).expect("utf8");
    assert!(captured.contains("plain line"), "{captured}");
    assert!(!captured.contains("[redacted]"), "{captured}");
}

/// AC-E8 (SPEC-003 W4): a seeded secret riding in a frame's payload never
/// reaches the logs. The dispatcher's structural rule (P-32) is that frames
/// are reduced to kind + session id and never logged — this drives hostile
/// frames through the real dispatcher under a capturing subscriber and
/// asserts neither the payload-bearing kind nor the secret-shaped session id
/// leaks.
#[test]
fn frame_payloads_never_reach_the_logs() {
    use ocbot::app::wake::Dispatcher;
    use opencode_v2_client::EventFrame;
    use tracing_subscriber::layer::SubscriberExt as _;

    let buffer = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .json()
            .with_writer(logging::scrub_writer(Buffer(buffer.clone()), redactor())),
    );

    let discord = FakeChat::new();
    let store = StoreHandle::open_in_memory().expect("store");
    let ctx = common::context(
        store,
        Arc::new(FakeOpenCode::new()),
        Arc::new(FakeGit::inert()),
        Arc::new(discord),
        OPERATOR,
        None,
    );

    // A dedicated current-thread runtime installed *inside* the capturing
    // subscriber's scope: every log line the dispatcher emits is captured.
    tracing::subscriber::with_default(subscriber, || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("inner runtime")
            .block_on(async move {
                let (tx, rx) = tokio::sync::mpsc::channel(8);
                let cfg = ocbot::config::EventsConfig::spec_defaults();
                let task = Dispatcher::spawn_if_enabled(ctx, &cfg, rx).expect("dispatcher");
                // Hostile frames: the secret rides where a payload would, and
                // a secret-shaped string rides where the session id would
                // (ids are the only frame fields the dispatcher ever touches).
                let _ = tx
                    .send(EventFrame {
                        event: format!("session.{SECRET}"),
                        session_id: Some("ses_secretabcdefgh123".to_owned()),
                    })
                    .await;
                drop(tx);
                let _ = task.await;
            });
    });

    let captured = String::from_utf8(buffer.lock().expect("lock").clone()).expect("utf8");
    assert!(
        !captured.contains("abcdefgh12345678"),
        "the frame's secret-shaped payload leaked into the logs: {captured}"
    );
    assert!(
        !captured.contains("ses_secretabcdefgh123"),
        "the frame's session id leaked into the logs: {captured}"
    );
}
