//! The plan-step picker surface (P12): the only browse-like list the operator
//! ever sees is their own plan's headings (SPEC-005 removed the `/ls` browser;
//! this picker names no paths).

use crate::plan::{OTHER_OPTION_LABEL, PLAN_PAGE_SIZE, PickOption};

use super::label::inline_safe;
use super::{ButtonKind, OutgoingMessage, component_button};

/// Render the plan-step picker (P12): one button per option on this page, paging,
/// and the always-present "Other: type it" / "Use typed label" affordances.
pub fn plan_picker_message(
    label: &str,
    options: &[PickOption],
    page: usize,
    action_id: i64,
    operator_id: u64,
) -> OutgoingMessage {
    let page_data = crate::plan::plan_page(options, page, PLAN_PAGE_SIZE);
    let mut content = format!(
        "**Plan steps — {}**\n{} option(s) · page {}/{}\n_pick a step to prefill the label; the prompt is still yours to edit._",
        inline_safe(label),
        options.len(),
        page_data.page + 1,
        page_data.total_pages
    );
    let mut components = Vec::new();
    for (offset, option) in page_data.options.iter().enumerate() {
        let index = page_data.page * PLAN_PAGE_SIZE + offset;
        let button_label: String = inline_safe(&option.label).chars().take(60).collect();
        components.push(component_button(
            button_label,
            format!("v1:plan:{action_id}:{index}"),
            ButtonKind::Primary,
        ));
    }
    if page_data.has_prev() {
        components.push(component_button(
            "◀ Prev",
            format!("v1:planp:{action_id}:{}", page_data.page - 1),
            ButtonKind::Secondary,
        ));
    }
    if page_data.has_next() {
        components.push(component_button(
            "Next ▶",
            format!("v1:planp:{action_id}:{}", page_data.page + 1),
            ButtonKind::Secondary,
        ));
    }
    components.push(component_button(
        OTHER_OPTION_LABEL,
        format!("v1:planother:{action_id}"),
        ButtonKind::Secondary,
    ));
    components.push(component_button(
        "Use typed label",
        format!("v1:planuse:{action_id}"),
        ButtonKind::Secondary,
    ));
    // Content lists the section context through the buttons; keep the header
    // short but non-empty (Discord rejects empty content without components).
    content.push_str("\n_options are the plan's list items under the chosen headings._");
    OutgoingMessage {
        content,
        attachment: None,
        components,
        mention_allowlist: vec![operator_id],
    }
}

#[cfg(test)]
mod tests {
    use super::plan_picker_message;
    use crate::plan::PLAN_PAGE_SIZE;

    #[test]
    fn plan_picker_pages_and_always_offers_other() {
        let options: Vec<crate::plan::PickOption> = (0..PLAN_PAGE_SIZE + 5)
            .map(|index| crate::plan::PickOption {
                label: format!("step {index}"),
                section: None,
            })
            .collect();
        let message = plan_picker_message("task", &options, 0, 3, 7);
        assert!(
            message
                .components
                .iter()
                .any(|c| c.custom_id == "v1:plan:3:0")
        );
        assert!(
            message
                .components
                .iter()
                .any(|c| c.custom_id == "v1:planp:3:1")
        );
        assert!(
            message
                .components
                .iter()
                .any(|c| c.custom_id == "v1:planother:3")
        );
        assert!(
            message
                .components
                .iter()
                .any(|c| c.custom_id == "v1:planuse:3")
        );
    }

    #[test]
    fn later_pages_carry_global_option_indices() {
        // The index in `v1:plan:<action>:<index>` must count from the start of
        // the whole option list, not the page — a page-relative index would
        // make a pick hit the wrong step.
        let options: Vec<crate::plan::PickOption> = (0..PLAN_PAGE_SIZE + 5)
            .map(|index| crate::plan::PickOption {
                label: format!("step {index}"),
                section: None,
            })
            .collect();
        let second = plan_picker_message("task", &options, 1, 11, 42);
        assert!(second.content.contains("page 2/"), "{}", second.content);
        assert!(
            second
                .components
                .iter()
                .any(|c| c.custom_id == format!("v1:plan:11:{PLAN_PAGE_SIZE}")),
            "the page's first option keeps its global index"
        );
        assert!(
            second
                .components
                .iter()
                .any(|c| c.custom_id == format!("v1:plan:11:{}", PLAN_PAGE_SIZE + 4)),
            "and its last option too"
        );
        assert!(
            second
                .components
                .iter()
                .any(|c| c.custom_id == "v1:planp:11:0"),
            "page 1 offers a way back"
        );
    }
}
