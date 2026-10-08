//! Light, read-only markdown plan parsing (spec P12, NG-4).
//!
//! The plan file is an input, never tracked state. This module parses it fresh
//! every time it is opened and returns a flat list of pick options: the
//! ordered/unordered list items that appear under headings within the
//! operator-chosen depth range (default `H2`–`H3`). A heading inside the range
//! that has no list items and no deeper child heading is itself an option, so
//! plans that use headings as steps still work. A pick only ever *prefills* the
//! label; the operator still reviews before anything is sent.
//!
//! The module is pure (no I/O): the shell reads the file through the fs adapter
//! and hands the text here.

use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};

use crate::render::collapse_whitespace;

/// Default minimum heading depth included in the picker (spec P12: 2–3).
pub const DEFAULT_HEADING_MIN: u32 = 2;
/// Default maximum heading depth included in the picker (spec P12: 2–3).
pub const DEFAULT_HEADING_MAX: u32 = 3;
/// Options per picker page. Stays well under Discord's 25-component cap once
/// paging/Other buttons are added (P10).
pub const PLAN_PAGE_SIZE: usize = 20;
/// The "Other: type it" affordance is always present (FR-6/P12).
pub const OTHER_OPTION_LABEL: &str = "Other: type it";

/// One selectable plan item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickOption {
    /// The text used to prefill the action label.
    pub label: String,
    /// The nearest enclosing heading text, when one exists (display context).
    pub section: Option<String>,
}

/// One page of pick choices plus its paging metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanPage {
    /// The options on this page.
    pub options: Vec<PickOption>,
    /// The zero-based page index (clamped into range).
    pub page: usize,
    /// The total number of pages (always at least 1).
    pub total_pages: usize,
}

impl PlanPage {
    /// Whether a previous page exists.
    pub fn has_prev(&self) -> bool {
        self.page > 0
    }

    /// Whether a next page exists.
    pub fn has_next(&self) -> bool {
        self.page + 1 < self.total_pages
    }
}

/// A heading section being collected.
struct Frame {
    level: u32,
    text: String,
    items: Vec<String>,
    has_child: bool,
}

/// Parse `markdown` into pick options for heading depths `min_depth..=max_depth`.
///
/// The depth pair is normalized (swapped when reversed) so a misconfigured
/// range degrades instead of silently returning nothing.
pub fn parse_plan(markdown: &str, min_depth: u32, max_depth: u32) -> Vec<PickOption> {
    let (min, max) = if min_depth <= max_depth {
        (min_depth, max_depth)
    } else {
        (max_depth, min_depth)
    };

    let mut frames: Vec<Frame> = Vec::new();
    let mut in_heading = false;
    let mut heading_depth = 0_u32;
    let mut heading_text = String::new();
    let mut in_item = false;
    let mut item_text = String::new();

    for event in Parser::new(markdown) {
        match event {
            Event::Start(Tag::Heading { level, .. }) => {
                in_heading = true;
                heading_depth = depth_of(level);
                heading_text.clear();
            }
            Event::End(TagEnd::Heading(_)) => {
                in_heading = false;
                let frame = Frame {
                    level: heading_depth,
                    text: collapse_whitespace(&heading_text),
                    items: Vec::new(),
                    has_child: false,
                };
                if let Some(previous) = frames.last_mut()
                    && frame.level > previous.level
                {
                    previous.has_child = true;
                }
                frames.push(frame);
            }
            Event::Start(Tag::Item) => {
                in_item = true;
                item_text.clear();
            }
            Event::End(TagEnd::Item) => {
                if in_item {
                    in_item = false;
                    let text = clean_item(&item_text);
                    if !text.is_empty()
                        && let Some(frame) = frames.last_mut()
                    {
                        frame.items.push(text);
                    }
                }
            }
            Event::Text(text) | Event::Code(text) => {
                if in_heading {
                    heading_text.push_str(&text);
                } else if in_item {
                    item_text.push_str(&text);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if in_heading {
                    heading_text.push(' ');
                } else if in_item {
                    item_text.push(' ');
                }
            }
            _ => {}
        }
    }

    let mut options = Vec::new();
    for frame in &frames {
        if frame.level < min || frame.level > max {
            continue;
        }
        let section = (!frame.text.is_empty()).then(|| frame.text.clone());
        if frame.items.is_empty() {
            if !frame.has_child && !frame.text.is_empty() {
                options.push(PickOption {
                    label: frame.text.clone(),
                    section,
                });
            }
        } else {
            for item in &frame.items {
                options.push(PickOption {
                    label: item.clone(),
                    section: section.clone(),
                });
            }
        }
    }
    options
}

/// Slice `options` into the requested page (clamped) with paging metadata.
pub fn plan_page(options: &[PickOption], page: usize, page_size: usize) -> PlanPage {
    let page_size = page_size.max(1);
    let total_pages = options.len().div_ceil(page_size).max(1);
    let page = page.min(total_pages - 1);
    let start = page * page_size;
    let end = (start + page_size).min(options.len());
    PlanPage {
        options: options[start..end].to_vec(),
        page,
        total_pages,
    }
}

/// Map a pulldown-cmark heading level to its numeric depth.
fn depth_of(level: HeadingLevel) -> u32 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

/// Normalize an item's text: drop a leading markdown checkbox marker, then
/// collapse runs of whitespace (labels are single-line).
fn clean_item(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_checkbox = ["[ ]", "[x]", "[X]"]
        .iter()
        .find_map(|marker| trimmed.strip_prefix(marker))
        .map_or(trimmed, str::trim_start);
    collapse_whitespace(without_checkbox)
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_HEADING_MAX, DEFAULT_HEADING_MIN, OTHER_OPTION_LABEL, PLAN_PAGE_SIZE, parse_plan,
        plan_page,
    };

    const PLAN: &str = "\
# Title
## Steps
1. Fix the parser
2. Add a test
### Notes
- a bullet item
## Later
";

    #[test]
    fn picks_ordered_items_under_depth_range() {
        let options = parse_plan(PLAN, DEFAULT_HEADING_MIN, DEFAULT_HEADING_MAX);
        let labels: Vec<&str> = options.iter().map(|o| o.label.as_str()).collect();
        assert!(labels.contains(&"Fix the parser"), "{labels:?}");
        assert!(labels.contains(&"Add a test"), "{labels:?}");
        assert!(labels.contains(&"a bullet item"), "{labels:?}");
        // H1 is below the minimum depth, so its text is not an option.
        assert!(!labels.contains(&"Title"), "{labels:?}");
        assert_eq!(options[0].section.as_deref(), Some("Steps"));
        assert_eq!(OTHER_OPTION_LABEL, "Other: type it");
    }

    #[test]
    fn heading_without_items_is_an_option_unless_nested() {
        let markdown = "## Phase one\n## Phase two\n1. do it\n";
        let options = parse_plan(markdown, 2, 2);
        let labels: Vec<&str> = options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, vec!["Phase one", "do it"]);
    }

    #[test]
    fn pagination_clamps_and_reports_metadata() {
        let mut options = Vec::new();
        for index in 0..=(PLAN_PAGE_SIZE * 2) {
            options.push(super::PickOption {
                label: format!("step {index}"),
                section: None,
            });
        }
        let last = plan_page(&options, 99, PLAN_PAGE_SIZE);
        assert_eq!(last.page, 2);
        assert_eq!(last.total_pages, 3);
        assert_eq!(last.options.len(), 1);
        assert!(!last.has_next());
        assert!(last.has_prev());
    }

    #[test]
    fn checkbox_markers_are_stripped() {
        let markdown = "## Steps\n- [ ] first\n- [x] second\n";
        let options = parse_plan(markdown, 2, 2);
        let labels: Vec<&str> = options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, vec!["first", "second"]);
    }

    #[test]
    fn reversed_depth_range_is_normalized() {
        let parsed = parse_plan(PLAN, 3, 2);
        assert!(parsed.iter().any(|option| option.label == "Fix the parser"));
    }
}
