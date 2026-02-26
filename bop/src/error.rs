//! Error type for the Bop interpreter.

#[derive(Debug, Clone)]
pub struct BopError {
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub message: String,
    pub friendly_hint: Option<String>,
}

impl std::fmt::Display for BopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(line) = self.line {
            write!(f, "[line {}] {}", line, self.message)
        } else {
            write!(f, "{}", self.message)
        }
    }
}

impl std::error::Error for BopError {}
