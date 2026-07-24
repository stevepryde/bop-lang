# bop-lang

The core of [Bop](https://github.com/stevepryde/bop-lang) — a small, dynamically-typed, **embeddable** scripting language for Rust applications.

Hand your users or your AI a real programming language at runtime, without shipping a compiler to the target machine.

> **Note:** Bop is experimental and not yet battle-tested. Fine for tooling, scripting, and embedding experiments; use with care in production.

## What's in this crate

`bop-lang` is the language core:

- **Lexer + parser** producing a typed AST
- **Tree-walking interpreter** (`bop::run`) — simplest runtime, works everywhere
- **Persistent interpreter** (`bop::BopInstance`) — load once, then call
  explicit `pub fn` entries while globals, modules, callbacks, and RNG state
  remain live
- **`BopHost` trait** — the only thing embedders need to implement to wire Bop into their Rust app
- **`Value` type + builtin operators** — the shared runtime surface every Bop engine uses
- **Transactional `ref` parameters** — explicit copy-in/copy-out updates to
  mutable caller variables, with rollback on errors
- **Resource limits** (`BopLimits`) — step and tracked-memory budgets, plus a fixed function-call depth cap

For a faster runtime (2–3× this crate's tree-walker, same semantics), add [`bop-vm`](https://crates.io/crates/bop-vm). For an AOT path to native Rust, see [`bop-compile`](https://crates.io/crates/bop-compile).

## Selling points

- **Embeddable.** One trait (`BopHost`) to implement; everything else is handled by the engine.
- **Zero Rust deps** Nothing to audit in your supply chain.
- **`no_std` support** via the `no_std` feature (uses the `libm` crate internally for float math, nothing else).
- **WASM-compatible.** Builds clean for `wasm32-unknown-unknown`. Use it in browsers, edge workers, or wherever you can run Rust.
- **Sandboxed by default.** `BopLimits` caps step count and tracked memory, and the runtime caps function-call depth, so runaway user scripts halt cleanly.

## Quick start

```toml
[dependencies]
bop-lang = "0.4"
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
            "greet" => Some(Ok(Value::from("hello!"))),
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

The language normally passes independent values. A user-defined function can
opt into a caller update with `ref` at both sites:

```bop
fn increment(ref value) {
  value += 1
}

let count = 0
increment(ref count)
print(count)    // 1
```

Reference calls stage their changes and commit only after a normal return.
See the [reference-parameters
guide](https://bop-lang.com/docs/functions/reference-parameters/) for target,
rollback, forwarding, method, and host-boundary rules.

For a stateful plugin rather than a one-shot script, declare root entry points
with `pub fn`, then load and call an instance:

```rust
use bop::{BopInstance, BopLimits, Value};

let mut instance = BopInstance::load(
    "let total = 0\npub fn add(n) { total += n; return total }",
    &mut host,
    &BopLimits::standard(),
)?;
let value = instance.call("add", &[Value::Int(3)], &mut host)?;
```

`entry_points()` exposes each public entry's name and arity, while
`call_value()` invokes a callback returned by that same instance. See the
[stateful embedding guide](https://bop-lang.com/docs/embedding/instances/)
for lifecycle, affinity, limits, and error-state behavior.

Host arguments support borrowed or owned typed extraction through
`Value::to_rust` (`&str`, integers, `Vec<T>`, `Option<T>`, `Result<T, E>`, and
deterministic `BTreeMap<String, T>`). Use the fallible, JSON-like `bop_value!`
macro to construct nested values while retaining Bop's depth checks.

## Features

| feature | default | what it does |
|---|---|---|
| `bop-std` | yes | bundles the Bop stdlib (`use std.math`, `std.json`, `std.collections`, `std.iter`, `std.string`, `std.test`) as `&'static str` constants reachable via [`bop::stdlib::resolve`] |
| `no_std` | no | opt in for bare-metal / embedded / edge wasm targets. Pulls in `libm` for float math. Enable with `default-features = false, features = ["no_std"]` (add `"bop-std"` too if you want the bundled stdlib on those targets). |

A truly minimal build — core language only, no bundled stdlib:

```toml
bop-lang = { version = "0.4", default-features = false }
```

## WASM example

```toml
[dependencies]
bop-lang = { version = "0.4", default-features = false, features = ["no_std", "bop-std"] }
```

Build for `wasm32-unknown-unknown` as usual. See [`bop-vm`](https://crates.io/crates/bop-vm) for the faster runtime if you need it.

## Related crates

- [`bop-vm`](https://crates.io/crates/bop-vm) — bytecode compiler + VM, 2–3× faster than this crate's walker, same API
- [`bop-compile`](https://crates.io/crates/bop-compile) — AOT Bop → Rust transpiler for native-speed scripts
- [`bop-sys`](https://crates.io/crates/bop-sys) — ready-made `StdHost` with filesystem / stdio / env / time
- [`bop-cli`](https://crates.io/crates/bop-cli) — the `bop` command-line tool (`bop run`, `bop compile`, REPL)

## License

Dual-licensed under [MIT](https://github.com/stevepryde/bop-lang/blob/main/LICENSE-MIT) or [Apache 2.0](https://github.com/stevepryde/bop-lang/blob/main/LICENSE-APACHE), at your option.
