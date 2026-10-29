//! Boot depth (SPEC-004 PH-2/PH-3 coverage lift): spawn the real binary
//! against a wiremock v2 server, wait for the `surface registry built` log
//! (past the whole bootstrap chain — config, secrets, probe, store, doctor
//! baseline, registry), then SIGTERM and accept the drain. The healthy
//! *gateway* boot needs real credentials and stays `live_manual`'s job, so
//! both drain outcomes (0) and a later gateway refusal (1) are accepted.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../opencode-client/fixtures/v2/2.0.24")
}

/// A discord-selected config with absolute paths (cwd-independent) and an
/// ephemeral metrics port.
fn write_config(dir: &Path, opencode_url: &str) -> PathBuf {
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "opencode_url = {opencode_url:?}\n\
             projects_root = {:?}\n\
             db_path = {:?}\n\
             templates_dir = {:?}\n\
             metrics_bind = \"127.0.0.1:0\"\n\
             permissions_file = {:?}\n\
             browse_roots = [{:?}]\n\
             inbox_dir = {:?}\n\
             [discord]\n\
             allowed_user_id = 42\n",
            dir.to_string_lossy(),
            dir.join("ocbot.db").to_string_lossy(),
            workspace().join("templates").to_string_lossy(),
            workspace()
                .join("config/opencode.permissions.jsonc")
                .to_string_lossy(),
            dir.to_string_lossy(),
            dir.join("inbox").to_string_lossy(),
        ),
    )
    .expect("write config");
    config
}

/// Spawn the real binary with piped stderr (the JSON logs are the only
/// observable before the drain).
#[expect(
    clippy::disallowed_methods,
    reason = "test harness: spawns the real ocbot binary for the deep-boot test"
)]
fn spawn(config: &Path) -> std::process::Child {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ocbot"));
    command
        .env("OCBOT_CONFIG", config)
        .env("DISCORD_TOKEN", "dummy-token")
        .env("OPENCODE_PASSWORD", "dummy-password")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for key in [
        "ALLOWED_USER_ID",
        "PER_SESSION_RULESET",
        "ALLOW_FREEFORM_PATHS",
        "ALLOW_NON_LOOPBACK",
        "SIGNAL_OPERATOR",
        "OCBOT_TEST_CRASH_AFTER",
    ] {
        command.env_remove(key);
    }
    command.spawn().expect("spawn ocbot")
}

#[tokio::test]
async fn boot_reaches_the_surface_registry_then_drains_on_sigterm() {
    let server = MockServer::start().await;
    let info: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(fixtures_dir().join("info.json")).expect("info fixture"),
    )
    .expect("info json");
    Mock::given(method("GET"))
        .and(path("/api/info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&info))
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().expect("tmp");
    let config = write_config(dir.path(), &server.uri());
    let mut child = spawn(&config);
    let stderr = child.stderr.take().expect("piped stderr");

    // Collect the JSON log lines from a reader thread; wait for the registry.
    let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let collector = lines.clone();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            collector.lock().expect("lock").push(line);
        }
    });
    let saw_registry = |needle: &str| {
        lines
            .lock()
            .expect("lock")
            .iter()
            .any(|line| line.contains(needle))
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while !saw_registry("surface registry built") {
        assert!(Instant::now() < deadline, "boot never reached the registry");
        std::thread::sleep(Duration::from_millis(50));
    }

    // The transport layers above the registry also ran (FR-42 log when the
    // ruleset is on is not; the reconcile line is the next milestone).
    while !saw_registry("boot reconcile done") {
        assert!(Instant::now() < deadline, "boot never reconciled");
        std::thread::sleep(Duration::from_millis(50));
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "test harness: delivers SIGTERM to the spawned ocbot binary"
    )]
    let sigterm = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill -TERM");
    assert!(sigterm.success());

    // Drain: exit 0, or 1 if the dummy token refused the gateway — both are
    // past the registry, which is what this test lifts coverage onto. The
    // SIGTERM may land mid-boot (before the gateway starts), which is exactly
    // the early-listener path: it still drains instead of dying on the signal.
    let status = wait_exit(&mut child);
    let recorded = lines.lock().expect("lock").clone();
    assert!(
        status.code() == Some(0) || status.code() == Some(1),
        "unexpected exit {:?} — log: {recorded:?}",
        status.code()
    );
    assert!(
        recorded
            .iter()
            .any(|line| line.contains("shutdown received; draining")),
        "the SIGTERM drain path ran: {recorded:?}"
    );
}

fn wait_exit(child: &mut std::process::Child) -> std::process::ExitStatus {
    for _ in 0..400 {
        if let Some(status) = child.try_wait().expect("wait") {
            return status;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    panic!("ocbot did not exit within 10s of SIGTERM");
}
