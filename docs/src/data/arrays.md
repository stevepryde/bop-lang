# Arrays

Arrays are ordered, mutable collections that can hold any mix of types.

## Creating arrays

```bop
let items = [1, 2, 3]
let empty = []
let mixed = [1, "two", true, none]
```

## Accessing elements

Arrays are 0-indexed. Negative indices count from the end:

```bop
let items = [10, 20, 30]
print(items[0])     // 10
print(items[2])     // 30
print(items[-1])    // 30 (last element)
print(items[-2])    // 20
```

Out-of-bounds access produces an error.

## Modifying elements

```bop
let items = [10, 20, 30]
items[0] = 99
print(items)    // [99, 20, 30]
```

## Methods

| Method | Returns | Description |
|--------|---------|-------------|
| `arr.len()` | int | Number of elements |
| `arr.push(val)` | none | Append to end |
| `arr.pop()` | value | Remove and return last element |
| `arr.has(val)` | bool | Whether the array contains the value |
| `arr.index_of(val)` | int | Index of first occurrence, or `-1` |
| `arr.insert(i, val)` | none | Insert at index, shifting elements right |
| `arr.remove(i)` | value | Remove at index, shifting elements left |
| `arr.slice(start, end)` | array | New sub-array |
| `arr.reverse()` | none | Reverse in place |
| `arr.sort()` | none | Sort in place |
| `arr.join(sep)` | string | Join elements into a string |

Plus the universal `arr.type()`, `arr.to_str()`, `arr.inspect()`.

## Practical examples

### Building a list

```bop
let squares = []
for i in range(1, 6) {
  squares.push(i * i)
}
print(squares)    // [1, 4, 9, 16, 25]
```

### Filtering values

```bop
let numbers = [3, 7, 1, 9, 4, 6, 2, 8]
let big = []
for n in numbers {
  if n > 5 {
    big.push(n)
  }
}
print(big)    // [7, 9, 6, 8]
```

### Checking membership

```bop
let allowed = ["admin", "editor", "viewer"]
let role = "editor"
if allowed.has(role) {
  print("Access granted")
} else {
  print("Access denied")
}
```

### Sorting and joining

```bop
let scores = [42, 17, 85, 3]
scores.sort()
print(scores)    // [3, 17, 42, 85]

let names = ["Charlie", "Alice", "Bob"]
names.sort()
print(names.join(", "))    // "Alice, Bob, Charlie"
```
