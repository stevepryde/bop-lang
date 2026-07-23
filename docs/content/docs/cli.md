+++
title = "Command-line interface"
description = "Install and use the bop CLI to run scripts with the VM or walker, compile native executables, emit Rust source, and open the persistent REPL."
weight = 16
template = "docs/page.html"
page_template = "docs/page.html"
[extra.previous]
title = "REPL"
path = "/docs/repl/"
[extra.next]
title = "Operators"
path = "/docs/reference/operators/"
+++

# Command-line interface

Install the current Bop command-line tool from crates.io:

```sh
cargo install bop-cli
```

## Run a script

```sh
bop run app.bop
```

`bop run` uses the bytecode VM by default. It runs the warning pass before
execution, resolves `std.*` from the bundled standard library, and resolves
other module paths relative to the script.

Use the tree-walker when debugging engine behavior or minimizing the active
runtime:

```sh
bop run --novm app.bop
```

Both paths render the same source snippets, carets, hints, module context, and
warnings.

## Compile a native executable

```sh
bop compile app.bop
bop compile app.bop -o my-app
```

The command transpiles Bop to Rust, creates a temporary Cargo project, builds a
release binary, and copies it to the requested output path. `cargo` and a Rust
toolchain must be available for this step.

Use `--emit-rs` to stop after transpilation:

```sh
bop compile --emit-rs app.bop -o app.rs
```

Use `--keep` when building a binary to retain the scratch Cargo project for
inspection after the command finishes.

## Open the REPL

Either spelling starts the persistent interactive session:

```sh
bop
bop repl
```

See [REPL](/docs/repl/) for multiline input, live bindings, history,
completion, meta-commands, and piped transcripts.

## Help and version

```sh
bop --help
bop --version
```

Argument and usage errors exit with status 2. Parse, runtime, module, and build
failures exit non-zero after rendering their diagnostic.
