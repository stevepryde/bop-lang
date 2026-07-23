# bop-compile

Ahead-of-time [Bop](https://github.com/stevepryde/bop-lang) → Rust transpiler.

Given Bop source, `bop-compile::transpile` produces a human-readable Rust source file that links against [`bop-lang`](https://crates.io/crates/bop-lang) and [`bop-sys`](https://crates.io/crates/bop-sys) and compiles via `cargo` to a native binary. The fastest of Bop's three engines.

## When to reach for the AOT

- **Scripts you'll run repeatedly** — builds once, runs at native speed forever after.
- **Performance-sensitive workloads** — where even the bytecode VM's 2–3× speedup isn't enough.
- **Deploying a script as a self-contained binary** — `bop compile script.bop` and ship the resulting executable.

For scripts you compile *at the host's runtime*, the bytecode VM in [`bop-vm`](https://crates.io/crates/bop-vm) is the right choice instead — AOT needs `rustc` on the target machine, which embedded hosts typically can't rely on.

## CLI usage (the common path)

Most users never call `bop-compile` directly; they use [`bop-cli`](https://crates.io/crates/bop-cli):

```sh
bop compile script.bop          # → ./script (native binary)
bop compile script.bop -o app   # → ./app
bop compile --emit-rs script.bop -o script.rs   # transpile only
```

## Library usage

When you want to wire the transpiler into your own build pipeline (a `build.rs`, a custom tool, a CI job):

```toml
[dependencies]
bop-compile = "0.3"
```

```rust
use bop_compile::{transpile, Options};

let rust_source = transpile(
    r#"print("hello from bop")"#,
    &Options::default(),
)?;
// write rust_source to src/main.rs and run `cargo build`…
```

`Options` controls the output shape: standalone program vs. library, module name wrapping, sandbox mode for step/memory enforcement, and the module resolver callback for `use` statements.

### Persistent sandboxed output

Sandbox mode can generate a stateful library surface for plugin-style AOT
embedding. Mark direct root entries with `pub fn`, disable `main`, and compile
the generated Rust into the host application:

```rust
let rust_source = transpile(
    "let count = 0\npub fn next() { count += 1; return count }",
    &Options {
        emit_main: false,
        use_bop_sys: false,
        sandbox: true,
        ..Options::default()
    },
)?;
```

The generated module exposes `BopInstance::load(host, limits)`,
`entry_points()`, `call(name, args, host)`, and
`call_value(callback, args, host)`. It retains program and module state across
calls and enforces instance affinity, re-entry, and resource limits.

The persistent API is sandbox-only. Unsandboxed generated output retains its
one-shot `run` API. See the [stateful embedding
guide](https://bop-lang.com/docs/embedding/instances/) for the
full lifecycle contract.

## Selling points

- **Native-speed scripts.** The transpiled output is ordinary Rust — rustc optimises it the same way it optimises hand-written code.
- **Human-readable output.** User-defined Bop functions become top-level Rust fns with reasonable names, so the generated code is debuggable.
- **Same semantics as walker + VM.** The three-engine differential suite exercises hundreds of programs to catch any behavioural drift.
- **Same `BopHost` surface.** The generated binary uses `bop-sys::StdHost` by default, so your custom hosts work without changes.

## Features

| feature | default | what it does |
|---|---|---|
| `bop-std` | yes | forwards to `bop-lang`'s `bop-std` feature (bundles the Bop stdlib). Turn off with `default-features = false` when building a truly minimal AOT pipeline. |

## Related crates

- [`bop-lang`](https://crates.io/crates/bop-lang) — the language core the generated code depends on
- [`bop-sys`](https://crates.io/crates/bop-sys) — the standard host the generated `main()` wires up
- [`bop-cli`](https://crates.io/crates/bop-cli) — the `bop compile` command-line driver
- [`bop-vm`](https://crates.io/crates/bop-vm) — bytecode VM, for when you need speed at the host's runtime rather than AOT

## License

Dual-licensed under [MIT](https://github.com/stevepryde/bop-lang/blob/main/LICENSE-MIT) or [Apache 2.0](https://github.com/stevepryde/bop-lang/blob/main/LICENSE-APACHE), at your option.
