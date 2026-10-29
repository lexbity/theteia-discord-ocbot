//! SPEC-004 PH-4/FR-111: the text grammar is a pure function with property
//! tests (≥1000 cases): verb/arg round-trips, whitespace and case tolerance,
//! unknown verbs, and prompt-text preservation.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "property tests assert by unwrapping and panicking (C6)"
)]

use ocbot_core::grammar::{GrammarError, Intent, parse};
use proptest::prelude::*;

/// The canonical verbs the grammar recognizes.
const VERBS: &[&str] = &[
    "run", "prompt", "status", "abort", "release", "mute", "history", "reset",
    "resetgo", "send", "edit", "pick", "accept", "park", "follow", "discard", "gate", "help",
];

/// A safe argument alphabet: no `|` (the run prompt split), no whitespace (so
/// the single-token verbs stay unambiguous), and short.
fn safe_arg() -> impl Strategy<Value = String> {
    "[A-Za-z0-9_./\\-]{1,24}"
}

/// The arguments a verb requires to parse successfully.
fn valid_tail(verb: &str, arg: &str) -> String {
    match verb {
        "gate" => format!("{arg} once"),
        "pick" | "history" => "2".to_owned(),
        "resetgo" => "deadbeef".to_owned(),
        _ => arg.to_owned(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1500))]

    /// Case-insensitivity: an uppercased *verb* parses identically (arguments
    /// keep their case — paths and labels are data).
    #[test]
    fn commands_are_case_insensitive(verb in prop::sample::select(VERBS), arg in safe_arg()) {
        let tail = valid_tail(verb, &arg);
        let lower = parse(&format!("{verb} {tail}")).map_err(|error| error.to_string());
        let upper = parse(&format!("{} {tail}", verb.to_uppercase())).map_err(|error| error.to_string());
        prop_assert_eq!(lower, upper);
    }

    /// Surrounding whitespace never changes the parse.
    #[test]
    fn surrounding_whitespace_is_ignored(verb in prop::sample::select(VERBS), arg in safe_arg(), pad in "[ \\t]{0,4}") {
        let command = format!("{verb} {}", valid_tail(verb, &arg));
        let plain = parse(&command).map_err(|error| error.to_string());
        let padded = parse(&format!("{pad}{command}{pad}")).map_err(|error| error.to_string());
        prop_assert_eq!(plain, padded);
    }

    /// Verb/arg round-trip: a well-formed command's intent names its verb, and
    /// a decision/send/edit intent carries the label it was given.
    #[test]
    fn verb_and_label_round_trip(verb in prop::sample::select(VERBS), arg in safe_arg()) {
        let command = format!("{verb} {}", valid_tail(verb, &arg));
        let intent = parse(&command).expect("a valid command parses");
        // `follow` is canonical (there is no `followup` in the generation set).
        prop_assert_eq!(intent.verb(), verb);
        let trimmed = arg.trim();
        match &intent {
            Intent::Decision { label, .. }
            | Intent::Send { label }
            | Intent::Edit { label } => {
                let expected = if trimmed.is_empty() { None } else { Some(trimmed) };
                prop_assert_eq!(label.as_deref(), expected);
            }
            _ => {}
        }
    }

    /// Unknown verbs never panic and always name the verb.
    #[test]
    fn unknown_verbs_are_named(verb in "[a-z]{1,12}") {
        prop_assume!(!VERBS.contains(&verb.as_str()));
        match parse(&verb) {
            Err(GrammarError::UnknownVerb(named)) => prop_assert_eq!(named, verb),
            other => prop_assert!(false, "expected UnknownVerb, got {other:?}"),
        }
    }

    /// Prompt text is preserved verbatim (multi-word, punctuation, emoji).
    #[test]
    fn prompt_text_is_preserved(text in "[^\\n]{1,60}") {
        let trimmed = text.trim();
        prop_assume!(!trimmed.is_empty());
        let intent = parse(&format!("prompt {text}")).expect("parses");
        match intent {
            Intent::Prompt { text: Some(captured) } => prop_assert_eq!(captured, trimmed),
            other => prop_assert!(false, "expected a prompt, got {other:?}"),
        }
    }

    /// Whitespace-only input is Empty; never a panic.
    #[test]
    fn whitespace_is_empty(pad in "[ \\t]{1,8}") {
        prop_assert_eq!(parse(&pad), Err(GrammarError::Empty));
    }
}

#[test]
fn the_property_suite_covers_every_canonical_verb() {
    for verb in VERBS {
        let tail = valid_tail(verb, "x");
        assert!(
            parse(&format!("{verb} {tail}")).is_ok(),
            "`{verb}` must parse"
        );
    }
}
