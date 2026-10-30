//! Project-centric renderers (SPEC-005): the `render::projects` concern,
//! exercised through its public API only. The invariants under test:
//!
//! 1. rendered messages never expose filesystem paths — only opaque
//!    project/plan names and 16-hex action tokens;
//! 2. every message and button stays within the platform caps (P10);
//! 3. pagination is stable: page indices clamp, navigation ids are opaque,
//!    and both the item and the byte budgets split pages.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration tests assert by unwrapping and panicking (C6)"
)]

use ocbot_core::paths::{plan_action_token, project_action_token};
use ocbot_core::ports::{PlanInfo, ProjectDetails, ProjectInfo};
use ocbot_core::render::projects::PROJECTS_PAGE_SIZE;
use ocbot_core::render::{
    DISCORD_MESSAGE_CAP, OutgoingMessage, full_readme_message, plan_preview_message,
    project_details_message, project_list_message,
};
use std::fmt::Write as _;
use std::path::PathBuf;

fn project(index: usize) -> ProjectInfo {
    ProjectInfo {
        name: format!("user/project-{index:02}"),
        dir: PathBuf::from(format!("/private/root/project-{index:02}")),
    }
}

fn assert_within_platform_limits(message: &OutgoingMessage) {
    assert!(
        message.content.len() <= DISCORD_MESSAGE_CAP,
        "content was {} bytes",
        message.content.len()
    );
    assert!(message.components.len() <= 25, "at most 25 buttons");
    for component in &message.components {
        assert!(
            component.custom_id.len() <= 100,
            "id cap: Discord rejects longer"
        );
        assert!(
            component.label.chars().count() <= 80,
            "label cap: Discord truncates longer"
        );
    }
}

/// Every rendered project list must hide the backing directories.
fn assert_no_paths(message: &OutgoingMessage) {
    assert!(
        !message.content.contains("/private"),
        "leaked a filesystem path: {}",
        message.content
    );
}

#[test]
fn empty_list_degrades_to_a_hint_without_buttons() {
    let message = project_list_message(&[], 0, 42);
    assert!(message.content.contains("No projects found"));
    assert!(message.components.is_empty());
    assert_eq!(message.mention_allowlist, vec![42]);
}

#[test]
fn short_list_is_a_single_page_without_navigation() {
    let projects: Vec<_> = (0..3).map(project).collect();
    let message = project_list_message(&projects, 0, 42);
    assert!(message.content.contains("Page 1 of 1"));
    // 3 project buttons, no navigation.
    assert_eq!(message.components.len(), 3);
    assert!(
        !message
            .components
            .iter()
            .any(|component| component.custom_id.starts_with("v1:projects:p:"))
    );
    assert_within_platform_limits(&message);
    assert_no_paths(&message);
}

#[test]
fn long_list_pages_with_opaque_navigation_ids() {
    let projects: Vec<_> = (0..(PROJECTS_PAGE_SIZE + 3)).map(project).collect();
    let first = project_list_message(&projects, 0, 42);
    assert!(first.content.contains("Page 1 of 2"));
    // 20 project buttons + one "Next".
    assert_eq!(first.components.len(), PROJECTS_PAGE_SIZE + 1);
    assert!(
        first
            .components
            .iter()
            .any(|component| component.custom_id == "v1:projects:p:1")
    );
    assert_within_platform_limits(&first);
    assert_no_paths(&first);

    let last = project_list_message(&projects, 1, 42);
    assert!(last.content.contains("Page 2 of 2"));
    // 3 project buttons + one "Previous".
    assert_eq!(last.components.len(), 4);
    assert!(
        last.components
            .iter()
            .any(|component| component.custom_id == "v1:projects:p:0")
    );
    assert_within_platform_limits(&last);

    // An out-of-range page clamps to the last page rather than panicking.
    let clamped = project_list_message(&projects, 99, 42);
    assert!(clamped.content.contains("Page 2 of 2"));
}

#[test]
fn project_select_ids_are_opaque_tokens_not_paths() {
    let projects = vec![project(0)];
    let message = project_list_message(&projects, 0, 42);
    let expected = format!(
        "v1:project_select:{}",
        project_action_token("user/project-00")
    );
    assert!(
        message
            .components
            .iter()
            .any(|component| component.custom_id == expected)
    );
    // The token is derived from the name, not the directory.
    assert!(
        !message
            .components
            .iter()
            .any(|component| component.custom_id.contains("private"))
    );
}

#[test]
fn details_hide_paths_and_carry_run_and_back_actions() {
    let details = ProjectDetails {
        name: "user/alpha".to_owned(),
        readme_preview: Some("# Alpha\n\nA test project.".to_owned()),
        plans: vec![PlanInfo {
            name: "rollout.md".to_owned(),
            path: PathBuf::from("/private/alpha/devdocs/plans/rollout.md"),
        }],
    };
    let message = project_details_message(&details, 0, 42);
    assert!(message.content.contains("**Project: `user/alpha`**"));
    assert!(message.content.contains("**README Preview**"));
    assert!(message.content.contains("> # Alpha"));
    assert!(message.content.contains("📄 `rollout.md`"));
    assert_no_paths(&message);
    assert_within_platform_limits(&message);

    let project_ref = project_action_token("user/alpha");
    let plan_ref = plan_action_token("user/alpha", "rollout.md");
    assert!(
        message
            .components
            .iter()
            .any(|component| component.custom_id
                == format!("v1:plan_select:{project_ref}:{plan_ref}"))
    );
    assert!(
        message
            .components
            .iter()
            .any(|component| component.custom_id == format!("v1:run_noplan:{project_ref}"))
    );
    assert!(
        message
            .components
            .iter()
            .any(|component| component.custom_id == "v1:projects")
    );
    assert!(
        message
            .components
            .iter()
            .any(|component| component.custom_id == format!("v1:readme_full:{project_ref}"))
    );
}

#[test]
fn details_without_readme_or_plans_degrade_gracefully() {
    let details = ProjectDetails {
        name: "user/beta".to_owned(),
        readme_preview: None,
        plans: vec![],
    };
    let message = project_details_message(&details, 0, 42);
    assert!(message.content.contains("_No plans found._"));
    assert!(!message.content.contains("README Preview"));
    // No README: no "Show Full README" button.
    assert!(
        !message
            .components
            .iter()
            .any(|component| component.custom_id.starts_with("v1:readme_full:"))
    );
    // The back and run actions survive.
    assert!(
        message
            .components
            .iter()
            .any(|component| component.custom_id == "v1:projects")
    );
    assert!(
        message
            .components
            .iter()
            .any(|component| component.custom_id.starts_with("v1:run_noplan:"))
    );
    assert_within_platform_limits(&message);
}

#[test]
fn many_plans_page_and_clamp_within_the_cap() {
    let details = ProjectDetails {
        name: "user/gamma".to_owned(),
        readme_preview: None,
        plans: (0..40)
            .map(|index| PlanInfo {
                name: format!("plan-{index:02}.md"),
                path: PathBuf::from(format!("/private/plans/plan-{index:02}.md")),
            })
            .collect(),
    };
    let first = project_details_message(&details, 0, 42);
    assert!(first.content.contains("page 1 of 3"));
    assert_within_platform_limits(&first);
    assert_no_paths(&first);
    let project_ref = project_action_token("user/gamma");
    assert!(
        first
            .components
            .iter()
            .any(|component| component.custom_id == format!("v1:project_page:{project_ref}:1"))
    );

    let middle = project_details_message(&details, 1, 42);
    assert!(middle.content.contains("page 2 of 3"));
    assert!(
        middle
            .components
            .iter()
            .any(|component| component.custom_id == format!("v1:project_page:{project_ref}:0"))
    );
    assert!(
        middle
            .components
            .iter()
            .any(|component| component.custom_id == format!("v1:project_page:{project_ref}:2"))
    );
    assert_within_platform_limits(&middle);

    let clamped = project_details_message(&details, 99, 42);
    assert!(clamped.content.contains("page 3 of 3"));
    assert_within_platform_limits(&clamped);
}

#[test]
fn readme_preview_shows_at_most_five_quoted_lines() {
    let details = ProjectDetails {
        name: "user/delta".to_owned(),
        readme_preview: Some("1\n2\n3\n4\n5\n6\n7".to_owned()),
        plans: vec![],
    };
    let message = project_details_message(&details, 0, 42);
    assert!(message.content.contains("> 5"));
    assert!(!message.content.contains("> 6"));
}

#[test]
fn full_readme_single_page_has_no_navigation() {
    let message = full_readme_message("user/alpha", Some("# Title\n\nBody."), 0, 42);
    assert!(
        message
            .content
            .contains("**Project: `user/alpha` — Full README**")
    );
    assert!(message.content.contains("Page 1 of 1"));
    assert!(message.content.contains("# Title"));
    // Only "Back to Project".
    assert_eq!(message.components.len(), 1);
    assert_within_platform_limits(&message);
}

#[test]
fn full_readme_without_readme_points_back_to_the_project() {
    let message = full_readme_message("user/alpha", None, 0, 42);
    assert!(message.content.contains("No README found"));
    assert!(
        message
            .components
            .iter()
            .any(|component| component.custom_id.starts_with("v1:project_select:"))
    );
    assert_within_platform_limits(&message);
}

#[test]
fn long_readme_pages_with_opaque_navigation_and_stays_bounded() {
    let readme = format!("# README\n{}\nEND", "x".repeat(4_000));
    let first = full_readme_message("user/alpha", Some(&readme), 0, 42);
    assert!(first.content.contains("Page 1 of"));
    assert_within_platform_limits(&first);
    let project_ref = project_action_token("user/alpha");
    assert!(
        first
            .components
            .iter()
            .any(|component| { component.custom_id == format!("v1:readme_page:{project_ref}:1") })
    );

    let last = full_readme_message("user/alpha", Some(&readme), 99, 42);
    assert!(last.content.contains("END"));
    assert!(
        last.components
            .iter()
            .any(|component| component.custom_id.starts_with("v1:readme_page:"))
    );
    assert_within_platform_limits(&last);
}

#[test]
fn empty_readme_is_one_blank_page() {
    let message = full_readme_message("user/alpha", Some(""), 0, 42);
    assert!(message.content.contains("Page 1 of 1"));
    assert!(
        !message
            .components
            .iter()
            .any(|component| component.custom_id.starts_with("v1:readme_page:"))
    );
}

#[test]
fn multibyte_readme_splits_on_char_boundaries() {
    let readme = "é".repeat(4_000);
    let first = full_readme_message("user/alpha", Some(&readme), 0, 42);
    assert!(first.content.contains("Page 1 of"));
    assert_within_platform_limits(&first);
    let second = full_readme_message("user/alpha", Some(&readme), 1, 42);
    assert!(second.content.contains("Page 2 of"));
    assert_within_platform_limits(&second);
}

#[test]
fn plan_preview_caps_at_twenty_lines() {
    let mut content = String::from("# Plan\n");
    for step in 1..=25 {
        let _ = writeln!(content, "Step {step}");
    }
    let message = plan_preview_message("user/alpha", "rollout.md", &content, 42);
    assert!(message.content.contains("**Plan: `rollout.md`**"));
    // The heading counts toward the 20-line cap: steps 1..=19 show, 20 does not.
    assert!(message.content.contains("Step 19"));
    assert!(!message.content.contains("Step 20"));
    assert_within_platform_limits(&message);
}

#[test]
fn plan_preview_neutralizes_fences_and_marks_truncation() {
    let content = format!("```danger\n{}\n```", "x".repeat(5_000));
    let message = plan_preview_message("user/alpha", "rollout.md", &content, 42);
    assert!(
        !message.content.contains("```danger"),
        "fences are neutralized"
    );
    assert!(message.content.contains("...[truncated]"));
    assert_within_platform_limits(&message);

    let project_ref = project_action_token("user/alpha");
    let plan_ref = plan_action_token("user/alpha", "rollout.md");
    assert!(
        message.components.iter().any(
            |component| component.custom_id == format!("v1:load_plan:{project_ref}:{plan_ref}")
        )
    );
}
