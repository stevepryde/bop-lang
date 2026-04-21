# std.math

Numeric constants and helpers that aren't idiomatic as methods.

> The core math operations — `abs`, `sqrt`, `sin`, `cos`, `tan`, `floor`, `ceil`, `round`, `pow`, `log`, `exp`, `min`, `max` — are [methods on numbers](../reference/methods.md), not stdlib functions. They live in core because they wrap `f64::*` operations that Bop can't implement itself.

## Import

```bop
use std.math                           // glob
use std.math.{PI, clamp}               // selective
use std.math as m                      // aliased
```

## Constants

All three are `const` (all-caps name, value is fixed at module load).

| Name | Value |
|------|-------|
| `PI` | `3.141592653589793` |
| `E` | `2.718281828459045` |
| `TAU` | `6.283185307179586` |

## Functions

### `clamp(x, lo, hi)`

Clamp `x` into the range `[lo, hi]`. Works on any mix of `int` and `number`; the return type mirrors the widest input.

```bop
use std.math.{clamp}
print(clamp(5, 0, 10))     // 5
print(clamp(-3, 0, 10))    // 0
print(clamp(42, 0, 10))    // 10
```

### `sign(x)`

Returns `-1`, `0`, or `1`. Works on both `int` and `number`.

```bop
use std.math.{sign}
print(sign(-7))       // -1
print(sign(0))        // 0
print(sign(3.14))     // 1
```

### `factorial(n)`

`n!` using iterative multiplication. Raises an integer-overflow error for `n ≥ 21` (the smallest factorial that doesn't fit in `i64`). Negative `n` returns `0`.

```bop
use std.math.{factorial}
print(factorial(5))    // 120
print(factorial(10))   // 3628800
```

### `gcd(a, b)`

Greatest common divisor using the Euclidean algorithm. Handles negatives by taking absolute values. `gcd(0, 0)` returns `0`.

```bop
use std.math.{gcd}
print(gcd(12, 18))    // 6
print(gcd(-15, 25))   // 5
```

### `lcm(a, b)`

Least common multiple. `lcm(0, x)` is `0` (so callers don't have to special-case).

```bop
use std.math.{lcm}
print(lcm(4, 6))     // 12
print(lcm(0, 9))     // 0
```

### `mean(arr)`

Arithmetic mean of a numeric array. Raises on an empty array so callers notice rather than silently getting `0`.

```bop
use std.math.{mean}
print(mean([1, 2, 3, 4]))     // 2.5
print(mean([10.0, 20.0]))     // 15
```

Need the sum or product instead? Use [`std.iter.sum`](iter.md#sumarr) / [`std.iter.product`](iter.md#productarr).
