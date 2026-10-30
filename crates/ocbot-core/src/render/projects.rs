//! Project-centric message renderers (SPEC-005).
//!
//! This module provides the message rendering functions for the project-centric
//! remote control interface. All user-facing messages hide filesystem paths and
//! only expose opaque project names and plan names.

use std::fmt::Write as _;
use std::ops::Range;

use crate::paths::{plan_action_token, project_action_token};
use crate::render::report::{DISCORD_MESSAGE_CAP, fence_neutralize, split_excerpt};

use super::label::{safe_name, short_name};
use super::{ButtonKind, OutgoingMessage, component_button};

/// Maximum number of selectable projects on one message page.
pub const PROJECTS_PAGE_SIZE: usize = 20;
/// Maximum number of selectable plans on one project page.
pub const PROJECT_PLANS_PAGE_SIZE: usize = 18;

const PROJECT_LIST_TEXT_BUDGET: usize = 1_650;
const README_PREVIEW_BUDGET: usize = 500;
const README_PAGE_BODY_BUDGET: usize = 1_250;
const PLAN_PREVIEW_MARKER: &str = "\n...[truncated]";

fn char_prefix_bytes(text: &str, max_bytes: usize) -> usize {
    text.char_indices()
        .map(|(offset, character)| offset.saturating_add(character.len_utf8()))
        .take_while(|end| *end <= max_bytes)
        .last()
        .unwrap_or(0)
}

fn truncate_utf8_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let marker = "…".get(..max_bytes.min("…".len())).unwrap_or_default();
    let prefix_end = char_prefix_bytes(text, max_bytes.saturating_sub(marker.len()));
    format!("{}{marker}", &text[..prefix_end])
}

fn page_ranges<T>(
    items: &[T],
    max_items: usize,
    text_budget: usize,
    render_line: impl Fn(&T, usize) -> String,
) -> Vec<Range<usize>> {
    if items.is_empty() {
        return std::iter::once(0..0).collect();
    }

    let max_items = max_items.max(1);
    let text_budget = text_budget.max(1);
    let mut ranges = Vec::new();
    let mut start = 0;
    let mut used_bytes: usize = 0;

    for (index, item) in items.iter().enumerate() {
        let line_bytes = render_line(item, text_budget).len();
        let page_is_full = index - start >= max_items;
        let line_would_overflow = used_bytes.saturating_add(line_bytes) > text_budget;
        if page_is_full || line_would_overflow {
            ranges.push(start..index);
            start = index;
            used_bytes = 0;
        }
        used_bytes = used_bytes.saturating_add(line_bytes);
    }
    ranges.push(start..items.len());
    ranges
}

fn project_line(project: &crate::ports::ProjectInfo, budget: usize) -> String {
    let name_budget = budget.saturating_sub("📁 ``\n".len());
    let name = truncate_utf8_bytes(&safe_name(&project.name), name_budget);
    format!("📁 `{name}`\n")
}

fn plan_line(plan: &crate::ports::PlanInfo, budget: usize) -> String {
    let name_budget = budget.saturating_sub("📄 ``\n".len());
    let name = truncate_utf8_bytes(&safe_name(&plan.name), name_budget);
    format!("📄 `{name}`\n")
}

fn navigation_buttons(
    components: &mut Vec<super::ComponentSpec>,
    page: usize,
    page_count: usize,
    previous_id: impl Fn(usize) -> String,
    next_id: impl Fn(usize) -> String,
) {
    if page > 0 {
        components.push(component_button(
            "◀ Previous",
            previous_id(page - 1),
            ButtonKind::Secondary,
        ));
    }
    if page.saturating_add(1) < page_count {
        components.push(component_button(
            "Next ▶",
            next_id(page + 1),
            ButtonKind::Secondary,
        ));
    }
}

/// Render list of projects.
///
/// Displays one selectable page of projects under `projects_root`.
/// Project names are shown without paths; component IDs carry short opaque
/// tokens that the handler resolves against the current project list.
pub fn project_list_message(
    projects: &[crate::ports::ProjectInfo],
    page: usize,
    operator_id: u64,
) -> OutgoingMessage {
    if projects.is_empty() {
        return OutgoingMessage {
            content: "No projects found. Check `projects_root` in config.".to_owned(),
            attachment: None,
            components: Vec::new(),
            mention_allowlist: vec![operator_id],
        };
    }

    let mut ordered: Vec<_> = projects.iter().collect();
    ordered.sort_by(|left, right| left.name.cmp(&right.name));
    let ranges = page_ranges(
        &ordered,
        PROJECTS_PAGE_SIZE,
        PROJECT_LIST_TEXT_BUDGET,
        |project, budget| project_line(project, budget),
    );
    let page = page.min(ranges.len().saturating_sub(1));
    let range = ranges.get(page).cloned().unwrap_or(0..0);
    let mut content = format!("**Projects**\n_Page {} of {}_\n", page + 1, ranges.len());
    let mut components = Vec::new();
    for project in &ordered[range] {
        content.push_str(&project_line(project, PROJECT_LIST_TEXT_BUDGET));
        let project_ref = project_action_token(&project.name);
        components.push(component_button(
            format!("📁 {}", short_name(&project.name, 77)),
            format!("v1:project_select:{project_ref}"),
            ButtonKind::Primary,
        ));
    }
    navigation_buttons(
        &mut components,
        page,
        ranges.len(),
        |target| format!("v1:projects:p:{target}"),
        |target| format!("v1:projects:p:{target}"),
    );

    OutgoingMessage {
        content,
        attachment: None,
        components,
        mention_allowlist: vec![operator_id],
    }
}

/// Render project details (README preview + plans).
///
/// Shows the project's README preview (first 5 lines) and one selectable page
/// of plans. Buttons navigate between pages, open the full README, select a
/// plan, or run without a plan.
pub fn project_details_message(
    details: &crate::ports::ProjectDetails,
    page: usize,
    operator_id: u64,
) -> OutgoingMessage {
    let project_ref = project_action_token(&details.name);
    let project_title = format!("**Project: `{}`**\n", short_name(&details.name, 160));
    let readme_text = details
        .readme_preview
        .as_deref()
        .map_or_else(String::new, |preview| {
            let quoted = preview
                .lines()
                .take(5)
                .map(|line| format!("> {line}"))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "\n**README Preview**\n{}\n",
                truncate_utf8_bytes(&quoted, README_PREVIEW_BUDGET)
            )
        });
    let prefix = format!("{project_title}{readme_text}");
    let plan_text_budget = DISCORD_MESSAGE_CAP
        .saturating_sub(prefix.len())
        .saturating_sub(128)
        .max(1);
    let mut plans: Vec<_> = details.plans.iter().collect();
    plans.sort_by(|left, right| left.name.cmp(&right.name));
    let ranges = page_ranges(
        &plans,
        PROJECT_PLANS_PAGE_SIZE,
        plan_text_budget,
        |plan, budget| plan_line(plan, budget),
    );
    let page = page.min(ranges.len().saturating_sub(1));
    let range = ranges.get(page).cloned().unwrap_or(0..0);
    let mut content = prefix;
    let mut components = Vec::new();

    if details.readme_preview.is_some() {
        components.push(component_button(
            "Show Full README",
            format!("v1:readme_full:{project_ref}"),
            ButtonKind::Secondary,
        ));
    }

    if plans.is_empty() {
        content.push_str("\n_No plans found._");
    } else {
        let _ = writeln!(
            content,
            "\n**Available Plans · page {} of {}**",
            page + 1,
            ranges.len()
        );
        for plan in &plans[range] {
            content.push_str(&plan_line(plan, plan_text_budget));
            let plan_ref = plan_action_token(&details.name, &plan.name);
            components.push(component_button(
                format!("📄 {}", short_name(&plan.name, 77)),
                format!("v1:plan_select:{project_ref}:{plan_ref}"),
                ButtonKind::Primary,
            ));
        }
    }

    components.push(component_button(
        "Run Without Plan",
        format!("v1:run_noplan:{project_ref}"),
        ButtonKind::Success,
    ));
    components.push(component_button(
        "Back to Projects",
        "v1:projects".to_owned(),
        ButtonKind::Secondary,
    ));
    navigation_buttons(
        &mut components,
        page,
        ranges.len(),
        |target| format!("v1:project_page:{project_ref}:{target}"),
        |target| format!("v1:project_page:{project_ref}:{target}"),
    );

    OutgoingMessage {
        content,
        attachment: None,
        components,
        mention_allowlist: vec![operator_id],
    }
}

/// Render full README.
///
/// Displays one bounded page of the complete README, with navigation controls
/// when the content exceeds the platform message limit.
pub fn full_readme_message(
    project_name: &str,
    readme: Option<&str>,
    page: usize,
    operator_id: u64,
) -> OutgoingMessage {
    let project_ref = project_action_token(project_name);
    let safe_project_name = short_name(project_name, 160);
    let Some(readme) = readme else {
        return OutgoingMessage {
            content: format!("**Project: `{safe_project_name}`**\nNo README found."),
            attachment: None,
            components: vec![component_button(
                "Back to Project",
                format!("v1:project_select:{project_ref}"),
                ButtonKind::Primary,
            )],
            mention_allowlist: vec![operator_id],
        };
    };

    let header = format!("**Project: `{safe_project_name}` — Full README**\n");
    let pages = text_pages(readme, README_PAGE_BODY_BUDGET);
    let page = page.min(pages.len().saturating_sub(1));
    let current_page = pages.get(page).map(String::as_str).unwrap_or_default();
    let content = format!(
        "{header}_Page {} of {}_\n\n{current_page}",
        page + 1,
        pages.len()
    );
    let mut components = vec![component_button(
        "Back to Project",
        format!("v1:project_select:{project_ref}"),
        ButtonKind::Primary,
    )];
    navigation_buttons(
        &mut components,
        page,
        pages.len(),
        |target| format!("v1:readme_page:{project_ref}:{target}"),
        |target| format!("v1:readme_page:{project_ref}:{target}"),
    );
    OutgoingMessage {
        content,
        attachment: None,
        components,
        mention_allowlist: vec![operator_id],
    }
}

/// Render plan preview.
///
/// Shows the first 20 lines of a plan file as a bounded, fence-safe preview.
pub fn plan_preview_message(
    project_name: &str,
    plan_name: &str,
    content: &str,
    operator_id: u64,
) -> OutgoingMessage {
    let project_ref = project_action_token(project_name);
    let plan_ref = plan_action_token(project_name, plan_name);
    let project_title = short_name(project_name, 160);
    let plan_title = short_name(plan_name, 160);
    let header =
        format!("**Project: `{project_title}`**\n**Plan: `{plan_title}`**\n\n**Preview**\n```\n");
    let footer = "\n```\n";
    let available = DISCORD_MESSAGE_CAP
        .saturating_sub(header.len())
        .saturating_sub(footer.len())
        .saturating_sub(PLAN_PREVIEW_MARKER.len());
    let preview = content.lines().take(20).collect::<Vec<_>>().join("\n");
    let safe_preview = fence_neutralize(&preview);
    let (preview, truncated) = split_excerpt(&safe_preview, available);
    let truncation = if truncated { PLAN_PREVIEW_MARKER } else { "" };
    let content = format!("{header}{preview}{truncation}{footer}");

    OutgoingMessage {
        content,
        attachment: None,
        components: vec![
            component_button(
                "Load This Plan",
                format!("v1:load_plan:{project_ref}:{plan_ref}"),
                ButtonKind::Success,
            ),
            component_button(
                "Back to Project",
                format!("v1:project_select:{project_ref}"),
                ButtonKind::Secondary,
            ),
        ],
        mention_allowlist: vec![operator_id],
    }
}

fn text_pages(text: &str, max_bytes: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }

    let max_bytes = max_bytes.max(1);
    let mut pages = Vec::new();
    let mut page = String::new();
    for line in text.split_inclusive('\n') {
        if line.len() <= max_bytes {
            if page.len().saturating_add(line.len()) > max_bytes && !page.is_empty() {
                pages.push(std::mem::take(&mut page));
            }
            page.push_str(line);
            continue;
        }

        for character in line.chars() {
            if page.len().saturating_add(character.len_utf8()) > max_bytes && !page.is_empty() {
                pages.push(std::mem::take(&mut page));
            }
            page.push(character);
        }
    }
    if !page.is_empty() || pages.is_empty() {
        pages.push(page);
    }
    pages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::{plan_action_token, project_action_token};
    use crate::ports::{PlanInfo, ProjectDetails, ProjectInfo};
    use std::path::PathBuf;

    #[test]
    fn test_project_list_message_empty() {
        let message = project_list_message(&[], 0, 123);
        assert!(message.content.contains("No projects found"));
        assert!(message.components.is_empty());
    }

    #[test]
    fn test_project_list_message_with_projects() {
        let projects = vec![
            ProjectInfo {
                name: "user/project-alpha".to_owned(),
                dir: PathBuf::from("/home/user/project-alpha"),
            },
            ProjectInfo {
                name: "user/project-beta".to_owned(),
                dir: PathBuf::from("/home/user/project-beta"),
            },
        ];
        let message = project_list_message(&projects, 0, 123);
        assert!(message.content.contains("**Projects**"));
        assert!(message.content.contains("📁 `user/project-alpha`"));
        assert!(message.content.contains("📁 `user/project-beta`"));
        assert_eq!(message.components.len(), 2);
        assert_eq!(
            message.components[0].custom_id,
            format!(
                "v1:project_select:{}",
                project_action_token("user/project-alpha")
            )
        );
        assert_eq!(
            message.components[1].custom_id,
            format!(
                "v1:project_select:{}",
                project_action_token("user/project-beta")
            )
        );
        assert!(!message.content.contains("/home/user"));
        assert!(message.components.iter().all(|component| {
            component.custom_id.len() <= 100 && component.label.chars().count() <= 80
        }));
    }

    #[test]
    fn project_list_is_paginated_with_valid_selectors() {
        let projects: Vec<_> = (0..23)
            .map(|index| ProjectInfo {
                name: format!("user/project-{index:02}"),
                dir: PathBuf::from(format!("/private/root/project-{index:02}")),
            })
            .collect();

        let first = project_list_message(&projects, 0, 123);
        assert!(first.content.contains("Page 1 of 2"));
        assert_eq!(first.components.len(), 21);
        assert!(
            first
                .components
                .iter()
                .any(|component| { component.custom_id == "v1:projects:p:1" })
        );
        assert!(!first.content.contains("/private/root"));
        assert!(first.content.len() <= super::super::report::DISCORD_MESSAGE_CAP);

        let last = project_list_message(&projects, 1, 123);
        assert!(last.content.contains("Page 2 of 2"));
        assert_eq!(last.components.len(), 4);
        assert!(last.content.contains("project-22"));
        assert!(last.content.len() <= DISCORD_MESSAGE_CAP);
        assert!(
            last.components
                .iter()
                .any(|component| { component.custom_id == "v1:projects:p:0" })
        );
    }

    #[test]
    fn project_list_respects_exact_item_and_byte_page_boundaries() {
        let exact_item_page: Vec<_> = (0..PROJECTS_PAGE_SIZE)
            .map(|index| ProjectInfo {
                name: format!("user/project-{index:02}"),
                dir: PathBuf::from(format!("/private/project-{index:02}")),
            })
            .collect();
        let exact_page = project_list_message(&exact_item_page, 0, 123);
        assert!(exact_page.content.contains("Page 1 of 1"));
        assert_eq!(exact_page.components.len(), PROJECTS_PAGE_SIZE);

        let over_item_page: Vec<_> = (0..=PROJECTS_PAGE_SIZE)
            .map(|index| ProjectInfo {
                name: format!("user/project-{index:02}"),
                dir: PathBuf::from(format!("/private/project-{index:02}")),
            })
            .collect();
        let first_item_page = project_list_message(&over_item_page, 0, 123);
        assert!(first_item_page.content.contains("Page 1 of 2"));
        assert_eq!(first_item_page.components.len(), PROJECTS_PAGE_SIZE + 1);

        let line_prefix_bytes = "📁 ``\n".len();
        let exact_fit = vec![
            ProjectInfo {
                name: "a".repeat(800 - line_prefix_bytes),
                dir: PathBuf::from("/private/a"),
            },
            ProjectInfo {
                name: "b".repeat(PROJECT_LIST_TEXT_BUDGET - 800 - line_prefix_bytes),
                dir: PathBuf::from("/private/b"),
            },
        ];
        let exact_byte_page = project_list_message(&exact_fit, 0, 123);
        assert!(exact_byte_page.content.contains("Page 1 of 1"));
        assert_eq!(exact_byte_page.components.len(), 2);

        let over_budget = vec![
            ProjectInfo {
                name: "a".repeat(PROJECT_LIST_TEXT_BUDGET - line_prefix_bytes),
                dir: PathBuf::from("/private/a"),
            },
            ProjectInfo {
                name: "b".to_owned(),
                dir: PathBuf::from("/private/b"),
            },
        ];
        let split_byte_page = project_list_message(&over_budget, 0, 123);
        assert!(split_byte_page.content.contains("Page 1 of 2"));
        assert_eq!(split_byte_page.components.len(), 2);
    }

    #[test]
    fn test_project_details_message_with_readme_and_plans() {
        let details = ProjectDetails {
            name: "user/project-alpha".to_owned(),
            readme_preview: Some("# Test Project\n\nDescription".to_owned()),
            plans: vec![
                PlanInfo {
                    name: "plan1.md".to_owned(),
                    path: PathBuf::from("/home/user/project-alpha/devdocs/plans/plan1.md"),
                },
                PlanInfo {
                    name: "plan2.md".to_owned(),
                    path: PathBuf::from("/home/user/project-alpha/devdocs/plans/plan2.md"),
                },
            ],
        };
        let message = project_details_message(&details, 0, 123);
        assert!(
            message
                .content
                .contains("**Project: `user/project-alpha`**")
        );
        assert!(message.content.contains("**README Preview**"));
        assert!(message.content.contains("# Test Project"));
        assert!(
            message
                .content
                .contains("**Available Plans · page 1 of 1**")
        );
        assert!(message.components.len() >= 4); // README button + 2 plans + Run Without Plan + Back
        assert!(message.components.len() <= 25);
        assert!(message.content.contains("plan1.md"));
        assert!(message.content.contains("plan2.md"));
        assert!(!message.content.contains("/home/user"));
        assert!(message.content.len() <= DISCORD_MESSAGE_CAP);
        assert!(message.components.iter().all(|component| {
            component.custom_id.len() <= 100 && component.label.chars().count() <= 80
        }));
    }

    #[test]
    fn test_project_details_message_no_readme_no_plans() {
        let details = ProjectDetails {
            name: "user/empty-project".to_owned(),
            readme_preview: None,
            plans: vec![],
        };
        let message = project_details_message(&details, 0, 123);
        assert!(
            message
                .content
                .contains("**Project: `user/empty-project`**")
        );
        assert!(!message.content.contains("**README Preview**"));
        assert!(!message.content.contains("**Available Plans**"));
        assert!(message.content.contains("_No plans found._"));
    }

    #[test]
    fn project_details_page_keeps_plan_buttons_within_platform_limits() {
        let details = ProjectDetails {
            name: "user/project".to_owned(),
            readme_preview: None,
            plans: (0..40)
                .map(|index| PlanInfo {
                    name: format!("plan-{index:02}.md"),
                    path: PathBuf::from(format!("/private/plans/plan-{index:02}.md")),
                })
                .collect(),
        };

        let first = project_details_message(&details, 0, 123);
        assert!(first.content.contains("Plans · page 1 of 3"));
        assert!(first.content.len() <= DISCORD_MESSAGE_CAP);
        assert!(first.components.len() <= 25);
        assert!(
            first
                .components
                .iter()
                .all(|component| component.custom_id.len() <= 100)
        );
        assert!(first.components.iter().any(|component| {
            component.custom_id
                == format!(
                    "v1:plan_select:{}:{}",
                    project_action_token("user/project"),
                    plan_action_token("user/project", "plan-00.md")
                )
        }));
        assert!(first.components.iter().any(|component| {
            component.custom_id
                == format!("v1:project_page:{}:1", project_action_token("user/project"))
        }));

        let middle = project_details_message(&details, 1, 123);
        assert!(middle.content.contains("Plans · page 2 of 3"));
        assert!(middle.content.len() <= DISCORD_MESSAGE_CAP);
        assert!(middle.components.len() <= 25);
        assert!(middle.components.iter().any(|component| {
            component.custom_id
                == format!("v1:project_page:{}:0", project_action_token("user/project"))
        }));
        assert!(middle.components.iter().any(|component| {
            component.custom_id
                == format!("v1:project_page:{}:2", project_action_token("user/project"))
        }));
    }

    #[test]
    fn test_full_readme_message() {
        let readme = "# Full README\n\nThis is the full content.";
        let message = full_readme_message("user/project", Some(readme), 0, 123);
        assert!(
            message
                .content
                .contains("**Project: `user/project` — Full README**")
        );
        assert!(message.content.contains("# Full README"));
        assert_eq!(message.components.len(), 1);
        assert_eq!(message.components[0].label, "Back to Project");
    }

    #[test]
    fn test_full_readme_message_no_readme() {
        let message = full_readme_message("user/project", None, 0, 123);
        assert!(message.content.contains("**Project: `user/project`**"));
        assert!(message.content.contains("No README found"));
    }

    #[test]
    fn test_plan_preview_message() {
        let content = "# Plan\n\nStep 1\nStep 2\nStep 3\nStep 4\nStep 5\nStep 6\nStep 7\nStep 8\nStep 9\nStep 10\nStep 11\nStep 12\nStep 13\nStep 14\nStep 15\nStep 16\nStep 17\nStep 18\nStep 19\nStep 20\nStep 21";
        let message = plan_preview_message("user/project", "test-plan.md", content, 123);
        assert!(message.content.contains("**Project: `user/project`**"));
        assert!(message.content.contains("**Plan: `test-plan.md`**"));
        assert!(message.content.contains("**Preview**"));
        // Should only show first 20 lines
        assert!(!message.content.contains("Step 21"));
        assert_eq!(message.components.len(), 2);
        assert!(message.components.iter().any(|component| {
            component.custom_id
                == format!(
                    "v1:load_plan:{}:{}",
                    project_action_token("user/project"),
                    plan_action_token("user/project", "test-plan.md")
                )
        }));
        assert!(message.content.len() <= super::super::report::DISCORD_MESSAGE_CAP);
    }

    #[test]
    fn long_readme_is_split_into_bounded_pages() {
        let readme = format!("# README\n{}\nEND", "内容".repeat(2_000));
        let first = full_readme_message("user/project", Some(&readme), 0, 123);
        assert!(first.content.len() <= super::super::report::DISCORD_MESSAGE_CAP);
        assert!(first.content.contains("Page 1 of"));
        let next_page = first
            .components
            .iter()
            .find(|component| component.custom_id.starts_with("v1:readme_page:"));
        assert!(next_page.is_some());

        let last = full_readme_message("user/project", Some(&readme), 100, 123);
        assert!(last.content.len() <= super::super::report::DISCORD_MESSAGE_CAP);
        assert!(last.content.contains("END"));
        assert!(
            last.components
                .iter()
                .any(|component| { component.custom_id.starts_with("v1:readme_page:") })
        );
    }

    #[test]
    fn long_multibyte_readme_lines_are_split_without_invalid_utf8() {
        let readme = "é".repeat(4_000);
        let first = full_readme_message("user/project", Some(&readme), 0, 123);
        assert!(first.content.len() <= DISCORD_MESSAGE_CAP);
        assert!(first.content.contains("Page 1 of"));
        let second = full_readme_message("user/project", Some(&readme), 1, 123);
        assert!(second.content.len() <= DISCORD_MESSAGE_CAP);
        assert!(second.content.contains("Page 2 of"));
    }

    #[test]
    fn readme_page_byte_limit_keeps_exact_fit_characters_together() {
        let readme = "a".repeat(README_PAGE_BODY_BUDGET + 1);
        let first = full_readme_message("user/project", Some(&readme), 0, 123);
        let first_body = first
            .content
            .rsplit_once("\n\n")
            .map(|(_, body)| body)
            .unwrap_or_default();
        assert_eq!(first_body, "a".repeat(README_PAGE_BODY_BUDGET));

        let second = full_readme_message("user/project", Some(&readme), 1, 123);
        let second_body = second
            .content
            .rsplit_once("\n\n")
            .map(|(_, body)| body)
            .unwrap_or_default();
        assert_eq!(second_body, "a");
    }

    #[test]
    fn readme_lines_that_exactly_fill_a_page_stay_together() {
        let readme = format!("{}\n{}\nEND", "a".repeat(599), "b".repeat(649));
        let first = full_readme_message("user/project", Some(&readme), 0, 123);

        assert!(first.content.contains("Page 1 of 2"));
        assert!(first.content.contains(&"b".repeat(649)));
        assert!(first.content.len() <= DISCORD_MESSAGE_CAP);
    }

    #[test]
    fn oversized_plan_preview_is_clamped_and_fence_safe() {
        let content = format!("# Plan\n```danger\n{}\n```\nStep 21", "x".repeat(5_000));
        let message = plan_preview_message("user/project", "test-plan.md", &content, 123);

        assert!(message.content.len() <= super::super::report::DISCORD_MESSAGE_CAP);
        assert!(message.content.contains("...[truncated]"));
        assert!(!message.content.contains("```danger"));
        assert!(!message.content.contains("Step 21"));
    }

    #[test]
    fn byte_name_truncation_handles_zero_and_multibyte_limits() {
        assert_eq!(truncate_utf8_bytes("text", 0), "");
        assert_eq!(truncate_utf8_bytes("abcdef", 4), "a…");
        assert_eq!(truncate_utf8_bytes("éé", 3), "…");
        assert_eq!(truncate_utf8_bytes("éé", 2), "é");
    }

    #[test]
    fn names_cannot_break_out_of_their_inline_code_span() {
        // A backtick in a name would close the `…` span early and a control
        // character could smuggle raw markup, so both are rewritten.
        let hostile = "user/pro`ject\u{0007}-alpha";
        let projects = vec![ProjectInfo {
            name: hostile.to_owned(),
            dir: PathBuf::from("/private/hostile"),
        }];

        let list = project_list_message(&projects, 0, 123);
        assert!(list.content.contains("📁 `user/pro'ject -alpha`"));
        assert!(!list.content.contains('\u{0007}'));

        let details = ProjectDetails {
            name: hostile.to_owned(),
            readme_preview: None,
            plans: vec![PlanInfo {
                name: "plan`.md".to_owned(),
                path: PathBuf::from("/private/plan.md"),
            }],
        };
        let message = project_details_message(&details, 0, 123);
        assert!(message.content.contains("📄 `plan'.md`"));
        assert!(!message.content.contains('\u{0007}'));
        // The opaque token keeps hostile names within the platform id cap.
        assert!(
            message
                .components
                .iter()
                .filter(|component| component.custom_id.starts_with("v1:plan_select:"))
                .all(|component| component.custom_id.len() <= 100)
        );
    }

    #[test]
    fn readme_preview_shows_at_most_five_lines() {
        let details = ProjectDetails {
            name: "user/project".to_owned(),
            readme_preview: Some("1\n2\n3\n4\n5\n6\n7".to_owned()),
            plans: vec![],
        };
        let message = project_details_message(&details, 0, 123);
        assert!(message.content.contains("> 5"));
        assert!(!message.content.contains("> 6"));
    }

    #[test]
    fn empty_readme_is_a_single_blank_page() {
        let message = full_readme_message("user/project", Some(""), 0, 123);
        assert!(message.content.contains("Page 1 of 1"));
        assert!(
            message
                .components
                .iter()
                .all(|component| !component.custom_id.starts_with("v1:readme_page:"))
        );
    }
}
