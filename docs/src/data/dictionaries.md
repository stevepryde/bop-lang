# Dictionaries

Dictionaries (dicts) are key-value stores. Keys are always strings; values can be any type.

## Creating dictionaries

```bop
let person = {"name": "Alice", "age": 30, "active": true}
let empty = {}
```

## Accessing values

Use bracket notation with a string key:

```bop
let name = person["name"]     // "Alice"
let age = person["age"]       // 30
```

Accessing a missing key returns `none` (no error):

```bop
let email = person["email"]
print(email)    // none
```

## Modifying values

```bop
person["age"] = 31             // update existing key
person["email"] = "a@b.com"   // add new entry
```

## Methods

| Method | Returns | Description |
|--------|---------|-------------|
| `d.len()` | number | Number of entries |
| `d.keys()` | array | Array of all keys |
| `d.values()` | array | Array of all values |
| `d.has(key)` | bool | Whether the key exists |

## Practical examples

### Counting occurrences

```bop
let words = ["apple", "banana", "apple", "cherry", "banana", "apple"]
let counts = {}
for word in words {
  if counts.has(word) {
    counts[word] += 1
  } else {
    counts[word] = 1
  }
}

for key in counts {
  print(key + ": " + str(counts[key]))
}
```

### Storing structured data

```bop
let point = {"x": 10, "y": 20}
let x = str(point["x"])
let y = str(point["y"])
print("Position: ({x}, {y})")
```

### Iterating over entries

```bop
let config = {"width": 800, "height": 600, "title": "My App"}
for key in config {
  let val = str(config[key])
  print(key + ": " + val)
}
```

### Checking for a key before using it

```bop
let settings = {"volume": 80}

if settings.has("volume") {
  let v = str(settings["volume"])
  print("Volume is {v}")
} else {
  print("Using default volume")
}
```
