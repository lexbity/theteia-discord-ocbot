//! The pure inbound text grammar (SPEC-004 §5.5, FR-105/FR-111).
//!
//! Surfaces without components (Signal) turn typed text into the same router
//! entry points Discord buttons and slash commands reach. This module is a
//! **pure** parser: it performs no I/O, owns no state, and never touches the
//! store — the adapter resolves an [`Intent`] against the DB afterwards. That
//! keeps the grammar property-testable (`tests/grammar_prop.rs`) and keeps the
//! surface adapter thin.
//!
//! Lexical rules (§5.5): commands are case-insensitive; arguments split on
//! whitespace; a free-text argument (a prompt, a label) is everything after the
//! verb and its structured arguments, so multi-word and emoji-bearing text is
//! preserved verbatim.

use std::fmt;

/// A decision verb: the four Discord decision buttons, plus the send/edit pair
/// that has no emoji mapping but must have a text equivalent (FR-105).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionVerb {
    /// Accept the reported action.
    Accept,
    /// Park the reported action.
    Park,
    /// Follow up on the reported action.
    FollowUp,
    /// Discard the reported action.
    Discard,
}

impl DecisionVerb {
    /// The canonical spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            DecisionVerb::Accept => "accept",
            DecisionVerb::Park => "park",
            DecisionVerb::FollowUp => "follow",
            DecisionVerb::Discard => "discard",
        }
    }

    /// The `<custom_id>` prefix the button equivalent carries (router grammar).
    pub fn custom_id_prefix(self) -> &'static str {
        match self {
            DecisionVerb::Accept => "v1:accept:",
            DecisionVerb::Park => "v1:park:",
            DecisionVerb::FollowUp => "v1:followup:",
            DecisionVerb::Discard => "v1:discard:",
        }
    }
}

/// A normalized inbound command (SPEC-004 §5.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// `run [path] [| prompt]`: open the pending run flow (or start directly
    /// when a prompt follows a `|`).
    Run {
        /// The directory to run in, when given on the command line.
        path: Option<String>,
        /// The prompt, when supplied inline after a `|`.
        prompt: Option<String>,
    },
    /// `prompt [text]`: the free-form prompt flow.
    Prompt {
        /// The prompt text, when supplied inline.
        text: Option<String>,
    },
    /// `status`.
    Status,
    /// `abort`.
    Abort,
    /// `release`.
    Release,
    /// `mute`.
    Mute,
    /// `history [page]`.
    History {
        /// The page to render, when given.
        page: Option<usize>,
    },
    /// `reset`: show the history with reset targets.
    Reset,
    /// `resetgo <sha>`: confirm a reset.
    ResetGo {
        /// The target sha.
        sha: String,
    },
    /// `send [label]`: send the (latest) staged action.
    Send {
        /// The action label, when given.
        label: Option<String>,
    },
    /// `edit [label]`: open the prompt editor for a staged action.
    Edit {
        /// The action label, when given.
        label: Option<String>,
    },
    /// `pick <n>`: choose plan step `n` on a staged plan action.
    Pick {
        /// The zero-based? no — the one-based plan index typed by the operator.
        index: usize,
    },
    /// `accept|park|follow|discard [label]`: a decision.
    Decision {
        /// The verb.
        verb: DecisionVerb,
        /// The action label, when given (else the latest reported in the chat).
        label: Option<String>,
    },
    /// `gate <label> <option>`: answer a permission gate.
    Gate {
        /// The action label.
        label: String,
        /// The decision option (`once`/`always`/`reject`, or a form value).
        option: String,
    },
    /// `projects`: list available projects (SPEC-005).
    Projects,
    /// `help`: the legend and the command table.
    Help,
}

impl Intent {
    /// The verb of this intent (the first token that would parse back to it).
    pub fn verb(&self) -> &'static str {
        match self {
            Intent::Run { .. } => "run",
            Intent::Prompt { .. } => "prompt",
            Intent::Status => "status",
            Intent::Abort => "abort",
            Intent::Release => "release",
            Intent::Mute => "mute",
            Intent::History { .. } => "history",
            Intent::Reset => "reset",
            Intent::ResetGo { .. } => "resetgo",
            Intent::Send { .. } => "send",
            Intent::Edit { .. } => "edit",
            Intent::Pick { .. } => "pick",
            Intent::Decision { verb, .. } => verb.as_str(),
            Intent::Gate { .. } => "gate",
            Intent::Projects => "projects",
            Intent::Help => "help",
        }
    }
}

impl fmt::Display for Intent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.verb())
    }
}

/// Why a line did not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrammarError {
    /// The input held no verb.
    Empty,
    /// The first word is not a known verb.
    UnknownVerb(String),
    /// A required argument is missing.
    MissingArgument {
        /// The verb that needed the argument.
        verb: &'static str,
        /// The argument name.
        argument: &'static str,
    },
}

impl fmt::Display for GrammarError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GrammarError::Empty => formatter.write_str("no command given"),
            GrammarError::UnknownVerb(verb) => {
                write!(formatter, "unknown command {verb:?} — send `help`")
            }
            GrammarError::MissingArgument { verb, argument } => {
                write!(formatter, "`{verb}` needs {argument}")
            }
        }
    }
}

impl std::error::Error for GrammarError {}

/// Parse one inbound line into an [`Intent`].
///
/// The parser is total: every input either yields an intent or a
/// [`GrammarError`]; it never panics, whatever bytes the operator's phone sends.
pub fn parse(input: &str) -> Result<Intent, GrammarError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(GrammarError::Empty);
    }
    // The first whitespace-delimited token is the verb; the remainder is the
    // verb's argument tail, whitespace-trimmed but otherwise preserved.
    let (verb, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((verb, rest)) => (verb, rest.trim()),
        None => (trimmed, ""),
    };
    let verb_lower = verb.to_ascii_lowercase();
    match verb_lower.as_str() {
        "run" => parse_run(rest),
        "prompt" => Ok(Intent::Prompt {
            text: non_empty(rest).map(str::to_owned),
        }),
        "status" => Ok(Intent::Status),
        "abort" => Ok(Intent::Abort),
        "release" => Ok(Intent::Release),
        "mute" => Ok(Intent::Mute),
        "history" => Ok(Intent::History {
            page: parse_optional_page(rest)?,
        }),
        "reset" => Ok(Intent::Reset),
        "resetgo" => {
            let sha = non_empty(rest).ok_or(GrammarError::MissingArgument {
                verb: "resetgo",
                argument: "the target commit",
            })?;
            Ok(Intent::ResetGo {
                sha: sha.to_owned(),
            })
        }
        "send" => Ok(Intent::Send {
            label: non_empty(rest).map(str::to_owned),
        }),
        "edit" => Ok(Intent::Edit {
            label: non_empty(rest).map(str::to_owned),
        }),
        "pick" => {
            let raw = non_empty(rest).ok_or(GrammarError::MissingArgument {
                verb: "pick",
                argument: "a step number",
            })?;
            let index = raw
                .parse::<usize>()
                .map_err(|_| GrammarError::MissingArgument {
                    verb: "pick",
                    argument: "a numeric step",
                })?;
            Ok(Intent::Pick { index })
        }
        "accept" | "park" | "follow" | "followup" | "discard" => {
            let verb = match verb_lower.as_str() {
                "accept" => DecisionVerb::Accept,
                "park" => DecisionVerb::Park,
                "discard" => DecisionVerb::Discard,
                // `follow` and `followup` are the same verb (D-05's label is
                // "follow up"; §5.5 spells it `follow`).
                _ => DecisionVerb::FollowUp,
            };
            Ok(Intent::Decision {
                verb,
                label: non_empty(rest).map(str::to_owned),
            })
        }
        "gate" => parse_gate(rest),
        "projects" => Ok(Intent::Projects),
        "help" => Ok(Intent::Help),
        other => Err(GrammarError::UnknownVerb(other.to_owned())),
    }
}

/// `run [path]` or `run path | prompt`.
fn parse_run(rest: &str) -> Result<Intent, GrammarError> {
    if let Some((path, prompt)) = rest.split_once('|') {
        Ok(Intent::Run {
            path: non_empty(path.trim()).map(str::to_owned),
            prompt: non_empty(prompt.trim()).map(str::to_owned),
        })
    } else {
        Ok(Intent::Run {
            path: non_empty(rest).map(str::to_owned),
            prompt: None,
        })
    }
}

/// `gate <label> <option>`: the option is the last token, the label the rest
/// (so a label containing spaces still resolves).
fn parse_gate(rest: &str) -> Result<Intent, GrammarError> {
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    let Some((option, label)) = tokens.split_last() else {
        return Err(GrammarError::MissingArgument {
            verb: "gate",
            argument: "a label and a decision",
        });
    };
    if label.is_empty() {
        return Err(GrammarError::MissingArgument {
            verb: "gate",
            argument: "a label",
        });
    }
    Ok(Intent::Gate {
        label: label.join(" "),
        option: (*option).to_owned(),
    })
}

/// `history [page]`: a single optional page number.
fn parse_optional_page(rest: &str) -> Result<Option<usize>, GrammarError> {
    let trimmed = rest.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed
        .split_whitespace()
        .next()
        .and_then(|token| token.parse::<usize>().ok())
        .map(Some)
        .ok_or(GrammarError::MissingArgument {
            verb: "history",
            argument: "a numeric page",
        })
}

/// `Some` iff `text` has non-whitespace content.
fn non_empty(text: &str) -> Option<&str> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::{DecisionVerb, GrammarError, Intent, parse};

    #[test]
    fn bare_verbs_parse() {
        assert_eq!(parse("status").unwrap(), Intent::Status);
        assert_eq!(parse("ABORT").unwrap(), Intent::Abort);
        assert_eq!(parse("release").unwrap(), Intent::Release);
        assert_eq!(parse("mute").unwrap(), Intent::Mute);
        assert_eq!(parse("help").unwrap(), Intent::Help);
        assert_eq!(parse("reset").unwrap(), Intent::Reset);
    }

    #[test]
    fn decisions_carry_an_optional_label() {
        assert_eq!(
            parse("accept").unwrap(),
            Intent::Decision {
                verb: DecisionVerb::Accept,
                label: None
            }
        );
        assert_eq!(
            parse("accept my feature label").unwrap(),
            Intent::Decision {
                verb: DecisionVerb::Accept,
                label: Some("my feature label".to_owned())
            }
        );
        assert_eq!(
            parse("followup").unwrap(),
            Intent::Decision {
                verb: DecisionVerb::FollowUp,
                label: None
            }
        );
        assert_eq!(
            parse("follow").unwrap(),
            Intent::Decision {
                verb: DecisionVerb::FollowUp,
                label: None
            }
        );
        assert_eq!(
            parse("park").unwrap(),
            Intent::Decision {
                verb: DecisionVerb::Park,
                label: None
            }
        );
        assert_eq!(
            parse("discard").unwrap(),
            Intent::Decision {
                verb: DecisionVerb::Discard,
                label: None
            }
        );
    }

    #[test]
    fn decision_verbs_keep_their_text_and_component_spellings() {
        let cases = [
            (DecisionVerb::Accept, "accept", "v1:accept:"),
            (DecisionVerb::Park, "park", "v1:park:"),
            (DecisionVerb::FollowUp, "follow", "v1:followup:"),
            (DecisionVerb::Discard, "discard", "v1:discard:"),
        ];
        for (verb, spelling, component_prefix) in cases {
            assert_eq!(verb.as_str(), spelling);
            assert_eq!(verb.custom_id_prefix(), component_prefix);
            let intent = Intent::Decision { verb, label: None };
            assert_eq!(intent.verb(), spelling);
            assert_eq!(intent.to_string(), spelling);
        }
    }

    #[test]
    fn intent_and_grammar_errors_display_their_canonical_text() {
        assert_eq!(Intent::Projects.to_string(), "projects");
        assert_eq!(Intent::History { page: Some(2) }.to_string(), "history");
        assert_eq!(GrammarError::Empty.to_string(), "no command given");
        assert_eq!(
            GrammarError::UnknownVerb("mystery".to_owned()).to_string(),
            "unknown command \"mystery\" — send `help`"
        );
        assert_eq!(
            GrammarError::MissingArgument {
                verb: "pick",
                argument: "a numeric step",
            }
            .to_string(),
            "`pick` needs a numeric step"
        );
    }

    #[test]
    fn run_splits_path_and_prompt() {
        assert_eq!(
            parse("run /home/me/proj").unwrap(),
            Intent::Run {
                path: Some("/home/me/proj".to_owned()),
                prompt: None
            }
        );
        assert_eq!(
            parse("run /home/me/proj | fix the thing").unwrap(),
            Intent::Run {
                path: Some("/home/me/proj".to_owned()),
                prompt: Some("fix the thing".to_owned())
            }
        );
        assert_eq!(
            parse("run").unwrap(),
            Intent::Run {
                path: None,
                prompt: None
            }
        );
    }

    #[test]
    fn gate_splits_label_and_option() {
        assert_eq!(
            parse("gate my label once").unwrap(),
            Intent::Gate {
                label: "my label".to_owned(),
                option: "once".to_owned()
            }
        );
        assert_eq!(
            parse("gate once").unwrap_err(),
            GrammarError::MissingArgument {
                verb: "gate",
                argument: "a label"
            }
        );
        assert_eq!(
            parse("gate").unwrap_err(),
            GrammarError::MissingArgument {
                verb: "gate",
                argument: "a label and a decision"
            }
        );
    }

    #[test]
    fn projects_parses_with_no_arguments() {
        assert_eq!(parse("projects").unwrap(), Intent::Projects);
        assert_eq!(parse("PROJECTS").unwrap(), Intent::Projects);
        assert_eq!(parse("  projects  ").unwrap(), Intent::Projects);
    }

    #[test]
    fn history_page_parsing_preserves_zero_and_rejects_invalid_numbers() {
        assert_eq!(parse("history").unwrap(), Intent::History { page: None });
        assert_eq!(
            parse("history 0").unwrap(),
            Intent::History { page: Some(0) }
        );
        assert_eq!(
            parse("history 7").unwrap(),
            Intent::History { page: Some(7) }
        );
        assert_eq!(
            parse("history nope").unwrap_err(),
            GrammarError::MissingArgument {
                verb: "history",
                argument: "a numeric page"
            }
        );
    }

    #[test]
    fn unknown_and_empty_are_errors() {
        assert_eq!(parse("").unwrap_err(), GrammarError::Empty);
        assert_eq!(parse("   ").unwrap_err(), GrammarError::Empty);
        assert_eq!(
            parse("frobnicate").unwrap_err(),
            GrammarError::UnknownVerb("frobnicate".to_owned())
        );
    }

    #[test]
    fn prompt_text_is_preserved_verbatim() {
        let text = "fix the parser — the 🐛 is in `grammar.rs`";
        assert_eq!(
            parse(&format!("prompt {text}")).unwrap(),
            Intent::Prompt {
                text: Some(text.to_owned())
            }
        );
    }
}
