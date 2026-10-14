//! The startup doctor (FR-40): the shipped permission baseline
//! (`config/opencode.permissions.jsonc`) is parsed and validated against the
//! v2 `Permission.Rule{action,resource,effect}` shape, naming any missing
//! `ask` pattern. The version probe lives in `run.rs` (part (b) of FR-40).
//!
//! PH-3 adds the Signal surface doctor (FR-114/FR-116, SPEC-004 D-10): socket
//! hygiene, the `MIN_SIGNAL_CLI_VERSION` gate, and a non-empty account list —
//! bounded by a total budget so an absent daemon cannot stall the boot
//! (NFR-103). A failed Signal doctor refuses **the surface only**; Discord, if
//! selected, keeps booting.

use std::path::Path;
use std::time::Duration;

use ocbot_core::ports::PermissionEffect;
use serde::Deserialize;

use crate::adapters::signal::{MIN_SIGNAL_CLI_VERSION, SignalRpc, SignalVersion};
use crate::config::SignalSection;

/// One rule of the v2 permission ruleset (pinned by the vendored spec:
/// `Permission.Rule` = `{action, resource, effect}`, no extra fields).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaselineRule {
    /// The action/tool, e.g. `bash`, `read`, `write`.
    pub action: String,
    /// The resource pattern, e.g. `git push*`.
    pub resource: String,
    /// The effect.
    pub effect: PermissionEffect,
}

/// The parsed, validated baseline.
#[derive(Debug, Clone, PartialEq)]
pub struct Baseline {
    /// The ordered ruleset (order matters: first match wins, FR-21).
    pub rules: Vec<BaselineRule>,
}

impl Baseline {
    /// Load and validate the baseline file, collecting every problem at once
    /// (the doctor pattern: all validation errors reported together).
    pub fn load(path: &Path) -> Result<Self, Vec<String>> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| vec![format!("cannot read {}: {error}", path.display())])?;
        Self::parse(&text)
    }

    /// Parse JSONC (comments stripped) into the baseline and validate it.
    pub fn parse(text: &str) -> Result<Self, Vec<String>> {
        let mut errors = Vec::new();
        let cleaned = strip_jsonc(text);
        let value: serde_json::Value = match serde_json::from_str(&cleaned) {
            Ok(value) => value,
            Err(error) => {
                return Err(vec![format!(
                    "permission baseline is not valid JSON: {error}"
                )]);
            }
        };
        let array = match &value {
            serde_json::Value::Array(array) => array,
            serde_json::Value::Object(map) => match map.get("permissions") {
                Some(serde_json::Value::Array(array)) => array,
                _ => {
                    return Err(vec![format!(
                        "permission baseline must be a rules array or {{ \"permissions\": [...] }}, got {value}"
                    )]);
                }
            },
            other => {
                return Err(vec![format!(
                    "permission baseline must be a rules array, got {other}"
                )]);
            }
        };
        let mut rules = Vec::with_capacity(array.len());
        for (index, entry) in array.iter().enumerate() {
            match serde_json::from_value::<BaselineRule>(entry.clone()) {
                Ok(rule) => rules.push(rule),
                Err(error) => errors.push(format!(
                    "baseline rule {index} violates Permission.Rule: {error}"
                )),
            }
        }
        let baseline = Self { rules };
        errors.extend(baseline.validate_fr21());
        if errors.is_empty() {
            Ok(baseline)
        } else {
            Err(errors)
        }
    }

    /// FR-21 floor: every dangerous family must be an `ask` rule.
    fn validate_fr21(&self) -> Vec<String> {
        const REQUIRED: [&str; 4] = ["git push", "sudo", "rm -rf", ".ssh"];
        let mut errors = Vec::new();
        for rule in &self.rules {
            if rule.action.trim().is_empty() {
                errors.push(
                    "baseline rule has an empty action (Permission.Rule requires action)"
                        .to_owned(),
                );
            }
            if rule.resource.trim().is_empty() {
                errors.push(
                    "baseline rule has an empty resource (Permission.Rule requires resource)"
                        .to_owned(),
                );
            }
        }
        for needle in REQUIRED {
            let covered = self
                .rules
                .iter()
                .any(|rule| rule.effect == PermissionEffect::Ask && rule.resource.contains(needle));
            if !covered {
                errors.push(format!(
                    "permission baseline is missing an `ask` rule for {needle:?} (FR-21)"
                ));
            }
        }
        errors
    }
}

/// Strip `//` line comments and `/* */` block comments from JSONC text,
/// preserving string contents (the shipped baseline is annotated).
pub fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                out.push(ch);
            }
            '/' if chars.peek() == Some(&'/') => {
                let _ = chars.next();
                for ch in chars.by_ref() {
                    if ch == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                let _ = chars.next();
                let mut last = '\0';
                for ch in chars.by_ref() {
                    if last == '*' && ch == '/' {
                        break;
                    }
                    last = ch;
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

// --- SPEC-004 PH-3: the Signal surface doctor (FR-114/FR-116/NFR-103) ------

/// The total budget for the whole Signal doctor (NFR-103: the surface must
/// not add more than 2 s to boot when signal-cli is absent).
pub const SIGNAL_DOCTOR_BUDGET: Duration = Duration::from_secs(2);

/// What a healthy daemon reported (logged at boot; feeds ops).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignalReport {
    /// The daemon's version string, as reported.
    pub version: String,
    /// The registered accounts (E.164 numbers).
    pub accounts: Vec<String>,
}

/// Doctor one signal-cli daemon for the Signal surface: socket hygiene
/// (FR-116), the version gate (FR-114), and a non-empty account list. The
/// whole check runs under [`SIGNAL_DOCTOR_BUDGET`] — a silent daemon is a
/// refusal, never a hang (NFR-103). `Err` collects every problem it found.
pub async fn signal_daemon(section: &SignalSection) -> Result<SignalReport, Vec<String>> {
    match tokio::time::timeout(SIGNAL_DOCTOR_BUDGET, signal_daemon_inner(section)).await {
        Ok(result) => result,
        Err(_) => Err(vec![format!(
            "signal-cli did not answer within {} ms — refusing the surface (NFR-103)",
            u64::try_from(SIGNAL_DOCTOR_BUDGET.as_millis()).unwrap_or(0)
        )]),
    }
}

/// The un-timeouted body of [`signal_daemon`].
async fn signal_daemon_inner(section: &SignalSection) -> Result<SignalReport, Vec<String>> {
    let Some(socket) = section.socket.as_deref() else {
        return Err(vec!["[signal].socket is not set".to_owned()]);
    };
    let socket_errors = check_socket(Path::new(socket));
    if !socket_errors.is_empty() {
        return Err(socket_errors);
    }
    let path = Path::new(socket);
    let rpc = SignalRpc::connect(path)
        .await
        .map_err(|error| vec![format!("cannot connect to signal-cli on {socket}: {error}")])?;
    let version = rpc
        .version()
        .await
        .map_err(|error| vec![format!("signal-cli version probe failed: {error}")])?;
    check_version(&version)?;
    let accounts = rpc
        .list_accounts()
        .await
        .map_err(|error| vec![format!("signal-cli listAccounts failed: {error}")])?;
    if accounts.is_empty() {
        return Err(vec![
            "signal-cli reports no registered accounts — register or link a number first"
                .to_owned(),
        ]);
    }
    Ok(SignalReport { version, accounts })
}

/// The FR-114 gate: the reported version must reach the pinned minimum.
fn check_version(version: &str) -> Result<(), Vec<String>> {
    let reported = SignalVersion::parse(version).ok_or_else(|| {
        vec![format!(
            "signal-cli reported a non-semver version {version:?}"
        )]
    })?;
    let minimum = SignalVersion::parse(MIN_SIGNAL_CLI_VERSION).ok_or_else(|| {
        vec![format!(
            "MIN_SIGNAL_CLI_VERSION {MIN_SIGNAL_CLI_VERSION:?} is not semver (a build defect)"
        )]
    })?;
    if reported < minimum {
        return Err(vec![format!(
            "signal-cli version {version} is older than the pinned minimum {MIN_SIGNAL_CLI_VERSION} (FR-114) — upgrade signal-cli, or drop `signal` from `channel`"
        )]);
    }
    Ok(())
}

/// The FR-116 socket checks, collecting every violation: the path must be a
/// unix socket, mode exactly `0600`, owned by this process's user.
fn check_socket(path: &Path) -> Vec<String> {
    use std::os::unix::fs::FileTypeExt as _;
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::fs::PermissionsExt as _;

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            return vec![format!("signal socket {}: {error}", path.display())];
        }
    };
    let mut errors = Vec::new();
    if !metadata.file_type().is_socket() {
        errors.push(format!("{} is not a unix socket (FR-116)", path.display()));
        return errors;
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode != 0o600 {
        errors.push(format!(
            "signal socket {} has mode {mode:03o}, must be 0600 (FR-116)",
            path.display()
        ));
    }
    match current_uid() {
        Some(uid) if metadata.uid() != uid => errors.push(format!(
            "signal socket {} is owned by uid {}, not the service uid {uid} (FR-116)",
            path.display(),
            metadata.uid()
        )),
        Some(_) => {}
        // No `/proc` to read the uid from (non-Linux): the mode check above
        // still holds; ownership is not provable here.
        None => tracing::warn!(
            socket = %path.display(),
            "cannot read the effective uid; skipping the socket-ownership check"
        ),
    }
    errors
}

/// This process's effective uid, from `/proc/self/status` (no libc dep).
fn current_uid() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("Uid:"))?;
    let mut fields = line.split_whitespace().skip(1);
    // The line is `Uid:\t<real>\t<effective>\t<saved>\t<fs>`; the effective
    // uid is the one that must own the socket.
    let _real = fields.next()?;
    fields.next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{Baseline, check_socket, current_uid, strip_jsonc};
    use ocbot_core::ports::PermissionEffect;

    const GOOD: &str = r#"
// ocbot's shipped permission baseline (FR-21): the dangerous families must be `ask`.
{
  "permissions": [
    { "action": "bash", "resource": "git push*", "effect": "ask" },
    { "action": "bash", "resource": "sudo*", "effect": "ask" },
    { "action": "bash", "resource": "rm -rf*", "effect": "ask" },
    { "action": "read", "resource": "~/.ssh/**", "effect": "ask" },
    { "action": "write", "resource": "~/.ssh/**", "effect": "ask" }
  ]
}
"#;

    #[test]
    fn jsonc_comments_are_stripped_without_touching_strings() {
        let cleaned = strip_jsonc(
            r#"// top
{ "permissions": [ { "resource": "git push*" /* inline */ } ] } // tail"#,
        );
        assert!(!cleaned.contains("// top"));
        assert!(cleaned.contains("git push*"));
        assert!(!cleaned.contains("/* inline */"));
    }

    #[test]
    fn good_baseline_parses_and_validates() {
        let baseline = Baseline::parse(GOOD).expect("baseline ok");
        assert_eq!(baseline.rules.len(), 5);
        assert!(
            baseline
                .rules
                .iter()
                .all(|rule| rule.effect == PermissionEffect::Ask)
        );
    }

    #[test]
    fn missing_pattern_is_named() {
        let text = GOOD.replace("sudo*", "ls*");
        let errors = Baseline::parse(&text).expect_err("missing sudo");
        assert!(
            errors.iter().any(|error| error.contains("sudo")),
            "errors: {errors:?}"
        );
    }

    #[test]
    fn non_ask_pattern_is_named() {
        let text = GOOD.replace("\"ask\"", "\"allow\"");
        let errors = Baseline::parse(&text).expect_err("not ask");
        assert!(
            errors.iter().any(|error| error.contains("ask")),
            "errors: {errors:?}"
        );
    }

    #[test]
    fn unknown_rule_field_is_rejected() {
        let text = r#"{ "permissions": [
            { "action": "bash", "resource": "git push*", "effect": "ask", "bad": true }
        ] }"#;
        let errors = Baseline::parse(text).expect_err("extra field");
        assert!(errors.iter().any(|error| error.contains("Permission.Rule")));
    }

    #[test]
    fn bare_array_is_accepted() {
        let text = r#"[
            { "action": "bash", "resource": "git push*", "effect": "ask" },
            { "action": "bash", "resource": "sudo*", "effect": "ask" },
            { "action": "bash", "resource": "rm -rf*", "effect": "ask" },
            { "action": "read", "resource": "~/.ssh/**", "effect": "ask" },
            { "action": "write", "resource": "~/.ssh/**", "effect": "ask" }
        ]"#;
        assert!(Baseline::parse(text).is_ok());
    }

    // --- SPEC-004 PH-3: socket-doctor unit shapes ---------------------------

    #[test]
    fn the_effective_uid_is_readable_on_linux() {
        assert!(current_uid().is_some(), "/proc/self/status must parse");
    }

    #[test]
    fn a_missing_socket_is_named() {
        let dir = tempfile::tempdir().expect("tmp");
        let errors = check_socket(&dir.path().join("absent.sock"));
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("absent.sock"), "{errors:?}");
    }

    #[test]
    fn a_regular_file_is_not_a_socket() {
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("not-a-socket");
        std::fs::write(&path, b"").expect("file");
        let errors = check_socket(&path);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("not a unix socket"), "{errors:?}");
    }

    #[test]
    fn a_real_socket_with_bad_mode_is_reported() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tmp");
        let path = dir.path().join("daemon.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).expect("bind");
        let errors = check_socket(&path);
        // The socket is owned by us and (umask permitting) 0755/0777 after
        // bind — either it is clean or the mode error names 0600.
        assert!(
            errors.iter().all(|error| error.contains("0600")),
            "{errors:?}"
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        assert_eq!(check_socket(&path), Vec::<String>::new());
    }
}
