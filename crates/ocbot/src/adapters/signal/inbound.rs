//! The Signal inbound pump (SPEC-004 PH-4 §5.5/§5.6): envelopes → dedupe →
//! allowlist → rate → group reject → pending flow → quote adopt → grammar →
//! the same guarded router Discord uses. The router and every handler stay
//! surface-blind; this module owns only the text→interaction translation and
//! the text-form replacement for modals (D-06).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use ocbot_core::grammar::{self, DecisionVerb, Intent};
use ocbot_core::render::{
    FLOW_PROMPT, FLOW_RUN, cancelled as flow_cancelled, help_text,
    unknown_flow as unknown_flow_text, with_cancel_hint,
};
use ocbot_core::{Destination, MsgId, OutgoingMessage, PostReceipt, Surface};

use crate::adapters::store::StoreHandle;
use crate::app::context::AppContext;
use crate::app::handlers::{adopt, files, prompt, run};
use crate::app::interaction::{AckPlan, InteractionSource, SourceKind, TextInput};
use crate::app::metrics::Metrics;
use crate::app::router::{self, PlanOutcome};
use crate::config::RateLimit;

use super::adapter::SignalAdapter;
use super::envelope::{Attachment, Envelope, Quote, Reaction};
use super::flows::{Advance, FlowEngine, advance as advance_flow, run_draft, run_label};

/// The dedupe cache bound (SPEC-004 D-11.5: 4096 entries).
const SEEN_CAPACITY: usize = 4096;

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
    flows: FlowEngine,
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
        let operator = operator.into();
        let store = ctx.store.clone();
        let metrics = ctx.metrics.clone();
        let flows = FlowEngine::new(ctx.clone(), adapter.clone(), operator.clone());
        Self {
            ctx,
            adapter,
            operator,
            store,
            metrics,
            flows,
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
        self.flows.actor()
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
            && let Some(row) = self.take_flow().await
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
                None => self.open_flow(FLOW_PROMPT).await,
            },
            Intent::Status => self.slash("status").await,
            Intent::Abort => self.slash("abort").await,
            Intent::Release => self.slash("release").await,
            Intent::Mute => self.slash("mute").await,
            Intent::Reset => self.slash("reset").await,
            Intent::ResetGo { sha } => {
                self.component(format!("v1:resetgo:{sha}")).await;
            }
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

    /// Handle text supplied to an open flow: the engine decides the transition
    /// ([`super::flows::advance`] is pure), this method applies it.
    async fn handle_pending(&self, row: crate::adapters::store::PendingInputRow, text: &str) {
        let cwd = self.ui_cwd().await;
        match advance_flow(&row, text, &cwd) {
            Advance::Cancelled => self.flows.close(flow_cancelled()).await,
            Advance::Unknown => self.flows.close(unknown_flow_text()).await,
            Advance::Next {
                step,
                draft,
                prompt,
            } => {
                if self.flows.persist(&row.flow, &step, &draft).await.is_some() {
                    self.post_notice(with_cancel_hint(&prompt)).await;
                }
            }
            Advance::Submit { custom_id, values } => {
                // The run flow chains the review's "Send as-is" button so a
                // text `run` reaches the agent in one exchange (D-06).
                if custom_id == run::NEW_RUN_MODAL {
                    self.submit_run(values).await;
                } else {
                    self.submit_modal(&custom_id, values).await;
                }
            }
        }
    }

    /// Open a named flow at its first step (replacing any open flow).
    async fn open_flow(&self, flow: &str) {
        self.flows.open(flow).await;
    }

    /// Open a named flow at `step` with a persisted draft.
    async fn open_flow_at(&self, flow: &str, step: &str, draft: &str) {
        self.flows.open_at(flow, step, draft).await;
    }

    /// Take the open flow, if any.
    async fn take_flow(&self) -> Option<crate::adapters::store::PendingInputRow> {
        self.flows.take().await
    }

    // --- run flow (D-06) ------------------------------------------------------

    /// `run [path] [| prompt]`: open the run flow or start directly.
    async fn begin_run(&self, path: Option<String>, prompt: Option<String>) {
        match (path, prompt) {
            (path, Some(prompt)) => {
                let directory = match path {
                    Some(path) => path,
                    None => self.ui_cwd().await,
                };
                self.submit_run(self.run_values(&directory, &prompt).await)
                    .await;
            }
            (Some(path), None) => {
                self.open_flow_at(FLOW_RUN, "await_prompt", &run_draft(&path))
                    .await;
            }
            (None, None) => self.open_flow(FLOW_RUN).await,
        }
    }

    /// The newrun modal values a text run produces (the pure engine derives the
    /// label, so every text path agrees byte-for-byte).
    async fn run_values(&self, directory: &str, prompt_text: &str) -> Vec<(String, String)> {
        let directory = if directory.trim().is_empty() {
            self.ui_cwd().await
        } else {
            directory.trim().to_owned()
        };
        vec![
            ("directory".to_owned(), directory),
            ("label".to_owned(), run_label(prompt_text)),
            ("plan".to_owned(), String::new()),
        ]
    }

    /// Submit the collected run: create the staged action, then send it.
    async fn submit_run(&self, values: Vec<(String, String)>) {
        // The ModalSubmit-equivalent the Discord modal would produce (FR-109).
        self.execute_modal(run::NEW_RUN_MODAL, values).await;
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
        if let Some(Advance::Submit { custom_id, values }) = self.flows.open_modal(spec).await {
            self.submit_modal(&custom_id, values).await;
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
    use super::super::flows::{RunDraft, run_draft, run_label};
    use super::{SeenCache, StateSet, TokenBucket};
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
    fn the_run_draft_round_trips_the_directory() {
        let draft = run_draft("/tmp/x");
        let back: RunDraft = serde_json::from_str(&draft).expect("deserialize");
        assert_eq!(back.dir, "/tmp/x");
        assert_eq!(run_label("a prompt"), "a prompt");
        assert_eq!(run_draft("/tmp/x"), r#"{"dir":"/tmp/x"}"#);
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
