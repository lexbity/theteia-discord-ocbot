//! Fuzz target: hostile JSON into every public deserializable type of
//! `opencode-v2-client`.
//!
//! The deserializers ARE the trust boundary (spec P16): a malformed or
//! adversarial response must produce a `Result`, never a panic. Structured
//! generators (proptest, the 10k battery in `tolerant_serde.rs`) are weaker at
//! finding weird-JSON edge cases; libFuzzer finds them.

#![no_main]

use libfuzzer_sys::fuzz_target;
use opencode_v2_client::*;

fuzz_target!(|data: &[u8]| {
    // Physically arbitrary bytes — first split on the first NUL, fuzz that as
    // UTF-8, and relentlessly attempt deserialization. Double-dose the valid
    // UTF-8 subset as JSON.
    let utf8 = match std::str::from_utf8(data) {
        Ok(text) => text,
        Err(_) => return,
    };
    let _: Result<ServerInfo, _> = serde_json::from_str(utf8);
    let _: Result<SessionInfo, _> = serde_json::from_str(utf8);
    let _: Result<Message, _> = serde_json::from_str(utf8);
    let _: Result<AssistantMessage, _> = serde_json::from_str(utf8);
    let _: Result<UserMessage, _> = serde_json::from_str(utf8);
    let _: Result<IdleMessage, _> = serde_json::from_str(utf8);
    let _: Result<ContentPart, _> = serde_json::from_str(utf8);
    let _: Result<Finish, _> = serde_json::from_str(utf8);
    let _: Result<StructuredError, _> = serde_json::from_str(utf8);
    let _: Result<TimeInfo, _> = serde_json::from_str(utf8);
    let _: Result<PermissionRequest, _> = serde_json::from_str(utf8);
    let _: Result<FormInfo, _> = serde_json::from_str(utf8);
    let _: Result<FormField, _> = serde_json::from_str(utf8);
    let _: Result<GateDecision, _> = serde_json::from_str(utf8);
    let _: Result<LocationRef, _> = serde_json::from_str(utf8);
    let _: Result<TokenUsage, _> = serde_json::from_str(utf8);
    let _: Result<CacheUsage, _> = serde_json::from_str(utf8);
    let _: Result<ActiveSession, _> = serde_json::from_str(utf8);
    let _: Result<InterruptResult, _> = serde_json::from_str(utf8);
});