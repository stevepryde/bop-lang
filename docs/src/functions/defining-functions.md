# Defining Functions

Functions let you name a sequence of actions and reuse it. Bop has first-class functions — you can store them in variables, pass them to other functions, return them from other functions, and stash them in arrays.

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

let result = double(5)
print(result)    // 10
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
print(result)    // 15
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
print("Sum of squares: {result}")    // Sum of squares: 55
```

## Recursion

Functions can call themselves. Bop caps recursion depth to prevent runaway stacks (also bounded by the step limit):

```bop
fn factorial(n) {
  if n <= 1 {
    return 1
  }
  return n * factorial(n - 1)
}

print(factorial(5))    // 120
```

## First-class functions

A named `fn` is a value just like anything else — you can assign it to a variable, pass it as an argument, return it from another function, or store it in a collection:

```bop
fn double(x) { return x * 2 }
let f = double
print(f(7))          // 14

fn apply(f, x) { return f(x) }
print(apply(double, 21))    // 42
```

### Function expressions (lambdas)

`fn(...) { ... }` — without a name — is an expression that produces a function value. Use it when you want a one-off function inline:

```bop
let square = fn(x) { return x * x }
print(square(6))     // 36

let mul = fn(a, b) { return a * b }
print([mul(2, 3), mul(4, 5)])    // [6, 20]
```

### Closures

Function expressions capture variables from the enclosing scope. The capture is a **snapshot** taken at the moment the closure is built — mutating the outer variable afterwards doesn't change what the closure sees:

```bop
let n = 5
let add_n = fn(x) { return x + n }
n = 100
print(add_n(3))      // 8, not 103
```

The classic "factory returning a specialised function" pattern works:

```bop
fn make_adder(n) {
  return fn(x) { return x + n }
}

let add5 = make_adder(5)
let add10 = make_adder(10)
print(add5(3))       // 8
print(add10(3))      // 13
```

### Recursion in lambdas

An anonymous `fn(...)` can't see itself by name. If a lambda needs to recurse, assign it to a named `fn` instead — named fns are visible inside their own body:

```bop
fn fib(n) {
  if n < 2 { return n }
  return fib(n - 1) + fib(n - 2)
}
print(fib(10))       // 55
```
