#[cfg(not(feature = "std"))]
use alloc::{format, string::{String, ToString}, vec::Vec};

use crate::builtins::{error, expect_number};
use crate::error::BopError;
use crate::value::{Value, values_equal};

/// Returns (return_value, optional_mutated_object)
pub fn array_method(
    arr: &[Value],
    method: &str,
    args: &[Value],
    line: u32,
) -> Result<(Value, Option<Value>), BopError> {
    match method {
        "len" => Ok((Value::Int(arr.len() as i64), None)),
        "push" => {
            if args.len() != 1 {
                return Err(error(line, ".push() needs exactly 1 argument"));
            }
            let mut new_arr = arr.to_vec();
            new_arr.push(args[0].clone());
            Ok((Value::None, Some(Value::new_array(new_arr))))
        }
        "pop" => {
            let mut new_arr = arr.to_vec();
            let popped = new_arr.pop().unwrap_or(Value::None);
            Ok((popped, Some(Value::new_array(new_arr))))
        }
        "has" => {
            if args.len() != 1 {
                return Err(error(line, ".has() needs exactly 1 argument"));
            }
            let found = arr.iter().any(|v| values_equal(v, &args[0]));
            Ok((Value::Bool(found), None))
        }
        "index_of" => {
            if args.len() != 1 {
                return Err(error(line, ".index_of() needs exactly 1 argument"));
            }
            let idx = arr.iter().position(|v| values_equal(v, &args[0]));
            Ok((Value::Int(idx.map_or(-1, |i| i as i64)), None))
        }
        "insert" => {
            if args.len() != 2 {
                return Err(error(line, ".insert() needs 2 arguments: index and value"));
            }
            let i = expect_number("insert", &args[0], line)? as usize;
            let mut new_arr = arr.to_vec();
            if i > new_arr.len() {
                return Err(error(line, format!("Insert index {} is out of bounds", i)));
            }
            new_arr.insert(i, args[1].clone());
            Ok((Value::None, Some(Value::new_array(new_arr))))
        }
        "remove" => {
            if args.len() != 1 {
                return Err(error(line, ".remove() needs exactly 1 argument (index)"));
            }
            let i = expect_number("remove", &args[0], line)? as usize;
            let mut new_arr = arr.to_vec();
            if i >= new_arr.len() {
                return Err(error(line, format!("Remove index {} is out of bounds", i)));
            }
            let removed = new_arr.remove(i);
            Ok((removed, Some(Value::new_array(new_arr))))
        }
        "slice" => {
            if args.len() != 2 {
                return Err(error(line, ".slice() needs 2 arguments: start and end"));
            }
            let start = expect_number("slice", &args[0], line)? as usize;
            let end = (expect_number("slice", &args[1], line)? as usize).min(arr.len());
            let start = start.min(end);
            let slice = arr[start..end].to_vec();
            Ok((Value::new_array(slice), None))
        }
        "reverse" => {
            let mut new_arr = arr.to_vec();
            new_arr.reverse();
            Ok((Value::None, Some(Value::new_array(new_arr))))
        }
        "sort" => {
            let mut new_arr = arr.to_vec();
            new_arr.sort_by(|a, b| match (a, b) {
                (Value::Int(x), Value::Int(y)) => x.cmp(y),
                (Value::Number(x), Value::Number(y)) => {
                    x.partial_cmp(y).unwrap_or(core::cmp::Ordering::Equal)
                }
                // Mixed numeric sort — widen through f64 so
                // `[1, 2.5, 0]` sorts in the obvious numeric
                // order.
                (Value::Int(x), Value::Number(y)) => (*x as f64)
                    .partial_cmp(y)
                    .unwrap_or(core::cmp::Ordering::Equal),
                (Value::Number(x), Value::Int(y)) => x
                    .partial_cmp(&(*y as f64))
                    .unwrap_or(core::cmp::Ordering::Equal),
                (Value::Str(x), Value::Str(y)) => x.cmp(y),
                _ => core::cmp::Ordering::Equal,
            });
            Ok((Value::None, Some(Value::new_array(new_arr))))
        }
        "join" => {
            if args.len() != 1 {
                return Err(error(line, ".join() needs exactly 1 argument (separator)"));
            }
            let sep = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".join() separator must be a string")),
            };
            let result = arr
                .iter()
                .map(|v| format!("{}", v))
                .collect::<Vec<_>>()
                .join(sep);
            Ok((Value::new_str(result), None))
        }
        _ => Err(error(line, format!("Array doesn't have a .{}() method", method))),
    }
}

pub fn string_method(
    s: &str,
    method: &str,
    args: &[Value],
    line: u32,
) -> Result<(Value, Option<Value>), BopError> {
    match method {
        "len" => Ok((Value::Int(s.chars().count() as i64), None)),
        "contains" => {
            if args.len() != 1 {
                return Err(error(line, ".contains() needs 1 argument"));
            }
            let substr = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".contains() needs a string argument")),
            };
            Ok((Value::Bool(s.contains(substr)), None))
        }
        "starts_with" => {
            if args.len() != 1 {
                return Err(error(line, ".starts_with() needs 1 argument"));
            }
            let prefix = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".starts_with() needs a string")),
            };
            Ok((Value::Bool(s.starts_with(prefix)), None))
        }
        "ends_with" => {
            if args.len() != 1 {
                return Err(error(line, ".ends_with() needs 1 argument"));
            }
            let suffix = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".ends_with() needs a string")),
            };
            Ok((Value::Bool(s.ends_with(suffix)), None))
        }
        "index_of" => {
            if args.len() != 1 {
                return Err(error(line, ".index_of() needs 1 argument"));
            }
            let substr = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".index_of() needs a string")),
            };
            let idx = s.find(substr).map_or(-1, |i| i as i64);
            Ok((Value::Int(idx), None))
        }
        "split" => {
            if args.len() != 1 {
                return Err(error(line, ".split() needs 1 argument"));
            }
            let sep = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".split() needs a string")),
            };
            let parts: Vec<Value> = s
                .split(sep)
                .map(|p| Value::new_str(p.to_string()))
                .collect();
            Ok((Value::new_array(parts), None))
        }
        "replace" => {
            if args.len() != 2 {
                return Err(error(line, ".replace() needs 2 arguments: old and new"));
            }
            let old = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".replace() arguments must be strings")),
            };
            let new = match &args[1] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".replace() arguments must be strings")),
            };
            let result = s.replace(old, new);
            Ok((Value::new_str(result), None))
        }
        "upper" => {
            let result = s.to_uppercase();
            Ok((Value::new_str(result), None))
        }
        "lower" => {
            let result = s.to_lowercase();
            Ok((Value::new_str(result), None))
        }
        "trim" => {
            let result = s.trim().to_string();
            Ok((Value::new_str(result), None))
        }
        "slice" => {
            if args.len() != 2 {
                return Err(error(line, ".slice() needs 2 arguments: start and end"));
            }
            let start = expect_number("slice", &args[0], line)? as usize;
            let chars: Vec<char> = s.chars().collect();
            let end = (expect_number("slice", &args[1], line)? as usize).min(chars.len());
            let start = start.min(end);
            let result: String = chars[start..end].iter().collect();
            Ok((Value::new_str(result), None))
        }
        "to_int" => {
            if !args.is_empty() {
                return Err(error(line, ".to_int() takes no arguments"));
            }
            // Integer-first parse so `"42".to_int()` stays an
            // `Int`. Fall back to float-then-truncate for
            // decimal-shaped strings, matching the old
            // `"3.7".to_int()` behaviour.
            if let Ok(n) = s.parse::<i64>() {
                return Ok((Value::Int(n), None));
            }
            let n: f64 = s.parse().map_err(|_| {
                error(line, format!("Can't convert \"{}\" to a number", s))
            })?;
            Ok((Value::Int(n as i64), None))
        }
        "to_float" => {
            if !args.is_empty() {
                return Err(error(line, ".to_float() takes no arguments"));
            }
            let n: f64 = s.parse().map_err(|_| {
                error(line, format!("Can't convert \"{}\" to a number", s))
            })?;
            Ok((Value::Number(n), None))
        }
        _ => Err(error(line, format!("String doesn't have a .{}() method", method))),
    }
}

pub fn dict_method(
    entries: &[(String, Value)],
    method: &str,
    args: &[Value],
    line: u32,
) -> Result<(Value, Option<Value>), BopError> {
    match method {
        "len" => Ok((Value::Int(entries.len() as i64), None)),
        "keys" => {
            let keys: Vec<Value> = entries
                .iter()
                .map(|(k, _)| Value::new_str(k.clone()))
                .collect();
            Ok((Value::new_array(keys), None))
        }
        "values" => {
            let vals: Vec<Value> = entries.iter().map(|(_, v)| v.clone()).collect();
            Ok((Value::new_array(vals), None))
        }
        "has" => {
            if args.len() != 1 {
                return Err(error(line, ".has() needs 1 argument"));
            }
            let key = match &args[0] {
                Value::Str(s) => s.as_str(),
                _ => return Err(error(line, ".has() needs a string key")),
            };
            Ok((Value::Bool(entries.iter().any(|(k, _)| k == key)), None))
        }
        _ => Err(error(line, format!("Dict doesn't have a .{}() method", method))),
    }
}

/// Methods every value understands: introspection + stringification.
/// Dispatched from `call_method` *before* the type-specific method
/// tables so `x.type()`, `x.to_str()`, and `x.inspect()` work
/// uniformly across every `Value` shape.
///
/// Returns `Ok(Some(result))` when the method name matched,
/// `Ok(None)` when it didn't (so the caller falls through to
/// the type-specific dispatcher). `Err` on arg-count mismatches.
pub fn common_method(
    receiver: &Value,
    method: &str,
    args: &[Value],
    line: u32,
) -> Result<Option<(Value, Option<Value>)>, BopError> {
    match method {
        "type" => {
            crate::builtins::expect_args("type", args, 0, line)?;
            Ok(Some((
                Value::new_str(receiver.type_name().to_string()),
                None,
            )))
        }
        "to_str" => {
            crate::builtins::expect_args("to_str", args, 0, line)?;
            Ok(Some((
                Value::new_str(format!("{}", receiver)),
                None,
            )))
        }
        "inspect" => {
            crate::builtins::expect_args("inspect", args, 0, line)?;
            Ok(Some((Value::new_str(receiver.inspect()), None)))
        }
        _ => Ok(None),
    }
}

/// Method dispatch for `Int` and `Number` receivers. Covers the
/// math operations that used to be global builtins (`abs`,
/// `sqrt`, `sin`, …) plus numeric coercions (`to_int`,
/// `to_float`). Returns `Err` on argument errors or unknown
/// method names — no fall-through.
pub fn numeric_method(
    receiver: &Value,
    method: &str,
    args: &[Value],
    line: u32,
) -> Result<(Value, Option<Value>), BopError> {
    use crate::builtins::{expect_args, finite_to_int_or_number};
    match method {
        // Preserves type: Int stays Int, Number stays Number.
        "abs" => {
            expect_args("abs", args, 0, line)?;
            match receiver {
                Value::Int(n) => n
                    .checked_abs()
                    .map(Value::Int)
                    .map(|v| (v, None))
                    .ok_or_else(|| error(line, "Integer overflow in `.abs()`")),
                Value::Number(n) => Ok((Value::Number(n.abs()), None)),
                _ => unreachable!("numeric_method called on non-numeric receiver"),
            }
        }
        // Square / trig / exp / log: always return `Number`.
        "sqrt" => unary_number(receiver, args, line, "sqrt", crate::math::sqrt),
        "sin" => unary_number(receiver, args, line, "sin", crate::math::sin),
        "cos" => unary_number(receiver, args, line, "cos", crate::math::cos),
        "tan" => unary_number(receiver, args, line, "tan", crate::math::tan),
        "exp" => unary_number(receiver, args, line, "exp", crate::math::exp),
        "log" => unary_number(receiver, args, line, "log", crate::math::ln),
        // Round-to-integer: return Int when the result fits i64,
        // Number otherwise.
        "floor" => unary_round(receiver, args, line, "floor", crate::math::floor),
        "ceil" => unary_round(receiver, args, line, "ceil", crate::math::ceil),
        "round" => unary_round(receiver, args, line, "round", crate::math::round),
        "pow" => {
            expect_args("pow", args, 1, line)?;
            let base = to_f64_or_error(receiver, "pow", line)?;
            let exp = to_f64_or_error(&args[0], "pow", line)?;
            Ok((Value::Number(crate::math::powf(base, exp)), None))
        }
        // Binary pick-min / pick-max. Preserves type when both
        // sides match; widens to Number on mixed operands —
        // same rule the old `a.min(b)` / `a.max(b)` builtins
        // used.
        "min" => pair_pick(receiver, args, line, "min", true),
        "max" => pair_pick(receiver, args, line, "max", false),
        // Explicit numeric coercion. `int` ↔ `int` is a no-op,
        // `number` → `int` truncates toward zero.
        "to_int" => {
            expect_args("to_int", args, 0, line)?;
            match receiver {
                Value::Int(n) => Ok((Value::Int(*n), None)),
                Value::Number(n) => Ok((Value::Int(*n as i64), None)),
                _ => unreachable!(),
            }
        }
        "to_float" => {
            expect_args("to_float", args, 0, line)?;
            match receiver {
                Value::Int(n) => Ok((Value::Number(*n as f64), None)),
                Value::Number(n) => Ok((Value::Number(*n), None)),
                _ => unreachable!(),
            }
        }
        _ => {
            let _ = finite_to_int_or_number;
            Err(error(
                line,
                crate::error_messages::no_such_method(receiver.type_name(), method),
            ))
        }
    }
}

/// Method dispatch for `Bool`. Only the numeric coercions
/// (`true.to_int()` → `1`, etc.); `type` / `to_str` / `inspect`
/// go through `common_method` before this is called.
pub fn bool_method(
    receiver: &Value,
    method: &str,
    args: &[Value],
    line: u32,
) -> Result<(Value, Option<Value>), BopError> {
    use crate::builtins::expect_args;
    let b = match receiver {
        Value::Bool(b) => *b,
        _ => unreachable!("bool_method called on non-bool receiver"),
    };
    match method {
        "to_int" => {
            expect_args("to_int", args, 0, line)?;
            Ok((Value::Int(if b { 1 } else { 0 }), None))
        }
        "to_float" => {
            expect_args("to_float", args, 0, line)?;
            Ok((Value::Number(if b { 1.0 } else { 0.0 }), None))
        }
        _ => Err(error(
            line,
            crate::error_messages::no_such_method("bool", method),
        )),
    }
}

// ─── numeric_method helpers ────────────────────────────────────

/// Coerce an `Int` / `Number` receiver to `f64`. Anything else
/// is a programmer error — `numeric_method` only reaches this
/// helper for numeric receivers.
fn to_f64_or_error(v: &Value, method: &str, line: u32) -> Result<f64, BopError> {
    match v {
        Value::Int(n) => Ok(*n as f64),
        Value::Number(n) => Ok(*n),
        other => Err(error(
            line,
            format!(
                "`.{}` expects a number, got {}",
                method,
                other.type_name()
            ),
        )),
    }
}

/// Shared implementation for trig / exp / log — zero-arg
/// methods that always return a `Number`.
fn unary_number(
    receiver: &Value,
    args: &[Value],
    line: u32,
    method: &str,
    op: fn(f64) -> f64,
) -> Result<(Value, Option<Value>), BopError> {
    crate::builtins::expect_args(method, args, 0, line)?;
    let x = to_f64_or_error(receiver, method, line)?;
    Ok((Value::Number(op(x)), None))
}

/// Shared implementation for `floor` / `ceil` / `round`. Return
/// type mirrors the stdlib: `Int` when the rounded value fits
/// in `i64`, `Number` otherwise.
fn unary_round(
    receiver: &Value,
    args: &[Value],
    line: u32,
    method: &str,
    op: fn(f64) -> f64,
) -> Result<(Value, Option<Value>), BopError> {
    use crate::builtins::{expect_args, finite_to_int_or_number};
    expect_args(method, args, 0, line)?;
    match receiver {
        Value::Int(n) => Ok((Value::Int(*n), None)),
        Value::Number(n) => Ok((finite_to_int_or_number(op(*n)), None)),
        _ => unreachable!("unary_round called on non-numeric receiver"),
    }
}

/// `.min(other)` / `.max(other)` — preserves numeric type
/// when both operands match, widens to Number on mixed shape.
fn pair_pick(
    receiver: &Value,
    args: &[Value],
    line: u32,
    method: &str,
    pick_smaller: bool,
) -> Result<(Value, Option<Value>), BopError> {
    use crate::builtins::expect_args;
    expect_args(method, args, 1, line)?;
    match (receiver, &args[0]) {
        (Value::Int(a), Value::Int(b)) => {
            let pick = if pick_smaller { (*a).min(*b) } else { (*a).max(*b) };
            Ok((Value::Int(pick), None))
        }
        (Value::Number(a), Value::Number(b)) => {
            let pick = if pick_smaller { a.min(*b) } else { a.max(*b) };
            Ok((Value::Number(pick), None))
        }
        (Value::Int(a), Value::Number(b)) => {
            let af = *a as f64;
            let pick = if pick_smaller { af.min(*b) } else { af.max(*b) };
            Ok((Value::Number(pick), None))
        }
        (Value::Number(a), Value::Int(b)) => {
            let bf = *b as f64;
            let pick = if pick_smaller { a.min(bf) } else { a.max(bf) };
            Ok((Value::Number(pick), None))
        }
        (_, other) => Err(error(
            line,
            format!(
                "`.{}({})` expects a number, got {}",
                method,
                other.type_name(),
                other.type_name()
            ),
        )),
    }
}

pub fn is_mutating_method(method: &str) -> bool {
    matches!(
        method,
        "push" | "pop" | "insert" | "remove" | "reverse" | "sort"
    )
}

// ─── Result methods ────────────────────────────────────────────
//
// `Result` is a built-in enum (see `builtins::builtin_result_variants`
// seeded into every engine's type table). Its combinators live
// here so `r.is_ok()`, `r.unwrap()`, etc. are always available
// without `use std.result`. Callable-taking variants (`map`,
// `map_err`, `and_then`) need the evaluator's call primitive and
// therefore live in each engine's MethodCall dispatch alongside
// the user-method table, not here — see [`is_result_callable_method`].

/// True when `receiver` is a `Value::EnumVariant` whose type
/// identity is the built-in `Result`. Shared by each engine
/// so a user enum named `Result` in some module can't
/// accidentally steal the built-in method dispatch.
pub fn is_builtin_result(receiver: &Value) -> bool {
    match receiver {
        Value::EnumVariant(e) => {
            e.module_path() == crate::value::BUILTIN_MODULE_PATH && e.type_name() == "Result"
        }
        _ => false,
    }
}

/// Identifies `map` / `map_err` / `and_then` as the
/// callable-taking Result methods each engine handles inline.
/// Returns `None` for anything else so the caller can fall
/// through to [`result_method`] or the standard dispatcher.
pub enum ResultCallableKind {
    /// `r.map(f)` — `Ok(v)` becomes `Ok(f(v))`, `Err(e)` passes.
    Map,
    /// `r.map_err(f)` — `Err(e)` becomes `Err(f(e))`, `Ok(v)` passes.
    MapErr,
    /// `r.and_then(f)` — `Ok(v)` becomes `f(v)` (expected to
    /// return a Result), `Err(e)` passes.
    AndThen,
}

pub fn is_result_callable_method(method: &str) -> Option<ResultCallableKind> {
    match method {
        "map" => Some(ResultCallableKind::Map),
        "map_err" => Some(ResultCallableKind::MapErr),
        "and_then" => Some(ResultCallableKind::AndThen),
        _ => None,
    }
}

/// Pure Result method dispatch. Handles `is_ok`, `is_err`,
/// `unwrap`, `expect`, `unwrap_or` — the combinators whose
/// implementation doesn't need to invoke a user callable.
///
/// Returns `Ok(Some(value))` when the method name matched,
/// `Ok(None)` when it didn't (so the caller falls through to
/// the callable-taking Result methods, then to the standard
/// dispatcher). Assumes `receiver` is already known to be a
/// built-in Result — callers check [`is_builtin_result`] first.
pub fn result_method(
    receiver: &Value,
    method: &str,
    args: &[Value],
    line: u32,
) -> Result<Option<Value>, BopError> {
    use crate::builtins::expect_args;
    let e = match receiver {
        Value::EnumVariant(e) => e,
        _ => return Ok(None),
    };
    let ok_payload = || -> Option<Value> {
        if e.variant() != "Ok" {
            return None;
        }
        match e.payload() {
            crate::value::EnumPayload::Tuple(items) if items.len() == 1 => {
                Some(items[0].clone())
            }
            _ => None,
        }
    };
    let err_payload = || -> Option<Value> {
        if e.variant() != "Err" {
            return None;
        }
        match e.payload() {
            crate::value::EnumPayload::Tuple(items) if items.len() == 1 => {
                Some(items[0].clone())
            }
            _ => None,
        }
    };
    match method {
        "is_ok" => {
            expect_args("is_ok", args, 0, line)?;
            Ok(Some(Value::Bool(e.variant() == "Ok")))
        }
        "is_err" => {
            expect_args("is_err", args, 0, line)?;
            Ok(Some(Value::Bool(e.variant() == "Err")))
        }
        "unwrap" => {
            expect_args("unwrap", args, 0, line)?;
            if let Some(v) = ok_payload() {
                return Ok(Some(v));
            }
            // Err case — construct the same message std.result
            // used to emit so migrated programs keep their
            // crash-trace text stable.
            let detail = match err_payload() {
                Some(payload) => format!("unwrap on Err: {}", payload.inspect()),
                None => String::from("unwrap on Err"),
            };
            Err(error(line, detail))
        }
        "expect" => {
            expect_args("expect", args, 1, line)?;
            if let Some(v) = ok_payload() {
                return Ok(Some(v));
            }
            let message = match &args[0] {
                Value::Str(s) => s.as_str().to_string(),
                other => format!("{}", other),
            };
            Err(error(line, message))
        }
        "unwrap_or" => {
            expect_args("unwrap_or", args, 1, line)?;
            if let Some(v) = ok_payload() {
                return Ok(Some(v));
            }
            Ok(Some(args[0].clone()))
        }
        _ => Ok(None),
    }
}

/// Build a `Result::Ok(value)` with the builtin's module path so
/// pattern matches against `Result::Ok(_)` fire regardless of
/// which module the receiver came from. Mirror of the helper in
/// `builtins` but specialised to the "wrap a value" case each
/// engine's callable dispatch uses.
pub fn make_result_ok(value: Value) -> Value {
    Value::new_enum_tuple(
        String::from(crate::value::BUILTIN_MODULE_PATH),
        String::from("Result"),
        String::from("Ok"),
        alloc_vec_of(value),
    )
}

/// Same as [`make_result_ok`] but for `Err`.
pub fn make_result_err(value: Value) -> Value {
    Value::new_enum_tuple(
        String::from(crate::value::BUILTIN_MODULE_PATH),
        String::from("Result"),
        String::from("Err"),
        alloc_vec_of(value),
    )
}

fn alloc_vec_of(value: Value) -> Vec<Value> {
    let mut v = Vec::with_capacity(1);
    v.push(value);
    v
}
