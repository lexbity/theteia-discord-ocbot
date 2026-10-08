//! Outcome classification (spec P3): a pure function over snapshot fields with
//! no text inspection, no prose-question detection, and a gate-substatus hook.

/// Token counts as reported by the last assistant message.
#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct TokenCounts {
    /// Input tokens.
    pub input: f64,
    /// Output tokens.
    pub output: f64,
    /// Reasoning tokens.
    pub reasoning: f64,
}

/// Everything `classify` may look at — the report input (spec A7).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssistantSnapshot {
    /// The concatenated assistant text (empty for tool-only endings).
    pub text: String,
    /// Whether the step issued tool calls.
    pub has_tool_calls: bool,
    /// A structured error message, when the step failed.
    pub error: Option<String>,
    /// The provider finish reason, when reported.
    pub finish: Option<String>,
    /// Cost in USD, when reported.
    pub cost: Option<f64>,
    /// Token usage, when reported.
    pub tokens: Option<TokenCounts>,
}

impl AssistantSnapshot {
    /// Construct an empty snapshot (e.g. when no assistant message exists).
    pub fn empty() -> Self {
        Self::default()
    }
}

/// The terminal classification of a finished session.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Idle, no error, text present.
    Completed {
        /// The report body.
        report: String,
    },
    /// Idle with a structured error / provider failure.
    Errored {
        /// The surfaced error text.
        error: String,
    },
    /// The bot recorded an abort intent.
    Aborted,
    /// Idle with no assistant text (tool-only ending).
    Empty,
}

/// `blocked` is a substatus, never an outcome (spec P3/P4): a session waiting
/// on a permission or form is still `running`.
#[derive(Debug, Clone, PartialEq)]
pub enum Classification {
    /// The session finished.
    Outcome(Outcome),
    /// The session is blocked waiting for a human; no report yet.
    Blocked,
}

/// Classify a finished session. Pure and deterministic:
///
/// 1. blocked ⇒ [`Classification::Blocked`] (a pending gate is a server-side
///    fact — the bot must not fabricate a completion);
/// 2. abort requested ⇒ `Aborted`;
/// 3. structured error present ⇒ `Errored`;
/// 4. assistant text present ⇒ `Completed`;
/// 5. otherwise `Empty` (including "no assistant message" and tool-only
///    endings).
///
/// The bot MUST NOT search prose for questions — the report *is* the question
/// (spec P3 note, unchanged by v2 forms).
pub fn classify(
    snapshot: Option<&AssistantSnapshot>,
    abort_requested: bool,
    blocked: bool,
) -> Classification {
    if blocked {
        return Classification::Blocked;
    }
    if abort_requested {
        return Classification::Outcome(Outcome::Aborted);
    }
    if let Some(error) = snapshot.and_then(|snapshot| snapshot.error.as_deref()) {
        return Classification::Outcome(Outcome::Errored {
            error: error.to_owned(),
        });
    }
    let text = snapshot.map_or("", |snapshot| snapshot.text.as_str());
    if !text.is_empty() {
        return Classification::Outcome(Outcome::Completed {
            report: text.to_owned(),
        });
    }
    Classification::Outcome(Outcome::Empty)
}

#[cfg(test)]
mod tests {
    use super::{AssistantSnapshot, Classification, Outcome, TokenCounts, classify};

    fn snapshot(text: &str, error: Option<&str>, tool_calls: bool) -> AssistantSnapshot {
        AssistantSnapshot {
            text: text.to_owned(),
            has_tool_calls: tool_calls,
            error: error.map(str::to_owned),
            finish: None,
            cost: Some(0.1),
            tokens: Some(TokenCounts {
                input: 10.0,
                output: 20.0,
                reasoning: 0.0,
            }),
        }
    }

    #[test]
    fn completed_when_text_present() {
        let classified = classify(Some(&snapshot("PONG", None, false)), false, false);
        assert_eq!(
            classified,
            Classification::Outcome(Outcome::Completed {
                report: "PONG".to_owned()
            })
        );
    }

    #[test]
    fn errored_when_error_present_even_with_text() {
        let classified = classify(
            Some(&snapshot("partial", Some("rate limit exceeded"), true)),
            false,
            false,
        );
        assert_eq!(
            classified,
            Classification::Outcome(Outcome::Errored {
                error: "rate limit exceeded".to_owned()
            })
        );
    }

    #[test]
    fn aborted_wins_over_text() {
        let classified = classify(Some(&snapshot("partial", None, true)), true, false);
        assert_eq!(classified, Classification::Outcome(Outcome::Aborted));
    }

    #[test]
    fn tool_only_ending_is_empty() {
        let classified = classify(Some(&snapshot("", None, true)), false, false);
        assert_eq!(classified, Classification::Outcome(Outcome::Empty));
    }

    #[test]
    fn missing_snapshot_is_empty_not_completed() {
        let classified = classify(None, false, false);
        assert_eq!(classified, Classification::Outcome(Outcome::Empty));
    }

    #[test]
    fn blocked_is_not_an_outcome() {
        let classified = classify(Some(&snapshot("done", None, true)), false, true);
        assert_eq!(classified, Classification::Blocked);
        // Even an erroring snapshot must not be reported while blocked.
        let classified = classify(Some(&snapshot("x", Some("err"), false)), false, true);
        assert_eq!(classified, Classification::Blocked);
    }
}
