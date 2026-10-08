//! Pending gates — permission requests and forms, unified into one lifecycle.
//!
//! v2 lists both kinds server-side as facts (spec P4, A5, A12); the bot's gate
//! state machine is Phase 4, but the *client* must already know the wire
//! shapes so the Phase 4 lifecycle never guesses.

use serde::{Deserialize, Serialize};

use crate::Client;
use crate::error::OcError;
use crate::paths;

/// A permission decision, matching the wire enum `Permission.Reply`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GateDecision {
    /// Allow this once.
    Once,
    /// Remember and always allow.
    Always,
    /// Deny.
    Reject,
}

/// A pending permission request.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct PermissionRequest {
    /// The request id (wire prefix `per`).
    pub id: String,
    /// The session requesting permission.
    #[serde(rename = "sessionID", default)]
    pub session_id: Option<String>,
    /// The verb/action, e.g. `bash`, `git.push`, `read`.
    #[serde(default)]
    pub action: String,
    /// The pattern/path resources the request applies to.
    #[serde(default)]
    pub resources: Vec<String>,
    /// Save/remember tokens offered alongside the request.
    #[serde(default)]
    pub save: Vec<String>,
    /// A server-provided human message, when present.
    #[serde(default)]
    pub message: Option<String>,
    /// Additional unstructured metadata.
    #[serde(default)]
    pub metadata: Option<serde_json::Value>,
}

/// A pending form.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct FormInfo {
    /// The form id (wire prefix `frm`).
    pub id: String,
    /// The session that owns the form.
    #[serde(rename = "sessionID", default)]
    pub session_id: String,
    /// The form's question/title.
    #[serde(default)]
    pub title: String,
    /// The fields that make up the form.
    #[serde(default)]
    pub fields: Vec<FormField>,
}

/// A renderable or surface-only form field.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FormField {
    /// Free-text input.
    String(FormStringField),
    /// Numeric input.
    Number(FormNumberField),
    /// Integer input.
    Integer(FormNumberField),
    /// Boolean (checkbox / yes-no).
    Boolean(FormBooleanField),
    /// Choice or multi-choice selection.
    Multiselect(FormMultiselectField),
    /// An externally hosted field (surface-only by construction, D-R5).
    External(FormExternalField),
    /// Unknown field type; the form degrades to surface-only.
    #[serde(other)]
    Other,
}

/// Shared string field shape.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FormStringField {
    /// The field key.
    #[serde(default)]
    pub key: String,
    /// Display title.
    #[serde(default)]
    pub title: Option<String>,
    /// Optional choice values for constrained strings.
    #[serde(default)]
    pub options: Vec<FormOption>,
}

/// Numeric-valued field shape (number and integer share it).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FormNumberField {
    /// The field key.
    #[serde(default)]
    pub key: String,
    /// Display title.
    #[serde(default)]
    pub title: Option<String>,
}

/// Boolean field shape.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FormBooleanField {
    /// The field key.
    #[serde(default)]
    pub key: String,
    /// Display title.
    #[serde(default)]
    pub title: Option<String>,
}

/// Multi-select field shape.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FormMultiselectField {
    /// The field key.
    #[serde(default)]
    pub key: String,
    /// Display title.
    #[serde(default)]
    pub title: Option<String>,
    /// The selectable options.
    #[serde(default)]
    pub options: Vec<FormOption>,
}

/// Externally hosted field; surface-only by definition (D-R5).
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FormExternalField {
    /// The field key.
    #[serde(default)]
    pub key: String,
    /// The external resource URL.
    #[serde(default)]
    pub url: String,
}

/// One selectable option.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct FormOption {
    /// The option value submitted on reply.
    #[serde(default)]
    pub value: String,
    /// The display label.
    #[serde(default)]
    pub label: String,
}

/// A pending gate, permissions and forms unified (spec P4).
#[derive(Debug, Clone, PartialEq)]
pub struct GateRequest {
    /// The server-side gate id (`per_*` or `frm_*`).
    pub id: String,
    /// The owning session id.
    pub session_id: Option<String>,
    /// The concrete kind.
    pub kind: GateKind,
}

/// The concrete kind of a pending gate.
#[derive(Debug, Clone, PartialEq)]
pub enum GateKind {
    /// A permission request.
    Permission(PermissionRequest),
    /// A form.
    Form(FormInfo),
}

impl GateRequest {
    /// The owning session id, as a borrowed string when known.
    pub fn session(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}

impl From<PermissionRequest> for GateRequest {
    fn from(permission: PermissionRequest) -> Self {
        let id = permission.id.clone();
        let session_id = permission.session_id.clone();
        Self {
            id,
            session_id,
            kind: GateKind::Permission(permission),
        }
    }
}

impl From<FormInfo> for GateRequest {
    fn from(form: FormInfo) -> Self {
        let id = form.id.clone();
        let session_id = (!form.session_id.is_empty()).then_some(form.session_id.clone());
        Self {
            id,
            session_id,
            kind: GateKind::Form(form),
        }
    }
}

/// Location-scoped lists (the shape of the permission and form list responses).
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub(crate) struct GateList<T> {
    #[serde(default)]
    pub(crate) data: Vec<T>,
}

impl Client {
    /// All pending gates for the default location: permissions plus forms
    /// (spec P4 unifies them; the bot's lifecycle sees one list).
    pub async fn pending_gates(&self) -> Result<Vec<GateRequest>, OcError> {
        let permissions: GateList<PermissionRequest> =
            self.get(paths::PERMISSION_REQUEST, &[]).await?;
        let forms: GateList<FormInfo> = self.get(paths::FORM, &[]).await?;
        let mut gates: Vec<GateRequest> = permissions
            .data
            .into_iter()
            .map(GateRequest::from)
            .collect();
        gates.extend(forms.data.into_iter().map(GateRequest::from));
        Ok(gates)
    }

    /// Reply to a pending permission request (wire decision values confirmed
    /// in Phase 0: `once`, `always`, `reject`).
    pub async fn reply_permission(
        &self,
        session_id: &str,
        request_id: &str,
        decision: GateDecision,
    ) -> Result<(), OcError> {
        let path = paths::session_permission_reply(session_id, request_id);
        let body = serde_json::json!({ "decision": decision });
        self.post_empty(&path, &body).await
    }

    /// Reply to a pending form with `{ "key": value }` answers.
    pub async fn reply_form(
        &self,
        session_id: &str,
        form_id: &str,
        answer: serde_json::Value,
    ) -> Result<(), OcError> {
        let path = paths::session_form_reply(session_id, form_id);
        let body = serde_json::json!({ "answer": answer });
        self.post_empty(&path, &body).await
    }
}

#[cfg(test)]
mod tests {
    use super::{FormField, FormInfo, GateDecision, PermissionRequest};

    #[test]
    fn permission_request_decodes_tolerantly() {
        let json = r#"{
            "id": "per_x",
            "sessionID": "ses_y",
            "action": "bash",
            "resources": ["echo hi"],
            "save": ["once"],
            "message": "Allow bash?",
            "metadata": {"x": 1}
        }"#;
        let request: PermissionRequest = serde_json::from_str(json).expect("decode");
        assert_eq!(request.action, "bash");
        assert_eq!(request.session_id.as_deref(), Some("ses_y"));
    }

    #[test]
    fn form_decodes_with_typed_and_unknown_fields() {
        let json = r#"{
            "id": "frm_x",
            "sessionID": "ses_y",
            "title": "Continue?",
            "fields": [
                {"key": "choice", "type": "string", "options": [{"value": "a", "label": "A"}]},
                {"key": "mystery", "type": "dimensional-array", "depth": 5}
            ]
        }"#;
        let form: FormInfo = serde_json::from_str(json).expect("decode");
        assert_eq!(form.fields.len(), 2);
        assert!(matches!(form.fields[0], FormField::String(_)));
        assert!(matches!(form.fields[1], FormField::Other));
    }

    #[test]
    fn decision_wire_values_are_snake_case() {
        assert_eq!(
            serde_json::to_string(&GateDecision::Always).expect("encode"),
            "\"always\""
        );
        assert_eq!(
            serde_json::from_str::<GateDecision>("\"reject\"").expect("decode"),
            GateDecision::Reject
        );
    }
}
