use std::string::String;

use bop::BopError;

pub(crate) fn runtime(line: u32, message: impl Into<String>) -> BopError {
    BopError {
        line: Some(line),
        column: None,
        message: message.into(),
        friendly_hint: None,
    }
}
