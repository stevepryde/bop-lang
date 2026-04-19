use bop::{BopError, Value};

use crate::error::runtime;

pub(crate) fn expect_args(
    name: &str,
    args: &[Value],
    expected: usize,
    line: u32,
) -> Result<(), BopError> {
    if args.len() == expected {
        return Ok(());
    }

    Err(runtime(
        line,
        format!(
            "`{}` expects {} argument{}, but got {}",
            name,
            expected,
            if expected == 1 { "" } else { "s" },
            args.len()
        ),
    ))
}

pub(crate) fn expect_string<'a>(
    name: &str,
    value: &'a Value,
    line: u32,
) -> Result<&'a str, BopError> {
    match value {
        Value::Str(s) => Ok(s.as_str()),
        other => Err(runtime(
            line,
            format!("`{}` expects a string, but got {}", name, other.type_name()),
        )),
    }
}
