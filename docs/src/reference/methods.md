# Methods

Bop dispatches methods with `.name(args...)`. Every built-in method on primitives, arrays, strings, and dicts is listed here. User-defined methods on structs use the same syntax — see [Structs & Enums](../data/structs-and-enums.md) for the `fn Type.method(self, ...)` form.

## Methods on every value

Three methods work on any value — introspection + stringification. They're dispatched before the type-specific tables, so they're always available:

| Method | Returns | Notes |
|--------|---------|-------|
| `x.type()` | string | One of `"int"`, `"number"`, `"string"`, `"bool"`, `"none"`, `"array"`, `"dict"`, `"fn"`, `"struct"`, `"enum"`, `"module"`, `"iter"` |
| `x.to_str()` | string | Display repr — same as what `print(x)` would emit for a single arg |
| `x.inspect()` | string | Debug repr — strings are wrapped in `"..."`, nested strings stay quoted inside arrays / dicts |
| `x.is_none()` | bool | `true` iff `x` is the `none` value. Equivalent to `x == none`. |
| `x.is_some()` | bool | Inverse of `.is_none()` — `true` for every value except `none`. |

```bop
print((42).type())                 // "int"
print("hi".to_str())               // "hi"
print("hi".inspect())              // "hi"   (quoted)
print([1, "two"].inspect())        // [1, "two"]

print(none.is_none())              // true
print((0).is_none())               // false — `0` is falsy but not `none`
print(first_result().is_some())    // check an optional return without `== none`
```

> `.is_none()` / `.is_some()` cover Bop's "any variable can be `none`" story — they work on every receiver, not just `Option`-shaped ones (Bop doesn't have `Option`). Equivalent to `x == none` / `x != none`, but reads better in method chains.

### Parens around numeric literals

Number literals need parens before a method call because `.` is otherwise a decimal point:

```bop
// print(42.type())   // parse error — `42.t…` looks like a decimal
print((42).type())     // "int"
print((-5).abs())      // 5
```

Identifiers don't have this problem: `x.type()`, `count.to_str()`, etc.

## Numeric methods — `int` and `number`

All of these work on both `int` and `number` receivers. Return type is noted per method; most math operations always widen to `number`.

| Method | Returns | Description |
|--------|---------|-------------|
| `x.abs()` | int / number | Absolute value. Preserves receiver type. `(-5).abs()` → `5` (int), `(-2.7).abs()` → `2.7` (number). Integer overflow on `(i64::MIN).abs()` is a runtime error. |
| `x.sqrt()` | number | Square root. |
| `x.sin()`, `x.cos()`, `x.tan()` | number | Trig. Angles in radians. |
| `x.exp()` | number | `e^x`. |
| `x.log()` | number | Natural log. |
| `x.pow(e)` | number | `x` raised to `e`. |
| `x.floor()`, `x.ceil()`, `x.round()` | int / number | Round toward `-∞`, `+∞`, or nearest (ties away from zero). Returns `int` when the rounded result fits in `i64`, `number` otherwise (so rounding a number that overflows `i64` stays a `number` instead of raising). Int receivers pass through unchanged. |
| `a.min(b)`, `a.max(b)` | int / number | Pair-wise. Preserves type when both sides match; widens to `number` on mixed int/number. |
| `x.to_int()` | int | Truncates toward zero. `(3.7).to_int()` → `3`, `(-2.7).to_int()` → `-2`. |
| `x.to_float()` | number | Widens `int` → `number`; `number` passes through. |

```bop
print((9).sqrt())                   // 3
print((0).cos())                    // 1
print((2).pow(10))                  // 1024
print((3).min(7))                   // 3
print((1).max(2.5))                 // 2.5   (widened)
print((3.7).floor())                // 3     (int)
print((3.7).floor().type())         // "int"
```

## Boolean methods — `bool`

| Method | Returns | Description |
|--------|---------|-------------|
| `b.to_int()` | int | `true.to_int()` → `1`, `false.to_int()` → `0`. |
| `b.to_float()` | number | `true.to_float()` → `1` (as number), `false.to_float()` → `0`. |

Plus the universal `type` / `to_str` / `inspect`.

## String methods — `string`

See [Strings](../data/strings.md) for worked examples.

| Method | Returns | Description |
|--------|---------|-------------|
| `s.len()` | int | Number of Unicode code points. |
| `s.contains(sub)` | bool | Whether `sub` appears anywhere. |
| `s.starts_with(prefix)` | bool | |
| `s.ends_with(suffix)` | bool | |
| `s.index_of(sub)` | int | Byte index of first occurrence, or `-1` if not found. |
| `s.split(sep)` | array | Split into an array of strings on `sep`. |
| `s.replace(old, new)` | string | Replace every occurrence. |
| `s.upper()`, `s.lower()` | string | Case conversion. |
| `s.trim()` | string | Strip leading / trailing whitespace. |
| `s.slice(start, end)` | string | Substring by code-point index. |
| `s.to_int()` | int | Parse. `"3.7".to_int()` parses as float then truncates → `3`. Raises on junk. |
| `s.to_float()` | number | Parse. Raises on junk. |
| `s.iter()` | iter | Lazy iterator over Unicode code points. See [Iter methods](#iter-methods--iter). |

## Array methods — `array`

See [Arrays](../data/arrays.md) for worked examples.

| Method | Returns | Description |
|--------|---------|-------------|
| `arr.len()` | int | Number of elements. |
| `arr.push(v)` | none | Append. |
| `arr.pop()` | value | Remove and return the last element. |
| `arr.has(v)` | bool | Structural equality check. |
| `arr.index_of(v)` | int | Index of first match, or `-1`. |
| `arr.insert(i, v)` | none | Insert at index, shifting right. |
| `arr.remove(i)` | value | Remove at index, returning the removed value. |
| `arr.slice(start, end)` | array | Sub-array. |
| `arr.reverse()` | none | In-place. |
| `arr.sort()` | none | In-place, numeric or lexicographic depending on element types. |
| `arr.join(sep)` | string | Join after stringifying each element. |
| `arr.iter()` | iter | Lazy iterator over the elements. See [Iter methods](#iter-methods--iter). |

## Dict methods — `dict`

See [Dictionaries](../data/dictionaries.md) for worked examples.

| Method | Returns | Description |
|--------|---------|-------------|
| `d.len()` | int | Number of entries. |
| `d.keys()` | array | All keys as strings. |
| `d.values()` | array | All values. |
| `d.has(key)` | bool | Whether `key` exists. |
| `d.iter()` | iter | Lazy iterator over keys, in declaration order. See [Iter methods](#iter-methods--iter). |

## Result methods — `Result`

`Result` is an [engine built-in](../errors.md). All combinators are methods on the built-in type — no import required. `Ok(x)` and `Err(e)` are [parser-level shorthand](../errors.md#ok--err-shorthand) for `Result::Ok(x)` / `Result::Err(e)` in both expression and pattern position.

| Method | Returns | Description |
|--------|---------|-------------|
| `r.is_ok()` | bool | `true` when `r` is `Result::Ok(_)`. |
| `r.is_err()` | bool | `true` when `r` is `Result::Err(_)`. |
| `r.unwrap()` | value | Payload on `Ok`; raises a runtime error on `Err` (message includes the `.inspect()` of the payload). |
| `r.expect(msg)` | value | Payload on `Ok`; raises with `msg` on `Err`. |
| `r.unwrap_or(default)` | value | Payload on `Ok`; `default` on `Err`. |
| `r.map(f)` | Result | `Ok(v)` → `Ok(f(v))`; `Err(e)` passes through. |
| `r.map_err(f)` | Result | `Err(e)` → `Err(f(e))`; `Ok(v)` passes through. |
| `r.and_then(f)` | Result | `Ok(v)` → `f(v)` (expected to return a Result); `Err(e)` passes through. |

```bop
print(Ok(5).is_ok())                           // true
print(Err("bad").unwrap_or(0))                 // 0
print(Ok(5).map(fn(v) { return v * 2 }))       // Result::Ok(10)
print(Err("x").map(fn(v) { return v * 2 }))    // Result::Err("x")

fn halve(x) {
  if x % 2 == 0 { return Ok((x / 2).to_int()) }
  return Err("odd")
}
print(Ok(8).and_then(halve).and_then(halve))   // Result::Ok(2)
```

## Iter methods — `iter`

An `iter` is Bop's lazy iterator. Values you can iterate over — arrays, strings, dicts, built-in iterators, and user-defined containers — all participate in the same protocol:

1. `v.iter()` returns an iterator.
2. `it.next()` advances it, returning `Iter::Next(value)` or `Iter::Done`.

`for x in v` uses this protocol, so anything with a working `.iter()` method works with `for`.

| Method | Returns | Description |
|--------|---------|-------------|
| `it.next()` | `Iter::Next(v)` / `Iter::Done` | Advance by one. Cloning an iterator shares its cursor (like Python / Rust / JS) — two names pointing at the same iterator advance together. |
| `it.iter()` | iter | Returns the same iterator. Makes `for x in it` work whether `it` is already an iterator or a fresh iterable. |

```bop
let it = [10, 20, 30].iter()
print(it.type())                // "iter"
print(it.next())                // Iter::Next(10)
print(it.next())                // Iter::Next(20)

for x in it { print(x) }        // 30  (picks up from the current cursor)
```

### User-defined iterables

A struct can participate in the iterator protocol by implementing `.iter()` (and, if it's its own iterator, `.next()`):

```bop
struct Bag { items }
fn bag_of(arr) { return Bag { items: arr } }
fn Bag.iter(self) { return self.items.iter() }   // delegate to the backing array

let b = bag_of(["x", "y", "z"])
for v in b { print(v) }                           // x  y  z
```

That's the minimal shape. A container that wraps an array and delegates `.iter()` is the 80% case. User types with genuine internal state (like a lazy counter) work the same way — define `fn Counter.iter(self)` to return an iterator (either the backing data's iterator, or `self` if `self` also has `.next()`).

### The `Iter` enum

`.next()` returns one of two variants of the built-in `Iter` enum — always in scope, no `use` required:

```
enum Iter {
  Next(value),
  Done,
}
```

Pattern-match directly:

```bop
let it = [1, 2].iter()
let r = it.next()
print(match r {
  Iter::Next(v) => "got: " + v.to_str(),
  Iter::Done    => "exhausted",
})
// got: 1
```

## Struct / enum methods

User-declared. See [Structs & Enums](../data/structs-and-enums.md). Method dispatch on a struct tries the universal common methods first, then looks up `fn TypeName.method` declared in the same module.

## Module methods

If you `use path as m`, `m` is a `Value::Module`. `m.type()` → `"module"`, `m.inspect()` → `"<module path>"`. Otherwise `.` on a module accesses its exports:

```bop
use std.math as m
print(m.PI)             // exported constant
print(m.type())         // "module"   (universal method, not an export)
```
