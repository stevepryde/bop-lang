//! Language-level builtins (`range`, `str`, `int`, `type`, `len`, ...) and
//! the shared argument-validation helpers used across the runtime.
//!
//! These are pure-data operations on `Value`. Host-backed builtins like
//! file I/O live in `bop-sys` instead.

#[cfg(feature = "no_std")]
use alloc::{format, string::{String, ToString}, vec::Vec};

use crate::error::BopError;
use crate::memory::bop_would_exceed;
use crate::parser::{VariantDecl, VariantKind};
use crate::value::Value;

// ─── Engine-wide builtin types ────────────────────────────────────
//
// `Result` and `RuntimeError` are pre-declared in every engine
// (walker, VM, AOT) so:
//
//   - `try` / `try_call` can construct `Result::Ok(..)` /
//     `Result::Err(RuntimeError { .. })` without requiring the
//     program to have imported `std.result` first;
//   - user programs can write `Result::Ok(..)` or match on
//     `RuntimeError { message, line }` out of the box;
//   - engine-to-engine behaviour stays in lockstep — each engine
//     seeds its type table from these same helpers, so the
//     shapes can't drift.
//
// The combinator fns (`is_ok`, `unwrap`, `map`, …) stay in
// `std.result`; only the bare type shapes live here.

/// The canonical `Result { Ok(value), Err(error) }` enum shape,
/// seeded into every engine's type registry at construction time.
pub fn builtin_result_variants() -> Vec<VariantDecl> {
    alloc_import::vec![
        VariantDecl {
            name: String::from("Ok"),
            kind: VariantKind::Tuple(alloc_import::vec![String::from("value")]),
        },
        VariantDecl {
            name: String::from("Err"),
            kind: VariantKind::Tuple(alloc_import::vec![String::from("error")]),
        },
    ]
}

/// The canonical `RuntimeError { message, line }` struct field
/// list. `try_call` produces these directly; declaring them as a
/// builtin lets user code pattern-match the same shape.
pub fn builtin_runtime_error_fields() -> Vec<String> {
    alloc_import::vec![String::from("message"), String::from("line")]
}

/// The canonical `Iter { Next(value), Done }` enum shape —
/// lazy iterators' return type from `.next()`. Seeded into every
/// engine's type registry alongside `Result` so user code can
/// pattern-match `Iter::Next(v) | Iter::Done` without importing
/// anything.
pub fn builtin_iter_variants() -> Vec<VariantDecl> {
    alloc_import::vec![
        VariantDecl {
            name: String::from("Next"),
            kind: VariantKind::Tuple(alloc_import::vec![String::from("value")]),
        },
        VariantDecl {
            name: String::from("Done"),
            kind: VariantKind::Unit,
        },
    ]
}

/// Build `Iter::Next(value)` with the builtin module path so the
/// caller's pattern against `Iter::Next(v)` fires regardless of
/// which module the iterator's `.next()` was declared in.
pub fn make_iter_next(value: Value) -> Value {
    let mut items: Vec<Value> = Vec::with_capacity(1);
    items.push(value);
    Value::new_enum_tuple(
        String::from(crate::value::BUILTIN_MODULE_PATH),
        String::from("Iter"),
        String::from("Next"),
        items,
    )
}

/// Build the `Iter::Done` sentinel. Carries the builtin module
/// path for the same matching reason as [`make_iter_next`].
pub fn make_iter_done() -> Value {
    Value::new_enum_unit(
        String::from(crate::value::BUILTIN_MODULE_PATH),
        String::from("Iter"),
        String::from("Done"),
    )
}

// Small alias so this file compiles both under std and no_std. The
// parser module already uses `alloc::vec!` under no_std, so the
// engines follow the same convention here. Nothing clever — just a
// re-export that picks the right `vec!` macro per config.
#[cfg(not(feature = "no_std"))]
use std as alloc_import;
#[cfg(feature = "no_std")]
use alloc as alloc_import;

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

/// Convert a finite `f64` that's already integer-valued into a
/// `Value::Int` when it fits in `i64`; fall back to
/// `Value::Number` otherwise. Non-finite inputs stay as
/// `Number` (the caller's `f64::floor` / `ceil` / `round`
/// already handled `NaN` / `±inf` correctly).
pub fn finite_to_int_or_number(n: f64) -> Value {
    if n.is_finite() && n >= i64::MIN as f64 && n <= i64::MAX as f64 {
        Value::Int(n as i64)
    } else {
        Value::Number(n)
    }
}

/// `panic(message)` — raise a non-fatal runtime error carrying
/// `message`. Useful for stdlib helpers (`unwrap`, `expect`,
/// `assert_*`) that need to bail with a readable message from an
/// expression position where a plain `return` isn't enough.
///
/// Non-fatal, so `try_call` catches it — same contract as any
/// other runtime error the program raises.
pub fn builtin_panic(args: &[Value], line: u32) -> Result<Value, BopError> {
    expect_args("panic", args, 1, line)?;
    let message = match &args[0] {
        Value::Str(s) => s.as_str().to_string(),
        // Non-string arguments are stringified via Display so a
        // caller that hands us a struct or int still gets a
        // useful trace — cheaper than rejecting and forcing the
        // caller to add `.to_str()`.
        other => format!("{}", other),
    };
    Err(error(line, message))
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
        is_try_return: false,
    }
}

/// Like [`error`] but takes a niche-packed column alongside
/// the line. Call sites with an `Expr` or `Stmt` in hand
/// prefer this over `error` so the rendered carat points at
/// the offending character rather than just the line start.
pub fn error_at(
    line: u32,
    column: Option<core::num::NonZeroU32>,
    message: impl Into<String>,
) -> BopError {
    BopError {
        line: Some(line),
        column: column.map(|c| c.get()),
        message: message.into(),
        friendly_hint: None,
        is_fatal: false,
        is_try_return: false,
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
        is_try_return: false,
    }
}

/// Column-aware variant of [`error_with_hint`]. Same hint
/// payload, plus a `column` slot so the renderer can draw the
/// carat.
pub fn error_with_hint_at(
    line: u32,
    column: Option<core::num::NonZeroU32>,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> BopError {
    BopError {
        line: Some(line),
        column: column.map(|c| c.get()),
        message: message.into(),
        friendly_hint: Some(hint.into()),
        is_fatal: false,
        is_try_return: false,
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
        is_try_return: false,
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
        is_try_return: false,
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
/// successful call. `Result` is an engine builtin, so the value
/// carries `<builtin>` as its module path — any program that
/// matches it via `Result::Ok(v)` resolves `Result` to the same
/// builtin in its own type-binding scope.
pub fn make_try_call_ok(value: Value) -> Value {
    let mut items: Vec<Value> = Vec::with_capacity(1);
    items.push(value);
    Value::new_enum_tuple(
        String::from(crate::value::BUILTIN_MODULE_PATH),
        String::from("Result"),
        String::from("Ok"),
        items,
    )
}

/// Build the `Result::Err(RuntimeError { message, line })`
/// variant `try_call` returns on a caught non-fatal error.
/// `RuntimeError` is also a builtin — same `<builtin>` module
/// path as `Result`.
pub fn make_try_call_err(err: &BopError) -> Value {
    let message = Value::new_str(err.message.clone());
    // Line numbers are integers — use Int now that phase 6
    // distinguishes them from floats.
    let line = Value::Int(err.line.unwrap_or(0) as i64);
    let mut fields: Vec<(String, Value)> = Vec::with_capacity(2);
    fields.push((String::from("message"), message));
    fields.push((String::from("line"), line));
    let rt_err = Value::new_struct(
        String::from(crate::value::BUILTIN_MODULE_PATH),
        String::from("RuntimeError"),
        fields,
    );
    let mut items: Vec<Value> = Vec::with_capacity(1);
    items.push(rt_err);
    Value::new_enum_tuple(
        String::from(crate::value::BUILTIN_MODULE_PATH),
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
