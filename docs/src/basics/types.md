# Types

Bop is dynamically typed — variables can hold any type, and types are checked at runtime. Every value has a `.type()` method that returns its type name:

| `.type()` returns | Description | Literal examples |
|-------------------|-------------|------------------|
| `"int"` | 64-bit signed integer | `0`, `42`, `-7` |
| `"number"` | 64-bit floating point | `3.14`, `-0.5`, `4.0` |
| `"string"` | UTF-8 text | `"hello"`, `"got {n} items"` |
| `"bool"` | Boolean | `true`, `false` |
| `"none"` | Absence of a value | `none` |
| `"array"` | Ordered, mutable collection | `[1, 2, 3]`, `[]` |
| `"dict"` | String-keyed map | `{"x": 10, "name": "Alice"}` |
| `"fn"` | First-class function / closure | `fn(x) { return x + 1 }` |
| `"struct"` | User-defined struct instance | `Point { x: 3, y: 4 }` |
| `"enum"` | User-defined enum variant | `Color::Red` |
| `"module"` | Aliased module namespace | result of `use foo as m` |

```bop
let x = 42
print(x.type())      // "int"

let y = 3.14
print(y.type())      // "number"

let s = "hello"
print(s.type())      // "string"
```

## Integers and floats

Integer literals (`42`, `-7`) produce `int` values; anything with a decimal point (`3.14`, `4.0`) produces `number`. There's no exponent-shaped literal — write the full decimal or build large values by multiplication. The two numeric types coexist: arithmetic widens to `number` on mixed operands, and `==` compares numerically across the split:

```bop
print(1 + 1)         // 2       (int + int → int)
print(1 + 1.0)       // 2       (int + number → number, prints as whole)
print(1 == 1.0)      // true    (cross-type numeric equality)
```

Use `.to_int()` to truncate a number to an integer (toward zero), and `.to_float()` to widen an int to a number:

```bop
print((3.7).to_int())      // 3
print((-2.7).to_int())     // -2
print((5).to_float())      // 5       (now a number internally)
```

Number literals need parens before a method call (otherwise `3.7.to_int()` looks like a decimal followed by a field). Variables don't: `let x = 3.7; print(x.to_int())`.

### Division

`/` always produces a `number`, even for `int / int`:

```bop
print(7 / 2)              // 3.5
print(6 / 2)              // 3        (whole value — still a number)
print((6 / 2).type())     // "number"
```

This sidesteps the classic "1 / 2 == 0" footgun that trips beginners in C / Rust / Java.

When you *do* want an integer result (index math, bucketing, etc.), coerce the quotient back with `.to_int()`:

```bop
print((7 / 2).to_int())           // 3
print((-7 / 2).to_int())          // -3
print((7 / 2).to_int().type())    // "int"
```

`.to_int()` truncates toward zero. `/` raises a runtime error on division by zero.

## Strings

Strings use double quotes only. Supported escape sequences: `\"`, `\\`, `\n`, `\t`, `\r`, `\{`, `\}`.

```bop
let greeting = "Hello, world!"
let with_newline = "Line 1\nLine 2"
```

Strings are indexable and iterable, but immutable — you can read characters but not change them in place:

```bop
let s = "hello"
print(s[0])          // "h"
print(s[-1])         // "o"

for ch in s {
  print(ch)          // "h", "e", "l", "l", "o"
}
```

### String interpolation

Use `{variable}` inside a string to insert a variable's value. Only variable names are allowed inside `{}` — not expressions:

```bop
let name = "Alice"
let count = 5
print("Hello, {name}! You have {count} items.")
```

For computed values, store the result in a variable first:

```bop
let doubled = count * 2
print("Double: {doubled}")

// Or use concatenation:
print("Double: " + (count * 2).to_str())
```

To include a literal `{` or `}` in a string, escape it with a backslash:

```bop
print("Use \{name\} for interpolation")
// prints: Use {name} for interpolation
```

### String concatenation

`+` joins two strings, or a string and a number (the number is converted first):

```bop
print("Score: " + (42).to_str())    // "Score: 42"
print("n=" + 7)                      // "n=7"   (int auto-stringified)
```

## Booleans

`true` and `false`. Used in conditions and comparisons:

```bop
let found = true

if found {
  print("Got it!")
}
```

## None

`none` represents the absence of a value. Functions that don't explicitly return a value return `none`. It's also what you get when looking up a missing dictionary key:

```bop
let stats = {"hp": 10}
let missing = stats["armor"]
print(missing)    // none
```

`none` is falsy in conditions; every other value except `false` is truthy.

## User-defined types

Bop also lets you declare your own struct and enum types with methods — see [Structs & Enums](../data/structs-and-enums.md).
