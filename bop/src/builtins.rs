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
    // `range` operates in integer space — matches Python and
    // keeps `range(5)[2]` predictable. Float args error out.
    let (start, end, step) = match args.len() {
        1 => {
            let n = expect_int("range", &args[0], line)?;
            (0i64, n, 1i64)
        }
        2 => {
            let start = expect_int("range", &args[0], line)?;
            let end = expect_int("range", &args[1], line)?;
            (start, end, if start <= end { 1 } else { -1 })
        }
        3 => {
            let start = expect_int("range", &args[0], line)?;
            let end = expect_int("range", &args[1], line)?;
            let step = expect_int("range", &args[2], line)?;
            if step == 0 {
                return Err(error(line, "range step can't be 0"));
            }
            (start, end, step)
        }
        _ => return Err(error(line, "range takes 1, 2, or 3 arguments")),
    };

    let mut result = Vec::new();
    let mut i = start;
    let max_items = 10_000usize;
    if step > 0 {
        while i < end && result.len() < max_items {
            result.push(Value::Int(i));
            i = match i.checked_add(step) {
                Some(v) => v,
                None => break,
            };
        }
    } else {
        while i > end && result.len() < max_items {
            result.push(Value::Int(i));
            i = match i.checked_add(step) {
                Some(v) => v,
                None => break,
            };
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
        Value::Int(n) => Ok(Value::Int(*n)),
        Value::Number(n) => Ok(Value::Int(*n as i64)),
        Value::Str(s) => {
            // Integer-first parse so `int("42")` stays an Int.
            // Fall back to float-then-truncate for strings like
            // `"3.7"` that Python's `int()` also accepts in a
            // limited way (we allow it via float coercion).
            if let Ok(n) = s.parse::<i64>() {
                return Ok(Value::Int(n));
            }
            let n: f64 = s.parse().map_err(|_| {
                error(line, format!("Can't convert \"{}\" to a number", s))
            })?;
            Ok(Value::Int(n as i64))
        }
        Value::Bool(b) => Ok(Value::Int(if *b { 1 } else { 0 })),
        _ => Err(error(
            line,
            format!("Can't convert {} to int", args[0].type_name()),
        )),
    }
}

/// `float(x)` — phase-6 companion to `int(x)`. Coerces any
/// numeric or numeric-string value to a `Value::Number`.
pub fn builtin_float(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("float", args, 1, line)?;
    match &args[0] {
        Value::Int(n) => Ok(Value::Number(*n as f64)),
        Value::Number(n) => Ok(Value::Number(*n)),
        Value::Str(s) => {
            let n: f64 = s.parse().map_err(|_| {
                error(line, format!("Can't convert \"{}\" to a number", s))
            })?;
            Ok(Value::Number(n))
        }
        Value::Bool(b) => Ok(Value::Number(if *b { 1.0 } else { 0.0 })),
        _ => Err(error(
            line,
            format!("Can't convert {} to float", args[0].type_name()),
        )),
    }
}

pub fn builtin_type(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("type", args, 1, line)?;
    Ok(Value::new_str(args[0].type_name().to_string()))
}

pub fn builtin_abs(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("abs", args, 1, line)?;
    match &args[0] {
        Value::Int(n) => n
            .checked_abs()
            .map(Value::Int)
            .ok_or_else(|| error(line, "Integer overflow in `abs`")),
        Value::Number(n) => Ok(Value::Number(n.abs())),
        other => Err(error(
            line,
            format!("`abs` expects a number, but got {}", other.type_name()),
        )),
    }
}

pub fn builtin_min(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("min", args, 2, line)?;
    min_max(&args[0], &args[1], true, "min", line)
}

pub fn builtin_max(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("max", args, 2, line)?;
    min_max(&args[0], &args[1], false, "max", line)
}

/// Shared `min` / `max` implementation that preserves the input
/// type when both operands are the same numeric shape (Int /
/// Int → Int; Number / Number → Number) and widens to Number
/// on mixed operands.
fn min_max(a: &Value, b: &Value, pick_smaller: bool, fname: &str, line: u32) -> Result<Value, BopError> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => {
            let pick = if pick_smaller {
                (*x).min(*y)
            } else {
                (*x).max(*y)
            };
            Ok(Value::Int(pick))
        }
        (Value::Number(x), Value::Number(y)) => {
            let pick = if pick_smaller { x.min(*y) } else { x.max(*y) };
            Ok(Value::Number(pick))
        }
        (Value::Int(x), Value::Number(y)) => {
            let xf = *x as f64;
            let pick = if pick_smaller { xf.min(*y) } else { xf.max(*y) };
            Ok(Value::Number(pick))
        }
        (Value::Number(x), Value::Int(y)) => {
            let yf = *y as f64;
            let pick = if pick_smaller { x.min(yf) } else { x.max(yf) };
            Ok(Value::Number(pick))
        }
        _ => Err(error(
            line,
            format!(
                "`{}` expects two numbers, but got {} and {}",
                fname,
                a.type_name(),
                b.type_name()
            ),
        )),
    }
}

// ─── Math builtins ─────────────────────────────────────────────
//
// These wrap `f64::*` operations that can't be implemented in
// Bop itself. They're always available (no `import` needed);
// `std.math` in the stdlib exposes constants (`pi`, `e`) plus
// convenience wrappers that call these under the hood.

/// `sqrt(x)` — non-negative square root. Matches `f64::sqrt`
/// (returns NaN for negative inputs rather than raising).
pub fn builtin_sqrt(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("sqrt", args, 1, line)?;
    let x = expect_number("sqrt", &args[0], line)?;
    Ok(Value::Number(x.sqrt()))
}

/// `sin(x)` — sine of `x` in radians.
pub fn builtin_sin(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("sin", args, 1, line)?;
    let x = expect_number("sin", &args[0], line)?;
    Ok(Value::Number(x.sin()))
}

/// `cos(x)` — cosine of `x` in radians.
pub fn builtin_cos(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("cos", args, 1, line)?;
    let x = expect_number("cos", &args[0], line)?;
    Ok(Value::Number(x.cos()))
}

/// `tan(x)` — tangent of `x` in radians.
pub fn builtin_tan(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("tan", args, 1, line)?;
    let x = expect_number("tan", &args[0], line)?;
    Ok(Value::Number(x.tan()))
}

/// `floor(x)` — largest integer ≤ `x`. Returns `Int` when the
/// result fits in `i64`, else `Number` (matches the widening
/// convention: we'd rather stay lossless than truncate).
pub fn builtin_floor(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("floor", args, 1, line)?;
    match &args[0] {
        Value::Int(n) => Ok(Value::Int(*n)),
        Value::Number(n) => Ok(finite_to_int_or_number(n.floor())),
        other => Err(error(
            line,
            format!("`floor` expects a number, but got {}", other.type_name()),
        )),
    }
}

/// `ceil(x)` — smallest integer ≥ `x`. Return-type rules
/// mirror [`builtin_floor`].
pub fn builtin_ceil(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("ceil", args, 1, line)?;
    match &args[0] {
        Value::Int(n) => Ok(Value::Int(*n)),
        Value::Number(n) => Ok(finite_to_int_or_number(n.ceil())),
        other => Err(error(
            line,
            format!("`ceil` expects a number, but got {}", other.type_name()),
        )),
    }
}

/// `round(x)` — nearest integer, ties away from zero. Return
/// type mirrors [`builtin_floor`].
pub fn builtin_round(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("round", args, 1, line)?;
    match &args[0] {
        Value::Int(n) => Ok(Value::Int(*n)),
        Value::Number(n) => Ok(finite_to_int_or_number(n.round())),
        other => Err(error(
            line,
            format!("`round` expects a number, but got {}", other.type_name()),
        )),
    }
}

/// `pow(base, exp)` — `base` raised to `exp`. Returns `Number`
/// even for integer inputs — the full result could overflow
/// `i64` silently otherwise.
pub fn builtin_pow(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("pow", args, 2, line)?;
    let base = expect_number("pow", &args[0], line)?;
    let exp = expect_number("pow", &args[1], line)?;
    Ok(Value::Number(base.powf(exp)))
}

/// `log(x)` — natural log (ln). Matches `f64::ln`.
pub fn builtin_log(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("log", args, 1, line)?;
    let x = expect_number("log", &args[0], line)?;
    Ok(Value::Number(x.ln()))
}

/// `exp(x)` — e^x.
pub fn builtin_exp(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("exp", args, 1, line)?;
    let x = expect_number("exp", &args[0], line)?;
    Ok(Value::Number(x.exp()))
}

/// Convert a finite `f64` that's already integer-valued into a
/// `Value::Int` when it fits in `i64`; fall back to
/// `Value::Number` otherwise. Non-finite inputs stay as
/// `Number` (the caller's `f64::floor` / `ceil` / `round`
/// already handled `NaN` / `±inf` correctly).
fn finite_to_int_or_number(n: f64) -> Value {
    if n.is_finite() && n >= i64::MIN as f64 && n <= i64::MAX as f64 {
        Value::Int(n as i64)
    } else {
        Value::Number(n)
    }
}

pub fn builtin_rand(args: &[Value], line: u32, rand_state: &mut u64) -> Result<Value, BopError> {
    expect_args("rand", args, 1, line)?;
    let n = expect_int("rand", &args[0], line)?;
    if n <= 0 {
        return Err(error(line, "rand needs a positive number"));
    }
    // Simple PCG-style PRNG for deterministic behaviour
    *rand_state = rand_state
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let value = (*rand_state >> 33) % (n as u64);
    Ok(Value::Int(value as i64))
}

pub fn builtin_len(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("len", args, 1, line)?;
    match &args[0] {
        // `len` returns a count — always an Int now.
        Value::Str(s) => Ok(Value::Int(s.chars().count() as i64)),
        Value::Array(a) => Ok(Value::Int(a.len() as i64)),
        Value::Dict(d) => Ok(Value::Int(d.len() as i64)),
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
        Value::Int(n) => Ok(*n as f64),
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

/// Like [`expect_number`] but strictly requires an `Int`. Used
/// by builtins that have to produce exact integer counts
/// (e.g. `range`, `rand`). `Number` inputs are rejected rather
/// than silently truncated.
pub fn expect_int(
    func_name: &str,
    val: &Value,
    line: u32,
) -> Result<i64, BopError> {
    match val {
        Value::Int(n) => Ok(*n),
        _ => Err(error(
            line,
            format!(
                "`{}` expects an int, but got {}",
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
    // Line numbers are integers — use Int now that phase 6
    // distinguishes them from floats.
    let line = Value::Int(err.line.unwrap_or(0) as i64);
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
