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
