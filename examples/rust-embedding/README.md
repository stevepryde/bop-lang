# Bop Rust embedding examples

These standalone binaries are compile-tested as part of the Bop workspace.
They are intended to be copied by developers and coding models integrating Bop
into another Rust application.

For an application using published Bop 0.4 crates, start with:

```toml
[dependencies]
bop = { package = "bop-lang", version = "0.4" }
bop-vm = "0.4" # optional

[build-dependencies]
bop-compile = "0.4" # AOT example only
```

The workspace manifest uses local paths so these examples test the source
currently being developed:

- [`custom_host.rs`](src/bin/custom_host.rs) implements a narrow capability
  boundary and runs the same source with the walker and VM.
- [`persistent_instance.rs`](src/bin/persistent_instance.rs) loads a program
  once and calls stateful `pub fn` entries through both engines.
- [`aot_plugin.rs`](src/bin/aot_plugin.rs), [`build.rs`](build.rs), and
  [`plugin.bop`](src/plugin.bop) form a complete sandboxed AOT integration.

Run them from the workspace root:

```sh
cargo run -p bop-rust-embedding-examples --bin custom_host
cargo run -p bop-rust-embedding-examples --bin persistent_instance
cargo run -p bop-rust-embedding-examples --bin aot_plugin
```

`bop-sys::StandardHost` is intentionally absent: it grants filesystem, stdin,
environment, and clock capabilities. Use a narrow custom `BopHost` like these
examples when the script is untrusted.
