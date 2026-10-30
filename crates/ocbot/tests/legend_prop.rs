//! SPEC-004 PH-4/FR-106/FR-111: the reaction legend is a pure function whose
//! entries and rendered line never drift, and whose disabled-verb handling
//! matches FR-15 (struck through, absent from the map).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "property tests assert by unwrapping and panicking (C6)"
)]

use ocbot_core::render::{ButtonKind, ComponentSpec, render_legend};
use proptest::prelude::*;

/// The `custom_id` families a legend can map.
fn custom_id() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("v1:accept:7".to_owned()),
        Just("v1:park:7".to_owned()),
        Just("v1:followup:7".to_owned()),
        Just("v1:discard:7".to_owned()),
        (1_usize..12).prop_map(|n| format!("v1:gate:9:{n}")),
        // Unmappable navigation families.
        (0_usize..3).prop_map(|n| format!("v1:page:history:{n}")),
        Just("v1:projects:p:1".to_owned()),
        Just("v1:plan:3:1".to_owned()),
    ]
}

fn component() -> impl Strategy<Value = ComponentSpec> {
    (custom_id(), any::<bool>()).prop_map(|(custom_id, disabled)| ComponentSpec {
        label: "label".to_owned(),
        custom_id,
        kind: ButtonKind::Primary,
        disabled,
    })
}

fn is_mappable(custom_id: &str) -> bool {
    custom_id.starts_with("v1:accept:")
        || custom_id.starts_with("v1:park:")
        || custom_id.starts_with("v1:followup:")
        || custom_id.starts_with("v1:discard:")
        || custom_id.starts_with("v1:gate:")
}

/// A real message never carries the same `custom_id` twice; dedupe keeping the
/// first occurrence so the properties model that invariant.
fn dedupe(components: &[ComponentSpec]) -> Vec<ComponentSpec> {
    let mut seen = std::collections::HashSet::new();
    components
        .iter()
        .filter(|component| seen.insert(component.custom_id.clone()))
        .cloned()
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1500))]

    /// Every entry names an enabled, mappable component that is present.
    #[test]
    fn entries_point_at_enabled_mappable_components(raw in prop::collection::vec(component(), 0..12)) {
        let components = dedupe(&raw);
        let legend = render_legend(&components);
        for entry in &legend.entries {
            let matched = components
                .iter()
                .find(|component| component.custom_id == entry.custom_id)
                .expect("an entry must name a component");
            prop_assert!(!matched.disabled, "disabled verbs are never mapped (FR-15)");
            prop_assert!(is_mappable(&entry.custom_id));
            prop_assert!(legend.line.contains(&entry.emoji), "the line names every entry");
        }
    }

    /// Every enabled mappable component has exactly one entry; disabled ones
    /// have none.
    #[test]
    fn mapping_is_complete_and_unique(raw in prop::collection::vec(component(), 0..12)) {
        let components = dedupe(&raw);
        let legend = render_legend(&components);
        for component in &components {
            let mapped = legend.entries.iter().filter(|entry| entry.custom_id == component.custom_id).count();
            if component.disabled || !is_mappable(&component.custom_id) {
                prop_assert_eq!(mapped, 0, "{}", component.custom_id);
            } else {
                prop_assert_eq!(mapped, 1, "{}", component.custom_id);
            }
        }
    }

    /// Entries never outnumber components, and the line is empty exactly when
    /// nothing mappable exists.
    #[test]
    fn bounds_hold(raw in prop::collection::vec(component(), 0..12)) {
        let components = dedupe(&raw);
        let legend = render_legend(&components);
        prop_assert!(legend.entries.len() <= components.len());
        let any_mappable = components.iter().any(|component| is_mappable(&component.custom_id));
        prop_assert_eq!(legend.line.is_empty(), !any_mappable);
    }

    /// Emoji lookups are stable under a normalized round-trip.
    #[test]
    fn normalization_is_idempotent(emoji in ".{0,6}") {
        let once = ocbot_core::render::normalize_emoji(&emoji);
        let twice = ocbot_core::render::normalize_emoji(&once);
        prop_assert_eq!(once, twice);
    }
}
