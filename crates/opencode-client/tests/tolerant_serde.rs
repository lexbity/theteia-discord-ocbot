//! Tolerant deserialization is the literal trust boundary of the client
//! (spec P16): **hostile JSON must produce a `Result`, never a panic**. Two
//! complementary batteries: a proptest over structured-but-arbitrary JSON and
//! a deterministic loop over 10,000 pseudorandom JSON-ish strings.
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

use opencode_v2_client::*;
use proptest::prelude::*;

/// Run every public deserializable type against one JSON string. Any panic in
/// here is a bug in the tolerant-serde contract.
fn deserialize_all(json: &str) {
    let _: Result<ServerInfo, _> = serde_json::from_str(json);
    let _: Result<SessionInfo, _> = serde_json::from_str(json);
    let _: Result<Message, _> = serde_json::from_str(json);
    let _: Result<AssistantMessage, _> = serde_json::from_str(json);
    let _: Result<UserMessage, _> = serde_json::from_str(json);
    let _: Result<IdleMessage, _> = serde_json::from_str(json);
    let _: Result<ContentPart, _> = serde_json::from_str(json);
    let _: Result<Finish, _> = serde_json::from_str(json);
    let _: Result<StructuredError, _> = serde_json::from_str(json);
    let _: Result<TimeInfo, _> = serde_json::from_str(json);
    let _: Result<PermissionRequest, _> = serde_json::from_str(json);
    let _: Result<FormInfo, _> = serde_json::from_str(json);
    let _: Result<FormField, _> = serde_json::from_str(json);
    let _: Result<GateDecision, _> = serde_json::from_str(json);
    let _: Result<LocationRef, _> = serde_json::from_str(json);
    let _: Result<TokenUsage, _> = serde_json::from_str(json);
    let _: Result<CacheUsage, _> = serde_json::from_str(json);
    let _: Result<ActiveSession, _> = serde_json::from_str(json);
    let _: Result<InterruptResult, _> = serde_json::from_str(json);
    let _: Result<EventFrame, _> = serde_json::from_str(json);
}

/// Arbitrary raw strings (mostly not even JSON) must never panic.
#[test]
fn arbitrary_strings_never_panic() {
    proptest!(ProptestConfig::with_cases(2048), |(raw in "\\PC*")| {
        deserialize_all(&raw);
    });
}

/// Arbitrary *valid* JSON values (names, depths, and shapes we have and
/// haven't seen) must never panic.
#[test]
fn arbitrary_json_never_panic() {
    proptest!(ProptestConfig::with_cases(1024), |(value in any::<ArbitraryJson>())| {
        let encoded = serde_json::to_string(&value.0).expect("Value always serializes");
        deserialize_all(&encoded);
    });
}

/// Proptest-serializable wrapper producing deeply recursive JSON.
#[derive(Debug, Clone)]
struct ArbitraryJson(serde_json::Value);

impl Arbitrary for ArbitraryJson {
    type Parameters = ();
    type Strategy = BoxedStrategy<Self>;

    fn arbitrary_with((): ()) -> Self::Strategy {
        just_scalar()
            .prop_recursive(4, 16, 8, |inner| {
                prop_oneof![
                    prop::collection::vec(inner.clone(), 0..8).prop_map(|items| {
                        Self(serde_json::Value::Array(
                            items.into_iter().map(|item| item.0).collect(),
                        ))
                    }),
                    prop::collection::hash_map("[a-zA-Z0-9_ -]{0,12}", inner.clone(), 0..8)
                        .prop_map(|map| {
                            Self(serde_json::Value::Object(
                                map.into_iter().map(|(key, value)| (key, value.0)).collect(),
                            ))
                        }),
                ]
            })
            .boxed()
    }
}

/// Random scalar JSON: string, number, bool, null.
fn just_scalar() -> impl Strategy<Value = ArbitraryJson> {
    prop_oneof![
        any::<f64>().prop_map(|f| ArbitraryJson(serde_json::json!(f))),
        any::<bool>().prop_map(|b| ArbitraryJson(serde_json::json!(b))),
        any::<String>().prop_map(|s| ArbitraryJson(serde_json::json!(s))),
        Just(ArbitraryJson(serde_json::Value::Null)),
    ]
    .boxed()
}

/// A tiny deterministic LCG so the "10,000 arbitrary JSON-ish strings" loop is
/// reproducible byte-for-byte across machines and runs.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn scalar(&mut self) -> serde_json::Value {
        match self.next() % 4 {
            0 => serde_json::Value::Null,
            1 => {
                let parity = self.next();
                serde_json::Value::Bool(parity.is_multiple_of(2))
            }
            2 => {
                let integer = u32::try_from(self.next() % 10_000).expect("fits");
                serde_json::json!(f64::from(integer) / 10.0)
            }
            _ => {
                // Random strings, frequently with JSON-hostile characters.
                let length = usize::try_from(self.next() % 24).expect("fits");
                let mut text = String::new();
                for _ in 0..length {
                    let byte = u8::try_from(self.next() % 128).expect("fits");
                    text.push(char::from(byte));
                }
                serde_json::Value::String(text)
            }
        }
    }

    fn value(&mut self, depth: u32) -> serde_json::Value {
        if depth == 0 {
            return self.scalar();
        }
        match self.next() % 5 {
            // 0, 3, 4 all fall through to scalar; the distribution skew is
            // intentional — most iterations should stay shallow.
            1 => {
                let count = usize::try_from(self.next() % 6).expect("fits");
                let mut items = Vec::new();
                for _ in 0..count {
                    items.push(self.value(depth - 1));
                }
                serde_json::Value::Array(items)
            }
            2 => {
                let count = usize::try_from(self.next() % 6).expect("fits");
                let mut object = serde_json::Map::new();
                for _ in 0..count {
                    let key = self.scalar().to_string();
                    let value = self.value(depth - 1);
                    object.insert(key.trim_matches('"').to_owned(), value);
                }
                serde_json::Value::Object(object)
            }
            _ => self.scalar(),
        }
    }
}

#[test]
fn ten_thousand_pseudorandom_jsons_never_panic() {
    let mut rng = Lcg(0x_9E37_79B9_7F4A_7C15);
    for iteration in 0..10_000_u64 {
        let value = rng.value(3);
        let encoded = serde_json::to_string(&value).expect("Value always serializes");
        deserialize_all(&encoded);
        // Also feed the number of iterations in as a string (boundary hygiene).
        deserialize_all(&iteration.to_string());
    }
}

#[test]
fn pathological_trailing_inputs_never_panic() {
    for json in [
        "\0\0\0",
        "{\"id\":",
        "}}}}}",
        "\x00\x01\x02",
        "{\"type\":\"assistant\",\"content\":",
        "null",
        "true",
        "[]",
        "\"\\u0000\"",
        "{\"type\":\"frm\",\"fields\":[[[[[[[",
    ] {
        let _: Result<serde_json::Value, _> = serde_json::from_str(json);
        deserialize_all(json);
    }
}
