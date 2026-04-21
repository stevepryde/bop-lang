# bop-lang

A small, dynamically-typed, **embeddable** programming language for Rust hosts — give your users or your agent a real scripting language at runtime, with the sandbox treated as a first-class invariant instead of a bolted-on afterthought.

[Documentation](https://stevepryde.github.io/bop-lang/)

> **Note:** Bop is experimental and not yet battle-tested. Good for tooling, scripting, embedding experiments, and sandbox-first workloads; use with care in production.

## Why Bop?

- **Embedded-first.** One crate (`bop-lang`), one trait (`BopHost`), no runtime dependencies. You wire up the functions you want Bop to reach; Bop can't touch anything else.
- **Sandboxed by default.** No filesystem, network, clock, or ambient I/O. `BopLimits` caps three things the language itself can't escape: steps executed, bytes allocated, and fn-call depth. A runaway script halts cleanly with a diagnostic, not a hung process.
- **Three engines, one language.** Walker, bytecode VM, or AOT-to-Rust transpiler — same parser, same semantics, same error shapes. Switch engines with a one-line change.
- **`no_std` + WASM.** Core crate builds clean for `wasm32-unknown-unknown` and bare-metal targets. Enable the `no_std` feature for a `libm`-backed math facade.
- **Small, stable grammar.** Functions, closures, arrays, dicts, structs, enums, pattern matching, string interpolation, modules, `Result` / `Iter` built-ins. Deliberately small — easy to teach, easy for tooling to target.
- **Helpful errors.** Parse and runtime errors include the source snippet, a carat under the offending column, and `hint:` suggestions (`"I don't know what 'pritn' is — did you mean 'print'?"`).

## Three engines, one language

| engine | crate | when to use it |
|---|---|---|
| Tree-walker | [`bop-lang`](https://crates.io/crates/bop-lang) | simplest embedding, smallest binary, best diagnostics |
| Bytecode VM | [`bop-vm`](https://crates.io/crates/bop-vm) | **2–3× faster** than the walker, drop-in API, still zero deps |
| AOT transpile | [`bop-compile`](https://crates.io/crates/bop-compile) | compile a script to a native binary at hand-written-Rust speed |

All three share the same parser, `BopHost` trait, `Value` type, and semantics. A three-way differential test suite pins them to byte-for-byte output agreement.

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
    return n.to_str()
}

// Loops, arrays, method calls
let results = []
for i in range(1, 16) {
    results.push(fizzbuzz(i))
}
print(results.join(", "))

// Dicts + missing-key soft lookup
let player = {"name": "Ada", "hp": 100}
player["hp"] -= 20
if player["inventory"].is_none() { print("no inventory") }

// Result + try
fn parse_positive(s) {
    let n = s.to_int()
    if n <= 0 { return Err("must be positive") }
    return Ok(n)
}

// Stdlib
use std.math
print(PI)
```

## Quick start — CLI

```
cargo install bop-cli
```

| | |
|---|---|
| `bop`                       | open the REPL |
| `bop run script.bop`        | run a script (bytecode VM by default) |
| `bop run script.bop --novm` | run with the tree-walker instead |
| `bop compile script.bop`    | AOT-compile to a native binary |
| `bop --help`                | full usage |

## Quick start — embedding

```toml
[dependencies]
bop-lang = "0.3"
bop-vm   = "0.3"       # optional — drop in for 2–3× speed
bop-sys  = "0.3"       # ready-made filesystem / stdio / env / time host
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

Custom sandboxed host — Bop can only reach the fns you expose:

```rust
use bop::{BopError, BopHost, BopLimits, Value};

struct SandboxedHost;

impl BopHost for SandboxedHost {
    fn call(&mut self, name: &str, args: &[Value], _line: u32)
        -> Option<Result<Value, BopError>>
    {
        match name {
            // Expose exactly the primitives your program wants to
            // let scripts reach. Everything else is invisible.
            "now" => Some(Ok(Value::Int(42))),
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

- [`bop-lang`](bop/) — the language core (parser, walker, `BopHost` trait, `Value`). The Bop stdlib (`use std.math`, `std.json`, …) ships inside this crate as bundled Bop source, gated behind the `bop-std` feature (on by default).
- [`bop-vm`](bop-vm/) — bytecode compiler + VM, 2–3× the walker
- [`bop-compile`](bop-compile/) — Bop → Rust AOT transpiler
- [`bop-sys`](bop-sys/) — `StdHost`, the default OS-backed host
- [`bop-cli`](bop-cli/) — the `bop` command-line tool

## License

Dual-licensed under [Apache 2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT), at your option.
