//! End-to-end differential tests for the AOT transpiler.
//!
//! Each test:
//!
//! 1. Runs the Bop program through the tree-walker to get the
//!    reference output.
//! 2. Transpiles the same program to Rust via `bop-compile`.
//! 3. Drops the generated Rust into a scratch `cargo` project under
//!    `target/bop-compile-e2e/<test-name>/`, pointing at the
//!    workspace `bop` / `bop-sys` crates by path.
//! 4. Runs `cargo run` and captures stdout.
//! 5. Asserts the AOT output matches the tree-walker's.
//!
//! These are marked `#[ignore]` because each test spins up a full
//! `cargo build` — cheap per-test (~1s warm cache) but too heavy for
//! every `cargo test` run. Opt in with
//!
//! ```text
//! cargo test -p bop-compile --test e2e -- --ignored
//! ```
//!
//! The scratch dir is reused across invocations, so the second run
//! is markedly faster than the first (dep tree compiled once).

use std::cell::RefCell;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use bop::{BopError, BopHost, BopLimits, Value};
use bop_compile::{Options, transpile};

// ─── Tree-walker reference ────────────────────────────────────────

struct RecordHost {
    prints: RefCell<Vec<String>>,
}

impl BopHost for RecordHost {
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

fn walker_output(code: &str) -> String {
    let host = RecordHost {
        prints: RefCell::new(Vec::new()),
    };
    let mut host = host;
    bop::run(code, &mut host, &BopLimits::standard())
        .expect("tree-walker failed on e2e program");
    host.prints.borrow().join("\n")
}

// ─── AOT scratch project ──────────────────────────────────────────

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at the crate under test; the
    // workspace root is one level up.
    let crate_dir: PathBuf = env!("CARGO_MANIFEST_DIR").into();
    crate_dir.parent().unwrap().to_path_buf()
}

fn scratch_dir(test_name: &str) -> PathBuf {
    let mut p = workspace_root();
    p.push("target");
    p.push("bop-compile-e2e");
    p.push(test_name);
    p
}

fn write_scratch_project(test_name: &str, rust_src: &str) -> PathBuf {
    let root = workspace_root();
    let dir = scratch_dir(test_name);
    let src_dir = dir.join("src");
    std::fs::create_dir_all(&src_dir).expect("create scratch src dir");

    let bop_path = root.join("bop");
    let bop_sys_path = root.join("bop-sys");
    let manifest = format!(
        r#"[package]
name = "bop-e2e-{name}"
version = "0.0.0"
edition = "2024"
publish = false

[dependencies]
bop = {{ path = "{bop}", package = "bop-lang" }}
bop-sys = {{ path = "{bop_sys}" }}

[[bin]]
name = "program"
path = "src/main.rs"

[workspace]
"#,
        name = test_name,
        bop = bop_path.display(),
        bop_sys = bop_sys_path.display(),
    );
    std::fs::write(dir.join("Cargo.toml"), manifest).expect("write Cargo.toml");
    std::fs::write(src_dir.join("main.rs"), rust_src).expect("write main.rs");
    dir
}

fn run_aot(code: &str, test_name: &str) -> String {
    let rust_src = transpile(code, &Options::default()).expect("transpile");
    let dir = write_scratch_project(test_name, &rust_src);
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
            rust_src,
        );
    }
    String::from_utf8_lossy(&output.stdout)
        .trim_end_matches('\n')
        .to_string()
}

fn assert_aot_matches(test_name: &str, code: &str) {
    let expected = walker_output(code);
    let actual = run_aot(code, test_name);
    assert_eq!(
        actual,
        expected,
        "aot output diverged from tree-walker on {}:\n--- tree-walker ---\n{}\n--- aot ---\n{}",
        test_name, expected, actual,
    );
}

fn cargo_available() -> bool {
    Command::new("cargo")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ─── Tests ────────────────────────────────────────────────────────

#[test]
#[ignore]
fn e2e_hello_world() {
    if !cargo_available() {
        eprintln!("cargo not available — skipping");
        return;
    }
    assert_aot_matches("hello_world", r#"print("hello, world")"#);
}

#[test]
#[ignore]
fn e2e_arithmetic() {
    assert_aot_matches(
        "arithmetic",
        r#"print(1 + 2)
print(10 - 3)
print(4 * 5)
print(7 / 2)
print(10 % 3)
print(2 + 3 * 4)"#,
    );
}

#[test]
#[ignore]
fn e2e_variables_and_assign() {
    assert_aot_matches(
        "variables",
        r#"let x = 10
print(x)
x = 42
print(x)
x += 8
print(x)
x *= 2
print(x)"#,
    );
}

#[test]
#[ignore]
fn e2e_if_and_while() {
    assert_aot_matches(
        "if_and_while",
        r#"let i = 0
let total = 0
while i < 5 {
    if i % 2 == 0 {
        total = total + i
    }
    i = i + 1
}
print(total)"#,
    );
}

#[test]
#[ignore]
fn e2e_repeat_and_for() {
    assert_aot_matches(
        "repeat_and_for",
        r#"let n = 0
repeat 4 { n = n + 1 }
print(n)

let sum = 0
for x in [10, 20, 30] { sum = sum + x }
print(sum)

let s = 0
for i in range(5) { s = s + i }
print(s)"#,
    );
}

#[test]
#[ignore]
fn e2e_user_fn_with_recursion() {
    assert_aot_matches(
        "recursion",
        r#"fn fib(n) {
    if n <= 1 { return n }
    return fib(n - 1) + fib(n - 2)
}
print(fib(10))"#,
    );
}

#[test]
#[ignore]
fn e2e_truthiness_and_short_circuit() {
    assert_aot_matches(
        "truthiness",
        r#"print(true && false)
print(true || false)
print(false || true)
print(if 0 { "t" } else { "f" })
print(if "" { "t" } else { "f" })
print(if [1] { "t" } else { "f" })"#,
    );
}

#[test]
#[ignore]
fn e2e_builtins_str_int_type() {
    assert_aot_matches(
        "builtins",
        r#"print(str(42))
print(int(3.7))
print(type("hi"))
print(type(42))
print(abs(-7))
print(min(3, 7))
print(max(3, 7))
print(len([1, 2, 3]))"#,
    );
}
