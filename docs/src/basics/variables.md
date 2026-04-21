# Variables

Variables store values that you can use and change throughout your program.

## Declaring variables

Use `let` to create a new variable:

```bop
let x = 5
let name = "Alice"
let found = true
let items = [1, 2, 3]
let config = {"width": 10, "height": 5}
```

`let` is required the first time — using an undeclared variable is an error. This catches typos early:

```bop
let count = 5
conut = 10    // Error: I don't know what 'conut' is — did you mean 'count'?
```

## Constants

Use `const` for values that won't change:

```bop
const PI = 3.14
const MAX_SIZE = 100
```

Reassigning a constant is rejected at parse time — the compiler sees that the left-hand side is an all-caps identifier and refuses:

```bop
const MAX_SIZE = 100
MAX_SIZE = 200      // Error: can't reassign a constant
```

Constants must be **all-caps** (with digits / underscores allowed). `const Pi = 3.14` is rejected: the parser will suggest `const PI = 3.14` instead.

## Reassignment

After declaration, reassign with just `=`:

```bop
let score = 0
score = 10
score += 5     // score is now 15
```

Compound assignment operators: `+=`, `-=`, `*=`, `/=`, `%=`.

```bop
let x = 10
x += 3    // x = x + 3 → 13
x -= 1    // x = x - 1 → 12
x *= 2    // x = x * 2 → 24
```

## Name shapes are checked

Bop enforces case conventions at declaration sites so intent is visible at a glance:

| Declaration | Required shape |
|-------------|----------------|
| `let x`, `fn foo(param)`, struct fields, match bindings, `for` variables, aliases | starts with lowercase or `_` |
| `const FOO` | all caps (+ digits / `_`) |
| `struct Point`, `enum Shape`, enum variants | starts with uppercase |

Mis-shaped declarations parse-error with a suggestion:

```bop
let Count = 5        // Error: names bound by `let` start with a lowercase letter. Try `count`?
const pi = 3.14      // Error: `const` names are SCREAMING_SNAKE_CASE. Try `PI`?
struct point {}      // Error: type names start with an uppercase letter. Try `Point`?
```

A leading underscore marks a name as "private by convention." It doesn't change the shape check — `_count` is still a lowercase-starting name — but glob `use` imports skip names that start with `_` (see [Modules](../modules.md)).

## Block scoping

Variables are block-scoped — a variable declared inside `{ }` is not visible outside:

```bop
let x = 1
if true {
  let y = 2       // y only exists inside this block
  print(y)        // 2
}
// print(y)       // Error: I don't know what 'y' is
```

## Shadowing

You can re-declare a variable with `let` in an inner block. The inner variable "shadows" the outer one:

```bop
let x = 1
if true {
  let x = 2     // shadows outer x
  x = 3         // reassigns inner x
  print(x)      // 3
}
print(x)         // 1 — outer x is unchanged
```

## Copying and passing values

Every value in Bop is a **copy**. When you assign a variable, pass an argument to a function, or return a value, you get an independent copy — even for arrays, dicts, and structs. Changing the copy never affects the original.

### Assignment copies

```bop
let a = [1, 2, 3]
let b = a           // b is a separate copy
b.push(4)
print(a)            // [1, 2, 3] — unchanged
print(b)            // [1, 2, 3, 4]
```

### Function arguments are copies

When you pass a value to a function, the function gets its own copy. Modifying it inside the function has no effect on the caller's variable:

```bop
fn try_to_modify(items) {
  items.push(99)
  print(items)       // [1, 2, 3, 99]
}

let original = [1, 2, 3]
try_to_modify(original)
print(original)      // [1, 2, 3] — unchanged
```

To get a modified value out of a function, `return` it:

```bop
fn add_item(items, val) {
  items.push(val)
  return items
}

let original = [1, 2, 3]
original = add_item(original, 99)
print(original)      // [1, 2, 3, 99]
```

This applies to every value type — numbers, strings, bools, arrays, dicts, structs, enum variants. Closures (`Value::Fn`) and modules (`Value::Module`) are reference-counted, so passing one of those around is cheap and the shared state is visible from both handles.

## Dynamic typing

Variables can hold any type. You can even change the type of a variable by reassigning it:

```bop
let val = 42
print(type(val))    // "int"

val = "hello"
print(type(val))    // "string"
```

This flexibility is useful but can be surprising — the error only surfaces when some later operation expects the original type. Case conventions help: a `count`-like variable holding a string usually means the wrong thing landed in it upstream.
