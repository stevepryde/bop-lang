# bop-lang

The core of [Bop](https://github.com/stevepryde/bop-lang) — a small, dynamically-typed, **embeddable** scripting language for Rust applications.

Hand your users or your AI a real programming language at runtime, without shipping a compiler to the target machine.

> **Note:** Bop is experimental and not yet battle-tested. Fine for tooling, scripting, and embedding experiments; use with care in production.

## What's in this crate

`bop-lang` is the language core:

- **Lexer + parser** producing a typed AST
- **Tree-walking interpreter** (`bop::run`) — simplest runtime, works everywhere
- **`BopHost` trait** — the only thing embedders need to implement to wire Bop into their Rust app
- **`Value` type + builtin operators** — the shared runtime surface every Bop engine uses
- **Resource limits** (`BopLimits`) — step count and memory caps for safe sandboxing

For a faster runtime (2–3× this crate's tree-walker, same semantics), add [`bop-vm`](https://crates.io/crates/bop-vm). For an AOT path to native Rust, see [`bop-compile`](https://crates.io/crates/bop-compile).

## Selling points

- **Embeddable.** One trait (`BopHost`) to implement; everything else is handled by the engine.
- **Zero Rust deps** with the default `std` feature. Nothing to audit in your supply chain.
- **`no_std` support** via the `no_std` feature (uses the `libm` crate internally for float math, nothing else).
- **WASM-compatible.** Builds clean for `wasm32-unknown-unknown` in both std and no_std modes. Use it in browsers, edge workers, or wherever you can run Rust.
- **Sandboxed by default.** `BopLimits` caps step count and memory so a runaway user script can't hang or OOM your process.

## Quick start

```toml
[dependencies]
bop-lang = "0.3"
```

```rust
use bop::{run, BopError, BopHost, BopLimits, Value};

struct MyHost;

impl BopHost for MyHost {
    fn call(&mut self, name: &str, _args: &[Value], _line: u32)
        -> Option<Result<Value, BopError>>
    {
        // Return Some(Ok(...)) to handle a custom function call,
        // Some(Err(...)) to raise, None to defer to builtins.
        match name {
            "greet" => Some(Ok(Value::new_str("hello!".to_string()))),
            _ => None,
        }
    }

    fn on_print(&mut self, msg: &str) {
        println!("{msg}");
    }
}

fn main() {
    let mut host = MyHost;
    let limits = BopLimits::standard();
    run(r#"print(greet())"#, &mut host, &limits).unwrap();
}
```

## Features

| feature | default | what it does |
|---|---|---|
| `std` | yes | uses `std::f64` math, `std` collections, and full runtime |
| `bop-std` | yes | bundles the Bop stdlib (`use std.math`, `std.json`, `std.collections`, `std.iter`, `std.string`, `std.result`, `std.test`) as `&'static str` constants reachable via [`bop::stdlib::resolve`] |
| `no_std` | no | drops the `std` dep, pulls in `libm` for float math. Enable with `default-features = false, features = ["no_std"]` |

A truly minimal build — core language only, no bundled stdlib:

```toml
bop-lang = { version = "0.3", default-features = false, features = ["std"] }
```

## WASM example

```toml
[dependencies]
bop-lang = { version = "0.3", default-features = false, features = ["no_std", "bop-std"] }
```

Build for `wasm32-unknown-unknown` as usual. See [`bop-vm`](https://crates.io/crates/bop-vm) for the faster runtime if you need it.

## Related crates

- [`bop-vm`](https://crates.io/crates/bop-vm) — bytecode compiler + VM, 2–3× faster than this crate's walker, same API
- [`bop-compile`](https://crates.io/crates/bop-compile) — AOT Bop → Rust transpiler for native-speed scripts
- [`bop-sys`](https://crates.io/crates/bop-sys) — ready-made `StdHost` with filesystem / stdio / env / time
- [`bop-cli`](https://crates.io/crates/bop-cli) — the `bop` command-line tool (`bop run`, `bop compile`, REPL)

## License

Dual-licensed under [MIT](https://github.com/stevepryde/bop-lang/blob/main/LICENSE-MIT) or [Apache 2.0](https://github.com/stevepryde/bop-lang/blob/main/LICENSE-APACHE), at your option.
