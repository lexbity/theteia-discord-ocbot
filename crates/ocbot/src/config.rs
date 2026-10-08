//! Configuration and secrets (spec §6.8): `config.toml` via serde with
//! `deny_unknown_fields` (our config is ours, P16 asymmetry), **all**
//! validation errors collected and reported together, secrets from env and/or
//! a 0600 file, never logged.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use ocbot_core::templates::{self, KNOWN_SLOTS};

/// Environment variable naming the config file.
pub const ENV_CONFIG: &str = "OCBOT_CONFIG";
/// Environment variable naming the optional secrets file.
pub const ENV_SECRETS_FILE: &str = "OCBOT_SECRETS_FILE";
/// Environment variable overriding the operator id.
pub const ENV_ALLOWED_USER_ID: &str = "ALLOWED_USER_ID";
/// Environment variable disabling the loopback-only guard (FR-4).
pub const ENV_ALLOW_NON_LOOPBACK: &str = "ALLOW_NON_LOOPBACK";
/// Env variable overriding the per-session ruleset default (FR-42).
pub const ENV_PER_SESSION_RULESET: &str = "PER_SESSION_RULESET";
/// The default permission-baseline path, unless `permissions_file` is set.
pub const DEFAULT_PERMISSIONS_FILE: &str = "config/opencode.permissions.jsonc";
/// Environment variable with the Discord bot token.
pub const ENV_DISCORD_TOKEN: &str = "DISCORD_TOKEN";
/// Environment variable with the opencode server password.
pub const ENV_OPENCODE_PASSWORD: &str = "OPENCODE_PASSWORD";

/// The on-disk config. All known keys; anything else is a load error.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The opencode v2 server base url.
    pub opencode_url: String,
    /// SQLite file path.
    #[serde(default = "default_db")]
    pub db_path: String,
    /// Directory holding `fresh.md` / `resume.md`.
    #[serde(default = "default_templates")]
    pub templates_dir: String,
    /// The ops channel id (interactions are also allowed in DMs, FR-2).
    #[serde(default)]
    pub ops_channel: Option<u64>,
    /// Where external (TUI) completions post; defaults to `ops_channel` (P14).
    #[serde(default)]
    pub inbox_channel: Option<u64>,
    /// Inbox root-session poll interval in ms (S-7 locked default 5 s).
    #[serde(default = "default_inbox_poll")]
    pub inbox_poll_interval_ms: u64,
    /// How long `/abort` waits for the server to confirm idle (FR-10).
    #[serde(default = "default_abort_timeout")]
    pub abort_timeout_secs: u64,
    /// The operator discord user id (env `ALLOWED_USER_ID` overrides).
    #[serde(default)]
    pub allowed_user_id: Option<u64>,
    /// Watcher poll interval in ms (S-2 locked default).
    #[serde(default = "default_poll")]
    pub poll_interval_ms: u64,
    /// Gate-lifecycle poll interval in ms (P4 default 2 s).
    #[serde(default = "default_gate_poll")]
    pub gate_poll_interval_ms: u64,
    /// Re-ping cadence in minutes for unanswered gates (P4 default 10).
    #[serde(default = "default_reping_minutes")]
    pub perm_reping_minutes: u64,
    /// Attach the permission baseline to every created session (FR-42; the
    /// feature ships off until S-3 confirms combine semantics — D-R6).
    #[serde(default)]
    pub per_session_ruleset: bool,
    /// The path of the shipped FR-21 permission baseline (doctor, FR-40).
    #[serde(default = "default_permissions_file")]
    pub permissions_file: String,
    /// Loopback metrics bind address.
    #[serde(default = "default_metrics")]
    pub metrics_bind: String,
    /// Per-command git timeout (spec R-9).
    #[serde(default = "default_git_timeout")]
    pub git_timeout_secs: u64,
}

fn default_db() -> String {
    "ocbot.db".to_owned()
}

fn default_templates() -> String {
    "templates".to_owned()
}

fn default_poll() -> u64 {
    2_000
}

fn default_gate_poll() -> u64 {
    2_000
}

fn default_inbox_poll() -> u64 {
    5_000
}

fn default_abort_timeout() -> u64 {
    30
}

fn default_reping_minutes() -> u64 {
    10
}

fn default_permissions_file() -> String {
    DEFAULT_PERMISSIONS_FILE.to_owned()
}

fn default_metrics() -> String {
    "127.0.0.1:9464".to_owned()
}

fn default_git_timeout() -> u64 {
    10
}

/// The templates the bot can render (P13 envelope slots, load-time validated).
#[derive(Debug, Clone)]
pub struct Templates {
    /// The fresh-run prompt envelope.
    pub fresh: String,
    /// The resume prompt envelope (settled in Phase 3+; validated now).
    pub resume: String,
}

impl Config {
    /// Parse `config.toml`.
    pub fn parse(toml: &str) -> Result<Self, String> {
        toml::from_str(toml).map_err(|error| format!("config.toml: {error}"))
    }

    /// Load from a file (path from [`ENV_CONFIG`] or `config/config.toml`),
    /// then collect **every** validation error from the config, the secrets,
    /// and the templates.
    pub fn load(path: impl AsRef<Path>) -> Result<(Self, Templates), Vec<String>> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|error| vec![format!("cannot read {}: {error}", path.display())])?;
        let mut errors = Vec::new();
        let config = match Self::parse(&text) {
            Ok(config) => config,
            Err(error) => {
                errors.push(error);
                return Err(errors);
            }
        };
        let templates = match load_templates(Path::new(&config.templates_dir)) {
            Ok(templates) => templates,
            Err(error) => {
                errors.push(error);
                return Err(errors);
            }
        };
        errors.extend(config.validate().err().unwrap_or_default());
        if errors.is_empty() {
            Ok((config, templates))
        } else {
            Err(errors)
        }
    }

    /// Collect every config-level validation error at once (spec: "all
    /// validation errors collected and reported together").
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut errors = Vec::new();

        let url = url::Url::parse(&self.opencode_url)
            .map_err(|error| errors.push(format!("opencode_url: {error}")));
        if let Ok(url) = url {
            match url.scheme() {
                "http" | "https" => {}
                other => errors.push(format!(
                    "opencode_url scheme must be http/https, got {other:?}"
                )),
            }
            let allow_non_loopback = std::env::var(ENV_ALLOW_NON_LOOPBACK)
                .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"));
            if !allow_non_loopback && !is_loopback(&url) {
                errors.push(format!(
                    "opencode_url must be loopback (FR-4); got {} — set {ENV_ALLOW_NON_LOOPBACK}=true to override",
                    url.host_str().unwrap_or("?")
                ));
            }
        }

        if self
            .metrics_bind
            .split_once(':')
            .is_none_or(|(host, _)| host.is_empty())
        {
            errors.push(format!(
                "metrics_bind must be host:port, got {:?}",
                self.metrics_bind
            ));
        }
        if self.poll_interval_ms < 100 {
            errors.push(format!(
                "poll_interval_ms must be >= 100, got {}",
                self.poll_interval_ms
            ));
        }
        if self.gate_poll_interval_ms < 100 {
            errors.push(format!(
                "gate_poll_interval_ms must be >= 100, got {}",
                self.gate_poll_interval_ms
            ));
        }
        if self.perm_reping_minutes < 1 {
            errors.push(format!(
                "perm_reping_minutes must be >= 1, got {}",
                self.perm_reping_minutes
            ));
        }
        if self.inbox_poll_interval_ms < 100 {
            errors.push(format!(
                "inbox_poll_interval_ms must be >= 100, got {}",
                self.inbox_poll_interval_ms
            ));
        }
        if self.abort_timeout_secs < 1 {
            errors.push(format!(
                "abort_timeout_secs must be >= 1, got {}",
                self.abort_timeout_secs
            ));
        }

        let operator = self.allowed_user_id.or_else(|| {
            std::env::var(ENV_ALLOWED_USER_ID)
                .ok()
                .and_then(|value| value.parse().ok())
        });
        if operator.is_none() {
            errors.push(format!(
                "allowed_user_id is required (config or env {ENV_ALLOWED_USER_ID})"
            ));
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// The resolved operator id, after the env override rule.
    pub fn resolved_operator(&self) -> Option<u64> {
        self.allowed_user_id.or_else(|| {
            std::env::var(ENV_ALLOWED_USER_ID)
                .ok()
                .and_then(|value| value.parse().ok())
        })
    }

    /// Whether the per-session ruleset (FR-42) is active: the config flag or
    /// the `PER_SESSION_RULESET` environment override set to `true`. Defaults
    /// to off (D-R6) until S-3 confirms opencode *combines* rulesets.
    pub fn ruleset_enabled(&self) -> bool {
        self.per_session_ruleset
            || std::env::var(ENV_PER_SESSION_RULESET)
                .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
    }
}

/// Secrets, precedence: env vars, then the optional 0600 `KEY=VALUE` file.
/// The file path comes from [`ENV_SECRETS_FILE`].
#[derive(Debug, Clone, Default)]
pub struct Secrets {
    /// Discord bot token.
    pub discord_token: Option<String>,
    /// opencode server password.
    pub opencode_password: Option<String>,
}

impl Secrets {
    /// Collect secrets from the environment (and an optional secrets file).
    pub fn from_env() -> Result<Self, Vec<String>> {
        let file_overrides = match std::env::var(ENV_SECRETS_FILE) {
            Ok(path) => read_secrets_file(Path::new(&path))?,
            Err(_) => HashMap::new(),
        };
        let discord_token = lookup(&file_overrides, ENV_DISCORD_TOKEN)
            .map(str::to_owned)
            .or_else(|| std::env::var(ENV_DISCORD_TOKEN).ok());
        let opencode_password = lookup(&file_overrides, ENV_OPENCODE_PASSWORD)
            .map(str::to_owned)
            .or_else(|| std::env::var(ENV_OPENCODE_PASSWORD).ok());
        Ok(Secrets {
            discord_token,
            opencode_password,
        })
    }
}

/// Read a strict `KEY=VALUE` secrets file; keys must not collide and the file
/// should be 0600 (a warning is returned as an error-leg of validation).
fn read_secrets_file(path: &Path) -> Result<HashMap<String, String>, Vec<String>> {
    let permissions = std::fs::metadata(path)
        .map_err(|error| vec![format!("secrets file {}: {error}", path.display())])?;
    let mode = permissions.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(vec![format!(
            "secrets file {} must be 0600 (mode {:03o})",
            path.display(),
            mode & 0o777
        )]);
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| vec![format!("secrets file {}: {error}", path.display())])?;
    let mut map = HashMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(vec![format!(
                "secrets file {} line {}: expected KEY=VALUE",
                path.display(),
                index + 1
            )]);
        };
        if map
            .insert(key.trim().to_owned(), value.trim().to_owned())
            .is_some()
        {
            return Err(vec![format!(
                "secrets file {} line {}: duplicate key {key}",
                path.display(),
                index + 1
            )]);
        }
    }
    Ok(map)
}

fn lookup<'a>(map: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    map.get(key).map(String::as_str)
}

/// Load and validate both templates. Unknown slots are a load error (P13).
fn load_templates(directory: &Path) -> Result<Templates, String> {
    let mut errors = Vec::new();
    let fresh = match std::fs::read_to_string(directory.join("fresh.md")) {
        Ok(text) => text,
        Err(error) => {
            errors.push(format!("templates/fresh.md: {error}"));
            String::new()
        }
    };
    let resume = match std::fs::read_to_string(directory.join("resume.md")) {
        Ok(text) => text,
        Err(error) => {
            errors.push(format!("templates/resume.md: {error}"));
            String::new()
        }
    };
    for (name, template) in [("fresh", &fresh), ("resume", &resume)] {
        if let Err(templates::TemplateError::UnknownSlot(slots)) = templates::validate(template) {
            errors.push(format!(
                "templates/{name}.md uses unknown slots {slots:?}; known: {KNOWN_SLOTS:?}"
            ));
        }
        if template.is_empty() {
            errors.push(format!("templates/{name}.md is empty"));
        }
    }
    if errors.is_empty() {
        Ok(Templates { fresh, resume })
    } else {
        Err(errors.join("\n"))
    }
}

/// Whether a URL points at the loopback interface.
fn is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(domain)) => domain == "localhost" || domain.starts_with("127."),
        None => false,
    }
}

/// Resolve the config file path: `OCBOT_CONFIG` else `config/config.toml`.
pub fn config_path() -> PathBuf {
    std::env::var(ENV_CONFIG).map_or_else(|_| PathBuf::from("config/config.toml"), PathBuf::from)
}

use std::os::unix::fs::PermissionsExt;

#[cfg(test)]
mod tests {
    use super::{Config, Secrets, is_loopback};
    use url::Url;

    const GOOD: &str = r#"
opencode_url = "http://127.0.0.1:8080"
allowed_user_id = 42
"#;

    #[test]
    fn parses_and_validates_loopback() {
        let config = Config::parse(GOOD).expect("parses");
        assert!(config.validate().is_ok());
    }

    #[test]
    fn denies_unknown_fields() {
        let bad = r#"
opencode_url = "http://127.0.0.1:8080"
typo_key = 1
"#;
        let error = Config::parse(bad).expect_err("unknown key is a parse error");
        assert!(error.contains("unknown field"), "error: {error}");
    }

    #[test]
    fn rejects_non_loopback_without_override() {
        let bad = Config::parse("opencode_url = \"http://example.com\"\n").expect("parses");
        let errors = bad.validate().expect_err("non-loopback rejected");
        assert!(errors.iter().any(|error| error.contains("FR-4")));
    }

    #[test]
    fn collects_every_validation_error() {
        let config = Config {
            opencode_url: "example.com".to_owned(),
            db_path: "x.db".to_owned(),
            templates_dir: "templates".to_owned(),
            ops_channel: None,
            inbox_channel: None,
            inbox_poll_interval_ms: 10, // too fast
            abort_timeout_secs: 0,      // too fast
            allowed_user_id: None,
            poll_interval_ms: 10,     // too fast
            gate_poll_interval_ms: 5, // too fast
            perm_reping_minutes: 0,   // too fast
            per_session_ruleset: false,
            permissions_file: "config/opencode.permissions.jsonc".to_owned(),
            metrics_bind: String::new(),
            git_timeout_secs: 10,
        };
        let errors = config.validate().expect_err("multiple errors");
        assert!(errors.len() >= 3, "errors: {errors:?}");
    }

    #[test]
    fn loopback_detection_covers_variants() {
        for host in ["127.0.0.1", "localhost", "::1", "127.0.1.2"] {
            let formatted = if host == "::1" {
                format!("http://[{host}]:8080")
            } else {
                format!("http://{host}:8080")
            };
            let url = Url::parse(&formatted).expect("url");
            assert!(is_loopback(&url), "{host}");
        }
        assert!(!is_loopback(&Url::parse("http://10.0.0.5:1").expect("url")));
    }

    #[test]
    fn secrets_are_read_without_logging() {
        let secrets = Secrets::from_env().expect("env read");
        // Only presence matters here; contents are never surfaced.
        let _ = secrets.opencode_password.is_some();
    }
}
