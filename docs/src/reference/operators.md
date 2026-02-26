# Operators

All Bop operators, grouped by category. No operator overloading — each operator works on specific types and produces an error on type mismatch.

## Arithmetic

| Operator | Name | Types | Notes |
|----------|------|-------|-------|
| `+` | Add | number | Also concatenates strings: `"a" + "b"` → `"ab"` |
| `-` | Subtract | number | Also unary negation: `-x` |
| `*` | Multiply | number | |
| `/` | Divide | number | Always produces float: `7 / 2` → `3.5` |
| `%` | Modulo | number | |

```bop
print(10 + 3)      // 13
print(10 - 3)      // 7
print(10 * 3)      // 30
print(10 / 3)      // 3.333...
print(10 % 3)      // 1

print("Hello" + " " + "world")    // "Hello world"
```

## Comparison

| Operator | Name | Returns |
|----------|------|---------|
| `==` | Equal | bool |
| `!=` | Not equal | bool |
| `<` | Less than | bool |
| `>` | Greater than | bool |
| `<=` | Less or equal | bool |
| `>=` | Greater or equal | bool |

`==` and `!=` work on all types. Comparing different types is always `false` (no implicit coercion). `<`, `>`, `<=`, `>=` work on numbers and strings (lexicographic).

```bop
print(5 == 5)         // true
print(5 == "5")       // false (different types)
print("abc" < "def")  // true (lexicographic)
```

## Boolean

| Operator | Name | Notes |
|----------|------|-------|
| `&&` | And | Short-circuits |
| `\|\|` | Or | Short-circuits |
| `!` | Not | Unary prefix |

Short-circuiting means the second operand isn't evaluated if the first determines the result:

```bop
// && stops at the first false
if x > 0 && x < 100 {
  print("In range")
}

// || stops at the first true
if name == "" || name == none {
  print("No name provided")
}

// ! inverts a boolean
if !found {
  print("Still searching...")
}
```

## Assignment

| Operator | Equivalent to |
|----------|--------------|
| `=` | Assign |
| `+=` | `x = x + ...` |
| `-=` | `x = x - ...` |
| `*=` | `x = x * ...` |
| `/=` | `x = x / ...` |
| `%=` | `x = x % ...` |

```bop
let score = 0
score += 10    // 10
score -= 3     // 7
score *= 2     // 14
```

## Conditional expressions

Bop has no ternary operator (`?:`). Use `if/else` as an expression instead:

```bop
let label = if count > 3 { "lots" } else { "few" }
```

Both branches are required when `if/else` is used as an expression. The last expression in each branch is the value.

## Precedence

From highest (evaluated first) to lowest:

| Priority | Operators |
|----------|-----------|
| 1 | `!`, `-` (unary) |
| 2 | `*`, `/`, `%` |
| 3 | `+`, `-` |
| 4 | `<`, `>`, `<=`, `>=` |
| 5 | `==`, `!=` |
| 6 | `&&` |
| 7 | `\|\|` |
| 8 | `=`, `+=`, `-=`, `*=`, `/=`, `%=` |

Parentheses override precedence:

```bop
let result = (1 + 2) * 3    // 9, not 7
```
