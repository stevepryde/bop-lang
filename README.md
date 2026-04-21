# bop-lang

Bop is a small, dynamically-typed, **embeddable** programming language for Rust applications — hand your users or your AI a real programming language at runtime, without shipping a compiler to the target machine.

[Documentation](https://stevepryde.github.io/bop-lang/)

> **Note:** Bop is experimental and not yet battle-tested. Fine for tooling, scripting, and embedding experiments; use with care in production.

## Three engines, one language

| engine | crate | when to use it |
|---|---|---|
| Tree-walker | [`bop-lang`](https://crates.io/crates/bop-lang) | simplest embedding, smallest binary, zero deps |
| Bytecode VM | [`bop-vm`](https://crates.io/crates/bop-vm) | **2–3× faster** than the walker, drop-in API, still zero deps |
| AOT transpile | [`bop-compile`](https://crates.io/crates/bop-compile) | compile a script to a native binary at hand-written-Rust speed |

All three share the same parser, `BopHost` trait, `Value` type, and semantics. Switching between them is a one-line change in the consumer's code.

## Features

- **Embeddable** via the `BopHost` trait — call your Rust functions from Bop, and vice-versa
- **Sandboxed by default** — `BopLimits` caps step count and memory so a runaway script can't hang or OOM your process
- **Zero Rust deps** in `std` mode — nothing in your supply chain but Bop itself
- **`no_std` + WASM** compatible (pulls in `libm` for float math, nothing else)
- **Expressive syntax** — functions, closures, arrays, dicts, structs, enums, pattern matching, string interpolation
- **Helpful errors** — source-pointer renderer with carats, "did you mean?" suggestions, match-exhaustiveness warnings

## Examples

```bop
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

// Loops, arrays, method calls
let results = []
for i in range(1, 16) {
    results.push(fizzbuzz(i))
}
print(results.join(", "))

// Dictionaries
let player = {"name": "Ada", "hp": 100}
player["hp"] -= 20
print("{player[\"name\"]} has {player[\"hp\"]} HP")

// Import the stdlib
import std.math
print(std.math.pi)
```

## Quick start — CLI

```
cargo install bop-cli
```

| | |
|---|---|
| `bop`                     | open the REPL |
| `bop script.bop`          | run a script (bytecode VM, 2–3× the walker) |
| `bop run script.bop --novm` | run with the tree-walker instead |
| `bop compile script.bop`  | AOT-compile to a native binary |
| `bop --help`              | full usage |

## Quick start — embedding

```toml
[dependencies]
bop-lang = "0.3"
bop-vm   = "0.3"       # optional — drop in for 2–3× speed
bop-sys  = "0.3"       # ready-made filesystem / stdio host
```

```rust
use bop::BopLimits;
use bop_sys::StdHost;

fn main() {
    let mut host = StdHost::new();
    // Walker path:
    bop::run("print(1 + 2)", &mut host, &BopLimits::standard()).unwrap();
    // Or, same API, the VM for speed:
    bop_vm::run("print(1 + 2)", &mut host, &BopLimits::standard()).unwrap();
}
```

Custom host (sandboxed, expose only what you want Bop to reach):

```rust
use bop::{BopError, BopHost, BopLimits, Value};

struct SandboxedHost;

impl BopHost for SandboxedHost {
    fn call(&mut self, name: &str, args: &[Value], _line: u32)
        -> Option<Result<Value, BopError>>
    {
        match name {
            "now" => Some(Ok(Value::Int(42))), // whatever your app wants
            _ => None,
        }
    }
    fn on_print(&mut self, msg: &str) {
        eprintln!("[sandbox] {msg}");
    }
}
```

## WASM / no_std

Bop builds clean for `wasm32-unknown-unknown` in both std and no_std modes. Walker + VM + libm + `lol_alloc` as `#[global_allocator]` ships at ~355 KB stripped.

```toml
[dependencies]
bop-lang = { version = "0.3", default-features = false, features = ["no_std"] }
bop-vm   = { version = "0.3", default-features = false, features = ["no_std"] }
```

## Crates in this workspace

- [`bop-lang`](bop/) — the language core (parser, walker, `BopHost` trait, `Value`)
- [`bop-vm`](bop-vm/) — bytecode compiler + VM, 2–3× the walker
- [`bop-compile`](bop-compile/) — Bop → Rust AOT transpiler
- [`bop-std`](bop-std/) — the Bop standard library (bundled as Bop source)
- [`bop-sys`](bop-sys/) — `StdHost`, the default OS-backed host
- [`bop-cli`](bop-cli/) — the `bop` command-line tool

## License

Dual-licensed under [Apache 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
