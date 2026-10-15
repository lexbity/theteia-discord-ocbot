//! The gate lifecycle loop (spec P4 / §6.4): mirrors server-side gate *facts*
//! (permissions and forms, unified) into the DB, posts and answers gate
//! messages, re-pings unanswered gates on the injected clock, and clears rows
//! when a gate leaves the pending list. Everything is idempotent by `ext_id`
//! and the `msg_id IS NULL` dedupe.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use ocbot_core::gates::{GateDecision, GateKind, GateRequest, form_renderable};
use ocbot_core::render::{OutgoingMessage, gate_message};

use crate::adapters::store::GateRow;
use crate::app::context::AppContext;
use crate::app::error::AppError;

/// How many re-pings before the final "still blocked" post (P4: max 6).
pub const MAX_GATE_PINGS: i64 = 6;
/// The answer marker stored when a gate left the pending list without a button
/// answer — the operator resolved it elsewhere (FR-41 clear). Stored in its
/// JSON-encoded form (a permission decision is stored as a JSON string too).
pub const RESOLVED_ELSEWHERE: &str = "\"resolved-elsewhere\"";

/// The poll-and-re-ping loop task (`gate_poll_interval_ms`).
pub struct GatesLoop {
    ctx: Arc<AppContext>,
    interval: Duration,
    reping_secs: i64,
}

impl GatesLoop {
    /// Build the loop from the shared context.
    pub fn new(ctx: Arc<AppContext>, interval: Duration, reping_secs: i64) -> Self {
        Self {
            ctx,
            interval,
            reping_secs,
        }
    }

    /// Run until cancelled.
    pub async fn run_forever(&self) {
        loop {
            if let Err(error) = tick(&self.ctx, self.reping_secs).await {
                tracing::error!(error = %error, "gate loop tick failed");
            }
            tokio::time::sleep(self.interval).await;
        }
    }
}

/// One loop pass: sync facts, then re-ping due gates.
pub async fn tick(ctx: &Arc<AppContext>, reping_secs: i64) -> Result<(), AppError> {
    sync(ctx).await?;
    reping(ctx, reping_secs).await?;
    Ok(())
}

/// Mirror the server's pending-gate list into the DB and the operator's wall:
/// upsert, post new messages, set the blocked substatus, clear departed gates.
pub async fn sync(ctx: &Arc<AppContext>) -> Result<(), AppError> {
    let gates = ctx
        .opencode
        .pending_gates()
        .await
        .map_err(AppError::OpenCode)?;
    let channels = GateChannels::discover(ctx).await;
    let server_ids: HashSet<String> = gates.iter().map(|gate| gate.id.clone()).collect();

    for gate in &gates {
        handle_gate(ctx, gate, &channels).await?;
    }
    clear_left(ctx, &server_ids).await?;

    let pending = ctx.store.pending_gates().await?.len();
    ctx.metrics.set_gate_pending(pending);
    Ok(())
}

/// Upsert one gate; post its message exactly once; record the blocked
/// substatus on the attributed action (FR-24: child sessions route to roots).
async fn handle_gate(
    ctx: &Arc<AppContext>,
    gate: &GateRequest,
    channels: &GateChannels,
) -> Result<(), AppError> {
    let session = gate.session_id.clone();
    let root_session = session
        .as_deref()
        .and_then(|session| channels.root_of(session));
    let action = match root_session.as_deref().or(session.as_deref()) {
        Some(owner) => ctx.store.running_for_session(owner).await?,
        None => None,
    };

    let label = action.as_ref().map(|action| action.label.clone());
    let action_id = action.as_ref().map(|action| action.id);
    // The gate message goes wherever the action's reports go (SPEC-004 §5.4:
    // point posts follow the action's persisted surface/chat_ref). Without an
    // attributed action, fall back to the operator's ops channel (Discord).
    let destination = action_destination(ctx, action.as_ref());
    // The legacy `gate.channel_id` column only carries a Discord snowflake;
    // other surfaces re-derive their destination from the action on re-ping.
    let channel = match &destination {
        Some(ocbot_core::Destination {
            surface: ocbot_core::Surface::Discord,
            chat_ref,
        }) => chat_ref.0.parse::<i64>().ok(),
        _ => None,
    };

    let renderable = match &gate.kind {
        GateKind::Permission { .. } => true,
        GateKind::Form { fields, .. } => form_renderable(fields),
    };
    let kind = match &gate.kind {
        GateKind::Permission { .. } => "permission",
        GateKind::Form { .. } => "form",
    };
    let payload_json = serde_json::to_string(gate)
        .map_err(|error| AppError::Other(format!("gate payload: {error}")))?;

    let (rowid, inserted) = ctx
        .store
        .upsert_gate(
            &gate.id,
            action_id,
            session.as_deref(),
            root_session.as_deref(),
            kind,
            &payload_json,
            renderable,
            channel,
            ctx.clock.now(),
        )
        .await?;
    if inserted {
        let tool = match &gate.kind {
            GateKind::Permission { action, .. } => action.as_str(),
            GateKind::Form { .. } => "form",
        };
        ctx.metrics.inc_gate_request(kind, tool);
    }

    if let Some(action_id) = action_id {
        ctx.store.set_action_blocked(action_id, rowid).await?;
    }

    let row = ctx
        .store
        .gate(rowid)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("gate {rowid}")))?;
    if row.msg_id.is_none() {
        let message = gate_message(
            label.as_deref(),
            gate,
            rowid,
            ctx.guard.discord_operator().unwrap_or(0),
        );
        match &destination {
            Some(destination) => {
                let receipt = ctx.chat.post(destination, &message).await?;
                ctx.store
                    .set_gate_msg(rowid, &receipt.msg_id.0, channel)
                    .await?;
                tracing::info!(gate = %gate.id, rowid, "gate message posted");
            }
            None => {
                tracing::warn!(gate = %gate.id, "no channel for the gate message; tracked only");
            }
        }
    }
    Ok(())
}

/// The destination a gate message goes to: the attributed action's own surface
/// (SPEC-004 §5.4), else the operator's ops channel on Discord.
fn action_destination(
    ctx: &Arc<AppContext>,
    action: Option<&crate::adapters::store::ActionRow>,
) -> Option<ocbot_core::Destination> {
    if let Some(action) = action
        && let Ok(destination) = crate::app::handlers::support::destination_for(action)
    {
        return Some(destination);
    }
    ctx.guard
        .inbox()
        .or(ctx.guard.ops_channel)
        .map(ocbot_core::Destination::discord)
}

/// A gate that left the server's pending list is resolved elsewhere: mark the
/// still-pending row answered and clear the action's blocked substatus (FR-41).
async fn clear_left(ctx: &Arc<AppContext>, server_ids: &HashSet<String>) -> Result<(), AppError> {
    for row in ctx.store.pending_gates().await? {
        let ext = row.ext_id.as_deref().unwrap_or_default();
        if server_ids.contains(ext) {
            continue;
        }
        if ctx
            .store
            .cas_gate(row.id, "pending", "answered", Some(RESOLVED_ELSEWHERE))
            .await?
        {
            tracing::info!(
                gate = row.id,
                "gate left the pending list; resolved elsewhere"
            );
        }
        if let Some(action_id) = row.action_id {
            ctx.store.clear_action_blocked(action_id, row.id).await?;
        }
    }
    Ok(())
}

/// Re-ping unanswered gates on the injected clock (P4): every `reping_secs`,
/// max [`MAX_GATE_PINGS`], then a final "still blocked" post.
pub async fn reping(ctx: &Arc<AppContext>, reping_secs: i64) -> Result<(), AppError> {
    for row in ctx.store.pending_gates().await? {
        if row.pings >= MAX_GATE_PINGS {
            continue;
        }
        let last = row.last_ping_at.unwrap_or(0);
        let now = ctx.clock.now();
        if now.saturating_sub(last) < reping_secs {
            continue;
        }
        let pings = ctx.store.bump_gate_pings(row.id, now).await?;
        let label = gate_label(ctx, &row).await?;
        let content = if pings >= MAX_GATE_PINGS {
            format!(
                "🔒 still blocked — {label}\n(answer it in the TUI, or the session stays stuck)"
            )
        } else {
            format!("⏳ still waiting: {label}")
        };
        let action = match row.action_id {
            Some(action_id) => ctx.store.action(action_id).await?,
            None => None,
        };
        if let Some(destination) = action_destination(ctx, action.as_ref()) {
            let message = OutgoingMessage {
                content,
                attachment: None,
                components: Vec::new(),
                mention_allowlist: ctx.guard.discord_operator().into_iter().collect(),
            };
            ctx.chat.post(&destination, &message).await?;
            tracing::info!(gate = row.id, pings, "gate re-ping posted");
        }
    }
    Ok(())
}

/// Re-POST locally-answered gates that are still pending on the server (the
/// §6.4 reconcile-table row; idempotent per FR-23, once per boot reconcile).
pub async fn repost_answered(ctx: &Arc<AppContext>) -> Result<(), AppError> {
    let gates = ctx
        .opencode
        .pending_gates()
        .await
        .map_err(AppError::OpenCode)?;
    let pending: HashSet<&str> = gates.iter().map(|gate| gate.id.as_str()).collect();
    for row in ctx.store.answered_gates().await? {
        let Some(ext) = row.ext_id.as_deref() else {
            continue;
        };
        if !pending.contains(ext) {
            continue; // the remote resolved it; nothing to re-send
        }
        if row.answer.as_deref() == Some(RESOLVED_ELSEWHERE) {
            continue; // answered elsewhere; there is no reply to re-send
        }
        if let Err(error) = post_answer(ctx, &row).await {
            tracing::warn!(gate = row.id, error = %error, "re-POST of a gate answer failed");
        }
    }
    Ok(())
}

/// Post the recorded answer to the server. A 404 ("unknown request") means the
/// server already resolved it — success (FR-23/S-3). Transport errors are
/// retried by the caller-provided policy in the handler (see `handlers::gate`).
pub(crate) async fn post_answer(ctx: &Arc<AppContext>, row: &GateRow) -> Result<(), AppError> {
    let session = row
        .session_id
        .clone()
        .ok_or_else(|| AppError::Other(format!("gate {} has no session", row.id)))?;
    let ext = row
        .ext_id
        .clone()
        .ok_or_else(|| AppError::Other(format!("gate {} has no ext id", row.id)))?;
    let answer_json = row
        .answer
        .clone()
        .ok_or_else(|| AppError::Other(format!("gate {} has no answer", row.id)))?;
    let answer: serde_json::Value = serde_json::from_str(&answer_json)
        .map_err(|error| AppError::Other(format!("gate answer decode: {error}")))?;

    let outcome = match &answer {
        serde_json::Value::String(text) => match text.as_str() {
            "once" => ctx
                .opencode
                .reply_gate(&session, &ext, GateDecision::Once)
                .await
                .map_err(AppError::OpenCode),
            "always" => ctx
                .opencode
                .reply_gate(&session, &ext, GateDecision::Always)
                .await
                .map_err(AppError::OpenCode),
            "reject" => ctx
                .opencode
                .reply_gate(&session, &ext, GateDecision::Reject)
                .await
                .map_err(AppError::OpenCode),
            _ => {
                return Err(AppError::Other(format!(
                    "unexpected permission decision {text:?}"
                )));
            }
        },
        serde_json::Value::Object(_) => ctx
            .opencode
            .reply_form(&session, &ext, answer)
            .await
            .map_err(AppError::OpenCode),
        _ => {
            return Err(AppError::Other(format!(
                "unexpected gate answer shape {answer}"
            )));
        }
    };

    match outcome {
        // A remote 404 ("unknown request") means the server already resolved
        // it — that is exactly the success we were trying to produce (FR-23).
        Ok(()) | Err(AppError::OpenCode(ocbot_core::OcError::Api { status: 404, .. })) => Ok(()),
        Err(error) => Err(error),
    }
}

/// The gate's display label (action label, else the session id).
async fn gate_label(ctx: &Arc<AppContext>, row: &GateRow) -> Result<String, AppError> {
    match row.action_id {
        Some(action_id) => match ctx.store.action(action_id).await? {
            Some(action) => Ok(action.label),
            None => Ok(row.session_id.clone().unwrap_or_else(|| "?".to_owned())),
        },
        None => Ok(row.session_id.clone().unwrap_or_else(|| "?".to_owned())),
    }
}

/// A session→parent index used to attribute a child-session gate to its root
/// action (FR-24). Shared with the event dispatcher, whose child-session
/// wakes resolve through the same parent chain (SPEC-003 §4.5).
pub(crate) struct GateChannels {
    parents: HashMap<String, Option<String>>,
}

impl GateChannels {
    /// Fetch all sessions and index the parent links.
    pub(crate) async fn discover(ctx: &Arc<AppContext>) -> Self {
        let mut parents = HashMap::new();
        match ctx.opencode.list_sessions(false).await {
            Ok(sessions) => {
                for session in sessions {
                    parents.insert(session.id.clone(), session.parent_id.clone());
                }
            }
            Err(error) => {
                tracing::warn!(error = %error, "gate attribution: session list failed");
            }
        }
        Self { parents }
    }

    /// The root session id of `session` (itself when a root).
    pub(crate) fn root_of(&self, session_id: &str) -> Option<String> {
        let mut current = session_id.to_owned();
        let mut hops = 0;
        while let Some(parent) = self.parents.get(&current).and_then(Clone::clone) {
            if hops >= 16 {
                return None; // sanity bound: never walk a parent cycle
            }
            hops += 1;
            current = parent;
        }
        Some(current)
    }
}
