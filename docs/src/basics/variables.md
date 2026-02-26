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

## Reassignment

After declaration, reassign with just `=`:

```bop
let score = 0
score = 10
score += 5     // score is now 15
```

Compound assignment operators work too: `+=`, `-=`, `*=`, `/=`, `%=`.

```bop
let x = 10
x += 3    // x = x + 3 → 13
x -= 1    // x = x - 1 → 12
x *= 2    // x = x * 2 → 24
```

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

Every value in Bop is a **copy**. When you assign a variable, pass an argument to a function, or return a value, you get an independent copy — even for arrays and dicts. Changing the copy never affects the original.

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

This works the same way for all types — numbers, strings, bools, arrays, and dicts are all copied. There are no references or shared mutable state in Bop.

## Dynamic typing

Variables can hold any type. You can even change the type of a variable by reassigning it:

```bop
let val = 42
print(type(val))    // "number"

val = "hello"
print(type(val))    // "string"
```

This flexibility is useful, but can be surprising if you're not careful.
