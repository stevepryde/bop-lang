# Types

Bop is dynamically typed — variables can hold any type, and types are checked at runtime. There are six types:

| Type | Literals | Examples |
|------|----------|---------|
| **number** | Digits, optional decimal | `0`, `42`, `-7`, `3.14` |
| **string** | Double-quoted | `"hello"`, `"got {n} items"` |
| **bool** | Keywords | `true`, `false` |
| **none** | Keyword | `none` |
| **array** | Square brackets | `[1, 2, 3]`, `[]` |
| **dict** | Curly braces with colons | `{"x": 10, "name": "Alice"}` |

You can check a value's type at runtime with the `type()` function:

```bop
let x = 42
print(type(x))    // "number"

let s = "hello"
print(type(s))    // "string"
```

## Numbers

Bop has a single `number` type (64-bit floating point internally). Whole numbers display without a decimal point: `5` not `5.0`.

```bop
let score = 100
let pi = 3.14
let negative = -7
```

Division always produces a float result:

```bop
print(7 / 2)     // 3.5
print(6 / 2)     // 3
```

Use `int()` to truncate the decimal part:

```bop
print(int(7 / 2))   // 3
print(int(3.9))      // 3
```

## Strings

Strings use double quotes only. Supported escape sequences: `\"`, `\\`, `\n`, `\t`, `\{`, `\}`.

```bop
let greeting = "Hello, world!"
let with_newline = "Line 1\nLine 2"
```

Strings are indexable and iterable, but immutable — you can read characters but not change them in place:

```bop
let s = "hello"
print(s[0])     // "h"
print(s[-1])    // "o"

for ch in s {
  print(ch)     // "h", "e", "l", "l", "o"
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
let doubled = str(count * 2)
print("Double: {doubled}")

// Or use concatenation:
print("Double: " + str(count * 2))
```

To include a literal `{` or `}` in a string, escape it with a backslash:

```bop
print("Use \{name\} for interpolation")
// prints: Use {name} for interpolation
```

## Booleans

`true` and `false`. Used in conditions and comparisons:

```bop
let found = true
let empty = false

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
