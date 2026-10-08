//! Pending gates: permissions and forms, unified into one lifecycle (spec P4),
//! plus the pure form-renderability logic (D-R5).

use serde::{Deserialize, Serialize};

/// A permission decision (wire values pinned in Phase 0: `once`/`always`/`reject`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateDecision {
    /// Allow this once.
    Once,
    /// Remember and always allow.
    Always,
    /// Deny.
    Reject,
}

impl GateDecision {
    /// The lowercase storage spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            GateDecision::Once => "once",
            GateDecision::Always => "always",
            GateDecision::Reject => "reject",
        }
    }
}

/// One selectable form option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormOption {
    /// The value submitted on reply.
    pub value: String,
    /// The display label.
    pub label: String,
}

/// The concrete nature of a pending gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GateKind {
    /// A permission request naming a tool and its patterns.
    Permission {
        /// The tool / verb, e.g. `bash`.
        action: String,
        /// The resources/patterns requested.
        resources: Vec<String>,
        /// A server-provided human message, when present.
        message: Option<String>,
    },
    /// A form asking structured questions.
    Form {
        /// The form title/question.
        title: String,
        /// The fields that make up the form.
        fields: Vec<FormField>,
    },
}

/// A pending gate: permissions and forms, unified (spec P4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateRequest {
    /// The server-side gate id (`per_*` or `frm_*`).
    pub id: String,
    /// The owning session id, when known.
    pub session_id: Option<String>,
    /// The gate kind.
    pub kind: GateKind,
    /// The wire payload, preserved verbatim for the surface-only degrade
    /// (D-R5): the bot renders the raw payload instead of guessing at an
    /// unknown wire format.
    pub payload: serde_json::Value,
}

/// How a field is answered (the wire `type` plus its options). Externally
/// tagged (with `rename_all = "snake_case"`), *not* internally tagged: a
/// `Choice(Vec<...>)` newtype variant containing a sequence cannot be
/// represented by an internally-tagged enum (serde), and this type is stored
/// and round-tripped through the gate row (`payload_json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FormFieldKind {
    /// Free text (string without options, number, integer).
    Text,
    /// A single-choice selection.
    Choice(Vec<FormOption>),
    /// Yes/no.
    Boolean,
    /// One or more of the listed options.
    Multiselect(Vec<FormOption>),
    /// An externally hosted field — surface-only by construction (D-R5).
    External(String),
    /// An unrecognized field type — the form degrades to surface-only.
    Unknown,
}

impl FormFieldKind {
    /// Whether the field can be mapped onto a button or modal input.
    pub fn is_interactive(&self) -> bool {
        matches!(
            self,
            FormFieldKind::Text
                | FormFieldKind::Choice(_)
                | FormFieldKind::Boolean
                | FormFieldKind::Multiselect(_)
        )
    }
}

/// One field of a form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormField {
    /// The field key (the reply map's key).
    pub key: String,
    /// The display title, when set.
    pub label: Option<String>,
    /// The answer kind.
    pub kind: FormFieldKind,
}

/// The most fields a renderable form may carry (Discord modals cap at 5, P10).
pub const MAX_FORM_FIELDS: usize = 5;

/// Whether a form can be answered entirely through buttons and a modal (D-R5):
/// at most [`MAX_FORM_FIELDS`] fields, none external or unknown.
pub fn form_renderable(fields: &[FormField]) -> bool {
    fields.len() <= MAX_FORM_FIELDS && fields.iter().all(|field| field.kind.is_interactive())
}

/// How a renderable form answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormPlaybook {
    /// The form is a single choice/boolean field: one button per option answers
    /// the whole form in a single tap.
    SingleChoice(Vec<FormOption>),
    /// The form is answered through one modal that collects every field.
    Modal,
}

/// The interaction plan for a renderable form; `None` when surface-only.
pub fn form_playbook(fields: &[FormField]) -> Option<FormPlaybook> {
    if !form_renderable(fields) {
        return None;
    }
    match fields {
        [single] if !single.key.is_empty() => match &single.kind {
            FormFieldKind::Choice(options) if !options.is_empty() => {
                Some(FormPlaybook::SingleChoice(options.clone()))
            }
            FormFieldKind::Boolean => Some(FormPlaybook::SingleChoice(vec![
                FormOption {
                    value: "true".to_owned(),
                    label: "Yes".to_owned(),
                },
                FormOption {
                    value: "false".to_owned(),
                    label: "No".to_owned(),
                },
            ])),
            _ => Some(FormPlaybook::Modal),
        },
        _ => Some(FormPlaybook::Modal),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FormField, FormFieldKind, FormOption, MAX_FORM_FIELDS, form_playbook, form_renderable,
    };

    fn choice(key: &str, options: &[&str]) -> FormField {
        FormField {
            key: key.to_owned(),
            label: None,
            kind: FormFieldKind::Choice(
                options
                    .iter()
                    .map(|option| FormOption {
                        value: (*option).to_owned(),
                        label: (*option).to_owned(),
                    })
                    .collect(),
            ),
        }
    }

    #[test]
    fn single_choice_form_is_a_button_playbook() {
        let fields = vec![choice("approach", &["a", "b"])];
        assert!(form_renderable(&fields));
        let playbook = form_playbook(&fields);
        assert!(
            matches!(&playbook, Some(super::FormPlaybook::SingleChoice(_))),
            "expected single choice"
        );
        let options = match playbook {
            Some(super::FormPlaybook::SingleChoice(options)) => options,
            _ => Vec::new(),
        };
        assert_eq!(options.len(), 2);
    }

    #[test]
    fn single_boolean_form_is_yes_no_buttons() {
        let fields = vec![FormField {
            key: "notify".to_owned(),
            label: None,
            kind: FormFieldKind::Boolean,
        }];
        let playbook = form_playbook(&fields);
        assert!(
            matches!(&playbook, Some(super::FormPlaybook::SingleChoice(_))),
            "expected boolean buttons"
        );
        let options = match playbook {
            Some(super::FormPlaybook::SingleChoice(options)) => options,
            _ => Vec::new(),
        };
        assert_eq!(options[0].value, "true");
        assert_eq!(options[1].value, "false");
    }

    #[test]
    fn text_field_form_is_a_modal() {
        let fields = vec![FormField {
            key: "note".to_owned(),
            label: None,
            kind: FormFieldKind::Text,
        }];
        assert_eq!(form_playbook(&fields), Some(super::FormPlaybook::Modal));
    }

    #[test]
    fn unknown_or_external_fields_make_the_form_surface_only() {
        let kinds = [
            FormFieldKind::Unknown,
            FormFieldKind::External("https://x".to_owned()),
        ];
        for kind in kinds {
            let fields = vec![FormField {
                key: "f".to_owned(),
                label: None,
                kind,
            }];
            assert!(!form_renderable(&fields));
            assert_eq!(form_playbook(&fields), None);
        }
    }

    #[test]
    fn too_many_fields_make_the_form_surface_only() {
        let fields: Vec<FormField> = (0..=MAX_FORM_FIELDS)
            .map(|index| FormField {
                key: format!("k{index}"),
                label: None,
                kind: FormFieldKind::Text,
            })
            .collect();
        assert!(!form_renderable(&fields));
    }
}
