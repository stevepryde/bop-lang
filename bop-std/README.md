# bop-std

The standard library for the [Bop](https://github.com/stevepryde/bop-lang) programming language, shipped as **bundled Bop source** rather than Rust code.

Nothing in this crate runs unless an embedder wires [`resolve`] into their `BopHost::resolve_module` implementation — which is exactly what [`bop-sys`](https://crates.io/crates/bop-sys)'s `StandardHost` does out of the box.

## Modules

| module             | what it provides                                                                   |
|--------------------|------------------------------------------------------------------------------------|
| `std.result`       | `Result::Ok` / `Result::Err` enum + `is_ok` / `unwrap` / `map` / `and_then` / …    |
| `std.math`         | `pi`, `e`, `tau`, `clamp`, `sign`, `factorial`, `gcd`, `lcm`, `sum`, `mean`        |
| `std.iter`         | `map`, `filter`, `reduce`, `take`, `drop`, `zip`, `enumerate`, `any`, `all`, …     |
| `std.string`       | `pad_left`, `pad_right`, `repeat`, `chars`, `reverse`, `is_palindrome`, `join`, …  |
| `std.collections`  | `Stack`, `Queue`, `Set` (struct types with value-semantic methods, set algebra)    |
| `std.json`         | `parse(text)` + `stringify(value)`                                                 |
| `std.test`         | `assert`, `assert_eq`, `assert_near`, `assert_raises`                              |

## Why bundled Bop source

- **Zero Rust runtime deps.** Each module is a `&'static str` constant baked in at build time via `include_str!`. Your embedding's dep graph doesn't grow.
- **Portable.** Works everywhere Bop itself does — `std`, `no_std`, WASM.
- **Transparent.** The `.bop` files under `src/modules/` *are* the stdlib; there's no hidden Rust layer. What the user reads in the source matches what the runtime executes.

## Usage

Most embedders get `bop-std` for free via `bop-sys`:

```rust
use bop::{run, BopLimits};
use bop_sys::StdHost;

let mut host = StdHost::new();  // resolves `import std.*` via bop-std automatically
run(r#"
    import std.math
    print(std.math.pi)
"#, &mut host, &BopLimits::standard()).unwrap();
```

If you're building a custom host, wire the resolver in yourself:

```rust
impl BopHost for MyHost {
    fn resolve_module(&mut self, name: &str) -> Option<Result<String, BopError>> {
        if let Some(src) = bop_std::resolve(name) {
            return Some(Ok(src.to_string()));
        }
        // your own import logic, then None to signal "not found"
        None
    }
    // ...
}
```

## Related crates

- [`bop-lang`](https://crates.io/crates/bop-lang) — the language core
- [`bop-sys`](https://crates.io/crates/bop-sys) — the `StdHost` that routes `import std.*` through this crate
- [`bop-cli`](https://crates.io/crates/bop-cli) — the `bop` CLI, which uses `StdHost` (and so `bop-std`) by default

## License

Dual-licensed under [MIT](https://github.com/stevepryde/bop-lang/blob/main/LICENSE-MIT) or [Apache 2.0](https://github.com/stevepryde/bop-lang/blob/main/LICENSE-APACHE), at your option.
