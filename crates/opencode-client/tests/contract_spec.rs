//! AC-15-adjacent contract test: every Json-* fixture recorded in Phase 0 is
//! validated against the *vendored* `OpenAPI` spec for the pinned opencode
//! version. A fixture that silently no longer matches the pinned contract is a
//! hard failure — the same tripwire the bot relies on to survive the
//! experimental API (spec P17).
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

use common::{fixture_manifest, load_fixture, load_spec, validator_into};

#[test]
fn every_fixture_validates_against_vendored_spec() {
    let spec = load_spec().expect("vendored spec loads");
    for (fixture_name, pointer) in fixture_manifest() {
        let instance = load_fixture(fixture_name).expect("fixture loads");
        let validator = validator_into(&spec, pointer)
            .unwrap_or_else(|error| panic!("{fixture_name}: schema setup failed: {error}"));
        let mut errors = validator.iter_errors(&instance);
        if let Some(first) = errors.next() {
            panic!("{fixture_name} does not validate against {pointer}:\n  {first}");
        }
    }
}

#[test]
fn spec_resolves_every_manifest_pointer_without_trailing_slash() {
    let spec = load_spec().expect("vendored spec loads");
    for (fixture_name, pointer) in fixture_manifest() {
        assert!(
            !pointer.ends_with('/'),
            "{fixture_name}: pointer {pointer} looks malformed (trailing slash)"
        );
        assert!(
            common::resolve_pointer(&spec, pointer).is_ok(),
            "{fixture_name}: pointer {pointer} does not resolve"
        );
    }
}

#[test]
fn sse_fixtures_have_realistic_frames() {
    // The SSE fixtures are not JSON-schema-validatable as documents, but they
    // must still be loadable and meaningful: one `log.synced` head marker and
    // one full completion event sequence.
    let log_synced = std::fs::read_to_string(common::fixtures_dir().join("log_synced.sse"))
        .expect("log_synced.sse exists");
    assert!(log_synced.contains("log.synced"));
    assert!(log_synced.contains("\"seq\""));

    let events = std::fs::read_to_string(common::fixtures_dir().join("event_stream.sse"))
        .expect("event_stream.sse exists");
    for expected in [
        "server.connected",
        "session.execution.started",
        "session.text.delta",
        "session.execution.succeeded",
    ] {
        assert!(
            events.contains(expected),
            "event_stream.sse missing {expected}"
        );
    }
    // Every non-empty, non-comment line is a single `data:`-prefixed JSON
    // object (comment lines — `: heartbeat` — are standard SSE; the parser
    // ignores them and so does this test).
    for line in events
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with(':') && !line.starts_with("event:"))
    {
        assert!(
            line.starts_with("data: "),
            "non-data line in SSE fixture: {line:?}"
        );
        serde_json::from_str::<serde_json::Value>(&line["data: ".len()..])
            .unwrap_or_else(|error| panic!("malformed SSE JSON {line:?}: {error}"));
    }
}
