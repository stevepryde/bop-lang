//! Error type for the Bop interpreter.

#[cfg(not(feature = "std"))]
use alloc::string::String;

#[derive(Debug, Clone)]
pub struct BopError {
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub message: String,
    pub friendly_hint: Option<String>,
    /// Fatal errors can't be caught by `try_call`; they always
    /// unwind to the engine boundary. This is the load-bearing
    /// property that makes `BopLimits` a real sandbox — a
    /// script can't wrap an infinite loop in `try_call` and
    /// loop forever by swallowing the step-limit error.
    ///
    /// Non-fatal errors (the default) describe ordinary runtime
    /// problems — type mismatches, missing fields, index out of
    /// bounds, "function not found". Those can be caught.
    ///
    /// Currently set only on resource-limit errors
    /// (`Your code took too many steps`, `Memory limit
    /// exceeded`). Any new fatal case must explicitly construct
    /// `BopError::fatal` rather than `BopError::runtime`.
    pub is_fatal: bool,
}

impl BopError {
    /// Create a runtime error at the given source line.
    pub fn runtime(message: impl Into<String>, line: u32) -> Self {
        Self {
            line: Some(line),
            column: None,
            message: message.into(),
            friendly_hint: None,
            is_fatal: false,
        }
    }

    /// Create a **fatal** runtime error at the given source line.
    /// Used for resource-limit violations (`too many steps`,
    /// `Memory limit exceeded`) — see [`BopError::is_fatal`]
    /// for why those must never be swallowed by `try_call`.
    pub fn fatal(message: impl Into<String>, line: u32) -> Self {
        Self {
            line: Some(line),
            column: None,
            message: message.into(),
            friendly_hint: None,
            is_fatal: true,
        }
    }
}

impl core::fmt::Display for BopError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if let Some(line) = self.line {
            write!(f, "[line {}] {}", line, self.message)
        } else {
            write!(f, "{}", self.message)
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for BopError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_error_sets_message_and_line() {
        let err = BopError::runtime("boom", 7);

        assert_eq!(err.message, "boom");
        assert_eq!(err.line, Some(7));
        assert_eq!(err.column, None);
        assert_eq!(err.friendly_hint, None);
        assert!(!err.is_fatal);
    }

    #[test]
    fn fatal_error_marks_is_fatal_true() {
        let err = BopError::fatal("step limit", 0);
        assert!(err.is_fatal);
        assert!(!BopError::runtime("nope", 0).is_fatal);
    }
}
