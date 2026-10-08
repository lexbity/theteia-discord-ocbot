//! The startup doctor (FR-40): the shipped permission baseline
//! (`config/opencode.permissions.jsonc`) is parsed and validated against the
//! v2 `Permission.Rule{action,resource,effect}` shape, naming any missing
//! `ask` pattern. The version probe lives in `run.rs` (part (b) of FR-40).

use std::path::Path;

use ocbot_core::ports::PermissionEffect;
use serde::Deserialize;

/// One rule of the v2 permission ruleset (pinned by the vendored spec:
/// `Permission.Rule` = `{action, resource, effect}`, no extra fields).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaselineRule {
    /// The action/tool, e.g. `bash`, `read`, `write`.
    pub action: String,
    /// The resource pattern, e.g. `git push*`.
    pub resource: String,
    /// The effect.
    pub effect: PermissionEffect,
}

/// The parsed, validated baseline.
#[derive(Debug, Clone, PartialEq)]
pub struct Baseline {
    /// The ordered ruleset (order matters: first match wins, FR-21).
    pub rules: Vec<BaselineRule>,
}

impl Baseline {
    /// Load and validate the baseline file, collecting every problem at once
    /// (the doctor pattern: all validation errors reported together).
    pub fn load(path: &Path) -> Result<Self, Vec<String>> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| vec![format!("cannot read {}: {error}", path.display())])?;
        Self::parse(&text)
    }

    /// Parse JSONC (comments stripped) into the baseline and validate it.
    pub fn parse(text: &str) -> Result<Self, Vec<String>> {
        let mut errors = Vec::new();
        let cleaned = strip_jsonc(text);
        let value: serde_json::Value = match serde_json::from_str(&cleaned) {
            Ok(value) => value,
            Err(error) => {
                return Err(vec![format!(
                    "permission baseline is not valid JSON: {error}"
                )]);
            }
        };
        let array = match &value {
            serde_json::Value::Array(array) => array,
            serde_json::Value::Object(map) => match map.get("permissions") {
                Some(serde_json::Value::Array(array)) => array,
                _ => {
                    return Err(vec![format!(
                        "permission baseline must be a rules array or {{ \"permissions\": [...] }}, got {value}"
                    )]);
                }
            },
            other => {
                return Err(vec![format!(
                    "permission baseline must be a rules array, got {other}"
                )]);
            }
        };
        let mut rules = Vec::with_capacity(array.len());
        for (index, entry) in array.iter().enumerate() {
            match serde_json::from_value::<BaselineRule>(entry.clone()) {
                Ok(rule) => rules.push(rule),
                Err(error) => errors.push(format!(
                    "baseline rule {index} violates Permission.Rule: {error}"
                )),
            }
        }
        let baseline = Self { rules };
        errors.extend(baseline.validate_fr21());
        if errors.is_empty() {
            Ok(baseline)
        } else {
            Err(errors)
        }
    }

    /// FR-21 floor: every dangerous family must be an `ask` rule.
    fn validate_fr21(&self) -> Vec<String> {
        const REQUIRED: [&str; 4] = ["git push", "sudo", "rm -rf", ".ssh"];
        let mut errors = Vec::new();
        for rule in &self.rules {
            if rule.action.trim().is_empty() {
                errors.push(
                    "baseline rule has an empty action (Permission.Rule requires action)"
                        .to_owned(),
                );
            }
            if rule.resource.trim().is_empty() {
                errors.push(
                    "baseline rule has an empty resource (Permission.Rule requires resource)"
                        .to_owned(),
                );
            }
        }
        for needle in REQUIRED {
            let covered = self
                .rules
                .iter()
                .any(|rule| rule.effect == PermissionEffect::Ask && rule.resource.contains(needle));
            if !covered {
                errors.push(format!(
                    "permission baseline is missing an `ask` rule for {needle:?} (FR-21)"
                ));
            }
        }
        errors
    }
}

/// Strip `//` line comments and `/* */` block comments from JSONC text,
/// preserving string contents (the shipped baseline is annotated).
pub fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_string = true;
                out.push(ch);
            }
            '/' if chars.peek() == Some(&'/') => {
                let _ = chars.next();
                for ch in chars.by_ref() {
                    if ch == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                let _ = chars.next();
                let mut last = '\0';
                for ch in chars.by_ref() {
                    if last == '*' && ch == '/' {
                        break;
                    }
                    last = ch;
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Baseline, strip_jsonc};
    use ocbot_core::ports::PermissionEffect;

    const GOOD: &str = r#"
// ocbot's shipped permission baseline (FR-21): the dangerous families must be `ask`.
{
  "permissions": [
    { "action": "bash", "resource": "git push*", "effect": "ask" },
    { "action": "bash", "resource": "sudo*", "effect": "ask" },
    { "action": "bash", "resource": "rm -rf*", "effect": "ask" },
    { "action": "read", "resource": "~/.ssh/**", "effect": "ask" },
    { "action": "write", "resource": "~/.ssh/**", "effect": "ask" }
  ]
}
"#;

    #[test]
    fn jsonc_comments_are_stripped_without_touching_strings() {
        let cleaned = strip_jsonc(
            r#"// top
{ "permissions": [ { "resource": "git push*" /* inline */ } ] } // tail"#,
        );
        assert!(!cleaned.contains("// top"));
        assert!(cleaned.contains("git push*"));
        assert!(!cleaned.contains("/* inline */"));
    }

    #[test]
    fn good_baseline_parses_and_validates() {
        let baseline = Baseline::parse(GOOD).expect("baseline ok");
        assert_eq!(baseline.rules.len(), 5);
        assert!(
            baseline
                .rules
                .iter()
                .all(|rule| rule.effect == PermissionEffect::Ask)
        );
    }

    #[test]
    fn missing_pattern_is_named() {
        let text = GOOD.replace("sudo*", "ls*");
        let errors = Baseline::parse(&text).expect_err("missing sudo");
        assert!(
            errors.iter().any(|error| error.contains("sudo")),
            "errors: {errors:?}"
        );
    }

    #[test]
    fn non_ask_pattern_is_named() {
        let text = GOOD.replace("\"ask\"", "\"allow\"");
        let errors = Baseline::parse(&text).expect_err("not ask");
        assert!(
            errors.iter().any(|error| error.contains("ask")),
            "errors: {errors:?}"
        );
    }

    #[test]
    fn unknown_rule_field_is_rejected() {
        let text = r#"{ "permissions": [
            { "action": "bash", "resource": "git push*", "effect": "ask", "bad": true }
        ] }"#;
        let errors = Baseline::parse(text).expect_err("extra field");
        assert!(errors.iter().any(|error| error.contains("Permission.Rule")));
    }

    #[test]
    fn bare_array_is_accepted() {
        let text = r#"[
            { "action": "bash", "resource": "git push*", "effect": "ask" },
            { "action": "bash", "resource": "sudo*", "effect": "ask" },
            { "action": "bash", "resource": "rm -rf*", "effect": "ask" },
            { "action": "read", "resource": "~/.ssh/**", "effect": "ask" },
            { "action": "write", "resource": "~/.ssh/**", "effect": "ask" }
        ]"#;
        assert!(Baseline::parse(text).is_ok());
    }
}
