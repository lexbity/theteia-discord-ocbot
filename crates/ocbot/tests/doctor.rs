//! AC-16 (baseline half): the doctor validates the shipped FR-21 permission
//! baseline against the v2 `Permission.Rule` shape and names any missing `ask`
//! pattern (the version-gate half is covered by the client's `version_gate`).

// Integration tests assert via unwrap/panic; production code remains strict
// (workspace lint floor + G3). See common/mod.rs for the shared harness.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

use std::path::PathBuf;

use ocbot::app::doctor::Baseline;

/// The shipped baseline, resolved from the workspace root.
fn shipped_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config/opencode.permissions.jsonc")
}

#[test]
fn the_shipped_baseline_is_valid_and_covers_fr21() {
    let baseline =
        Baseline::load(&shipped_path()).expect("the shipped baseline must pass the doctor");
    // FR-21: the dangerous families are all guarded.
    for needle in ["git push", "sudo", "rm -rf", ".ssh"] {
        assert!(
            baseline.rules.iter().any(|rule| rule.effect
                == ocbot_core::ports::PermissionEffect::Ask
                && rule.resource.contains(needle)),
            "baseline must guard {needle:?}"
        );
    }
}

#[test]
fn a_missing_pattern_exits_naming_it() {
    let text = std::fs::read_to_string(shipped_path()).expect("shipped baseline readable");
    let broken = text.replace("sudo*", "ls*");
    let errors = Baseline::parse(&broken).expect_err("missing sudo guard");
    assert!(
        errors.iter().any(|error| error.contains("sudo")),
        "errors: {errors:?}"
    );
}

#[test]
fn an_unknown_rule_field_is_named() {
    let text = r#"{
        "permissions": [
            { "action": "bash", "resource": "git push*", "effect": "ask", "surprise": true }
        ]
    }"#;
    let errors = Baseline::parse(text).expect_err("extra field");
    assert!(
        errors.iter().any(|error| error.contains("Permission.Rule")),
        "errors: {errors:?}"
    );
}
