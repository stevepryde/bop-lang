//! Error type for the Bop interpreter.

#[cfg(not(feature = "std"))]
use alloc::string::String;

#[derive(Debug, Clone)]
pub struct BopError {
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub message: String,
    pub friendly_hint: Option<String>,
}

impl BopError {
    /// Create a runtime error at the given source line.
    pub fn runtime(message: impl Into<String>, line: u32) -> Self {
        Self {
            line: Some(line),
            column: None,
            message: message.into(),
            friendly_hint: None,
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
    }
}
