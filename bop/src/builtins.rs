//! Language-level builtins (`range`, `str`, `int`, `type`, `len`, ...) and
//! the shared argument-validation helpers used across the runtime.
//!
//! These are pure-data operations on `Value`. Host-backed builtins like
//! file I/O live in `bop-sys` instead.

#[cfg(not(feature = "std"))]
use alloc::{format, string::{String, ToString}, vec::Vec};

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
        Value::Number(n) => Ok(Value::Number(*n as i64 as f64)),
        Value::Str(s) => {
            let n: f64 = s.parse().map_err(|_| {
                error(line, format!("Can't convert \"{}\" to a number", s))
            })?;
            Ok(Value::Number(n as i64 as f64))
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

// ─── Helpers (also used by evaluator / VM / AOT) ────────────────────────────

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
        is_fatal: false,
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
        is_fatal: false,
    }
}

/// Fatal variant of [`error_with_hint`] — `is_fatal = true`
/// blocks `try_call` from swallowing it. Used by resource-
/// limit violations (`too many steps`, `Memory limit
/// exceeded`) so a script can't wrap a step-bomb in
/// `try_call` and keep running.
pub fn error_fatal_with_hint(
    line: u32,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> BopError {
    BopError {
        line: Some(line),
        column: None,
        message: message.into(),
        friendly_hint: Some(hint.into()),
        is_fatal: true,
    }
}

/// Fatal variant of [`error`] (no hint). Same uncatchable
/// contract as [`error_fatal_with_hint`].
pub fn error_fatal(line: u32, message: impl Into<String>) -> BopError {
    BopError {
        line: Some(line),
        column: None,
        message: message.into(),
        friendly_hint: None,
        is_fatal: true,
    }
}

// ─── `try_call` result construction ────────────────────────────
//
// The `try_call(f)` builtin is Lua's `pcall` renamed — it calls
// `f` (a zero-arg callable), catches any non-fatal `BopError`,
// and reports the outcome as a `Result::Ok(value)` or
// `Result::Err(RuntimeError { message, line })` structurally-
// shaped value. These helpers construct those values directly
// via `Value::new_enum_tuple` / `Value::new_struct` and
// therefore don't require the program to have declared
// `Result` or `RuntimeError` — they produce the same shape
// either way, so user code can pattern-match them regardless.
//
// Fatal errors (`is_fatal == true`) are deliberately *not*
// wrapped — `try_call`'s callers never see them. See
// [`BopError::is_fatal`] for why.

/// Build the `Result::Ok(value)` variant `try_call` returns on a
/// successful call.
pub fn make_try_call_ok(value: Value) -> Value {
    let mut items: Vec<Value> = Vec::with_capacity(1);
    items.push(value);
    Value::new_enum_tuple(
        String::from("Result"),
        String::from("Ok"),
        items,
    )
}

/// Build the `Result::Err(RuntimeError { message, line })`
/// variant `try_call` returns on a caught non-fatal error.
pub fn make_try_call_err(err: &BopError) -> Value {
    let message = Value::new_str(err.message.clone());
    let line = Value::Number(err.line.unwrap_or(0) as f64);
    let mut fields: Vec<(String, Value)> = Vec::with_capacity(2);
    fields.push((String::from("message"), message));
    fields.push((String::from("line"), line));
    let rt_err = Value::new_struct(String::from("RuntimeError"), fields);
    let mut items: Vec<Value> = Vec::with_capacity(1);
    items.push(rt_err);
    Value::new_enum_tuple(
        String::from("Result"),
        String::from("Err"),
        items,
    )
}

/// Pre-flight check for string repeat
pub fn check_string_repeat_memory(len: usize, count: usize, line: u32) -> Result<(), BopError> {
    let result_len = len.saturating_mul(count);
    if bop_would_exceed(result_len) {
        Err(error_fatal_with_hint(
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
        Err(error_fatal_with_hint(
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
    let result_bytes = (a_len + b_len) * core::mem::size_of::<Value>();
    if bop_would_exceed(result_bytes) {
        Err(error_fatal_with_hint(
            line,
            "Memory limit exceeded",
            "This array concatenation would use too much memory.",
        ))
    } else {
        Ok(())
    }
}
