# bop

A small, dynamically-typed programming language, designed for learning to code.

## Features

- Simple, expressive syntax
- First-class functions with recursion
- Arrays, dictionaries, and string interpolation
- Built-in resource limits (step count, memory) for safe embedding
- Embeddable via the `BopHost` trait
- Easy to learn, with helpful error messages

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
bop-lang = "0.1"
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
