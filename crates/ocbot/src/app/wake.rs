//! The wake dispatcher (SPEC-003 W4, §4.3): the doorbell rings — in shadow
//! mode. One task consumes forwarded frames through the pure [`WakePolicy`]
//! and executes its decisions: **exactly one fact read per flushed wake**
//! through the store-first dispatch table (§4.5), `touch_last_event` on busy,
//! bounded contradiction re-reads (P-26), and the untracked ⇒ one list read
//! rule (P-27). Every completion this path causes is attributed
//! [`DetectedBy::Event`]; the fallback ticks keep their fast cadences, so a
//! lost or lying hint costs nothing (P-19).
//!
//! The dispatcher owns no state beyond [`WakeState`] and the idle-class hint
//! map; there is no store write here except the stall-feed
//! `touch_last_event`.
//!
//! P-32: no frame field other than the event kind and session id is ever
//! logged — and this module logs neither.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use ocbot_core::events::{
    OcEvent, ReadOutcome, WakeInput, WakeKey, WakeOutput, WakePolicy, WakeState, classify_event,
};
use opencode_v2_client::EventFrame;

use crate::adapters::store::ActionRow;
use crate::app::completion::{DetectedBy, finalize_action};
use crate::app::context::AppContext;
use crate::app::gates_loop;
use crate::app::inbox;
use ocbot_core::ports::Activity;

/// Frame kinds whose arrival implies the session *should* be idle. A read
/// that says busy after such a hint arms a settle re-read (P-26): the
/// event-vs-state propagation may simply lag.
const IDLE_CLASS_KINDS: [&str; 2] = ["session.execution.succeeded", "session.execution.failed"];

/// Monotonic milliseconds for the policy, from its own tokio-time anchor
/// (paused time in tests drives the deadlines deterministically).
fn now_ms(anchor: tokio::time::Instant) -> i64 {
    i64::try_from(u64::try_from(anchor.elapsed().as_millis()).unwrap_or(0)).unwrap_or(0)
}

/// How long a read may take before the policy deadline logic is irrelevant;
/// bounds the dispatcher's per-read await so the channel keeps draining.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// The wake dispatcher task. See the module docs for the phase contract.
pub struct Dispatcher {
    ctx: Arc<AppContext>,
    policy: WakePolicy,
    state: WakeState,
    frame_rx: mpsc::Receiver<EventFrame>,
    /// The policy's clock zero (tokio time: paused time in tests).
    anchor: tokio::time::Instant,
    /// Per-session: the arming hint was idle-class (P-26's expectation).
    idle_hint: HashMap<String, bool>,
}

impl Dispatcher {
    /// Build a dispatcher over the supervisor's frame side-channel, with the
    /// §4.3 default policy (the `[wake]` config block lands in W5).
    pub fn new(ctx: Arc<AppContext>, frame_rx: mpsc::Receiver<EventFrame>) -> Self {
        Self {
            ctx,
            policy: WakePolicy::new(),
            state: WakeState::new(),
            frame_rx,
            anchor: tokio::time::Instant::now(),
            idle_hint: HashMap::new(),
        }
    }

    /// Spawn behind `events.enabled`; `None` (and no frame consumption) in
    /// pure-poll mode — the dispatcher cannot exist without the stream.
    pub fn spawn_if_enabled(
        ctx: Arc<AppContext>,
        events: &crate::config::EventsConfig,
        frame_rx: mpsc::Receiver<EventFrame>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if !events.enabled {
            return None;
        }
        let dispatcher = Self::new(ctx, frame_rx);
        Some(tokio::spawn(async move {
            dispatcher.run_forever().await;
        }))
    }

    /// The §4.3 loop: frames and the policy deadline race; the policy decides,
    /// the dispatcher executes.
    pub async fn run_forever(mut self) {
        loop {
            // `next_deadline` is absolute on the policy clock, whose zero is
            // this dispatcher's anchor: sleep to `anchor + ms` directly.
            let deadline = self.policy.next_deadline(&self.state).map(|ms| {
                self.anchor + Duration::from_millis(u64::try_from(ms.max(0)).unwrap_or(0))
            });
            let item = if let Some(deadline) = deadline {
                tokio::select! {
                    frame = self.frame_rx.recv() => frame.map(Some),
                    () = tokio::time::sleep_until(deadline) => Some(None),
                }
            } else {
                self.frame_rx.recv().await.map(Some)
            };
            match item {
                Some(Some(frame)) => self.on_frame(frame).await,
                Some(None) => self.on_flush().await,
                None => return, // the supervisor is gone; the stream is over
            }
        }
    }

    /// One arrived frame: classify, hint the policy, execute its output.
    async fn on_frame(&mut self, frame: EventFrame) {
        let hint = classify_event(&frame.event, frame.session_id.as_deref());
        if let OcEvent::SessionChanged { session_id } = &hint {
            let idle_class = IDLE_CLASS_KINDS.contains(&frame.event.as_str());
            self.idle_hint.insert(session_id.clone(), idle_class);
        }
        let (state, out) = self.policy.step(
            std::mem::take(&mut self.state),
            now_ms(self.anchor),
            WakeInput::Hint(hint),
        );
        self.state = state;
        self.execute(out).await;
    }

    /// The policy deadline fired: flush whatever is due.
    async fn on_flush(&mut self) {
        let (state, out) = self.policy.step(
            std::mem::take(&mut self.state),
            now_ms(self.anchor),
            WakeInput::FlushDue,
        );
        self.state = state;
        self.execute(out).await;
    }

    /// Execute one policy decision: the reads the policy issued. (A dirty
    /// mark only ever rides a *read result*, handled in
    /// [`Self::apply_side_effects`] — a `Hint` or `FlushDue` never dirties.)
    async fn execute(&mut self, out: WakeOutput) {
        for key in &out.read {
            match key {
                WakeKey::Gates => {
                    if let Err(error) = gates_loop::sync(&self.ctx).await {
                        tracing::warn!(error = %error, "wake: gate sync failed");
                    }
                }
                WakeKey::SessionList => {
                    if let Err(error) = inbox::diff_sessions(&self.ctx, DetectedBy::Event).await {
                        tracing::warn!(error = %error, "wake: session diff failed");
                    }
                }
                WakeKey::Session(id) => self.session_read(key.clone(), id).await,
            }
        }
        if out.dirty == Some(WakeKey::SessionList) {
            // P-27: an untracked session costs exactly one list read; the
            // inbox diff picks it up, deduped by `notified`.
            if let Err(error) = inbox::diff_sessions(&self.ctx, DetectedBy::Event).await {
                tracing::warn!(error = %error, "wake: dirty list read failed");
            }
        }
    }

    /// The store-first session read (§4.5): tracked ⇒ one `activity` read on
    /// the root; untracked ⇒ zero session reads, the list is marked dirty.
    async fn session_read(&mut self, key: WakeKey, session_id: &str) {
        let Some(action) = self.running_action_for(session_id).await else {
            let (state, out) = self.policy.step(
                std::mem::take(&mut self.state),
                now_ms(self.anchor),
                WakeInput::ReadDone {
                    key,
                    outcome: ReadOutcome::Untracked,
                },
            );
            self.state = state;
            self.apply_side_effects(out).await;
            return;
        };
        let idle_expected = self.idle_hint.get(session_id).copied().unwrap_or(false);
        let Some(root_session) = action.session_id.clone() else {
            // The running action never bound a session; nothing to read.
            let (state, out) = self.policy.step(
                std::mem::take(&mut self.state),
                now_ms(self.anchor),
                WakeInput::ReadDone {
                    key,
                    outcome: ReadOutcome::Untracked,
                },
            );
            self.state = state;
            self.apply_side_effects(out).await;
            return;
        };
        let read =
            tokio::time::timeout(READ_TIMEOUT, self.ctx.opencode.activity(&root_session)).await;
        let (state, out) = match read {
            Ok(Ok(Activity::Busy)) => {
                // Feed the stall watchdog, then either settle (the hint said
                // idle-class and the read contradicts it, P-26) or close.
                let _ = self
                    .ctx
                    .store
                    .touch_last_event(action.id, self.ctx.clock.now())
                    .await;
                let input = if idle_expected {
                    WakeInput::Contradicted { key }
                } else {
                    WakeInput::ReadDone {
                        key,
                        outcome: ReadOutcome::Busy,
                    }
                };
                self.policy
                    .step(std::mem::take(&mut self.state), now_ms(self.anchor), input)
            }
            Ok(Ok(Activity::Idle)) => {
                if let Err(error) =
                    finalize_action(&self.ctx, &action, false, DetectedBy::Event).await
                {
                    tracing::error!(action = action.id, error = %error, "wake: finalize failed");
                }
                self.idle_hint.remove(session_id);
                self.policy.step(
                    std::mem::take(&mut self.state),
                    now_ms(self.anchor),
                    WakeInput::ReadDone {
                        key,
                        outcome: ReadOutcome::Idle,
                    },
                )
            }
            Ok(Err(error)) => {
                tracing::warn!(session = %session_id, error = %error, "wake: activity read failed");
                self.policy.step(
                    std::mem::take(&mut self.state),
                    now_ms(self.anchor),
                    WakeInput::ReadDone {
                        key,
                        outcome: ReadOutcome::Busy,
                    },
                )
            }
            Err(_elapsed) => {
                tracing::warn!(session = %session_id, "wake: activity read timed out");
                self.policy.step(
                    std::mem::take(&mut self.state),
                    now_ms(self.anchor),
                    WakeInput::ReadDone {
                        key,
                        outcome: ReadOutcome::Busy,
                    },
                )
            }
        };
        self.state = state;
        self.apply_side_effects(out).await;
    }

    /// Apply a post-read output: at most a dirty-marked list read (a read
    /// decision can never chain another *session* read — settle re-reads are
    /// scheduled through the policy deadline, not nested here).
    async fn apply_side_effects(&mut self, out: WakeOutput) {
        if out.dirty == Some(WakeKey::SessionList)
            && let Err(error) = inbox::diff_sessions(&self.ctx, DetectedBy::Event).await
        {
            tracing::warn!(error = %error, "wake: dirty list read failed");
        }
    }

    /// Store-first lookup (P-27): a running action whose session is `id`
    /// itself, or whose *root* is (FR-24's parent chain: child sessions never
    /// complete independently). A miss costs one session-list discovery —
    /// O(1), never one read per frame.
    async fn running_action_for(&self, session_id: &str) -> Option<ActionRow> {
        if let Some(action) = self
            .ctx
            .store
            .running_for_session(session_id)
            .await
            .ok()
            .flatten()
        {
            return Some(action);
        }
        let channels = gates_loop::GateChannels::discover(&self.ctx).await;
        let root = channels.root_of(session_id)?;
        self.ctx
            .store
            .running_for_session(&root)
            .await
            .ok()
            .flatten()
    }
}
