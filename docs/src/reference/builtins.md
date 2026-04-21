# Built-in Functions

Bop ships a very small set of built-in functions that are always in scope. They can't be shadowed by user-defined fns. Host-backed builtins (I/O, time) are provided separately by the embedding host — see `bop-sys`'s `StandardHost` for the reference implementation.

Anything math- or conversion-shaped lives as a [method on a value](methods.md), not as a global — `(-5).abs()`, `"42".to_int()`, `[1, 2, 3].len()`. The global list is deliberately short: only things that are variadic (`print`), constructor-shaped (`range`), session-stateful (`rand`), or that take a callable (`try_call`).

## `print(args...)`

Prints values to the host's stdout, separated by spaces. Returns `none`.

```bop
print("hello")           // hello
print("x =", 42)         // x = 42
print(1, "plus", 2)      // 1 plus 2
```

Accepts any number of arguments (including zero). Each argument is converted to its string representation (Display) automatically — you don't need `.to_str()` first.

## `range(n)` / `range(start, end)` / `range(start, end, step)`

Builds an array of `int` values.

```bop
range(5)           // [0, 1, 2, 3, 4]
range(2, 6)        // [2, 3, 4, 5]
range(0, 10, 2)    // [0, 2, 4, 6, 8]
range(5, 0)        // [5, 4, 3, 2, 1]  (auto-detects direction)
range(10, 0, -3)   // [10, 7, 4, 1]
```

- With 1 arg: `range(n)` → `[0, 1, ..., n-1]`.
- With 2 args: `range(start, end)` auto-detects direction.
- With 3 args: explicit `step` (error if `step == 0`).
- All arguments must be `int` — floats are rejected.
- Maximum 10,000 elements. Bigger ranges raise a runtime error so loops can't build gigabyte arrays by accident.

## `rand(n)`

Returns a random integer from `0` to `n - 1`, inclusive. `n` must be a positive integer.

```bop
rand(6)     // 0..=5 (die roll)
rand(2)     // 0 or 1 (coin flip)
```

Uses a deterministic PRNG seeded per-session. The same inputs produce the same sequence — handy for tests, surprising for crypto (don't use it for that).

## `try_call(callable)`

Runs `callable` with no arguments and catches any non-fatal runtime error:

```bop
let r = try_call(fn() { return 1 / 0 })
print(match r {
  Result::Ok(v) => v,
  Result::Err(e) => "caught: " + e.message,
})
// caught: Division by zero
```

On success returns `Result::Ok(value)`; on a non-fatal error returns `Result::Err(RuntimeError { message, line })`. Fatal errors (step-limit / memory-limit / fn-call-depth) are *not* caught — they propagate past `try_call` unchanged so the sandbox invariant holds.

See [Error Handling](../errors.md) for the full story on `try`, `try_call`, `Result`, and `RuntimeError`.

## Everything else lives on a value

Category | Was | Now
--- | --- | ---
Introspection | `type(x)`, `inspect(x)` | `x.type()`, `x.inspect()`
Conversion | `str(x)`, `int(x)`, `float(x)` | `x.to_str()`, `x.to_int()`, `x.to_float()`
Length | `len(x)` | `x.len()` (arrays, strings, dicts)
Absolute / min / max | `abs(x)`, `min(a, b)`, `max(a, b)` | `x.abs()`, `a.min(b)`, `a.max(b)`
Trig / roots | `sqrt(x)`, `sin(x)`, `cos(x)`, `tan(x)` | `x.sqrt()`, `x.sin()`, `x.cos()`, `x.tan()`
Rounding | `floor(x)`, `ceil(x)`, `round(x)` | `x.floor()`, `x.ceil()`, `x.round()`
Power / log / exp | `pow(b, e)`, `log(x)`, `exp(x)` | `b.pow(e)`, `x.log()`, `x.exp()`

See [Methods](methods.md) for the full per-type method catalogue.
