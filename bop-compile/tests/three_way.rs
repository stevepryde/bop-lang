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
}

impl BufHost {
    fn new() -> Self {
        Self {
            prints: RefCell::new(Vec::new()),
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
}

/// Normalised per-engine outcome the harness compares across
/// walker / VM / AOT.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Outcome {
    prints: Vec<String>,
    error: Option<String>,
}

fn walker_outcome(code: &str) -> Outcome {
    let mut host = BufHost::new();
    let result = bop::run(code, &mut host, &BopLimits::standard());
    Outcome {
        prints: host.prints.borrow().clone(),
        error: result.err().map(|e| e.message),
    }
}

fn vm_outcome(code: &str) -> Outcome {
    let mut host = BufHost::new();
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

/// Build the single-file AOT driver that runs every program in the
/// corpus and emits per-program envelopes on stdout.
fn build_driver(programs: &[(&str, &str)]) -> String {
    let mut out = String::new();
    out.push_str(DRIVER_HEADER);

    // One pub mod per program. Sandbox is on so runaway programs
    // can't hang the CI machine — the walker and VM run with the
    // same `BopLimits::standard()` so the comparison stays fair.
    for (name, src) in programs {
        let mod_src = transpile(
            src,
            &Options {
                emit_main: false,
                use_bop_sys: false,
                sandbox: true,
                module_name: Some(format!("p_{}", name)),
            },
        )
        .unwrap_or_else(|e| panic!("transpile failed for {}: {}", name, e.message));
        out.push_str(&mod_src);
        out.push('\n');
    }

    // Driver: iterate through programs, capture prints, emit
    // envelope. We inline the calls rather than build a Vec of
    // trait objects — each `p_X::run` is generic over H and can't
    // be trivially type-erased.
    out.push_str("fn main() {\n");
    out.push_str("    let mut out = ::std::string::String::new();\n");
    for (name, _) in programs {
        writeln!(
            out,
            concat!(
                "    {{\n",
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

fn run_aot_batch(programs: &[(&str, &str)]) -> Vec<(String, Outcome)> {
    let driver_src = build_driver(programs);
    let dir = write_driver_project(&driver_src);
    let output = Command::new("cargo")
        .arg("run")
        .arg("--quiet")
        .arg("--release")
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
        "if_else_if",
        r#"let x = 2
if x == 1 { print("one") } else if x == 2 { print("two") } else { print("other") }"#,
    ),
    ("if_expression", "let x = if true { 1 } else { 2 }\nprint(x)"),
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
        result.push(str(i))
    }
}
print(result.join(", "))"#,
    ),
    ("builtin_str_int_type", "print(str(42))\nprint(int(3.7))\nprint(type([]))"),
    ("builtin_abs_min_max", "print(abs(-5))\nprint(min(3, 7))\nprint(max(3, 7))"),
    ("builtin_range", "print(range(5))\nprint(range(2, 5))\nprint(range(0, 10, 3))"),
    ("builtin_len_inspect", r#"print(len("hello"))
print(len([1, 2, 3]))
print(inspect("hi"))"#),
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
    // NOTE: `print(nope)` — an undefined identifier — is *not*
    // included: the walker raises "Variable `nope` not found" at
    // runtime, but the AOT emits `nope.clone()` which rustc
    // rejects at compile time with a different message. Both halt
    // with a useful error; the three-way harness just can't
    // phrase the assertion as "same message text".
    ("error_array_oob", "let a = [1]\nprint(a[5])"),
];

// ─── The actual three-way test ────────────────────────────────────

#[test]
#[ignore]
fn three_way_diff() {
    // Step 1: compute walker + VM outcomes up-front so we can
    // compare against AOT after the slow compile.
    let mut walker = std::collections::HashMap::new();
    let mut vm = std::collections::HashMap::new();
    for (name, src) in CORPUS {
        walker.insert(*name, walker_outcome(src));
        vm.insert(*name, vm_outcome(src));
    }

    // Step 2: run the batched AOT once.
    let aot_results = run_aot_batch(CORPUS);
    let aot: std::collections::HashMap<String, Outcome> = aot_results.into_iter().collect();

    // Step 3: every program's outcome must agree across all three.
    let mut failures: Vec<String> = Vec::new();
    for (name, _src) in CORPUS {
        let w = &walker[*name];
        let v = &vm[*name];
        let a = aot.get(*name).unwrap_or_else(|| {
            panic!("AOT did not produce an envelope for {}", name);
        });

        if w != v || v != a {
            let mut msg = format!("\n--- {} ---\n", name);
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
