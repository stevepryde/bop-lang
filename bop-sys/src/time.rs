use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bop::{BopError, Value};

use crate::args::expect_args;
use crate::error::runtime;

pub(crate) fn unix_time(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("unix_time", args, 0, line)?;

    Ok(Value::Number(unix_duration(line)?.as_secs_f64()))
}

pub(crate) fn unix_time_ms(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("unix_time_ms", args, 0, line)?;

    Ok(Value::Number(unix_duration(line)?.as_millis() as f64))
}

fn unix_duration(line: u32) -> Result<Duration, BopError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| runtime(line, format!("system clock is before Unix epoch: {}", e)))
}
