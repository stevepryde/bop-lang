# bop

A small, dynamically-typed programming language, designed for learning to code.

[Documentation](https://stevepryde.github.io/bop-lang/)

> **Note:** Bop is experimental and has not been battle-tested. Do not use it in production or mission-critical environments.

## Features

- No dependencies
- Simple, expressive syntax
- Functions with recursion
- Arrays, dictionaries, and string interpolation
- Built-in resource limits (step count, memory) for safe embedding
- Embeddable via the `BopHost` trait
- Easy to learn, with helpful error messages

## Examples

```
// Variables and string interpolation
let name = "world"
print("Hello {name}!")

// Functions
fn fizzbuzz(n) {
    if n % 15 == 0 { return "FizzBuzz" }
    if n % 3 == 0 { return "Fizz" }
    if n % 5 == 0 { return "Buzz" }
    return str(n)
}

// Loops and arrays
let results = []
for i in range(1, 16) {
    results.push(fizzbuzz(i))
}
print(results.join(", "))

// Dictionaries
let player = {"name": "Ada", "hp": 100}
player["hp"] -= 20
let name = player["name"]
let hp = player["hp"]
print("{name} has {hp} HP")

// Built-in methods
let words = "the quick brown fox".split(" ")
words.sort()
print(words.join(", "))  // brown, fox, quick, the
```

## Quick start

```
cargo install bop-cli
```

Run a file:

```
bop script.bop
```

Or start the REPL:

```
bop
```

## Embedding

Add `bop-lang` to your `Cargo.toml`:

```toml
[dependencies]
bop-lang = "0.2"
```

```rust
use bop::{run, BopLimits, StdHost};

fn main() {
    let mut host = StdHost;
    run("print(1 + 2)", &mut host, &BopLimits::standard()).unwrap();
}
```

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT License](LICENSE-MIT) at your option.
