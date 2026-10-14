//! Surface-selection config tests (SPEC-004 D-09/FR-117, PH-1): the nested
//! `[discord]`/`[signal]` shape, single-or-list `channel`, collected
//! validation, and the honest loud refusal of the not-yet-implemented Signal
//! surface.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration tests assert by unwrapping (C6)"
)]

use ocbot::config::{ChannelSelection, Config};

const BASE: &str = "opencode_url = \"http://127.0.0.1:8080\"\n";

fn parse(extra: &str) -> Config {
    Config::parse(&format!("{BASE}{extra}")).expect("parses")
}

#[test]
fn channel_accepts_single_and_list_forms() {
    assert_eq!(
        parse("[discord]\nallowed_user_id = 42\n").channel,
        ChannelSelection::default(),
        "omitted channel defaults to [\"discord\"]"
    );
    assert_eq!(
        parse("channel = \"discord\"\n[discord]\nallowed_user_id = 42\n").channel,
        ChannelSelection::Single("discord".to_owned())
    );
    assert_eq!(
        parse("channel = [\"discord\", \"signal\"]\n[discord]\nallowed_user_id = 42\n").channel,
        ChannelSelection::Multiple(vec!["discord".to_owned(), "signal".to_owned()])
    );
    assert_eq!(
        parse("channel = [\"discord\"]\n[discord]\nallowed_user_id = 42\n")
            .channel
            .names(),
        vec!["discord"]
    );
}

#[test]
fn flat_discord_keys_are_now_a_pointed_error() {
    let error = Config::parse(&format!("{BASE}allowed_user_id = 42\n"))
        .expect_err("flat keys were removed (D-09)");
    assert!(
        error.contains("allowed_user_id") && error.contains("unknown field"),
        "error names the key: {error}"
    );
}

#[test]
fn selected_discord_requires_its_section_keys() {
    // No [discord] section at all: the required operator id is missing.
    let errors = parse("").validate().expect_err("operator required");
    assert!(
        errors
            .iter()
            .any(|error| error.contains("[discord] allowed_user_id is required")),
        "{errors:?}"
    );
}

#[test]
fn unknown_surface_is_rejected() {
    let errors = parse("channel = \"telegram\"\n[discord]\nallowed_user_id = 42\n")
        .validate()
        .expect_err("unknown surface");
    assert!(
        errors.iter().any(|error| error.contains("unknown surface")),
        "{errors:?}"
    );
}

#[test]
fn duplicate_surface_entries_are_rejected() {
    let errors = parse("channel = [\"discord\", \"discord\"]\n[discord]\nallowed_user_id = 42\n")
        .validate()
        .expect_err("duplicates");
    assert!(
        errors.iter().any(|error| error.contains("twice")),
        "{errors:?}"
    );
}

#[test]
fn selecting_signal_is_a_loud_not_yet_error() {
    // The section parses (and validates its own shape), but selecting the
    // surface is refused until PH-4 (honest failure, not a silent no-op).
    let errors = parse(
        "channel = \"signal\"\n[signal]\nsocket = \"/run/signal-cli/socket\"\noperator_number = \"+15551234567\"\n",
    )
    .validate()
    .expect_err("signal not implemented");
    assert!(
        errors
            .iter()
            .any(|error| error.contains("not implemented until SPEC-004 PH-4")),
        "{errors:?}"
    );
}

#[test]
fn unselected_configured_sections_are_only_a_warning() {
    // A Discord-only box may carry [signal] config for a later rollout: the
    // section is validated (bad E.164 still errors) but its mere presence is
    // not an error.
    let config = parse(
        "[discord]\nallowed_user_id = 42\n[signal]\nsocket = \"/run/signal-cli/socket\"\noperator_number = \"+15551234567\"\n",
    );
    assert!(
        config.validate().is_ok(),
        "configured-but-unselected is fine"
    );
}

#[test]
fn signal_section_validates_its_own_shape() {
    let config = parse("[discord]\nallowed_user_id = 42\n[signal]\noperator_number = \"1555\"\n");
    let errors = config.validate().expect_err("bad E.164");
    assert!(
        errors.iter().any(|error| error.contains("operator_number")),
        "{errors:?}"
    );
    let config = parse("[discord]\nallowed_user_id = 42\n[signal]\nsocket = \"relative/path\"\n");
    let errors = config.validate().expect_err("relative socket");
    assert!(
        errors.iter().any(|error| error.contains("absolute path")),
        "{errors:?}"
    );
}

#[test]
fn per_surface_report_policies_carry_their_defaults() {
    let config = parse("[discord]\nallowed_user_id = 42\n[signal]\n");
    assert_eq!(
        config.discord.excerpt(),
        ocbot::config::ReportExcerpt::Excerpt
    );
    assert!(
        !config.discord.attachments(),
        "D-12: discord attachments default off"
    );
    assert_eq!(config.signal.excerpt(), ocbot::config::ReportExcerpt::Full);
    assert!(config.signal.attachments(), "signal attachments default on");

    let config = parse(
        "[discord]\nallowed_user_id = 42\nreport_attachments = true\nreport_excerpt = \"off\"\n",
    );
    assert!(
        config.discord.attachments(),
        "opt back in restores old behavior"
    );
    assert_eq!(config.discord.excerpt(), ocbot::config::ReportExcerpt::Off);
}

#[test]
fn unknown_section_keys_remain_load_errors() {
    let error = Config::parse(&format!(
        "{BASE}[discord]\nallowed_user_id = 42\nops_channel_lies = 7\n"
    ))
    .expect_err("deny_unknown_fields");
    assert!(error.contains("unknown field"), "{error}");
}
