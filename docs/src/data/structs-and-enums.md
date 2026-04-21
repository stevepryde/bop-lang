# Structs & Enums

Bop supports user-defined **struct** and **enum** types, with methods attached via `fn Type.method(self, ...)`. They give you type names in error messages, structural pattern matching, and an identity-aware equality.

## Structs

A `struct` is a named record — a set of fields in a declared order. Field names are the part that matters; types aren't declared.

```bop
struct Point { x, y }
struct Player { name, hp, inventory }
```

Create a value with `TypeName { field: value, ... }`:

```bop
let p = Point { x: 3, y: 4 }
print(p)                  // Point { x: 3, y: 4 }
print(p.x)                // 3
print(type(p))            // "struct"
```

Construction is **strict**: fields you provide must match the declaration exactly (no unknown fields, no duplicates, no missing ones). Extra fields or typos become parse or runtime errors with a "did you mean?" suggestion.

### Field access and assignment

Read with `.field`, write with `.field = value` or any compound assignment:

```bop
let c = Counter { n: 10 }
c.n += 5                 // works
c.n *= 2                 // works
print(c.n)               // 30
```

The field has to already exist; assigning to an undeclared field is an error.

### Passing structs

Structs follow Bop's copy-by-value rule: passing one to a function, returning it, or assigning it to another variable makes an independent copy. Mutating the copy leaves the original alone.

```bop
fn grow(p) { p.x += 10; return p }

let a = Point { x: 1, y: 2 }
let b = grow(a)
print(a)                 // Point { x: 1, y: 2 }
print(b)                 // Point { x: 11, y: 2 }
```

## Enums

An `enum` is a tagged union — one of several named variants, each with an optional payload:

```bop
enum Shape {
  Circle(r),
  Rectangle { w, h },
  Empty,
}
```

Variants come in three shapes:

| Shape | Declaration | Construction |
|-------|-------------|--------------|
| Unit | `Empty` | `Shape::Empty` |
| Tuple | `Circle(r)` | `Shape::Circle(5)` |
| Struct | `Rectangle { w, h }` | `Shape::Rectangle { w: 4, h: 3 }` |

```bop
let a = Shape::Circle(5)
let b = Shape::Rectangle { w: 4, h: 3 }
let c = Shape::Empty
print(type(a))           // "enum"
```

Variants with a struct payload expose their fields via `.field` just like structs:

```bop
let r = Shape::Rectangle { w: 4, h: 3 }
print(r.w * r.h)         // 12
```

Short-name variants like `enum Dir { N, E, S, W }` are accepted — the case rule is "starts with an uppercase letter", not "must contain a lowercase".

## Methods

Attach a method to a type with `fn Type.method(self, ...) { ... }`. The receiver arrives as the first parameter (called `self` by convention; any name works).

```bop
struct Point { x, y }

fn Point.sum(self)      { return self.x + self.y }
fn Point.moved(self, dx, dy) {
  return Point { x: self.x + dx, y: self.y + dy }
}

let p = Point { x: 3, y: 4 }
print(p.sum())                   // 7
print(p.moved(1, 1))             // Point { x: 4, y: 5 }
```

For enums, methods dispatch on the enum type — not per-variant:

```bop
enum Shape { Circle(r), Rectangle { w, h } }

fn Shape.area(self) {
  return match self {
    Shape::Circle(r)          => 3.14159 * r * r,
    Shape::Rectangle { w, h } => w * h,
  }
}

print(Shape::Circle(3).area())                  // 28.27431
print(Shape::Rectangle { w: 4, h: 3 }.area())   // 12
```

A user-declared method with the same name as a builtin (`len`, `keys`, etc.) **wins** over the builtin for receivers of that type — the precedence matches the walker, VM, and AOT.

### Methods return a new value

Because values are copy-by-value, a method can't mutate the receiver in place. Return a new instance and reassign:

```bop
fn Point.shift(self, dx, dy) {
  return Point { x: self.x + dx, y: self.y + dy }
}

let p = Point { x: 0, y: 0 }
p = p.shift(3, 4)        // p is now Point { x: 3, y: 4 }
```

This plays well with fluent chains:

```bop
let final = Point { x: 0, y: 0 }
  .shift(1, 0)
  .shift(0, 2)
  .shift(5, 5)
```

## Equality

Two struct or enum values are equal when their **full type identity** — the module they were declared in plus the type name — *and* every payload matches structurally:

```bop
let p = Point { x: 1, y: 2 }
let q = Point { x: 1, y: 2 }
print(p == q)            // true (structural)

let r = Point { x: 1, y: 3 }
print(p == r)            // false (field differs)
```

Two types with the same name declared in different modules are **distinct** — see [Modules](../modules.md#type-identity).

## Redeclaring the same shape is fine

Declaring the exact same struct or enum twice inside one module is a no-op — matches the "idempotent re-import" rule `use` already follows. Declaring two different shapes with the same name in the same module is a hard error.
