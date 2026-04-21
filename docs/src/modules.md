# Modules

A Bop program can be split across multiple files — or in-memory source strings, asset bundles, anywhere the embedding host can return Bop source. The `use` statement pulls another module's public surface into the current scope.

## The four forms of `use`

```bop
use path                    // glob:        everything public
use path.{a, b, Type}       // selective:   just the listed items
use path as m               // aliased:     binds `m` as a Value::Module
use path.{a, b} as m        // aliased + selective
```

Paths are dot-joined identifiers: `std.math`, `game.entity.player`. How the host resolves a path is up to the embedder — `bop-sys`'s `StandardHost::with_module_root` maps `foo.bar` to `<root>/foo/bar.bop`, in-memory hosts can look up a string table, a web host can fetch a URL. See [Embedding](embedding.md#resolve_module--custom-use-resolution).

## Glob `use`

Brings every public export of a module into the current scope as a bare name:

```bop
use std.math
print(pi)            // constant from std.math
print(sqrt(9))       // fn from std.math
```

Names that start with `_` are considered **private by convention** and glob imports skip them:

```bop
// In module `foo`:
fn _helper() { return 42 }
fn public() { return _helper() }

// Elsewhere:
use foo
print(public())      // 42
// print(_helper())  // error: `_helper` not in scope
```

Glob is idempotent at the injection site — running `use foo` twice in the same scope is a no-op (matches Python's `import foo; import foo`). When two glob imports would introduce the same name, the first wins and the second emits a runtime warning — explicit selective imports are the way to disambiguate.

## Selective `use`

Pick exactly which names you want:

```bop
use std.math.{pi, sqrt}
print(pi)
print(sqrt(16))
// print(sin(0))   // error — not imported
```

Selective imports can reach private names explicitly:

```bop
use foo.{_helper}
print(_helper())     // ok — explicit opt-in
```

If a listed name doesn't exist in the target module, you get a clear error pointing at the `use` site.

## Aliased `use`

Binds the whole module as a single value under the alias:

```bop
use std.math as m
print(m.pi)
print(m.sqrt(9))
```

`m` is a `Value::Module` — `type(m)` is `"module"`. You access its exports via the `.` operator. Methods on aliased modules (`m.helper(...)`) work the same way they would on a bare imported fn.

Combine with selective to shrink the alias's surface:

```bop
use std.math.{pi, sqrt} as m
print(m.pi)
print(m.sqrt(9))
// print(m.sin(0))   // error — `sin` wasn't imported
```

## Namespaced types

User-defined `struct` and `enum` types can be constructed and pattern-matched through the alias:

```bop
// In `paint.bop`:
enum Color { Red, Green, Blue }
struct Point { x, y }

// In main:
use paint as p
let c = p.Color::Red
let origin = p.Point { x: 0, y: 0 }

print(match c {
  p.Color::Red   => "stop",
  p.Color::Green => "go",
  p.Color::Blue  => "cool",
})
```

The namespace is required — bare `Color::Red` inside the main file wouldn't find the type unless you also imported `paint.{Color}` by bare name.

## Type identity

Types carry their declaring module as part of their identity. Two modules can declare a type with the same name; values from them are **distinct types** — equality is always `false` across the module boundary, and patterns only match values from the module the pattern named.

```bop
// paint.bop: enum Color { Red, Blue }
// other.bop: enum Color { Red, Green, Yellow }

use paint as p
use other as o

let a = p.Color::Red
let b = o.Color::Red

print(a == b)        // false — different `Color` types
print(a == a)        // true
```

A pattern over an aliased module's type only fires for values from that module:

```bop
fn label(c) {
  return match c {
    p.Color::Red => "paint-red",
    o.Color::Red => "other-red",
    _            => "something else",
  }
}
print(label(p.Color::Red))   // "paint-red"
print(label(o.Color::Red))   // "other-red"
```

This is Bop's answer to the "same-named type, different shape, in different modules" problem. No renames required.

## Re-exports are transitive

A module's effective exports include everything it `use`s from other modules (minus privacy filtering). If `a` does `use b` and `b` declares `fn foo()`, then `use a` in the top-level program makes `foo` visible too. The same applies to types — importing `a` brings `b`'s public types in scope.

## Builtin types

`Result` and `RuntimeError` are engine built-ins. They're always in scope — you don't need `use std.result` to write `Result::Ok(v)` or to match on `RuntimeError { message, line }`. The `std.result` module exists for combinators (`unwrap`, `map`, `and_then`, …), not for the types themselves. See [Error Handling](errors.md).

## Cycles

Circular imports (`a` uses `b` which uses `a`) are detected at load time and raise a clear error naming the cycle path. Restructure the code so the cycle breaks — usually by pulling shared definitions into a third module that neither circular node depends on.

## Inside a function body

Aliased modules and bare-imported types remain visible inside function bodies declared in the same module:

```bop
use paint as p

fn describe(c) {
  return match c {
    p.Color::Red   => "red",
    p.Color::Blue  => "blue",
    _              => "other",
  }
}
```

The `p` alias doesn't need to be a parameter — module-level aliases persist across function call boundaries so patterns inside fn bodies can resolve them.
