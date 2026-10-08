//! Permission and form gate messages (FR-20/FR-41, D-R5 surface-only).
//!
//! A permission request gets one-tap decision buttons; a renderable form gets
//! an Answer button (plus per-option quick buttons for a single-choice form);
//! anything else renders the *raw payload* verbatim with no buttons — the bot
//! never guesses at an unknown wire format.

use std::fmt::Write as _;

use crate::gates::{FormFieldKind, FormPlaybook, GateKind, GateRequest, form_playbook};

use super::label::inline_safe;
use super::report::fence_neutralize;
use super::{ButtonKind, OutgoingMessage, component_button, component_button_state};

/// The gate message (FR-20/FR-41): a permission request gets one-tap decision
/// buttons; a renderable form gets an Answer button (plus per-option quick
/// buttons for a single-choice form); anything else renders the *raw payload*
/// verbatim with no buttons (D-R5 surface-only — the bot never guesses at an
/// unknown wire format).
pub fn gate_message(
    label: Option<&str>,
    gate: &GateRequest,
    rowid: i64,
    operator_id: u64,
) -> OutgoingMessage {
    let caller = label.map_or_else(|| "?".to_owned(), inline_safe);
    match &gate.kind {
        GateKind::Permission {
            action,
            resources,
            message,
        } => {
            let mut content = format!("🔒 **{caller}** — permission needed\n");
            let _ = writeln!(content, "tool: `{}`", inline_safe(action));
            for resource in resources {
                let _ = writeln!(content, "  `{}`", inline_safe(resource));
            }
            if let Some(message) = message {
                let _ = write!(content, "_{}_", inline_safe(message));
            }
            OutgoingMessage {
                content,
                attachment: None,
                components: vec![
                    component_button("Once", format!("v1:gate:{rowid}:once"), ButtonKind::Success),
                    component_button(
                        "Always",
                        format!("v1:gate:{rowid}:always"),
                        ButtonKind::Primary,
                    ),
                    component_button(
                        "Reject",
                        format!("v1:gate:{rowid}:reject"),
                        ButtonKind::Danger,
                    ),
                ],
                mentioned_users: vec![operator_id],
            }
        }
        GateKind::Form { title, fields } => {
            if let Some(playbook) = form_playbook(fields) {
                let mut content = format!("📋 **{caller}** — {}\n", inline_safe(title));
                for field in fields {
                    let label = field.label.as_deref().map(inline_safe).unwrap_or_default();
                    let label: String = label.chars().take(80).collect();
                    let _ = writeln!(
                        content,
                        "• `{}` · {} {}",
                        inline_safe(&field.key),
                        describe_kind(&field.kind),
                        label
                    );
                }
                let components = match playbook {
                    FormPlaybook::SingleChoice(options) => options
                        .iter()
                        .map(|option| {
                            component_button_state(
                                inline_safe(&option.label),
                                format!("v1:formpick:{rowid}:{}", option.value),
                                ButtonKind::Secondary,
                                false,
                            )
                        })
                        .collect(),
                    FormPlaybook::Modal => vec![component_button(
                        "✍ Answer form",
                        format!("v1:formgo:{rowid}"),
                        ButtonKind::Primary,
                    )],
                };
                OutgoingMessage {
                    content,
                    attachment: None,
                    components,
                    mentioned_users: vec![operator_id],
                }
            } else {
                let text = serde_json::to_string_pretty(&gate.payload)
                    .unwrap_or_else(|_| gate.payload.to_string());
                OutgoingMessage {
                    content: format!(
                        "📋 **{caller}** — form (answer in the TUI)\n```json\n{}\n```",
                        fence_neutralize(&text)
                    ),
                    attachment: None,
                    components: Vec::new(),
                    mentioned_users: vec![operator_id],
                }
            }
        }
    }
}

/// One-line description of a form field's answer kind (message body text).
fn describe_kind(kind: &FormFieldKind) -> String {
    match kind {
        FormFieldKind::Text => "text".to_owned(),
        FormFieldKind::Choice(options) => {
            let options: Vec<String> = options
                .iter()
                .map(|option| inline_safe(&option.label))
                .collect();
            format!("choice: {}", options.join(", "))
        }
        FormFieldKind::Boolean => "yes/no".to_owned(),
        FormFieldKind::Multiselect(_) => "multi-choice".to_owned(),
        FormFieldKind::External(_) => "external".to_owned(),
        FormFieldKind::Unknown => "unknown".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::gate_message;
    use crate::gates::{
        FormField, FormFieldKind, FormOption, GateKind, GateRequest, MAX_FORM_FIELDS,
    };

    fn field(key: &str, kind: FormFieldKind) -> FormField {
        FormField {
            key: key.to_owned(),
            label: None,
            kind,
        }
    }

    fn gate_form(fields: Vec<FormField>) -> GateRequest {
        GateRequest {
            id: "frm_x".to_owned(),
            session_id: Some("ses_x".to_owned()),
            kind: GateKind::Form {
                title: "Which one?".to_owned(),
                fields,
            },
            payload: serde_json::json!({ "id": "frm_x", "title": "Which one?" }),
        }
    }

    #[test]
    fn gate_message_renders_permission_decision_buttons() {
        let gate = GateRequest {
            id: "per_x".to_owned(),
            session_id: Some("ses_x".to_owned()),
            kind: GateKind::Permission {
                action: "bash".to_owned(),
                resources: vec!["rm -rf /tmp/x".to_owned()],
                message: None,
            },
            payload: serde_json::json!({}),
        };
        let message = gate_message(Some("poke"), &gate, 7, 42);
        assert!(message.content.contains("bash"));
        assert_eq!(
            message
                .components
                .iter()
                .map(|component| component.custom_id.as_str())
                .collect::<Vec<_>>(),
            vec!["v1:gate:7:once", "v1:gate:7:always", "v1:gate:7:reject"]
        );
        assert_eq!(message.mentioned_users, vec![42]);
    }

    #[test]
    fn gate_message_renders_renderable_form_modal_button() {
        let gate = gate_form(vec![
            field(
                "approach",
                FormFieldKind::Choice(vec![
                    FormOption {
                        value: "a".to_owned(),
                        label: "A".to_owned(),
                    },
                    FormOption {
                        value: "b".to_owned(),
                        label: "B".to_owned(),
                    },
                ]),
            ),
            field("note", FormFieldKind::Text),
        ]);
        let message = gate_message(Some("poke"), &gate, 8, 42);
        assert!(
            message
                .components
                .iter()
                .any(|component| component.custom_id == "v1:formgo:8"),
            "renderable form carries the answer button"
        );
    }

    #[test]
    fn gate_message_renders_single_choice_form_buttons() {
        let gate = gate_form(vec![field(
            "approach",
            FormFieldKind::Choice(vec![FormOption {
                value: "a".to_owned(),
                label: "A".to_owned(),
            }]),
        )]);
        let message = gate_message(None, &gate, 9, 42);
        assert!(
            message
                .components
                .iter()
                .any(|component| component.custom_id == "v1:formpick:9:a"),
            "single-choice form answers by button"
        );
    }

    #[test]
    fn gate_message_renders_unrenderable_form_verbatim_without_buttons() {
        let fields: Vec<FormField> = (0..=MAX_FORM_FIELDS)
            .map(|index| field(&format!("f{index}"), FormFieldKind::Text))
            .collect();
        let gate = gate_form(fields);
        let message = gate_message(Some("poke"), &gate, 10, 42);
        assert!(message.components.is_empty(), "no buttons for surface-only");
        assert!(message.content.contains("answer in the TUI"));
        assert!(message.content.contains("frm_x"));
    }

    #[test]
    fn gate_message_renders_unknown_field_form_surface_only() {
        let gate = gate_form(vec![field("mystery", FormFieldKind::Unknown)]);
        let message = gate_message(None, &gate, 11, 42);
        assert!(message.components.is_empty());
        assert!(message.content.contains("answer in the TUI"));
    }
}
