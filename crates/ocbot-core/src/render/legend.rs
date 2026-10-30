//! The reaction legend (SPEC-004 D-05/FR-106): the single source of truth for
//! the verb→emoji table a surface without buttons appends to a message that
//! carries components, and for the `component_map` entries that resolve an
//! inbound reaction back to a `custom_id`.
//!
//! Everything here is **pure** (no I/O, no store): the adapter renders the
//! legend for an [`OutgoingMessage`](super::OutgoingMessage) and records the
//! returned entries; the inbound side normalizes an emoji and looks it up.
//! `tests/legend_prop.rs` proves the legend and the entry list never drift.

use super::ComponentSpec;

/// The accept emoji (U+2705).
pub const ACCEPT_EMOJI: &str = "✅";
/// The park emoji (U+1F17F U+FE0F).
pub const PARK_EMOJI: &str = "🅿️";
/// The follow-up emoji (U+1F501).
pub const FOLLOWUP_EMOJI: &str = "🔁";
/// The discard emoji (U+1F5D1 U+FE0F).
pub const DISCARD_EMOJI: &str = "🗑️";

/// The keycap digits 1..=9 (D-05: gate options map by index; a gate with more
/// than nine options renders text-only on surfaces without buttons).
const KEYCAPS: [&str; 9] = ["1️⃣", "2️⃣", "3️⃣", "4️⃣", "5️⃣", "6️⃣", "7️⃣", "8️⃣", "9️⃣"];

/// The keycap for a zero-based option index, if one exists.
pub fn keycap(index: usize) -> Option<&'static str> {
    KEYCAPS.get(index).copied()
}

/// The fixed emoji for a decision `custom_id`, when it has one.
pub fn emoji_for_custom_id(custom_id: &str) -> Option<&'static str> {
    if custom_id.starts_with("v1:accept:") {
        Some(ACCEPT_EMOJI)
    } else if custom_id.starts_with("v1:park:") {
        Some(PARK_EMOJI)
    } else if custom_id.starts_with("v1:followup:") {
        Some(FOLLOWUP_EMOJI)
    } else if custom_id.starts_with("v1:discard:") {
        Some(DISCARD_EMOJI)
    } else {
        None
    }
}

/// A human verb for a `custom_id`, for the legend text (Discord labels are not
/// always short enough for a phone line).
pub fn verb_label(custom_id: &str) -> Option<&'static str> {
    if custom_id.starts_with("v1:accept:") {
        Some("accept")
    } else if custom_id.starts_with("v1:park:") {
        Some("park")
    } else if custom_id.starts_with("v1:followup:") {
        Some("follow up")
    } else if custom_id.starts_with("v1:discard:") {
        Some("discard")
    } else if custom_id.starts_with("v1:send:") {
        Some("send")
    } else if custom_id.starts_with("v1:edit:") {
        Some("edit")
    } else if custom_id.starts_with("v1:gate:") {
        Some("once/always/reject")
    } else {
        None
    }
}

/// One mapped reaction: the emoji to add and the `custom_id` it resolves to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegendEntry {
    /// The reaction emoji (canonical form).
    pub emoji: String,
    /// The `custom_id` the reaction stands in for.
    pub custom_id: String,
    /// The human verb shown in the legend line.
    pub label: String,
}

/// A rendered legend: the text line appended to the message and the entries to
/// record in `component_map`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Legend {
    /// The text line (`"✅ accept · 🔁 follow up"`), or empty when nothing maps.
    pub line: String,
    /// The reaction→custom-id entries (disabled verbs excluded, FR-15/FR-106).
    pub entries: Vec<LegendEntry>,
}

impl Legend {
    /// Whether any component mapped to a reaction.
    pub fn is_empty(&self) -> bool {
        self.line.is_empty() && self.entries.is_empty()
    }
}

/// Strip variation selectors (U+FE0F/U+FE0E) so a client that omits them still
/// resolves (SPEC-004 §8 risk 3). Skin-tone modifiers are likewise dropped:
/// 👍🏽 and 👍 must resolve to the same entry.
pub fn normalize_emoji(emoji: &str) -> String {
    emoji
        .chars()
        .filter(|ch| !matches!(*ch, '\u{FE0F}' | '\u{FE0E}' | '\u{1F3FB}'..='\u{1F3FF}'))
        .collect()
}

/// Render the legend for a message's components.
///
/// Fixed decision verbs map to their emoji; gate components map to keycaps by
/// order. Every other component (paging, plan picker, history rows, send/edit)
/// has no reaction: it is listed as a text hint by the caller, never here. A
/// disabled component is named in the line struck-through but is deliberately
/// **absent** from the entries — reacting on it resolves to nothing (FR-15).
pub fn render_legend(components: &[ComponentSpec]) -> Legend {
    let mut parts: Vec<String> = Vec::new();
    let mut entries: Vec<LegendEntry> = Vec::new();
    let mut gate_index: usize = 0;
    for component in components {
        let custom_id = component.custom_id.as_str();
        let mapped: Option<(String, String)> = if let Some(emoji) = emoji_for_custom_id(custom_id) {
            Some((
                emoji.to_owned(),
                verb_label(custom_id).unwrap_or("action").to_owned(),
            ))
        } else if custom_id.starts_with("v1:gate:") {
            match keycap(gate_index) {
                Some(keycap) => {
                    gate_index += 1;
                    Some((
                        keycap.to_owned(),
                        verb_label(custom_id).unwrap_or("gate").to_owned(),
                    ))
                }
                // Past nine options: text-only (D-05).
                None => continue,
            }
        } else {
            None
        };
        let Some((emoji, label)) = mapped else {
            continue;
        };
        if component.disabled {
            parts.push(format!("~~{emoji} {label}~~"));
        } else {
            parts.push(format!("{emoji} {label}"));
            entries.push(LegendEntry {
                emoji,
                custom_id: custom_id.to_owned(),
                label,
            });
        }
    }
    Legend {
        line: parts.join(" · "),
        entries,
    }
}

/// The command table appended to a `help` reply (FR-105). Pure and static so
/// the adapter can post it without touching the store.
pub fn help_text() -> String {
    let lines = vec![
        "ocbot commands:".to_owned(),
        "`run [dir]` — start a run (then send the prompt)".to_owned(),
        "`prompt [text]` — prompt the current directory's session".to_owned(),
        "`status` · `abort` · `release` · `mute` · `reset`".to_owned(),
        "`projects` — browse projects and their plans".to_owned(),
        "`history [page]` — git history".to_owned(),
        "`send [label]` — send the staged action".to_owned(),
        "`edit [label]` — rewrite the staged prompt".to_owned(),
        "`accept|park|follow|discard [label]` — decide the latest report".to_owned(),
        "`gate <label> <once|always|reject>` — answer a permission gate".to_owned(),
        "`cancel` — close an open flow".to_owned(),
        format!(
            "reactions: {ACCEPT_EMOJI} accept · {FOLLOWUP_EMOJI} follow up · {PARK_EMOJI} park · {DISCARD_EMOJI} discard"
        ),
    ];
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::{
        ACCEPT_EMOJI, LegendEntry, PARK_EMOJI, help_text, keycap, normalize_emoji, render_legend,
    };
    use crate::render::ButtonKind;
    use crate::render::component_button;

    #[test]
    fn decision_buttons_map_to_fixed_emoji() {
        let components = vec![
            component_button("Accept", "v1:accept:7", ButtonKind::Success),
            component_button("Park", "v1:park:7", ButtonKind::Secondary),
        ];
        let legend = render_legend(&components);
        assert_eq!(
            legend.line,
            format!("{ACCEPT_EMOJI} accept · {PARK_EMOJI} park")
        );
        assert_eq!(
            legend.entries,
            vec![
                LegendEntry {
                    emoji: ACCEPT_EMOJI.to_owned(),
                    custom_id: "v1:accept:7".to_owned(),
                    label: "accept".to_owned(),
                },
                LegendEntry {
                    emoji: PARK_EMOJI.to_owned(),
                    custom_id: "v1:park:7".to_owned(),
                    label: "park".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn disabled_verbs_are_struck_through_and_not_recorded() {
        let mut button = component_button("Accept", "v1:accept:7", ButtonKind::Success);
        button.disabled = true;
        let legend = render_legend(&[button]);
        assert!(legend.line.contains("~~"), "{}", legend.line);
        assert!(legend.entries.is_empty());
    }

    #[test]
    fn gate_options_map_by_index() {
        let components = vec![
            component_button("once", "v1:gate:9:once", ButtonKind::Success),
            component_button("always", "v1:gate:9:always", ButtonKind::Primary),
            component_button("reject", "v1:gate:9:reject", ButtonKind::Danger),
        ];
        let legend = render_legend(&components);
        assert_eq!(legend.entries[0].emoji, keycap(0).unwrap());
        assert_eq!(legend.entries[0].custom_id, "v1:gate:9:once");
        assert_eq!(legend.entries[2].emoji, keycap(2).unwrap());
    }

    #[test]
    fn navigation_components_are_not_mapped() {
        let components = vec![component_button(
            "Next",
            "v1:page:history:2",
            ButtonKind::Secondary,
        )];
        let legend = render_legend(&components);
        assert!(legend.is_empty());
    }

    #[test]
    fn variation_selectors_are_normalized() {
        assert_eq!(normalize_emoji("🅿️"), "🅿");
        assert_eq!(normalize_emoji("🅿"), "🅿");
        assert_eq!(normalize_emoji(ACCEPT_EMOJI), ACCEPT_EMOJI);
    }

    #[test]
    fn help_text_advertises_the_projects_verb_not_the_removed_browser() {
        // SPEC-005: the filesystem browser is gone; the command table must
        // reflect exactly what the surfaces can still route.
        let text = help_text();
        assert!(text.contains("`projects`"), "{text}");
        assert!(!text.contains("ls [dir]"), "{text}");
        assert!(!text.contains("cd <dir>"), "{text}");
    }

    #[test]
    fn every_verb_advertised_by_help_text_resolves_on_the_surfaces() {
        // Each backticked token in the command table must either be a verb the
        // shared grammar knows (argument-missing still proves it routes) or be
        // the flow-cancel keyword; a verb the bot advertises but no surface
        // routes is a dead command (SPEC-005 phase 10).
        let verbs: Vec<String> = help_text()
            .split('`')
            .skip(1)
            .step_by(2)
            .map(|token| {
                token
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })
            .flat_map(|head| head.split('|').map(str::to_owned).collect::<Vec<_>>())
            .filter(|verb| !verb.is_empty())
            .collect();
        assert!(!verbs.is_empty(), "the command table must advertise verbs");
        for verb in verbs {
            // `Ok` or `MissingArgument` both prove the verb is a real grammar
            // entry; only an unknown verb means the table advertises a dead
            // command.
            let known = matches!(
                crate::grammar::parse(&verb),
                Ok(_) | Err(crate::grammar::GrammarError::MissingArgument { .. })
            );
            assert!(
                known || crate::render::is_cancel(&verb),
                "`{verb}` is advertised but no surface routes it"
            );
        }
    }
}
