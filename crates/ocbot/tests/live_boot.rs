//! Live boot + drain harness (the one thing the offline suite cannot cover:
//! the real Discord gateway and a real opencode server). Spawns the actual
//! `ocbot` binary, waits for it to settle, sends **SIGTERM** (what systemd
//! sends), and asserts a clean exit 0 — i.e. the drain path runs.
//!
//! Not run by default. To execute:
//!
//! ```sh
//! DISCORD_TOKEN=<token> OPENCODE_URL=http://127.0.0.1:8080 \
//! OPENCODE_PASSWORD=<pw> ALLOWED_USER_ID=<uid> \
//!   cargo test -p ocbot --test live_boot -- --ignored --nocapture
//! ```

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Spawn the real binary; returns the child and the stderr capture path.
#[expect(
    clippy::disallowed_methods,
    reason = "live harness: spawns the real ocbot binary"
)]
fn spawn_bot(config: &Path, stderr: &Path) -> (Child, std::fs::File) {
    let file = std::fs::File::create(stderr).expect("stderr file");
    let child = Command::new(env!("CARGO_BIN_EXE_ocbot"))
        .env("OCBOT_CONFIG", config)
        .stdout(Stdio::null())
        .stderr(Stdio::from(file.try_clone().expect("clone stderr")))
        .spawn()
        .expect("spawn ocbot");
    (child, file)
}

/// Send SIGTERM via `kill(1)` (no `unsafe`/`libc` in the workspace).
#[expect(
    clippy::disallowed_methods,
    reason = "live harness: uses kill(1) to deliver SIGTERM"
)]
fn send_sigterm(pid: u32) {
    let status = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .expect("run kill");
    assert!(status.success(), "kill -TERM {pid} failed");
}

/// Poll for up to `timeout`; returns the exit status if the child exited.
fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

#[test]
#[ignore = "requires DISCORD_TOKEN + a live opencode v2 server (OPENCODE_URL/OPENCODE_PASSWORD)"]
fn live_boot_starts_then_drains_on_sigterm() {
    if std::env::var("DISCORD_TOKEN").is_err() {
        tracing::warn!("DISCORD_TOKEN not set; skipping the live boot");
        return;
    }
    let Ok(server) = std::env::var("OPENCODE_URL") else {
        tracing::warn!("OPENCODE_URL not set; skipping the live boot");
        return;
    };
    let operator = std::env::var("ALLOWED_USER_ID").expect("ALLOWED_USER_ID set");
    // The child inherits the parent environment (DISCORD_TOKEN,
    // OPENCODE_PASSWORD); presence was checked above.
    let _ = std::env::var("OPENCODE_PASSWORD").expect("OPENCODE_PASSWORD set");

    let tmp = tempfile::tempdir().expect("tmp");
    let workspace = workspace();
    let config = tmp.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "opencode_url = {:?}\n\
             db_path = {:?}\n\
             templates_dir = {:?}\n\
             allowed_user_id = {}\n\
             metrics_bind = \"127.0.0.1:19465\"\n\
             permissions_file = {:?}\n\
             browse_roots = [{:?}]\n\
             inbox_dir = {:?}\n",
            server,
            tmp.path().join("ocbot.db").to_string_lossy(),
            workspace.join("templates").to_string_lossy(),
            operator,
            workspace
                .join("config/opencode.permissions.jsonc")
                .to_string_lossy(),
            tmp.path().to_string_lossy(),
            tmp.path().join("inbox").to_string_lossy(),
        ),
    )
    .expect("config");

    let stderr_path = tmp.path().join("stderr.log");
    let (mut child, _stderr) = spawn_bot(&config, &stderr_path);

    // Give the bot time to load config, doctor the server, reconcile and start
    // the gateway. If it exits this early, surface the captured stderr.
    std::thread::sleep(Duration::from_secs(8));
    if let Some(status) = child.try_wait().expect("try_wait") {
        let log = std::fs::read_to_string(&stderr_path).unwrap_or_default();
        panic!("ocbot exited early with {status:?}\n{log}");
    }

    send_sigterm(child.id());
    let Some(status) = wait_for_exit(&mut child, Duration::from_secs(30)) else {
        let _ = child.kill();
        panic!("ocbot did not exit within 30s of SIGTERM");
    };
    let log = std::fs::read_to_string(&stderr_path).unwrap_or_default();
    assert_eq!(
        status.code(),
        Some(0),
        "SIGTERM must drain cleanly (got {status:?})\n{log}"
    );
}
