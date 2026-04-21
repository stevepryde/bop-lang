# std.result

Combinators for the built-in `Result` type.

> `Result { Ok(value), Err(error) }` and `RuntimeError { message, line }` are **engine built-ins** — you don't need `use std.result` to write `Result::Ok(v)` or match on `Result::Err(e)`. See [Error Handling](../errors.md). This module only adds the helpers.

## Import

```bop
use std.result                                  // glob
use std.result.{is_ok, unwrap_or, map}          // selective
use std.result as r                             // aliased
```

## Predicates

### `is_ok(r)` / `is_err(r)`

```bop
use std.result.{is_ok, is_err}
print(is_ok(Result::Ok(1)))         // true
print(is_err(Result::Err("oops"))) // true
```

## Unwrapping

### `unwrap(r)`

Return the `Ok` payload; raise a runtime error on `Err` (via the [`panic`](../reference/builtins.md#panicmessage) builtin — `Err(...).inspect()` shows up verbatim in `e.message`). Use sparingly — prefer `try` or pattern matching in production code.

```bop
use std.result.{unwrap}
print(unwrap(Result::Ok(42)))     // 42
// unwrap(Result::Err("bad"))     // runtime error: unwrap on Err: "bad"
```

### `expect(r, message)`

Like `unwrap`, but raises with a caller-supplied message on `Err`.

```bop
use std.result.{expect}
print(expect(Result::Ok(42), "couldn't compute"))  // 42
```

### `unwrap_or(r, default)`

Return the `Ok` payload, or `default` on `Err`.

```bop
use std.result.{unwrap_or}
print(unwrap_or(Result::Ok(10), 0))         // 10
print(unwrap_or(Result::Err("fail"), 0))    // 0
```

## Transforms

### `map(r, f)`

Apply `f` to the `Ok` payload; pass `Err` through unchanged.

```bop
use std.result.{map}
print(map(Result::Ok(5), fn(n) { return n * n }))      // Result::Ok(25)
print(map(Result::Err("x"), fn(n) { return n * n }))   // Result::Err("x")
```

### `map_err(r, f)`

Apply `f` to the `Err` payload; pass `Ok` through unchanged.

```bop
use std.result.{map_err}
print(map_err(Result::Err("fail"), fn(e) { return e + "!" }))
// Result::Err("fail!")
```

### `and_then(r, f)`

Monadic bind — if `r` is `Ok(v)`, run `f(v)` (which should itself return a `Result`). If `Err`, pass it through.

```bop
use std.result.{and_then}

fn halve(x) {
  if x % 2 == 0 { return Result::Ok((x / 2).to_int()) }
  return Result::Err("odd")
}

print(and_then(and_then(Result::Ok(8), halve), halve))    // Result::Ok(2)
print(and_then(Result::Ok(7), halve))                      // Result::Err("odd")
```

## When to reach for each

| Situation | Tool |
|-----------|------|
| Have a Result, want the value or propagate | `try` (operator — [Error Handling](../errors.md#the-try-operator)) |
| Have a Result, need a default | `unwrap_or` |
| Have a Result, want to apply a pure function to `Ok` | `map` |
| Chaining several fallible steps | `and_then` |
| Catching a raised runtime error | `try_call` (built-in — [Errors](../errors.md#try_callf)) |
| You know it's Ok and want to panic if not | `unwrap` / `expect` |
