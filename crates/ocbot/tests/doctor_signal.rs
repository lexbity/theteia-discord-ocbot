//! SPEC-004 PH-3: the Signal surface doctor (FR-114/FR-116/NFR-103) against
//! the pinned fake daemon, plus the binary-level refusal family: a doctor
//! failure refuses the *surface* (exit 3 when nothing is left), never the
//! whole process while Discord can still serve (D-10).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::signal_fake::{FakeSignal, Script};
use ocbot::app::doctor::{SIGNAL_DOCTOR_BUDGET, signal_daemon};
use ocbot::config::SignalSection;

fn section(socket: &Path) -> SignalSection {
    SignalSection {
        socket: Some(socket.to_string_lossy().into_owned()),
        operator_number: Some("+15551234567".to_owned()),
        ..SignalSection::default()
    }
}

async fn expect_err(section: &SignalSection) -> Vec<String> {
    match signal_daemon(section).await {
        Ok(report) => panic!("expected a refusal, got {report:?}"),
        Err(errors) => errors,
    }
}

// --- the doctor itself ------------------------------------------------------

#[tokio::test]
async fn a_healthy_daemon_reports_version_and_accounts() {
    let dir = tempfile::tempdir().expect("tmp");
    let socket = dir.path().join("signal.sock");
    let _daemon = FakeSignal::bind(socket.clone(), Script::default());
    let report = signal_daemon(&section(&socket))
        .await
        .expect("healthy daemon");
    assert_eq!(report.version, "0.15.0");
    assert_eq!(report.accounts, vec!["+15551234567".to_owned()]);
}

#[tokio::test]
async fn an_old_version_is_refused_naming_both_versions() {
    let dir = tempfile::tempdir().expect("tmp");
    let socket = dir.path().join("signal.sock");
    let _daemon = FakeSignal::bind(
        socket.clone(),
        Script {
            version: "0.13.0".to_owned(),
            ..Script::default()
        },
    );
    let errors = expect_err(&section(&socket)).await;
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].contains("0.13.0") && errors[0].contains("0.15.0"),
        "{errors:?}"
    );
}

#[tokio::test]
async fn a_daemon_without_accounts_is_refused() {
    let dir = tempfile::tempdir().expect("tmp");
    let socket = dir.path().join("signal.sock");
    let _daemon = FakeSignal::bind(
        socket.clone(),
        Script {
            accounts: Vec::new(),
            ..Script::default()
        },
    );
    let errors = expect_err(&section(&socket)).await;
    assert!(
        errors
            .iter()
            .any(|error| error.contains("no registered accounts")),
        "{errors:?}"
    );
}

#[tokio::test]
async fn a_world_readable_socket_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("tmp");
    let socket = dir.path().join("signal.sock");
    let _daemon = FakeSignal::bind(socket.clone(), Script::default());
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o644)).expect("chmod");
    let errors = expect_err(&section(&socket)).await;
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0].contains("644") && errors[0].contains("0600"),
        "{errors:?}"
    );
}

#[tokio::test]
async fn a_missing_socket_is_refused() {
    let dir = tempfile::tempdir().expect("tmp");
    let errors = expect_err(&section(&dir.path().join("absent.sock"))).await;
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("absent.sock"), "{errors:?}");
}

#[tokio::test]
async fn a_regular_file_at_the_socket_path_is_refused() {
    let dir = tempfile::tempdir().expect("tmp");
    let path = dir.path().join("signal.sock");
    std::fs::write(&path, b"not a socket").expect("file");
    let errors = expect_err(&section(&path)).await;
    assert!(
        errors
            .iter()
            .any(|error| error.contains("not a unix socket")),
        "{errors:?}"
    );
}

#[tokio::test]
async fn a_stale_socket_file_is_refused_quickly() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().expect("tmp");
    let socket = dir.path().join("signal.sock");
    {
        // A bound-then-dropped listener leaves the file behind: connecting is
        // refused (the classic dead-daemon leftover). The mode check must not
        // fire first, or the test would never reach the connect attempt — the
        // dead socket is left pristine at 0600.
        let stale = std::os::unix::net::UnixListener::bind(&socket).expect("stale bind");
        drop(stale);
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .expect("chmod 0600");
    }
    let started = Instant::now();
    let errors = expect_err(&section(&socket)).await;
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the refusal must not stall the boot (NFR-103)"
    );
    assert!(
        errors.iter().any(|error| error.contains("cannot connect")),
        "{errors:?}"
    );
}

#[tokio::test]
async fn a_hanging_daemon_exhausts_the_budget() {
    let dir = tempfile::tempdir().expect("tmp");
    let socket = dir.path().join("signal.sock");
    let _daemon = FakeSignal::bind(
        socket.clone(),
        Script {
            hang: true,
            ..Script::default()
        },
    );
    let started = Instant::now();
    let errors = expect_err(&section(&socket)).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed >= SIGNAL_DOCTOR_BUDGET,
        "the budget is the contract: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "NFR-103: the boot must not stall: {elapsed:?}"
    );
    assert!(
        errors.iter().any(|error| error.contains("did not answer")),
        "{errors:?}"
    );
}

#[tokio::test]
async fn a_missing_socket_setting_is_reported() {
    let errors = expect_err(&SignalSection::default()).await;
    assert!(
        errors.iter().any(|error| error.contains("[signal].socket")),
        "{errors:?}"
    );
}

// --- the binary refusal family ----------------------------------------------

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A signal-aware config: `channel` is a **verbatim TOML value** (`"signal"`,
/// `["discord", "signal"]`), `socket` the path each test varies.
/// `opencode_url` points at a closed port — the probe then degrades (warn)
/// without needing a server.
fn write_config(dir: &Path, channel: &str, socket: Option<&Path>, discord: bool) -> PathBuf {
    let config = dir.join("config.toml");
    let mut text = format!(
        "opencode_url = \"http://127.0.0.1:9\"\n\
         projects_root = {:?}\n\
         channel = {channel}\n\
         db_path = {:?}\n\
         templates_dir = {:?}\n\
         metrics_bind = \"127.0.0.1:0\"\n\
         permissions_file = {:?}\n\
         browse_roots = [{:?}]\n\
         inbox_dir = {:?}\n",
        dir.to_string_lossy(),
        dir.join("ocbot.db").to_string_lossy(),
        workspace().join("templates").to_string_lossy(),
        workspace()
            .join("config/opencode.permissions.jsonc")
            .to_string_lossy(),
        dir.to_string_lossy(),
        dir.join("inbox").to_string_lossy(),
    );
    if discord {
        text.push_str("[discord]\nallowed_user_id = 42\n");
    }
    text.push_str("[signal]\n");
    if let Some(socket) = socket {
        use std::fmt::Write as _;
        let _ = writeln!(text, "socket = {:?}", socket.to_string_lossy());
    }
    text.push_str("operator_number = \"+15551234567\"\n");
    std::fs::write(&config, text).expect("write config");
    config
}

const SECRETS: &[(&str, &str)] = &[
    ("DISCORD_TOKEN", "dummy-token"),
    ("OPENCODE_PASSWORD", "dummy-password"),
];
const UNSET: &[&str] = &[
    "DISCORD_TOKEN",
    "OPENCODE_PASSWORD",
    "ALLOWED_USER_ID",
    "SIGNAL_OPERATOR",
    "PER_SESSION_RULESET",
    "ALLOW_FREEFORM_PATHS",
    "ALLOW_NON_LOOPBACK",
];

/// A spawned binary plus its captured stderr (the JSON logs are the only
/// observable of *why* a refusal family fired).
struct Spawned {
    child: std::process::Child,
    stderr: Arc<std::sync::Mutex<String>>,
}

/// Spawn the real binary for the doctor matrix (secrets are removed unless a
/// test passes them: the signal doctor runs before secrets, NFR-103).
#[expect(
    clippy::disallowed_methods,
    reason = "test harness: spawns the real ocbot binary for the doctor matrix"
)]
fn spawn_binary(config: &Path, envs: &[(&str, &str)]) -> Spawned {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ocbot"));
    command
        .env("OCBOT_CONFIG", config)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    for key in UNSET {
        command.env_remove(key);
    }
    for (key, value) in envs {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("spawn ocbot");
    let stderr = Arc::new(std::sync::Mutex::new(String::new()));
    let sink = stderr.clone();
    let pipe = child.stderr.take().expect("piped stderr");
    std::thread::spawn(move || {
        use std::io::BufRead as _;
        for line in std::io::BufReader::new(pipe).lines() {
            let Ok(line) = line else { break };
            let mut sink = sink.lock().expect("lock");
            sink.push_str(&line);
            sink.push('\n');
        }
    });
    Spawned { child, stderr }
}

impl Spawned {
    /// The captured log lines so far.
    fn log(&self) -> String {
        self.stderr.lock().expect("lock").clone()
    }

    /// `true` while the process lives (a doctor refusal of *one* surface must
    /// not take the process down).
    fn alive(&mut self) -> bool {
        self.child.try_wait().expect("wait").is_none()
    }

    /// Deliver SIGTERM (systemd's `KillSignal`) and return what it exited
    /// with, and after how long.
    async fn sigterm_and_wait(mut self) -> (std::process::ExitStatus, Duration) {
        #[expect(
            clippy::disallowed_methods,
            reason = "test harness: delivers SIGTERM to the spawned ocbot binary"
        )]
        let sigterm = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("kill -TERM");
        assert!(sigterm.success());
        self.wait_for_exit().await
    }

    /// Wait for the process to exit (bounded), yielding to the runtime so the
    /// in-process fake daemon's accept loop keeps serving while we wait — a
    /// blocking sleep here would starve it and turn every daemon answer into
    /// a budget timeout.
    async fn wait_for_exit(&mut self) -> (std::process::ExitStatus, Duration) {
        let started = Instant::now();
        for _ in 0..800 {
            if let Some(status) = self.child.try_wait().expect("wait") {
                return (status, started.elapsed());
            }
            assert!(
                started.elapsed() < Duration::from_secs(20),
                "ocbot did not exit within 20s"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        panic!("ocbot did not exit within 20s");
    }

    /// Sleep `delay` (asynchronously — see `wait_for_exit`) and report
    /// whether the process is still running.
    async fn alive_after(&mut self, delay: Duration) -> bool {
        tokio::time::sleep(delay).await;
        self.alive()
    }
}

#[tokio::test]
async fn signal_only_with_a_wrong_version_daemon_exits_three() {
    let dir = tempfile::tempdir().expect("tmp");
    let socket = dir.path().join("signal.sock");
    let _daemon = FakeSignal::bind(
        socket.clone(),
        Script {
            version: "0.9.0".to_owned(),
            ..Script::default()
        },
    );
    let config = write_config(dir.path(), "\"signal\"", Some(&socket), false);
    let mut spawned = spawn_binary(&config, &[]);
    let (status, elapsed) = spawned.wait_for_exit().await;
    assert_eq!(
        status.code(),
        Some(3),
        "a refused surface with no survivors is exit 3"
    );
    assert!(elapsed < Duration::from_secs(10), "NFR-103: {elapsed:?}");
    // The refusal is the version gate, not a budget timeout: assert the reason.
    assert!(
        spawned.log().contains("older than the pinned minimum"),
        "the version gate named the daemon: {}",
        spawned.log()
    );
}

#[tokio::test]
async fn signal_only_with_an_absent_socket_exits_three_quickly() {
    let dir = tempfile::tempdir().expect("tmp");
    let config = write_config(
        dir.path(),
        "\"signal\"",
        Some(&dir.path().join("absent.sock")),
        false,
    );
    let (status, elapsed) = spawn_binary(&config, &[]).wait_for_exit().await;
    assert_eq!(status.code(), Some(3));
    assert!(
        elapsed < Duration::from_secs(5),
        "an absent daemon must not stall the boot (NFR-103): {elapsed:?}"
    );
}

#[tokio::test]
async fn signal_only_without_a_socket_setting_exits_one() {
    // Config-level refusal (the section keys are required, D-09) — exit 1, the
    // config family, not the surface family. The log must say so.
    let dir = tempfile::tempdir().expect("tmp");
    let config = write_config(dir.path(), "\"signal\"", None, false);
    let mut spawned = spawn_binary(&config, &[]);
    let (status, _elapsed) = spawned.wait_for_exit().await;
    assert_eq!(status.code(), Some(1));
}

#[tokio::test]
async fn signal_only_with_a_healthy_daemon_boots_and_drains() {
    // PH-4: a healthy daemon now boots the Signal surface — no Discord needed.
    // The opencode probe degrades (closed port) without blocking the surface.
    let dir = tempfile::tempdir().expect("tmp");
    let socket = dir.path().join("signal.sock");
    let _daemon = FakeSignal::bind(socket.clone(), Script::default());
    let config = write_config(dir.path(), "\"signal\"", Some(&socket), false);
    let mut spawned = spawn_binary(&config, SECRETS);
    assert!(
        spawned.alive_after(Duration::from_millis(3_500)).await,
        "the signal surface must boot: {}",
        spawned.log()
    );
    assert!(
        spawned.log().contains("signal-cli daemon healthy"),
        "the healthy path is logged: {}",
        spawned.log()
    );
    assert!(
        spawned.log().contains("signal inbound pump started"),
        "the inbound pump started: {}",
        spawned.log()
    );
    let (status, _elapsed) = spawned.sigterm_and_wait().await;
    assert_eq!(status.code(), Some(0), "a clean drain");
}

#[tokio::test]
async fn a_healthy_signal_daemon_next_to_discord_boots_both_surfaces() {
    // PH-4: both selected surfaces boot; the registry names them both.
    let dir = tempfile::tempdir().expect("tmp");
    let socket = dir.path().join("signal.sock");
    let _daemon = FakeSignal::bind(socket.clone(), Script::default());
    let config = write_config(dir.path(), "[\"discord\", \"signal\"]", Some(&socket), true);
    let mut spawned = spawn_binary(&config, SECRETS);
    assert!(
        spawned.alive_after(Duration::from_millis(3_500)).await,
        "both surfaces must boot: {}",
        spawned.log()
    );
    assert!(
        spawned.log().contains("signal inbound pump started"),
        "the signal surface is live: {}",
        spawned.log()
    );
    let (status, _elapsed) = spawned.sigterm_and_wait().await;
    assert_ne!(status.code(), Some(3));
}

#[tokio::test]
async fn discord_keeps_booting_when_the_signal_surface_is_refused() {
    let dir = tempfile::tempdir().expect("tmp");
    let socket = dir.path().join("signal.sock");
    let _daemon = FakeSignal::bind(
        socket.clone(),
        Script {
            version: "0.9.0".to_owned(),
            ..Script::default()
        },
    );
    let config = write_config(dir.path(), "[\"discord\", \"signal\"]", Some(&socket), true);
    let mut spawned = spawn_binary(&config, SECRETS);

    // The signal surface is refused, but the process must not exit: the
    // discord surface keeps booting (D-10 / NFR-103).
    assert!(
        spawned.alive_after(Duration::from_millis(3_500)).await,
        "ocbot exited — the signal refusal took the process down: {}",
        spawned.log()
    );
    assert!(
        spawned
            .log()
            .contains("signal surface refused by the boot doctor"),
        "the refusal is loud in the log: {}",
        spawned.log()
    );
    assert!(
        spawned.log().contains("older than the pinned minimum"),
        "and the reason is the version gate, not a budget timeout: {}",
        spawned.log()
    );

    // A SIGTERM drains the discord-booting process to a clean exit.
    let (status, _elapsed) = spawned.sigterm_and_wait().await;
    assert_ne!(status.code(), Some(3), "only the surface was refused");
}
