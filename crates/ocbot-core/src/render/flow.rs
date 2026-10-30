//! The pending-input flow vocabulary (SPEC-004 D-06/§5.6, PH-7).
//!
//! A surface without modals turns one into a short text conversation. The
//! *words* that conversation uses — every prompt, every lifecycle notice, and
//! the step table that says what a supplied text fills and what comes next —
//! live here, pure and static, so the adapter holds no prompt text and the
//! tables are unit-testable. The adapter (`adapters/signal/flows.rs`) owns only
//! the effects: persisting the draft, posting the prompt, and synthesizing the
//! `ModalSubmit` the Discord modal would have produced.
//!
//! `MODAL_FAMILIES` is the coverage claim: every modal `custom_id` the router
//! accepts must be convertible to a text form. `tests/signal_flows.rs` crosses
//! it against `router::modal_ids()` so a new modal that has no flow entry fails
//! CI instead of silently degrading to an unanswered form.

/// The flow name stored in `pending_input.flow` for the run flow.
pub const FLOW_RUN: &str = "run";
/// The flow name for a generic modal form (edit, accept, follow-up, form, plan).
pub const FLOW_MODAL: &str = "modal";
/// The flow name for the `/prompt` text form.
pub const FLOW_PROMPT: &str = "prompt";

/// Every flow name the gauge and the sweeps know about (D-06/FR-113).
pub const FLOW_NAMES: [&str; 3] = [FLOW_RUN, FLOW_MODAL, FLOW_PROMPT];

/// The pending-flow time-to-live in seconds (D-06/FR-109: ten minutes).
pub const FLOW_TTL_SECS: i64 = 600;

/// The word that closes an open flow (D-06/§5.5).
pub const CANCEL_WORD: &str = "cancel";

/// One step of a named flow: what to ask, which field the answer fills, and
/// what happens next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlowStep {
    /// The step name persisted in `pending_input.step`.
    pub step: &'static str,
    /// The prompt posted when the step opens.
    pub prompt: &'static str,
    /// The field a supplied text fills (adapter-side draft storage).
    pub field: &'static str,
    /// The next step, or `None` to submit the flow.
    pub next: Option<&'static str>,
}

/// The `run` step table (D-06): directory, then prompt.
pub const RUN_STEPS: [FlowStep; 2] = [
    FlowStep {
        step: "await_path",
        prompt: "Send the directory to run in.",
        field: "dir",
        next: Some("await_prompt"),
    },
    FlowStep {
        step: "await_prompt",
        prompt: "Send the prompt for this run.",
        field: "prompt",
        next: None,
    },
];

/// The `prompt` step table: one free-text prompt.
pub const PROMPT_STEPS: [FlowStep; 1] = [FlowStep {
    step: "await_text",
    prompt: "Send the prompt text.",
    field: "prompt",
    next: None,
}];

/// The step table for a named flow (empty for the generic `modal` flow, whose
/// steps come from the modal spec itself).
#[must_use]
pub fn flow_steps(flow: &str) -> &'static [FlowStep] {
    match flow {
        FLOW_RUN => &RUN_STEPS,
        FLOW_PROMPT => &PROMPT_STEPS,
        _ => &[],
    }
}

/// The step a named flow opens at (its first step, when it has one).
#[must_use]
pub fn first_step(flow: &str) -> Option<&'static str> {
    flow_steps(flow).first().map(|step| step.step)
}

/// The prompt for a named flow step.
#[must_use]
pub fn step_prompt(flow: &str, step: &str) -> Option<&'static str> {
    flow_steps(flow)
        .iter()
        .find(|candidate| candidate.step == step)
        .map(|found| found.prompt)
}

/// The step after `step`, or `None` when the flow submits.
#[must_use]
pub fn next_step(flow: &str, step: &str) -> Option<&'static str> {
    flow_steps(flow)
        .iter()
        .find(|candidate| candidate.step == step)
        .and_then(|found| found.next)
}

/// The field a named flow step's text fills.
#[must_use]
pub fn step_field(flow: &str, step: &str) -> Option<&'static str> {
    flow_steps(flow)
        .iter()
        .find(|candidate| candidate.step == step)
        .map(|found| found.field)
}

/// Whether a text is the flow-closing word (case-insensitive, D-06).
#[must_use]
pub fn is_cancel(text: &str) -> bool {
    text.trim().eq_ignore_ascii_case(CANCEL_WORD)
}

/// The prompt with the `cancel` hint appended (every prompt carries it).
#[must_use]
pub fn with_cancel_hint(prompt: &str) -> String {
    format!("{prompt} (or `{CANCEL_WORD}`)")
}

/// The header posted when a modal becomes a text form (D-04).
#[must_use]
pub fn form_header(title: &str) -> String {
    format!("{title} — text form")
}

/// The per-field prompt of a modal form: position, field name, and the default
/// an empty reply keeps.
#[must_use]
pub fn field_prompt(id: &str, label: &str, index: usize, total: usize) -> String {
    format!("{}/{} — {}: send the {id}", index + 1, total, label)
}

/// The default hint appended to a field prompt when the modal prefill is
/// non-empty (an empty reply keeps it).
#[must_use]
pub fn default_hint(default: &str) -> String {
    if default.is_empty() {
        String::new()
    } else {
        format!(" (empty keeps: {default})")
    }
}

/// The notice posted when the operator closes a flow (D-06).
#[must_use]
pub fn cancelled() -> &'static str {
    "flow cancelled"
}

/// The notice posted when a flow names a step the adapter cannot advance.
#[must_use]
pub fn unknown_flow() -> &'static str {
    "unknown open flow; it has been cancelled"
}

/// The expiry notice the ops sweep posts (FR-113): one line, naming the flow.
#[must_use]
pub fn expired(flow: &str) -> String {
    format!("the `{flow}` flow expired (10 minutes); send a command to start again")
}

/// The boot notice for a flow that survived a restart (D-06): the draft is
/// still on disk, so the operator is told what was in flight instead of typing
/// into a bot that ignores them. Full resume is deliberately not built — the
/// only persistent state is the text itself, and re-asking is one message.
#[must_use]
pub fn restart_notice(flow: &str, draft: &str) -> String {
    let echo = draft_echo(draft);
    let echo = if echo.is_empty() {
        String::new()
    } else {
        format!(" [{echo}]")
    };
    match flow {
        FLOW_RUN => {
            format!("a `run` draft was open{echo} — send `run` to restart or reply to continue")
        }
        FLOW_PROMPT => format!(
            "a `prompt` draft was open{echo} — send `prompt` to restart or reply to continue"
        ),
        FLOW_MODAL => format!(
            "a text form was open{echo} — reply to continue or send `{CANCEL_WORD}` to close it"
        ),
        other => format!(
            "a `{other}` flow was open{echo} — reply to continue or send `{CANCEL_WORD}` to close it"
        ),
    }
}

/// A compact echo of a persisted draft, for the restart notice.
///
/// Opaque by design: the core reads back the flat text fields a flow wrote and
/// never interprets their meaning (the adapter owns the shapes). The engine's
/// own bookkeeping keys are skipped, arrays are joined in order, and the whole
/// thing is truncated so one line on a phone stays one line. Hostile or
/// non-JSON input yields an empty echo rather than an error.
#[must_use]
pub fn draft_echo(draft: &str) -> String {
    /// Keys the flow engine owns; never shown to the operator.
    const BOOKKEEPING: [&str; 2] = ["fields", "ask"];
    /// The longest echo appended to a notice.
    const ECHO_CHARS: usize = 120;

    let Ok(value) = serde_json::from_str::<serde_json::Value>(draft) else {
        return String::new();
    };
    let Some(object) = value.as_object() else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    for (key, value) in object {
        if BOOKKEEPING.contains(&key.as_str()) {
            continue;
        }
        if let Some(text) = echo_value(value) {
            parts.push(format!("{key}: {text}"));
        }
    }
    truncate(&parts.join(" · "), ECHO_CHARS)
}

/// One draft value, as a short operator-readable string (`None` for the empty
/// and the non-textual shape, both of which say nothing).
fn echo_value(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(text) if !text.is_empty() => Some(text.clone()),
        serde_json::Value::Array(items) => {
            let rendered: Vec<String> = items.iter().filter_map(echo_item).collect();
            if rendered.is_empty() {
                None
            } else {
                Some(rendered.join(" → "))
            }
        }
        _ => None,
    }
}

/// One array item: a plain string, or a two-element pair rendered `key=value`
/// (the shape a collected answer takes).
fn echo_item(item: &serde_json::Value) -> Option<String> {
    match item {
        serde_json::Value::String(text) if !text.is_empty() => Some(text.clone()),
        serde_json::Value::Array(pair) => {
            let mut parts = pair.iter().filter_map(serde_json::Value::as_str);
            let key = parts.next()?;
            let value = parts.next()?;
            Some(format!("{key}={value}"))
        }
        _ => None,
    }
}

/// Truncate to at most `max` characters, ending on a non-whitespace character.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max).collect();
    while out.ends_with(char::is_whitespace) {
        out.pop();
    }
    out.push('…');
    out
}

/// Every modal `custom_id` family the router accepts, and therefore every
/// modal that must have a Signal text form (PH-7's drift guard).
///
/// This is the *coverage claim*, not the router: `router::modal_ids()` is the
/// router's own list, and `tests/signal_flows.rs` asserts every router id is
/// covered here. Adding a modal to the router without adding it here fails CI.
pub const MODAL_FAMILIES: [&str; 8] = [
    "v1:newrun",
    "v1:prompt",
    "v1:editgo:",
    "v1:acceptgo:",
    "v1:formgo:",
    "v1:followupgo:",
    "v1:planothergo:",
    "v1:run_noplan_go:",
];

/// Whether a `custom_id` belongs to one of the modal families (an exact id for
/// the three bare modals, a prefix for the action-suffixed ones).
#[must_use]
pub fn is_modal_id(custom_id: &str) -> bool {
    MODAL_FAMILIES.iter().any(|family| {
        if family.ends_with(':') {
            custom_id.starts_with(family)
        } else {
            custom_id == *family
        }
    })
}

/// One sample `custom_id` per modal family, for the coverage test.
#[must_use]
pub fn modal_family_samples() -> Vec<String> {
    MODAL_FAMILIES
        .iter()
        .map(|family| {
            if family.ends_with(':') {
                format!("{family}1")
            } else {
                (*family).to_owned()
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        CANCEL_WORD, FLOW_MODAL, FLOW_PROMPT, FLOW_RUN, MODAL_FAMILIES, cancelled, default_hint,
        draft_echo, expired, field_prompt, first_step, flow_steps, form_header, is_cancel,
        is_modal_id, modal_family_samples, next_step, restart_notice, step_field, step_prompt,
        unknown_flow, with_cancel_hint,
    };

    #[test]
    fn every_flow_name_has_a_step_table_or_is_generic() {
        assert_eq!(flow_steps(FLOW_RUN).len(), 2);
        assert_eq!(flow_steps(FLOW_PROMPT).len(), 1);
        assert!(flow_steps(FLOW_MODAL).is_empty());
    }

    #[test]
    fn the_run_flow_walks_path_then_prompt_then_submits() {
        assert_eq!(first_step(FLOW_RUN), Some("await_path"));
        assert_eq!(next_step(FLOW_RUN, "await_path"), Some("await_prompt"));
        assert_eq!(next_step(FLOW_RUN, "await_prompt"), None);
        assert_eq!(step_field(FLOW_RUN, "await_path"), Some("dir"));
        assert_eq!(step_field(FLOW_RUN, "await_prompt"), Some("prompt"));
        assert_eq!(
            step_prompt(FLOW_RUN, "await_prompt"),
            Some("Send the prompt for this run.")
        );
    }

    #[test]
    fn single_step_flows_submit_immediately() {
        assert_eq!(first_step(FLOW_PROMPT), Some("await_text"));
        assert_eq!(next_step(FLOW_PROMPT, "await_text"), None);
    }

    #[test]
    fn unknown_flows_and_steps_are_absent_not_panics() {
        assert!(flow_steps("nope").is_empty());
        assert_eq!(step_prompt("nope", "await_path"), None);
        assert_eq!(next_step(FLOW_RUN, "await_text"), None);
        assert_eq!(step_field(FLOW_RUN, "await_text"), None);
    }

    #[test]
    fn cancel_is_case_and_space_insensitive() {
        assert!(is_cancel("cancel"));
        assert!(is_cancel("  CANCEL "));
        assert!(!is_cancel("cancelled the run"));
        assert!(!is_cancel(""));
    }

    #[test]
    fn prompts_carry_the_cancel_hint() {
        assert_eq!(
            with_cancel_hint("Send the directory to run in."),
            "Send the directory to run in. (or `cancel`)"
        );
    }

    #[test]
    fn the_form_header_and_field_prompt_name_the_step() {
        assert_eq!(
            form_header("Accept — commit message"),
            "Accept — commit message — text form"
        );
        assert_eq!(
            field_prompt("subject", "Commit subject", 0, 2),
            "1/2 — Commit subject: send the subject"
        );
    }

    #[test]
    fn a_non_empty_default_is_hinted_an_empty_one_is_not() {
        assert_eq!(default_hint("keep me"), " (empty keeps: keep me)");
        assert!(default_hint("").is_empty());
    }

    #[test]
    fn lifecycle_notices_name_their_flow() {
        assert_eq!(cancelled(), "flow cancelled");
        assert!(unknown_flow().contains("unknown open flow"));
        assert!(expired(FLOW_RUN).contains("`run` flow expired"));
        assert!(expired(FLOW_MODAL).contains("`modal` flow expired"));
    }

    #[test]
    fn the_restart_notice_names_the_restart_command() {
        assert!(restart_notice(FLOW_RUN, "{}").contains("`run` draft was open"));
        assert!(restart_notice(FLOW_MODAL, "{}").contains("text form"));
        assert!(restart_notice("mystery", "{}").contains("mystery"));
    }

    #[test]
    fn the_restart_notice_echoes_what_the_draft_held() {
        let notice = restart_notice(FLOW_RUN, r#"{"dir":"/tmp/x"}"#);
        assert!(notice.contains("dir: /tmp/x"), "{notice}");
        assert!(notice.contains("send `run` to restart"), "{notice}");
    }

    #[test]
    fn a_modal_form_echo_skips_the_engines_bookkeeping() {
        // `fields` and `ask` are engine shape, never operator content; the
        // collected answers are what the operator wants echoed.
        let draft = serde_json::json!({
            "custom_id": "v1:acceptgo:7",
            "fields": [{"id": "subject", "label": "Subject", "default": null, "required": true}],
            "ask": [0],
            "collected": [["subject", "ship it"]]
        })
        .to_string();
        let notice = restart_notice(FLOW_MODAL, &draft);
        assert!(notice.contains("ship it"), "{notice}");
        assert!(notice.contains("custom_id: v1:acceptgo:7"), "{notice}");
        assert!(!notice.contains("fields"), "{notice}");
    }

    #[test]
    fn a_draft_echo_survives_hostile_input_and_is_bounded() {
        assert!(draft_echo("not json").is_empty());
        assert!(draft_echo("[]").is_empty());
        assert!(draft_echo("{}").is_empty());
        let long = format!(r#"{{"dir":"{}"}}"#, "x".repeat(500));
        let echo = draft_echo(&long);
        assert!(
            echo.chars().count() <= 121,
            "{} chars",
            echo.chars().count()
        );
        assert!(echo.ends_with('…'), "{echo}");
    }

    #[test]
    fn every_modal_family_matches_its_own_sample() {
        for sample in modal_family_samples() {
            assert!(is_modal_id(&sample), "{sample} must be a modal id");
        }
        // A component id is not a modal id.
        assert!(!is_modal_id("v1:accept:7"));
        assert!(!is_modal_id("v1:page:history:2"));
    }

    #[test]
    fn prefixed_families_do_not_match_bare_ids() {
        assert!(!is_modal_id("v1:newrunx"), "an unknown id must not pass");
        assert!(is_modal_id("v1:editgo:7"));
        assert_eq!(MODAL_FAMILIES.len(), 8);
    }

    #[test]
    fn the_cancel_word_is_only_the_cancel_word() {
        assert_eq!(CANCEL_WORD, "cancel");
    }
}
