//! The Signal surface's outbound side (SPEC-004 PH-4): a [`ChatPort`] over the
//! signal-cli JSON-RPC connection the machine publishes in a
//! [`ConnectionSlot`]. Rendering (the legend, §5.5), attachment staging, and
//! the D-13 edit fallback live here; the inbound half lives in [`super::inbound`].

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use ocbot_core::render::render_legend;
use ocbot_core::{ChatRef, Destination, MsgId, OutgoingMessage, PostReceipt, Surface};

use crate::adapters::store::StoreHandle;
use crate::app::context::{BroadcastOutcome, ChatPort, SendError};
use crate::app::metrics::Metrics;

use super::rpc::ConnectionSlot;

/// The minimum spacing between `↻ update` fallback posts on one surface
/// (NFR-106: at most one folded edit per ops tick — 30 s default).
const EDIT_FOLD: Duration = Duration::from_secs(30);

/// The Signal surface as an outbound [`ChatPort`].
pub struct SignalAdapter {
    operator: String,
    slot: ConnectionSlot,
    store: StoreHandle,
    metrics: Arc<Metrics>,
    /// The staging directory outgoing attachments are written to.
    staging: PathBuf,
    /// Serializes sends so a report never lands after its own confirmation
    /// (D-14: one ordered sender per surface).
    send_lock: tokio::sync::Mutex<()>,
    /// The last `↻ update` fallback post (NFR-106).
    last_edit: Mutex<Option<Instant>>,
}

impl SignalAdapter {
    /// Build the adapter over a live-connection slot.
    pub fn new(
        operator: impl Into<String>,
        slot: ConnectionSlot,
        store: StoreHandle,
        metrics: Arc<Metrics>,
        staging: PathBuf,
    ) -> Self {
        Self {
            operator: operator.into(),
            slot,
            store,
            metrics,
            staging,
            send_lock: tokio::sync::Mutex::new(()),
            last_edit: Mutex::new(None),
        }
    }

    /// The operator's destination (the only chat this surface ever posts to).
    pub fn destination(&self) -> Destination {
        Destination {
            surface: Surface::Signal,
            chat_ref: ChatRef(self.operator.clone()),
        }
    }

    /// Whether a `↻ update` fallback post is allowed now (NFR-106 fold).
    fn edit_allowed(&self) -> bool {
        let mut guard = self
            .last_edit
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        match *guard {
            Some(last) if now.duration_since(last) < EDIT_FOLD => false,
            _ => {
                *guard = Some(now);
                true
            }
        }
    }

    /// Write one attachment to a unique staging file; the caller removes it
    /// after the send. signal-cli reads the path itself, so it must exist as a
    /// real file for the duration of the call.
    fn stage_attachment(&self, spec: &ocbot_core::AttachmentSpec) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(&self.staging)?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let path = self.staging.join(format!(
            "sig-{}-{nanos}-{}",
            std::process::id(),
            ocbot_core::sanitize_name(&spec.filename)
        ));
        std::fs::write(&path, &spec.bytes)?;
        Ok(path)
    }
}

#[async_trait]
impl ChatPort for SignalAdapter {
    async fn post(
        &self,
        dest: &Destination,
        message: &OutgoingMessage,
    ) -> Result<PostReceipt, SendError> {
        if dest.surface != Surface::Signal {
            return Err(SendError {
                surface: Surface::Signal,
                message: format!("signal adapter asked to post to {dest:?}"),
            });
        }
        let recipient = dest.chat_ref.0.clone();
        let legend = render_legend(&message.components);
        let mut content = message.content.clone();
        if !legend.line.is_empty() {
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str(&legend.line);
        }

        let staged = if let Some(spec) = &message.attachment {
            Some(self.stage_attachment(spec).map_err(|error| SendError {
                surface: Surface::Signal,
                message: format!("attachment staging: {error}"),
            })?)
        } else {
            None
        };
        let paths: Vec<String> = staged
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();

        // One ordered sender per surface (D-14).
        let guard = self.send_lock.lock().await;
        let Some(rpc) = self.slot.current().await else {
            drop(guard);
            if let Some(path) = &staged {
                let _ = std::fs::remove_file(path);
            }
            return Err(SendError {
                surface: Surface::Signal,
                message: "no live signal-cli connection".to_owned(),
            });
        };
        let result = rpc
            .send_with_attachments(&recipient, &content, &paths)
            .await;
        drop(guard);
        if let Some(path) = &staged {
            let _ = std::fs::remove_file(path);
        }
        let msg_id = result.map_err(|error| SendError {
            surface: Surface::Signal,
            message: error.to_string(),
        })?;

        // Record the reaction map for this message (used by the inbound pump).
        if !legend.entries.is_empty() {
            let entries: Vec<(String, String)> = legend
                .entries
                .iter()
                .map(|entry| (entry.emoji.clone(), entry.custom_id.clone()))
                .collect();
            if let Err(error) = self
                .store
                .insert_component_map(Surface::Signal.as_str(), &msg_id, &entries)
                .await
            {
                tracing::warn!(error = %error, "component_map insert failed");
            } else if let Ok(count) = self.store.component_map_count().await {
                self.metrics
                    .set_component_map_rows(usize::try_from(count).unwrap_or(usize::MAX));
            }
        }

        Ok(PostReceipt {
            surface: Surface::Signal,
            msg_id: MsgId(msg_id),
        })
    }

    async fn edit(&self, receipt: &PostReceipt, replacement: &OutgoingMessage) {
        // D-13: the tested fallback — a new message prefixed `↻ update:` —
        // because native edit is version-dependent; folded at NFR-106's bound.
        if receipt.surface != Surface::Signal || !self.edit_allowed() {
            return;
        }
        let message = OutgoingMessage {
            content: format!("↻ update: {}", replacement.content),
            attachment: replacement.attachment.clone(),
            components: replacement.components.clone(),
            mention_allowlist: Vec::new(),
        };
        if let Err(error) = self.post(&self.destination(), &message).await {
            self.metrics
                .inc_surface_send_failure(Surface::Signal, "edit");
            tracing::warn!(error = %error, "signal update post failed");
        }
    }

    async fn react(&self, receipt: &PostReceipt, emoji: char) {
        if receipt.surface != Surface::Signal {
            return;
        }
        let Ok(target) = receipt.msg_id.0.parse::<u64>() else {
            return;
        };
        let Some(rpc) = self.slot.current().await else {
            return;
        };
        if let Err(error) = rpc
            .send_reaction(&self.operator, target, &emoji.to_string(), false)
            .await
        {
            self.metrics
                .inc_surface_send_failure(Surface::Signal, "react");
            tracing::warn!(error = %error, "signal reaction failed");
        }
    }

    async fn broadcast(&self, message: &OutgoingMessage) -> BroadcastOutcome {
        let mut outcome = BroadcastOutcome::default();
        match self.post(&self.destination(), message).await {
            Ok(receipt) => outcome.delivered.push((Surface::Signal, receipt)),
            Err(error) => outcome.failed.push((Surface::Signal, error)),
        }
        outcome
    }

    fn surfaces(&self) -> Vec<Surface> {
        vec![Surface::Signal]
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::SignalAdapter;
    use crate::adapters::store::StoreHandle;
    use crate::app::context::ChatPort;
    use crate::app::metrics::Metrics;
    use ocbot_core::{ChatRef, Destination, OutgoingMessage, Surface};

    use super::super::rpc::ConnectionSlot;

    fn adapter(staging: std::path::PathBuf) -> SignalAdapter {
        SignalAdapter::new(
            "+15551234567",
            ConnectionSlot::new(),
            StoreHandle::open_in_memory().expect("store"),
            Arc::new(Metrics::new()),
            staging,
        )
    }

    fn message(text: &str) -> OutgoingMessage {
        OutgoingMessage {
            content: text.to_owned(),
            attachment: None,
            components: Vec::new(),
            mention_allowlist: Vec::new(),
        }
    }

    #[test]
    fn the_destination_is_the_operator_chat() {
        let dir = tempfile::tempdir().expect("tmp");
        let adapter = adapter(dir.path().to_path_buf());
        let destination = adapter.destination();
        assert_eq!(destination.surface, Surface::Signal);
        assert_eq!(destination.chat_ref.0, "+15551234567");
    }

    #[test]
    fn the_edit_fold_allows_one_update_per_window() {
        let dir = tempfile::tempdir().expect("tmp");
        let adapter = adapter(dir.path().to_path_buf());
        assert!(adapter.edit_allowed(), "the first update goes");
        assert!(!adapter.edit_allowed(), "the second folds (NFR-106)");
    }

    #[tokio::test]
    async fn posting_to_a_foreign_surface_fails_loud() {
        let dir = tempfile::tempdir().expect("tmp");
        let adapter = adapter(dir.path().to_path_buf());
        let error = adapter
            .post(&Destination::discord(7), &message("hi"))
            .await
            .expect_err("a discord destination is a programming error");
        assert_eq!(error.surface, Surface::Signal);
    }

    #[tokio::test]
    async fn posting_without_a_connection_fails_loud() {
        let dir = tempfile::tempdir().expect("tmp");
        let adapter = adapter(dir.path().to_path_buf());
        let destination = Destination {
            surface: Surface::Signal,
            chat_ref: ChatRef("+15551234567".to_owned()),
        };
        let error = adapter
            .post(&destination, &message("hi"))
            .await
            .expect_err("no connection");
        assert!(error.message.contains("no live"), "{}", error.message);
    }

    #[test]
    fn attachments_are_staged_under_unique_names() {
        let dir = tempfile::tempdir().expect("tmp");
        let adapter = adapter(dir.path().to_path_buf());
        let spec = ocbot_core::AttachmentSpec {
            filename: "report.md".to_owned(),
            bytes: b"# hi\n".to_vec(),
        };
        let path = adapter.stage_attachment(&spec).expect("staged");
        assert!(path.exists());
        assert!(path.to_string_lossy().ends_with("report.md"));
        assert_eq!(std::fs::read(&path).expect("read"), b"# hi\n");
    }

    #[tokio::test]
    async fn edit_and_react_without_a_connection_are_quiet() {
        let dir = tempfile::tempdir().expect("tmp");
        let adapter = adapter(dir.path().to_path_buf());
        let receipt = ocbot_core::PostReceipt {
            surface: Surface::Signal,
            msg_id: ocbot_core::MsgId("1700000000000".to_owned()),
        };
        // No connection: edit posts nothing (best-effort), react is a no-op.
        adapter.edit(&receipt, &message("status")).await;
        adapter.react(&receipt, '✅').await;
        // A foreign receipt is ignored entirely.
        let foreign = ocbot_core::PostReceipt {
            surface: Surface::Discord,
            msg_id: ocbot_core::MsgId("1".to_owned()),
        };
        adapter.edit(&foreign, &message("status")).await;
        adapter.react(&foreign, '✅').await;
    }

    #[tokio::test]
    async fn broadcast_without_a_connection_reports_the_failure() {
        let dir = tempfile::tempdir().expect("tmp");
        let adapter = adapter(dir.path().to_path_buf());
        let outcome = adapter.broadcast(&message("announce")).await;
        assert!(outcome.delivered.is_empty());
        assert_eq!(outcome.failed.len(), 1);
        assert_eq!(adapter.surfaces(), vec![Surface::Signal]);
    }
}
