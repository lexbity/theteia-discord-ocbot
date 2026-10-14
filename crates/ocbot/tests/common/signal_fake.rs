//! A scripted signal-cli JSON-RPC daemon on a unix socket (SPEC-004 PH-3).
//!
//! The fake *is* the pinned contract (`KNOWN_CAPS.md`): it speaks exactly the
//! documented subset — `version`, `listAccounts`, `subscribe`, `send`,
//! `sendReaction` — over newline-delimited JSON, and every scriptable
//! behavior the doctor and connection machine must survive (wrong version,
//! empty accounts, close-after-subscribe, garbage lines, hanging replies)
//! can be scripted per daemon instance.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

/// How one daemon instance behaves.
#[derive(Debug, Clone)]
pub struct Script {
    /// The `version` reply.
    pub version: String,
    /// The `listAccounts` reply.
    pub accounts: Vec<String>,
    /// Reply to `subscribe` with its ack, then close the connection.
    pub close_after_subscribe: bool,
    /// Accept connections but never answer anything (the NFR-103 budget test).
    /// The connection loop stays responsive, so raw streamed lines still get
    /// delivered while requests sit pending.
    pub hang: bool,
    /// Delay before the `version` reply (interleaving tests).
    pub version_delay_ms: u64,
    /// Replace the `version` result with this raw value (decode-path tests).
    pub version_reply: Option<serde_json::Value>,
    /// Reply to `send` with this JSON-RPC error instead of a timestamp.
    pub send_error: Option<(i64, String)>,
    /// Reply to `subscribe` with this JSON-RPC error instead of the ack.
    pub subscribe_error: Option<(i64, String)>,
    /// Replace the `sendReaction` result with this raw value (decode tests).
    pub reaction_reply: Option<serde_json::Value>,
}

impl Default for Script {
    fn default() -> Self {
        Self {
            version: "0.15.0".to_owned(),
            accounts: vec!["+15551234567".to_owned()],
            close_after_subscribe: false,
            hang: false,
            version_delay_ms: 0,
            version_reply: None,
            send_error: None,
            subscribe_error: None,
            reaction_reply: None,
        }
    }
}

/// One request the daemon received (recorded for assertions).
#[derive(Debug, Clone)]
pub struct Recorded {
    /// The JSON-RPC id the client chose.
    pub id: u64,
    /// The method name.
    pub method: String,
    /// The `params` value as sent.
    pub params: serde_json::Value,
}

struct Inner {
    script: Script,
    connections: AtomicUsize,
    /// Live connections, for streaming raw lines (envelopes, garbage).
    streamers: std::sync::Mutex<Vec<mpsc::UnboundedSender<String>>>,
    requests: std::sync::Mutex<Vec<Recorded>>,
}

/// A running fake daemon. Dropping it stops the accept loop; live connections
/// end when their sockets close.
pub struct FakeSignal {
    socket: PathBuf,
    inner: Arc<Inner>,
}

impl FakeSignal {
    /// Bind `socket` (chmod 0600 — the FR-116 hygiene the doctor demands) and
    /// start serving `script`. A leftover socket file is unlinked first, the
    /// way signal-cli (and every other unix daemon) re-binds after a crash:
    /// without it the "stale file, then the daemon returns" scenario could
    /// never be scripted.
    pub fn bind(socket: PathBuf, script: Script) -> Self {
        // Only a *socket* file is unlinked: a regular file at the path is a
        // configuration fault the doctor must report (FR-116), not hide.
        let stale = std::fs::symlink_metadata(&socket).ok().filter(|metadata| {
            use std::os::unix::fs::FileTypeExt as _;
            metadata.file_type().is_socket()
        });
        if stale.is_some() {
            std::fs::remove_file(&socket).expect("unlink the stale socket");
        }
        let listener = UnixListener::bind(&socket).expect("bind the fake signal socket");
        set_mode_0600(&socket);
        let inner = Arc::new(Inner {
            script,
            connections: AtomicUsize::new(0),
            streamers: std::sync::Mutex::new(Vec::new()),
            requests: std::sync::Mutex::new(Vec::new()),
        });
        tokio::spawn(accept_loop(listener, inner.clone()));
        Self { socket, inner }
    }

    /// The socket path to hand the client/doctor.
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// How many connections the daemon has accepted so far.
    pub fn connection_count(&self) -> usize {
        self.inner.connections.load(Ordering::Relaxed)
    }

    /// Every request received, in arrival order.
    pub fn requests(&self) -> Vec<Recorded> {
        self.inner.requests.lock().expect("lock").clone()
    }

    /// The first `subscribe` request's id (envelope lines must carry it).
    pub fn subscribe_id(&self) -> u64 {
        self.requests()
            .into_iter()
            .find(|recorded| recorded.method == "subscribe")
            .map(|recorded| recorded.id)
            .expect("a subscribe request")
    }

    /// Write one raw line to every live connection (envelopes, garbage).
    pub fn stream_line(&self, line: &str) {
        let mut streamers = self.inner.streamers.lock().expect("lock");
        streamers.retain(|sender| sender.send(line.to_owned()).is_ok());
    }

    /// Stream one envelope `result` object under `id` (the pinned shape).
    pub fn stream_envelope(&self, id: u64, result: &serde_json::Value) {
        let line = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        });
        self.stream_line(&line.to_string());
    }
}

/// The accept loop: one task per connection.
async fn accept_loop(listener: UnixListener, inner: Arc<Inner>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        inner.connections.fetch_add(1, Ordering::Relaxed);
        let (stream_tx, stream_rx) = mpsc::unbounded_channel();
        inner.streamers.lock().expect("lock").push(stream_tx);
        tokio::spawn(serve_connection(stream, inner.clone(), stream_rx));
    }
}

/// Serve one connection: answer the pinned subset, forward streamed lines.
async fn serve_connection(
    stream: tokio::net::UnixStream,
    inner: Arc<Inner>,
    mut stream_rx: mpsc::UnboundedReceiver<String>,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { return };
                if line.trim().is_empty() {
                    continue;
                }
                let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
                    continue;
                };
                if !handle_request(&inner, &value, &mut write_half).await {
                    return;
                }
            }
            raw = stream_rx.recv() => {
                let Some(raw) = raw else { return };
                let mut payload = raw;
                payload.push('\n');
                if write_half.write_all(payload.as_bytes()).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Answer one request; `false` closes the connection after the reply.
async fn handle_request(
    inner: &Arc<Inner>,
    value: &serde_json::Value,
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
) -> bool {
    let id = value
        .get("id")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let method = value
        .get("method")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_owned();
    let params = value
        .get("params")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    inner.requests.lock().expect("lock").push(Recorded {
        id,
        method: method.clone(),
        params,
    });
    if inner.script.hang {
        return true;
    }
    let mut close_after = false;
    let reply = match method.as_str() {
        "version" => {
            if inner.script.version_delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(
                    inner.script.version_delay_ms,
                ))
                .await;
            }
            match &inner.script.version_reply {
                Some(raw) => ok_reply(id, raw),
                None => ok_reply(id, &serde_json::json!(inner.script.version)),
            }
        }
        "listAccounts" => {
            let accounts = serde_json::json!(inner.script.accounts);
            ok_reply(id, &accounts)
        }
        "subscribe" => {
            close_after = inner.script.close_after_subscribe;
            match &inner.script.subscribe_error {
                Some((code, message)) => error_reply(id, *code, message),
                None => ok_reply(id, &serde_json::Value::Null),
            }
        }
        "send" => match &inner.script.send_error {
            Some((code, message)) => error_reply(id, *code, message),
            None => ok_reply(id, &serde_json::json!("1700000000000")),
        },
        "sendReaction" => match &inner.script.reaction_reply {
            Some(raw) => ok_reply(id, raw),
            None => ok_reply(id, &serde_json::json!("1700000000001")),
        },
        _ => error_reply(id, -32601, "method not found"),
    };
    let mut payload = reply.to_string();
    payload.push('\n');
    if write_half.write_all(payload.as_bytes()).await.is_err() {
        return false;
    }
    !close_after
}

fn ok_reply(id: u64, result: &serde_json::Value) -> serde_json::Value {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error_reply(id: u64, code: i64, message: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    })
}

/// The FR-116 mode on the socket file (the daemon itself cannot chmod from
/// inside a bound listener, so the harness does it).
fn set_mode_0600(socket: &Path) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
        .expect("chmod 0600 the fake signal socket");
}

/// A verbatim text-message envelope result (the pinned corpus shape).
pub fn text_envelope(sender: &str, timestamp_ms: u64, text: &str) -> serde_json::Value {
    serde_json::json!({
        "envelope": {
            "source": sender,
            "sourceNumber": sender,
            "timestamp": timestamp_ms,
            "dataMessage": {"timestamp": timestamp_ms, "message": text}
        },
        "account": "+15551234567"
    })
}
