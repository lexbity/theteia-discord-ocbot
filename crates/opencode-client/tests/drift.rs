//! Drift test (spec AC-15): every path the client constructs MUST exist in the
//! vendored `OpenAPI` spec, and the detector MUST be provably able to fail.
//!
//! When the pinned opencode version is bumped, upgrading the vendored spec in
//! the same commit is what keeps this green; removing an endpoint the client
//! calls turns CI red.
//!
//! Integration-test crates assert by unwrapping/panicking; the workspace
//! `panic`/`unwrap`/`expect` bans exist to keep *production* code honest
//! (spec C6, NFR-10).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

mod common;

use std::collections::HashSet;

use opencode_v2_client::CONSTRUCTED_PATHS;

/// Collapse `{parameter}` segments to `{}` so the client's `snake_case`
/// placeholders compare against the spec's `camelCase` ones.
fn collapse_params(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            if segment.starts_with('{') {
                "{}"
            } else {
                segment
            }
        })
        .collect::<Vec<&str>>()
        .join("/")
}

/// Path templates in the vendored spec, normalized the same way.
fn spec_path_templates(spec: &serde_json::Value) -> HashSet<String> {
    spec["paths"]
        .as_object()
        .expect("vendored spec has paths")
        .keys()
        .map(|path| collapse_params(path))
        .collect()
}

/// The constructed client paths missing from `spec`.
fn missing_paths(spec: &serde_json::Value, constructed: &[&str]) -> Vec<String> {
    let existing = spec_path_templates(spec);
    constructed
        .iter()
        .copied()
        .filter(|path| !existing.contains(&collapse_params(path)))
        .map(str::to_owned)
        .collect()
}

#[test]
fn every_constructed_path_exists_in_vendored_spec() {
    let spec = common::load_spec().expect("vendored spec loads");
    assert!(
        missing_paths(&spec, CONSTRUCTED_PATHS).is_empty(),
        "client paths missing from the vendored spec (bump the pinned opencode version or fix the client): {:?}",
        missing_paths(&spec, CONSTRUCTED_PATHS)
    );
}

#[test]
fn drift_detector_provably_fails() {
    // Negation self-test (AC-15): if `/api/info` were removed from the vendored
    // spec, the detector MUST report it. This proves the detector is not
    // decoration.
    let mut spec = common::load_spec().expect("vendored spec loads");
    let paths = spec["paths"].as_object_mut().expect("paths object");
    paths.remove("/api/info");
    let missing = missing_paths(&spec, CONSTRUCTED_PATHS);
    assert!(
        missing.iter().any(|path| path.as_str() == "/api/info"),
        "detector failed to notice the removed endpoint; got {missing:?}"
    );
}

#[test]
fn normalization_handles_camel_and_snake_parameters() {
    // `{sessionID}` (spec) vs `{session_id}` (client) must be equivalent.
    assert_eq!(
        collapse_params("/api/session/{sessionID}"),
        "/api/session/{}"
    );
    assert_eq!(
        collapse_params("/api/session/{session_id}/prompt"),
        "/api/session/{}/prompt"
    );
    assert_eq!(
        collapse_params("/api/experimental/session/{sessionID}/log"),
        "/api/experimental/session/{}/log"
    );
}
