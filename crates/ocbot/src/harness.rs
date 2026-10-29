//! A debug/test bridge that exposes the individually-callable git-truth steps
//! to a separate process, for the AC-6 real-SIGKILL chaos tests.
//!
//! This is **not** part of the bot's production wiring (`ocbot` is
//! `publish = false`); it exists so `tests/chaos_sigkill.rs` can spawn a real
//! process, have it park at a labelled `app::crashpoint`, SIGKILL it, and then
//! assert that `reconcile_git_states` recovers from DB/git facts alone. In a
//! release build the checkpoint hook is a no-op, so the same binary just runs a
//! sequence to completion.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;

use ocbot_core::error::OcError;
use ocbot_core::outcome::AssistantSnapshot;
use ocbot_core::ports::{
    Activity, GateDecision, GateRequest, OpenCodePort, PortSession, PromptAdmission, ServerFacts,
    SessionId,
};
use ocbot_core::render::OutgoingMessage;
use ocbot_core::{Destination, PostReceipt, Surface};

use crate::adapters::git::GitAdapter;
use crate::adapters::store::StoreHandle;
use crate::app::context::{AppContext, BroadcastOutcome, ChatPort, SendError};
use crate::app::guard::Guard;
use crate::app::handlers::{accept, park, reset};
use crate::app::metrics::Metrics;
use crate::config::Templates;

/// Read the harness environment and run one step sequence.
///
/// Environment: `OCBOT_CRASH_DB`, `OCBOT_CRASH_MODE`
/// (`accept|park|reset|store`), `OCBOT_CRASH_ACTION` (accept/park),
/// `OCBOT_CRASH_DIR`/`OCBOT_CRASH_TARGET` (reset/store), `OCBOT_CRASH_OPERATOR`,
/// and the `OCBOT_TEST_CRASH_AFTER`/`OCBOT_TEST_CRASH_DIR` pair consumed by the
/// checkpoint hook. `store` runs a fixed typed-store write sequence with a
/// checkpoint after each commit, so `tests/store_actor_chaos` can SIGKILL
/// between operations and assert durability.
pub async fn run_from_env() -> Result<(), String> {
    let db = env("OCBOT_CRASH_DB")?;
    let mode = env("OCBOT_CRASH_MODE")?;
    let operator = std::env::var("OCBOT_CRASH_OPERATOR")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(42_u64);
    let actor = format!("discord:{operator}");

    let store = StoreHandle::open(Path::new(&db)).map_err(|error| error.to_string())?;
    let fs = crate::adapters::fs::FsAdapter::new(
        vec![std::path::PathBuf::from("/")],
        std::env::temp_dir().join("ocbot-harness-inbox"),
        true,
        Arc::new(crate::adapters::clock::SystemClock),
    )
    .map_err(|error| error.to_string())?;
    let ctx = Arc::new(AppContext {
        store: store.clone(),
        opencode: Arc::new(DeadOpenCode),
        git: Arc::new(GitAdapter::new()),
        fs: Arc::new(fs),
        clock: Arc::new(crate::adapters::clock::SystemClock),
        abort_timeout_secs: 30,
        permission_ruleset: None,
        chat: Arc::new(NullChat {
            next: std::sync::atomic::AtomicU64::new(1),
        }),
        metrics: Arc::new(Metrics::new()),
        redactor: Arc::new(ocbot_core::Redactor::default()),
        health: Arc::new(crate::app::degraded::HealthMonitor::new()),
        guard: Guard {
            allowed_actors: vec![ocbot_core::ActorId::from_parts(
                ocbot_core::Surface::Discord,
                &operator.to_string(),
            )],
            ops_channel: None,
            inbox_channel: None,
        },
        templates: Templates {
            fresh: String::new(),
            resume: String::new(),
        },
        plan_heading_min: ocbot_core::DEFAULT_HEADING_MIN,
        plan_heading_max: ocbot_core::DEFAULT_HEADING_MAX,
        ls_page_size: ocbot_core::LS_PAGE_SIZE,
        projects: Arc::new(crate::adapters::projects::FsProjectAdapter::new(
            std::path::PathBuf::from("/tmp/projects"),
            ocbot_core::ports::default_plans_dirs(),
            ocbot_core::ports::default_readme_files(),
            Arc::new(crate::adapters::projects::TomlProjectConfigAdapter),
        )),
        project_config: Arc::new(crate::adapters::projects::TomlProjectConfigAdapter),
        projects_root: std::path::PathBuf::from("/tmp/projects"),
    });

    match mode.as_str() {
        "accept" => {
            let action_id = parse_id(&env("OCBOT_CRASH_ACTION")?)?;
            let subject =
                std::env::var("OCBOT_CRASH_SUBJECT").unwrap_or_else(|_| "recovered".to_owned());
            accept::execute_accept(&ctx, action_id, &subject, "", &actor)
                .await
                .map_err(|error| error.to_string())?;
        }
        "park" => {
            let action_id = parse_id(&env("OCBOT_CRASH_ACTION")?)?;
            park::begin_park(&ctx, action_id, &actor)
                .await
                .map_err(|error| error.to_string())?;
        }
        "reset" => {
            let dir = env("OCBOT_CRASH_DIR")?;
            let target = env("OCBOT_CRASH_TARGET")?;
            reset::begin_reset(&ctx, &dir, &target, &actor, None)
                .await
                .map_err(|error| error.to_string())?;
        }
        // A fixed typed-store write sequence; each commit is a labelled crash
        // point. `StoreHandle::open` above already created the schema.
        "store" => run_store_sequence(&store, &env("OCBOT_CRASH_DIR")?).await?,
        other => return Err(format!("unknown OCBOT_CRASH_MODE {other:?}")),
    }
    Ok(())
}

/// The `store` crash sequence: one typed commit per labelled checkpoint, so a
/// SIGKILL between them leaves exactly the committed prefix.
async fn run_store_sequence(store: &StoreHandle, dir: &str) -> Result<(), String> {
    let checkpoint = crate::app::crashpoint::checkpoint;
    let store_err = |error: crate::adapters::store::StoreError| error.to_string();
    store
        .upsert_project(dir, None, None)
        .await
        .map_err(store_err)?;
    checkpoint("STORE-PROJECT");
    let run_id = store.insert_run(dir, "free").await.map_err(store_err)?;
    checkpoint("STORE-RUN");
    let action_id = store
        .insert_action(run_id, Some("7"), "chaos", "prompt", 1)
        .await
        .map_err(store_err)?;
    checkpoint("STORE-ACTION");
    if !store
        .begin_send(action_id, dir, "ses_chaos", None, false)
        .await
        .map_err(store_err)?
    {
        return Err("begin_send did not take".to_owned());
    }
    checkpoint("STORE-SEND");
    if !store
        .deliver_report(
            action_id,
            ocbot_core::State::Reported,
            "m1",
            "completed",
            Some("done"),
            "{}",
            1,
        )
        .await
        .map_err(store_err)?
    {
        return Err("deliver_report did not take".to_owned());
    }
    checkpoint("STORE-REPORT");
    if !store
        .release_lock(dir, &action_id.to_string())
        .await
        .map_err(store_err)?
    {
        return Err("release_lock did not take".to_owned());
    }
    checkpoint("STORE-RELEASE");
    Ok(())
}

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("missing environment variable {name}"))
}

fn parse_id(raw: &str) -> Result<i64, String> {
    raw.parse().map_err(|_| format!("bad action id {raw:?}"))
}

/// A chat surface that accepts posts but does nothing (the harness does not
/// exercise notifications). Receipts are deterministic `test:<n>` ids.
struct NullChat {
    next: std::sync::atomic::AtomicU64,
}

#[async_trait]
impl ChatPort for NullChat {
    async fn post(
        &self,
        _dest: &Destination,
        _message: &OutgoingMessage,
    ) -> Result<PostReceipt, SendError> {
        let n = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(PostReceipt {
            surface: Surface::Discord,
            msg_id: ocbot_core::MsgId(format!("test:{n}")),
        })
    }

    async fn edit(&self, _receipt: &PostReceipt, _replacement: &OutgoingMessage) {}

    async fn react(&self, _receipt: &PostReceipt, _emoji: char) {}

    async fn broadcast(&self, _message: &OutgoingMessage) -> BroadcastOutcome {
        BroadcastOutcome::default()
    }

    fn surfaces(&self) -> Vec<Surface> {
        vec![Surface::Discord]
    }
}

/// An opencode port that is never called by accept/park/reset; its methods
/// exist only to satisfy the context type.
struct DeadOpenCode;

#[async_trait]
impl OpenCodePort for DeadOpenCode {
    async fn probe(&self) -> Result<ServerFacts, OcError> {
        Ok(ServerFacts {
            version: "2.0.24".to_owned(),
            urls: Vec::new(),
        })
    }

    async fn list_sessions(&self, _root_only: bool) -> Result<Vec<PortSession>, OcError> {
        Ok(Vec::new())
    }

    async fn activity(&self, _session_id: &str) -> Result<Activity, OcError> {
        Ok(Activity::Idle)
    }

    async fn create_session(
        &self,
        _directory: &Path,
        _title: Option<&str>,
        _permissions: Option<&[ocbot_core::ports::PermissionRule]>,
    ) -> Result<SessionId, OcError> {
        Ok("ses_harness".to_owned())
    }

    async fn send_prompt(&self, session_id: &str, _text: &str) -> Result<PromptAdmission, OcError> {
        Ok(PromptAdmission {
            id: "msg_harness".to_owned(),
            session_id: session_id.to_owned(),
            delivery: None,
        })
    }

    async fn last_assistant(
        &self,
        _session_id: &str,
    ) -> Result<Option<AssistantSnapshot>, OcError> {
        Ok(None)
    }

    async fn interrupt(&self, _session_id: &str) -> Result<(), OcError> {
        Ok(())
    }

    async fn pending_gates(&self) -> Result<Vec<GateRequest>, OcError> {
        Ok(Vec::new())
    }

    async fn reply_gate(
        &self,
        _session_id: &str,
        _gate_id: &str,
        _decision: GateDecision,
    ) -> Result<(), OcError> {
        Ok(())
    }

    async fn reply_form(
        &self,
        _session_id: &str,
        _form_id: &str,
        _answer: serde_json::Value,
    ) -> Result<(), OcError> {
        Ok(())
    }
}
