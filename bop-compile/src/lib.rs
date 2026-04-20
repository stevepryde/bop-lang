//! AOT Bop → Rust transpiler.
//!
//! [`transpile`] takes a Bop source string, parses it with `bop-lang`,
//! and emits Rust source that links against `bop-lang` (for
//! [`Value`](bop::value::Value), operators, and language-level
//! builtins) and optionally `bop-sys` (for the standard host).
//!
//! The emitted code is intentionally human-readable: user-defined
//! Bop functions become top-level Rust fns, the top-level program
//! becomes `run_program`, and the `main` entry point drives it
//! against [`bop_sys::StandardHost`]. Run the output with `rustc` or
//! `cargo build` to produce a native binary.
//!
//! # Scope (v1 starter)
//!
//! Supported today:
//!
//! - All literals (numbers, strings, bools, `none`, arrays, dicts)
//! - Binary / unary operators, including short-circuit `&&` / `||`
//! - `let`, assign, compound assign on plain variables
//! - `if` / `else` (both as statement and expression)
//! - `while`, `repeat`, `for x in ...` (over arrays, ranges, or
//!   strings)
//! - `break`, `continue`
//! - Built-in function calls (`print`, `range`, `str`, `int`, `type`,
//!   `abs`, `min`, `max`, `rand`, `len`, `inspect`)
//! - User-defined functions with recursion
//! - Indexed reads (`arr[i]`, `dict[k]`, `"str"[i]`)
//!
//! Not yet emitted (tracked for follow-ups — the transpiler returns
//! an error naming the missing feature):
//!
//! - Method calls (e.g. `arr.push(1)`)
//! - String interpolation (`"hi {name}"`)
//! - Indexed writes (`arr[i] = val`, `arr[i] += 1`)
//! - `BopLimits` sandbox mode (step / memory enforcement in the
//!   emitted code)

use std::cell::RefCell;
use std::rc::Rc;

use bop::error::BopError;
use bop::parser::Stmt;

mod emit;

/// A compile-time module resolver. The AOT runs this eagerly for
/// every `import` it encounters, threading the entire module graph
/// into the generated Rust. Same contract as
/// [`bop::BopHost::resolve_module`]: `None` = not handled,
/// `Some(Ok(source))` = module source text,
/// `Some(Err(_))` = resolver failure.
///
/// Wrapped as `Rc<RefCell<..>>` so `Options` stays `Clone` while
/// the callback keeps `FnMut` freedom.
pub type ModuleResolver =
    Rc<RefCell<dyn FnMut(&str) -> Option<Result<String, BopError>> + 'static>>;

/// Build a [`ModuleResolver`] from an in-memory name→source map.
/// Convenience for tests and simple embedders.
pub fn modules_from_map<S: Into<String>>(
    modules: impl IntoIterator<Item = (S, S)>,
) -> ModuleResolver {
    let map: std::collections::HashMap<String, String> = modules
        .into_iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
    Rc::new(RefCell::new(move |name: &str| {
        map.get(name).cloned().map(Ok)
    }))
}

/// Options that control the shape of the emitted Rust.
///
/// `Debug` is skipped — `module_resolver` is a trait object that
/// has no useful debug representation. If you need to diff options
/// in tests, compare specific fields directly.
#[derive(Clone)]
pub struct Options {
    /// If true, emit a `fn main()` that drives the program with
    /// [`bop_sys::StandardHost`]. If false, emit only the library
    /// surface (the caller is expected to provide their own
    /// entry point and host).
    pub emit_main: bool,
    /// If true, pull `bop-sys::StandardHost` into the generated code
    /// so `main()` can construct it directly. Implied by
    /// [`Self::emit_main`].
    pub use_bop_sys: bool,
    /// If true, the emitted code enforces [`bop::BopLimits`]: step
    /// counts are checked at every loop iteration and function
    /// entry, `bop::memory`'s allocation hooks are initialised with
    /// `max_memory`, and [`bop::BopHost::on_tick`] fires at the
    /// same checkpoints. The generated `run` takes a `&BopLimits`
    /// parameter in this mode.
    ///
    /// When false (the default), the emitted code is straight-line
    /// Rust with no accounting overhead: `run` takes only a host,
    /// and runaway programs are the caller's problem. This matches
    /// the plan's "hot path should be clean" goal.
    pub sandbox: bool,
    /// If `Some(name)`, wrap the entire emitted output in
    /// `pub mod <name> { ... }`. Use this when you want to embed
    /// several transpiled programs in one Rust source file without
    /// colliding on top-level items (`Ctx`, `run_program`,
    /// `run`, `__bop_tick`, user-fn names, etc.). `emit_main` is
    /// ignored in this mode — you provide your own driver.
    pub module_name: Option<String>,
    /// Resolver used to inline imported modules into the emitted
    /// Rust at transpile time. Required when the program contains
    /// any `import` statement; missing (`None`) + an import in
    /// source raises a clear "set `module_resolver`" error.
    ///
    /// The resolver is called eagerly for each transitive import
    /// before any Rust is emitted, so cycle detection and missing
    /// modules both surface at build time rather than at run time.
    pub module_resolver: Option<ModuleResolver>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            emit_main: true,
            use_bop_sys: true,
            sandbox: false,
            module_name: None,
            module_resolver: None,
        }
    }
}

/// Parse Bop source and emit the equivalent Rust source.
///
/// The caller is responsible for writing the returned string to a
/// crate (or `src/main.rs`) and invoking `cargo` or `rustc` on it.
/// The result depends on:
///
/// - `bop-lang` (`bop` crate) — for `Value`, `ops`, `builtins`, …
/// - `bop-sys` — only if [`Options::use_bop_sys`] is true.
pub fn transpile(source: &str, opts: &Options) -> Result<String, BopError> {
    let stmts = bop::parse(source)?;
    transpile_ast(&stmts, opts)
}

/// Lower-level entry point: emit Rust from an already-parsed AST.
pub fn transpile_ast(stmts: &[Stmt], opts: &Options) -> Result<String, BopError> {
    emit::emit(stmts, opts)
}
