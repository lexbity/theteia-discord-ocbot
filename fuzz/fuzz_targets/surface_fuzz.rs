//! Fuzz target: the SPEC-004 surface boundary (grammar, envelope, config).
//!
//! Three parsers sit where untrusted input enters the bot, and each must answer
//! with a value or an error — never a panic, whatever bytes arrive:
//!
//! 1. **The text grammar** (`ocbot-core::grammar`) — every Signal command line.
//!    It is a pure parser over operator text, so it is fuzzed directly: the
//!    property under test is totality.
//! 2. **The signal-cli envelope decoder** — hostile daemon JSON. signal-cli is
//!    unofficial and its schema is advisory, so unknown fields must be ignored
//!    and malformed ones reported.
//! 3. **The config deserializer** — the deployment's own file, which refuses
//!    unknown keys by design (`deny_unknown_fields`). The property under test is
//!    that a hostile file is a `Result`, not a crash.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Non-UTF-8 bytes are not text and not JSON: return early, exactly as the
    // daemon's reader does.
    let utf8 = match std::str::from_utf8(data) {
        Ok(text) => text,
        Err(_) => return,
    };

    // 1. The text grammar: every input yields an intent or an error.
    let _ = ocbot_core::parse_intent(utf8);
    let _ = ocbot_core::grammar::parse(utf8);

    // 2. The envelope decoder, driven through the same path the reader loop
    //    uses: JSON text → value → decode. The decoder is tolerant of unknown
    //    fields and total: every input yields an envelope or a reason.
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(utf8) {
        let _ = ocbot::adapters::signal::Envelope::decode(&value);
    }

    // 3. The config deserializer, in the shape it actually arrives in: TOML.
    let _: Result<ocbot::config::Config, _> = toml::from_str(utf8);
});
