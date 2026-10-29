//! Binary-level boot matrix: spawn the real `ocbot` executable and assert the
//! exit codes of the first-live-run failure surface — config parse, missing
//! secrets, V1 refusal (FR-40), and a bad permission baseline. These all exit
//! *before* the Discord gateway, so they are testable offline. The healthy
//! boot + SIGTERM drain needs real Discord credentials and is `live_manual`'s
//! job.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::Duration;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../opencode-client/fixtures/v2/2.0.24")
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

/// Render a config with absolute paths so it is cwd-independent. `opencode_url`
/// and `permissions_file` are the two knobs each test varies.
fn write_config(dir: &Path, opencode_url: &str, permissions_file: &str) -> PathBuf {
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "opencode_url = {opencode_url:?}\n\
             projects_root = {dir:?}\n\
             db_path = {:?}\n\
             templates_dir = {templates:?}\n\
             metrics_bind = \"127.0.0.1:19964\"\n\
             permissions_file = {permissions_file:?}\n\
             browse_roots = [{dir:?}]\n\
             inbox_dir = {:?}\n\
             [discord]\n\
             allowed_user_id = 42\n",
            dir.join("ocbot.db").to_string_lossy(),
            dir.join("inbox").to_string_lossy(),
            templates = templates_dir(),
            dir = dir.to_string_lossy(),
        ),
    )
    .expect("write config");
    config
}

/// Spawn the real binary with a bounded wait; return its exit status.
#[expect(
    clippy::disallowed_methods,
    reason = "test harness: spawns the real ocbot binary for the boot matrix"
)]
fn run_binary(config: &Path, envs: &[(&str, &str)], remove: &[&str]) -> ExitStatus {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ocbot"));
    command
        .env("OCBOT_CONFIG", config)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for key in remove {
        command.env_remove(key);
    }
    for (key, value) in envs {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("spawn ocbot");
    for _ in 0..600 {
        if let Some(status) = child.try_wait().expect("wait") {
            return status;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    let _ = child.kill();
    panic!("ocbot binary did not exit within 15s");
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
];

#[test]
fn a_bad_config_exits_one() {
    let dir = tempfile::tempdir().expect("tmp");
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "opencode_url = \"http://127.0.0.1:9\"\nprojects_root = \"/tmp\"\n[discord]\nallowed_user_id = 42\nsurprise_key = true\n",
    )
    .expect("config");
    let status = run_binary(&config, &[], UNSET);
    assert_eq!(status.code(), Some(1), "config parse failure is exit 1");
}

#[test]
fn missing_secrets_exit_one() {
    let dir = tempfile::tempdir().expect("tmp");
    let config = write_config(dir.path(), "http://127.0.0.1:9", &shipped_baseline());
    let status = run_binary(&config, &[], UNSET);
    assert_eq!(status.code(), Some(1), "missing secrets are exit 1");
}

#[tokio::test]
async fn a_v1_server_is_refused_nonzero() {
    // No `/api/info` route: a 404 is the v1 signature (AC-16).
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().expect("tmp");
    let config = write_config(dir.path(), &server.uri(), &shipped_baseline());
    let status = run_binary(&config, SECRETS, UNSET);
    assert_eq!(status.code(), Some(1), "v1 refusal is exit 1 (FR-40)");
}

#[tokio::test]
async fn a_v2_server_with_a_bad_baseline_exits_two() {
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

    // A baseline missing the FR-21 `sudo` guard is a config fault: exit 2.
    let baseline_dir = tempfile::tempdir().expect("baseline tmp");
    let broken = baseline_dir.path().join("permissions.jsonc");
    let text = std::fs::read_to_string(shipped_baseline()).expect("shipped baseline");
    std::fs::write(&broken, text.replace("sudo*", "ls*")).expect("broken baseline");

    let dir = tempfile::tempdir().expect("tmp");
    let config = write_config(dir.path(), &server.uri(), &broken.to_string_lossy());
    let status = run_binary(&config, SECRETS, UNSET);
    assert_eq!(status.code(), Some(2), "baseline fault is exit 2 (FR-40c)");
}

// --- SPEC-003 W3: the [events] block is parsed at boot (P-30: loud errors) --

#[test]
fn an_unknown_events_key_exits_one() {
    let dir = tempfile::tempdir().expect("tmp");
    let config = write_config(dir.path(), "http://127.0.0.1:9", &shipped_baseline());
    let mut text = std::fs::read_to_string(&config).expect("config");
    text.push_str("\n[events]\nenabled_lies = true\n");
    std::fs::write(&config, text).expect("rewrite config");
    let status = run_binary(&config, &[], UNSET);
    assert_eq!(
        status.code(),
        Some(1),
        "an unknown [events] key is a load-time error (P-30)"
    );
}
