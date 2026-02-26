# if / else

The `if` statement lets your program make decisions based on conditions.

## Basic if

```bop
if count > 3 {
  print("That's a lot!")
}
```

The condition does **not** need parentheses, but they're allowed: `if (x > 3) { ... }`. Braces are always required.

## if / else

```bop
if temperature > 30 {
  print("It's hot!")
} else {
  print("Not too bad.")
}
```

## if / else if / else

Chain multiple conditions with `else if`:

```bop
if score > 90 {
  print("Excellent!")
} else if score > 70 {
  print("Good job!")
} else if score > 50 {
  print("Not bad!")
} else {
  print("Keep trying!")
}
```

Only the first matching branch runs. If none match and there's an `else`, that branch runs.

## if as an expression

`if/else` can produce a value when used in expression position (e.g., after `=`):

```bop
let label = if count > 3 { "lots" } else { "few" }
print("You have {label} of items")
```

When used as an expression, both `if` and `else` branches are required. The last expression in each branch is the value.

```bop
let message = if x > 0 {
  "positive"
} else {
  "non-positive"
}
print(message)
```

## Common patterns

### Guard clause

```bop
fn process(value) {
  if value == none {
    return
  }
  print("Processing: " + str(value))
}
```

### Classify a value

```bop
fn classify(n) {
  if n > 0 {
    return "positive"
  } else if n < 0 {
    return "negative"
  } else {
    return "zero"
  }
}
```

### Combine conditions with `&&` and `||`

```bop
if age >= 18 && has_ticket {
  print("Welcome in!")
}

if x < 0 || x > 100 {
  print("Out of range!")
}
```
