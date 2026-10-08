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

fn env(name: &str) -> Result<String, Box<dyn Error>> {
    std::env::var(name).map_err(|_| format!("missing required env var {name}").into())
}

#[tokio::main]
async fn main() -> Result<ExitCode, Box<dyn Error>> {
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
