//! SPEC-005 phase 14: the error Display strings the operator-facing surfaces
//! forward (`operator_error` maps variants to their message; the store's
//! failure text reaches users through these types). Pinning the exact strings
//! here turns every "message contains …" spot-check in the shell suites into
//! a restatement of one source of truth.

use ocbot_core::error::{ProjectConfigError, ProjectError};
use ocbot_core::ports::{PermissionEffect, ProjectConfig};

#[test]
fn project_error_displays_are_stable_prose() {
    assert_eq!(
        ProjectError::NotConfigured.to_string(),
        "projects root not configured"
    );
    assert_eq!(
        ProjectError::NotFound("root/alpha".to_owned()).to_string(),
        "project not found: root/alpha"
    );
    assert_eq!(
        ProjectError::NoProjects("/srv/projects".to_owned()).to_string(),
        "no projects found in /srv/projects"
    );
    assert_eq!(
        ProjectError::NoPlans("root/alpha".to_owned()).to_string(),
        "no plans found in project: root/alpha"
    );
    assert_eq!(
        ProjectError::IoError("/a".to_owned(), "denied".to_owned()).to_string(),
        "failed to read directory /a: denied"
    );
    assert_eq!(
        ProjectError::ReadError("/a/b.md".to_owned(), "is a directory".to_owned()).to_string(),
        "failed to read file /a/b.md: is a directory"
    );
}

#[test]
fn project_config_error_displays_are_stable_prose() {
    assert_eq!(
        ProjectConfigError::ParseError("/p/ocbot.toml".to_owned(), "boom".to_owned()).to_string(),
        "invalid TOML in /p/ocbot.toml: boom"
    );
    assert_eq!(
        ProjectConfigError::IoError("/p/ocbot.toml".to_owned(), "denied".to_owned()).to_string(),
        "failed to read config /p/ocbot.toml: denied"
    );
}

#[test]
fn project_config_default_matches_the_documented_defaults() {
    // The same values the serde defaults yield (SPEC-005 §Global
    // Configuration / §Per-Project Configuration): plans in
    // `devdocs/plans`, README as `README.md`.
    let default = ProjectConfig::default();
    assert_eq!(default.plans_dirs, vec!["devdocs/plans".to_owned()]);
    assert_eq!(default.readme_file, "README.md");
}

#[test]
fn permission_effect_wire_spellings_are_stable() {
    // These strings are the wire spelling the ruleset sends to the server
    // (Permission.Effect); a silent rename breaks FR-42 payloads.
    assert_eq!(PermissionEffect::Allow.as_str(), "allow");
    assert_eq!(PermissionEffect::Deny.as_str(), "deny");
    assert_eq!(PermissionEffect::Ask.as_str(), "ask");
}
