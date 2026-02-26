# Built-in Functions

Bop provides 11 built-in functions available in every program. These cannot be shadowed by user-defined functions.

## Output

### `print(args...)`

Prints values to output, separated by spaces. Returns `none`.

```bop
print("hello")           // hello
print("x =", 42)         // x = 42
print(1, "plus", 2)      // 1 plus 2
```

`print` accepts any number of arguments (including zero). Each argument is converted to its string representation automatically.

### `inspect(value)`

Returns a debug representation of a value as a string. Strings are wrapped in quotes; other types look the same as `str()`.

```bop
print(inspect("hello"))    // "hello"
print(inspect(42))         // 42
print(inspect([1, 2]))     // [1, 2]
```

Useful for debugging when you need to distinguish strings from other types.

## Type Conversion

### `str(value)`

Converts any value to its string representation.

```bop
str(42)        // "42"
str(3.14)      // "3.14"
str(true)      // "true"
str(none)      // "none"
str([1, 2])    // "[1, 2]"
```

### `int(value)`

Truncates a number to an integer, or parses a string as a number and truncates.

```bop
int(3.9)       // 3
int(-2.7)      // -2
int("42")      // 42
int(true)      // 1
int(false)     // 0
```

Produces an error if the value can't be converted (e.g., `int("hello")`).

### `type(value)`

Returns the type name of a value as a string.

```bop
type(42)           // "number"
type("hello")      // "string"
type(true)         // "bool"
type(none)         // "none"
type([1, 2])       // "array"
type({"a": 1})     // "dict"
```

## Math

### `abs(x)`

Returns the absolute value of a number.

```bop
abs(-5)     // 5
abs(3)      // 3
abs(-2.7)   // 2.7
```

### `min(a, b)`

Returns the smaller of two numbers.

```bop
min(3, 7)      // 3
min(-1, -5)    // -5
```

### `max(a, b)`

Returns the larger of two numbers.

```bop
max(3, 7)      // 7
max(-1, -5)    // -1
```

### `rand(n)`

Returns a random integer from 0 to n-1 (inclusive). The argument must be a positive integer.

```bop
rand(6)     // 0, 1, 2, 3, 4, or 5
rand(2)     // 0 or 1 (coin flip)
rand(100)   // 0 to 99
```

> **Note:** `rand` uses a deterministic pseudo-random number generator. The same seed produces the same sequence of values.

## Collections

### `len(value)`

Returns the length of a string, array, or dictionary.

```bop
len("hello")       // 5
len([1, 2, 3])     // 3
len({"a": 1})      // 1
len("")            // 0
len([])            // 0
```

Produces an error for numbers, bools, and `none`.

## Ranges

### `range(n)` / `range(start, end)` / `range(start, end, step)`

Generates an array of numbers.

```bop
range(5)           // [0, 1, 2, 3, 4]
range(2, 6)        // [2, 3, 4, 5]
range(0, 10, 2)    // [0, 2, 4, 6, 8]
range(5, 0)        // [5, 4, 3, 2, 1]  (auto-detects direction)
range(10, 0, -3)   // [10, 7, 4, 1]
```

- With 1 argument: `range(n)` produces `[0, 1, ..., n-1]`
- With 2 arguments: `range(start, end)` auto-detects direction
- With 3 arguments: `range(start, end, step)` uses the given step (error if step is 0)
- Maximum 10,000 elements
