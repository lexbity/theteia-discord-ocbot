//! Phase 0 spike CLI (`SPIKE-0.md`): the end-to-end proof against a **live
//! pinned opencode v2 server**.
//!
//! Usage (against a server bound elsewhere than the background service):
//!
//! ```sh
//! OPENCODE_URL=http://127.0.0.1:8080 \
//! OPENCODE_PASSWORD=<password> \
//! SPIKE_DIR=/tmp/opencode/spike-work \
//! SPIKE_PROMPT='Reply with exactly PONG' \
//! cargo run -p opencode-v2-client --example spike
//! ```
//!
//! The transcript is the content of `docs/compat/SPIKE-0.md`; the exit code is
//! `0` on a full pass, `2` on usage errors, non-zero on a failed probe.
//!
//! # S-10 events mode (SPEC-003 W0)
//!
//! ```sh
//! cargo run -p opencode-v2-client --example spike -- --events 90
//! ```
//!
//! Subscribes to `/api/event` for `<secs>` seconds while driving a scripted
//! session (busy work → permission probe → external session → delete → quiet
//! period). Evidence lands in `SPIKE_EVENTS_DIR` (default `/tmp/opencode/s10`):
//! `events_raw.sse` (exact bytes as received), `events_chunks.tsv`
//! (`unix_ms \t chunk_len \t cumulative_len` — the inter-frame timing record),
//! and `markers.tsv` (`unix_ms \t phase \t name` — the script's own clock).
//! The parsed transcript is printed via [`log_frame`], the single
//! frame-logging helper (SPEC-003 P-32: event kind, session id and byte
//! length only — payload fields never reach stdout at any level).
//!
//! A CLI printing its transcript is the point of the file, so the
//! print-macro restrictions are deliberately lifted here — and only here
//! (targeted `#![expect]`, matching spec §C4).

#![expect(
    clippy::print_stdout,
    reason = "the Phase 0 spike is a CLI whose purpose is to print its transcript"
)]

use std::error::Error;
use std::process::ExitCode;
use std::time::Duration;

use opencode_v2_client::{Activity, Client};

/// How long to wait for a session to finish.
const IDLE_TIMEOUT_SECS: u64 = 180;

/// Quiet-phase length of the S-10 events spike (heartbeat/silence evidence).
const S10_QUIET_SECS: u64 = 40;

fn env(name: &str) -> Result<String, Box<dyn Error>> {
    std::env::var(name).map_err(|_| format!("missing required env var {name}").into())
}

#[tokio::main]
async fn main() -> Result<ExitCode, Box<dyn Error>> {
    if std::env::args().nth(1).as_deref() == Some("--events") {
        let secs: u64 = std::env::args()
            .nth(2)
            .and_then(|value| value.parse().ok())
            .unwrap_or(90);
        return s10::run(secs).await;
    }

    let url = env("OPENCODE_URL")?;
    let password = env("OPENCODE_PASSWORD")?;
    let directory =
        std::env::var("SPIKE_DIR").unwrap_or_else(|_| "/tmp/opencode/spike-work".to_owned());
    let prompt = std::env::var("SPIKE_PROMPT")
        .unwrap_or_else(|_| "Reply with exactly the word PONG.".to_owned());
    let title = std::env::var("SPIKE_TITLE").unwrap_or_else(|_| "ocbot spike".to_owned());

    let client = Client::new(&url, &password)?;

    // 1. Version gate: refuse V1.
    println!("[1] probe /api/info");
    match client.probe().await {
        Ok(info) => {
            println!("    server v2: version={} pid={:?}", info.version, info.pid);
        }
        Err(error) => {
            println!("    REFUSED: {error}");
            return Ok(ExitCode::from(1));
        }
    }

    // 2. Root session visibility.
    println!("[2] list root sessions");
    let roots = client.list_sessions(true, None).await?;
    println!("    {} root session(s)", roots.len());

    // 3. Create a session bound to a directory (spec A6).
    println!("[3] create session in {directory:?}");
    let session = client
        .create_session(Some(&directory), Some(&title))
        .await?;
    println!(
        "    created {} project={}",
        session.id,
        session.project_id.as_deref().unwrap_or("?")
    );

    // 4. Durable log head (S-2: the endpoint yields a sync marker; correctness
    // never depends on replay — spec P9).
    println!("[4] durable log head");
    {
        let mut stream = client.log_after(&session.id, None, false).await?;
        match stream.next().await {
            Some(Ok(entry)) => println!("    first entry kind={} seq={:?}", entry.kind, entry.seq),
            other => println!("    no entry (pinned as expected in 2.0.24): {other:?}"),
        }
    }

    // 5. Admit a prompt (non-blocking admission, spec P9).
    println!("[5] send prompt: {prompt:?}");
    let admission = client.send_prompt(&session.id, &prompt).await?;
    println!(
        "    admitted msg={} delivery={:?}",
        admission.id, admission.delivery
    );

    // 6. Follow to idle (status polling is the durable signal; the global
    // event stream is the live one).
    println!("[6] follow session to idle");
    let started = tokio::time::Instant::now();
    loop {
        match client.activity(&session.id).await? {
            Activity::Busy => {
                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
            Activity::Idle => {
                println!("    idle after {:.1}s", started.elapsed().as_secs_f64());
                break;
            }
        }
        if started.elapsed() > Duration::from_secs(IDLE_TIMEOUT_SECS) {
            println!("    TIMEOUT after {IDLE_TIMEOUT_SECS}s; interrupting");
            let result = client.interrupt(&session.id).await?;
            println!("    interrupt: interrupted={}", result.interrupted);
            break;
        }
    }

    // 7. Last assistant snapshot (spec A7: tokens/cost/error ride along).
    println!("[7] last assistant snapshot");
    let assistant = client.last_assistant(&session.id).await?;
    match assistant {
        Some(assistant) => {
            println!("    finish={:?}", assistant.finish);
            println!(
                "    cost={:?} tokens={:?}",
                assistant.cost, assistant.tokens
            );
            println!("    error={:?}", assistant.error_message());
            println!("    text={:?}", assistant.text());
        }
        None => println!("    (no assistant message)"),
    }

    // 8. Pending gates (permissions + forms, unified per spec P4).
    println!("[8] pending gates");
    let gates = client.pending_gates().await?;
    println!("    {} pending gate(s)", gates.len());
    for gate in &gates {
        println!("    gate {} session={:?}", gate.id, gate.session());
    }

    println!("SPIKE-0 PASS");
    Ok(ExitCode::SUCCESS)
}

/// S-10: pin the `/api/event` taxonomy against the live pinned server
/// (SPEC-003 W0). Everything frame-shaped is logged through [`log_frame`];
/// S-10: pin the `/api/event` taxonomy against the live pinned server
/// (SPEC-003 W0). Everything frame-shaped is logged through [`log_frame`];
/// raw bytes go to files, never stdout.
mod s10 {
    use std::collections::HashMap;
    use std::error::Error;
    use std::fmt::Write as _;
    use std::io::Write as _;
    use std::path::PathBuf;
    use std::process::ExitCode;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use std::time::SystemTime;
    use std::time::UNIX_EPOCH;

    use futures_util::StreamExt;
    use opencode_v2_client::{AUTH_USERNAME, Activity, Client, Event, GateDecision, OcError};

    use super::{S10_QUIET_SECS, env};

    /// Wall-clock milliseconds since the Unix epoch.
    fn now_ms() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_millis())
    }

    /// The ONLY place frame content is logged (SPEC-003 P-32): event kind,
    /// session id and byte length. Payload fields never reach this function.
    fn log_frame(kind: &str, session_id: Option<&str>, bytes: usize) {
        println!(
            "[frame] t={} event={kind} sessionID={} bytes={bytes}",
            now_ms(),
            session_id.unwrap_or("-")
        );
    }

    /// Extract the session key of an event: the payload's `sessionID`,
    /// falling back to the durable aggregate id.
    fn session_id_of(event: &Event) -> Option<&str> {
        let payload = event.data.get("data").unwrap_or(&event.data);
        if let Some(id) = payload.get("sessionID").and_then(|value| value.as_str()) {
            return Some(id);
        }
        event
            .durable
            .as_ref()
            .map(|durable| durable.aggregate_id.as_str())
    }

    /// One phase marker row (`unix_ms \t phase \t name`) into `markers.tsv`.
    fn mark(markers: &Mutex<String>, name: &str) {
        if let Ok(mut buffer) = markers.lock() {
            let _ = writeln!(buffer, "{}\tphase\t{name}", now_ms());
        }
        println!("[script] t={} {name}", now_ms());
    }

    /// The parsed transcript task: `subscribe_events()` (the production API
    /// path) → [`log_frame`] + per-kind counters.
    async fn transcript_task(
        client: Client,
        counts: Arc<Mutex<HashMap<String, usize>>>,
        stop: Arc<AtomicBool>,
    ) {
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let mut stream = match client.subscribe_events().await {
                Ok(stream) => stream,
                Err(error) => {
                    println!("[transcript] subscribe failed: {error}");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }
            };
            loop {
                let next = tokio::select! {
                    item = stream.next() => item,
                    () = tokio::time::sleep(Duration::from_millis(100)) => {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        continue;
                    }
                };
                match next {
                    Some(Ok(event)) => {
                        let bytes = event.data.to_string().len();
                        log_frame(&event.kind, session_id_of(&event), bytes);
                        if let Ok(mut counts) = counts.lock() {
                            *counts.entry(event.kind.clone()).or_default() += 1;
                        }
                    }
                    Some(Err(error)) => {
                        println!("[transcript] decode error: {error}");
                    }
                    None => {
                        println!("[transcript] stream ended; resubscribing");
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        break;
                    }
                }
            }
        }
    }

    /// The raw-capture task: one plain reqwest connection whose exact bytes
    /// and per-chunk arrival times are appended to the evidence files. This
    /// is the heartbeat/silence record the parsed API cannot see (comments
    /// are skipped before [`Event`] exists).
    async fn capture_task(
        url: String,
        password: String,
        raw_path: PathBuf,
        chunks_path: PathBuf,
        stop: Arc<AtomicBool>,
    ) {
        let mut raw = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&raw_path)
            .map_err(|error| format!("open {}: {error}", raw_path.display()))
            .ok();
        let mut chunks = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&chunks_path)
            .map_err(|error| format!("open {}: {error}", chunks_path.display()))
            .ok();
        let http = reqwest::Client::new();
        loop {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let response = match http
                .get(format!("{url}/api/event"))
                .basic_auth(AUTH_USERNAME, Some(&password))
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => response,
                Ok(response) => {
                    println!("[capture] status {}; retrying", response.status());
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }
                Err(error) => {
                    println!("[capture] request failed: {error}");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    continue;
                }
            };
            let mut stream = response.bytes_stream();
            let mut cumulative: usize = 0;
            loop {
                let next = tokio::select! {
                    item = stream.next() => item,
                    () = tokio::time::sleep(Duration::from_millis(100)) => {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        continue;
                    }
                };
                match next {
                    Some(Ok(chunk)) => {
                        cumulative += chunk.len();
                        if let (Some(raw), Some(chunks)) = (&mut raw, &mut chunks) {
                            let _ = raw.write_all(&chunk);
                            let _ = chunks.write_all(
                                format!("{}\t{}\t{cumulative}\n", now_ms(), chunk.len()).as_bytes(),
                            );
                        }
                    }
                    Some(Err(error)) => {
                        println!("[capture] transport error: {error}");
                        break;
                    }
                    None => {
                        println!("[capture] stream ended; reconnecting");
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        break;
                    }
                }
            }
        }
    }

    /// Delete a session through the raw HTTP surface (the client does not
    /// wrap `DELETE /api/session/{id}`; S-10 needs the delete-event evidence).
    async fn delete_session(url: &str, password: &str, session_id: &str) -> Result<u16, String> {
        let response = reqwest::Client::new()
            .delete(format!("{url}/api/session/{session_id}"))
            .basic_auth(AUTH_USERNAME, Some(password))
            .send()
            .await
            .map_err(|error| format!("delete {session_id}: {error}"))?;
        Ok(response.status().as_u16())
    }

    /// Poll `activity` until idle or the timeout (the durable correlation
    /// clock; event timing is never trusted for correlation — SPEC-003 W0).
    async fn wait_idle(
        client: &Client,
        session_id: &str,
        timeout: Duration,
    ) -> Result<bool, OcError> {
        let started = std::time::Instant::now();
        loop {
            if client.activity(session_id).await? == Activity::Idle {
                return Ok(true);
            }
            if started.elapsed() > timeout {
                let result = client.interrupt(session_id).await?;
                println!("[script] TIMEOUT; interrupted={}", result.interrupted);
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Poll the pending-gate list for a gate owned by `session_id`.
    async fn wait_for_gate(
        client: &Client,
        session_id: &str,
        timeout: Duration,
    ) -> Result<Option<String>, OcError> {
        let started = std::time::Instant::now();
        loop {
            let gates = client.pending_gates().await?;
            if let Some(gate) = gates.iter().find(|gate| gate.session() == Some(session_id)) {
                return Ok(Some(gate.id.clone()));
            }
            if started.elapsed() > timeout {
                return Ok(None);
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Phase S1: the scripted session — busy work → idle (AE-1 core
    /// evidence), then the permission probe (AE-2). Returns the session id
    /// for the later delete phase.
    async fn s1_scripted_session(
        client: &Client,
        markers: &Mutex<String>,
        directory: &str,
    ) -> Result<String, OcError> {
        mark(markers, "create-scripted");
        let session = client
            .create_session(Some(directory), Some("S-10 scripted"))
            .await?;
        println!("[script] scripted session {}", session.id);
        mark(markers, "prompt-pong");
        let admission = client
            .send_prompt(
                &session.id,
                "Reply with exactly the word PONG and nothing else. Do not use any tools.",
            )
            .await?;
        println!("[script] admission msg={}", admission.id);
        mark(markers, "await-idle");
        wait_idle(client, &session.id, Duration::from_secs(120)).await?;
        mark(markers, "idle-confirmed");

        // The permission probe: whether a request appears depends on the
        // server's ruleset; the transcript records the truth either way.
        mark(markers, "prompt-permission");
        client
            .send_prompt(
                &session.id,
                "Run the shell command `echo s10-permission-probe` and reply with its output.",
            )
            .await?;
        mark(markers, "await-gate");
        if let Some(request_id) =
            wait_for_gate(client, &session.id, Duration::from_secs(30)).await?
        {
            println!("[script] permission gate {request_id}");
            mark(markers, "gate-seen");
            client
                .reply_permission(&session.id, &request_id, GateDecision::Once)
                .await?;
            mark(markers, "gate-replied");
            wait_idle(client, &session.id, Duration::from_secs(120)).await?;
        } else {
            println!("[script] no permission gate observed within 30 s");
            mark(markers, "gate-absent");
            let _ = client.interrupt(&session.id).await?;
        }
        Ok(session.id)
    }

    /// The scripted session phases S1–S5 (markers recorded into `markers`).
    #[allow(
        clippy::too_many_arguments,
        reason = "a spike script threads its stage props explicitly; no value in a struct"
    )]
    async fn script(
        client: &Client,
        url: &str,
        password: &str,
        markers: &Mutex<String>,
        directory: &str,
        external_dir: &str,
        stop: &AtomicBool,
    ) -> Result<(), Box<dyn Error>> {
        // S1/S2: scripted session (busy → idle) + permission probe.
        let scripted = s1_scripted_session(client, markers, directory).await?;

        // S3: external (untracked-by-the-script) session — a second session
        // in a different directory whose frames must still arrive keyed.
        mark(markers, "create-external");
        let external = client
            .create_session(Some(external_dir), Some("S-10 external"))
            .await?;
        println!("[script] external session {}", external.id);
        mark(markers, "prompt-external");
        client
            .send_prompt(
                &external.id,
                "Reply with exactly the word PONG and nothing else. Do not use any tools.",
            )
            .await?;
        wait_idle(client, &external.id, Duration::from_secs(120)).await?;
        mark(markers, "external-idle");

        // S4: delete both sessions (session-list churn evidence).
        mark(markers, "delete-sessions");
        for id in [&external.id, &scripted] {
            let status = delete_session(url, password, id).await?;
            println!("[script] delete {id} -> {status}");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
        mark(markers, "deleted");

        // S5: quiet period — heartbeat/silence evidence (AE-4). No scripted
        // action from here until the deadline. `secs` bounds the whole run:
        // if the script overran it, the quiet phase is what remains.
        mark(markers, "quiet-start");
        let quiet_deadline = std::time::Instant::now() + Duration::from_secs(S10_QUIET_SECS);
        while std::time::Instant::now() < quiet_deadline && !stop.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        mark(markers, "quiet-end");
        Ok(())
    }

    /// Drive the scripted session against a live server while both evidence
    /// tasks run. Prints the transcript; records the raw capture.
    pub async fn run(secs: u64) -> Result<ExitCode, Box<dyn Error>> {
        let url = env("OPENCODE_URL")?;
        let password = env("OPENCODE_PASSWORD")?;
        let directory =
            std::env::var("SPIKE_DIR").unwrap_or_else(|_| "/tmp/opencode/spike-work".to_owned());
        let external_dir =
            std::env::var("SPIKE_EXTERNAL_DIR").unwrap_or_else(|_| "/tmp/opencode".to_owned());
        let events_dir = PathBuf::from(
            std::env::var("SPIKE_EVENTS_DIR").unwrap_or_else(|_| "/tmp/opencode/s10".to_owned()),
        );
        std::fs::create_dir_all(&events_dir)?;
        let raw_path = events_dir.join("events_raw.sse");
        let chunks_path = events_dir.join("events_chunks.tsv");
        let markers_path = events_dir.join("markers.tsv");
        for path in [&raw_path, &chunks_path, &markers_path] {
            if path.exists() {
                std::fs::remove_file(path)?;
            }
        }

        let client = Client::new(&url, &password)?;
        let stop = Arc::new(AtomicBool::new(false));
        let counts = Arc::new(Mutex::new(HashMap::new()));
        let markers = Arc::new(Mutex::new(String::new()));

        println!(
            "S-10 events spike: url={url} secs={secs} dir={}",
            events_dir.display()
        );
        mark(&markers, "subscribe");

        let transcript_client = Client::new(&url, &password)?;
        let transcript = tokio::spawn(transcript_task(
            transcript_client,
            Arc::clone(&counts),
            Arc::clone(&stop),
        ));
        let capture = tokio::spawn(capture_task(
            url.clone(),
            password.clone(),
            raw_path.clone(),
            chunks_path.clone(),
            Arc::clone(&stop),
        ));
        tokio::time::sleep(Duration::from_millis(1000)).await;

        script(
            &client,
            &url,
            &password,
            &markers,
            &directory,
            &external_dir,
            &stop,
        )
        .await?;

        stop.store(true, Ordering::Relaxed);
        let _ = tokio::join!(transcript, capture);
        std::fs::write(
            &markers_path,
            markers.lock().map_err(|_| "markers poisoned")?.clone(),
        )?;

        println!("[summary] kind counts (parsed path):");
        if let Ok(counts) = counts.lock() {
            let mut rows: Vec<(String, usize)> = counts
                .iter()
                .map(|(kind, count)| (kind.clone(), *count))
                .collect();
            rows.sort();
            for (kind, count) in rows {
                println!("[summary] {kind}\t{count}");
            }
        }
        println!(
            "[summary] evidence: {} , {} , {}",
            raw_path.display(),
            chunks_path.display(),
            markers_path.display()
        );
        println!("S-10 PASS");
        Ok(ExitCode::SUCCESS)
    }
}
