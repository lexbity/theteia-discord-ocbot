//! Structural guard for the typed store actor (SPEC-002 §4.1, FR-1..FR-5).
//!
//! The store's message set is a typed enum; replies travel over per-variant
//! oneshot channels. This test reads the source so a reintroduced
//! `Box<dyn Any>` / `TypeMismatch` / erased `call` path turns red.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

/// Patterns that must not exist in the store adapter.
const BANNED: &[&str] = &[
    "Box<dyn Any",
    "TypeMismatch",
    "type Job",
    "type Request",
    "pub async fn call",
];

#[test]
fn store_has_no_type_erasure() {
    let source = include_str!("../src/adapters/store.rs");
    for banned in BANNED {
        assert!(
            !source.contains(banned),
            "store.rs still contains {banned:?}; the message set must stay typed"
        );
    }
    assert!(
        source.contains("enum StoreRequest"),
        "the typed StoreRequest enum is missing"
    );
    assert!(
        source.contains("fn dispatch("),
        "the typed dispatcher is missing"
    );
}
