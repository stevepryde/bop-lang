# bop-vm

Bytecode compiler + stack VM for the [Bop](https://github.com/stevepryde/bop-lang) programming language.

**2–3× faster** than the tree-walker in [`bop-lang`](https://crates.io/crates/bop-lang), with identical semantics. Same `BopHost` trait, same `BopLimits`, same error shapes — swap `bop::run` for `bop_vm::run` and you have the fast engine.

## Why a VM at all

A tree-walker is simple but does a lot of redundant work per instruction — scope lookups, AST shuffling, allocation. `bop-vm` compiles Bop source to a compact bytecode where:

- Locals live in a flat `Vec<Value>` (numbered slots, no `String` hashing)
- Common patterns (`i = i + 1`, `n - 1`, `n < 2`, `total + i`) collapse into fused superinstructions with typed Int fast paths
- Function calls pop args directly into the new frame's slot array, no intermediate `Vec<Value>`
- Slot vecs are pooled across calls, so a recursive program doesn't hit the allocator 500k times

Net result (release, Apple silicon):

| workload | walker | VM | speedup |
|---|---|---|---|
| `fib(28)` — call-heavy | 180 ms | 72 ms | **2.5×** |
| 500k-iter tight loop | 72 ms | 23 ms | **3.1×** |
| combined | 70 ms | 27 ms | **2.6×** |

## The embedding niche

`bop-vm` earns its place in the embedding use case: a Rust application that hands Bop source to its users (or to an AI) at runtime. You can't bring `rustc` to the user's machine for AOT — the VM fills the gap:

- **No dependencies** (default `std` feature)
- **no_std-capable** via the `no_std` feature (pulls in `libm` internally for float math)
- **WASM-compatible** (builds clean for `wasm32-unknown-unknown`; ~90 KB added over the walker-only bundle)
- **Same trait surface as the walker** — any `BopHost` impl works unchanged

## Quick start

```toml
[dependencies]
bop-lang = "0.3"
bop-vm = "0.3"
```

```rust
use bop::{BopError, BopHost, BopLimits, Value};

struct MyHost;
impl BopHost for MyHost {
    fn call(&mut self, _: &str, _: &[Value], _: u32) -> Option<Result<Value, BopError>> { None }
    fn on_print(&mut self, msg: &str) { println!("{msg}"); }
}

fn main() {
    let mut host = MyHost;
    bop_vm::run("print(1 + 2)", &mut host, &BopLimits::standard()).unwrap();
}
```

For scripts you'll run repeatedly, compile once and execute many times:

```rust
use bop_vm::{compile, execute};
let stmts = bop::parse(source)?;
let chunk = compile(&stmts)?;
for _ in 0..1000 {
    execute(chunk.clone(), &mut host, &BopLimits::standard())?;
}
```

## Features

| feature | default | what it does |
|---|---|---|
| `std` | yes | standard runtime, no external deps |
| `no_std` | no | forwards to `bop-lang`'s `no_std` feature, which pulls in `libm`. Enable with `default-features = false, features = ["no_std"]` |

## WASM example

```toml
[dependencies]
bop-lang = { version = "0.3", default-features = false, features = ["no_std"] }
bop-vm   = { version = "0.3", default-features = false, features = ["no_std"] }
```

Tested and working end-to-end on `wasm32-unknown-unknown` with `lol_alloc` as the global allocator.

## Related crates

- [`bop-lang`](https://crates.io/crates/bop-lang) — core language + `BopHost` trait + walker
- [`bop-compile`](https://crates.io/crates/bop-compile) — AOT Bop → Rust transpiler
- [`bop-cli`](https://crates.io/crates/bop-cli) — the `bop` binary (`bop run` uses this VM by default)

## License

Dual-licensed under [MIT](https://github.com/stevepryde/bop-lang/blob/main/LICENSE-MIT) or [Apache 2.0](https://github.com/stevepryde/bop-lang/blob/main/LICENSE-APACHE), at your option.
