# Built-in Functions

Bop ships a compact set of built-in functions that are always in scope. They can't be shadowed by user-defined fns. Host-backed builtins (I/O, time) are provided separately by the embedding host — see `bop-sys`'s `StandardHost` for the reference implementation.

## Output

### `print(args...)`

Prints values to the host's stdout, separated by spaces. Returns `none`.

```bop
print("hello")           // hello
print("x =", 42)         // x = 42
print(1, "plus", 2)      // 1 plus 2
```

Accepts any number of arguments (including zero). Each argument is converted to its string representation (Display) automatically.

### `inspect(value)`

Returns a debug representation of a value as a string. Strings are wrapped in quotes; every other type matches its normal Display but nested strings remain quoted.

```bop
print(inspect("hello"))    // "hello"
print(inspect(42))         // 42
print(inspect([1, 2]))     // [1, 2]
print(inspect([1, "two"])) // [1, "two"]
```

Use it when you need to distinguish strings from other types at a glance.

## Type conversion

### `str(value)`

Converts any value to its string representation:

```bop
str(42)        // "42"
str(3.14)      // "3.14"
str(true)      // "true"
str(none)      // "none"
str([1, 2])    // "[1, 2]"
```

### `int(value)`

Coerces to an `int`. Truncates toward zero for numbers; parses strings as integer-then-float.

```bop
int(3.9)       // 3
int(-2.7)      // -2
int("42")      // 42
int("3.7")     // 3
int(true)      // 1
int(false)     // 0
```

Raises a runtime error if a string can't be parsed.

### `float(value)`

Coerces to a `number`. Widens ints, parses strings.

```bop
float(5)       // 5       (stored as number)
float("3.14")  // 3.14
float(true)    // 1
```

### `type(value)`

Returns the type name as a string:

```bop
type(42)           // "int"
type(3.14)         // "number"
type("hello")      // "string"
type(true)         // "bool"
type(none)         // "none"
type([1, 2])       // "array"
type({"a": 1})     // "dict"
type(fn(x){x})     // "fn"
type(Point{x:0,y:0})   // "struct"    (assuming `struct Point`)
type(Color::Red)       // "enum"      (assuming `enum Color`)
```

## Math

### `abs(x)`

Absolute value. Preserves the numeric type (`int` stays `int`; `number` stays `number`).

```bop
abs(-5)     // 5
abs(-2.7)   // 2.7
```

Integer overflow on `abs(i64::MIN)` surfaces as a runtime error.

### `min(a, b)` / `max(a, b)`

Pair-wise min / max. Preserves type when both sides match; widens to `number` on mixed `int` / `number`.

```bop
min(3, 7)      // 3
max(-1, -5)    // -1
max(1, 2.5)    // 2.5   (widened)
```

### `rand(n)`

Returns a random integer from `0` to `n - 1`, inclusive. `n` must be a positive integer.

```bop
rand(6)     // 0..=5 (die roll)
rand(2)     // 0 or 1 (coin flip)
```

Uses a deterministic PRNG seeded per-session. The same inputs produce the same sequence — handy for tests, surprising for crypto (don't use it for that).

### `sqrt(x)`, `sin(x)`, `cos(x)`, `tan(x)`

Standard trigonometric / root functions. Take an `int` or `number`, always return `number`.

```bop
sqrt(9)      // 3
sqrt(2)      // 1.4142135623730951
sin(0)       // 0
cos(0)       // 1
```

### `floor(x)`, `ceil(x)`, `round(x)`

Round to an integer. When the rounded result fits in an `i64`, the return type is `int`; otherwise it's a `number` (so `floor(1e30)` stays a `number` rather than overflowing).

```bop
floor(3.7)           // 3    (int)
ceil(3.2)            // 4    (int)
round(2.5)           // 3    (ties away from zero)
type(floor(3.7))     // "int"
type(floor(1.0e30))  // "number"
```

### `pow(base, exp)`, `log(x)`, `exp(x)`

Floating-point power, natural log, and `e^x`. All return `number`.

```bop
pow(2, 10)     // 1024
log(2.718281828459045)   // 1
exp(1)         // 2.718281828459045
```

## Collections

### `len(value)`

Returns the length of a string, array, or dict, as an `int`.

```bop
len("hello")       // 5
len([1, 2, 3])     // 3
len({"a": 1})      // 1
len("")            // 0
len([])            // 0
```

Raises a runtime error for numbers, bools, `none`, and functions.

### `range(n)` / `range(start, end)` / `range(start, end, step)`

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

## Errors

### `try_call(callable)`

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
