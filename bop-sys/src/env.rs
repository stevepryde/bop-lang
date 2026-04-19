use bop::{BopError, Value};

use crate::args::{expect_args, expect_string};
use crate::error::runtime;

pub(crate) fn env(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("env", args, 1, line)?;
    let name = expect_string("env", &args[0], line)?;

    match std::env::var(name) {
        Ok(value) => Ok(Value::new_str(value)),
        Err(std::env::VarError::NotPresent) => Ok(Value::None),
        Err(std::env::VarError::NotUnicode(_)) => Err(runtime(
            line,
            format!("env variable `{}` is not valid Unicode", name),
        )),
    }
}
