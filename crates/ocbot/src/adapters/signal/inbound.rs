//! The Signal inbound pump (SPEC-004 PH-4 §5.5/§5.6): envelopes → dedupe →
//! allowlist → rate → group reject → pending flow → quote adopt → grammar →
//! the same guarded router Discord uses. The router and every handler stay
//! surface-blind; this module owns only the text→interaction translation and
//! the text-form replacement for modals (D-06).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use ocbot_core::grammar::{self, DecisionVerb, Intent};
use ocbot_core::render::help_text;
use ocbot_core::{ActorId, Destination, MsgId, OutgoingMessage, PostReceipt, Surface};

use crate::adapters::store::StoreHandle;
use crate::app::context::AppContext;
use crate::app::handlers::{adopt, files, ls, prompt, run};
use crate::app::interaction::{AckPlan, InteractionSource, SourceKind, TextInput};
use crate::app::metrics::Metrics;
use crate::app::router::{self, PlanOutcome};
use crate::config::RateLimit;

use super::adapter::SignalAdapter;
use super::envelope::{Attachment, Envelope, Quote, Reaction};

/// The dedupe cache bound (SPEC-004 D-11.5: 4096 entries).
const SEEN_CAPACITY: usize = 4096;
/// The pending-flow time-to-live (D-06: 10 minutes).
const FLOW_TTL_SECS: i64 = 600;

/// One required modal input to ask for, in the text form (D-04/D-06).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AskInput {
    id: String,
    label: String,
    value: Option<String>,
}

/// The persisted draft of a generic modal flow.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ModalDraft {
    custom_id: String,
    #[serde(default)]
    ask: Vec<AskInput>,
    #[serde(default)]
    defaults: Vec<(String, String)>,
    #[serde(default)]
    collected: Vec<(String, String)>,
}

/// The persisted draft of the run flow.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RunDraft {
    #[serde(default)]
    dir: String,
}

/// The bounded `(sender, timestamp)` seen-cache (D-11.5).
struct SeenCache {
    set: std::collections::HashSet<(String, u64)>,
    order: VecDeque<(String, u64)>,
}

impl SeenCache {
    fn new() -> Self {
        Self {
            set: std::collections::HashSet::new(),
            order: VecDeque::new(),
        }
    }

    /// Record a key; `false` when it was already seen (a redelivery).
    fn insert(&mut self, key: (String, u64)) -> bool {
        if self.set.contains(&key) {
            return false;
        }
        self.order.push_back(key.clone());
        self.set.insert(key);
        while self.order.len() > SEEN_CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }
}

/// A per-sender token bucket (D-11.4).
struct TokenBucket {
    capacity: f64,
    tokens: f64,
    refill_per_sec: f64,
    last: Instant,
}

impl TokenBucket {
    fn new(limit: RateLimit) -> Self {
        Self {
            capacity: f64::from(limit.capacity),
            tokens: f64::from(limit.capacity),
            refill_per_sec: limit.refill_per_sec.max(0.0),
            last: Instant::now(),
        }
    }

    fn try_take(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// The inbound half of the Signal surface.
pub struct SignalInbound {
    ctx: Arc<AppContext>,
    adapter: Arc<SignalAdapter>,
    operator: String,
    store: StoreHandle,
    metrics: Arc<Metrics>,
    seen: Mutex<SeenCache>,
    bucket: Mutex<TokenBucket>,
}

impl SignalInbound {
    /// Build the pump over the app context and the surface's adapter.
    pub fn new(
        ctx: Arc<AppContext>,
        adapter: Arc<SignalAdapter>,
        operator: impl Into<String>,
        limit: RateLimit,
    ) -> Self {
        let store = ctx.store.clone();
        let metrics = ctx.metrics.clone();
        Self {
            ctx,
            adapter,
            operator: operator.into(),
            store,
            metrics,
            seen: Mutex::new(SeenCache::new()),
            bucket: Mutex::new(TokenBucket::new(limit)),
        }
    }

    /// Drain the envelope receiver forever, dispatching one task per envelope
    /// (the router is the concurrency arbiter, exactly as on Discord).
    pub async fn run(self: Arc<Self>, mut envelopes: tokio::sync::mpsc::Receiver<Envelope>) {
        while let Some(envelope) = envelopes.recv().await {
            let this = self.clone();
            tokio::spawn(async move {
                this.handle(envelope).await;
            });
        }
    }

    /// The actor id for the operator on this surface.
    fn actor(&self) -> String {
        ActorId::from_parts(Surface::Signal, &self.operator).0
    }

    /// The destination of every reply on this surface.
    fn destination(&self) -> Destination {
        self.adapter.destination()
    }

    /// Build an interaction source on this surface.
    fn source(&self, kind: SourceKind) -> InteractionSource {
        InteractionSource {
            kind,
            user_id: self.actor(),
            destination: self.destination(),
            guild_id: None,
            message_id: None,
            is_dm: true,
        }
    }

    /// Build a plain notice message.
    fn notice(content: impl Into<String>) -> OutgoingMessage {
        OutgoingMessage {
            content: content.into(),
            attachment: None,
            components: Vec::new(),
            mention_allowlist: Vec::new(),
        }
    }

    /// Post a plain notice (best-effort; a send failure is logged, never
    /// propagated — the inbound loop must keep draining). Posts ride the
    /// context's chat port so instrumentation and health apply.
    async fn post_notice(&self, content: impl Into<String>) {
        if let Err(error) = self
            .ctx
            .chat
            .post(&self.destination(), &Self::notice(content))
            .await
        {
            tracing::warn!(error = %error, "signal notice post failed");
        }
    }

    /// Process one envelope (SPEC-004 §5.5).
    async fn handle(&self, envelope: Envelope) {
        // 1. Redelivery dedupe (D-11.5).
        let key = (envelope.sender.clone(), envelope.timestamp_ms);
        if !self
            .seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key)
        {
            self.metrics.inc_signal_envelope_duplicates();
            return;
        }
        // 2. Allowlist by silence (D-11.1).
        if envelope.sender != self.operator {
            self.metrics.inc_surface_denied(Surface::Signal);
            let _ = self
                .store
                .audit(&self.actor(), "denied_surface", None, "not-allowlisted")
                .await;
            return;
        }
        // 3. Group conversations are rejected (FR-104).
        if envelope.is_group {
            self.metrics.inc_surface_denied(Surface::Signal);
            let _ = self
                .store
                .audit(&self.actor(), "denied_surface", None, "group")
                .await;
            return;
        }
        // 4. Rate bucket (D-11.4).
        if !self
            .bucket
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .try_take()
        {
            self.metrics.inc_surface_rate_dropped(Surface::Signal);
            return;
        }

        // 5. Reactions resolve through the component map.
        if let Some(reaction) = envelope.reaction.clone() {
            self.handle_reaction(&reaction).await;
            return;
        }

        // 6. An open pending flow consumes the next text (D-06).
        if let Some(text) = envelope.text.as_deref()
            && let Some(row) = self.take_pending().await
        {
            self.handle_pending(row, text).await;
            return;
        }

        // 7. Attachments are persisted (FR-36/FR-118).
        for attachment in &envelope.attachments {
            self.ingest_attachment(attachment).await;
        }

        // 8. A quote adopts the quoted session (FR-108).
        if let Some(quote) = envelope.quote.clone() {
            if let Some(text) = envelope.text.as_deref() {
                self.handle_quote(&quote, text).await;
            }
            return;
        }

        // 9. Otherwise the text is a command.
        if let Some(text) = envelope.text.as_deref() {
            self.handle_text(text).await;
        }
    }

    /// Resolve a reaction to a component interaction (D-05 tier 1).
    async fn handle_reaction(&self, reaction: &Reaction) {
        if reaction.remove {
            return; // removals are ignored (D-05)
        }
        let emoji = ocbot_core::render::normalize_emoji(&reaction.emoji);
        let target = reaction.target_timestamp_ms.to_string();
        let lookup = self
            .store
            .lookup_component_map(Surface::Signal.as_str(), &target, &emoji)
            .await;
        match lookup {
            Ok(Some(custom_id)) => {
                self.dispatch(self.source(SourceKind::Component { custom_id }))
                    .await;
            }
            Ok(None) => {
                self.post_notice("that reaction is not mapped to an action (send `help`)")
                    .await;
            }
            Err(error) => tracing::warn!(error = %error, "component_map lookup failed"),
        }
    }

    /// Adopt the session behind a quoted report (FR-108).
    async fn handle_quote(&self, quote: &Quote, text: &str) {
        let target = quote.target_timestamp_ms.to_string();
        match self
            .store
            .lookup_msg_map(Surface::Signal.as_str(), &target)
            .await
        {
            Ok(Some((session, _))) => {
                if let Err(error) = adopt::adopt(
                    &self.ctx,
                    &session,
                    text,
                    &self.destination(),
                    &self.actor(),
                )
                .await
                {
                    self.post_notice(format!("adopt failed: {error}")).await;
                }
            }
            Ok(None) => {}
            Err(error) => tracing::warn!(error = %error, "msg_map lookup failed"),
        }
    }

    /// Persist one inbound attachment through the FR-36 path (FR-118).
    async fn ingest_attachment(&self, attachment: &Attachment) {
        let Some(path) = attachment.local_path.as_deref() else {
            return;
        };
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(path, error = %error, "signal attachment read failed");
                return;
            }
        };
        let filename = attachment
            .filename
            .clone()
            .unwrap_or_else(|| "attachment".to_owned());
        if let Err(error) = files::ingest(&self.ctx, &self.actor(), &filename, &bytes).await {
            tracing::warn!(error = %error, "signal attachment ingest failed");
        }
    }

    /// Parse a command line and act on it.
    async fn handle_text(&self, text: &str) {
        match grammar::parse(text) {
            Ok(intent) => self.run_intent(intent).await,
            Err(error) => {
                // Unknown verbs get the command table (operator only — the
                // allowlist already filtered strangers out).
                self.post_notice(format!("{error}\n\n{}", help_text()))
                    .await;
            }
        }
    }

    /// Translate one intent into a router interaction (or a pending flow).
    async fn run_intent(&self, intent: Intent) {
        match intent {
            Intent::Run { path, prompt } => self.begin_run(path, prompt).await,
            Intent::Prompt { text } => match text {
                Some(text) => {
                    let directory = self.ui_cwd().await;
                    self.submit_modal(
                        prompt::PROMPT_MODAL,
                        vec![
                            ("directory".to_owned(), directory),
                            ("prompt".to_owned(), text),
                        ],
                    )
                    .await;
                }
                None => self.open_pending("prompt", "await_text", "{}").await,
            },
            Intent::Status => self.slash("status").await,
            Intent::Abort => self.slash("abort").await,
            Intent::Release => self.slash("release").await,
            Intent::Mute => self.slash("mute").await,
            Intent::Reset => self.slash("reset").await,
            Intent::ResetGo { sha } => {
                self.component(format!("v1:resetgo:{sha}")).await;
            }
            Intent::Ls { path, page } => match (path, page) {
                (Some(path), _) => {
                    self.submit_modal(ls::CD_MODAL, vec![("path".to_owned(), path)])
                        .await;
                }
                (None, Some(page)) => self.component(format!("v1:ls:p:{page}")).await,
                (None, None) => self.slash("ls").await,
            },
            Intent::Cd { path } => match path {
                Some(path) => {
                    self.submit_modal(ls::CD_MODAL, vec![("path".to_owned(), path)])
                        .await;
                }
                None => self.open_pending("cd", "await_path", "{}").await,
            },
            Intent::History { page } => match page {
                Some(page) => self.component(format!("v1:page:history:{page}")).await,
                None => self.slash("history").await,
            },
            Intent::Send { label } => {
                self.staged_component(label, "v1:send:").await;
            }
            Intent::Edit { label } => {
                self.staged_component(label, "v1:edit:").await;
            }
            Intent::Pick { index } => {
                self.pick(index).await;
            }
            Intent::Decision { verb, label } => self.decide(verb, label).await,
            Intent::Gate { label, option } => self.gate(&label, &option).await,
            Intent::Help => self.post_notice(help_text()).await,
        }
    }

    /// A slash-command interaction.
    async fn slash(&self, name: &str) {
        self.dispatch(self.source(SourceKind::SlashCommand {
            name: name.to_owned(),
        }))
        .await;
    }

    /// A component interaction.
    async fn component(&self, custom_id: String) {
        self.dispatch(self.source(SourceKind::Component { custom_id }))
            .await;
    }

    /// Resolve the latest staged action (or the label-qualified one) and build
    /// a component interaction with `prefix`.
    async fn staged_component(&self, label: Option<String>, prefix: &str) {
        let action = self.resolve(Some(StateSet::Staged), label).await;
        match action {
            Some(action) => self.component(format!("{prefix}{}", action.id)).await,
            None => {
                self.post_notice("no staged action in this chat").await;
            }
        }
    }

    /// `pick <n>`: the plan picker's nth step (1-based for the operator).
    async fn pick(&self, index: usize) {
        let action = self.resolve(Some(StateSet::Staged), None).await;
        match action {
            Some(action) => {
                let index = index.saturating_sub(1);
                self.component(format!("v1:plan:{}:{index}", action.id))
                    .await;
            }
            None => self.post_notice("no staged plan action in this chat").await,
        }
    }

    /// A decision verb, targeting the latest reported action (or the
    /// label-qualified one).
    async fn decide(&self, verb: DecisionVerb, label: Option<String>) {
        let set = match verb {
            DecisionVerb::Accept => StateSet::Reported,
            _ => StateSet::Decidable,
        };
        let Some(action) = self.resolve(Some(set), label).await else {
            self.post_notice("no decidable action in this chat").await;
            return;
        };
        // Risk mitigation (§8): echo the target before the CAS commit.
        self.post_notice(format!("{} `{}`…", verb.as_str(), action.label))
            .await;
        self.component(format!("{}{}", verb.custom_id_prefix(), action.id))
            .await;
    }

    /// Answer a permission gate (FR-15/P4) by label.
    async fn gate(&self, label: &str, option: &str) {
        let Some(action) = self
            .store
            .action_by_label(
                Surface::Signal.as_str(),
                &self.operator,
                label,
                &[ocbot_core::State::Running],
            )
            .await
            .ok()
            .flatten()
        else {
            self.post_notice("no blocked action with that label").await;
            return;
        };
        let gates = match self.store.pending_gates().await {
            Ok(gates) => gates,
            Err(error) => {
                tracing::warn!(error = %error, "pending_gates read failed");
                return;
            }
        };
        let Some(gate) = gates.iter().find(|gate| gate.action_id == Some(action.id)) else {
            self.post_notice("that action has no pending gate").await;
            return;
        };
        let custom_id = if ["once", "always", "reject"].contains(&option) {
            format!("v1:gate:{}:{option}", gate.id)
        } else {
            format!("v1:formpick:{}:{option}", gate.id)
        };
        self.component(custom_id).await;
    }

    /// Resolve an action by label, or the latest in `set`.
    async fn resolve(
        &self,
        set: Option<StateSet>,
        label: Option<String>,
    ) -> Option<crate::adapters::store::ActionRow> {
        let states = set.unwrap_or(StateSet::Staged).states();
        match label {
            Some(label) => self
                .store
                .action_by_label(Surface::Signal.as_str(), &self.operator, &label, states)
                .await
                .ok()
                .flatten(),
            None => self
                .store
                .latest_action(Surface::Signal.as_str(), &self.operator, states)
                .await
                .ok()
                .flatten(),
        }
    }

    /// The remembered cwd for this chat (empty when none).
    async fn ui_cwd(&self) -> String {
        self.store
            .ui_cwd(&self.actor())
            .await
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    // --- pending flows (D-06) -------------------------------------------------

    /// Open (replacing any open flow) a pending flow and post its first prompt.
    async fn open_pending(&self, flow: &str, step: &str, draft: &str) {
        if let Err(error) = self
            .store
            .put_pending(
                Surface::Signal.as_str(),
                &self.operator,
                flow,
                step,
                draft,
                FLOW_TTL_SECS,
                self.ctx.clock.now(),
            )
            .await
        {
            tracing::warn!(error = %error, "put_pending failed");
            return;
        }
        self.refresh_pending_gauge().await;
        let prompt = match (flow, step) {
            ("run", "await_path") => "Send the directory to run in.",
            ("run", "await_prompt") => "Send the prompt for this run.",
            ("cd", "await_path") => "Send the directory to browse.",
            ("prompt", "await_text") => "Send the prompt text.",
            _ => "Send the next value.",
        };
        self.post_notice(format!("{prompt} (or `cancel`)")).await;
    }

    /// Take the open pending flow, if any.
    async fn take_pending(&self) -> Option<crate::adapters::store::PendingInputRow> {
        let row = self
            .store
            .take_pending(Surface::Signal.as_str(), &self.operator)
            .await
            .ok()
            .flatten();
        if row.is_some() {
            self.refresh_pending_gauge().await;
        }
        row
    }

    /// Handle text supplied to an open flow.
    async fn handle_pending(&self, row: crate::adapters::store::PendingInputRow, text: &str) {
        if text.trim().eq_ignore_ascii_case("cancel") {
            self.post_notice("flow cancelled").await;
            self.refresh_pending_gauge().await;
            return;
        }
        match (row.flow.as_str(), row.step.as_str()) {
            ("run", "await_path") => {
                let draft = serde_json::to_string(&RunDraft {
                    dir: text.trim().to_owned(),
                })
                .unwrap_or_else(|_| "{}".to_owned());
                self.open_pending("run", "await_prompt", &draft).await;
            }
            ("run", "await_prompt") => {
                let draft: RunDraft = serde_json::from_str(&row.draft).unwrap_or_default();
                self.submit_run(&draft.dir, text).await;
            }
            ("modal", _) => self.advance_modal(&row, text).await,
            ("prompt", "await_text") => {
                let directory = self.ui_cwd().await;
                self.submit_modal(
                    prompt::PROMPT_MODAL,
                    vec![
                        ("directory".to_owned(), directory),
                        ("prompt".to_owned(), text.to_owned()),
                    ],
                )
                .await;
            }
            ("cd", "await_path") => {
                self.submit_modal(
                    ls::CD_MODAL,
                    vec![("path".to_owned(), text.trim().to_owned())],
                )
                .await;
            }
            _ => {
                self.post_notice("unknown open flow; it has been cancelled")
                    .await;
            }
        }
    }

    /// Advance a generic modal flow by one field.
    async fn advance_modal(&self, row: &crate::adapters::store::PendingInputRow, text: &str) {
        let mut draft: ModalDraft = serde_json::from_str(&row.draft).unwrap_or_default();
        let step = row.step.parse::<usize>().unwrap_or(0);
        let Some(current) = draft.ask.get(step).cloned() else {
            self.submit_modal(&draft.custom_id, Self::modal_values(&draft))
                .await;
            return;
        };
        let value = if text.trim().is_empty() {
            current.value.clone().unwrap_or_default()
        } else {
            text.trim().to_owned()
        };
        draft.collected.push((current.id.clone(), value));
        if step + 1 < draft.ask.len() {
            draft.collected.truncate(step + 1);
            let next = draft.ask.get(step + 1).cloned();
            let serialized = serde_json::to_string(&draft).unwrap_or_else(|_| "{}".to_owned());
            let next_step = (step + 1).to_string();
            if let Err(error) = self
                .store
                .put_pending(
                    Surface::Signal.as_str(),
                    &self.operator,
                    "modal",
                    &next_step,
                    &serialized,
                    FLOW_TTL_SECS,
                    self.ctx.clock.now(),
                )
                .await
            {
                tracing::warn!(error = %error, "put_pending (modal) failed");
            }
            self.refresh_pending_gauge().await;
            if let Some(next) = next {
                self.post_notice(Self::ask_prompt(&next, step + 1, draft.ask.len()))
                    .await;
            }
        } else {
            self.submit_modal(&draft.custom_id, Self::modal_values(&draft))
                .await;
        }
    }

    /// The collected + default values of a modal draft.
    fn modal_values(draft: &ModalDraft) -> Vec<(String, String)> {
        let mut values = draft.defaults.clone();
        for (id, value) in &draft.collected {
            if let Some(existing) = values.iter_mut().find(|(key, _)| key == id) {
                existing.1.clone_from(value);
            } else {
                values.push((id.clone(), value.clone()));
            }
        }
        values
    }

    /// The text-form prompt for one modal field.
    fn ask_prompt(input: &AskInput, index: usize, total: usize) -> String {
        let hint = match &input.value {
            Some(value) if !value.is_empty() => format!(" (empty keeps: {value})"),
            _ => String::new(),
        };
        format!(
            "{}/{} — {}: send the {}{hint}",
            index + 1,
            total,
            input.label,
            input.id
        )
    }

    // --- run flow (D-06) ------------------------------------------------------

    /// `run [path] [| prompt]`: open the run flow or start directly.
    async fn begin_run(&self, path: Option<String>, prompt: Option<String>) {
        let directory = self.ui_cwd().await;
        match (path, prompt) {
            (path, Some(prompt)) => {
                let dir = path.unwrap_or(directory);
                self.submit_run(&dir, &prompt).await;
            }
            (Some(path), None) => {
                let draft = serde_json::to_string(&RunDraft { dir: path })
                    .unwrap_or_else(|_| "{}".to_owned());
                self.open_pending("run", "await_prompt", &draft).await;
            }
            (None, None) => self.open_pending("run", "await_path", "{}").await,
        }
    }

    /// Submit the collected run: create the staged action, then send it.
    async fn submit_run(&self, directory: &str, prompt: &str) {
        let directory = if directory.trim().is_empty() {
            self.ui_cwd().await
        } else {
            directory.trim().to_owned()
        };
        let label: String = prompt
            .lines()
            .next()
            .unwrap_or("run")
            .chars()
            .take(40)
            .collect();
        // The ModalSubmit-equivalent the Discord modal would produce (FR-109).
        self.execute_modal(
            run::NEW_RUN_MODAL,
            vec![
                ("directory".to_owned(), directory),
                ("label".to_owned(), label),
                ("plan".to_owned(), String::new()),
            ],
        )
        .await;
        // Then send the staged action (the Discord "Send as-is" button).
        let action = self.resolve(Some(StateSet::Staged), None).await;
        match action {
            Some(action) => self.component(format!("v1:send:{}", action.id)).await,
            None => self.post_notice("could not stage the run").await,
        }
    }

    // --- dispatch -------------------------------------------------------------

    /// Build the modal-submit interaction source (FR-109).
    fn modal_source(&self, custom_id: &str, values: Vec<(String, String)>) -> InteractionSource {
        self.source(SourceKind::ModalSubmit {
            custom_id: custom_id.to_owned(),
            values: values
                .into_iter()
                .map(|(id, value)| TextInput { id, value })
                .collect(),
        })
    }

    /// Submit a synthesized modal and post its result.
    async fn submit_modal(&self, custom_id: &str, values: Vec<(String, String)>) {
        let source = self.modal_source(custom_id, values);
        // Boxed: a modal plan may itself open a flow that submits (the type
        // checker sees the cycle even though the runtime custom ids differ).
        Box::pin(self.dispatch(source)).await;
    }

    /// Run the modal-submit execute pass without echoing the intermediate text.
    async fn execute_modal(&self, custom_id: &str, values: Vec<(String, String)>) {
        let source = self.modal_source(custom_id, values);
        match router::guarded_plan(&self.ctx, &source).await {
            PlanOutcome::Plan(_) => match router::execute(&self.ctx, &source).await {
                Ok(_) => {}
                Err(error) => self.post_notice(format!("error: {error}")).await,
            },
            PlanOutcome::Failed(error) => self.post_notice(format!("error: {error}")).await,
            PlanOutcome::Deny => self.post_notice("not authorized").await,
            PlanOutcome::Ignore => {}
        }
    }

    /// Guard, plan, and execute one interaction; post its ephemeral text.
    async fn dispatch(&self, source: InteractionSource) {
        match router::guarded_plan(&self.ctx, &source).await {
            PlanOutcome::Deny => self.post_notice("not authorized").await,
            PlanOutcome::Ignore => {}
            PlanOutcome::Failed(error) => self.post_notice(format!("error: {error}")).await,
            PlanOutcome::Plan(AckPlan::Modal(spec)) => self.open_modal_flow(&spec).await,
            PlanOutcome::Plan(_) => match router::execute(&self.ctx, &source).await {
                Ok(Some(text)) => self.post_notice(text).await,
                Ok(None) => {}
                Err(error) => self.post_notice(format!("error: {error}")).await,
            },
        }
    }

    /// Turn a modal plan into a text form (D-04/D-06): ask each required input
    /// in order, defaulting the rest; submit immediately when nothing is left
    /// to ask.
    async fn open_modal_flow(&self, spec: &crate::app::interaction::ModalSpec) {
        let mut ask = Vec::new();
        let mut defaults = Vec::new();
        for input in &spec.inputs {
            if input.required {
                ask.push(AskInput {
                    id: input.id.clone(),
                    label: input.label.clone(),
                    value: input.value.clone(),
                });
            } else {
                defaults.push((input.id.clone(), input.value.clone().unwrap_or_default()));
            }
        }
        if ask.is_empty() {
            self.submit_modal(&spec.custom_id, defaults).await;
            return;
        }
        let draft = ModalDraft {
            custom_id: spec.custom_id.clone(),
            ask,
            defaults,
            collected: Vec::new(),
        };
        let serialized = serde_json::to_string(&draft).unwrap_or_else(|_| "{}".to_owned());
        self.post_notice(format!("{} — text form", spec.title))
            .await;
        if let Err(error) = self
            .store
            .put_pending(
                Surface::Signal.as_str(),
                &self.operator,
                "modal",
                "0",
                &serialized,
                FLOW_TTL_SECS,
                self.ctx.clock.now(),
            )
            .await
        {
            tracing::warn!(error = %error, "put_pending (modal open) failed");
            return;
        }
        self.refresh_pending_gauge().await;
        if let Some(first) = draft.ask.first() {
            self.post_notice(Self::ask_prompt(first, 0, draft.ask.len()))
                .await;
        }
    }

    /// Refresh the `ocbot_pending_flows_open` gauge from the store.
    async fn refresh_pending_gauge(&self) {
        let Ok(rows) = self.store.all_pending().await else {
            return;
        };
        let mut counts: HashMap<String, usize> = HashMap::new();
        for row in rows {
            *counts.entry(row.flow).or_insert(0) += 1;
        }
        for flow in ["run", "modal", "prompt", "cd"] {
            let count = counts.get(flow).copied().unwrap_or(0);
            self.metrics.set_pending_flows_open(flow, count);
        }
    }
}

/// The state sets a bare decision resolves against.
#[derive(Debug, Clone, Copy)]
enum StateSet {
    Staged,
    Reported,
    Decidable,
}

impl StateSet {
    fn states(self) -> &'static [ocbot_core::State] {
        match self {
            StateSet::Staged => &[ocbot_core::State::Staged],
            StateSet::Reported => &[ocbot_core::State::Reported],
            StateSet::Decidable => &[
                ocbot_core::State::Reported,
                ocbot_core::State::Errored,
                ocbot_core::State::Aborted,
            ],
        }
    }
}

/// Build the outbound [`PostReceipt`] for a Signal message id (used by tests
/// and by the adapter's own `post`).
pub fn receipt(msg_id: impl Into<String>) -> PostReceipt {
    PostReceipt {
        surface: Surface::Signal,
        msg_id: MsgId(msg_id.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::{ModalDraft, SeenCache, StateSet, TokenBucket};
    use crate::config::RateLimit;

    #[test]
    fn the_seen_cache_dedupes_and_bounds() {
        let mut cache = SeenCache::new();
        assert!(cache.insert(("+1555".to_owned(), 1)));
        assert!(!cache.insert(("+1555".to_owned(), 1)), "a redelivery");
        assert!(cache.insert(("+1555".to_owned(), 2)));
        // The bound evicts the oldest first.
        let cap = u64::try_from(super::SEEN_CAPACITY).expect("capacity fits");
        for ts in 3..=(cap + 10) {
            cache.insert(("+1555".to_owned(), ts));
        }
        assert!(cache.order.len() <= super::SEEN_CAPACITY);
    }

    #[test]
    fn the_token_bucket_refills_and_exhausts() {
        let mut bucket = TokenBucket::new(RateLimit {
            capacity: 2,
            refill_per_sec: 0.0,
        });
        assert!(bucket.try_take());
        assert!(bucket.try_take());
        assert!(!bucket.try_take(), "the bucket is empty");
        // A fast refill lets one through.
        let mut fast = TokenBucket::new(RateLimit {
            capacity: 1,
            refill_per_sec: 1_000.0,
        });
        assert!(fast.try_take());
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(fast.try_take(), "the refill replenished a token");
    }

    #[test]
    fn modal_drafts_round_trip_and_prefer_collected_values() {
        let draft = ModalDraft {
            custom_id: "v1:acceptgo:7".to_owned(),
            ask: vec![super::AskInput {
                id: "subject".to_owned(),
                label: "Commit subject".to_owned(),
                value: Some("default".to_owned()),
            }],
            defaults: vec![("body".to_owned(), String::new())],
            collected: vec![("subject".to_owned(), "typed".to_owned())],
        };
        let json = serde_json::to_string(&draft).expect("serialize");
        let back: ModalDraft = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.custom_id, draft.custom_id);
        let values = super::SignalInbound::modal_values(&back);
        assert_eq!(
            values,
            vec![
                ("body".to_owned(), String::new()),
                ("subject".to_owned(), "typed".to_owned()),
            ]
        );
    }

    #[test]
    fn ask_prompts_name_the_field_and_its_default() {
        let with_default = super::AskInput {
            id: "subject".to_owned(),
            label: "Commit subject".to_owned(),
            value: Some("keep me".to_owned()),
        };
        let prompt = super::SignalInbound::ask_prompt(&with_default, 0, 2);
        assert!(prompt.contains("1/2"));
        assert!(prompt.contains("keep me"));
        let without = super::AskInput {
            id: "prompt".to_owned(),
            label: "Prompt".to_owned(),
            value: None,
        };
        let prompt = super::SignalInbound::ask_prompt(&without, 1, 2);
        assert!(prompt.contains("2/2"));
        assert!(!prompt.contains("keeps"));
    }

    #[test]
    fn state_sets_name_the_decision_families() {
        assert_eq!(StateSet::Staged.states().len(), 1);
        assert_eq!(StateSet::Reported.states().len(), 1);
        assert_eq!(StateSet::Decidable.states().len(), 3);
    }

    #[test]
    fn receipts_carry_the_signal_surface() {
        let receipt = super::receipt("1700000000000");
        assert_eq!(receipt.surface, ocbot_core::Surface::Signal);
        assert_eq!(receipt.msg_id.0, "1700000000000");
    }
}
