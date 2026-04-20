use std::string::String;

use bop::BopError;

/// Thin wrapper around [`BopError::runtime`] with the argument order used
/// throughout bop-sys (line first, message second).
pub(crate) fn runtime(line: u32, message: impl Into<String>) -> BopError {
    BopError::runtime(message, line)
}

/// Error helper for I/O and resolver failures where no particular
/// source line applies (e.g. a module-resolution error carries no
/// Bop call site — it originates in the host).
pub(crate) fn io_error(message: &str, line: Option<u32>) -> BopError {
    BopError {
        line,
        column: None,
        message: message.to_string(),
        friendly_hint: None,
        is_fatal: false,
    }
}
