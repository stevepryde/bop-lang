# Standard Library — overview

`bop-std` ships a small set of modules written in Bop itself. They live under the `std.*` namespace and are resolved by the default host (`StandardHost` in `bop-sys`) — any host that defers to `StandardHost::resolve_module` picks them up for free.

The stdlib is deliberately thin. Core math operations are [methods on numbers](../reference/methods.md) (`(-5).abs()`, `(9).sqrt()`, `x.floor()`) — they don't need a module. The stdlib covers what's left: constants, higher-order helpers on arrays, data-structure types, string formatting, JSON, `Result` combinators, test assertions.

## Modules

| Module | What it gives you |
|--------|-------------------|
| [`std.math`](math.md) | `pi`, `e`, `tau`, `clamp`, `sign`, `factorial`, `gcd`, `lcm`, `mean` |
| [`std.iter`](iter.md) | `map`, `filter`, `reduce`, `take`, `drop`, `zip`, `enumerate`, `all`, `any`, `count`, `find`, `find_index`, `flatten`, `sum`, `product`, `min_array`, `max_array` |
| [`std.collections`](collections.md) | `Stack`, `Queue`, `Set` as value-semantics structs |
| [`std.string`](string.md) | `pad_left`, `pad_right`, `center`, `chars`, `reverse`, `is_palindrome`, `count`, `join` |
| [`std.result`](result.md) | `is_ok`, `is_err`, `unwrap`, `expect`, `unwrap_or`, `map`, `map_err`, `and_then` (combinators over the built-in `Result` type) |
| [`std.json`](json.md) | `parse`, `stringify` (RFC-8259, pure Bop) |
| [`std.test`](test.md) | `assert`, `assert_eq`, `assert_near`, `assert_raises` |

## Using the stdlib

`std` modules work with every [`use` form](../modules.md):

```bop
use std.math                   // glob — `pi`, `clamp`, etc. available bare
use std.iter.{map, filter}     // selective
use std.result as r            // aliased
```

The modules are plain Bop source — you can find the implementations in `bop/src/modules/*.bop` if you want to see how a helper is wired, or copy-paste the source into a host that doesn't ship `bop-std`.

## Built-ins that don't need `use`

`Result` and `RuntimeError` are engine built-ins — you never have to `use std.result` to write `Result::Ok(v)` or `Result::Err(e)`. [`std.result`](result.md) adds combinators, not the type itself.

Similarly, `print`, `range`, `rand`, `try_call` are built-ins, not stdlib — see [Built-in Functions](../reference/builtins.md).

## Hosts without the stdlib

Embedders who don't want `bop-std` on the host side can leave `resolve_module` unimplemented; any `use std.*` then fails with "can't resolve module". Nothing about the language depends on the stdlib — it's a convenience, not a runtime prerequisite.
