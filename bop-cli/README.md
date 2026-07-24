# bop-cli

The `bop` command-line tool for the [Bop](https://github.com/stevepryde/bop-lang) programming language.

Building or installing Bop 0.4 requires Rust 1.88 or newer.

```sh
cargo install bop-cli
```

Then:

| command                       | what it does                                          |
|-------------------------------|-------------------------------------------------------|
| `bop`                         | open the REPL                                         |
| `bop run script.bop`          | run with the bytecode VM (default, 2–3× the walker)   |
| `bop run script.bop --novm`   | run with the tree-walker                              |
| `bop compile script.bop`      | AOT-compile to a native binary                        |
| `bop compile --emit-rs ...`   | emit the transpiled Rust source only                  |
| `bop --help`                  | full usage                                            |

## REPL

`bop` and `bop repl` open the same stateful session. Declarations survive
between submissions, bare expressions echo their value, incomplete blocks
continue on a secondary prompt, and tab completion includes keywords,
built-ins, and current bindings.

History is persisted at `$HOME/.bop_history`. Meta-commands are:

- `:vars` — list live top-level bindings
- `:reset` or `:clear` — clear the session
- `:help` — show REPL help
- `:quit`, `:q`, or `:exit` — leave

Piped input uses the same multiline submission rules. A parse or runtime error
sets a failing exit status but does not discard later transcript input.

The REPL, `bop run`, and `bop compile` all support transactional `ref`
parameters. See the [reference-parameters
guide](https://bop-lang.com/docs/functions/reference-parameters/) for syntax,
target restrictions, rollback, and host-boundary rules.

## `bop compile`

Transpiles the script via [`bop-compile`](https://crates.io/crates/bop-compile), drops the result into a scratch cargo project, builds it, and copies the binary next to the script (or wherever `-o` points).

```sh
bop compile fib.bop
# builds ./fib  — a standalone native binary
./fib
```

An extensionless source such as `fib` builds `./fib-bin` by default
(`fib-bin.exe` on Windows). Explicit output paths that resolve to the source
file are rejected before Cargo runs, preventing accidental source overwrite.

Flags:

- `-o PATH` / `--output PATH` — where to put the output
- `--emit-rs` — emit the transpiled `.rs` only, don't invoke cargo
- `--keep` — keep the scratch cargo project around (for inspection)

If `cargo` isn't on the PATH, `bop compile` prints a pointer to https://rustup.rs and suggests `--emit-rs` as an escape hatch. `bop run` never needs a toolchain — it only depends on the CLI itself.

## Why the VM by default

Running `bop script.bop` goes through the bytecode VM because it's **2–3× faster than the tree-walker on realistic workloads** with identical semantics. `--novm` is kept as an escape hatch for debugging, or for targets where binary size matters more than execution speed.

## Related crates

- [`bop-lang`](https://crates.io/crates/bop-lang) — the language core
- [`bop-vm`](https://crates.io/crates/bop-vm) — the bytecode runtime `bop run` uses by default
- [`bop-compile`](https://crates.io/crates/bop-compile) — the AOT transpiler `bop compile` drives
- [`bop-sys`](https://crates.io/crates/bop-sys) — the standard host `bop` uses (filesystem imports, stdio, env, time)
- The Bop stdlib (`use std.math`, `std.json`, …) is bundled inside `bop-lang` behind the `bop-std` feature — on by default, so `bop run` / `bop compile` Just Work.

## License

Dual-licensed under [MIT](https://github.com/stevepryde/bop-lang/blob/main/LICENSE-MIT) or [Apache 2.0](https://github.com/stevepryde/bop-lang/blob/main/LICENSE-APACHE), at your option.
