//! Shared test harness: recording fake ports + context builder for the ocbot
//! integration tests. Integration tests assert via unwrap/panic; production
//! code remains strict (workspace lint floor + G3).

#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "shared test harness; the subset used varies per test crate, and tests assert by unwrapping (C6)"
)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;

use ocbot_core::outcome::{AssistantSnapshot, TokenCounts};
use ocbot_core::ports::{
    Activity, Clock, GateDecision, GateRequest, GitPort, HookFailure, OpenCodePort, PermissionRule,
    PortSession, PromptAdmission, ServerFacts, SessionId,
};
use ocbot_core::{GitError, OcError, OutgoingMessage};

use ocbot::adapters::store::StoreHandle;
use ocbot::app::context::{AppContext, DiscordPort, SendError};
use ocbot::app::guard::Guard;
use ocbot::app::metrics::Metrics;
use ocbot::config::Templates;

/// A recorded fake call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// `probe`
    Probe,
    /// `list_sessions(root_only)`
    ListSessions(bool),
    /// `activity(session)`
    Activity(String),
    /// `create_session(dir, title)`
    CreateSession(String, Option<String>),
    /// `send_prompt(session, text)`
    SendPrompt(String, String),
    /// `last_assistant(session)`
    LastAssistant(String),
    /// `interrupt(session)`
    Interrupt(String),
    /// `pending_gates`
    PendingGates,
    /// `reply_gate(session, gate, decision)`
    ReplyGate(String, String, GateDecision),
    /// `reply_form(session, form, answer)`
    ReplyForm(String, String, serde_json::Value),
    /// git: `is_repo(dir)`
    GitIsRepo(String),
    /// git: `head(dir)`
    GitHead(String),
    /// git: `branch(dir)`
    GitBranch(String),
    /// git: `status_porcelain(dir)`
    GitStatus(String),
    /// git: `diff_stat(dir, base)`
    GitDiffStat(String, String),
    /// git: `log(dir, base)`
    GitLog(String, Option<String>),
    /// git: `is_ancestor`
    GitIsAncestor(String, String, String),
    /// git: `untracked_files(dir)`
    GitUntracked(String),
    /// git: `commit_all(dir, no_verify)`
    GitCommit(String, bool),
    /// git: `branch_exists(dir, name)`
    GitBranchExists(String, String),
    /// git: `create_branch_at(dir, name, at)`
    GitCreateBranch(String, String, String),
    /// git: `reset_hard(dir, sha)`
    GitReset(String, String),
}

/// Stateful fake opencode port. Behavior is scripted; every call is recorded.
#[derive(Debug, Clone, Default)]
pub struct FakeOpenCodeInner {
    pub calls: Vec<Call>,
    pub activities: HashMap<String, Activity>,
    pub snapshots: HashMap<String, AssistantSnapshot>,
    pub gates: Vec<GateRequest>,
    pub sessions: Vec<PortSession>,
    pub sessions_created: Vec<String>,
    pub unreachable: bool,
    /// When set, `reply_gate`/`reply_form` return this many transport failures
    /// first (retry test).
    pub transport_failures_remaining: u64,
    /// When set, `reply_gate`/`reply_form` answer with an "unknown request"
    /// 404 (FR-23 success path).
    pub reply_404: bool,
}

/// A `Clone`-able fake opencode port.
#[derive(Clone)]
pub struct FakeOpenCode {
    inner: Arc<Mutex<FakeOpenCodeInner>>,
}

impl FakeOpenCode {
    /// Build an empty fake.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(FakeOpenCodeInner::default())),
        }
    }

    /// Access the inner scripted state.
    pub fn inner(&self) -> std::sync::MutexGuard<'_, FakeOpenCodeInner> {
        self.inner.lock().expect("lock")
    }

    /// Mark a session busy or idle.
    pub fn set_activity(&self, session: &str, activity: Activity) {
        self.inner().activities.insert(session.to_owned(), activity);
    }

    /// Script the report snapshot a session ends with.
    pub fn set_snapshot(&self, session: &str, snapshot: AssistantSnapshot) {
        self.inner().snapshots.insert(session.to_owned(), snapshot);
    }

    /// Serve a completed PONG snapshot.
    pub fn completed_snapshot() -> AssistantSnapshot {
        AssistantSnapshot {
            text: "PONG".to_owned(),
            has_tool_calls: false,
            error: None,
            finish: Some("stop".to_owned()),
            cost: Some(0.01),
            tokens: Some(TokenCounts {
                input: 10.0,
                output: 5.0,
                reasoning: 0.0,
            }),
        }
    }

    /// Script the pending gates served to `pending_gates`.
    pub fn set_gates(&self, gates: Vec<GateRequest>) {
        self.inner().gates = gates;
    }

    /// Script the session list served to `list_sessions` (gate attribution).
    pub fn set_sessions(&self, sessions: Vec<PortSession>) {
        self.inner().sessions = sessions;
    }

    /// Make the next `transport_failures_remaining` replies fail with a
    /// transport error (tests the FR-23 retry).
    pub fn fail_next_replies(&self, failures: u64) {
        self.inner().transport_failures_remaining = failures;
    }

    /// Make replies answer "unknown request" (404) — the FR-23 success path.
    pub fn reply_unknown(&self) {
        self.inner().reply_404 = true;
    }
}

impl Default for FakeOpenCode {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl OpenCodePort for FakeOpenCode {
    async fn probe(&self) -> Result<ServerFacts, OcError> {
        self.inner().calls.push(Call::Probe);
        if self.inner().unreachable {
            return Err(OcError::Transport("server down".to_owned()));
        }
        Ok(ServerFacts {
            version: "2.0.24".to_owned(),
            urls: vec!["http://127.0.0.1:8080".to_owned()],
        })
    }

    async fn list_sessions(&self, root_only: bool) -> Result<Vec<PortSession>, OcError> {
        self.inner().calls.push(Call::ListSessions(root_only));
        let sessions = self.inner().sessions.clone();
        if root_only {
            return Ok(sessions
                .into_iter()
                .filter(|session| session.parent_id.is_none())
                .collect());
        }
        Ok(sessions)
    }

    async fn activity(&self, session_id: &str) -> Result<Activity, OcError> {
        self.inner()
            .calls
            .push(Call::Activity(session_id.to_owned()));
        Ok(self
            .inner()
            .activities
            .get(session_id)
            .copied()
            .unwrap_or(Activity::Idle))
    }

    async fn create_session(
        &self,
        directory: &Path,
        title: Option<&str>,
        _permissions: Option<&[PermissionRule]>,
    ) -> Result<SessionId, OcError> {
        let dir = directory.display().to_string();
        self.inner()
            .calls
            .push(Call::CreateSession(dir.clone(), title.map(str::to_owned)));
        let session = format!("ses_fake_{}", self.inner().sessions_created.len() + 1);
        self.inner().sessions_created.push(session.clone());
        Ok(session)
    }

    async fn send_prompt(&self, session_id: &str, text: &str) -> Result<PromptAdmission, OcError> {
        self.inner()
            .calls
            .push(Call::SendPrompt(session_id.to_owned(), text.to_owned()));
        Ok(PromptAdmission {
            id: "msg_fake".to_owned(),
            session_id: session_id.to_owned(),
            delivery: Some("steer".to_owned()),
        })
    }

    async fn last_assistant(&self, session_id: &str) -> Result<Option<AssistantSnapshot>, OcError> {
        self.inner()
            .calls
            .push(Call::LastAssistant(session_id.to_owned()));
        Ok(self.inner().snapshots.get(session_id).cloned())
    }

    async fn interrupt(&self, session_id: &str) -> Result<(), OcError> {
        self.inner()
            .calls
            .push(Call::Interrupt(session_id.to_owned()));
        Ok(())
    }

    async fn pending_gates(&self) -> Result<Vec<GateRequest>, OcError> {
        self.inner().calls.push(Call::PendingGates);
        Ok(self.inner().gates.clone())
    }

    async fn reply_gate(
        &self,
        session_id: &str,
        gate_id: &str,
        decision: GateDecision,
    ) -> Result<(), OcError> {
        self.inner().calls.push(Call::ReplyGate(
            session_id.to_owned(),
            gate_id.to_owned(),
            decision,
        ));
        self.fail_or_404()
    }

    async fn reply_form(
        &self,
        session_id: &str,
        form_id: &str,
        answer: serde_json::Value,
    ) -> Result<(), OcError> {
        self.inner().calls.push(Call::ReplyForm(
            session_id.to_owned(),
            form_id.to_owned(),
            answer,
        ));
        self.fail_or_404()
    }
}

impl FakeOpenCode {
    fn fail_or_404(&self) -> Result<(), OcError> {
        let mut inner = self.inner();
        if inner.transport_failures_remaining > 0 {
            inner.transport_failures_remaining -= 1;
            return Err(OcError::Transport("flaky transport".to_owned()));
        }
        if inner.reply_404 {
            return Err(OcError::Api {
                status: 404,
                message: "unknown request".to_owned(),
            });
        }
        Ok(())
    }
}

/// Fake git port backed by a real repo directory (honest behavior for the
/// e2e) with call recording.
#[derive(Clone)]
pub struct FakeGit {
    repo: Option<PathBuf>,
    fake_adapter: ocbot::adapters::git::GitAdapter,
    calls: Arc<Mutex<Vec<Call>>>,
}

impl FakeGit {
    /// A fake that answers clean/no-repo without touching git.
    pub fn inert() -> Self {
        Self::with_repo(None)
    }

    /// A fake backed by a real git repository.
    pub fn with_repo(repo: Option<PathBuf>) -> Self {
        Self {
            repo,
            fake_adapter: ocbot::adapters::git::GitAdapter::new(),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The recorded git calls.
    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().expect("lock").clone()
    }
}

#[async_trait]
impl GitPort for FakeGit {
    async fn is_repo(&self, directory: &Path) -> Result<bool, GitError> {
        self.calls
            .lock()
            .expect("lock")
            .push(Call::GitIsRepo(directory.display().to_string()));
        if let Some(repo) = &self.repo {
            return self.fake_adapter.is_repo(repo).await;
        }
        Ok(false)
    }

    async fn head(&self, directory: &Path) -> Result<String, GitError> {
        self.calls
            .lock()
            .expect("lock")
            .push(Call::GitHead(directory.display().to_string()));
        let repo = self.repo.as_ref().ok_or(GitError::Spawn {
            kind: "head",
            message: "no repo".to_owned(),
        })?;
        self.fake_adapter.head(repo).await
    }

    async fn branch(&self, directory: &Path) -> Result<String, GitError> {
        self.calls
            .lock()
            .expect("lock")
            .push(Call::GitBranch(directory.display().to_string()));
        let repo = self.repo.as_ref().ok_or(GitError::Spawn {
            kind: "branch",
            message: "no repo".to_owned(),
        })?;
        self.fake_adapter.branch(repo).await
    }

    async fn status_porcelain(&self, directory: &Path) -> Result<String, GitError> {
        self.calls
            .lock()
            .expect("lock")
            .push(Call::GitStatus(directory.display().to_string()));
        if let Some(repo) = &self.repo {
            return self.fake_adapter.status_porcelain(repo).await;
        }
        Ok(String::new())
    }

    async fn diff_stat(&self, directory: &Path, base: &str) -> Result<String, GitError> {
        self.calls.lock().expect("lock").push(Call::GitDiffStat(
            directory.display().to_string(),
            base.to_owned(),
        ));
        let repo = self.repo.as_ref().ok_or(GitError::Spawn {
            kind: "diff-stat",
            message: "no repo".to_owned(),
        })?;
        self.fake_adapter.diff_stat(repo, base).await
    }

    async fn log(
        &self,
        directory: &Path,
        base: Option<&str>,
    ) -> Result<Vec<ocbot_core::ports::CommitRow>, GitError> {
        self.calls.lock().expect("lock").push(Call::GitLog(
            directory.display().to_string(),
            base.map(str::to_owned),
        ));
        let repo = self.repo.as_ref().ok_or(GitError::Spawn {
            kind: "log",
            message: "no repo".to_owned(),
        })?;
        self.fake_adapter.log(repo, base).await
    }

    async fn is_ancestor(&self, directory: &Path, sha: &str, of: &str) -> Result<bool, GitError> {
        self.calls.lock().expect("lock").push(Call::GitIsAncestor(
            directory.display().to_string(),
            sha.to_owned(),
            of.to_owned(),
        ));
        let repo = self.repo.as_ref().ok_or(GitError::Spawn {
            kind: "merge-base",
            message: "no repo".to_owned(),
        })?;
        self.fake_adapter.is_ancestor(repo, sha, of).await
    }

    async fn untracked_files(&self, directory: &Path) -> Result<Vec<String>, GitError> {
        self.calls
            .lock()
            .expect("lock")
            .push(Call::GitUntracked(directory.display().to_string()));
        match &self.repo {
            Some(repo) => self.fake_adapter.untracked_files(repo).await,
            None => Ok(Vec::new()),
        }
    }

    async fn commit_all(
        &self,
        directory: &Path,
        message_path: &Path,
        no_verify: bool,
    ) -> Result<Result<String, HookFailure>, GitError> {
        self.calls
            .lock()
            .expect("lock")
            .push(Call::GitCommit(directory.display().to_string(), no_verify));
        let repo = self.repo.as_ref().ok_or(GitError::Spawn {
            kind: "commit",
            message: "no repo".to_owned(),
        })?;
        self.fake_adapter
            .commit_all(repo, message_path, no_verify)
            .await
    }

    async fn branch_exists(
        &self,
        directory: &Path,
        name: &str,
    ) -> Result<Option<String>, GitError> {
        self.calls.lock().expect("lock").push(Call::GitBranchExists(
            directory.display().to_string(),
            name.to_owned(),
        ));
        let repo = self.repo.as_ref().ok_or(GitError::Spawn {
            kind: "branch",
            message: "no repo".to_owned(),
        })?;
        self.fake_adapter.branch_exists(repo, name).await
    }

    async fn create_branch_at(
        &self,
        directory: &Path,
        name: &str,
        at: &str,
    ) -> Result<(), GitError> {
        self.calls.lock().expect("lock").push(Call::GitCreateBranch(
            directory.display().to_string(),
            name.to_owned(),
            at.to_owned(),
        ));
        let repo = self.repo.as_ref().ok_or(GitError::Spawn {
            kind: "branch",
            message: "no repo".to_owned(),
        })?;
        self.fake_adapter.create_branch_at(repo, name, at).await
    }

    async fn reset_hard(&self, directory: &Path, sha: &str) -> Result<(), GitError> {
        self.calls.lock().expect("lock").push(Call::GitReset(
            directory.display().to_string(),
            sha.to_owned(),
        ));
        let repo = self.repo.as_ref().ok_or(GitError::Spawn {
            kind: "reset",
            message: "no repo".to_owned(),
        })?;
        self.fake_adapter.reset_hard(repo, sha).await
    }
}

/// A posted message.
#[derive(Debug, Clone)]
pub struct Posted {
    /// The channel.
    pub channel: u64,
    /// The message spec.
    pub message: OutgoingMessage,
    /// The assigned (fake) message id.
    pub id: u64,
}

/// Recording fake discord surface.
#[derive(Clone, Default)]
pub struct FakeDiscord {
    inner: Arc<Mutex<FakeDiscordInner>>,
}

#[derive(Default)]
struct FakeDiscordInner {
    posts: Vec<Posted>,
    reactions: Vec<(u64, u64, char)>,
    next_id: u64,
}

impl FakeDiscord {
    /// Build an empty fake.
    pub fn new() -> Self {
        Self::default()
    }

    /// All posts in order.
    pub fn posts(&self) -> Vec<Posted> {
        self.inner.lock().expect("lock").posts.clone()
    }

    /// Reactions in order.
    pub fn reactions(&self) -> Vec<(u64, u64, char)> {
        self.inner.lock().expect("lock").reactions.clone()
    }

    /// The most recent post, if any.
    pub fn last_post(&self) -> Option<Posted> {
        self.posts().pop()
    }
}

#[async_trait]
impl DiscordPort for FakeDiscord {
    async fn post(&self, channel: u64, message: &OutgoingMessage) -> Result<u64, SendError> {
        let mut inner = self.inner.lock().expect("lock");
        let id = inner.next_id;
        inner.next_id += 1;
        inner.posts.push(Posted {
            channel,
            message: message.clone(),
            id,
        });
        Ok(id)
    }

    async fn edit(
        &self,
        _channel: u64,
        _message: u64,
        _replacement: &OutgoingMessage,
    ) -> Result<(), SendError> {
        Ok(())
    }

    async fn react(&self, channel: u64, message: u64, emoji: char) -> Result<(), SendError> {
        self.inner
            .lock()
            .expect("lock")
            .reactions
            .push((channel, message, emoji));
        Ok(())
    }
}

/// The real template files from `templates/` (they are load-time validated).
pub fn workspace_templates() -> Templates {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let fresh = std::fs::read_to_string(base.join("templates/fresh.md"))
        .expect("fresh.md must exist in the workspace");
    let resume = std::fs::read_to_string(base.join("templates/resume.md"))
        .expect("resume.md must exist in the workspace");
    Templates { fresh, resume }
}

/// Build an app context wired to the supplied ports.
pub fn context(
    store: StoreHandle,
    opencode: Arc<dyn OpenCodePort>,
    git: Arc<dyn GitPort>,
    discord: Arc<dyn DiscordPort>,
    operator: u64,
    ops_channel: Option<u64>,
) -> Arc<AppContext> {
    context_with(
        store,
        opencode,
        git,
        Arc::new(ocbot::adapters::clock::SystemClock),
        None,
        discord,
        operator,
        ops_channel,
    )
}

/// A context builder with the injected clock and optional permission ruleset
/// (the P4 fake-clock tests use this).
#[expect(
    clippy::too_many_arguments,
    reason = "test harness context builder: every parameter is a deliberate injection"
)]
pub fn context_with(
    store: StoreHandle,
    opencode: Arc<dyn OpenCodePort>,
    git: Arc<dyn GitPort>,
    clock: Arc<dyn Clock>,
    permission_ruleset: Option<Vec<PermissionRule>>,
    discord: Arc<dyn DiscordPort>,
    operator: u64,
    ops_channel: Option<u64>,
) -> Arc<AppContext> {
    Arc::new(AppContext {
        store,
        opencode,
        git,
        clock,
        permission_ruleset,
        discord,
        metrics: Arc::new(Metrics::new()),
        guard: Guard {
            allowed_user_id: operator,
            ops_channel,
        },
        templates: workspace_templates(),
    })
}

/// A scriptable clock for the re-ping policy tests (P4).
#[derive(Clone)]
pub struct FakeClock {
    now: Arc<std::sync::Mutex<i64>>,
}

impl FakeClock {
    /// Start the fake at a fixed unix timestamp.
    pub fn at(epoch_secs: i64) -> Self {
        Self {
            now: Arc::new(std::sync::Mutex::new(epoch_secs)),
        }
    }

    /// Advance the fake clock.
    pub fn advance(&self, secs: i64) {
        *self.now.lock().expect("clock lock") += secs;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> i64 {
        *self.now.lock().expect("clock lock")
    }
}

/// Raw `git status --porcelain` output (test-only diagnostic).
#[expect(
    clippy::disallowed_methods,
    reason = "test harness: drives real git directly on temp repos"
)]
pub fn porcelain(repo: &Path) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain"])
        .output()
        .expect("git status");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Seed a project row so `run.dir`'s foreign key is satisfiable.
pub async fn seed_project(store: &ocbot::adapters::store::StoreHandle, dir: &str) {
    store
        .upsert_project(dir, Some("main"), Some("a".repeat(40).as_str()))
        .await
        .expect("upsert project");
}

/// A seeded run/action pair for the git-truth tests.
#[derive(Debug, Clone, Copy)]
pub struct Seeded {
    /// The run row id.
    pub run_id: i64,
    /// The action row id.
    pub action_id: i64,
}

/// Seed an action that has run and delivered a report (state `reported`), with
/// the directory lock held by it.
#[expect(
    clippy::too_many_arguments,
    reason = "test harness seed: each parameter is a column the test needs to control"
)]
pub async fn seed_reported(
    store: &ocbot::adapters::store::StoreHandle,
    dir: &str,
    work_branch: Option<&str>,
    base_sha: Option<&str>,
    channel: u64,
    label: &str,
    prompt: &str,
    session: &str,
) -> Seeded {
    store
        .upsert_project(dir, work_branch, base_sha)
        .await
        .expect("upsert project");
    let run_id = store.insert_run(dir, "free").await.expect("run");
    let action_id = store
        .insert_action(
            run_id,
            Some(i64::try_from(channel).expect("channel fits i64")),
            label,
            prompt,
            1,
        )
        .await
        .expect("action");
    assert!(
        store
            .begin_send(action_id, dir, session, base_sha, false)
            .await
            .expect("send"),
        "begin_send takes the lock"
    );
    assert!(
        store
            .cas_state(
                action_id,
                ocbot_core::State::Running,
                ocbot_core::State::Reported,
                &[]
            )
            .await
            .expect("reported"),
        "running → reported"
    );
    Seeded { run_id, action_id }
}

/// Seed an action still in `running` (gate attribution targets running
/// actions; a blocked action must stay `running`).
pub async fn seed_running(
    store: &ocbot::adapters::store::StoreHandle,
    dir: &str,
    work_branch: Option<&str>,
    base_sha: Option<&str>,
    channel: u64,
    label: &str,
    session: &str,
) -> Seeded {
    store
        .upsert_project(dir, work_branch, base_sha)
        .await
        .expect("upsert project");
    let run_id = store.insert_run(dir, "free").await.expect("run");
    let action_id = store
        .insert_action(
            run_id,
            Some(i64::try_from(channel).expect("channel fits i64")),
            label,
            "prompt",
            1,
        )
        .await
        .expect("action");
    assert!(
        store
            .begin_send(action_id, dir, session, base_sha, false)
            .await
            .expect("send"),
        "begin_send takes the lock"
    );
    Seeded { run_id, action_id }
}

/// A permission gate with a plausible verbatim payload.
pub fn permission_gate(
    id: &str,
    session: Option<&str>,
    action: &str,
    resources: &[&str],
) -> ocbot_core::GateRequest {
    let payload = serde_json::json!({
        "id": id,
        "sessionID": session,
        "action": action,
        "resources": resources,
        "save": ["once"],
        "message": null,
        "metadata": {}
    });
    ocbot_core::GateRequest {
        id: id.to_owned(),
        session_id: session.map(str::to_owned),
        kind: ocbot_core::GateKind::Permission {
            action: action.to_owned(),
            resources: resources
                .iter()
                .map(|resource| (*resource).to_owned())
                .collect(),
            message: None,
        },
        payload,
    }
}

/// A free-text form field.
pub fn text_field(key: &str, label: Option<&str>) -> ocbot_core::FormField {
    ocbot_core::FormField {
        key: key.to_owned(),
        label: label.map(str::to_owned),
        kind: ocbot_core::FormFieldKind::Text,
    }
}

/// A single-choice form field.
pub fn choice_field(key: &str, values: &[(&str, &str)]) -> ocbot_core::FormField {
    ocbot_core::FormField {
        key: key.to_owned(),
        label: None,
        kind: ocbot_core::FormFieldKind::Choice(
            values
                .iter()
                .map(|(value, label)| ocbot_core::FormOption {
                    value: (*value).to_owned(),
                    label: (*label).to_owned(),
                })
                .collect(),
        ),
    }
}

/// A yes/no form field.
pub fn boolean_field(key: &str) -> ocbot_core::FormField {
    ocbot_core::FormField {
        key: key.to_owned(),
        label: None,
        kind: ocbot_core::FormFieldKind::Boolean,
    }
}

/// A form gate with a verbatim payload carrying the typed fields.
pub fn form_gate(
    id: &str,
    session: &str,
    title: &str,
    fields: Vec<ocbot_core::FormField>,
) -> ocbot_core::GateRequest {
    let payload = serde_json::json!({
        "id": id,
        "sessionID": session,
        "title": title,
        "metadata": {},
        "fields": fields
    });
    ocbot_core::GateRequest {
        id: id.to_owned(),
        session_id: Some(session.to_owned()),
        kind: ocbot_core::GateKind::Form {
            title: title.to_owned(),
            fields,
        },
        payload,
    }
}

/// Initialize a git repo with one commit and return its path.
#[expect(
    clippy::disallowed_methods,
    reason = "test harness: builds real git repos on temp dirs"
)]
pub fn init_repo(dir: &Path) -> std::process::Output {
    std::process::Command::new("git")
        .arg("init")
        .arg("-b")
        .arg("main")
        .arg(dir)
        .output()
        .expect("git init runs")
}

/// Write a file into the repo.
pub fn write_file(repo: &Path, name: &str, contents: &str) {
    std::fs::write(repo.join(name), contents).expect("write");
}

/// Run a raw `git -C <repo> <args>` (test-only diagnostics and setup).
#[expect(
    clippy::disallowed_methods,
    reason = "test harness: drives real git directly on temp repos"
)]
pub fn run_git(repo: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git runs")
}

/// Run a raw git command and return trimmed stdout, asserting success.
pub fn git_stdout(repo: &Path, args: &[&str]) -> String {
    let output = run_git(repo, args);
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// Spawn the AC-6 crash-harness binary with the supplied environment (see
/// `src/harness.rs`). The test watches for the checkpoint marker, then SIGKILLs
/// this child.
#[expect(
    clippy::disallowed_methods,
    reason = "test harness: spawns the crash-harness process for AC-6"
)]
pub fn spawn_harness(envs: &[(&str, String)]) -> std::process::Child {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_ocbot-crash-harness"));
    for (key, value) in envs {
        command.env(key, value);
    }
    command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn crash harness")
}

/// Append to a file in the repo (marking it modified).
pub fn append_file(repo: &Path, name: &str, contents: &str) {
    use std::io::Write as _;
    let mut handle = std::fs::OpenOptions::new()
        .append(true)
        .open(repo.join(name))
        .expect("open for append");
    handle.write_all(contents.as_bytes()).expect("append");
}

/// Commit everything in the repo, returning the new head.
#[expect(
    clippy::disallowed_methods,
    reason = "test harness: builds real git repos on temp dirs"
)]
pub fn commit_all(repo: &Path, message: &str) -> String {
    for args in [vec!["add", "-A"], vec!["commit", "-m", message]] {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(&args)
            .output()
            .expect("git runs");
        assert!(output.status.success(), "{args:?} failed");
    }
    let head = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .expect("git rev-parse");
    String::from_utf8_lossy(&head.stdout).trim().to_owned()
}
