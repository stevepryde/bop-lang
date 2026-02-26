# Defining Functions

Functions let you name a sequence of actions and reuse it. This is where Bop starts to feel like real programming.

## Declaring a function

```bop
fn greet() {
  print("Hello!")
}

greet()    // "Hello!"
greet()    // "Hello!"
```

## Parameters

Functions can take parameters — values you pass in when calling:

```bop
fn repeat_string(text, times) {
  let result = ""
  repeat times {
    result += text
  }
  return result
}

print(repeat_string("ha", 3))    // "hahaha"
```

Parameters are positional. There are no default values or type annotations.

## Return values

Use `return` to send a value back from the function:

```bop
fn double(x) {
  return x * 2
}

let result = double(5)    // 10
print(str(result))
```

`return` with no value (or reaching the end of the function) returns `none`:

```bop
fn do_something() {
  print("Working...")
  // no return — returns none
}

let result = do_something()
print(result)    // none
```

## Early return

`return` exits the function immediately, even from inside loops or conditionals:

```bop
fn find_first_big(numbers, threshold) {
  for n in numbers {
    if n > threshold {
      return n
    }
  }
  return none
}

let result = find_first_big([3, 7, 1, 15, 4], 10)
print(str(result))    // 15
```

## Calling functions

Parentheses are always required, even with no arguments:

```bop
greet()          // correct
// greet         // error — 'greet' is a function, call it with greet()
```

## Practical example: sum of squares

```bop
fn sum_of_squares(n) {
  let total = 0
  for i in range(1, n + 1) {
    total += i * i
  }
  return total
}

let result = sum_of_squares(5)
print("Sum of squares: " + str(result))    // 55
```

## Recursion

Functions can call themselves. Recursion is capped by the step limit to prevent infinite recursion:

```bop
fn factorial(n) {
  if n <= 1 {
    return 1
  }
  return n * factorial(n - 1)
}

print(str(factorial(5)))    // 120
```

> **Note:** Functions are named declarations only. You can't assign a function to a variable or pass one as an argument (yet). First-class functions may be added in a future version.
