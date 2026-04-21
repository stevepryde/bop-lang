# Error Handling

Bop uses a `Result`-shaped value model for recoverable errors, with two language features that make it ergonomic:

- `try` — unwrap an `Ok(v)` or propagate an `Err(e)` up to the enclosing function.
- `try_call(f)` — run a zero-arg callable, catch any runtime error, and return the outcome as a `Result`.

Both `Result` and `RuntimeError` are **engine built-ins** — always in scope, never need `use std.result` to exist. (The `std.result` module exists for combinators, not for the types.)

## The `Result` type

```
enum Result {
  Ok(value),
  Err(error),
}
```

By convention, `Ok(v)` carries the successful value and `Err(e)` carries whatever describes the failure — a string, a struct, anything. Values from fallible operations are the typical shape:

```bop
fn parse_positive(s) {
  let n = int(s)
  if n <= 0 {
    return Result::Err("must be positive, got {n}")
  }
  return Result::Ok(n)
}

print(parse_positive("42"))    // Result::Ok(42)
print(parse_positive("-3"))    // Result::Err("must be positive, got -3")
```

## The `try` operator

`try expr` evaluates `expr` and:

- If the result is `Result::Ok(v)`, unwraps it to `v`.
- If the result is `Result::Err(e)`, immediately returns `e` from the enclosing function as-is (wrapped in the same `Err` variant the caller will see).
- If the result is anything else (not `Result`-shaped), raises a runtime error.

```bop
fn pipeline(s) {
  let n = try parse_positive(s)        // Err propagates; Ok unwraps to `n`
  let doubled = try double_checked(n)
  return Result::Ok(doubled)
}

print(pipeline("21"))    // Result::Ok(42)
print(pipeline("-3"))    // Result::Err("must be positive, got -3")
```

Because `try` propagates by returning from the *enclosing function*, it only works inside fn bodies. A top-level `try` that hits an `Err` raises a runtime error — wrap the call site in a fn.

### Unit-Ok

`try Result::Ok` with no payload (or `try Result::Ok` where `Ok` is a unit variant) yields `none`. Mostly relevant for APIs where the success case carries no meaningful value.

## `try_call(f)`

Catch runtime errors from a zero-arg callable. Returns `Result::Ok(value)` on success or `Result::Err(RuntimeError { message, line })` on a caught error.

```bop
let r = try_call(fn() { return 1 / 0 })

print(match r {
  Result::Ok(v)                      => "got {v}",
  Result::Err(RuntimeError { message, line }) =>
    "failed at line {line}: {message}",
})
// failed at line 1: Division by zero
```

`try_call` is Bop's answer to exception-like error handling without exceptions. It *only* catches **non-fatal** errors. Fatal conditions — step-budget exhaustion, memory-limit violation, host `on_tick` returning `BopError::fatal` — are **not** caught. That keeps the sandbox invariant intact: a runaway loop can't wrap itself in `try_call` and keep going.

### `RuntimeError` — the caught error shape

```
struct RuntimeError {
  message,   // string
  line,      // int — 1-indexed source line of the failing expression
}
```

You can construct one explicitly (it's a regular struct), but most of the time you'll see them as the payload inside `Result::Err(...)` returned from `try_call`.

## Combinators — `std.result`

The `std.result` module provides the usual Result helpers. All take a `Result` and don't panic on `Err`:

```bop
use std.result

print(is_ok(Result::Ok(1)))                  // true
print(is_err(Result::Err("oops")))            // true

// unwrap_or — default on Err
print(unwrap_or(Result::Ok(10), 0))          // 10
print(unwrap_or(Result::Err("fail"), 0))     // 0

// map — transform the Ok payload, pass Err through
print(map(Result::Ok(5), fn(n) { return n * n }))     // Result::Ok(25)
print(map(Result::Err("x"), fn(n) { return n * n }))  // Result::Err("x")

// and_then — monadic bind (for chaining fallible steps)
fn halve(x) {
  if x % 2 == 0 { return Result::Ok(int(x / 2)) }
  return Result::Err("odd")
}
print(and_then(and_then(Result::Ok(8), halve), halve))   // Result::Ok(2)
print(and_then(Result::Ok(7), halve))                     // Result::Err("odd")
```

Available: `is_ok`, `is_err`, `unwrap`, `expect`, `unwrap_or`, `map`, `map_err`, `and_then`.

`unwrap` and `expect` raise a runtime error on `Err` — use sparingly, and prefer `try` / pattern matching in production code.

## When to use which

| Situation | Use |
|-----------|-----|
| Writing a fallible function | Return `Result::Ok(v)` / `Result::Err(e)` |
| Chaining several fallible calls | `try` inside a fn, or `and_then` |
| Running user-supplied code with a safety net | `try_call(fn() { ... })` |
| Handling every `Err` case explicitly | `match` |
| You know it's `Ok` and want the value | `unwrap()` / `expect("...")` (sparingly) |
| Supplying a default on `Err` | `unwrap_or(r, default)` |

## Fatal vs non-fatal

- **Non-fatal** (catchable by `try_call`): division by zero, "variable not found", type mismatches, host-raised errors via `BopError::runtime`, wrong arg count, missing field, etc.
- **Fatal** (not catchable): step-budget exceeded, memory-limit exceeded, fn-call-depth exceeded, host-raised `BopError::fatal`.

A script can observe whether an error was fatal by inspecting whether `try_call` caught it — fatal errors propagate past `try_call` to the host.
