//! Shared helpers for the client crate's integration tests.
//!
//! Everything here reads from disk/`env!` only — tests never touch a network
//! server unless they build a mock explicitly.
//!
//! Integration-test crates assert by unwrapping and panicking (the spec's C6
//! `allow-*-in-tests` covers unit tests; the workspace `panic`/`unwrap`/
//! `expect` bans exist to keep *production* code honest, so this shared module
//! carries a scoped relax with the same reason).

#![allow(
    dead_code,
    reason = "shared helpers; the subset used varies per test crate"
)]

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Absolute path of the vendored `OpenAPI` spec for the pinned server version.
pub fn spec_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("spec/openapi-2.0.24.json")
}

/// Absolute path of the fixture directory for the pinned server version.
pub fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/v2/2.0.24")
}

/// Load the vendored `OpenAPI` spec document.
pub fn load_spec() -> Result<Value, String> {
    let text =
        std::fs::read_to_string(spec_path()).map_err(|error| format!("vendored spec: {error}"))?;
    serde_json::from_str(&text).map_err(|error| format!("vendored spec not valid JSON: {error}"))
}

/// Load a fixture by file name (`.json` required; `.sse` handled by callers).
pub fn load_fixture(name: &str) -> Result<Value, String> {
    let text = std::fs::read_to_string(fixtures_dir().join(name))
        .map_err(|error| format!("{name}: {error}"))?;
    serde_json::from_str(&text).map_err(|error| format!("{name}: {error}"))
}

/// Resolve an RFC 6901 JSON pointer (with optional leading `#`) inside `doc`.
pub fn resolve_pointer<'a>(doc: &'a Value, pointer: &str) -> Result<&'a Value, String> {
    let trimmed = pointer.trim_start_matches('#');
    if trimmed.is_empty() {
        return Ok(doc);
    }
    let mut current = doc;
    for raw in trimmed.trim_start_matches('/').split('/') {
        let segment = raw.replace("~1", "/").replace("~0", "~");
        current = current
            .get(segment.as_str())
            .ok_or_else(|| format!("pointer segment {segment:?} not found in {pointer}"))?;
    }
    Ok(current)
}

/// Build a JSON-Schema validator for the schema at `pointer` inside the
/// vendored spec document. `$ref`s inside the spec resolve against the whole
/// document (registered as a resource).
pub fn validator_into(doc: &Value, pointer: &str) -> Result<jsonschema::Validator, String> {
    resolve_pointer(doc, pointer)?;
    let resource = jsonschema::Resource::from_contents(doc.clone())
        .map_err(|error| format!("resource: {error}"))?;
    // JSON pointers may contain `{`/`}` (spec path params), which are invalid
    // in a URI fragment — percent-encode them for the `$ref`.
    let fragment = pointer
        .trim_start_matches('#')
        .replace('{', "%7B")
        .replace('}', "%7D");
    let wrapper = serde_json::json!({ "$ref": format!("urn:ocbot-spec#{fragment}") });
    jsonschema::options()
        .with_resource("urn:ocbot-spec", resource)
        .build(&wrapper)
        .map_err(|error| format!("validator: {error}"))
}

/// The fixture → (response-schema pointer) manifest used by `contract_spec.rs`.
pub fn fixture_manifest() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "info.json",
            "#/paths/~1api~1info/get/responses/200/content/application~1json/schema",
        ),
        (
            "session_create.json",
            "#/paths/~1api~1session/post/responses/200/content/application~1json/schema",
        ),
        (
            "session_get.json",
            "#/paths/~1api~1session~1{sessionID}/get/responses/200/content/application~1json/schema",
        ),
        (
            "sessions_root.json",
            "#/paths/~1api~1session/get/responses/200/content/application~1json/schema",
        ),
        (
            "session_active.json",
            "#/paths/~1api~1session~1active/get/responses/200/content/application~1json/schema",
        ),
        (
            "messages.json",
            "#/paths/~1api~1session~1{sessionID}~1message/get/responses/200/content/application~1json/schema",
        ),
        (
            "prompt_admission.json",
            "#/paths/~1api~1session~1{sessionID}~1prompt/post/responses/200/content/application~1json/schema",
        ),
        (
            "interrupt_idle.json",
            "#/paths/~1api~1session~1{sessionID}~1interrupt/post/responses/200/content/application~1json/schema",
        ),
        (
            "permission_pending.json",
            "#/paths/~1api~1permission~1request/get/responses/200/content/application~1json/schema",
        ),
        (
            "permission_pending_empty.json",
            "#/paths/~1api~1permission~1request/get/responses/200/content/application~1json/schema",
        ),
        (
            "form_pending.json",
            "#/paths/~1api~1form/get/responses/200/content/application~1json/schema",
        ),
        (
            "form_pending_empty.json",
            "#/paths/~1api~1form/get/responses/200/content/application~1json/schema",
        ),
    ]
}
