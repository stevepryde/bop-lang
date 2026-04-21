# Types

Bop is dynamically typed — variables can hold any type, and types are checked at runtime. The `type()` builtin returns a name for each:

| `type()` returns | Description | Literal examples |
|------------------|-------------|------------------|
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
print(type(x))      # "int"

let y = 3.14
print(type(y))      # "number"

let s = "hello"
print(type(s))      # "string"
```

## Integers and floats

Integer literals (`42`, `-7`) produce `int` values; anything with a decimal point or `e`-exponent (`3.14`, `4.0`, `1e6`) produces `number`. The two coexist — arithmetic between them widens to `number`, and `==` compares numerically across the split:

```bop
print(1 + 1)         # 2       (int + int → int)
print(1 + 1.0)       # 2       (int + number → number, prints as whole)
print(1 == 1.0)      # true    (cross-type numeric equality)
```

Use `int(x)` to truncate a number to an integer (toward zero), and `float(x)` to widen an int to a number:

```bop
print(int(3.7))      # 3
print(int(-2.7))     # -2
print(float(5))      # 5       (now a number internally)
```

### Division

Bop has two division operators:

- `/` always produces a `number`. `7 / 2` → `3.5`, `6 / 2` → `3` (whole, but still a number type).
- `//` is **integer division** — truncates toward zero and always returns an `int`. `7 // 2` → `3`, `-7 // 2` → `-3`.

```bop
print(7 / 2)         # 3.5
print(7 // 2)        # 3
print(type(7 // 2))  # "int"
```

Both raise a runtime error on division by zero.

## Strings

Strings use double quotes only. Supported escape sequences: `\"`, `\\`, `\n`, `\t`, `\r`, `\{`, `\}`.

```bop
let greeting = "Hello, world!"
let with_newline = "Line 1\nLine 2"
```

Strings are indexable and iterable, but immutable — you can read characters but not change them in place:

```bop
let s = "hello"
print(s[0])          # "h"
print(s[-1])         # "o"

for ch in s {
  print(ch)          # "h", "e", "l", "l", "o"
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

# Or use concatenation:
print("Double: " + str(count * 2))
```

To include a literal `{` or `}` in a string, escape it with a backslash:

```bop
print("Use \{name\} for interpolation")
# prints: Use {name} for interpolation
```

### String concatenation

`+` joins two strings, or a string and a number (the number is converted first):

```bop
print("Score: " + str(42))    # "Score: 42"
print("n=" + 7)                # "n=7"   (int auto-stringified)
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
print(missing)    # none
```

`none` is falsy in conditions; every other value except `false` is truthy.

## User-defined types

Bop also lets you declare your own struct and enum types with methods — see [Structs & Enums](../data/structs-and-enums.md).
