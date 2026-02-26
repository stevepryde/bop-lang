use crate::error::BopError;
use crate::memory::bop_would_exceed;
use crate::value::Value;

pub fn builtin_range(
    args: &[Value],
    line: u32,
    rand_state: &mut u64,
) -> Result<Value, BopError> {
    let _ = rand_state; // unused here, keeping signature uniform
    let (start, end, step) = match args.len() {
        1 => {
            let n = expect_number("range", &args[0], line)?;
            (0.0, n, 1.0)
        }
        2 => {
            let start = expect_number("range", &args[0], line)?;
            let end = expect_number("range", &args[1], line)?;
            (start, end, if start <= end { 1.0 } else { -1.0 })
        }
        3 => {
            let start = expect_number("range", &args[0], line)?;
            let end = expect_number("range", &args[1], line)?;
            let step = expect_number("range", &args[2], line)?;
            if step == 0.0 {
                return Err(error(line, "range step can't be 0"));
            }
            (start, end, step)
        }
        _ => return Err(error(line, "range takes 1, 2, or 3 arguments")),
    };

    let mut result = Vec::new();
    let mut i = start;
    let max_items = 10_000usize;
    if step > 0.0 {
        while i < end && result.len() < max_items {
            result.push(Value::Number(i));
            i += step;
        }
    } else {
        while i > end && result.len() < max_items {
            result.push(Value::Number(i));
            i += step;
        }
    }
    Ok(Value::new_array(result))
}

pub fn builtin_str(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("str", args, 1, line)?;
    Ok(Value::new_str(format!("{}", args[0])))
}

pub fn builtin_int(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("int", args, 1, line)?;
    match &args[0] {
        Value::Number(n) => Ok(Value::Number(n.trunc())),
        Value::Str(s) => {
            let n: f64 = s.parse().map_err(|_| {
                error(line, format!("Can't convert \"{}\" to a number", s))
            })?;
            Ok(Value::Number(n.trunc()))
        }
        Value::Bool(b) => Ok(Value::Number(if *b { 1.0 } else { 0.0 })),
        _ => Err(error(
            line,
            format!("Can't convert {} to int", args[0].type_name()),
        )),
    }
}

pub fn builtin_type(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("type", args, 1, line)?;
    Ok(Value::new_str(args[0].type_name().to_string()))
}

pub fn builtin_abs(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("abs", args, 1, line)?;
    let n = expect_number("abs", &args[0], line)?;
    Ok(Value::Number(n.abs()))
}

pub fn builtin_min(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("min", args, 2, line)?;
    let a = expect_number("min", &args[0], line)?;
    let b = expect_number("min", &args[1], line)?;
    Ok(Value::Number(a.min(b)))
}

pub fn builtin_max(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("max", args, 2, line)?;
    let a = expect_number("max", &args[0], line)?;
    let b = expect_number("max", &args[1], line)?;
    Ok(Value::Number(a.max(b)))
}

pub fn builtin_rand(args: &[Value], line: u32, rand_state: &mut u64) -> Result<Value, BopError> {
    expect_args("rand", args, 1, line)?;
    let n = expect_number("rand", &args[0], line)? as i64;
    if n <= 0 {
        return Err(error(line, "rand needs a positive number"));
    }
    // Simple PCG-style PRNG for deterministic behaviour
    *rand_state = rand_state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let value = (*rand_state >> 33) % (n as u64);
    Ok(Value::Number(value as f64))
}

pub fn builtin_len(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("len", args, 1, line)?;
    match &args[0] {
        Value::Str(s) => Ok(Value::Number(s.chars().count() as f64)),
        Value::Array(a) => Ok(Value::Number(a.len() as f64)),
        Value::Dict(d) => Ok(Value::Number(d.len() as f64)),
        _ => Err(error(line, format!("Can't get length of {}", args[0].type_name()))),
    }
}

pub fn builtin_inspect(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("inspect", args, 1, line)?;
    Ok(Value::new_str(args[0].inspect()))
}

// ─── Helpers (also used by evaluator) ──────────────────────────────────────

pub fn expect_args(
    name: &str,
    args: &[Value],
    expected: usize,
    line: u32,
) -> Result<(), BopError> {
    if args.len() != expected {
        Err(error(
            line,
            format!(
                "`{}` expects {} argument{}, but got {}",
                name,
                expected,
                if expected == 1 { "" } else { "s" },
                args.len()
            ),
        ))
    } else {
        Ok(())
    }
}

pub fn expect_number(
    func_name: &str,
    val: &Value,
    line: u32,
) -> Result<f64, BopError> {
    match val {
        Value::Number(n) => Ok(*n),
        _ => Err(error(
            line,
            format!(
                "`{}` expects a number, but got {}",
                func_name,
                val.type_name()
            ),
        )),
    }
}

pub fn error(line: u32, message: impl Into<String>) -> BopError {
    BopError {
        line: Some(line),
        column: None,
        message: message.into(),
        friendly_hint: None,
    }
}

pub fn error_with_hint(
    line: u32,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> BopError {
    BopError {
        line: Some(line),
        column: None,
        message: message.into(),
        friendly_hint: Some(hint.into()),
    }
}

/// Pre-flight check for string repeat
pub fn check_string_repeat_memory(len: usize, count: usize, line: u32) -> Result<(), BopError> {
    let result_len = len.saturating_mul(count);
    if bop_would_exceed(result_len) {
        Err(error_with_hint(
            line,
            "Memory limit exceeded",
            "This string repeat would use too much memory.",
        ))
    } else {
        Ok(())
    }
}

/// Pre-flight check for string concat
pub fn check_string_concat_memory(a_len: usize, b_len: usize, line: u32) -> Result<(), BopError> {
    let result_len = a_len + b_len;
    if bop_would_exceed(result_len) {
        Err(error_with_hint(
            line,
            "Memory limit exceeded",
            "This string concatenation would use too much memory.",
        ))
    } else {
        Ok(())
    }
}

/// Pre-flight check for array concat
pub fn check_array_concat_memory(a_len: usize, b_len: usize, line: u32) -> Result<(), BopError> {
    let result_bytes = (a_len + b_len) * std::mem::size_of::<Value>();
    if bop_would_exceed(result_bytes) {
        Err(error_with_hint(
            line,
            "Memory limit exceeded",
            "This array concatenation would use too much memory.",
        ))
    } else {
        Ok(())
    }
}
