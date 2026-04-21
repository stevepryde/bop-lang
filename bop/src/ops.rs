//! Operator primitives shared across execution engines.
//!
//! These are pure functions over `Value` — no interpreter state, no AST.
//! The tree-walking evaluator, the bytecode VM, and AOT-Rust output all
//! dispatch to these so the language-level semantics of `+`, `*`, `==`,
//! etc. live in exactly one place.
//!
//! Short-circuiting operators (`&&`, `||`) are NOT here: they depend on
//! evaluation order and are the engine's responsibility.

#[cfg(not(feature = "std"))]
use alloc::{format, string::ToString, vec::Vec};

use crate::builtins::{
    check_array_concat_memory, check_string_concat_memory, check_string_repeat_memory, error,
    error_with_hint,
};
use crate::error::BopError;
use crate::value::{Value, values_equal};

// ─── Numeric coercion helpers ──────────────────────────────────────
//
// Int↔Number interplay follows Python's rules:
// - `Int op Int` stays Int (overflow → `BopError`).
// - `Int op Number` / `Number op Int` widens to `Number`.
// - `Number op Number` stays `Number`.
//
// Division is split: `/` always returns `Number`; `//` always
// returns `Int` via truncation toward zero. Modulo mirrors the
// operand types.

/// Promote a value to `f64` if it's a numeric type. Used for
/// cross-type widening where one side is `Number`.
fn to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(n) => Some(*n as f64),
        Value::Number(n) => Some(*n),
        _ => None,
    }
}

fn int_overflow(op: &str, line: u32) -> BopError {
    error(line, format!("Integer overflow in `{}`", op))
}

pub fn add(left: &Value, right: &Value, line: u32) -> Result<Value, BopError> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => a
            .checked_add(*b)
            .map(Value::Int)
            .ok_or_else(|| int_overflow("+", line)),
        (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a + b)),
        (Value::Int(a), Value::Number(b)) => Ok(Value::Number(*a as f64 + b)),
        (Value::Number(a), Value::Int(b)) => Ok(Value::Number(a + *b as f64)),
        (Value::Str(a), Value::Str(b)) => {
            check_string_concat_memory(a.len(), b.len(), line)?;
            Ok(Value::new_str(format!("{}{}", a, b)))
        }
        (Value::Str(a), b) => {
            let b_display = format!("{}", b);
            check_string_concat_memory(a.len(), b_display.len(), line)?;
            Ok(Value::new_str(format!("{}{}", a, b_display)))
        }
        (a, Value::Str(b)) => {
            let a_display = format!("{}", a);
            check_string_concat_memory(a_display.len(), b.len(), line)?;
            Ok(Value::new_str(format!("{}{}", a_display, b)))
        }
        (Value::Array(a), Value::Array(b)) => {
            check_array_concat_memory(a.len(), b.len(), line)?;
            let mut result = a.to_vec();
            result.extend(b.to_vec());
            Ok(Value::new_array(result))
        }
        _ => Err(error(
            line,
            format!("Can't add {} and {}", left.type_name(), right.type_name()),
        )),
    }
}

pub fn sub(left: &Value, right: &Value, line: u32) -> Result<Value, BopError> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => a
            .checked_sub(*b)
            .map(Value::Int)
            .ok_or_else(|| int_overflow("-", line)),
        (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a - b)),
        (Value::Int(a), Value::Number(b)) => Ok(Value::Number(*a as f64 - b)),
        (Value::Number(a), Value::Int(b)) => Ok(Value::Number(a - *b as f64)),
        _ => Err(error(
            line,
            format!(
                "Can't use `-` with {} and {}",
                left.type_name(),
                right.type_name()
            ),
        )),
    }
}

pub fn mul(left: &Value, right: &Value, line: u32) -> Result<Value, BopError> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => a
            .checked_mul(*b)
            .map(Value::Int)
            .ok_or_else(|| int_overflow("*", line)),
        (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a * b)),
        (Value::Int(a), Value::Number(b)) => Ok(Value::Number(*a as f64 * b)),
        (Value::Number(a), Value::Int(b)) => Ok(Value::Number(a * *b as f64)),
        // String repeat accepts any numeric count. Integers use
        // their direct value; floats cast through `as usize`
        // after a positivity / finiteness check (unchanged from
        // the pre-phase-6 behaviour).
        (Value::Str(s), Value::Int(n)) | (Value::Int(n), Value::Str(s)) => {
            if *n < 0 {
                return Err(error(line, format!("Can't repeat a string {} times", n)));
            }
            let count = *n as usize;
            check_string_repeat_memory(s.len(), count, line)?;
            Ok(Value::new_str(s.repeat(count)))
        }
        (Value::Str(s), Value::Number(n)) | (Value::Number(n), Value::Str(s)) => {
            let nf = *n;
            if nf < 0.0 || !nf.is_finite() {
                return Err(error(line, format!("Can't repeat a string {} times", nf)));
            }
            let count = nf as usize;
            check_string_repeat_memory(s.len(), count, line)?;
            Ok(Value::new_str(s.repeat(count)))
        }
        _ => Err(error(
            line,
            format!(
                "Can't multiply {} and {}",
                left.type_name(),
                right.type_name()
            ),
        )),
    }
}

/// `/` always returns a `Number`, even for `Int / Int`. Matches
/// Python's `/` and sidesteps the "1 / 2 == 0" surprise.
pub fn div(left: &Value, right: &Value, line: u32) -> Result<Value, BopError> {
    let a = to_f64(left).ok_or_else(|| {
        error(
            line,
            format!("Can't divide {} by {}", left.type_name(), right.type_name()),
        )
    })?;
    let b = to_f64(right).ok_or_else(|| {
        error(
            line,
            format!("Can't divide {} by {}", left.type_name(), right.type_name()),
        )
    })?;
    if b == 0.0 {
        return Err(error_with_hint(
            line,
            "Division by zero",
            "You can't divide by 0.",
        ));
    }
    Ok(Value::Number(a / b))
}

pub fn rem(left: &Value, right: &Value, line: u32) -> Result<Value, BopError> {
    match (left, right) {
        (Value::Int(_), Value::Int(b)) if *b == 0 => Err(error_with_hint(
            line,
            "Modulo by zero",
            "You can't use % with 0.",
        )),
        (Value::Int(a), Value::Int(b)) => a
            .checked_rem(*b)
            .map(Value::Int)
            .ok_or_else(|| int_overflow("%", line)),
        (Value::Number(_), Value::Number(b)) if *b == 0.0 => Err(error_with_hint(
            line,
            "Modulo by zero",
            "You can't use % with 0.",
        )),
        (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a % b)),
        (Value::Int(a), Value::Number(b)) => {
            if *b == 0.0 {
                return Err(error_with_hint(
                    line,
                    "Modulo by zero",
                    "You can't use % with 0.",
                ));
            }
            Ok(Value::Number((*a as f64) % b))
        }
        (Value::Number(a), Value::Int(b)) => {
            if *b == 0 {
                return Err(error_with_hint(
                    line,
                    "Modulo by zero",
                    "You can't use % with 0.",
                ));
            }
            Ok(Value::Number(a % (*b as f64)))
        }
        _ => Err(error(
            line,
            format!(
                "Can't use % with {} and {}",
                left.type_name(),
                right.type_name()
            ),
        )),
    }
}

pub fn eq(left: &Value, right: &Value) -> Value {
    Value::Bool(values_equal(left, right))
}

pub fn not_eq(left: &Value, right: &Value) -> Value {
    Value::Bool(!values_equal(left, right))
}

pub fn lt(left: &Value, right: &Value, line: u32) -> Result<Value, BopError> {
    compare(left, right, |a, b| a < b, "<", line)
}

pub fn gt(left: &Value, right: &Value, line: u32) -> Result<Value, BopError> {
    compare(left, right, |a, b| a > b, ">", line)
}

pub fn lt_eq(left: &Value, right: &Value, line: u32) -> Result<Value, BopError> {
    compare(left, right, |a, b| a <= b, "<=", line)
}

pub fn gt_eq(left: &Value, right: &Value, line: u32) -> Result<Value, BopError> {
    compare(left, right, |a, b| a >= b, ">=", line)
}

pub fn neg(val: &Value, line: u32) -> Result<Value, BopError> {
    match val {
        Value::Int(n) => n
            .checked_neg()
            .map(Value::Int)
            .ok_or_else(|| int_overflow("-", line)),
        Value::Number(n) => Ok(Value::Number(-n)),
        _ => Err(error(line, format!("Can't negate a {}", val.type_name()))),
    }
}

pub fn not(val: &Value) -> Value {
    Value::Bool(!val.is_truthy())
}

/// Coerce any numeric index (Int or Number) to an `i64`. Returns
/// `None` for non-numeric values. Used by the `index_get` /
/// `index_set` paths so both `arr[0]` (Int) and `arr[0.0]`
/// (Number) still work after phase 6's Int/Number split.
fn numeric_index(idx: &Value) -> Option<i64> {
    match idx {
        Value::Int(n) => Some(*n),
        Value::Number(n) => Some(*n as i64),
        _ => None,
    }
}

pub fn index_get(obj: &Value, idx: &Value, line: u32) -> Result<Value, BopError> {
    match obj {
        Value::Array(arr) => {
            let i = numeric_index(idx).ok_or_else(|| {
                error(
                    line,
                    format!(
                        "Can't index {} with {}",
                        obj.type_name(),
                        idx.type_name()
                    ),
                )
            })?;
            let actual = if i < 0 {
                (arr.len() as i64 + i) as usize
            } else {
                i as usize
            };
            arr.get(actual).cloned().ok_or_else(|| {
                error(
                    line,
                    format!(
                        "Index {} is out of bounds (array has {} items)",
                        i,
                        arr.len()
                    ),
                )
            })
        }
        Value::Str(s) => {
            let i = numeric_index(idx).ok_or_else(|| {
                error(
                    line,
                    format!(
                        "Can't index {} with {}",
                        obj.type_name(),
                        idx.type_name()
                    ),
                )
            })?;
            let chars: Vec<char> = s.chars().collect();
            let actual = if i < 0 {
                (chars.len() as i64 + i) as usize
            } else {
                i as usize
            };
            chars
                .get(actual)
                .map(|c| Value::new_str(c.to_string()))
                .ok_or_else(|| {
                    error(
                        line,
                        format!(
                            "Index {} is out of bounds (string has {} characters)",
                            i,
                            chars.len()
                        ),
                    )
                })
        }
        Value::Dict(entries) => match idx {
            Value::Str(key) => entries
                .iter()
                .find(|(k, _)| k.as_str() == key.as_str())
                .map(|(_, v)| v.clone())
                .ok_or_else(|| error(line, format!("Key \"{}\" not found in dict", key))),
            _ => Err(error(
                line,
                format!(
                    "Can't index {} with {}",
                    obj.type_name(),
                    idx.type_name()
                ),
            )),
        },
        _ => Err(error(
            line,
            format!("Can't index {} with {}", obj.type_name(), idx.type_name()),
        )),
    }
}

pub fn index_set(
    obj: &mut Value,
    idx: &Value,
    val: Value,
    line: u32,
) -> Result<(), BopError> {
    match obj {
        Value::Array(arr) => {
            let i = numeric_index(idx).ok_or_else(|| {
                error(line, "Can't set index with these types")
            })?;
            let len = arr.len();
            let actual = if i < 0 {
                (len as i64 + i) as usize
            } else {
                i as usize
            };
            if actual >= len {
                return Err(error(
                    line,
                    format!("Index {} is out of bounds (array has {} items)", i, len),
                ));
            }
            arr.set(actual, val);
            Ok(())
        }
        Value::Dict(entries) => match idx {
            Value::Str(key) => {
                entries.set_key(key, val);
                Ok(())
            }
            _ => Err(error(line, "Can't set index with these types")),
        },
        _ => Err(error(line, "Can't set index with these types")),
    }
}

// ─── Internal helpers ───────────────────────────────────────────────────────

fn compare(
    left: &Value,
    right: &Value,
    f: impl Fn(f64, f64) -> bool,
    op_str: &str,
    line: u32,
) -> Result<Value, BopError> {
    match (left, right) {
        // Int / Int uses exact integer comparison so magnitudes
        // beyond f64's 2^53 precision still compare correctly.
        (Value::Int(a), Value::Int(b)) => {
            let result = match op_str {
                "<" => a < b,
                ">" => a > b,
                "<=" => a <= b,
                _ => a >= b,
            };
            Ok(Value::Bool(result))
        }
        (Value::Number(a), Value::Number(b)) => Ok(Value::Bool(f(*a, *b))),
        // Cross-type numeric comparison widens through `f64`.
        (Value::Int(a), Value::Number(b)) => Ok(Value::Bool(f(*a as f64, *b))),
        (Value::Number(a), Value::Int(b)) => Ok(Value::Bool(f(*a, *b as f64))),
        (Value::Str(a), Value::Str(b)) => {
            let result = match op_str {
                "<" => a < b,
                ">" => a > b,
                "<=" => a <= b,
                _ => a >= b,
            };
            Ok(Value::Bool(result))
        }
        _ => Err(error(
            line,
            format!(
                "Can't compare {} and {} with `{}`",
                left.type_name(),
                right.type_name(),
                op_str
            ),
        )),
    }
}
