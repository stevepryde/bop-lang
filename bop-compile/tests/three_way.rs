//! Three-way differential tests: tree-walker vs bytecode VM vs AOT.
//!
//! For a curated corpus of programs, every engine must produce
//! identical prints and agree on success/error. This is the final
//! piece of step 3 — once this harness is green, the transpiled
//! path is proven to match the reference semantics on the same
//! programs the walker and VM already agree on.
//!
//! # Why it's `#[ignore]`'d
//!
//! The AOT leg spins up a single `cargo run` per test invocation.
//! Even with a batched driver that compiles all corpus programs in
//! one Rust source file, each run is ~2s (first build) to ~0.5s
//! (warm), which is too slow for every `cargo test` pass. Run it
//! with
//!
//! ```text
//! cargo test -p bop-compile --test three_way -- --ignored
//! ```
//!
//! when verifying AOT or before a release.
//!
//! # Batching
//!
//! Each program is transpiled into its own `pub mod p_<name>` via
//! `Options::module_name`. All modules plus a small driver `fn
//! main()` are concatenated into one `src/main.rs`. The driver
//! runs each program sequentially, captures its prints into a
//! buffered host, and emits a delimited envelope on stdout. The
//! harness parses that envelope back into `(Vec<String>, Result)`
//! outcomes and compares them against the walker / VM.

use std::cell::RefCell;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use bop::{BopError, BopHost, BopLimits, Value};
use bop_compile::{Options, transpile};

// ─── Shared test host ─────────────────────────────────────────────

struct BufHost {
    prints: RefCell<Vec<String>>,
    modules: std::collections::HashMap<String, String>,
}

impl BufHost {
    fn new(modules: &[(&str, &str)]) -> Self {
        Self {
            prints: RefCell::new(Vec::new()),
            modules: modules
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
        }
    }
}

impl BopHost for BufHost {
    fn call(
        &mut self,
        _name: &str,
        _args: &[Value],
        _line: u32,
    ) -> Option<Result<Value, BopError>> {
        None
    }

    fn on_print(&mut self, message: &str) {
        self.prints.borrow_mut().push(message.to_string());
    }

    fn resolve_module(&mut self, name: &str) -> Option<Result<String, BopError>> {
        if let Some(src) = self.modules.get(name) {
            return Some(Ok(src.clone()));
        }
        bop::stdlib::resolve(name).map(|s| Ok(s.to_string()))
    }
}

/// Normalised per-engine outcome the harness compares across
/// walker / VM / AOT.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Outcome {
    prints: Vec<String>,
    error: Option<String>,
}

fn walker_outcome(code: &str, modules: &[(&str, &str)]) -> Outcome {
    let mut host = BufHost::new(modules);
    let result = bop::run(code, &mut host, &BopLimits::standard());
    Outcome {
        prints: host.prints.borrow().clone(),
        error: result.err().map(|e| e.message),
    }
}

fn vm_outcome(code: &str, modules: &[(&str, &str)]) -> Outcome {
    let mut host = BufHost::new(modules);
    let result = bop_vm::run(code, &mut host, &BopLimits::standard());
    Outcome {
        prints: host.prints.borrow().clone(),
        error: result.err().map(|e| e.message),
    }
}

// ─── AOT batched runner ───────────────────────────────────────────
//
// The expensive bit: transpile every corpus program, stitch them
// into one `src/main.rs` wrapped under per-program modules, write a
// scratch cargo project, `cargo run`, parse the delimited output
// back into `Outcome`s.

fn workspace_root() -> PathBuf {
    let crate_dir: PathBuf = env!("CARGO_MANIFEST_DIR").into();
    crate_dir.parent().unwrap().to_path_buf()
}

fn scratch_dir(name: &str) -> PathBuf {
    let mut p = workspace_root();
    p.push("target");
    p.push("bop-compile-three-way");
    p.push(name);
    p
}

/// One test in the three-way corpus. `modules` is optional — empty
/// slice means the program has no imports.
struct CorpusEntry {
    name: &'static str,
    source: &'static str,
    modules: &'static [(&'static str, &'static str)],
}

/// Construct an AOT `ModuleResolver` that looks up corpus-local
/// overrides first, then falls back to `bop::stdlib::resolve` so
/// `use std.*` works without every test having to redeclare
/// the stdlib. Entries with no imports at all still receive a
/// resolver — it's never called for them, so the extra
/// allocation is cheap.
fn build_resolver(
    overrides: &[(&str, &str)],
) -> Option<bop_compile::ModuleResolver> {
    let map: std::collections::HashMap<String, String> = overrides
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    Some(std::rc::Rc::new(std::cell::RefCell::new(move |name: &str| {
        if let Some(src) = map.get(name) {
            return Some(Ok(src.clone()));
        }
        bop::stdlib::resolve(name).map(|s| Ok(s.to_string()))
    })))
}

/// Build the single-file AOT driver that runs every program in the
/// corpus and emits per-program envelopes on stdout.
fn build_driver(programs: &[CorpusEntry]) -> String {
    let mut out = String::new();
    out.push_str(DRIVER_HEADER);

    // One pub mod per program. Sandbox is on so runaway programs
    // can't hang the CI machine — the walker and VM run with the
    // same `BopLimits::standard()` so the comparison stays fair.
    for entry in programs {
        // Resolver: entry-local modules first, then bop-std
        // stdlib as a fallback. Programs with no imports and no
        // stdlib reach get `None` to avoid building an empty
        // resolver for every no-use corpus entry.
        let resolver = build_resolver(entry.modules);
        let mod_src = transpile(
            entry.source,
            &Options {
                emit_main: false,
                use_bop_sys: false,
                sandbox: true,
                module_name: Some(format!("p_{}", entry.name)),
                module_resolver: resolver,
            },
        )
        .unwrap_or_else(|e| panic!("transpile failed for {}: {}", entry.name, e.message));
        out.push_str(&mod_src);
        out.push('\n');
    }

    // Driver: iterate through programs, capture prints, emit
    // envelope. We inline the calls rather than build a Vec of
    // trait objects — each `p_X::run` is generic over H and can't
    // be trivially type-erased.
    out.push_str("fn main() {\n");
    out.push_str("    let mut out = ::std::string::String::new();\n");
    for entry in programs {
        let name = entry.name;
        writeln!(
            out,
            concat!(
                "    {{\n",
                // Driver-side BufHost, defined in DRIVER_HEADER —
                // distinct from the harness-side one, no modules
                // map (AOT resolves at compile time).
                "        let mut host = BufHost::new();\n",
                "        let limits = ::bop::BopLimits::standard();\n",
                "        let result = p_{name}::run(&mut host, &limits);\n",
                "        out.push_str(\"<<TEST>>{name}\\n\");\n",
                "        for p in &host.prints {{\n",
                "            out.push_str(\"<<PRINT>>\");\n",
                "            out.push_str(p);\n",
                "            out.push_str(\"<<END>>\\n\");\n",
                "        }}\n",
                "        match result {{\n",
                "            Ok(()) => out.push_str(\"<<OK>>\\n\"),\n",
                "            Err(e) => {{\n",
                "                out.push_str(\"<<ERR>>\");\n",
                "                out.push_str(&e.message);\n",
                "                out.push_str(\"<<END>>\\n\");\n",
                "            }}\n",
                "        }}\n",
                "    }}\n",
            ),
            name = name
        )
        .unwrap();
    }
    out.push_str("    ::std::print!(\"{}\", out);\n");
    out.push_str("}\n");
    out
}

const DRIVER_HEADER: &str = r#"// Auto-generated by three_way.rs for the AOT leg of the
// tree-walker / VM / AOT differential harness.
#![allow(dead_code, unused_imports, unused_variables, clippy::all)]

pub struct BufHost {
    pub prints: ::std::vec::Vec<String>,
}

impl BufHost {
    pub fn new() -> Self {
        Self { prints: ::std::vec::Vec::new() }
    }
}

impl ::bop::BopHost for BufHost {
    fn call(
        &mut self,
        _name: &str,
        _args: &[::bop::value::Value],
        _line: u32,
    ) -> Option<Result<::bop::value::Value, ::bop::error::BopError>> {
        None
    }

    fn on_print(&mut self, message: &str) {
        self.prints.push(message.to_string());
    }
}

"#;

fn write_driver_project(driver_src: &str) -> PathBuf {
    let root = workspace_root();
    let dir = scratch_dir("driver");
    let src_dir = dir.join("src");
    std::fs::create_dir_all(&src_dir).expect("create scratch src dir");

    let bop_path = root.join("bop");
    let manifest = format!(
        r#"[package]
name = "bop-three-way-driver"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
bop = {{ path = "{bop}", package = "bop-lang" }}

[[bin]]
name = "driver"
path = "src/main.rs"

[workspace]
"#,
        bop = bop_path.display()
    );
    std::fs::write(dir.join("Cargo.toml"), manifest).expect("write Cargo.toml");
    std::fs::write(src_dir.join("main.rs"), driver_src).expect("write main.rs");
    dir
}

fn run_aot_batch(programs: &[CorpusEntry]) -> Vec<(String, Outcome)> {
    let driver_src = build_driver(programs);
    let dir = write_driver_project(&driver_src);
    let output = Command::new("cargo")
        .arg("run")
        .arg("--quiet")
        .arg("--release")
        // Cargo gives this precedence over RUSTFLAGS. Remove an
        // inherited value so the warning-denial contract below
        // cannot be bypassed (or poisoned) by the parent shell.
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env("RUSTFLAGS", "-D warnings")
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run cargo");
    if !output.status.success() {
        panic!(
            "cargo run failed in {}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- generated ---\n{}",
            dir.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            &driver_src,
        );
    }
    parse_envelope(&String::from_utf8_lossy(&output.stdout))
}

/// Split the driver's stdout into per-program outcomes. Uses the
/// sentinel markers emitted by `build_driver`: each program starts
/// with `<<TEST>>name\n`, prints are framed `<<PRINT>>msg<<END>>`,
/// and the program ends with either `<<OK>>` or
/// `<<ERR>>msg<<END>>`.
fn parse_envelope(stdout: &str) -> Vec<(String, Outcome)> {
    let mut out = Vec::new();
    let mut lines = stdout.lines().peekable();
    while let Some(line) = lines.next() {
        let name = match line.strip_prefix("<<TEST>>") {
            Some(n) => n.to_string(),
            None => continue,
        };
        let mut prints = Vec::new();
        let mut error = None;
        loop {
            let next = match lines.next() {
                Some(l) => l,
                None => break,
            };
            if let Some(p) = next.strip_prefix("<<PRINT>>") {
                let p = p.strip_suffix("<<END>>").unwrap_or(p);
                prints.push(p.to_string());
            } else if next == "<<OK>>" {
                break;
            } else if let Some(err) = next.strip_prefix("<<ERR>>") {
                let err = err.strip_suffix("<<END>>").unwrap_or(err);
                error = Some(err.to_string());
                break;
            }
            // Anything else is treated as garbage and skipped.
        }
        out.push((name, Outcome { prints, error }));
    }
    out
}

// ─── The corpus ───────────────────────────────────────────────────
//
// A focused set of programs spanning every feature the AOT supports.
// Kept intentionally small so the AOT leg stays under ~10s (cold
// cargo build dominates; parsing + asserting is negligible).
//
// Safety / tight-limit tests are deliberately *not* included here:
// the walker, VM, and AOT sandbox each measure "steps" differently
// (per-statement vs per-bytecode vs per-tick-point), so they reach
// resource limits at slightly different points even though all
// three halt. That divergence is already documented in the 2c
// harness's `assert_both_resource_limit`, and extending it to three
// engines would muddy the strict equality guarantee this harness
// offers on the happy path.

const CORPUS: &[(&str, &str)] = &[
    (
        "asi_multiline_delimiters",
        r#"fn add(a, b) { return a + b }
let values = [
    1,
    add(
        2,
        3
    ),
    [
        6,
    ][
        0
    ],
]
let config = {
    "target": values[
        1
    ],
    "label": "bop"
}
if (
    config[
        "target"
    ] == 5 && values.len() == 3
) {
    print(
        values[0] +
        values[1] +
        values[2]
    )
}
let length = values
    // Leading-dot continuation may cross comments and blank lines.

    .len()
    .to_str()
print(length)"#,
    ),
    (
        "asi_nested_lambda_braces",
        r#"let functions = [
    fn() {
        let x = 1
        let y = 2
        return x + y
    },
]
let wrapped = (fn() {
    let x = 4
    let y = 5
    return x + y
})
print(functions[0]() + wrapped())"#,
    ),
    (
        "asi_return_newline",
        r#"fn bare() {
    return
    42
}
fn grouped() {
    return (
        42
    )
}
print(bare())
print(grouped())"#,
    ),
    ("arithmetic", "print(1 + 2 * 3 - 4)"),
    ("divide_float", "print(7 / 2)"),
    ("modulo", "print(10 % 3)"),
    ("unary_neg", "print(-5)"),
    ("unary_not", "print(!true)"),
    ("string_concat", r#"print("hello" + " " + "world")"#),
    ("string_repeat", r#"print("ab" * 3)"#),
    ("string_auto_coerce", r#"print("val=" + 42)"#),
    (
        "string_interpolation",
        r#"let name = "bop"
let version = 2
print("hi {name} v{version}!")"#,
    ),
    (
        "aot_identifier_hygiene",
        r#"fn subtract(a, b) { return a - b }
fn yield(crate, super, ctx) { return crate + super + ctx }
struct Holder { n }
fn Holder.read(self) {
    let bop_self = 40
    return self.n + bop_self
}
let __t0 = 1
let __t1 = 2
let __l = 4
let ctx = 3
let crate = 5
let super = 6
let x = 10
let __bop_user_value_78 = 20
let holder = Holder { n: 2 }
print(subtract(__t1, __t0))
print(1 + __l)
print(yield(crate, super, ctx))
print(x, __bop_user_value_78)
print(holder.read())"#,
    ),
    (
        "string_interpolation_function_locals",
        r#"fn greet(name) {
    let punctuation = "!"
    return "hi {name}{punctuation}"
}
print(greet("bop"))"#,
    ),
    (
        "string_interpolation_nested_closure_captures",
        r#"fn build(prefix) {
    let local = "local"
    return fn(suffix) {
        return fn() { return "{prefix}:{local}:{suffix}" }
    }
}
print(build("start")("end")())"#,
    ),
    ("equality", "print(1 == 1)\nprint(1 == 2)\nprint(1 != 2)"),
    (
        "ordering",
        "print(3 < 5)\nprint(5 <= 5)\nprint(6 > 5)\nprint(5 >= 6)",
    ),
    (
        "logical",
        // Note: the tree-walker accepts `false && x` / `true || x`
        // with an unbound `x` thanks to short-circuiting — the
        // right side is never evaluated. The AOT path compiles to
        // Rust, which resolves every identifier at compile time
        // regardless of dynamic reachability, so that construct is
        // a legitimate AOT divergence and is intentionally omitted
        // from the three-way corpus.
        "print(true && false)\nprint(true || false)\nprint(false && true)\nprint(true || false)",
    ),
    ("let_and_assign", "let x = 1\nx = 5\nprint(x)"),
    (
        "compound_assign",
        r#"let x = 10
x += 5
x -= 3
x *= 2
x /= 4
x %= 3
print(x)"#,
    ),
    (
        "named_container_assignment_order",
        r#"let values = [1, 2]
values[0] += values.remove(0)
print(values)
let dict = {"n": 4}
dict["n"] += 6
dict["extra"] = 8
print(dict)
struct Counter { n }
let counter = Counter { n: 3 }
counter.n *= 4
print(counter.n)"#,
    ),
    (
        "if_else_if",
        r#"let x = 2
if x == 1 { print("one") } else if x == 2 { print("two") } else { print("other") }"#,
    ),
    ("if_expression", "let x = if true { 1 } else { 2 }\nprint(x)"),
    (
        "if_expression_multiline_layout",
        r#"let first = if true {
    // Comments and blank lines remain layout.

    1 +
        2;
}
else {
    99
}
let second = if false {
    0
} else {
    if true {
        4
    }
    else {
        5
    }
}
print(first)
print(second)
let third = if true {
    (
        5
        + 6
    )
} else {
    0
}
print(third)"#,
    ),
    (
        "struct_literals_in_condition_delimiters",
        r#"struct Point { x, y }
enum Maybe { Some(value) }
fn get_x(point) { return point.x }
fn Point.same_x(self, other) { return self.x == other.x }
let choices = [false, true]
if get_x(Point { x: 1, y: 2 }) == 1 { print("call") }
if (Point { x: 2, y: 0 }).x == 2 { print("paren") }
if (Point { x: 3, y: 0 }).same_x(Point { x: 3, y: 1 }) { print("method-arg") }
if choices[Point { x: 1, y: 0 }.x] { print("index") }
if [Point { x: 4, y: 0 }][0].x == 4 { print("array") }
if {"point": Point { x: 5, y: 0 }}["point"].x == 5 { print("dict") }
if Ok(Point { x: 6, y: 0 }).is_ok() { print("result") }
if match Maybe::Some(Point { x: 7, y: 0 }) {
    Maybe::Some(point) => point.x,
} == 7 { print("enum-tuple") }
if match 1 {
    value if Point { x: value, y: 0 }.x == 1 => Point { x: 8, y: 0 }.x,
    _ => 0,
} == 8 { print("match") }
if (if true { Point { x: 9, y: 0 }.x } else { 0 }) == 9 { print("if-expr") }
if fn() { return Point { x: 10, y: 0 }.x }() == 10 { print("lambda") }
let count = 0
while get_x(Point { x: count, y: 0 }) < 1 { count += 1 }
print("while")
for point in [Point { x: 11, y: 0 }] { print(point.x) }
repeat [Point { x: 12, y: 0 }].len() { print("repeat") }"#,
    ),
    (
        "while_loop",
        "let i = 0\nwhile i < 5 { i += 1 }\nprint(i)",
    ),
    (
        "while_break",
        "let i = 0\nwhile true { i += 1\nif i == 3 { break } }\nprint(i)",
    ),
    (
        "while_continue",
        r#"let sum = 0
let i = 0
while i < 10 {
    i += 1
    if i % 2 == 0 { continue }
    sum += i
}
print(sum)"#,
    ),
    (
        "for_over_array",
        r#"let sum = 0
for x in [10, 20, 30] { sum += x }
print(sum)"#,
    ),
    (
        "for_over_range",
        "let sum = 0\nfor i in range(5) { sum += i }\nprint(sum)",
    ),
    (
        "for_over_string",
        r#"let out = ""
for ch in "abc" { out += ch + "-" }
print(out)"#,
    ),
    ("repeat_loop", "let n = 0\nrepeat 4 { n += 1 }\nprint(n)"),
    ("repeat_zero", "let n = 99\nrepeat 0 { n = 0 }\nprint(n)"),
    (
        "fn_basic",
        "fn double(x) { return x * 2 }\nprint(double(5))",
    ),
    (
        "fn_recursion",
        r#"fn fib(n) {
    if n <= 1 { return n }
    return fib(n - 1) + fib(n - 2)
}
print(fib(10))"#,
    ),
    (
        "nested_fn_calls",
        r#"fn square(n) { return n * n }
fn sum_squares(a, b) { return square(a) + square(b) }
print(sum_squares(3, 4))"#,
    ),
    (
        "array_literal_index",
        "let a = [10, 20, 30]\nprint(a[1])\nprint(a[-1])",
    ),
    (
        "array_mutation",
        r#"let a = [1, 2]
a.push(3)
a.push(4)
print(a.len())
print(a)
let last = a.pop()
print(last)
print(a)"#,
    ),
    (
        "array_mutation_value_semantics",
        r#"let original = [1, 2]
let alias = original
original.push(3)
print(original)
print(alias)
let nested = [1, 2]
nested.push(nested.pop())
print(nested)
let transient_source = [7]
(if true { transient_source } else { [] }).push(8)
[9].push(10)
print(transient_source)"#,
    ),
    (
        "array_large_append_loop",
        r#"let values = []
let next = 0
repeat 2048 {
    values.push(next)
    next += 1
}
print(values.len())
print(values[0])
print(values[-1])"#,
    ),
    (
        "array_mutation_methods_and_returns",
        r#"let values = [4, 1, 3]
print(values.push(2))
print(values.insert(1, 5))
print(values.remove(2))
print(values.pop())
values.sort()
values.reverse()
print(values)"#,
    ),
    (
        "array_mutation_errors_are_atomic",
        r#"let values = [1, 2, 3]
print(try_call(fn() { return values.push() }).is_err())
print(try_call(fn() { return values.insert(99, 4) }).is_err())
print(try_call(fn() { return values.remove(99) }).is_err())
print(values)"#,
    ),
    (
        "dynamic_struct_method_named_push",
        r#"struct Accumulator { total }
fn Accumulator.push(self, value) { return self.total + value }
let accumulator = Accumulator { total: 7 }
print(accumulator.push(5))"#,
    ),
    (
        "array_sort_reverse",
        r#"let a = [3, 1, 2]
a.sort()
print(a)
a.reverse()
print(a)"#,
    ),
    (
        "array_index_assign",
        r#"let a = [1, 2, 3]
a[0] = 99
a[1] += 10
a[-1] *= 2
print(a)"#,
    ),
    (
        "dict_basics",
        r#"let d = {"name": "bop", "hp": 100}
print(d["name"])
d["hp"] = 50
d["mp"] = 20
print(d["hp"])
print(d.keys())"#,
    ),
    (
        "string_methods",
        r#"print("Hello".upper())
print("HI".lower())
print("  trim  ".trim())
print("hello".len())
print("a,b,c".split(","))
print(["x", "y", "z"].join("-"))"#,
    ),
    (
        "fizzbuzz",
        r#"let result = []
for i in range(1, 16) {
    if i % 15 == 0 {
        result.push("FizzBuzz")
    } else if i % 3 == 0 {
        result.push("Fizz")
    } else if i % 5 == 0 {
        result.push("Buzz")
    } else {
        result.push(i.to_str())
    }
}
print(result.join(", "))"#,
    ),
    ("builtin_str_int_type", "print(42.to_str())\nprint(3.7.to_int())\nprint([].type())"),
    ("builtin_abs_min_max", "print((-5).abs())\nprint(3.min(7))\nprint(3.max(7))"),
    ("builtin_range", "print(range(5))\nprint(range(2, 5))\nprint(range(0, 10, 3))"),
    (
        "range_limit_boundary",
        "let values = range(10000)\nprint(values.len())\nprint(values[9999])",
    ),
    (
        "range_limit_error",
        r#"let result = try_call(fn() {
    return range(10001)
})
print("unreachable")"#,
    ),
    ("builtin_len_inspect", r#"print("hello".len())
print([1, 2, 3].len())
print("hi".inspect())"#),
    (
        "signed_index_methods",
        r#"let values = [10, 20, 30, 40]
print(values.remove(-1))
print(values.insert(-1, 25))
print(values)
print(values.slice(-3, -1))
print("a🙂é界"[-1])
print("a🙂é界".slice(-3, -1))
print(values[1.9])"#,
    ),
    (
        "signed_index_failures_are_nonfatal",
        r#"let values = [10, 20, 30]
print(try_call(fn() { return values.remove(-4) }).is_err())
print(try_call(fn() { return values.insert(-4, 0) }).is_err())
print(try_call(fn() { values[-4] = 0 }).is_err())
print(values)"#,
    ),
    (
        "nested_array_mutation_is_catchable",
        r#"struct Holder { items }
let indexed = {"items": [1]}
let fielded = Holder { items: [1, 2] }
let index_result = try_call(fn() {
    indexed["items"].push(2)
})
let field_result = try_call(fn() {
    fielded.items.pop()
})
print(match index_result { Result::Err(e) => e.message, _ => "missing" })
print(match index_result { Result::Err(e) => e.line, _ => -1 })
print(match field_result { Result::Err(e) => e.message, _ => "missing" })
print(match field_result { Result::Err(e) => e.line, _ => -1 })"#,
    ),
    (
        "nested_array_mutation_index_error",
        r#"let indexed = {"items": [1]}
indexed["items"].push(2)"#,
    ),
    (
        "nested_array_mutation_field_error",
        r#"struct Holder { items }
let fielded = Holder { items: [1, 2] }
fielded.items.pop()"#,
    ),
    (
        "temporary_array_mutation_and_dynamic_method_fallback",
        r#"fn make_array() { return [7] }
print([1].push(2))
print(make_array().pop())
struct Gadget { n }
fn Gadget.push(self, amount) { return self.n + amount }
struct Wrapper { item }
let wrapper = Wrapper { item: Gadget { n: 10 } }
let dynamic = {"item": Gadget { n: 20 }}
print(wrapper.item.push(2))
print(dynamic["item"].push(3))"#,
    ),
    (
        "signed_index_i64_extremes",
        r#"let min = -9223372036854775807 - 1
let max = 9223372036854775807
let values = [1, 2]
print(values.slice(min, max))
print(values.slice(max, min))
print(try_call(fn() { return values[min] }).is_err())
print(try_call(fn() { return values.remove(min) }).is_err())
print(try_call(fn() { return values.insert(max, 3) }).is_err())
print(values)"#,
    ),
    ("signed_index_bounds_error", "[1, 2].remove(-3)"),
    ("signed_index_type_error", r#"[1].remove("0")"#),
    ("nested_array_access", "let m = [[1, 2], [3, 4]]\nprint(m[1][0])"),
    ("method_chain", r#"print("  HELLO  ".trim().lower())"#),
    (
        "truthiness",
        r#"print(if 0 { "t" } else { "f" })
print(if "" { "t" } else { "f" })
print(if [1] { "t" } else { "f" })
print(if [] { "t" } else { "f" })"#,
    ),
    ("number_display", "print(5.0)\nprint(3.14)\nprint(0.1 + 0.2)"),
    (
        "error_division_by_zero",
        "print(1 / 0)",
    ),
    (
        "error_type_mismatch",
        r#"print("a" - 1)"#,
    ),
    ("error_unknown_fn", "nope()"),
    (
        // `panic` is a builtin; in walker / VM / AOT it has to
        // produce the same `Err` payload when caught by
        // `try_call`, matching down to the error message.
        "builtin_panic_via_try_call",
        r#"let r = try_call(fn() { panic("nope") })
print(match r {
    Result::Ok(_)  => "ok?",
    Result::Err(e) => e.message,
})"#,
    ),
    // NOTE: `print(nope)` — an undefined identifier — is *not*
    // included: the walker raises "Variable `nope` not found" at
    // runtime, but the AOT emits `nope.clone()` which rustc
    // rejects at compile time with a different message. Both halt
    // with a useful error; the three-way harness just can't
    // phrase the assertion as "same message text".
    ("error_array_oob", "let a = [1]\nprint(a[5])"),
    ("error_bare_panic", "panic(\"top level\")"),
    // ─── Closures / first-class fns (phase 1) ─────────────────
    (
        "closure_basic_lambda",
        r#"let double = fn(x) { return x * 2 }
print(double(5))
print(double(21))"#,
    ),
    (
        "lambda_parameter_binding_semantics",
        r#"fn named(value, value) { return value }
struct Holder { n }
fn Holder.pick(self, value, value) { return self.n + value }
fn make(value) { return fn(value) { return value } }
let outer = 40
let closure = fn(_ignored, value, value) { return outer + value }
print(closure(1, 2, 3))
print(named(4, 5))
print(Holder { n: 6 }.pick(7, 8))
print(make(9)(10))"#,
    ),
    (
        "closure_captures_value",
        r#"let n = 5
let add_n = fn(x) { return x + n }
print(add_n(3))
n = 100
print(add_n(3))"#,
    ),
    (
        "closure_factory",
        r#"fn make_adder(n) { return fn(x) { return x + n } }
let add5 = make_adder(5)
let add10 = make_adder(10)
print(add5(3))
print(add10(3))"#,
    ),
    (
        "named_fn_as_first_class_value",
        r#"fn double(x) { return x * 2 }
let f = double
print(f(7))"#,
    ),
    (
        "higher_order_apply",
        r#"fn apply(f, x) { return f(x) }
fn square(n) { return n * n }
print(apply(square, 4))
print(apply(fn(n) { return n + 1 }, 4))"#,
    ),
    (
        "fn_in_array_indexed_call",
        r#"fn add(x, y) { return x + y }
fn mul(x, y) { return x * y }
let ops = [add, mul]
print(ops[0](2, 3))
print(ops[1](2, 3))"#,
    ),
    ("iife_value_call", "print((fn(x) { return x * 3 })(4))"),
    ("type_of_fn_is_fn", "fn f() { }\nprint(f.type())"),
    // ─── Structs / enums / user methods (phase 3) ───────────────
    (
        "struct_basic",
        r#"struct Point { x, y }
let p = Point { x: 3, y: 4 }
print(p.x + p.y)
print(p)"#,
    ),
    (
        "struct_field_assign",
        r#"struct Counter { n }
let c = Counter { n: 10 }
c.n += 5
c.n *= 2
print(c.n)"#,
    ),
    (
        "struct_equality",
        r#"struct P { x, y }
let a = P { x: 1, y: 2 }
let b = P { x: 1, y: 2 }
print(a == b)"#,
    ),
    (
        "enum_unit_and_tuple",
        r#"enum E { A, B(n) }
print(E::A == E::A)
print(E::B(1) == E::B(1))
print(E::B(1) == E::B(2))"#,
    ),
    (
        "enum_struct_variant",
        r#"enum Shape { Rect { w, h } }
let r = Shape::Rect { w: 4, h: 3 }
print(r.w * r.h)
print(r)"#,
    ),
    (
        "method_on_struct",
        r#"struct Point { x, y }
fn Point.sum(self) { return self.x + self.y }
let p = Point { x: 3, y: 4 }
print(p.sum())"#,
    ),
    (
        "method_chain_user",
        r#"struct Adder { n }
fn Adder.then(self, m) { return Adder { n: self.n + m } }
let r = Adder { n: 1 }.then(2).then(3).then(4)
print(r.n)"#,
    ),
    (
        "method_on_enum",
        r#"enum Shape { Circle(r), Rect { w, h } }
fn Shape.label(self) { return "shape" }
print(Shape::Circle(5).label())
print(Shape::Rect { w: 4, h: 3 }.label())"#,
    ),
    (
        "method_overrides_builtin",
        r#"struct Wrapper { data }
fn Wrapper.len(self) { return 99 }
let w = Wrapper { data: [1, 2, 3] }
print(w.len())"#,
    ),
    // ─── Pattern matching (phase 4) ─────────────────────────────
    (
        "match_literal_number",
        r#"let x = 2
print(match x {
  1 => "one",
  2 => "two",
  _ => "other",
})"#,
    ),
    (
        "match_wildcard_catches_all",
        r#"let x = 42
print(match x {
  0 => "zero",
  _ => "big",
})"#,
    ),
    (
        "match_binding_captures",
        r#"let x = 7
print(match x { n => n * 2 })"#,
    ),
    (
        "match_guard_selects_arm",
        r#"let x = 10
print(match x {
  n if n < 5 => "small",
  n if n < 20 => "medium",
  _ => "big",
})"#,
    ),
    (
        "match_or_pattern",
        r#"let x = 3
print(match x {
  1 | 2 | 3 => "low",
  _ => "other",
})"#,
    ),
    (
        "match_or_pattern_complex_bindings",
        r#"struct Node { left, right }
enum Boxed { Pair(left, right), Record { first, second } }
let values = [
    Node { left: 1, right: 2 },
    Boxed::Pair(3, 4),
    Boxed::Record { first: 5, second: 6 },
]
for value in values {
    print(match value {
        Node { left, right } | Boxed::Pair(right, left) | Boxed::Record { first: left, second: right } => left * 10 + right,
    })
}
enum Packet { Values(items, marker) }
let packets = [Packet::Values([1, 2, 3], 9), Packet::Values([7], 8)]
for packet in packets {
    print(match packet {
        Packet::Values([_, head, ..tail] | [head, ..tail], marker) => head * 100 + marker * 10 + tail.len(),
    })
}
for items in [[1, 2], [3]] {
    print(match items { [x, x] | [x] => x })
}"#,
    ),
    (
        "match_enum_unit",
        r#"enum Light { Red, Green }
let l = Light::Green
print(match l {
  Light::Red => "stop",
  Light::Green => "go",
})"#,
    ),
    (
        "match_enum_tuple_binds",
        r#"enum Res { Ok(v), Err(m) }
let r = Res::Ok(42)
print(match r {
  Res::Ok(v) => v,
  Res::Err(_) => -1,
})"#,
    ),
    (
        "match_enum_struct_variant_binds",
        r#"enum Shape { Rect { w, h } }
let s = Shape::Rect { w: 4, h: 3 }
print(match s { Shape::Rect { w, h } => w * h })"#,
    ),
    (
        "match_struct_destructure",
        r#"struct Point { x, y }
let p = Point { x: 3, y: 4 }
print(match p { Point { x, y } => x + y })"#,
    ),
    (
        "match_struct_partial_rest",
        r#"struct Point { x, y, z }
let p = Point { x: 1, y: 2, z: 3 }
print(match p { Point { y, .. } => y * 10 })"#,
    ),
    (
        "match_nested_enum_struct",
        r#"enum FileError { NotFound(path) }
enum Res { Ok(v), Err(e) }
let r = Res::Err(FileError::NotFound("missing.txt"))
print(match r {
  Res::Ok(_) => "ok",
  Res::Err(FileError::NotFound(p)) => p,
})"#,
    ),
    (
        "match_array_exact",
        r#"let xs = [1, 2, 3]
print(match xs {
  [a, b, c] => a + b + c,
  _ => -1,
})"#,
    ),
    (
        "match_array_with_rest",
        r#"let xs = [10, 20, 30, 40]
print(match xs {
  [first, ..rest] => first,
  _ => -1,
})"#,
    ),
    (
        "match_no_arm_matched_errors",
        r#"let x = 99
match x {
  1 => print("one"),
  2 => print("two"),
}"#,
    ),
    // ─── `try` operator (phase 5) ────────────────────────────────
    (
        "try_unwraps_ok",
        r#"enum Result { Ok(v), Err(e) }
fn doit() {
    let v = try Result::Ok(42)
    return v
}
print(doit())"#,
    ),
    (
        "try_propagates_err",
        r#"enum Result { Ok(v), Err(e) }
fn doit() {
    let v = try Result::Err("boom")
    return Result::Ok(v)
}
let r = doit()
print(match r {
    Result::Ok(v) => v,
    Result::Err(e) => e,
})"#,
    ),
    (
        "try_chains_through_nested_calls",
        r#"enum Result { Ok(v), Err(e) }
fn leaf() { return Result::Err("leaf-err") }
fn middle() {
    let v = try leaf()
    return Result::Ok(v + 1)
}
fn top() {
    let v = try middle()
    return Result::Ok(v * 2)
}
print(match top() {
    Result::Ok(v) => v,
    Result::Err(e) => e,
})"#,
    ),
    // `try_ok_unit_variant_yields_none` used to live here — it
    // redeclared `Result` with a Unit `Ok` variant and relied on
    // the walker silently accepting it. Now that `Result` is an
    // engine builtin with the canonical `Ok(value)` shape, a
    // redeclaration with a different shape is a hard error in
    // all three engines. The per-engine unit tests cover the new
    // behaviour; there's nothing to diff here.
    (
        "try_inside_lambda_returns_from_lambda",
        r#"enum Result { Ok(v), Err(e) }
let f = fn() {
    let v = try Result::Err("inner")
    return Result::Ok(v)
}
let r = f()
print(match r {
    Result::Ok(_) => "ok",
    Result::Err(e) => e,
})"#,
    ),
    (
        "try_in_for_loop_short_circuits",
        r#"enum Result { Ok(v), Err(e) }
fn lookup(i) {
    if i == 2 { return Result::Err("stop") }
    return Result::Ok(i * 10)
}
fn sum_until_err() {
    let total = 0
    for i in range(5) {
        let v = try lookup(i)
        total = total + v
    }
    return Result::Ok(total)
}
print(match sum_until_err() {
    Result::Ok(v) => v,
    Result::Err(e) => e,
})"#,
    ),
    (
        "try_on_non_result_errors",
        r#"fn doit() {
    let v = try 42
    return v
}
doit()"#,
    ),
    (
        "try_top_level_on_err_errors",
        r#"enum Result { Ok(v), Err(e) }
let r = try Result::Err("boom")"#,
    ),
    // ─── Integer type (phase 6) ─────────────────────────────────
    (
        "int_literal_type",
        r#"print(42.type())
print(42.0.type())
print((-3).type())"#,
    ),
    (
        "int_arithmetic_stays_int",
        r#"print(1 + 2)
print((1 + 2).type())
print(10 - 4)
print(3 * 4)"#,
    ),
    (
        "division_always_number_int_via_cast",
        r#"print(10 / 3)
print((10 / 3).type())
print((10 / 3).to_int())
print((10 / 3).to_int().type())
print((-7 / 2).to_int())"#,
    ),
    (
        "int_number_mixed_widens",
        r#"print(1 + 2.0)
print((1 + 2.0).type())
print(3 * 0.5)"#,
    ),
    (
        "int_number_equality_is_numeric",
        r#"print(1 == 1.0)
print(2 > 1.5)"#,
    ),
    (
        "division_by_zero_errors",
        "print(10 / 0)",
    ),
    (
        "int_overflow_add_errors",
        "print(9223372036854775807 + 1)",
    ),
    (
        "int_builtin_and_float_builtin",
        r#"print(3.7.to_int())
print(3.7.to_int().type())
print(42.to_float())
print(42.to_float().type())"#,
    ),
    (
        "len_returns_int",
        r#"print("hi".len().type())
print([1, 2, 3].len().type())"#,
    ),
    (
        "range_int_elements",
        r#"let r = range(3)
print(r[0].type())"#,
    ),
    (
        "int_match_literal",
        r#"let x = 2
print(match x {
    1 => "one",
    2 => "two",
    _ => "other",
})"#,
    ),
    (
        "repeat_accepts_int",
        r#"let n = 0
repeat 5 { n = n + 1 }
print(n)"#,
    ),
    // ─── `try_call` builtin ─────────────────────────────────────
    (
        "try_call_wraps_ok",
        r#"let r = try_call(fn() { return 42 })
print(match r {
    Result::Ok(v) => v,
    Result::Err(_) => -1,
})"#,
    ),
    (
        "try_call_wraps_non_fatal_err",
        r#"let r = try_call(fn() { return 1 / 0 })
print(match r {
    Result::Ok(_) => "ok",
    Result::Err(e) => e.message,
})"#,
    ),
    (
        "try_call_err_carries_line",
        r#"let r = try_call(fn() {
    let x = 1
    return x / 0
})
print(match r {
    Result::Ok(_) => -1,
    Result::Err(e) => e.line,
})"#,
    ),
    (
        "try_call_composes_with_try_operator",
        r#"fn risky(x) {
    let arr = [1, 2]
    return arr[x]
}
let r = try_call(fn() { return risky(5) })
print(match r {
    Result::Ok(_) => "ok",
    Result::Err(e) => e.message,
})"#,
    ),
    (
        "try_call_wrong_arg_count_errors",
        "try_call()",
    ),
    (
        "try_call_non_function_errors",
        "try_call(42)",
    ),
    (
        "try_call_nested_outer_catches_inner_err_as_ok",
        r#"let r = try_call(fn() {
    let inner = try_call(fn() { return 1 / 0 })
    return inner
})
print(match r {
    Result::Ok(Result::Err(e)) => e.message,
    Result::Ok(Result::Ok(_)) => "inner ok?",
    Result::Err(_) => "outer caught",
})"#,
    ),
];

/// Programs that exercise the `use` surface. Each entry
/// pairs source with a module map the walker, VM, and AOT all
/// resolve against. AOT's compile-time resolver is seeded from
/// this same map via `modules_from_map`.
const IMPORTS_CORPUS: &[(&str, &str, &[(&str, &str)])] = &[
    (
        "import_basic_let",
        r#"use math
print(pi)"#,
        &[("math", "let pi = 3")],
    ),
    (
        "import_named_fn",
        r#"use math
print(square(7))"#,
        &[("math", "fn square(n) { return n * n }")],
    ),
    (
        "import_dotted_path",
        r#"use std.math
print(e)"#,
        &[("std.math", "let e = 2")],
    ),
    (
        "import_dot_underscore_slug_collision",
        r#"use a.b as dotted
use a_b as underscored
print(dotted.helper(), dotted.ctx, underscored.helper(), underscored.yield)"#,
        &[
            ("a.b", "let ctx = 10\nfn helper() { return 1 }"),
            ("a_b", "let yield = 20\nfn helper() { return 2 }"),
        ],
    ),
    (
        "import_transitive",
        r#"use a
print(doubled)"#,
        &[
            ("a", "use b\nlet doubled = pi + pi"),
            ("b", "let pi = 3"),
        ],
    ),
    (
        "import_shared_dependency_diamond",
        r#"use left
use right
print(one, two)"#,
        &[
            (
                "shared",
                "print(\"shared init\")\nfn helper(n) { return n + 10 }",
            ),
            ("left", "use shared\nlet one = helper(1)"),
            ("right", "use shared\nlet two = helper(2)"),
        ],
    ),
    (
        "import_shared_dependency_alias_diamond",
        r#"use left as l
use right as r
print(l.one, r.two)"#,
        &[
            (
                "shared",
                "print(\"shared alias init\")\nfn helper(n) { return n + 20 }",
            ),
            ("left", "use shared\nlet one = helper(1)"),
            ("right", "use shared\nlet two = helper(2)"),
        ],
    ),
    (
        "import_alias_only_function_surface",
        r#"use internal as module
print(module.twice(3))
print(module.recurse(4))
let returned = module.twice
print(returned(5))
print(module.closure(6))
print(module.Thing { value: 7 }.bump())
print(module._private)"#,
        &[(
            "internal",
            r#"fn helper(n) { return n + 1 }
fn twice(n) { return helper(n) + helper(n) }
fn recurse(n) {
    if n == 0 { return 0 }
    return 1 + recurse(n - 1)
}
let closure = fn(n) { return helper(n) }
struct Thing { value }
fn Thing.bump(self) { return helper(self.value) }
let _private = 99"#,
        )],
    ),
    (
        "import_alias_rejects_bare_function",
        r#"use internal as module
print(module.helper(1))
print(helper(2))"#,
        &[("internal", "fn helper(n) { return n + 1 }")],
    ),
    (
        "import_nested_alias_rejects_bare_function",
        r#"use wrapper
print(via_alias)"#,
        &[
            ("shared", "fn helper(n) { return n + 1 }"),
            (
                "wrapper",
                "use shared as dep\nlet via_alias = dep.helper(3)\nlet via_bare = helper(4)",
            ),
        ],
    ),
    (
        "import_selective_glob_mix",
        r#"use mixed as module
print(module.total)
print(module._private)
print(module.helper(5))"#,
        &[
            (
                "shared",
                "let public = 10\nlet _private = 2\nfn helper(n) { return n + 1 }",
            ),
            (
                "mixed",
                "use shared.{_private}\nuse shared\nlet total = _private + public",
            ),
        ],
    ),
    (
        "import_alias_of_alias_hygiene",
        r#"use facade as outer
print(outer.layer.ctx.helper(4))"#,
        &[
            ("shared", "fn helper(n) { return n + 1 }"),
            ("layer", "use shared as ctx"),
            ("facade", "use layer as layer"),
        ],
    ),
    (
        "import_idempotent_cache",
        r#"use m
use m
print(x)"#,
        &[("m", "let x = 42")],
    ),
    (
        "import_nested_every_runtime_body",
        r#"fn function_use() {
    use nested_shared as unused_alias
    use nested_shared.{inc}
    return inc(1)
}
fn branch_use(which) {
    if which == 1 {
        use nested_shared
        return inc(2)
    } else if which == 2 {
        use nested_shared.{inc}
        return inc(3)
    } else {
        use nested_shared
        return inc(4)
    }
}
fn loop_uses() {
    let total = 0
    while total == 0 {
        use nested_shared
        total = inc(4)
    }
    repeat 1 {
        use nested_shared.{inc}
        total += inc(5)
    }
    for item in [6] {
        use nested_shared
        total += inc(item)
    }
    return total
}
struct Holder { value }
fn Holder.load(self) {
    use nested_shared
    return inc(self.value)
}
let lambda_use = fn() {
    use nested_shared.{inc}
    return inc(7)
}
let match_lambda_use = match 1 {
    1 => fn() {
        use nested_shared
        return inc(8)
    },
    _ => fn() { return 0 },
}
print(function_use())
print(branch_use(1), branch_use(2), branch_use(3))
print(loop_uses())
print(Holder { value: 6 }.load())
print(lambda_use(), match_lambda_use())"#,
        &[("nested_shared", "fn inc(n) { return n + 1 }")],
    ),
    (
        "import_nested_inside_imported_module",
        r#"use wrapper as wrapper_module
print(wrapper_module.run())"#,
        &[
            (
                "wrapper",
                "fn run() { use leaf.{value}; return value }",
            ),
            ("leaf", "let value = 42"),
        ],
    ),
    (
        "import_lazy_edge_is_not_eager_cycle",
        r#"use lazy_a as a
print(a.value)
print(a.load())"#,
        &[
            (
                "lazy_a",
                "let value = 10\nfn load() { use lazy_b; return answer() }",
            ),
            ("lazy_b", "use lazy_a as parent\nfn answer() { return 32 }"),
        ],
    ),
    // ─── Result methods (engine-level, no `use` required) ────
    (
        "result_method_helpers",
        r#"print(Result::Ok(1).is_ok())
print(Result::Err("x").is_err())
print(Result::Err("x").unwrap_or(42))"#,
        &[],
    ),
    (
        "result_method_map",
        r#"let r = Result::Ok(5).map(fn(x) { return x * 2 })
print(match r { Result::Ok(v) => v, Result::Err(_) => -1 })"#,
        &[],
    ),
    (
        "result_method_map_err",
        r#"let r = Result::Err("bad").map_err(fn(e) { return e + "!" })
print(match r { Result::Err(v) => v, Result::Ok(_) => "ok?" })"#,
        &[],
    ),
    (
        "result_method_and_then",
        r#"fn halve(x) {
    if x % 2 == 0 { return Result::Ok((x / 2).to_int()) }
    return Result::Err("odd")
}
let r = Result::Ok(8).and_then(halve).and_then(halve)
print(match r { Result::Ok(v) => v, Result::Err(_) => -1 })"#,
        &[],
    ),
    // ─── Dict missing-key soft lookup ─────────────────────────
    (
        "dict_missing_key_returns_none",
        // AOT emits `ops::index_get` inline, so the soft-lookup
        // result has to match walker / VM exactly.
        r#"let d = {"hp": 10}
print(d["hp"])
print(d["missing"])
print(d["missing"].is_none())"#,
        &[],
    ),
    // ─── is_none / is_some universal methods ─────────────────
    (
        "is_none_basic_dispatch",
        // Has to work uniformly across every receiver shape so
        // walker / VM / AOT must agree per-type.
        r#"print(none.is_none())
print((0).is_none())
print("".is_none())
print([].is_none())
print(false.is_none())
print(none.is_some())
print((42).is_some())"#,
        &[],
    ),
    (
        "is_none_with_optional_return",
        r#"fn maybe(n) {
    if n < 0 { return none }
    return n
}
let a = maybe(-1)
let b = maybe(7)
if a.is_none() { print("a is none") }
if b.is_some() { print("b = " + b.to_str()) }"#,
        &[],
    ),
    // ─── Ok / Err shorthand ───────────────────────────────────
    (
        "ok_err_shorthand_expression",
        // Parser-level sugar — the resulting `Value` must match
        // `Result::Ok(...)` / `Result::Err(...)` byte-for-byte
        // across walker / VM / AOT.
        r#"print(Ok(1))
print(Err("x"))
print(Ok(1) == Result::Ok(1))
print(Err("x") == Result::Err("x"))"#,
        &[],
    ),
    (
        "ok_err_shorthand_pattern",
        r#"fn describe(r) {
    return match r {
        Ok(v)  => "ok: " + v.to_str(),
        Err(e) => "err: " + e,
    }
}
print(describe(Ok(5)))
print(describe(Err("stop")))"#,
        &[],
    ),
    // ─── Iterator protocol ────────────────────────────────────
    (
        "iter_basic_next_sequence",
        r#"let it = [10, 20, 30].iter()
print(it.next())
print(it.next())
print(it.next())
print(it.next())"#,
        &[],
    ),
    (
        "iter_for_over_value_iter",
        r#"let total = 0
for x in [1, 2, 3, 4, 5].iter() { total = total + x }
print(total)"#,
        &[],
    ),
    (
        "iter_for_over_user_container",
        // User wrapper delegates `.iter()` to a backing array —
        // exercises the full protocol path in every engine.
        r#"struct Bag { items }
fn bag_of(arr) { return Bag { items: arr } }
fn Bag.iter(self) { return self.items.iter() }

let b = bag_of([10, 20, 30])
let sum = 0
for v in b { sum = sum + v }
print(sum)"#,
        &[],
    ),
    (
        "iter_for_on_dict_yields_keys",
        r#"let out = ""
for k in {"a": 1, "b": 2, "c": 3} { out = out + k }
print(out)"#,
        &[],
    ),
    (
        "iter_string_yields_code_points",
        r#"let it = "bop".iter()
print(it.next())
print(it.next())
print(it.next())
print(it.next())"#,
        &[],
    ),
    // ─── bop-std stdlib (phase 7) ─────────────────────────────
    (
        "std_math_factorial",
        r#"use std.math
print(factorial(5))
print(gcd(12, 18))
print(clamp(99, 0, 10))"#,
        &[],
    ),
    (
        "std_iter_functional_helpers",
        r#"use std.iter
let nums = [1, 2, 3, 4, 5]
print(map(nums, fn(x) { return x + 1 }))
print(filter(nums, fn(x) { return x % 2 == 0 }))
print(reduce(nums, 0, fn(a, b) { return a + b }))"#,
        &[],
    ),
    (
        "std_string_reverse_and_pad",
        r#"use std.string
print(reverse("hello"))
print(pad_left("7", 3, "0"))
print(is_palindrome("racecar"))"#,
        &[],
    ),
    (
        "core_math_builtins_no_import",
        r#"print(16.sqrt())
print(3.7.floor())
print(3.2.ceil())
print(2.pow(10))"#,
        &[],
    ),
    (
        "imported_fn_calls_sibling_fn",
        r#"use helpers
print(quadruple(3))"#,
        &[(
            "helpers",
            r#"fn double(x) { return x * 2 }
fn quadruple(x) { return double(double(x)) }"#,
        )],
    ),
    (
        "imported_struct_type_in_caller",
        r#"use shapes
let p = Point { x: 3, y: 4 }
print(p.x + p.y)"#,
        &[(
            "shapes",
            r#"struct Point { x, y }"#,
        )],
    ),
    (
        "imported_enum_type_in_caller",
        r#"use shapes
let s = Shape::Rect { w: 4, h: 3 }
print(match s {
    Shape::Circle(r) => r,
    Shape::Rect { w, h } => w * h,
})"#,
        &[(
            "shapes",
            r#"enum Shape { Circle(r), Rect { w, h } }"#,
        )],
    ),
];

// ─── The actual three-way test ────────────────────────────────────

#[test]
#[ignore]
fn three_way_diff() {
    // Unify the flat CORPUS (no imports) and IMPORTS_CORPUS into
    // a single list of `CorpusEntry`. The empty-slice for
    // `modules` on flat entries is load-bearing — it's what lets
    // us skip threading a resolver through to simple programs.
    let mut entries: Vec<CorpusEntry> = CORPUS
        .iter()
        .map(|(name, source)| CorpusEntry {
            name,
            source,
            modules: &[],
        })
        .collect();
    for (name, source, modules) in IMPORTS_CORPUS {
        entries.push(CorpusEntry {
            name,
            source,
            modules,
        });
    }

    // Step 1: compute walker + VM outcomes up-front so we can
    // compare against AOT after the slow compile.
    let mut walker = std::collections::HashMap::new();
    let mut vm = std::collections::HashMap::new();
    for e in &entries {
        walker.insert(e.name, walker_outcome(e.source, e.modules));
        vm.insert(e.name, vm_outcome(e.source, e.modules));
    }

    // Step 2: run the batched AOT once.
    let aot_results = run_aot_batch(&entries);
    let aot: std::collections::HashMap<String, Outcome> = aot_results.into_iter().collect();

    // Step 3: every program's outcome must agree across all three.
    let mut failures: Vec<String> = Vec::new();
    for e in &entries {
        let w = &walker[e.name];
        let v = &vm[e.name];
        let a = aot.get(e.name).unwrap_or_else(|| {
            panic!("AOT did not produce an envelope for {}", e.name);
        });

        if w != v || v != a {
            let mut msg = format!("\n--- {} ---\n", e.name);
            writeln!(msg, "walker: {:?}", w).unwrap();
            writeln!(msg, "vm:     {:?}", v).unwrap();
            writeln!(msg, "aot:    {:?}", a).unwrap();
            failures.push(msg);
        }
    }

    assert!(
        failures.is_empty(),
        "three-way differential failures:\n{}",
        failures.join("")
    );
}
