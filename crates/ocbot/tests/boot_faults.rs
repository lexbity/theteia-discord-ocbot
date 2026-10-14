//! Boot fault lines (SPEC-004 PH-3 coverage lift): the remaining *early-exit*
//! families in `run.rs` that the boot matrix does not reach — an fs adapter
//! that rejects its browse roots, a non-loopback opencode url refused by the
//! client, and the FR-42 ruleset branch (on by config) that the default-off
//! gate (G13) otherwise leaves dark.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn templates_dir() -> String {
    workspace().join("templates").to_string_lossy().into_owned()
}

fn shipped_baseline() -> String {
    workspace()
        .join("config/opencode.permissions.jsonc")
        .to_string_lossy()
        .into_owned()
}

/// The config body shared by every test: `browse_roots`, the ruleset flag and
/// `ALLOW_NON_LOOPBACK` are the knobs each one varies.
fn config_text(dir: &Path, opencode_url: &str, browse_roots: &str, ruleset: &str) -> String {
    format!(
        "opencode_url = {opencode_url:?}\n\
         db_path = {:?}\n\
         templates_dir = {:?}\n\
         metrics_bind = \"127.0.0.1:0\"\n\
         permissions_file = {:?}\n\
         browse_roots = {browse_roots}\n\
         inbox_dir = {:?}\n\
         {ruleset}\n\
         [discord]\n\
         allowed_user_id = 42\n",
        dir.join("ocbot.db").to_string_lossy(),
        templates_dir(),
        shipped_baseline(),
        dir.join("inbox").to_string_lossy(),
    )
}

const SECRETS: &[(&str, &str)] = &[
    ("DISCORD_TOKEN", "dummy-token"),
    ("OPENCODE_PASSWORD", "dummy-password"),
];
const UNSET: &[&str] = &[
    "DISCORD_TOKEN",
    "OPENCODE_PASSWORD",
    "ALLOWED_USER_ID",
    "PER_SESSION_RULESET",
    "ALLOW_FREEFORM_PATHS",
    "ALLOW_NON_LOOPBACK",
    "SIGNAL_OPERATOR",
];

/// A spawned binary plus its captured stderr (the JSON logs are the only
/// observable of which branch ran).
struct Spawned {
    child: std::process::Child,
    stderr: Arc<Mutex<String>>,
}

#[expect(
    clippy::disallowed_methods,
    reason = "test harness: spawns the real ocbot binary for boot-fault assertions"
)]
fn spawn(config: &Path, envs: &[(&str, &str)]) -> Spawned {
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
    let stderr = Arc::new(Mutex::new(String::new()));
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
    fn log(&self) -> String {
        self.stderr.lock().expect("lock").clone()
    }

    /// Wait for `needle` in the log, or fail naming the lines seen.
    fn wait_for(&self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self.log().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "never saw {needle:?}: {}",
                self.log()
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn sigterm_and_wait(mut self) -> std::process::ExitStatus {
        #[expect(
            clippy::disallowed_methods,
            reason = "test harness: delivers SIGTERM to the spawned ocbot binary"
        )]
        let sigterm = Command::new("kill")
            .args(["-TERM", &self.child.id().to_string()])
            .status()
            .expect("kill -TERM");
        assert!(sigterm.success());
        for _ in 0..400 {
            if let Some(status) = self.child.try_wait().expect("wait") {
                return status;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = self.child.kill();
        panic!("ocbot did not exit within 10s of SIGTERM");
    }

    fn wait_for_exit(mut self) -> std::process::ExitStatus {
        for _ in 0..400 {
            if let Some(status) = self.child.try_wait().expect("wait") {
                return status;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let _ = self.child.kill();
        panic!("ocbot did not exit within 10s");
    }
}

fn write_config(dir: &Path, text: &str) -> PathBuf {
    let config = dir.join("config.toml");
    std::fs::write(&config, text).expect("write config");
    config
}

#[test]
fn empty_browse_roots_are_refused_by_the_filesystem_adapter() {
    // P11 containment: a bot with no browse root cannot resolve any typed
    // path, so the adapter refuses and boot exits 1.
    let dir = tempfile::tempdir().expect("tmp");
    let config = write_config(
        dir.path(),
        &config_text(dir.path(), "http://127.0.0.1:9", "[]", ""),
    );
    let status = spawn(&config, SECRETS).wait_for_exit();
    assert_eq!(status.code(), Some(1), "an fs adapter refusal is exit 1");
}

#[test]
fn a_non_loopback_opencode_url_is_refused_by_the_client() {
    // FR-4: the client refuses a non-loopback url unless the deployment opts
    // in. Without the override it is an exit-1 config fault, not a warning.
    let dir = tempfile::tempdir().expect("tmp");
    let config = write_config(
        dir.path(),
        &config_text(
            dir.path(),
            "http://opencode.example.com:8080",
            &format!("[{:?}]", dir.path()),
            "",
        ),
    );
    let status = spawn(&config, SECRETS).wait_for_exit();
    assert_eq!(status.code(), Some(1), "FR-4 loopback refusal is exit 1");
}

#[tokio::test]
async fn the_fr42_ruleset_branch_attaches_the_baseline_when_enabled() {
    // G13 pins this default-off; this proves the opt-in path runs to boot
    // (the log line and the ruleset the context carries).
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
    let config = write_config(
        dir.path(),
        &config_text(
            dir.path(),
            &server.uri(),
            &format!("[{:?}]", dir.path()),
            "per_session_ruleset = true\n",
        ),
    );
    let spawned = spawn(&config, SECRETS);
    spawned.wait_for("FR-42 active: attaching the baseline to created sessions");
    spawned.wait_for("surface registry built");
    let status = spawned.sigterm_and_wait();
    assert_ne!(
        status.code(),
        Some(1),
        "the enabled branch boots: {status:?}"
    );
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../opencode-client/fixtures/v2/2.0.24")
}
