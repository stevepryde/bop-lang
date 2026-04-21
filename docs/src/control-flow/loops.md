# Loops

Loops let you repeat actions. Bop has three kinds: `repeat`, `while`, and `for...in`.

## repeat

The simplest loop — just "do this N times." No loop variable, no fuss:

```bop
repeat 4 {
  print("Hello!")
}
```

The count can be any expression:

```bop
let times = 3
repeat times {
  print("Again!")
}
```

`repeat` is perfect when you just need repetition without tracking a counter.

## while

Loops as long as a condition is true:

```bop
let n = 1
while n <= 100 {
  n *= 2
}
print(n)    // 128
```

A `while true` loop runs forever (until you `break` out of it or hit the step limit):

```bop
let total = 0
let i = 1
while true {
  total += i
  if total > 100 {
    break
  }
  i += 1
}
print("Sum exceeded 100 at i=" + i.to_str())
```

### Counting example

```bop
// Count how many numbers under 50 are divisible by 7
let count = 0
let n = 1
while n < 50 {
  if n % 7 == 0 {
    count += 1
  }
  n += 1
}
print("Found " + count.to_str())
```

## for...in

Iterates over ranges, arrays, or dictionary keys.

### Ranges

```bop
for i in range(5) {
  print(i.to_str())     // 0, 1, 2, 3, 4
}
```

With a start value:

```bop
for i in range(2, 8) {
  print(i.to_str())     // 2, 3, 4, 5, 6, 7
}
```

### Arrays

```bop
let fruits = ["apple", "banana", "cherry"]
for fruit in fruits {
  print(fruit)
}
```

### Dictionary keys

```bop
let scores = {"Alice": 95, "Bob": 87, "Charlie": 92}
for name in scores {
  let s = scores[name].to_str()
  print(name + ": " + s)
}
```

### Iterators and user-defined containers

`for x in v` works on anything that participates in the [iterator protocol](../reference/methods.md#iter-methods--iter): arrays, strings, dicts, explicit iterators (`arr.iter()`), and user types that implement `.iter()`:

```bop
struct Bag { items }
fn bag_of(arr) { return Bag { items: arr } }
fn Bag.iter(self) { return self.items.iter() }

let b = bag_of([10, 20, 30])
for v in b { print(v) }         // 10  20  30
```

That's the structural-typing story: no trait declaration, no ceremony — if `v.iter()` returns something iterable, `for x in v` just works.

### Strings

You can iterate over the characters of a string:

```bop
let word = "hello"
for ch in word {
  print(ch)    // "h", "e", "l", "l", "o"
}
```

## Nesting loops

Loops can be nested. This is useful for working with grids or combinations:

```bop
for row in range(3) {
  for col in range(4) {
    print("(" + row.to_str() + ", " + col.to_str() + ")")
  }
}
```
