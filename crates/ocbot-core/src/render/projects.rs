//! Project-centric message renderers (SPEC-005).
//!
//! This module provides the message rendering functions for the project-centric
//! remote control interface. All user-facing messages hide filesystem paths and
//! only expose opaque project names and plan names.

use super::{ButtonKind, OutgoingMessage, component_button};

/// Render list of projects.
///
/// Displays all available projects under the `projects_root` directory.
/// Projects are shown with their opaque display names only.
pub fn project_list_message(
    projects: &[crate::ports::ProjectInfo],
    operator_id: u64,
) -> OutgoingMessage {
    let content = if projects.is_empty() {
        "No projects found. Check `projects_root` in config.".to_owned()
    } else {
        let mut content = "**Projects**\n".to_owned();
        for project in projects {
            content.push('📁');
            content.push(' ');
            content.push_str(&project.name);
            content.push('\n');
        }
        content
    };

    OutgoingMessage {
        content,
        attachment: None,
        components: Vec::new(),
        mention_allowlist: vec![operator_id],
    }
}

/// Render project details (README preview + plans).
///
/// Shows the project's README preview (first 5 lines) and available plans.
/// Buttons are provided for navigating to full README, selecting plans, or
/// running without a plan.
pub fn project_details_message(
    details: &crate::ports::ProjectDetails,
    operator_id: u64,
) -> OutgoingMessage {
    let mut components = Vec::new();

    // README section button
    if details.readme_preview.is_some() {
        components.push(component_button(
            "Show Full README",
            format!("v1:readme_full:{}", details.name),
            ButtonKind::Secondary,
        ));
    }

    // Plans section buttons
    for plan in &details.plans {
        components.push(component_button(
            format!("📄 {}", plan.name),
            format!("v1:plan_select:{}:{}", details.name, plan.name),
            ButtonKind::Primary,
        ));
    }

    // Action buttons
    components.push(component_button(
        "Run Without Plan",
        format!("v1:run_noplan:{}", details.name),
        ButtonKind::Success,
    ));
    components.push(component_button(
        "Back to Projects",
        "v1:projects".to_owned(),
        ButtonKind::Secondary,
    ));

    let readme_text = details.readme_preview
        .as_deref()
        .map(|preview| format!("\n**README Preview**\n> {preview}\n"))
        .unwrap_or_default();

    let plans_text = if details.plans.is_empty() {
        String::new()
    } else {
        "\n**Available Plans**\n".to_owned()
    };

    let no_plans = if details.plans.is_empty() {
        "\n_No plans found._"
    } else {
        ""
    };

    let content = format!(
        "**Project: {}**{readme_text}{plans_text}{no_plans}",
        details.name
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
/// Displays the complete README content for a project.
pub fn full_readme_message(
    project_name: &str,
    readme: Option<&str>,
    operator_id: u64,
) -> OutgoingMessage {
    let content = match readme {
        Some(text) => format!("**Project: {project_name} - Full README**\n\n{text}"),
        None => format!("**Project: {project_name}**\nNo README found."),
    };

    OutgoingMessage {
        content,
        attachment: None,
        components: vec![component_button(
            "Back to Project",
            format!("v1:project_select:{project_name}"),
            ButtonKind::Primary,
        )],
        mention_allowlist: vec![operator_id],
    }
}

/// Render plan preview.
///
/// Shows the first 20 lines of a plan file as a preview.
pub fn plan_preview_message(
    project_name: &str,
    plan_name: &str,
    content: &str,
    operator_id: u64,
) -> OutgoingMessage {
    let preview = content.lines().take(20).collect::<Vec<_>>().join("\n");
    let content = format!(
        "**Project: {project_name}**\n**Plan: {plan_name}**\n\n**Preview**\n```\n{preview}\n```\n"
    );

    OutgoingMessage {
        content,
        attachment: None,
        components: vec![
            component_button(
                "Load This Plan",
                format!("v1:load_plan:{project_name}:{plan_name}"),
                ButtonKind::Success,
            ),
            component_button(
                "Back to Project",
                format!("v1:project_select:{project_name}"),
                ButtonKind::Secondary,
            ),
        ],
        mention_allowlist: vec![operator_id],
    }
}

/// Render staged preview for project-centric runs.
///
/// Shows the staged run information with project name but without exposing paths.
pub fn staged_preview_project(
    label: &str,
    _prompt: &str, // Don't show prompt to user
    action_id: i64,
    operator_id: u64,
    template: &str,
    project_name: &str,
) -> OutgoingMessage {
    let label = super::label::truncate_chars(
        &super::label::collapse_whitespace(label),
        super::report::LABEL_MAX_CHARS,
    );
    let content = format!(
        "**Ready to run: {label}**\n_Project: {project_name}_\n_Template: {template}_\n"
    );

    OutgoingMessage {
        content,
        attachment: None,
        components: vec![
            component_button("Send as-is", format!("v1:send:{action_id}"), ButtonKind::Primary),
            component_button("Edit & send", format!("v1:edit:{action_id}"), ButtonKind::Secondary),
            component_button(
                if template == "resume" {
                    "Use fresh template"
                } else {
                    "Use resume template"
                },
                format!("v1:tpl:{action_id}"),
                ButtonKind::Secondary,
            ),
        ],
        mention_allowlist: vec![operator_id],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::{PlanInfo, ProjectDetails, ProjectInfo};
    use std::path::PathBuf;

    #[test]
    fn test_project_list_message_empty() {
        let message = project_list_message(&[], 123);
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
        let message = project_list_message(&projects, 123);
        assert!(message.content.contains("**Projects**"));
        assert!(message.content.contains("📁 user/project-alpha"));
        assert!(message.content.contains("📁 user/project-beta"));
        assert!(message.components.is_empty());
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
        let message = project_details_message(&details, 123);
        assert!(message.content.contains("**Project: user/project-alpha**"));
        assert!(message.content.contains("**README Preview**"));
        assert!(message.content.contains("# Test Project"));
        assert!(message.content.contains("**Available Plans**"));
        assert!(message.components.len() >= 4); // README button + 2 plans + Run Without Plan + Back
    }

    #[test]
    fn test_project_details_message_no_readme_no_plans() {
        let details = ProjectDetails {
            name: "user/empty-project".to_owned(),
            readme_preview: None,
            plans: vec![],
        };
        let message = project_details_message(&details, 123);
        assert!(message.content.contains("**Project: user/empty-project**"));
        assert!(!message.content.contains("**README Preview**"));
        assert!(!message.content.contains("**Available Plans**"));
        assert!(message.content.contains("_No plans found._"));
    }

    #[test]
    fn test_full_readme_message() {
        let readme = "# Full README\n\nThis is the full content.";
        let message = full_readme_message("user/project", Some(readme), 123);
        assert!(message.content.contains("**Project: user/project - Full README**"));
        assert!(message.content.contains("# Full README"));
        assert!(message.components.len() == 1);
        assert!(message.components[0].label == "Back to Project");
    }

    #[test]
    fn test_full_readme_message_no_readme() {
        let message = full_readme_message("user/project", None, 123);
        assert!(message.content.contains("**Project: user/project**"));
        assert!(message.content.contains("No README found"));
    }

    #[test]
    fn test_plan_preview_message() {
        let content = "# Plan\n\nStep 1\nStep 2\nStep 3\nStep 4\nStep 5\nStep 6\nStep 7\nStep 8\nStep 9\nStep 10\nStep 11\nStep 12\nStep 13\nStep 14\nStep 15\nStep 16\nStep 17\nStep 18\nStep 19\nStep 20\nStep 21";
        let message = plan_preview_message("user/project", "test-plan.md", content, 123);
        assert!(message.content.contains("**Project: user/project**"));
        assert!(message.content.contains("**Plan: test-plan.md**"));
        assert!(message.content.contains("**Preview**"));
        // Should only show first 20 lines
        assert!(!message.content.contains("Step 21"));
        assert!(message.components.len() == 2);
    }

    #[test]
    fn test_staged_preview_project() {
        let message = staged_preview_project("Test Label", "prompt", 42, 123, "fresh", "user/project");
        assert!(message.content.contains("**Ready to run: Test Label**"));
        assert!(message.content.contains("_Project: user/project_"));
        assert!(message.content.contains("_Template: fresh_"));
        assert!(message.components.len() == 3);
    }
}
