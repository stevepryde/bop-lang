# Strings

Strings are immutable sequences of characters. All string methods return new strings — the original is never modified.

## Creating strings

```bop
let s = "hello world"
let empty = ""
let escaped = "Line 1\nLine 2"
```

Supported escape sequences: `\"`, `\\`, `\n`, `\t`, `\{`, `\}`.

## Indexing

```bop
let s = "hello"
print(s[0])      // "h"
print(s[-1])     // "o"
```

Each index returns a single-character string (there's no separate character type).

## String interpolation

Insert variable values with `{name}` inside a string:

```bop
let name = "Alice"
let count = 5
print("Hello, {name}! You have {count} items.")
```

Only variable names are allowed inside `{}`. For expressions, use a temporary variable:

```bop
let total = str(count * 2)
print("Double: {total}")
```

Or use concatenation:

```bop
print("Double: " + str(count * 2))
```

To include a literal `{` or `}` in a string, escape it with `\{` and `\}`:

```bop
print("Use \{name\} for interpolation")
// prints: Use {name} for interpolation
```

## Concatenation

Use `+` to join strings:

```bop
let full = "Hello" + ", " + "world!"
print(full)    // "Hello, world!"
```

Numbers must be converted with `str()` first:

```bop
let msg = "Score: " + str(42)
```

## Methods

| Method | Returns | Description |
|--------|---------|-------------|
| `s.len()` | number | Number of characters |
| `s.contains(sub)` | bool | Whether the string contains `sub` |
| `s.starts_with(prefix)` | bool | Whether it starts with `prefix` |
| `s.ends_with(suffix)` | bool | Whether it ends with `suffix` |
| `s.index_of(sub)` | number or none | Index of first occurrence, or `none` |
| `s.split(sep)` | array | Split into array of strings on `sep` |
| `s.replace(old, new)` | string | Replace all occurrences |
| `s.upper()` | string | Uppercase copy |
| `s.lower()` | string | Lowercase copy |
| `s.trim()` | string | Copy with leading/trailing whitespace removed |
| `s.slice(start, end)` | string | Substring (both args optional) |

## Practical examples

### Parsing CSV data

```bop
let input = "Alice,95,A"
let parts = input.split(",")
print(parts[0])    // "Alice"
print(parts[1])    // "95"
```

### Checking prefixes

```bop
let filename = "report.csv"
if filename.ends_with(".csv") {
  print("CSV file detected")
}
```

### Building a formatted string

```bop
let items = ["apple", "banana", "cherry"]
let count = str(items.len())
let list = items.join(", ")
print("Found {count} items: {list}")
```
