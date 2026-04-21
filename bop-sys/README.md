# bop-sys

The standard host for the [Bop](https://github.com/stevepryde/bop-lang) programming language — a `BopHost` implementation that wires Bop up to the normal OS-backed conveniences (filesystem, stdio, environment, time) plus the bundled Bop stdlib.

If you're writing a command-line tool or a desktop / server app that runs Bop scripts, `StdHost` is the default you want. Custom embeddings (sandboxed, wasm, no_std) should write their own `BopHost` impl.

## What `StdHost` provides

### Import resolution

- `use std.math` / `std.json` / `std.collections` / … → resolved via `bop-lang`'s bundled stdlib (the `bop-std` feature, forwarded by default)
- `use my_module` / `my.nested.module` → resolved from the filesystem relative to the script

### Host functions (available to Bop code as `fn_name(...)`)

- `readline()` — read a line from stdin
- `read_file(path)` / `write_file(path, contents)` / `append_file(path, contents)` / `file_exists(path)` — filesystem basics
- `env(var_name)` — read an environment variable
- `unix_time()` / `unix_time_ms()` — current time, seconds / milliseconds since epoch
- `args()` — command-line arguments (for compiled Bop binaries)
- `print` is provided by `bop-lang` itself; `StdHost` routes output to stdout

## Quick start

```toml
[dependencies]
bop-lang = "0.3"
bop-sys  = "0.3"
```

```rust
use bop::{run, BopLimits};
use bop_sys::StdHost;

fn main() {
    let mut host = StdHost::new();
    run(r#"
        use std.math
        print("pi ≈ {pi}")
        let now = unix_time()
        print("running at {now}")
    "#, &mut host, &BopLimits::standard()).unwrap();
}
```

Prefer the faster bytecode runtime? Drop in [`bop-vm`](https://crates.io/crates/bop-vm) — `StdHost` works unchanged:

```rust
bop_vm::run(source, &mut host, &BopLimits::standard())?;
```

## When *not* to use `bop-sys`

- **Sandboxed embeddings** that need to block filesystem / env access — write a bare `BopHost` impl and skip this crate entirely.
- **WASM / no_std builds** — `bop-sys` depends on `std` for filesystem and time. Use `bop-lang` (optionally with `bop-vm`, and with `features = ["bop-std"]` if you want the stdlib) directly.
- **Anywhere you want a tighter custom host surface** — `BopHost::call` is the only thing you need to implement; your host can expose exactly the functions your app wants Bop to reach.

## Related crates

- [`bop-lang`](https://crates.io/crates/bop-lang) — the language core + `BopHost` trait
- The Bop stdlib this crate routes `use std.*` to lives inside `bop-lang` behind the `bop-std` feature (forwarded by default from `bop-sys`).
- [`bop-vm`](https://crates.io/crates/bop-vm) — faster bytecode runtime, drop-in with the same `StdHost`
- [`bop-cli`](https://crates.io/crates/bop-cli) — the `bop` binary built on top of `bop-sys`

## License

Dual-licensed under [MIT](https://github.com/stevepryde/bop-lang/blob/main/LICENSE-MIT) or [Apache 2.0](https://github.com/stevepryde/bop-lang/blob/main/LICENSE-APACHE), at your option.
