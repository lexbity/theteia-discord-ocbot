//! NFR-2 evidence: 100 report cycles against a wiremock-backed opencode (the
//! real client + adapter) complete well inside the 5 s p95 budget. Each cycle
//! seeds one running action in its own directory, ticks the watcher, and
//! records the idle→report wall time.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ocbot::adapters::opencode::OpenCodeAdapter;
use ocbot::adapters::store::StoreHandle;
use ocbot::app::interaction::{InteractionSource, SourceKind};
use ocbot::app::router;
use ocbot::app::watcher::Watcher;
use ocbot_core::State;
use serde_json::Value;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

use common::{FakeDiscord, FakeGit, FakeOpenCode};

const OPERATOR: u64 = 42;
const CHANNEL: u64 = 7;
const CYCLES: usize = 100;
const BUDGET: Duration = Duration::from_secs(5);

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../opencode-client/fixtures/v2/2.0.24")
}

fn fixture(name: &str) -> Value {
    let text = std::fs::read_to_string(fixtures_dir().join(name)).expect("fixture exists");
    serde_json::from_str(&text).expect("fixture json")
}

/// A stateless responder: every session is idle and ends with the recorded
/// completed assistant message.
struct LatencyResponder {
    info: Value,
    messages: Value,
    permission_empty: Value,
    form_empty: Value,
}

impl Respond for LatencyResponder {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let method = request.method.to_string();
        let path = request.url.path();
        if method == "GET" && path == "/api/info" {
            return ResponseTemplate::new(200).set_body_json(&self.info);
        }
        if method == "GET" && path == "/api/session/active" {
            return ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": {} }));
        }
        if method == "GET" && path.contains("/message") {
            return ResponseTemplate::new(200).set_body_json(&self.messages);
        }
        if method == "GET" && path == "/api/permission/request" {
            return ResponseTemplate::new(200).set_body_json(&self.permission_empty);
        }
        if method == "GET" && path == "/api/form" {
            return ResponseTemplate::new(200).set_body_json(&self.form_empty);
        }
        ResponseTemplate::new(404).set_body_string("unmocked")
    }
}

#[tokio::test]
async fn one_hundred_reports_stay_under_the_p95_budget() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(LatencyResponder {
            info: fixture("info.json"),
            messages: fixture("messages.json"),
            permission_empty: fixture("permission_pending_empty.json"),
            form_empty: fixture("form_pending_empty.json"),
        })
        .mount(&server)
        .await;

    let store = ocbot::adapters::store::StoreHandle::open_in_memory().expect("store");
    let opencode = OpenCodeAdapter::new(Arc::new(
        opencode_v2_client::Client::new(&server.uri(), "secret").expect("client"),
    ));
    let discord = FakeDiscord::new();
    let ctx = common::context(
        store.clone(),
        Arc::new(opencode),
        Arc::new(FakeGit::inert()),
        Arc::new(discord.clone()),
        OPERATOR,
        Some(CHANNEL),
    );
    let watcher = Watcher::new(ctx.clone(), Duration::from_millis(10));

    let mut latencies: Vec<Duration> = Vec::with_capacity(CYCLES);
    for index in 0..CYCLES {
        let dir = format!("/tmp/lat/{index}");
        let session = format!("ses_lat_{index}");
        let seeded =
            common::seed_running(&store, &dir, None, None, CHANNEL, "latency", &session).await;
        let started = Instant::now();
        watcher.tick().await.expect("tick");
        latencies.push(started.elapsed());
        let row = store
            .action(seeded.action_id)
            .await
            .expect("read")
            .expect("row");
        assert_eq!(row.state, State::Reported, "cycle {index} reported");
    }

    latencies.sort_unstable();
    let p95_index = latencies.len() * 95 / 100;
    let p95 = latencies[p95_index];
    assert!(
        p95 < BUDGET,
        "p95 idle→report {p95:?} exceeded the {BUDGET:?} budget (max {:?})",
        latencies.last().expect("non-empty"),
    );
    assert_eq!(discord.posts().len(), CYCLES, "one report per cycle");
}

/// NFR-1: the only work between receiving a button and issuing the deferral is
/// the guarded plan phase (one SQLite read). Measure it over 1,000 iterations
/// and hold it far under the 1 s ack budget.
#[tokio::test]
async fn plan_phase_stays_under_the_ack_budget() {
    let store = StoreHandle::open_in_memory().expect("store");
    store
        .upsert_project("/tmp/ack", Some("main"), Some(&"f".repeat(40)))
        .await
        .expect("project");
    let run = store.insert_run("/tmp/ack", "free").await.expect("run");
    let action = store
        .insert_action(
            run,
            Some(i64::try_from(CHANNEL).expect("channel")),
            "ack",
            "prompt",
            1,
        )
        .await
        .expect("action");
    let ctx = common::context(
        store,
        Arc::new(FakeOpenCode::new()),
        Arc::new(FakeGit::inert()),
        Arc::new(FakeDiscord::new()),
        OPERATOR,
        Some(CHANNEL),
    );
    let source = InteractionSource {
        kind: SourceKind::Component {
            custom_id: format!("v1:send:{action}"),
        },
        user_id: OPERATOR,
        channel_id: CHANNEL,
        guild_id: Some(1),
        message_id: Some(1),
        is_dm: false,
    };

    let mut samples = Vec::with_capacity(1_000);
    for _ in 0..1_000 {
        let started = Instant::now();
        let outcome = router::guarded_plan(&ctx, &source).await;
        samples.push(started.elapsed());
        assert!(matches!(outcome, router::PlanOutcome::Plan(_)));
    }
    samples.sort_unstable();
    let p99 = samples[samples.len() * 99 / 100];
    assert!(
        p99 < Duration::from_millis(50),
        "plan-phase p99 {p99:?} is not far under the 1 s ack budget"
    );
}
