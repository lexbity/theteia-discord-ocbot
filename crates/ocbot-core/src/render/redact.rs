//! Secret redaction for the inline report path (R-12).
//!
//! The full report body (`report_full`, the `report.md` attachment) is
//! deliberately left unredacted: it is stored verbatim for the operator's own
//! audit trail, and R-12 accepts that residual exposure in exchange for never
//! corrupting the record of what the agent actually said.

/// What every redaction pattern is replaced with (R-12).
pub const REDACTED: &str = "[redacted]";

/// A redaction pattern that failed to compile at load time. A bad pattern is a
/// deployment fault, not a runtime surprise: refusing it stops a typo from
/// silently disabling the scrubber.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("redaction pattern {pattern:?}: {message}")]
pub struct RedactError {
    /// The offending pattern.
    pub pattern: String,
    /// The regex compiler's message.
    pub message: String,
}

/// Secret scrubber for the **inline** report path (R-12). Patterns are
/// compiled once at startup; [`Redactor::default`] is the no-op used by tests
/// and by callers that have no secrets to protect.
///
/// The full report body (`report_full`, the `report.md` attachment) is
/// deliberately left unredacted (see the module docs).
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    patterns: Vec<regex::Regex>,
}

impl Redactor {
    /// Compile every pattern; the first failure aborts with its source.
    pub fn new(patterns: &[String]) -> Result<Self, RedactError> {
        let mut compiled = Vec::with_capacity(patterns.len());
        for pattern in patterns {
            match regex::Regex::new(pattern) {
                Ok(regex) => compiled.push(regex),
                Err(error) => {
                    return Err(RedactError {
                        pattern: pattern.clone(),
                        message: error.to_string(),
                    });
                }
            }
        }
        Ok(Self { patterns: compiled })
    }

    /// Whether any pattern is armed.
    pub fn is_active(&self) -> bool {
        !self.patterns.is_empty()
    }

    /// Replace every match of every pattern with [`REDACTED`].
    pub fn scrub(&self, text: &str) -> String {
        let mut out = text.to_owned();
        for pattern in &self.patterns {
            if pattern.is_match(&out) {
                out = pattern.replace_all(&out, REDACTED).into_owned();
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::{REDACTED, Redactor};

    #[test]
    fn invalid_redaction_pattern_is_a_load_error() {
        let error = Redactor::new(&["(".to_owned()]).expect_err("bad regex rejected");
        assert_eq!(error.pattern, "(");
        assert!(!Redactor::default().is_active());
    }

    #[test]
    fn scrub_replaces_every_match_and_deactivates_cleanly() {
        let redactor = Redactor::new(&[r"secret-\d+".to_owned()]).expect("pattern compiles");
        assert!(redactor.is_active());
        assert_eq!(
            redactor.scrub("a secret-1 b secret-2"),
            format!("a {REDACTED} b {REDACTED}")
        );
        assert_eq!(Redactor::default().scrub("unchanged"), "unchanged");
    }
}
