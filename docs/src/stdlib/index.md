# Standard Library — overview

`bop-std` ships a small set of modules written in Bop itself. They live under the `std.*` namespace and are resolved by the default host (`StandardHost` in `bop-sys`) — any host that defers to `StandardHost::resolve_module` picks them up for free.

The stdlib is deliberately thin. Core math and `Result` operations are [methods on values](../reference/methods.md) (`(-5).abs()`, `(9).sqrt()`, `r.unwrap_or(0)`, `r.map(f)`) — they don't need a module. The stdlib covers what's left: constants, higher-order helpers on arrays, data-structure types, string formatting, JSON, test assertions.

## Modules

| Module | What it gives you |
|--------|-------------------|
| [`std.math`](math.md) | `PI`, `E`, `TAU`, `clamp`, `sign`, `factorial`, `gcd`, `lcm`, `mean` |
| [`std.iter`](iter.md) | `map`, `filter`, `reduce`, `take`, `drop`, `zip`, `enumerate`, `all`, `any`, `count`, `find`, `find_index`, `flatten`, `sum`, `product`, `min_array`, `max_array` |
| [`std.collections`](collections.md) | `Stack`, `Queue`, `Set` as value-semantics structs |
| [`std.string`](string.md) | `pad_left`, `pad_right`, `center`, `chars`, `reverse`, `is_palindrome`, `count`, `join` |
| [`std.json`](json.md) | `parse`, `stringify` (RFC-8259, pure Bop) |
| [`std.test`](test.md) | `assert`, `assert_eq`, `assert_near`, `assert_raises` |

## Using the stdlib

`std` modules work with every [`use` form](../modules.md):

```bop
use std.math                   // glob — `PI`, `clamp`, etc. available bare
use std.iter.{map, filter}     // selective
use std.json as j              // aliased
```

The modules are plain Bop source — you can find the implementations in `bop/src/modules/*.bop` if you want to see how a helper is wired, or copy-paste the source into a host that doesn't ship `bop-std`.

## Things you might expect to find here

- **`Result` combinators** — `is_ok`, `is_err`, `unwrap`, `expect`, `unwrap_or`, `map`, `map_err`, `and_then` used to live in `std.result`. They're now **methods on the built-in `Result` type** and always available without any import. See [Methods → Result](../reference/methods.md#result-methods--result).
- **`print`, `range`, `rand`, `try_call`, `panic`** — always-in-scope [built-in functions](../reference/builtins.md), not stdlib.
- **Math on numbers** — `abs`, `sqrt`, `sin`, `cos`, `floor`, `ceil`, `round`, `pow`, `log`, `exp`, `min`, `max`, `to_int`, `to_float` are [methods on `int` / `number`](../reference/methods.md#numeric-methods--int-and-number), not stdlib.

## Hosts without the stdlib

Embedders who don't want `bop-std` on the host side can leave `resolve_module` unimplemented; any `use std.*` then fails with "can't resolve module". Nothing about the language depends on the stdlib — it's a convenience, not a runtime prerequisite.
