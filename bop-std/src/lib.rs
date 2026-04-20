//! # bop-std
//!
//! Bop's standard library, shipped as bundled **Bop source**
//! rather than Rust code. Nothing in this crate runs unless an
//! embedder wires [`resolve`] into their `BopHost::resolve_module`
//! implementation — which is exactly what `bop-sys`'s
//! `StandardHost` does out of the box.
//!
//! The crate has zero runtime dependencies; each module is a
//! plain string constant baked in at build time via
//! `include_str!`. That means the stdlib can't drift from what
//! ships — the `.bop` source files in `src/modules/` are the
//! source of truth.
//!
//! ## Available modules
//!
//! - `std.result` — `Result` enum, `RuntimeError` struct, and
//!   combinators (`is_ok`, `unwrap`, `map`, `and_then`, …).
//! - `std.math` — numeric constants and helpers that wrap core
//!   builtins (`pi`, `e`, `sqrt_safe`, `clamp`, …).
//! - `std.iter` — functional helpers on arrays (`map`, `filter`,
//!   `reduce`, `sum`, `find`, …).
//! - `std.string` — string helpers that didn't fit the
//!   method-on-string pattern (`pad_left`, `pad_right`,
//!   `chars`, …).
//! - `std.test` — `assert`, `assert_eq`, `assert_near` plus a
//!   tiny test-runner.
//! - `std.collections` — `Set`, `Queue`, `Stack` as struct
//!   types with value-semantic methods (`s = s.push(v)` etc.).

#![deny(missing_docs)]

const RESULT: &str = include_str!("modules/result.bop");
const MATH: &str = include_str!("modules/math.bop");
const ITER: &str = include_str!("modules/iter.bop");
const STRING_MOD: &str = include_str!("modules/string.bop");
const TEST_MOD: &str = include_str!("modules/test.bop");
const COLLECTIONS: &str = include_str!("modules/collections.bop");

/// Map a `std.*` module name to its bundled Bop source.
///
/// Returns `None` for any path outside the stdlib — chain this
/// with your own `BopHost::resolve_module` so user modules
/// still resolve:
///
/// ```ignore
/// impl BopHost for MyHost {
///     fn resolve_module(&mut self, name: &str) -> Option<Result<String, BopError>> {
///         if let Some(src) = bop_std::resolve(name) {
///             return Some(Ok(src.to_string()));
///         }
///         // fall back to your own resolver (filesystem, embedded
///         // modules, ...)
///         self.my_own_resolver(name)
///     }
/// }
/// ```
pub fn resolve(name: &str) -> Option<&'static str> {
    match name {
        "std.result" => Some(RESULT),
        "std.math" => Some(MATH),
        "std.iter" => Some(ITER),
        "std.string" => Some(STRING_MOD),
        "std.test" => Some(TEST_MOD),
        "std.collections" => Some(COLLECTIONS),
        _ => None,
    }
}

/// Every module name this crate can resolve. Useful for docs,
/// diagnostics, or a "did you mean…" suggestion in error paths.
pub const MODULES: &[&str] = &[
    "std.result",
    "std.math",
    "std.iter",
    "std.string",
    "std.test",
    "std.collections",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_returns_source_for_known_modules() {
        for name in MODULES {
            assert!(
                resolve(name).is_some(),
                "stdlib module {} should resolve",
                name
            );
        }
    }

    #[test]
    fn resolve_returns_none_for_unknown() {
        assert!(resolve("std.nope").is_none());
        assert!(resolve("user.code").is_none());
        assert!(resolve("").is_none());
    }
}
