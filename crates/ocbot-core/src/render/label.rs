//! Shared text helpers for the render layer: whitespace normalization and
//! inline-markdown neutralization.
//!
//! [`collapse_whitespace`] is the single implementation shared with the plan
//! parser (SPEC-002 FR-12): the report headline sanitizer and `plan::clean_item`
//! both call it, so a label can never inject structural markdown and a plan item
//! is always single-line.

/// Collapse every whitespace run to a single space and trim both ends.
///
/// This is the consolidated helper (SPEC-002 FR-12): the render label
/// sanitizer and the plan parser's item normalizer were the same operation
/// written twice, and now exist exactly once.
pub(crate) fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
            }
            pending_space = false;
            out.push(ch);
        }
    }
    out
}

/// Neutralize a value for inline (non-fenced) markdown: collapse whitespace and
/// drop backticks so a label cannot break the surrounding row structure.
pub(crate) fn inline_safe(text: &str) -> String {
    collapse_whitespace(text).replace('`', "'")
}

/// Truncate `text` to at most `max_chars` characters on a char boundary,
/// marking the cut with an ellipsis. Callers budget message sizes in bytes;
/// a char cap merely bounds the worst case (each char is at least its own
/// byte), while the byte accounting done on the clamped result stays exact.
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    let mut clamped: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    clamped.push('…');
    clamped
}

#[cfg(test)]
mod tests {
    use super::{collapse_whitespace, inline_safe};

    #[test]
    fn collapse_trims_and_squashes_whitespace() {
        assert_eq!(collapse_whitespace("  a\n\t b  "), "a b");
        assert_eq!(collapse_whitespace("single"), "single");
        assert_eq!(collapse_whitespace("   \n\t"), "");
    }

    #[test]
    fn inline_safe_drops_backticks_after_collapsing() {
        assert_eq!(inline_safe("a`b  c"), "a'b c");
        assert_eq!(inline_safe("  `x`  "), "'x'");
    }

    #[test]
    fn truncate_is_identity_within_budget() {
        assert_eq!(super::truncate_chars("short", 10), "short");
        assert_eq!(super::truncate_chars("", 0), "");
    }

    #[test]
    fn truncate_cuts_on_a_char_boundary_with_an_ellipsis() {
        // 2-byte chars: the cut can never land mid-codepoint.
        assert_eq!(super::truncate_chars("ééééé", 3), "éé…");
        assert_eq!(super::truncate_chars(&"x".repeat(500), 8), "xxxxxxx…");
    }
}
