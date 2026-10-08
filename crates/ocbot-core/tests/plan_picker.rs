//! Plan picking (spec P12): light markdown parsing over real fixtures, the
//! depth argument, ordered lists, >25-option paging, and the always-present
//! "Other: type it" affordance.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

use ocbot_core::plan::{OTHER_OPTION_LABEL, PLAN_PAGE_SIZE, PickOption, parse_plan, plan_page};
use ocbot_core::render::plan_picker_message;

const SAMPLE: &str = include_str!("fixtures/plan-sample.md");

#[test]
fn parses_ordered_items_under_in_range_headings() {
    let options = parse_plan(SAMPLE, 2, 3);
    let labels: Vec<&str> = options.iter().map(|option| option.label.as_str()).collect();
    assert_eq!(
        labels,
        vec![
            "Add the migration",
            "Wire the store method",
            "Cover it with a test",
            "Keep the diff small",
            "Do not change the wire format",
            "Deploy to staging",
            "Run the smoke suite",
        ]
    );
    assert_eq!(options[0].section.as_deref(), Some("Phase 1 — groundwork"));
    assert_eq!(options[5].section.as_deref(), Some("Phase 2 — rollout"));
}

#[test]
fn line_wrapped_items_join_with_a_single_space() {
    // A wrapped list item emits a softbreak event; the label must keep the
    // words separated (kills the missing-softbreak mutant).
    let options = parse_plan("## Steps\n1. first line\n   second line\n", 2, 2);
    assert_eq!(options[0].label, "first line second line");
}

#[test]
fn depth_argument_selects_the_heading_levels() {
    // H1 only: `Release plan` has children so it is not an option; the
    // childless `Appendix` heading is.
    let h1 = parse_plan(SAMPLE, 1, 1);
    let h1_labels: Vec<&str> = h1.iter().map(|option| option.label.as_str()).collect();
    assert_eq!(h1_labels, vec!["Appendix"]);
    // H3 only: the two notes bullets.
    let h3 = parse_plan(SAMPLE, 3, 3);
    assert_eq!(h3.len(), 2);
    assert_eq!(h3[0].label, "Keep the diff small");
}

#[test]
fn more_than_one_page_is_sliced_with_metadata() {
    let options: Vec<PickOption> = (0..=(PLAN_PAGE_SIZE * 2 + 2))
        .map(|index| PickOption {
            label: format!("step {index}"),
            section: None,
        })
        .collect();
    let first = plan_page(&options, 0, PLAN_PAGE_SIZE);
    assert_eq!(first.total_pages, 3);
    assert!(!first.has_prev());
    assert!(first.has_next());
    let last = plan_page(&options, 99, PLAN_PAGE_SIZE);
    assert_eq!(last.page, 2);
    assert_eq!(last.options.len(), 3);
    assert!(last.has_prev());
    assert!(!last.has_next());
}

#[test]
fn picker_always_offers_other_and_paging() {
    let options: Vec<PickOption> = (0..=PLAN_PAGE_SIZE)
        .map(|index| PickOption {
            label: format!("step {index}"),
            section: None,
        })
        .collect();
    let message = plan_picker_message("task", &options, 0, 11, 42);
    let ids: Vec<&str> = message
        .components
        .iter()
        .map(|component| component.custom_id.as_str())
        .collect();
    assert!(ids.contains(&"v1:plan:11:0"));
    assert!(ids.contains(&"v1:planp:11:1"));
    assert!(ids.contains(&"v1:planother:11"));
    assert!(ids.contains(&"v1:planuse:11"));
    assert!(
        message
            .components
            .iter()
            .any(|component| component.label == OTHER_OPTION_LABEL),
        "Other is always present"
    );

    // Even with zero options the picker is renderable (the handler falls back
    // before this, but the renderer must never produce an empty component set).
    let empty = plan_picker_message("task", &[], 0, 12, 42);
    assert!(
        empty
            .components
            .iter()
            .any(|component| component.custom_id == "v1:planother:12")
    );
}
