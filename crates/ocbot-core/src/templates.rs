//! Strict slot template engine (spec P13): known slots only, no expression
//! evaluation, load-time validation. The rendered prompt is stored verbatim in
//! `action.prompt_sent` at send time; Park & redo restages the stored prompt,
//! never a re-render.

use std::collections::HashMap;

/// Every slot the bot may substitute.
pub const KNOWN_SLOTS: &[&str] = &["label", "plan_path", "git_log", "git_status"];

/// Template errors. Load-time errors must be fatal for the bot's config
/// (a template with an unknown slot is a deployment error, not a runtime one).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TemplateError {
    /// The template uses slots the engine does not know; every offender is
    /// listed together so the fix is a single edit, not an iterate-loop.
    #[error("unknown template slot(s): {0:?}")]
    UnknownSlot(Vec<String>),
    /// The template references a known slot that had no value at render time.
    #[error("missing value for slot(s): {0:?}")]
    MissingSlot(Vec<String>),
}

/// Scan a template and return the slot names in first-appearance order.
pub fn slots_in(template: &str) -> Result<Vec<String>, TemplateError> {
    let mut slots: Vec<String> = Vec::new();
    let bytes = template.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'{' {
            let close = template[index + 1..].find('}');
            let Some(relative) = close else {
                return Err(TemplateError::UnknownSlot(vec![
                    template[index..].to_owned(),
                ]));
            };
            let name = &template[index + 1..index + 1 + relative];
            if !valid_slot_name(name) {
                return Err(TemplateError::UnknownSlot(vec![format!("{{{name}}}")]));
            }
            slots.push(name.to_owned());
            index += relative + 2;
        } else {
            index += 1;
        }
    }
    Ok(slots)
}

/// Whether a slot name is a plain lowercase identifier `[a-z_][a-z0-9_]*`.
fn valid_slot_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Validate a template for load: every slot must be known. Returns all unknown
/// slots at once.
pub fn validate(template: &str) -> Result<(), TemplateError> {
    let slots = slots_in(template)?;
    let unknown: Vec<String> = slots
        .iter()
        .filter(|slot| !KNOWN_SLOTS.contains(&slot.as_str()))
        .cloned()
        .collect();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(TemplateError::UnknownSlot(unknown))
    }
}

/// Render a template, substituting every slot. Known slots missing a value are
/// a render error (all listed at once); unknown slots are a load error.
pub fn render<S>(
    template: &str,
    values: &HashMap<&'static str, String, S>,
) -> Result<String, TemplateError>
where
    S: std::hash::BuildHasher,
{
    let slots = slots_in(template)?;
    let missing: Vec<String> = slots
        .iter()
        .filter(|slot| KNOWN_SLOTS.contains(&slot.as_str()) && !values.contains_key(slot.as_str()))
        .cloned()
        .collect();
    if !missing.is_empty() {
        return Err(TemplateError::MissingSlot(missing));
    }
    let unknown: Vec<String> = slots
        .iter()
        .filter(|slot| !KNOWN_SLOTS.contains(&slot.as_str()))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        return Err(TemplateError::UnknownSlot(unknown));
    }

    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            break; // unreachable after slots_in validated the input
        };
        let name = &after[..close];
        out.push_str(&values[name]);
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{KNOWN_SLOTS, TemplateError, render, slots_in, validate};
    use std::collections::HashMap;

    fn values() -> HashMap<&'static str, String> {
        let mut values = HashMap::new();
        values.insert("label", "implement x".to_owned());
        values.insert("git_log", "* abc123 initial".to_owned());
        values.insert("git_status", "(clean)".to_owned());
        values.insert("plan_path", String::new());
        values
    }

    #[test]
    fn known_slots_render() {
        let template = "Label: {label}\nGit: {git_log}\nStatus: {git_status}\nPlan: {plan_path}";
        let rendered = render(template, &values()).expect("render");
        assert_eq!(
            rendered,
            "Label: implement x\nGit: * abc123 initial\nStatus: (clean)\nPlan: "
        );
    }

    #[test]
    fn unknown_slot_is_a_load_error_listing_everything() {
        let template = "a {bad_slot} b {also_bad} {label}";
        assert_eq!(
            validate(template),
            Err(TemplateError::UnknownSlot(vec![
                "bad_slot".to_owned(),
                "also_bad".to_owned()
            ]))
        );
    }

    #[test]
    fn missing_value_is_a_render_error() {
        let template = "{label} {plan_path}";
        let mut only_label = values();
        only_label.remove("plan_path");
        assert_eq!(
            render(template, &only_label),
            Err(TemplateError::MissingSlot(vec!["plan_path".to_owned()]))
        );
    }

    #[test]
    fn malformed_brace_is_reported() {
        let template = "{broken";
        assert!(matches!(
            validate(template),
            Err(TemplateError::UnknownSlot(_))
        ));
    }

    #[test]
    fn empty_and_slot_free_templates_pass_validation() {
        assert!(validate("").is_ok());
        assert!(validate("no slots here").is_ok());
        assert!(slots_in("no slots here").expect("scan").is_empty());
    }

    #[test]
    fn every_known_slot_is_usable() {
        for slot in KNOWN_SLOTS {
            let template = format!("{{{slot}}}");
            assert!(validate(&template).is_ok(), "slot {slot}");
        }
    }
}
