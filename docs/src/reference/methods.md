# Methods

Bop dispatches methods with `.name(args...)`. Every built-in method on primitives, arrays, strings, and dicts is listed here. User-defined methods on structs use the same syntax — see [Structs & Enums](../data/structs-and-enums.md) for the `fn Type.method(self, ...)` form.

## Methods on every value

Three methods work on any value — introspection + stringification. They're dispatched before the type-specific tables, so they're always available:

| Method | Returns | Notes |
|--------|---------|-------|
| `x.type()` | string | One of `"int"`, `"number"`, `"string"`, `"bool"`, `"none"`, `"array"`, `"dict"`, `"fn"`, `"struct"`, `"enum"`, `"module"` |
| `x.to_str()` | string | Display repr — same as what `print(x)` would emit for a single arg |
| `x.inspect()` | string | Debug repr — strings are wrapped in `"..."`, nested strings stay quoted inside arrays / dicts |

```bop
print((42).type())                 // "int"
print("hi".to_str())               // "hi"
print("hi".inspect())              // "hi"   (quoted)
print([1, "two"].inspect())        // [1, "two"]
```

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

## Dict methods — `dict`

See [Dictionaries](../data/dictionaries.md) for worked examples.

| Method | Returns | Description |
|--------|---------|-------------|
| `d.len()` | int | Number of entries. |
| `d.keys()` | array | All keys as strings. |
| `d.values()` | array | All values. |
| `d.has(key)` | bool | Whether `key` exists. |

## Struct / enum methods

User-declared. See [Structs & Enums](../data/structs-and-enums.md). Method dispatch on a struct tries the universal common methods first, then looks up `fn TypeName.method` declared in the same module.

## Module methods

If you `use path as m`, `m` is a `Value::Module`. `m.type()` → `"module"`, `m.inspect()` → `"<module path>"`. Otherwise `.` on a module accesses its exports:

```bop
use std.math as m
print(m.pi)             // exported constant
print(m.type())         // "module"   (universal method, not an export)
```
