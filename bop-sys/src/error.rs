use std::string::String;

use bop::BopError;

/// Thin wrapper around [`BopError::runtime`] with the argument order used
/// throughout bop-sys (line first, message second).
pub(crate) fn runtime(line: u32, message: impl Into<String>) -> BopError {
    BopError::runtime(message, line)
}
