//! PH-5: Signal reaction resolution tests (SPEC-004 FR-106/FR-111).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping (C6)"
)]

mod common;

use ocbot_core::render::ButtonKind;
use ocbot_core::render::{normalize_emoji, render_legend};

#[test]
fn reaction_emoji_normalization() {
    // Variation selectors should be stripped
    assert_eq!(normalize_emoji("🅿️"), "🅿");
    assert_eq!(normalize_emoji("🅿"), "🅿");
    assert_eq!(normalize_emoji("✅"), "✅");
    // Skin-tone modifiers should also be stripped
    assert_eq!(normalize_emoji("👍🏽"), "👍");
    assert_eq!(normalize_emoji("👍"), "👍");
}

#[test]
fn legend_maps_decision_emoji() {
    use ocbot_core::render::component_button;

    let components = vec![
        component_button("Accept", "v1:accept:7", ButtonKind::Success),
        component_button("Park", "v1:park:7", ButtonKind::Secondary),
        component_button("Follow up", "v1:followup:7", ButtonKind::Primary),
        component_button("Discard", "v1:discard:7", ButtonKind::Danger),
    ];

    let legend = render_legend(&components);

    assert!(legend.line.contains("✅ accept"));
    assert!(legend.line.contains("🅿️ park"));
    assert!(legend.line.contains("🔁 follow up"));
    assert!(legend.line.contains("🗑️ discard"));

    // Should have 4 entries in component_map
    assert_eq!(legend.entries.len(), 4);
    assert_eq!(legend.entries[0].emoji, "✅");
    assert_eq!(legend.entries[0].custom_id, "v1:accept:7");
}

#[test]
fn legend_skips_disabled_verbs() {
    use ocbot_core::render::component_button;

    let mut button = component_button("Accept", "v1:accept:7", ButtonKind::Success);
    button.disabled = true;

    let legend = render_legend(&[button]);

    // Disabled verbs should appear struck-through in the legend line
    assert!(legend.line.contains("~~✅ accept~~"));

    // But should NOT be in component_map entries
    assert!(legend.entries.is_empty());
}

#[test]
fn legend_maps_gate_keycaps() {
    use ocbot_core::render::component_button;

    let components = vec![
        component_button("once", "v1:gate:9:once", ButtonKind::Success),
        component_button("always", "v1:gate:9:always", ButtonKind::Primary),
        component_button("reject", "v1:gate:9:reject", ButtonKind::Danger),
    ];

    let legend = render_legend(&components);

    assert_eq!(legend.entries.len(), 3);
    assert_eq!(legend.entries[0].emoji, "1️⃣");
    assert_eq!(legend.entries[0].custom_id, "v1:gate:9:once");
    assert_eq!(legend.entries[1].emoji, "2️⃣");
    assert_eq!(legend.entries[2].emoji, "3️⃣");
}

#[test]
fn legend_limits_gate_keycaps_to_nine() {
    use ocbot_core::render::component_button;

    // 10 gate options - 10th should not be mapped (keycap is None)
    let components: Vec<_> = (0..10)
        .map(|i| {
            component_button(
                format!("opt{i}"),
                format!("v1:gate:10:{i}"),
                ButtonKind::Secondary,
            )
        })
        .collect();

    let legend = render_legend(&components);

    // Should only have 9 entries (options 0-8)
    assert_eq!(legend.entries.len(), 9);
    // Line should contain 1️⃣ through 9️⃣
    assert!(legend.line.contains("1️⃣"));
    assert!(legend.line.contains("9️⃣"));
}

#[test]
fn legend_navigation_not_mapped() {
    use ocbot_core::render::component_button;

    let components = vec![
        component_button("Next", "v1:page:history:2", ButtonKind::Secondary),
        component_button("Prev", "v1:page:history:1", ButtonKind::Secondary),
    ];

    let legend = render_legend(&components);

    // Navigation components should NOT be mapped
    assert!(legend.is_empty());
}

#[tokio::test]
async fn component_map_lookup_by_normalized_emoji() {
    use ocbot::adapters::store::StoreHandle;
    use tempfile::TempDir;

    let tmp = TempDir::new().expect("tmp");
    let db_path = tmp.path().join("test.db");

    let store = StoreHandle::open(&db_path).expect("open");

    // Insert with the canonical emoji
    let emoji_canonical = "🅿";
    let msg_id = "1000000000000";
    let custom_id = "v1:park:7";

    store
        .insert_component_map(
            "signal",
            msg_id,
            &[(emoji_canonical.to_string(), custom_id.to_string())],
        )
        .await
        .expect("insert");

    // Lookup with the emoji variant (with variation selector)
    let emoji_variant = "🅿️";
    let normalized = normalize_emoji(emoji_variant);

    let found = store
        .lookup_component_map("signal", msg_id, &normalized)
        .await
        .expect("lookup");

    assert_eq!(found, Some(custom_id.to_string()));
}

#[tokio::test]
async fn component_map_lookup_fails_on_missing_emoji() {
    use ocbot::adapters::store::StoreHandle;
    use tempfile::TempDir;

    let tmp = TempDir::new().expect("tmp");
    let db_path = tmp.path().join("test.db");

    let store = StoreHandle::open(&db_path).expect("open");

    // Insert with ✅
    store
        .insert_component_map(
            "signal",
            "1000",
            &[("✅".to_string(), "v1:accept:7".to_string())],
        )
        .await
        .expect("insert");

    // Lookup with 🅿️ should not match
    let found = store
        .lookup_component_map("signal", "1000", "🅿")
        .await
        .expect("lookup");

    assert_eq!(found, None);
}
