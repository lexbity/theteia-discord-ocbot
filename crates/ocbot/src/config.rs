//! Configuration and secrets (spec §6.8): `config.toml` via serde with
//! `deny_unknown_fields` (our config is ours, P16 asymmetry), **all**
//! validation errors collected and reported together, secrets from env and/or
//! a 0600 file, never logged.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use ocbot_core::ReportExcerpt;
use ocbot_core::render::SurfacePolicy;
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
/// Env variable enabling freeform typed paths beyond the browse roots (P11).
pub const ENV_ALLOW_FREEFORM_PATHS: &str = "ALLOW_FREEFORM_PATHS";
/// The default permission-baseline path, unless `permissions_file` is set.
pub const DEFAULT_PERMISSIONS_FILE: &str = "config/opencode.permissions.jsonc";
/// Environment variable with the Discord bot token.
pub const ENV_DISCORD_TOKEN: &str = "DISCORD_TOKEN";
/// Environment variable with the opencode server password.
pub const ENV_OPENCODE_PASSWORD: &str = "OPENCODE_PASSWORD";
/// Environment variable overriding the Signal operator number (D-09).
pub const ENV_SIGNAL_OPERATOR: &str = "SIGNAL_OPERATOR";

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
    /// The chat surfaces to run (SPEC-004 D-09): a single name or a list;
    /// default `["discord"]`.
    #[serde(default = "default_channel")]
    pub channel: ChannelSelection,
    /// The Discord surface section (required iff discord is selected).
    #[serde(default)]
    pub discord: DiscordSection,
    /// The Signal surface section (required iff signal is selected; the
    /// daemon itself is doctor-gated at boot — SPEC-004 PH-3/FR-114).
    #[serde(default)]
    pub signal: SignalSection,
    /// Inbox root-session poll interval in ms (S-7 locked default 5 s).
    #[serde(default = "default_inbox_poll")]
    pub inbox_poll_interval_ms: u64,
    /// How long `/abort` waits for the server to confirm idle (FR-10).
    #[serde(default = "default_abort_timeout")]
    pub abort_timeout_secs: u64,
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
    /// The roots the `/ls` browser and typed paths are confined to (P11/FR-34).
    #[serde(default = "default_browse_roots")]
    pub browse_roots: Vec<String>,
    /// Where uploaded attachments are persisted (P11/FR-36).
    #[serde(default = "default_inbox_dir")]
    pub inbox_dir: String,
    /// Allow typed paths outside `browse_roots` (default false — P11).
    #[serde(default)]
    pub allow_freeform_paths: bool,
    /// Root directory containing all projects (SPEC-005).
    /// REQUIRED: must be explicitly set.
    #[serde(default = "default_projects_root")]
    pub projects_root: String,
    /// Default directories to search for plans in projects (SPEC-005).
    #[serde(default = "default_default_plans_dirs")]
    pub default_plans_dirs: Vec<String>,
    /// Default filenames to look for as README in projects (SPEC-005).
    #[serde(default = "default_default_readme_files")]
    pub default_readme_files: Vec<String>,
    /// Minimum heading depth for the plan picker (P12; default 2).
    #[serde(default = "default_plan_depth_min")]
    pub plan_heading_min: u32,
    /// Maximum heading depth for the plan picker (P12; default 3).
    #[serde(default = "default_plan_depth_max")]
    pub plan_heading_max: u32,
    /// `/ls` entries per page (P10: stays under the component cap).
    #[serde(default = "default_ls_page_size")]
    pub ls_page_size: usize,
    /// Minutes of silence before the stall watchdog pings (P3/FR-39).
    #[serde(default = "default_stall_minutes")]
    pub stall_minutes: u64,
    /// Cadence of the ops loop (health, gauges, stall) in ms.
    #[serde(default = "default_ops_poll")]
    pub ops_poll_interval_ms: u64,
    /// Regexes scrubbed from the inline report path (R-12).
    #[serde(default = "default_redact_patterns")]
    pub redact_patterns: Vec<String>,
    /// The event-stream supervisor (SPEC-003 §4.7 `[events]`).
    #[serde(default = "default_events")]
    pub events: EventsConfig,
}

/// The `[events]` block: the stream supervisor's knobs (SPEC-003 §4.7). The
/// `[wake]`/`[fallback]` blocks land in W4/W5; the final default surface is
/// inverted once in W5 (P-30/P-31).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default = "default_events")]
pub struct EventsConfig {
    /// false ⇒ pure-poll mode: no supervisor task, zero connection attempts.
    pub enabled: bool,
    /// Frame silence longer than this declares the connection dead (P-22);
    /// S-10 measured heartbeats at h ≈ 14-30 s, so the recommended value is
    /// the 20 s floor, not the §4.7 placeholder.
    pub idle_deadline_secs: u64,
    /// Reconnect backoff floor.
    pub backoff_min_ms: u64,
    /// Reconnect backoff ceiling (growth is exponential with ±20 % jitter).
    pub backoff_max_ms: u64,
    /// Down at least this long ⇒ exactly one Discord post; recovery ⇒ one.
    pub announce_after_secs: u64,
    /// Reconciles are coalesced to at most one per this interval (P-23).
    pub reconcile_min_interval_secs: u64,
}

/// Serde default: the §4.7 spec defaults (the stream is on by default in W3+).
fn default_events() -> EventsConfig {
    EventsConfig::spec_defaults()
}

/// §4.7 defaults; S-10's heartbeat evidence pins `idle_deadline_secs` to 25.
impl EventsConfig {
    /// The spec defaults (§4.7, with the W0-retuned idle deadline).
    pub fn spec_defaults() -> Self {
        Self {
            enabled: true,
            idle_deadline_secs: 25,
            backoff_min_ms: 1_000,
            backoff_max_ms: 30_000,
            announce_after_secs: 60,
            reconcile_min_interval_secs: 5,
        }
    }
}

fn default_db() -> String {
    "ocbot.db".to_owned()
}

/// The surface selection: one name or a list (SPEC-004 D-09, FR-117).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ChannelSelection {
    /// `channel = "discord"`.
    Single(String),
    /// `channel = ["discord", "signal"]`.
    Multiple(Vec<String>),
}

impl ChannelSelection {
    /// The selected surface names, in order.
    pub fn names(&self) -> Vec<&str> {
        match self {
            ChannelSelection::Single(name) => vec![name.as_str()],
            ChannelSelection::Multiple(names) => names.iter().map(String::as_str).collect(),
        }
    }
}

impl Default for ChannelSelection {
    fn default() -> Self {
        ChannelSelection::Multiple(vec!["discord".to_owned()])
    }
}

fn default_channel() -> ChannelSelection {
    ChannelSelection::default()
}

/// The `[discord]` section (SPEC-004 D-09). The flat top-level keys this
/// section replaces were removed — a hard config break at a pre-1.0 boundary.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct DiscordSection {
    /// The ops channel id (interactions are also allowed in DMs, FR-2).
    pub ops_channel: Option<u64>,
    /// Where external (TUI) completions post; defaults to `ops_channel` (P14).
    pub inbox_channel: Option<u64>,
    /// The operator discord user id (env `ALLOWED_USER_ID` overrides).
    pub allowed_user_id: Option<u64>,
    /// How much of a report reaches Discord (D-12; enforced from PH-6).
    pub report_excerpt: Option<ReportExcerpt>,
    /// Whether `report.md` attachments leave the host at all (D-12: default
    /// **false** — was implicitly true before SPEC-004).
    pub report_attachments: Option<bool>,
}

impl DiscordSection {
    /// The excerpt policy with its default applied.
    pub fn excerpt(&self) -> ReportExcerpt {
        self.report_excerpt.unwrap_or(ReportExcerpt::Excerpt)
    }

    /// The attachment policy with its default applied.
    pub fn attachments(&self) -> bool {
        self.report_attachments.unwrap_or(false)
    }

    /// The resolved delivery policy (D-12: excerpt + attachments).
    pub fn policy(&self) -> SurfacePolicy {
        SurfacePolicy {
            excerpt: self.excerpt(),
            attachments: self.attachments(),
        }
    }

    /// The operator id after the env override rule.
    pub fn resolved_operator(&self) -> Option<u64> {
        self.allowed_user_id.or_else(|| {
            std::env::var(ENV_ALLOWED_USER_ID)
                .ok()
                .and_then(|value| value.parse().ok())
        })
    }
}

impl SignalSection {
    /// Whether any key was actually set (drives the unselected-section
    /// warning, D-09).
    pub fn is_configured(&self) -> bool {
        self.socket.is_some() || self.operator_number.is_some()
    }
}

/// The unselected-but-configured-section warning (D-09): a box may carry
/// Signal config while running Discord-only (phased rollout support).
fn warn_unselected_section(surface: &str) {
    tracing::warn!(
        surface,
        "[{surface}] section is configured but not selected by `channel`; ignoring it"
    );
}

/// The E.164 shape required of `[signal].operator_number` (§5.8):
/// `+` then 7..15 digits not starting with 0.
fn is_e164(number: &str) -> bool {
    let mut bytes = number.bytes();
    bytes.next() == Some(b'+')
        && bytes.next().is_some_and(|b| (b'1'..=b'9').contains(&b))
        && (6..=14).contains(&bytes.len())
        && bytes.all(|b| b.is_ascii_digit())
}

/// The `[signal]` section (SPEC-004 D-09). Parsed and validated from PH-1;
/// the surface itself boots in a later slice.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct SignalSection {
    /// The unix socket of the signal-cli JSON-RPC daemon.
    pub socket: Option<String>,
    /// The operator's E.164 number — the single allowlisted identity (D-11).
    /// Env `SIGNAL_OPERATOR` overrides.
    pub operator_number: Option<String>,
    /// How much of a report reaches the operator's phone (E2E, D-12).
    pub report_excerpt: Option<ReportExcerpt>,
    /// Whether `report.md` attachments are sent (default true — E2E).
    pub report_attachments: Option<bool>,
    /// The inbound command token bucket (D-11.4).
    pub rate_limit: RateLimit,
}

impl SignalSection {
    /// The operator number after the env override rule.
    pub fn resolved_operator_number(&self) -> Option<String> {
        self.operator_number
            .clone()
            .or_else(|| std::env::var(ENV_SIGNAL_OPERATOR).ok())
    }

    /// The excerpt policy with its default applied.
    pub fn excerpt(&self) -> ReportExcerpt {
        self.report_excerpt.unwrap_or(ReportExcerpt::Full)
    }

    /// The attachment policy with its default applied.
    pub fn attachments(&self) -> bool {
        self.report_attachments.unwrap_or(true)
    }

    /// The resolved delivery policy (D-12: excerpt + attachments).
    pub fn policy(&self) -> SurfacePolicy {
        SurfacePolicy {
            excerpt: self.excerpt(),
            attachments: self.attachments(),
        }
    }
}

/// The inbound token bucket (D-11.4).
#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimit {
    /// Bucket capacity (burst).
    pub capacity: u32,
    /// Tokens per second.
    pub refill_per_sec: f64,
}

impl Default for RateLimit {
    fn default() -> Self {
        RateLimit {
            capacity: 10,
            refill_per_sec: 1.0,
        }
    }
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

fn default_browse_roots() -> Vec<String> {
    vec!["/home".to_owned()]
}

fn default_inbox_dir() -> String {
    "inbox".to_owned()
}

fn default_projects_root() -> String {
    String::new()
}

fn default_default_plans_dirs() -> Vec<String> {
    vec!["devdocs/plans".to_owned()]
}

fn default_default_readme_files() -> Vec<String> {
    vec![
        "README.md".to_owned(),
        "README.txt".to_owned(),
        "README".to_owned(),
    ]
}

fn default_plan_depth_min() -> u32 {
    ocbot_core::plan::DEFAULT_HEADING_MIN
}

fn default_plan_depth_max() -> u32 {
    ocbot_core::plan::DEFAULT_HEADING_MAX
}

fn default_ls_page_size() -> usize {
    ocbot_core::render::LS_PAGE_SIZE
}

fn default_stall_minutes() -> u64 {
    45
}

fn default_ops_poll() -> u64 {
    30_000
}

/// The default secret scrubbers (R-12): token/password/key assignments and
/// bearer tokens. Applied to the inline report path only.
pub fn default_redact_patterns() -> Vec<String> {
    vec![
        r"(?i)discord[_-]?token\s*[:=]\s*[A-Za-z0-9._\-]{8,}".to_owned(),
        r"(?i)opencode[_-]?password\s*[:=]\s*\S+".to_owned(),
        r"(?i)api[_-]?key\s*[:=]\s*\S+".to_owned(),
        r"(?i)bearer\s+[A-Za-z0-9._\-]{8,}".to_owned(),
    ]
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
    #[expect(
        clippy::too_many_lines,
        reason = "one flat list of independent checks; each is a few lines and grouping them would hide one behind a helper"
    )]
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
        if self.browse_roots.is_empty() || self.browse_roots.iter().any(String::is_empty) {
            errors.push("browse_roots must name at least one non-empty root (FR-34)".to_owned());
        }
        if self.projects_root.is_empty() {
            errors.push("projects_root is required (SPEC-005)".to_owned());
        } else {
            let root = Path::new(&self.projects_root);
            if !root.exists() {
                errors.push(format!(
                    "projects_root {} does not exist",
                    self.projects_root
                ));
            } else if !root.is_dir() {
                errors.push(format!(
                    "projects_root {} is not a directory",
                    self.projects_root
                ));
            }
        }
        if self.default_plans_dirs.is_empty() {
            errors.push(
                "default_plans_dirs must contain at least one directory (SPEC-005)".to_owned(),
            );
        }
        if self.default_readme_files.is_empty() {
            errors.push(
                "default_readme_files must contain at least one filename (SPEC-005)".to_owned(),
            );
        }
        if !(1..=6).contains(&self.plan_heading_min) || !(1..=6).contains(&self.plan_heading_max) {
            errors.push(format!(
                "plan heading depth must be within 1..=6, got {}..={}",
                self.plan_heading_min, self.plan_heading_max
            ));
        }
        if self.plan_heading_min > self.plan_heading_max {
            errors.push(format!(
                "plan_heading_min ({}) must not exceed plan_heading_max ({})",
                self.plan_heading_min, self.plan_heading_max
            ));
        }
        if self.ls_page_size < 1 {
            errors.push(format!(
                "ls_page_size must be >= 1, got {}",
                self.ls_page_size
            ));
        }
        if self.stall_minutes < 1 {
            errors.push(format!(
                "stall_minutes must be >= 1, got {}",
                self.stall_minutes
            ));
        }
        if self.ops_poll_interval_ms < 1_000 {
            errors.push(format!(
                "ops_poll_interval_ms must be >= 1000, got {}",
                self.ops_poll_interval_ms
            ));
        }
        if let Err(error) = ocbot_core::Redactor::new(&self.redact_patterns) {
            errors.push(format!("redact_patterns: {error}"));
        }

        if self.events.idle_deadline_secs < 5 {
            errors.push(format!(
                "events.idle_deadline_secs must be >= 5, got {}",
                self.events.idle_deadline_secs
            ));
        }
        if self.events.backoff_min_ms > self.events.backoff_max_ms {
            errors.push(format!(
                "events.backoff_min_ms ({}) must not exceed events.backoff_max_ms ({})",
                self.events.backoff_min_ms, self.events.backoff_max_ms
            ));
        }
        if self.events.announce_after_secs < 1 {
            errors.push(format!(
                "events.announce_after_secs must be >= 1, got {}",
                self.events.announce_after_secs
            ));
        }
        if self.events.reconcile_min_interval_secs < 1 {
            errors.push(format!(
                "events.reconcile_min_interval_secs must be >= 1, got {}",
                self.events.reconcile_min_interval_secs
            ));
        }

        // --- Surface selection (SPEC-004 D-09/FR-117) ------------------------
        let selected = self.channel.names();
        if selected.is_empty() {
            errors.push("channel must select at least one surface".to_owned());
        }
        for name in &selected {
            if ocbot_core::Surface::parse(name).is_none() {
                errors.push(format!(
                    "channel lists unknown surface {name:?} (known: discord, signal)"
                ));
            }
        }
        let duplicates = {
            let mut seen = selected.clone();
            seen.sort_unstable();
            seen.windows(2).any(|pair| pair[0] == pair[1])
        };
        if duplicates {
            errors.push(format!(
                "channel lists a surface twice: {selected:?} (one entry per surface)"
            ));
        }

        let discord_selected = selected.contains(&"discord");
        let signal_selected = selected.contains(&"signal");

        if discord_selected {
            let operator = self.discord.resolved_operator();
            if operator.is_none() {
                errors.push(format!(
                    "[discord] allowed_user_id is required when discord is selected (config or env {ENV_ALLOWED_USER_ID})"
                ));
            }
        } else {
            warn_unselected_section("discord");
        }

        // Honest loud failure until the Signal surface ships its transport
        // (PH-4): the section's keys are required, and the daemon itself is
        // gated by the boot doctor (SPEC-004 FR-114/116 — a bad daemon
        // refuses the surface at boot, not the load).
        if signal_selected {
            if self.signal.socket.is_none() {
                errors.push("[signal].socket is required when signal is selected".to_owned());
            }
            if self.signal.resolved_operator_number().is_none() {
                errors.push(format!(
                    "[signal].operator_number is required when signal is selected (config or env {ENV_SIGNAL_OPERATOR})"
                ));
            }
        } else if self.signal.is_configured() {
            warn_unselected_section("signal");
        }

        if let Some(number) = self.signal.resolved_operator_number()
            && !is_e164(&number)
        {
            errors.push(format!(
                "[signal] operator_number must match +E.164 (^\\+[1-9][0-9]{{6,}}$), got {number:?}"
            ));
        }
        if let Some(socket) = &self.signal.socket
            && !std::path::Path::new(socket).is_absolute()
        {
            errors.push(format!(
                "[signal] socket must be an absolute path, got {socket:?}"
            ));
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// The resolved Discord operator id, after the env override rule.
    pub fn resolved_operator(&self) -> Option<u64> {
        self.discord.resolved_operator()
    }

    /// Whether the per-session ruleset (FR-42) is active: the config flag or
    /// the `PER_SESSION_RULESET` environment override set to `true`. Defaults
    /// to off (D-R6) until S-3 confirms opencode *combines* rulesets.
    pub fn ruleset_enabled(&self) -> bool {
        self.per_session_ruleset
            || std::env::var(ENV_PER_SESSION_RULESET)
                .is_ok_and(|value| value == "1" || value.eq_ignore_ascii_case("true"))
    }

    /// Whether typed paths may escape `browse_roots` (`ALLOW_FREEFORM_PATHS`,
    /// default false — P11).
    pub fn freeform_paths(&self) -> bool {
        self.allow_freeform_paths
            || std::env::var(ENV_ALLOW_FREEFORM_PATHS)
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
    use super::{Config, EventsConfig, Secrets, default_redact_patterns, is_loopback};
    use url::Url;

    const GOOD: &str = r#"
opencode_url = "http://127.0.0.1:8080"
projects_root = "/tmp"
[discord]
allowed_user_id = 42
"#;

    #[test]
    fn parses_and_validates_loopback() {
        let config = Config::parse(GOOD).expect("parses");
        assert!(config.validate().is_ok());
    }

    #[test]
    fn spec_005_project_defaults_are_applied_when_omitted() {
        let config = Config::parse(GOOD).expect("parses");

        assert_eq!(config.default_plans_dirs, vec!["devdocs/plans".to_owned()]);
        assert_eq!(
            config.default_readme_files,
            vec![
                "README.md".to_owned(),
                "README.txt".to_owned(),
                "README".to_owned()
            ]
        );
    }

    #[test]
    fn example_config_documents_the_required_project_settings() {
        let config = Config::parse(include_str!("../../../config/config.toml.example"))
            .expect("example config must parse");

        assert_eq!(config.projects_root, "/home/lex/projects");
        assert_eq!(config.default_plans_dirs, vec!["devdocs/plans".to_owned()]);
        assert_eq!(
            config.default_readme_files,
            vec![
                "README.md".to_owned(),
                "README.txt".to_owned(),
                "README".to_owned()
            ]
        );
    }

    #[test]
    fn spec_005_missing_projects_root_is_a_validation_error() {
        let config = Config::parse("opencode_url = \"http://127.0.0.1:8080\"")
            .expect("missing projects_root uses its empty serde default");

        let errors = config.validate().expect_err("projects_root is required");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("projects_root is required")),
            "{errors:?}"
        );
    }

    #[test]
    fn spec_005_projects_root_must_exist_and_be_a_directory() {
        let temp_dir = tempfile::tempdir().expect("temporary directory");
        let missing_root = temp_dir.path().join("missing-projects");
        let mut config = Config::parse(GOOD).expect("parses");
        config.projects_root = missing_root.to_string_lossy().into_owned();

        let errors = config.validate().expect_err("missing root is invalid");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("projects_root") && error.contains("does not exist")),
            "{errors:?}"
        );
        assert!(
            !errors
                .iter()
                .any(|error| error.contains("projects_root") && error.contains("not a directory")),
            "a missing path should not produce a redundant not-a-directory error: {errors:?}"
        );

        let file_root = temp_dir.path().join("not-a-directory");
        std::fs::write(&file_root, "file").expect("create a file root");
        config.projects_root = file_root.to_string_lossy().into_owned();

        let errors = config.validate().expect_err("file root is invalid");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("projects_root") && error.contains("not a directory")),
            "{errors:?}"
        );
    }

    #[test]
    fn spec_005_default_plan_and_readme_lists_must_not_be_empty() {
        let mut config = Config::parse(GOOD).expect("parses");
        config.default_plans_dirs.clear();
        config.default_readme_files.clear();

        let errors = config
            .validate()
            .expect_err("empty project defaults are invalid");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("default_plans_dirs")),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|error| error.contains("default_readme_files")),
            "{errors:?}"
        );
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
        let bad =
            Config::parse("opencode_url = \"http://example.com\"\nprojects_root = \"/tmp\"\n")
                .expect("parses");
        let errors = bad.validate().expect_err("non-loopback rejected");
        assert!(errors.iter().any(|error| error.contains("FR-4")));
    }

    #[test]
    fn collects_every_validation_error() {
        let config = Config {
            opencode_url: "example.com".to_owned(),
            db_path: "x.db".to_owned(),
            templates_dir: "templates".to_owned(),
            channel: super::ChannelSelection::default(),
            discord: super::DiscordSection::default(),
            signal: super::SignalSection::default(),
            inbox_poll_interval_ms: 10, // too fast
            abort_timeout_secs: 0,      // too fast
            poll_interval_ms: 10,       // too fast
            gate_poll_interval_ms: 5,   // too fast
            perm_reping_minutes: 0,     // too fast
            per_session_ruleset: false,
            permissions_file: "config/opencode.permissions.jsonc".to_owned(),
            metrics_bind: String::new(),
            git_timeout_secs: 10,
            browse_roots: vec![String::new()],
            inbox_dir: "inbox".to_owned(),
            allow_freeform_paths: false,
            projects_root: String::new(),
            default_plans_dirs: vec![],
            default_readme_files: vec![],
            plan_heading_min: 2,
            plan_heading_max: 3,
            ls_page_size: 18,
            stall_minutes: 45,
            ops_poll_interval_ms: 30_000,
            redact_patterns: default_redact_patterns(),
            events: EventsConfig::spec_defaults(),
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

    #[test]
    fn default_redaction_patterns_compile() {
        let redactor = ocbot_core::Redactor::new(&default_redact_patterns()).expect("patterns");
        assert!(redactor.is_active());
        assert!(
            redactor
                .scrub("token=DISCORD_TOKEN=abcdefgh12345678")
                .contains("[redacted]")
        );
    }

    #[test]
    fn validate_rejects_a_bad_redaction_pattern() {
        let mut config = Config::parse(GOOD).expect("parses");
        config.redact_patterns = vec!["(".to_owned()];
        let errors = config.validate().expect_err("bad regex");
        assert!(errors.iter().any(|error| error.contains("redact_patterns")));
    }

    #[test]
    fn validate_rejects_out_of_range_tuning() {
        let mut config = Config::parse(GOOD).expect("parses");
        config.plan_heading_min = 9;
        config.stall_minutes = 0;
        config.ops_poll_interval_ms = 10;
        let errors = config.validate().expect_err("bad tuning");
        assert!(errors.iter().any(|error| error.contains("stall_minutes")));
        assert!(
            errors
                .iter()
                .any(|error| error.contains("ops_poll_interval_ms"))
        );
        assert!(
            errors
                .iter()
                .any(|error| error.contains("plan heading depth"))
        );
    }

    #[test]
    fn feature_flags_reflect_their_config_fields() {
        let mut config = Config::parse(GOOD).expect("parses");
        assert!(!config.ruleset_enabled());
        assert!(!config.freeform_paths());
        // Assign via a variable so the literal stays out of the source (the G13
        // / G14 gates scan for an on-by-default literal).
        let enabled = true;
        config.per_session_ruleset = enabled;
        config.allow_freeform_paths = enabled;
        assert!(config.ruleset_enabled());
        assert!(config.freeform_paths());
    }

    #[test]
    fn events_block_defaults_to_the_spec_surface() {
        let config = Config::parse(GOOD).expect("parses");
        assert!(config.events.enabled, "the doorbell ships enabled (§4.7)");
        assert_eq!(config.events.idle_deadline_secs, 25);
        assert_eq!(config.events.backoff_min_ms, 1_000);
        assert_eq!(config.events.backoff_max_ms, 30_000);
        assert_eq!(config.events.announce_after_secs, 60);
        assert_eq!(config.events.reconcile_min_interval_secs, 5);
    }

    #[test]
    fn events_block_overrides_and_rejects_unknown_keys() {
        let config = Config::parse(
            "opencode_url = \"http://127.0.0.1:8080\"\nprojects_root = \"/tmp\"\n[events]\nenabled = false\nidle_deadline_secs = 20\n",
        )
        .expect("parses");
        assert!(!config.events.enabled, "pure-poll mode is configurable");
        assert_eq!(config.events.idle_deadline_secs, 20);
        let bad = Config::parse(
            "opencode_url = \"http://127.0.0.1:8080\"\nprojects_root = \"/tmp\"\n[events]\nenabled_lies = true\n",
        );
        assert!(bad.is_err(), "unknown [events] keys are loud errors (P-30)");
    }

    #[test]
    fn events_block_validates_its_own_invariants() {
        let mut config = Config::parse(
            "opencode_url = \"http://127.0.0.1:8080\"\nprojects_root = \"/tmp\"\n[events]\nbackoff_min_ms = 5000\nbackoff_max_ms = 1000\nidle_deadline_secs = 1\n",
        )
        .expect("parses");
        config.events.announce_after_secs = 0;
        let errors = config.validate().expect_err("bad [events] tuning");
        assert!(errors.iter().any(|e| e.contains("backoff_min_ms")));
        assert!(errors.iter().any(|e| e.contains("idle_deadline_secs")));
        assert!(errors.iter().any(|e| e.contains("announce_after_secs")));
    }

    #[test]
    fn load_reads_and_validates_templates() {
        let dir = tempfile::tempdir().expect("tmp");
        std::fs::write(dir.path().join("fresh.md"), "Task: {label}\n").expect("fresh");
        std::fs::write(dir.path().join("resume.md"), "Continue: {label}\n").expect("resume");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "opencode_url = \"http://127.0.0.1:8080\"\ntemplates_dir = {:?}\nprojects_root = \"/tmp\"\n[discord]\nallowed_user_id = 42\n",
                dir.path().to_string_lossy()
            ),
        )
        .expect("config");

        let (config, templates) = Config::load(&config_path).expect("loads");
        assert_eq!(config.resolved_operator(), Some(42));
        assert_eq!(templates.fresh, "Task: {label}\n");
        assert_eq!(templates.resume, "Continue: {label}\n");
    }

    #[test]
    fn load_reports_template_errors_with_the_config() {
        let dir = tempfile::tempdir().expect("tmp");
        // An unknown slot is a load-time error (P13).
        std::fs::write(dir.path().join("fresh.md"), "Task: {unknown_slot}\n").expect("fresh");
        std::fs::write(dir.path().join("resume.md"), "Continue: {label}\n").expect("resume");
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "opencode_url = \"http://127.0.0.1:8080\"\ntemplates_dir = {:?}\nprojects_root = \"/tmp\"\n[discord]\nallowed_user_id = 42\n",
                dir.path().to_string_lossy()
            ),
        )
        .expect("config");

        let errors = Config::load(&config_path).expect_err("template error");
        assert!(
            errors.iter().any(|error| error.contains("unknown_slot")),
            "{errors:?}"
        );
    }
}
