//! Per-surface delivery policy (SPEC-004 D-12/FR-102/FR-107): how much of a
//! report reaches one surface. The handler builds one generic
//! [`OutgoingMessage`] spec; the *delivery path* transforms it per surface
//! from config — handlers never read surface config (FR-107), and the
//! transformation is a pure function here so it is unit-testable and shared
//! by every port wrapper.

use serde::{Deserialize, Serialize};

use super::OutgoingMessage;

/// How much of a report reaches a surface inline (D-12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportExcerpt {
    /// The whole report inline.
    #[serde(rename = "full")]
    Full,
    /// The report inline up to the excerpt budget (Discord's default).
    #[serde(rename = "excerpt")]
    Excerpt,
    /// Headline + verdict only.
    #[serde(rename = "off")]
    Off,
}

/// One surface's delivery policy, resolved from its config section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfacePolicy {
    /// How much of the report body goes inline.
    pub excerpt: ReportExcerpt,
    /// Whether the `report.md` attachment may leave the host at all.
    pub attachments: bool,
}

/// The hint appended when a truncated report's attachment is suppressed:
/// the operator must know where the full body still lives (D-12).
pub const SUPPRESSED_ATTACHMENT_HINT: &str =
    "*(full report not attached — it is stored in the bot's database; `/history` shows it)*";

/// Apply one surface's policy to an outgoing spec (pure; D-12):
///
/// - `attachments = false` drops the attachment, and a spec that *had* one
///   (the renderer attaches exactly when the inline excerpt was truncated)
///   gains the [`SUPPRESSED_ATTACHMENT_HINT`] line instead.
/// - `excerpt = off` strips the fenced report body entirely, leaving the
///   headline and the always-inline footer (the verdict).
/// - `full`/`excerpt` leave the inline content alone — the renderer already
///   budgets it against the surface cap, and the attachment (when kept)
///   carries the whole body.
///
/// Components and the mention allowlist pass through untouched (FR-3/FR-15
/// are not policy).
pub fn apply_surface_policy(spec: &OutgoingMessage, policy: &SurfacePolicy) -> OutgoingMessage {
    let mut content = spec.content.clone();
    if policy.excerpt == ReportExcerpt::Off {
        content = strip_fenced_body(&content);
    }
    let attachment = if policy.attachments {
        spec.attachment.clone()
    } else {
        if spec.attachment.is_some() && policy.excerpt != ReportExcerpt::Off {
            if !content.is_empty() {
                content.push('\n');
            }
            content.push_str(SUPPRESSED_ATTACHMENT_HINT);
        }
        None
    };
    OutgoingMessage {
        content,
        attachment,
        components: spec.components.clone(),
        mention_allowlist: spec.mention_allowlist.clone(),
    }
}

/// Remove the first fenced code block from `content`, keeping everything
/// before and after it (headline before, footer/verdict after). Content with
/// no fence is returned unchanged.
fn strip_fenced_body(content: &str) -> String {
    let Some(open) = content.find("```") else {
        return content.to_owned();
    };
    let after_open = open + 3;
    // Skip the rest of the opening fence line (a language tag).
    let body_start = content[after_open..]
        .find('\n')
        .map_or(content.len(), |newline| after_open + newline + 1);
    let Some(close) = content[body_start..].find("\n```") else {
        // An unterminated fence: keep the headline, drop the tail.
        return content[..open].trim_end().to_owned();
    };
    let body_end = body_start + close + 4; // consume "\n```"
    let mut kept = String::new();
    kept.push_str(content[..open].trim_end());
    let tail = content[body_end..].trim_start_matches('\n');
    if !tail.is_empty() {
        if !kept.is_empty() {
            kept.push('\n');
        }
        kept.push_str(tail);
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::{ReportExcerpt, SurfacePolicy, apply_surface_policy, strip_fenced_body};
    use crate::render::OutgoingMessage;

    fn spec(content: &str, attachment: Option<&str>) -> OutgoingMessage {
        OutgoingMessage {
            content: content.to_owned(),
            attachment: attachment.map(|text| crate::render::AttachmentSpec {
                filename: "report.md".to_owned(),
                bytes: text.as_bytes().to_vec(),
            }),
            components: Vec::new(),
            mention_allowlist: vec![42],
        }
    }

    const REPORT: &str =
        "✅ completed **fix the bug**\n```\nthe long body\nmore body\n```\nΔ +12 −3 · 4.2s";

    fn policy(excerpt: ReportExcerpt, attachments: bool) -> SurfacePolicy {
        SurfacePolicy {
            excerpt,
            attachments,
        }
    }

    #[test]
    fn full_with_attachments_passes_through() {
        let original = spec(REPORT, Some("full body"));
        let out = apply_surface_policy(&original, &policy(ReportExcerpt::Full, true));
        assert_eq!(out, original);
    }

    #[test]
    fn excerpt_is_the_inline_default_and_changes_nothing() {
        let original = spec(REPORT, Some("full body"));
        let out = apply_surface_policy(&original, &policy(ReportExcerpt::Excerpt, true));
        assert_eq!(out, original, "excerpt keeps the renderer's budgeting");
    }

    #[test]
    fn suppressed_attachments_gain_the_hint_line() {
        let out = apply_surface_policy(
            &spec(REPORT, Some("full body")),
            &policy(ReportExcerpt::Excerpt, false),
        );
        assert!(out.attachment.is_none());
        assert!(out.content.contains("✅ completed"));
        assert!(out.content.contains("Δ +12 −3"));
        assert!(out.content.contains(super::SUPPRESSED_ATTACHMENT_HINT));
    }

    #[test]
    fn suppression_without_an_attachment_adds_no_hint() {
        let out = apply_surface_policy(&spec(REPORT, None), &policy(ReportExcerpt::Excerpt, false));
        assert_eq!(out.content, REPORT);
        assert!(out.attachment.is_none());
    }

    #[test]
    fn off_strips_the_fenced_body_but_keeps_headline_and_verdict() {
        let out = apply_surface_policy(
            &spec(REPORT, Some("full body")),
            &policy(ReportExcerpt::Off, false),
        );
        assert_eq!(out.content, "✅ completed **fix the bug**\nΔ +12 −3 · 4.2s");
        assert!(out.attachment.is_none(), "off implies nothing to attach to");
        assert!(!out.content.contains("the long body"));
    }

    #[test]
    fn mention_allowlist_and_components_pass_through() {
        let mut original = spec(REPORT, None);
        original.components = vec![crate::render::component_button(
            "Accept",
            "v1:accept:1",
            crate::render::ButtonKind::Success,
        )];
        let out = apply_surface_policy(&original, &policy(ReportExcerpt::Off, false));
        assert_eq!(out.mention_allowlist, vec![42]);
        assert_eq!(out.components, original.components);
    }

    #[test]
    fn fences_without_a_closing_marker_keep_the_headline() {
        assert_eq!(strip_fenced_body("head\n```\nunterminated"), "head");
        assert_eq!(strip_fenced_body("no fence at all"), "no fence at all");
    }
}
