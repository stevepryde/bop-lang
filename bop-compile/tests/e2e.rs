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
use bop_compile::{Options, modules_from_map, transpile};

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

fn run_aot_with_opts(code: &str, test_name: &str, opts: &Options) -> AotRun {
    let rust_src = transpile(code, opts).expect("transpile");
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
    AotRun {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout)
            .trim_end_matches('\n')
            .to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        rust_src,
    }
}

fn run_generated_source(test_name: &str, rust_src: String) -> AotRun {
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
    AotRun {
        status: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout)
            .trim_end_matches('\n')
            .to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        rust_src,
    }
}

fn run_aot_with_modules_and_opts(
    code: &str,
    test_name: &str,
    modules: &[(&str, &str)],
    opts: &Options,
) -> AotRun {
    let mut opts = opts.clone();
    opts.module_resolver = Some(modules_from_map(modules.iter().map(|(k, v)| (*k, *v))));
    run_aot_with_opts(code, test_name, &opts)
}

struct AotRun {
    status: Option<i32>,
    stdout: String,
    stderr: String,
    rust_src: String,
}

fn run_aot(code: &str, test_name: &str) -> String {
    let run = run_aot_with_opts(code, test_name, &Options::default());
    if run.status != Some(0) {
        panic!(
            "cargo run failed for {}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- generated ---\n{}",
            test_name, run.stdout, run.stderr, run.rust_src,
        );
    }
    run.stdout
}

#[test]
#[ignore]
fn e2e_sandbox_function_sites_preserve_redeclaration_and_exact_self() {
    let opts = Options {
        sandbox: true,
        ..Options::default()
    };
    let cases = [
        (
            "pub fn f() { return 1 } pub fn f() { return 2 }\nprint(f())",
            "sandbox_same_line_function_sites",
            "2",
        ),
        (
            "fn outer() { fn f() { return 1 } fn f() { return 2 } return f() }\nprint(outer())",
            "sandbox_nested_function_sites",
            "2",
        ),
        (
            "fn outer() { fn inner() { return 1 } return inner() }\npub fn later() { return 3 }\nprint(later())",
            "sandbox_nested_before_later_persistent_site",
            "3",
        ),
        (
            "if true { fn h() { return 7 } }\nprint(h())",
            "sandbox_reached_block_function_persists",
            "7",
        ),
        (
            "fn install() { fn h() { return 8 } }\ninstall()\nprint(h())",
            "sandbox_called_function_installs_global_function",
            "8",
        ),
        (
            "let install = fn() { fn h() { return 9 } }\ninstall()\nprint(h())",
            "sandbox_lambda_installs_global_function",
            "9",
        ),
        (
            "let install = match 1 { 1 => fn() { fn h() { return 10 } }, _ => fn() { fn h() { return 11 } } }\ninstall()\nprint(h())",
            "sandbox_match_arm_lambda_installs_global_function",
            "10",
        ),
        (
            "fn f() { return 1 }\nif true { fn f() { return 2 } }\nprint(f())",
            "sandbox_reached_nested_redeclaration_updates_ordinary_lookup",
            "2",
        ),
        (
            "fn f() { return 1 }\nif false { fn f() { return 2 } }\nprint(f())",
            "sandbox_dead_nested_redeclaration_preserves_ordinary_lookup",
            "1",
        ),
        (
            "fn f() { return 1 }\nfn get() { return f }\nfn f() { return 2 }\nprint(get()())",
            "sandbox_non_self_function_values_resolve_active_site",
            "2",
        ),
        (
            "struct S {}\nfn S.install(self) { fn h() { return 14 } }\nlet s = S {}\ns.install()\nprint(h())",
            "sandbox_method_body_installs_global_function",
            "14",
        ),
        (
            "fn f(n) { if n == 0 { return 1 } return f(n - 1) }\nlet old = f\nfn f(n) { return 9 }\nprint(old(2))",
            "sandbox_retained_self_recursion",
            "1",
        ),
        (
            "fn f() { return f }\nlet old = f\nfn f(x) { return x }\nlet again = old()\nprint(again())",
            "sandbox_retained_self_value",
            "<fn f>",
        ),
    ];
    for (source, name, expected) in cases {
        let run = run_aot_with_opts(source, name, &opts);
        assert_eq!(run.status, Some(0), "generated program failed: {}", run.stderr);
        assert_eq!(run.stdout, expected);
    }
}

#[test]
#[ignore]
fn e2e_sandbox_unreached_nested_functions_are_not_statically_callable() {
    let opts = Options {
        sandbox: true,
        ..Options::default()
    };
    for (source, name) in [
        (
            "let g = fn() { if true { f(); fn f() {} } }\nprint(try_call(g))",
            "sandbox_nested_call_before_declaration",
        ),
        (
            "let g = fn() { if false { fn f() {} } f() }\nprint(try_call(g))",
            "sandbox_nested_call_outside_dead_branch",
        ),
    ] {
        let expected = walker_output(source);
        let run = run_aot_with_opts(source, name, &opts);
        assert_eq!(run.status, Some(0), "generated program failed: {}", run.stderr);
        assert_eq!(run.stdout, expected);
    }
}

#[test]
#[ignore]
fn e2e_sandbox_abi_uses_final_reached_public_declaration_sites() {
    let source = "pub fn first() { return 1 }\npub fn hidden() { return 2 }\nfn hidden() { return 3 }\npub fn first(x) { return x }\nfn install() { fn first(x) { return 99 } }\ninstall()\nreturn\npub fn skipped() { return 4 }";
    let mut rust_src = transpile(
        source,
        &Options {
            emit_main: false,
            use_bop_sys: false,
            sandbox: true,
            ..Options::default()
        },
    )
    .unwrap();
    rust_src.push_str(
        r#"
struct Host;
impl ::bop::BopHost for Host {
    fn call(&mut self, _name: &str, _args: &[::bop::value::Value], _line: u32) -> Option<Result<::bop::value::Value, ::bop::error::BopError>> { None }
}
fn main() {
    let limits = ::bop::BopLimits::standard();
    let mut host = Host;
    let mut state = __bop_load_state(&mut host, &limits).unwrap();
    let entries = __bop_instance_entry_points(&state);
    println!("{}", entries.iter().map(|entry| format!("{}/{}", entry.name(), entry.arity())).collect::<Vec<_>>().join(","));
    let site = state.abi_declarations.iter().copied().find(|site| __BOP_FUNCTION_SITES[*site].name == "first" && __BOP_FUNCTION_SITES[*site].is_public).unwrap();
    let mut ctx = Ctx { host: &mut host, state: &mut state, steps: 0, call_depth: 0, max_steps: limits.max_steps };
    println!("{}", __bop_call_function_site(&mut ctx, site, vec![::bop::value::Value::Int(7)], 0).unwrap());
    println!("{}", __bop_call_active_function(&mut ctx, "<root>", "first", vec![::bop::value::Value::Int(7)], 0).unwrap());
}
"#,
    );
    let run = run_generated_source("sandbox_exact_final_abi", rust_src);
    assert_eq!(run.status, Some(0), "generated program failed: {}", run.stderr);
    assert_eq!(run.stdout, "first/1\n7\n99");
}

#[test]
#[ignore]
fn e2e_sandbox_generated_instance_retains_state_and_callbacks() {
    let source = r#"init()
let count = 0
pub fn next(delta) { count += delta; return count }
fn private() { return 99 }
pub fn callback() {
    return fn(delta) { count += delta; return count }
}
pub fn recurse(n) {
    if n == 0 { return 0 }
    return recurse(n - 1)
}
pub fn recurse_value() { return recurse }
pub fn attempt(f) { return try_call(f) }
pub fn invoke(f) { return f() }
pub fn map_callback(f) { return Result::Ok(1).map(f) }
pub fn reenter() { return host_reenter() }"#;
    let mut rust_src = transpile(
        source,
        &Options {
            emit_main: false,
            use_bop_sys: false,
            sandbox: true,
            ..Options::default()
        },
    )
    .unwrap();
    rust_src.push_str(
        r#"
struct HostA { init_calls: usize }
impl ::bop::BopHost for HostA {
    fn call(&mut self, name: &str, _args: &[::bop::value::Value], _line: u32) -> Option<Result<::bop::value::Value, ::bop::error::BopError>> {
        if name == "init" {
            self.init_calls += 1;
            Some(Ok(::bop::value::Value::None))
        } else {
            None
        }
    }
}
struct HostB;
impl ::bop::BopHost for HostB {
    fn call(&mut self, name: &str, _args: &[::bop::value::Value], _line: u32) -> Option<Result<::bop::value::Value, ::bop::error::BopError>> {
        (name == "init").then_some(Ok(::bop::value::Value::None))
    }
}
fn expect_int(value: ::bop::value::Value, expected: i64) {
    assert!(matches!(value, ::bop::value::Value::Int(actual) if actual == expected));
}
fn main() {
    let limits = ::bop::BopLimits::standard();
    let mut host_a = HostA { init_calls: 0 };
    let mut instance = BopInstance::load(&mut host_a as &mut dyn ::bop::BopHost, &limits).unwrap();
    assert_eq!(host_a.init_calls, 1);
    drop(host_a);

    let entries = instance.entry_points().iter().map(|entry| format!("{}/{}", entry.name(), entry.arity())).collect::<Vec<_>>();
    assert_eq!(entries, ["next/1", "callback/0", "recurse/1", "recurse_value/0", "attempt/1", "invoke/1", "map_callback/1", "reenter/0"]);
    let mut host_b = HostB;
    expect_int(instance.call("next", &[::bop::value::Value::Int(1)], &mut host_b).unwrap(), 1);
    expect_int(instance.call("next", &[::bop::value::Value::Int(2)], &mut host_b).unwrap(), 3);
    assert!(instance.call("private", &[], &mut host_b).unwrap_err().message.contains("Public entry point"));
    assert!(instance.call("next", &[], &mut host_b).unwrap_err().message.contains("expects 1 argument"));

    let callback = instance.call("callback", &[], &mut host_b).unwrap();
    expect_int(instance.call_value(&callback, &[::bop::value::Value::Int(4)], &mut host_b).unwrap(), 7);
    expect_int(instance.call_value(&callback, &[::bop::value::Value::Int(5)], &mut host_b).unwrap(), 12);
    expect_int(bop_entry_points::__bop_entry_6e657874(&mut instance, &[::bop::value::Value::Int(1)], &mut host_b).unwrap(), 13);

    expect_int(instance.call("recurse", &[::bop::value::Value::Int(63)], &mut host_b).unwrap(), 0);
    let boundary = instance.call("recurse", &[::bop::value::Value::Int(64)], &mut host_b).unwrap_err();
    assert!(boundary.message.contains("nested function calls"), "{}", boundary.message);
    let deep = instance.call("recurse", &[::bop::value::Value::Int(100)], &mut host_b).unwrap_err();
    assert!(deep.message.contains("nested function calls"), "{}", deep.message);
    expect_int(instance.call("recurse", &[::bop::value::Value::Int(1)], &mut host_b).unwrap(), 0);

    let recurse_value = instance.call("recurse_value", &[], &mut host_b).unwrap();
    expect_int(instance.call_value(&recurse_value, &[::bop::value::Value::Int(63)], &mut host_b).unwrap(), 0);
    assert!(instance.call_value(&recurse_value, &[::bop::value::Value::Int(64)], &mut host_b).unwrap_err().message.contains("nested function calls"));
    expect_int(instance.call_value(&recurse_value, &[::bop::value::Value::Int(1)], &mut host_b).unwrap(), 0);

    let mut second = BopInstance::load(&mut host_b, &limits).unwrap();
    assert!(second.call_value(&callback, &[], &mut host_b).unwrap_err().message.contains("different Bop engine instance"));
    let second_callback = second.call("callback", &[], &mut host_b).unwrap();
    let foreign_attempt = instance.call("attempt", &[second_callback.clone()], &mut host_b).unwrap();
    assert!(matches!(foreign_attempt, ::bop::value::Value::EnumVariant(value) if value.variant() == "Err"));
    assert!(instance.call("invoke", &[second_callback.clone()], &mut host_b).unwrap_err().message.contains("different Bop engine instance"));
    assert!(instance.call("map_callback", &[second_callback], &mut host_b).unwrap_err().message.contains("different Bop engine instance"));
    let arity_attempt = instance.call("attempt", &[recurse_value.clone()], &mut host_b).unwrap();
    assert!(matches!(arity_attempt, ::bop::value::Value::EnumVariant(value) if value.variant() == "Err"));
    expect_int(instance.call("next", &[::bop::value::Value::Int(1)], &mut host_b).unwrap(), 14);

    let mut walker = ::bop::BopInstance::load(
        "pub fn callback() { return fn() { return 1 } }",
        &mut host_b,
        &limits,
    ).unwrap();
    let foreign = walker.call("callback", &[], &mut host_b).unwrap();
    assert!(instance.call_value(&foreign, &[], &mut host_b).unwrap_err().message.contains("different Bop engine instance"));
    assert!(instance.call_value(&::bop::value::Value::Int(1), &[], &mut host_b).unwrap_err().message.contains("expected function"));
    let external_ast = ::bop::value::Value::new_fn(Vec::new(), Vec::new(), Vec::new(), None);
    assert!(instance.call_value(&external_ast, &[], &mut host_b).unwrap_err().message.contains("wasn't compiled for the AOT"));
    let external_body: ::std::rc::Rc<dyn ::core::any::Any + 'static> = ::std::rc::Rc::new(1usize);
    let external_compiled = ::bop::value::Value::new_compiled_fn(Vec::new(), Vec::new(), external_body, None);
    assert!(instance.call_value(&external_compiled, &[], &mut host_b).unwrap_err().message.contains("wasn't compiled by the AOT"));

    instance.in_operation.set(true);
    assert!(instance.call("missing", &[::bop::value::Value::None], &mut host_b).unwrap_err().message.contains("cannot be re-entered"));
    instance.in_operation.set(false);
    expect_int(instance.call("next", &[::bop::value::Value::Int(1)], &mut host_b).unwrap(), 15);
    println!("ok");
}
"#,
    );
    let run = run_generated_source("sandbox_generated_instance_state_callbacks", rust_src);
    assert_eq!(run.status, Some(0), "generated program failed: {}", run.stderr);
    assert_eq!(run.stdout, "ok");
}

#[test]
#[ignore]
fn e2e_sandbox_generated_instance_scopes_limits_and_host_memory() {
    let source = r#"boot()
let kept = none
let mutation = 0
pub fn external_value() { return host_value() }
pub fn detach_value() {
    let value = host_value()
    value.push("script-owned-abcdefghijklmnopqrstuvwxyz0123456789")
    return value
}
pub fn print_it() { print("hello") }
pub fn hint_it() { return missing_host_function() }
pub fn spin(n) { repeat n { } return n }
pub fn call_other() { return nested_other() }
pub fn make_result() {
    let value = []
    repeat 8 {
        value.push("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789")
    }
    return value
}
pub fn mutate_then_fail() { mutation += 1; panic("boom") }
pub fn mutate_then_spin() { mutation += 1; repeat 200 { } }
pub fn read_mutation() { return mutation }
pub fn poison() {
    kept = host_value()
    repeat 16 {
        kept.push("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789")
    }
}"#;
    let mut rust_src = transpile(
        source,
        &Options {
            emit_main: false,
            use_bop_sys: false,
            sandbox: true,
            ..Options::default()
        },
    )
    .unwrap();
    rust_src.push_str(
        r#"
struct InnerHost;
impl ::bop::BopHost for InnerHost {
    fn call(&mut self, name: &str, _args: &[::bop::value::Value], _line: u32) -> Option<Result<::bop::value::Value, ::bop::error::BopError>> {
        (name == "boot").then_some(Ok(::bop::value::Value::None))
    }
}
struct Host {
    retained: ::std::cell::RefCell<Vec<::bop::value::Value>>,
    other: Option<(BopInstance, InnerHost)>,
}
impl Host {
    fn retain_external(&self) -> ::bop::value::Value {
        let value = ::bop::value::Value::new_array(vec![
            ::bop::value::Value::new_str("host-owned-abcdefghijklmnopqrstuvwxyz0123456789".to_string()),
        ]);
        self.retained.borrow_mut().push(value.clone());
        value
    }
}
impl ::bop::BopHost for Host {
    fn call(&mut self, name: &str, _args: &[::bop::value::Value], _line: u32) -> Option<Result<::bop::value::Value, ::bop::error::BopError>> {
        if name == "boot" {
            self.retain_external();
            return Some(Ok(::bop::value::Value::None));
        }
        if name == "host_value" {
            return Some(Ok(self.retain_external()));
        }
        if name == "nested_other" {
            let (mut other, mut inner_host) = self.other.take().expect("nested instance installed");
            let result = other.call("make_result", &[], &mut inner_host);
            self.other = Some((other, inner_host));
            return Some(result);
        }
        None
    }
    fn on_print(&mut self, _message: &str) {
        self.retain_external();
    }
    fn on_tick(&mut self) -> Result<(), ::bop::error::BopError> {
        self.retain_external();
        Ok(())
    }
    fn function_hint(&self) -> &str {
        self.retain_external();
        "host hint"
    }
}
fn main() {
    let limits = ::bop::BopLimits { max_steps: 100, max_memory: 1_600 };
    let mut host = Host { retained: ::std::cell::RefCell::new(Vec::new()), other: None };
    let mut instance = BopInstance::load(&mut host, &limits).unwrap();
    let baseline = instance.memory.__used();
    assert_eq!(baseline, 0, "top-level host allocations must not enter the instance account");
    let mut inner_host = InnerHost;
    let other = BopInstance::load(&mut inner_host, &limits).unwrap();
    let other_baseline = other.memory.__used();
    assert_eq!(other_baseline, 0);
    host.other = Some((other, inner_host));

    let external = instance.call("external_value", &[], &mut host).unwrap();
    assert_eq!(instance.memory.__used(), baseline);
    let nested = instance.call("call_other", &[], &mut host).unwrap();
    assert_eq!(instance.memory.__used(), baseline);
    assert!(host.other.as_ref().unwrap().0.memory.__used() > other_baseline);
    drop(nested);
    assert_eq!(host.other.as_ref().unwrap().0.memory.__used(), other_baseline);
    instance.call("print_it", &[], &mut host).unwrap();
    let hint = instance.call("hint_it", &[], &mut host).unwrap_err();
    assert_eq!(hint.friendly_hint.as_deref(), Some("host hint"));
    assert_eq!(instance.memory.__used(), baseline);
    drop(external);

    let detached = instance.call("detach_value", &[], &mut host).unwrap();
    assert!(instance.memory.__used() > baseline);
    drop(detached);
    assert_eq!(instance.memory.__used(), baseline);

    let steps = instance.call("spin", &[::bop::value::Value::Int(200)], &mut host).unwrap_err();
    assert!(steps.is_fatal && steps.message.contains("too many steps"));
    assert!(matches!(instance.call("spin", &[::bop::value::Value::Int(1)], &mut host).unwrap(), ::bop::value::Value::Int(1)));
    assert_eq!(instance.memory.__used(), baseline);

    let ordinary = instance.call("mutate_then_fail", &[], &mut host).unwrap_err();
    assert!(!ordinary.is_fatal && ordinary.message.contains("boom"));
    assert!(matches!(instance.call("read_mutation", &[], &mut host).unwrap(), ::bop::value::Value::Int(1)));
    let fatal = instance.call("mutate_then_spin", &[], &mut host).unwrap_err();
    assert!(fatal.is_fatal && fatal.message.contains("too many steps"));
    assert!(matches!(instance.call("read_mutation", &[], &mut host).unwrap(), ::bop::value::Value::Int(2)));

    let held = instance.call("make_result", &[], &mut host).unwrap();
    let held_memory = instance.memory.__used();
    assert!(held_memory > baseline);
    let nested_while_held = instance.call("call_other", &[], &mut host).unwrap();
    assert_eq!(instance.memory.__used(), held_memory);
    assert!(host.other.as_ref().unwrap().0.memory.__used() > other_baseline);
    drop(nested_while_held);
    assert_eq!(instance.memory.__used(), held_memory);
    assert_eq!(host.other.as_ref().unwrap().0.memory.__used(), other_baseline);
    let memory = instance.call("make_result", &[], &mut host).unwrap_err();
    assert!(memory.is_fatal && memory.message.contains("Memory limit exceeded"));
    drop(held);
    assert_eq!(instance.memory.__used(), baseline);
    let released = instance.call("make_result", &[], &mut host).unwrap();
    drop(released);
    assert_eq!(instance.memory.__used(), baseline);

    instance.in_operation.set(true);
    let reentry = instance.call("missing", &[::bop::value::Value::None], &mut host).unwrap_err();
    instance.in_operation.set(false);
    assert!(reentry.message.contains("cannot be re-entered"));
    assert!(matches!(instance.call("spin", &[::bop::value::Value::Int(1)], &mut host).unwrap(), ::bop::value::Value::Int(1)));

    let mut poisoned = BopInstance::load(&mut host, &limits).unwrap();
    let poison = poisoned.call("poison", &[], &mut host).unwrap_err();
    assert!(poison.is_fatal && poison.message.contains("Memory limit exceeded"));
    let poisoned_again = poisoned.call("spin", &[::bop::value::Value::Int(1)], &mut host).unwrap_err();
    assert!(poisoned_again.is_fatal && poisoned_again.message.contains("Memory limit exceeded"));
    assert!(matches!(instance.call("spin", &[::bop::value::Value::Int(1)], &mut host).unwrap(), ::bop::value::Value::Int(1)));
    println!("ok");
}
"#,
    );
    let run = run_generated_source("sandbox_generated_instance_limits_memory", rust_src);
    assert_eq!(run.status, Some(0), "generated program failed: {}", run.stderr);
    assert_eq!(run.stdout, "ok");
}

#[test]
#[ignore]
fn e2e_sandbox_generated_instance_retains_modules_types_methods_and_rng() {
    let root = r#"use wrapper as api
pub fn next() { return api.next() }
pub fn make(value) {
    let point = api.Point { value: value }
    return point.bump()
}
pub fn random() { return rand(1000000000) }"#;
    let modules = [
        (
            "dep",
            r#"let count = 0
struct Point { value }
fn Point.bump(self) { return self.value + 1 }
fn next() { count += 1; return count }"#,
        ),
        ("wrapper", "use dep"),
    ];
    let mut rust_src = transpile(
        root,
        &Options {
            emit_main: false,
            use_bop_sys: false,
            sandbox: true,
            module_resolver: Some(modules_from_map(modules)),
            ..Options::default()
        },
    )
    .unwrap();
    rust_src.push_str(
        r#"
struct Host;
impl ::bop::BopHost for Host {
    fn call(&mut self, _name: &str, _args: &[::bop::value::Value], _line: u32) -> Option<Result<::bop::value::Value, ::bop::error::BopError>> { None }
}
fn expect_int(value: ::bop::value::Value, expected: i64) {
    assert!(matches!(value, ::bop::value::Value::Int(actual) if actual == expected));
}
fn int(value: ::bop::value::Value) -> i64 {
    match value { ::bop::value::Value::Int(value) => value, other => panic!("expected int, got {}", other.type_name()) }
}
fn main() {
    let limits = ::bop::BopLimits::standard();
    let mut host = Host;
    let mut first = BopInstance::load(&mut host, &limits).unwrap();
    expect_int(first.call("next", &[], &mut host).unwrap(), 1);
    expect_int(first.call("next", &[], &mut host).unwrap(), 2);
    expect_int(first.call("make", &[::bop::value::Value::Int(41)], &mut host).unwrap(), 42);
    let first_random = int(first.call("random", &[], &mut host).unwrap());
    let second_random = int(first.call("random", &[], &mut host).unwrap());
    assert_ne!(first_random, second_random, "successive calls must advance retained RNG state");
    expect_int(first.call("next", &[], &mut host).unwrap(), 3);

    let mut second = BopInstance::load(&mut host, &limits).unwrap();
    assert_eq!(int(second.call("random", &[], &mut host).unwrap()), first_random);
    expect_int(second.call("next", &[], &mut host).unwrap(), 1);
    expect_int(first.call("next", &[], &mut host).unwrap(), 4);
    println!("ok");
}
"#,
    );
    let run = run_generated_source("sandbox_generated_instance_modules_types_rng", rust_src);
    assert_eq!(run.status, Some(0), "generated program failed: {}", run.stderr);
    assert_eq!(run.stdout, "ok");
}

#[test]
#[ignore]
fn e2e_sandbox_failed_module_load_rolls_back_reached_nested_function_sites() {
    let mut opts = Options {
        emit_main: false,
        use_bop_sys: false,
        sandbox: true,
        ..Options::default()
    };
    opts.module_resolver = Some(modules_from_map([
        ("bad", "let value = 1\nif true { fn leaked() { return 1 } }\nmissing()"),
    ]));
    let mut rust_src = transpile(
        "let attempt = fn() { use bad }\ntry_call(attempt)",
        &opts,
    )
    .unwrap();
    rust_src.push_str(
        r#"
struct Host;
impl ::bop::BopHost for Host {
    fn call(&mut self, _name: &str, _args: &[::bop::value::Value], _line: u32) -> Option<Result<::bop::value::Value, ::bop::error::BopError>> { None }
}
fn main() {
    let limits = ::bop::BopLimits::standard();
    let mut host = Host;
    let state = __bop_load_state(&mut host, &limits).unwrap();
    assert!(!state.active_function_sites.keys().any(|(module, _)| module == "bad"));
    assert!(!state.bindings.keys().any(|(module, _)| module == "bad"));
    println!("clean");
}
"#,
    );
    let run = run_generated_source("sandbox_failed_module_nested_site_rollback", rust_src);
    assert_eq!(run.status, Some(0), "generated program failed: {}", run.stderr);
    assert_eq!(run.stdout, "clean");
}

#[test]
#[ignore]
fn e2e_sandbox_dynamic_module_function_exports_follow_runtime_presence() {
    let opts = Options {
        sandbox: true,
        ..Options::default()
    };
    let reached = "if true { fn h() { return 12 } }";
    for (source, name) in [
        ("use m.{h}\nprint(h())", "sandbox_dynamic_export_selective"),
        ("use m\nprint(h())", "sandbox_dynamic_export_glob"),
        ("use m as x\nprint(x.h())", "sandbox_dynamic_export_alias"),
    ] {
        let run = run_aot_with_modules_and_opts(source, name, &[("m", reached)], &opts);
        assert_eq!(run.status, Some(0), "generated program failed: {}", run.stderr);
        assert_eq!(run.stdout, "12");
    }

    let run = run_aot_with_modules_and_opts(
        "use facade.{h}\nprint(h())",
        "sandbox_dynamic_export_facade",
        &[("m", reached), ("facade", "use m")],
        &opts,
    );
    assert_eq!(run.status, Some(0), "generated program failed: {}", run.stderr);
    assert_eq!(run.stdout, "12");

    let dead = "if false { fn h() { return 13 } }";
    for (source, name) in [
        (
            "let load = fn() { use m.{h} }\nprint(try_call(load))",
            "sandbox_dead_dynamic_export_selective",
        ),
        (
            "use m\nlet invoke = fn() { return h() }\nprint(try_call(invoke))",
            "sandbox_dead_dynamic_export_glob",
        ),
        (
            "use m as x\nlet invoke = fn() { return x.h() }\nprint(try_call(invoke))",
            "sandbox_dead_dynamic_export_alias",
        ),
    ] {
        let run = run_aot_with_modules_and_opts(source, name, &[("m", dead)], &opts);
        assert_eq!(run.status, Some(0), "generated program failed: {}", run.stderr);
        assert!(run.stdout.contains("Err("), "unexpected output: {}", run.stdout);
    }
}

#[test]
#[ignore]
fn e2e_sandbox_optional_imports_preserve_presence_and_lambda_snapshots() {
    let opts = Options {
        sandbox: true,
        ..Options::default()
    };
    let module = "if true { fn h() { return 12 } }";
    for (source, name, expected) in [
        (
            "fn make() { use m.{h}\nlet saved = fn() { return h() }\nh = 3\nreturn saved }\nprint(make()())",
            "sandbox_local_import_lambda_snapshot",
            "12",
        ),
        (
            "use m\nlet saved = fn() { return h() }\nh = 3\nprint(saved())",
            "sandbox_persistent_import_lambda_snapshot",
            "12",
        ),
        (
            "use m\nlet saved = fn() { return h() }\nfn h() { return 20 }\nprint(saved())\nprint(h())",
            "sandbox_import_before_function_stays_bound_and_captured",
            "12\n12",
        ),
        (
            "fn h() { return 20 }\nuse m\nprint(h())",
            "sandbox_function_before_import_blocks_binding",
            "20",
        ),
    ] {
        let run = run_aot_with_modules_and_opts(source, name, &[("m", module)], &opts);
        assert_eq!(run.status, Some(0), "generated program failed: {}", run.stderr);
        assert_eq!(run.stdout, expected);
    }

    let run = run_aot_with_modules_and_opts(
        "fn make() { use m\nreturn fn() { return h() } }\nlet saved = make()\nprint(try_call(saved))",
        "sandbox_absent_import_lambda_falls_back_at_call_time",
        &[("m", "if false { fn h() { return 13 } }")],
        &opts,
    );
    assert_eq!(run.status, Some(0), "generated program failed: {}", run.stderr);
    assert!(run.stdout.contains("Err("), "unexpected output: {}", run.stdout);
}

#[test]
#[ignore]
fn e2e_sandbox_dynamic_value_exports_follow_early_return_presence() {
    let opts = Options {
        sandbox: true,
        ..Options::default()
    };
    let module = "let before = 1\nreturn\nlet after = 2";
    let present = run_aot_with_modules_and_opts(
        "use m.{before}\nprint(before)",
        "sandbox_early_return_value_export_present",
        &[("m", module)],
        &opts,
    );
    assert_eq!(present.status, Some(0), "generated program failed: {}", present.stderr);
    assert_eq!(present.stdout, "1");

    let absent_alias = run_aot_with_modules_and_opts(
        "use facade\nprint(1)",
        "sandbox_early_return_module_alias_absent",
        &[
            ("dep", "struct Point { value }"),
            ("wrapper", "return\nuse dep as api"),
            ("facade", "use wrapper"),
        ],
        &opts,
    );
    assert_eq!(
        absent_alias.status,
        Some(0),
        "generated program failed: {}",
        absent_alias.stderr
    );
    assert_eq!(absent_alias.stdout, "1");

    for (source, name, modules) in [
        (
            "let load = fn() { use m.{after} }\nprint(try_call(load))",
            "sandbox_early_return_value_selective_absent",
            vec![("m", module)],
        ),
        (
            "use m\nlet read = fn() { return after }\nprint(try_call(read))",
            "sandbox_early_return_value_glob_absent",
            vec![("m", module)],
        ),
        (
            "use m as x\nlet read = fn() { return x.after }\nprint(try_call(read))",
            "sandbox_early_return_value_alias_absent",
            vec![("m", module)],
        ),
        (
            "use facade\nlet read = fn() { return after }\nprint(try_call(read))",
            "sandbox_early_return_value_facade_absent",
            vec![("m", module), ("facade", "use m")],
        ),
    ] {
        let run = run_aot_with_modules_and_opts(source, name, &modules, &opts);
        assert_eq!(run.status, Some(0), "generated program failed: {}", run.stderr);
        assert!(run.stdout.contains("Err("), "unexpected output: {}", run.stdout);
    }
}

#[test]
#[ignore]
fn e2e_sandbox_callable_shadows_preserve_module_alias_context() {
    let source = r#"use types as t
fn clobber() {
    let t = 0
    t = 1
}
fn clobber_import() {
    use wrapper
    t = 3
}
fn clobber_param(t) { t = 2 }
fn probe(value) {
    return match value {
        t.Point { value: found } => found,
        _ => 0,
    }
}
let point = t.Point { value: 42 }
clobber()
clobber_import()
clobber_param(0)
print(probe(point))"#;
    let run = run_aot_with_modules_and_opts(
        source,
        "sandbox_callable_shadow_preserves_module_alias_context",
        &[
            ("types", "struct Point { value }"),
            ("wrapper", "use types as t"),
        ],
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(run.status, Some(0), "generated program failed: {}", run.stderr);
    assert_eq!(run.stdout, "42");
}

#[test]
#[ignore]
fn e2e_sandbox_reassigned_alias_patterns_read_authoritative_binding() {
    let source = r#"use first as dep
use second as other
fn probe(value) {
    return match value {
        dep.Point { value: found } => found,
        _ => 0,
    }
}
fn select_second() { dep = other }
let first_value = dep.Point { value: 1 }
print(probe(first_value))
select_second()
let second_value = dep.Point { value: 2 }
print(probe(second_value))
print(match second_value {
    dep.Point { value: found } => found,
    _ => 0,
})"#;
    let run = run_aot_with_modules_and_opts(
        source,
        "sandbox_reassigned_alias_patterns_read_authoritative_binding",
        &[
            ("first", "struct Point { value }"),
            ("second", "struct Point { value }"),
        ],
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(run.status, Some(0), "generated program failed: {}", run.stderr);
    assert_eq!(run.stdout, "1\n2\n2");

    let module_body = run_aot_with_modules_and_opts(
        "use switched\nprint(result)",
        "sandbox_reassigned_alias_module_body_pattern_reads_authoritative_binding",
        &[
            ("first", "struct Point { value }"),
            ("second", "struct Point { value }"),
            (
                "switched",
                r#"use first as dep
use second as other
fn select_second() { dep = other }
select_second()
let value = dep.Point { value: 3 }
let result = match value {
    dep.Point { value: found } => found,
    _ => 0,
}"#,
            ),
        ],
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(
        module_body.status,
        Some(0),
        "generated program failed: {}",
        module_body.stderr
    );
    assert_eq!(module_body.stdout, "3");
}

#[test]
#[ignore]
fn e2e_sandbox_local_flat_import_tracks_module_alias_presence() {
    let opts = Options {
        sandbox: true,
        ..Options::default()
    };
    let dep = r#"struct Point { value }
enum State { Item(value), Empty }
fn make(value) { return Point { value: value } }"#;
    let wrapper = "use dep as api";
    let present = run_aot_with_modules_and_opts(
        r#"fn build() {
    use wrapper
    let point = api.make(7)
    let state = api.State::Item(point.value)
    return match state {
        api.State::Item(value) => value,
        _ => 0,
    }
}
fn maker() {
    use wrapper
    return fn() {
        let point = api.Point { value: 11 }
        return match point {
            api.Point { value: found } => found,
            _ => 0,
        }
    }
}
print(build())
let saved = maker()
print(saved())"#,
        "sandbox_local_flat_import_module_alias_present",
        &[("dep", dep), ("wrapper", wrapper)],
        &opts,
    );
    assert_eq!(present.status, Some(0), "generated program failed: {}", present.stderr);
    assert_eq!(present.stdout, "7\n11");

    let absent = run_aot_with_modules_and_opts(
        "fn build() { use wrapper\nreturn api.Point { value: 1 } }\nprint(try_call(build))",
        "sandbox_local_flat_import_module_alias_absent",
        &[("dep", dep), ("wrapper", "return\nuse dep as api")],
        &opts,
    );
    assert_eq!(absent.status, Some(0), "generated program failed: {}", absent.stderr);
    assert!(absent.stdout.contains("Err("), "unexpected output: {}", absent.stdout);
    assert!(
        absent.stdout.contains("isn't a module alias in scope"),
        "unexpected output: {}",
        absent.stdout
    );
}

#[test]
#[ignore]
fn e2e_sandbox_local_imported_value_beats_same_named_host_function() {
    let mut opts = Options {
        emit_main: false,
        use_bop_sys: false,
        sandbox: true,
        ..Options::default()
    };
    opts.module_resolver = Some(modules_from_map([("m", "fn h() { return 12 }")]));
    let mut rust_src = transpile(
        "fn invoke() { use m.{h}\nreturn h() }\nprint(invoke())",
        &opts,
    )
    .unwrap();
    rust_src.push_str(
        r#"
struct Host { calls: usize }
impl ::bop::BopHost for Host {
    fn call(&mut self, name: &str, _args: &[::bop::value::Value], _line: u32) -> Option<Result<::bop::value::Value, ::bop::error::BopError>> {
        if name == "h" { self.calls += 1; Some(Ok(::bop::value::Value::Int(99))) } else { None }
    }
    fn on_print(&mut self, message: &str) { println!("{}", message); }
}
fn main() {
    let limits = ::bop::BopLimits::standard();
    let mut host = Host { calls: 0 };
    run(&mut host, &limits).unwrap();
    println!("calls={}", host.calls);
}
"#,
    );
    let run = run_generated_source("sandbox_local_import_host_precedence", rust_src);
    assert_eq!(run.status, Some(0), "generated program failed: {}", run.stderr);
    assert_eq!(run.stdout, "12\ncalls=0");
}

#[test]
#[ignore]
fn e2e_sandbox_state_backed_mutation_preserves_receiver_semantics() {
    let source = r#"let a = [1, 2]
a.push(3)
let b = a
a.push(4)
print(a)
print(b)
[8].push(9)
let d = { "a": [1] }
let nested = fn() { d["a"].push(2) }
print(try_call(nested))"#;
    let run = run_aot_with_opts(
        source,
        "sandbox_state_backed_receiver_semantics",
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(run.status, Some(0), "generated program failed: {}", run.stderr);
    assert_eq!(run.stdout.lines().next(), Some("[1, 2, 3, 4]"));
    assert_eq!(run.stdout.lines().nth(1), Some("[1, 2, 3]"));
    assert!(run.stdout.lines().nth(2).is_some_and(|line| line.contains("Err(")));
}

#[test]
#[ignore]
fn e2e_sandbox_exact_self_bypasses_same_named_host_function() {
    let source = "fn f(n) { if n == 0 { return 1 } return f(n - 1) }\nlet old = f\nfn f(n) { return 9 }\nprint(old(2))";
    let mut rust_src = transpile(
        source,
        &Options {
            emit_main: false,
            use_bop_sys: false,
            sandbox: true,
            ..Options::default()
        },
    )
    .unwrap();
    rust_src.push_str(
        r#"
struct Host { calls: usize }
impl ::bop::BopHost for Host {
    fn call(&mut self, name: &str, _args: &[::bop::value::Value], _line: u32) -> Option<Result<::bop::value::Value, ::bop::error::BopError>> {
        if name == "f" { self.calls += 1; Some(Ok(::bop::value::Value::Int(99))) } else { None }
    }
    fn on_print(&mut self, message: &str) { println!("{}", message); }
}
fn main() {
    let limits = ::bop::BopLimits::standard();
    let mut host = Host { calls: 0 };
    run(&mut host, &limits).unwrap();
    println!("calls={}", host.calls);
}
"#,
    );
    let run = run_generated_source("sandbox_exact_self_host_precedence", rust_src);
    assert_eq!(run.status, Some(0), "generated program failed: {}", run.stderr);
    assert_eq!(run.stdout, "1\ncalls=0");
}

fn assert_aot_nested_mutation_error(code: &str, test_name: &str, expected_line: u32) {
    let mut rust_src = transpile(
        code,
        &Options {
            emit_main: false,
            use_bop_sys: false,
            ..Options::default()
        },
    )
    .expect("transpile");
    rust_src.push_str(&format!(
        r#"
struct ErrorHost;
impl ::bop::BopHost for ErrorHost {{
    fn call(
        &mut self,
        _name: &str,
        _args: &[::bop::Value],
        _line: u32,
    ) -> Option<Result<::bop::Value, ::bop::BopError>> {{
        None
    }}
}}

fn main() {{
    let mut host = ErrorHost;
    let err = run(&mut host).expect_err("nested mutation should fail");
    assert_eq!(err.message, ::bop::error_messages::NESTED_MUTATION_ERROR_MESSAGE);
    assert_eq!(
        err.friendly_hint.as_deref(),
        Some(::bop::error_messages::NESTED_MUTATION_HINT),
    );
    assert_eq!(err.line, Some({expected_line}));
    assert!(!err.is_fatal);
}}
"#,
    ));
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
    assert!(
        output.status.success(),
        "AOT diagnostic assertion failed for {}:\n--- stdout ---\n{}\n--- stderr ---\n{}\n--- generated ---\n{}",
        test_name,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        rust_src,
    );
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
fn e2e_method_calls_array_and_string() {
    assert_aot_matches(
        "method_calls",
        r#"let a = [1, 2, 3]
a.push(4)
print(a.len())
print(a)
print("hello world".upper())
print("a,b,c".split(","))
print(["x", "y", "z"].join("-"))
let sorted = [3, 1, 2]
sorted.sort()
print(sorted)"#,
    );
}

#[test]
#[ignore]
fn e2e_array_mutation_fast_path_semantics() {
    let output = run_aot(
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
print(transient_source)

struct Accumulator { total }
fn Accumulator.push(self, value) { return self.total + value }
let accumulator = Accumulator { total: 7 }
print(accumulator.push(5))

let values = []
let next = 0
repeat 2048 {
    values.push(next)
    next += 1
}
print(values.len())
print(values[0])
print(values[-1])

let changed = [4, 1, 3]
print(changed.push(2))
print(changed.insert(1, 5))
print(changed.remove(2))
print(changed.pop())
changed.sort()
changed.reverse()
print(changed)

let unchanged = [1, 2, 3]
print(try_call(fn() { return unchanged.push() }).is_err())
print(try_call(fn() { return unchanged.insert(99, 4) }).is_err())
print(try_call(fn() { return unchanged.remove(99) }).is_err())
print(unchanged)"#,
        "array_mutation_fast_path_semantics",
    );
    assert_eq!(
        output,
        concat!(
            "[1, 2, 3]\n",
            "[1, 2]\n",
            "[1, 2]\n",
            "[7]\n",
            "12\n",
            "2048\n",
            "0\n",
            "2047\n",
            "none\n",
            "none\n",
            "1\n",
            "2\n",
            "[5, 4, 3]\n",
            "true\n",
            "true\n",
            "true\n",
            "[1, 2, 3]"
        )
    );
}

#[test]
#[ignore]
fn e2e_array_push_depth_error_is_clean() {
    let run = run_aot_with_opts(
        r#"let deep = none
repeat 64 { deep = [deep] }
let values = []
values.push(deep)"#,
        "array_push_depth_error",
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(
        run.status,
        Some(1),
        "expected a clean Bop error exit, not an abort; stderr:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains(bop::value::VALUE_DEPTH_ERROR_MESSAGE),
        "expected value-depth diagnostic; got:\n{}",
        run.stderr
    );
}

#[test]
#[ignore]
fn e2e_signed_indices_across_methods_and_subscripts() {
    let output = run_aot(
        r#"let values = [10, 20, 30, 40]
print(values.remove(-1))
print(values.insert(-1, 25))
print(values)
print(values.slice(-3, -1))
print(values.slice(-99, 99))
print(values.slice(99, -99))
let fractional = [10, 20, 30]
print(fractional[1.9])
fractional[-1.9] = 99
print(fractional.remove(-1.9))
print(fractional.insert(1.9, 15))
print(fractional)
let text = "a🙂é界"
print(text[-1])
print(text.slice(-3, -1))
print(text.slice(-99, 99))
let unchanged = [1, 2, 3]
print(try_call(fn() { return unchanged.remove(-4) }).is_err())
print(try_call(fn() { return unchanged.insert(-4, 0) }).is_err())
print(try_call(fn() { unchanged[-4] = 0 }).is_err())
print(try_call(fn() { return unchanged.remove("0") }).is_err())
print(unchanged)"#,
        "signed_indices_across_methods_and_subscripts",
    );
    assert_eq!(
        output,
        concat!(
            "40\n",
            "none\n",
            "[10, 20, 25, 30]\n",
            "[20, 25]\n",
            "[10, 20, 25, 30]\n",
            "[]\n",
            "20\n",
            "99\n",
            "none\n",
            "[10, 15, 20]\n",
            "界\n",
            "🙂é\n",
            "a🙂é界\n",
            "true\n",
            "true\n",
            "true\n",
            "true\n",
            "[1, 2, 3]"
        )
    );
}

#[test]
#[ignore]
fn e2e_nested_array_mutation_receiver_contract() {
    let output = run_aot(
        r#"struct Holder { items }
let indexed = {"items": [1]}
let fielded = Holder { items: [1, 2] }
let index_result = try_call(fn() {
    indexed["items"].push(2)
})
let field_result = try_call(fn() {
    fielded.items.pop()
})
print(index_result.is_err())
print(match index_result { Result::Err(e) => e.message, _ => "missing" })
print(match index_result { Result::Err(e) => e.line, _ => -1 })
print(field_result.is_err())
print(match field_result { Result::Err(e) => e.message, _ => "missing" })
print(match field_result { Result::Err(e) => e.line, _ => -1 })
fn make_array() { return [7] }
print([1].push(2))
print(make_array().pop())
struct Gadget { n }
fn Gadget.push(self, amount) { return self.n + amount }
struct Wrapper { item }
let wrapper = Wrapper { item: Gadget { n: 10 } }
let dynamic = {"item": Gadget { n: 20 }}
print(wrapper.item.push(2))
print(dynamic["item"].push(3))"#,
        "nested_array_mutation_receiver_contract",
    );
    let message = bop::error_messages::NESTED_MUTATION_ERROR_MESSAGE;
    assert_eq!(
        output,
        format!(
            "true\n{}\n5\ntrue\n{}\n8\nnone\n7\n12\n23",
            message, message
        )
    );
}

#[test]
#[ignore]
fn e2e_nested_array_mutation_diagnostic_and_grouped_receivers() {
    assert_aot_nested_mutation_error(
        r#"let indexed = {"items": [1]}
(indexed["items"]).push(2)"#,
        "nested_array_mutation_grouped_index_diagnostic",
        2,
    );
    assert_aot_nested_mutation_error(
        r#"struct Holder { items }
let fielded = Holder { items: [1] }
(fielded.items).pop()"#,
        "nested_array_mutation_grouped_field_diagnostic",
        3,
    );
}

#[test]
#[ignore]
fn e2e_string_interpolation() {
    assert_aot_matches(
        "interpolation",
        r#"let name = "bop"
let version = 2
print("hi {name}!")
print("bop v{version} ready")"#,
    );
}

#[test]
#[ignore]
fn e2e_indexed_writes_and_compound() {
    assert_aot_matches(
        "indexed_writes",
        r#"let a = [1, 2, 3]
a[0] = 99
print(a)
a[1] += 10
print(a)
a[-1] *= 2
print(a)
let d = {"hp": 100}
d["hp"] = 50
d["mp"] = 20
print(d["hp"])
print(d["mp"])"#,
    );
}

#[test]
#[ignore]
fn e2e_fizzbuzz_roundtrip() {
    // Canonical smoke test — uses arrays, method calls, string
    // interpolation indirectly through str(), for/range, if/else
    // chain, and mutation back-assign on `push`.
    assert_aot_matches(
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
    );
}

// ─── Sandbox ───────────────────────────────────────────────────

#[test]
#[ignore]
fn e2e_sandbox_happy_path_matches_walker() {
    // With sandbox on, output for a well-behaved program should
    // still match the tree-walker — ticks / memory checks fire but
    // don't change semantics.
    let code = r#"let sum = 0
for i in range(10) { sum = sum + i }
print(sum)"#;
    let expected = walker_output(code);
    let run = run_aot_with_opts(
        code,
        "sandbox_happy",
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(run.status, Some(0), "stderr:\n{}", run.stderr);
    assert_eq!(run.stdout, expected);
}

#[test]
#[ignore]
fn e2e_sandbox_halts_infinite_loop() {
    // Default limits are `BopLimits::standard()` — 10k steps. A
    // bare `while true { }` burns one tick per iteration and hits
    // the cap. The process should exit non-zero with the
    // canonical "too many steps" message on stderr.
    let run = run_aot_with_opts(
        "while true { }",
        "sandbox_infinite",
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_ne!(run.status, Some(0), "expected non-zero exit; stderr:\n{}", run.stderr);
    assert!(
        run.stderr.contains("too many steps"),
        "expected 'too many steps' in stderr; got:\n{}",
        run.stderr
    );
}

#[test]
#[ignore]
fn e2e_sandbox_halts_memory_bomb() {
    // `"x" * 999999` trips the pre-flight memory check
    // (`check_string_repeat_memory`) since standard limits set
    // max_memory to 10 MB. AOT routes through the same `ops::mul`
    // → builtins path, so the error message is identical.
    let run = run_aot_with_opts(
        r#"let s = "x" * 99999999
print(s.len())"#,
        "sandbox_memory_bomb",
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_ne!(run.status, Some(0), "expected non-zero exit; stderr:\n{}", run.stderr);
    assert!(
        run.stderr.contains("Memory limit"),
        "expected 'Memory limit' in stderr; got:\n{}",
        run.stderr
    );
}

#[test]
#[ignore]
fn e2e_sandbox_recursion_halts() {
    // Generated AOT now shares the walker/VM MAX_CALL_DEPTH = 64
    // boundary and reports the canonical recoverable depth error
    // before the native Rust stack is at risk.
    let run = run_aot_with_opts(
        "fn f() { f() }\nf()",
        "sandbox_recursion",
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_ne!(run.status, Some(0), "expected non-zero exit; stderr:\n{}", run.stderr);
    assert!(
        run.stderr.contains("nested function calls"),
        "expected the call-depth diagnostic in stderr; got:\n{}",
        run.stderr
    );
}

#[test]
#[ignore]
fn e2e_sandbox_rejects_deep_value_hidden_in_lambda_without_aborting() {
    // AOT lambdas retain captures inside the generated Rust callable rather
    // than in `BopFn::captures`. Build a value at depth 63, capture it (making
    // the function depth 64), then try to wrap that fn in an array (depth 65).
    // The generated binary must return the fatal Bop diagnostic with a normal
    // exit code, not overflow the native stack or abort by signal.
    let run = run_aot_with_opts(
        r#"let value = none
repeat 63 { value = [value] }
let f = fn() { return value }
let too_deep = [f]
print(too_deep)"#,
        "sandbox_deep_opaque_capture",
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(
        run.status,
        Some(1),
        "expected a clean Bop error exit, not an abort; stderr:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains(bop::value::VALUE_DEPTH_ERROR_MESSAGE),
        "expected value-depth diagnostic; got:\n{}",
        run.stderr
    );
}

#[test]
#[ignore]
fn e2e_sandbox_counts_namespaced_module_capture_in_lambda_depth() {
    let run = run_aot_with_modules_and_opts(
        "use shapes as s\nlet f = fn() { return s.Box { value: none } }\nprint(f)",
        "sandbox_deep_namespaced_module_capture",
        &[(
            "shapes",
            "struct Box { value }\nlet deep = none\nrepeat 63 { deep = [deep] }",
        )],
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(
        run.status,
        Some(1),
        "expected a clean depth error exit; stderr:\n{}",
        run.stderr
    );
    assert!(
        run.stderr.contains(bop::value::VALUE_DEPTH_ERROR_MESSAGE),
        "expected value-depth diagnostic; got:\n{}",
        run.stderr
    );
}

#[test]
#[ignore]
fn e2e_nested_lambda_param_shadow_does_not_capture_deep_outer_value() {
    assert_aot_matches(
        "nested_lambda_param_shadow_depth",
        r#"let x = none
repeat 64 { x = [x] }
let outer = fn(x) { return fn() { return x } }
print(outer(none)())"#,
    );
}

// ─── Closures / first-class fns ───────────────────────────────

// ─── Imports (phase 2c) ──────────────────────────────────────────

/// Compare AOT output against a walker run that resolves modules
/// from the same in-memory table. Used by the use tests —
/// lets the same map drive both engines so we can assert they
/// produce identical output.
fn assert_aot_matches_with_modules(
    test_name: &str,
    code: &str,
    modules: &[(&str, &str)],
) {
    // Walker reference — run against a host backed by the map.
    struct MapHost<'a> {
        prints: std::cell::RefCell<Vec<String>>,
        modules: std::collections::HashMap<String, String>,
        _marker: std::marker::PhantomData<&'a ()>,
    }
    impl<'a> BopHost for MapHost<'a> {
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
            self.modules.get(name).cloned().map(Ok)
        }
    }
    let mut walker_host = MapHost {
        prints: std::cell::RefCell::new(Vec::new()),
        modules: modules
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        _marker: std::marker::PhantomData,
    };
    bop::run(code, &mut walker_host, &BopLimits::standard())
        .expect("walker failed");
    let expected = walker_host.prints.borrow().join("\n");

    let resolver = modules_from_map(modules.iter().map(|(k, v)| (*k, *v)));
    let rust_src = transpile(
        code,
        &Options {
            module_resolver: Some(resolver),
            ..Options::default()
        },
    )
    .expect("transpile");
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
            "cargo run failed for {}:\n--- stderr ---\n{}\n--- generated ---\n{}",
            test_name,
            String::from_utf8_lossy(&output.stderr),
            rust_src,
        );
    }
    let actual = String::from_utf8_lossy(&output.stdout)
        .trim_end_matches('\n')
        .to_string();
    assert_eq!(
        actual, expected,
        "AOT output diverged from walker for {}:\n--- walker ---\n{}\n--- aot ---\n{}",
        test_name, expected, actual,
    );
}

fn assert_aot_compiles_without_warnings_with_modules(
    test_name: &str,
    code: &str,
    modules: &[(&str, &str)],
) {
    let resolver = modules_from_map(modules.iter().map(|(k, v)| (*k, *v)));
    let rust_src = transpile(
        code,
        &Options {
            module_resolver: Some(resolver),
            ..Options::default()
        },
    )
    .expect("transpile");
    let dir = write_scratch_project(test_name, &rust_src);
    let output = Command::new("cargo")
        .arg("rustc")
        .arg("--quiet")
        .arg("--release")
        .arg("--")
        .arg("-D")
        .arg("warnings")
        .current_dir(&dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("compile generated Rust with warnings denied");
    assert!(
        output.status.success(),
        "generated Rust failed a native -D warnings compile for {}:\n--- stderr ---\n{}\n--- generated ---\n{}",
        test_name,
        String::from_utf8_lossy(&output.stderr),
        rust_src,
    );
}

#[test]
#[ignore]
fn e2e_match_arms_and_imports_compile_without_rustc_warnings() {
    if !cargo_available() {
        eprintln!("cargo not available — skipping");
        return;
    }
    assert_aot_compiles_without_warnings_with_modules(
        "warning_free_match_arms_and_imports",
        r#"use warning_fixture
let unguarded = match flag { true => value + 1, _ => 0 }
let guarded = match flag { true if value > 0 => value + 2, _ => 0 }
print(unguarded, guarded)"#,
        &[("warning_fixture", "let flag = true\nlet value = 40")],
    );
}

#[test]
#[ignore]
fn e2e_dynamic_method_sites_compile_without_rustc_warnings() {
    if !cargo_available() {
        eprintln!("cargo not available — skipping");
        return;
    }
    assert_aot_compiles_without_warnings_with_modules(
        "warning_free_dynamic_method_sites",
        r#"use methods as api
let value = api.Item { value: 4 }
print(try_call(fn() { return value.read() }).is_err())
install()
print(value.read())"#,
        &[(
            "methods",
            r#"struct Item { value }
fn install() {
    if true { fn Item.read(self) { return self.value + 1 } }
}"#,
        )],
    );
}

#[test]
#[ignore]
fn e2e_dynamic_method_sites_compile_and_run_in_sandbox_mode() {
    let code = r#"struct Item { value }
fn Item.read(self) { return self.value + 1 }
print(Item { value: 4 }.read())"#;
    let run = run_aot_with_opts(
        code,
        "sandbox_dynamic_method_sites",
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(run.status, Some(0), "sandbox method program failed: {}", run.stderr);
    assert_eq!(run.stdout, walker_output(code));
}

#[test]
#[ignore]
fn e2e_import_basic_let() {
    assert_aot_matches_with_modules(
        "import_basic_let",
        r#"use math
print(pi)"#,
        &[("math", "let pi = 3")],
    );
}

#[test]
#[ignore]
fn e2e_import_named_fn() {
    assert_aot_matches_with_modules(
        "import_named_fn",
        r#"use math
print(square(7))"#,
        &[("math", "fn square(n) { return n * n }")],
    );
}

#[test]
#[ignore]
fn e2e_non_sandbox_named_fn_reads_and_mutates_root_bindings() {
    assert_aot_matches(
        "non_sandbox_named_fn_root_bindings",
        r#"let base = 5
const STEP = 2
let calls = 0
fn calculate(n) {
    calls += 1
    return base + STEP + n + calls
}
print(calculate(3))
print(calculate(3))
print(calls)"#,
    );
}

#[test]
#[ignore]
fn e2e_non_sandbox_module_fn_reads_its_module_bindings() {
    assert_aot_matches_with_modules(
        "non_sandbox_named_fn_module_bindings",
        r#"use counter
print(next())
print(next())"#,
        &[(
            "counter",
            r#"const STEP = 3
let value = 4
fn next() {
    value += STEP
    return value
}"#,
        )],
    );
}

#[test]
#[ignore]
fn e2e_non_sandbox_named_fns_call_bare_and_transitive_imports() {
    assert_aot_matches_with_modules(
        "non_sandbox_named_fn_bare_imports",
        r#"use outer
fn root_call(n) { return increment(n) }
print(root_call(10))
print(transitive(10))"#,
        &[
            (
                "outer",
                r#"use inner
fn transitive(n) { return increment(increment(n)) }"#,
            ),
            ("inner", "fn increment(n) { return n + 1 }"),
        ],
    );
}

#[test]
#[ignore]
fn e2e_import_dotted_path() {
    assert_aot_matches_with_modules(
        "import_dotted_path",
        r#"use std.math
print(e)"#,
        &[("std.math", "let e = 2")],
    );
}

#[test]
#[ignore]
fn e2e_import_transitive() {
    assert_aot_matches_with_modules(
        "import_transitive",
        r#"use a
print(doubled)"#,
        &[
            ("a", "use b\nlet doubled = pi + pi"),
            ("b", "let pi = 3"),
        ],
    );
}

#[test]
#[ignore]
fn e2e_import_idempotent_reload_cache() {
    // Second use shouldn't re-run the module body. The walker
    // caches; the AOT caches via the __mod_*_load fn's
    // module_cache check.
    assert_aot_matches_with_modules(
        "import_idempotent",
        r#"use m
use m
print(x)"#,
        &[("m", "let x = 42")],
    );
}

#[test]
#[ignore]
fn e2e_use_selective_items() {
    // `use m.{a, b}` brings only the listed exports in as locals.
    assert_aot_matches_with_modules(
        "use_selective_items",
        r#"use m.{pi, tau}
print(pi)
print(tau)"#,
        &[("m", "let pi = 3\nlet tau = 6\nlet unused = 99")],
    );
}

#[test]
#[ignore]
fn e2e_use_selective_reaches_private() {
    // Selective form can reach `_`-prefixed names; glob can't.
    assert_aot_matches_with_modules(
        "use_selective_private",
        r#"use m.{_helper}
print(_helper(5))"#,
        &[("m", "fn _helper(n) { return n * 10 }")],
    );
}

#[test]
#[ignore]
fn e2e_use_aliased_glob() {
    // `use m as n` — namespaced binding read + call through alias.
    assert_aot_matches_with_modules(
        "use_aliased_glob",
        r#"use m as n
print(n.pi)
print(n.double(7))"#,
        &[("m", "let pi = 3\nfn double(x) { return x + x }")],
    );
}

#[test]
#[ignore]
fn e2e_use_aliased_selective() {
    // `use m.{double} as n` — only `double` ends up on `n`.
    assert_aot_matches_with_modules(
        "use_aliased_selective",
        r#"use m.{double} as n
print(n.double(21))"#,
        &[("m", "let pi = 3\nfn double(x) { return x + x }")],
    );
}

#[test]
#[ignore]
fn e2e_use_namespaced_struct_construct() {
    // `n.Entity { ... }` constructs the aliased module's type.
    assert_aot_matches_with_modules(
        "use_namespaced_struct",
        r#"use m as n
let p = n.Point { x: 3, y: 4 }
print(p.x + p.y)"#,
        &[("m", "struct Point { x, y }")],
    );
}

#[test]
#[ignore]
fn e2e_two_modules_same_type_name_distinct_identity() {
    // Phase 2b — two modules both declare `enum Color { ... }`
    // with different variants. Under module-qualified types
    // they coexist as distinct runtime types. Equality never
    // fires across module boundaries even when both values are
    // named `Color::Red`.
    assert_aot_matches_with_modules(
        "two_modules_same_type_name",
        r#"use paint as p
use other as o
let a = p.Color::Red
let b = o.Color::Red
print(a == b)
print(a == a)
print(a)
print(b)"#,
        &[
            ("paint", "enum Color { Red, Blue }"),
            ("other", "enum Color { Red, Green, Yellow }"),
        ],
    );
}

#[test]
#[ignore]
fn e2e_namespaced_pattern_picks_correct_module() {
    // Patterns embed the resolved module path in the emitter's
    // per-site resolver closure; `p.Color::Red` only matches
    // values tagged with the paint module.
    assert_aot_matches_with_modules(
        "namespaced_pattern_picks_correct_module",
        r#"use paint as p
use other as o
fn label(c) {
    return match c {
        p.Color::Red => "paint-red",
        o.Color::Red => "other-red",
        _ => "none",
    }
}
print(label(p.Color::Red))
print(label(o.Color::Red))
print(label(p.Color::Blue))"#,
        &[
            ("paint", "enum Color { Red, Blue }"),
            ("other", "enum Color { Red, Green }"),
        ],
    );
}

#[test]
#[ignore]
fn e2e_use_namespaced_enum_construct() {
    // `n.Color::Red` and `n.Result::Ok(v)` via alias.
    assert_aot_matches_with_modules(
        "use_namespaced_enum",
        r#"use m as n
print(n.Color::Red)
print(n.Result::Ok(42))"#,
        &[(
            "m",
            "enum Color { Red, Green, Blue }\nenum Result { Ok(v), Err(e) }",
        )],
    );
}

// ─── Structs / enums / user methods ──────────────────────────────

#[test]
#[ignore]
fn e2e_struct_basic() {
    assert_aot_matches(
        "struct_basic",
        r#"struct Point { x, y }
let p = Point { x: 3, y: 4 }
print(p.x + p.y)
print(p)"#,
    );
}

#[test]
#[ignore]
fn e2e_struct_field_assign() {
    assert_aot_matches(
        "struct_field_assign",
        r#"struct Counter { n }
let c = Counter { n: 10 }
c.n += 5
c.n *= 2
print(c.n)"#,
    );
}

#[test]
#[ignore]
fn e2e_enum_variants() {
    assert_aot_matches(
        "enum_variants",
        r#"enum Shape { Circle(r), Rect { w, h }, Empty }
print(Shape::Circle(3))
print(Shape::Rect { w: 4, h: 3 })
print(Shape::Empty)"#,
    );
}

#[test]
#[ignore]
fn e2e_enum_struct_variant_field_access() {
    assert_aot_matches(
        "enum_struct_access",
        r#"enum Shape { Rect { w, h } }
let r = Shape::Rect { w: 4, h: 3 }
print(r.w * r.h)"#,
    );
}

#[test]
#[ignore]
fn e2e_method_on_struct() {
    assert_aot_matches(
        "method_struct",
        r#"struct Point { x, y }
fn Point.sum(self) { return self.x + self.y }
let p = Point { x: 3, y: 4 }
print(p.sum())"#,
    );
}

#[test]
#[ignore]
fn e2e_method_chain() {
    assert_aot_matches(
        "method_chain",
        r#"struct Adder { n }
fn Adder.then(self, m) { return Adder { n: self.n + m } }
let r = Adder { n: 1 }.then(2).then(3).then(4)
print(r.n)"#,
    );
}

#[test]
#[ignore]
fn e2e_method_on_enum() {
    assert_aot_matches(
        "method_enum",
        r#"enum Shape { Circle(r), Rect { w, h } }
fn Shape.label(self) { return "shape" }
print(Shape::Circle(5).label())
print(Shape::Rect { w: 4, h: 3 }.label())"#,
    );
}

#[test]
#[ignore]
fn e2e_method_overrides_builtin() {
    assert_aot_matches(
        "method_override",
        r#"struct Wrapper { data }
fn Wrapper.len(self) { return 99 }
let w = Wrapper { data: [1, 2, 3] }
print(w.len())"#,
    );
}

#[test]
#[ignore]
fn e2e_closure_basic_lambda() {
    assert_aot_matches(
        "closure_basic",
        r#"let double = fn(x) { return x * 2 }
print(double(5))
print(double(21))"#,
    );
}

#[test]
#[ignore]
fn e2e_closure_captures_value() {
    assert_aot_matches(
        "closure_captures",
        r#"let n = 5
let add_n = fn(x) { return x + n }
print(add_n(3))
n = 100
print(add_n(3))"#,
    );
}

#[test]
#[ignore]
fn e2e_closure_factory() {
    assert_aot_matches(
        "closure_factory",
        r#"fn make_adder(n) { return fn(x) { return x + n } }
let add5 = make_adder(5)
let add10 = make_adder(10)
print(add5(3))
print(add10(3))"#,
    );
}

#[test]
#[ignore]
fn e2e_named_fn_as_value() {
    assert_aot_matches(
        "named_fn_as_value",
        r#"fn double(x) { return x * 2 }
let f = double
print(f(7))"#,
    );
}

#[test]
#[ignore]
fn e2e_higher_order_apply() {
    assert_aot_matches(
        "higher_order",
        r#"fn apply(f, x) { return f(x) }
fn square(n) { return n * n }
print(apply(square, 4))
print(apply(fn(n) { return n + 1 }, 4))"#,
    );
}

#[test]
#[ignore]
fn e2e_iife() {
    assert_aot_matches(
        "iife",
        "print((fn(x) { return x * 3 })(4))",
    );
}

#[test]
#[ignore]
fn e2e_builtins_str_int_type() {
    assert_aot_matches(
        "builtins",
        r#"print(42.to_str())
print(3.7.to_int())
print("hi".type())
print(42.type())
print((-7).abs())
print(3.min(7))
print(3.max(7))
print([1, 2, 3].len())"#,
    );
}

#[test]
#[ignore]
fn e2e_range_boundaries_steps_and_i64_edges() {
    let output = run_aot(
        r#"let boundary = range(10000)
let ascending = range(-7, 29993, 3)
let descending = range(29993, -7, -3)
let min = -9223372036854775807 - 1
let max = 9223372036854775807
print(boundary.len())
print(boundary[9999])
print(ascending.len())
print(ascending[9999])
print(descending.len())
print(descending[9999])
print(range(5, 0, 1))
print(range(0, 5, -1))
print(range(min, max, max))
print(range(max, min, min))"#,
        "range_boundaries_steps_and_i64_edges",
    );
    assert_eq!(
        output,
        concat!(
            "10000\n",
            "9999\n",
            "10000\n",
            "29990\n",
            "10000\n",
            "-4\n",
            "[]\n",
            "[]\n",
            "[-9223372036854775808, -1, 9223372036854775806]\n",
            "[9223372036854775807, -1]"
        )
    );
}

#[test]
#[ignore]
fn e2e_i64_min_literal_stays_exact_through_native_aot() {
    let output = run_aot(
        r#"let min = -9223372036854775808
print(min)
print(min.type())
print(min + 1)
print(min < -9223372036854775807)
print(match min {
    -9223372036854775808 => "minimum",
    _ => "other",
})"#,
        "i64_min_literal_exact",
    );
    assert_eq!(
        output,
        "-9223372036854775808\nint\n-9223372036854775807\ntrue\nminimum"
    );
}

#[test]
#[ignore]
fn e2e_i64_min_literal_keeps_native_overflow_checks() {
    for (source, name) in [
        ("print(--9223372036854775808)", "i64_min_neg_overflow"),
        (
            "print(-9223372036854775808 - 1)",
            "i64_min_sub_overflow",
        ),
    ] {
        let run = run_aot_with_opts(source, name, &Options::default());
        assert_eq!(run.status, Some(1), "stderr:\n{}", run.stderr);
        assert!(run.stdout.is_empty(), "unexpected stdout: {}", run.stdout);
        assert!(
            run.stderr.contains("[line 1] Integer overflow in `-`"),
            "unexpected stderr:\n{}",
            run.stderr
        );
    }
}

#[test]
#[ignore]
fn e2e_range_limit_is_fatal_through_try_call() {
    let run = run_aot_with_opts(
        r#"let result = try_call(fn() {
    return range(10001)
})
print("unreachable")"#,
        "range_limit_is_fatal_through_try_call",
        &Options {
            sandbox: true,
            ..Options::default()
        },
    );
    assert_eq!(
        run.status,
        Some(1),
        "expected a clean Bop error exit, not an abort; stderr:\n{}",
        run.stderr
    );
    assert!(run.stdout.is_empty(), "unexpected stdout: {}", run.stdout);
    assert!(
        run.stderr.contains(&format!(
            "[line 2] {}",
            bop::builtins::RANGE_LIMIT_ERROR_MESSAGE
        )),
        "range-limit diagnostic had the wrong message or source line; stderr:\n{}",
        run.stderr
    );
}

#[test]
#[ignore]
fn e2e_top_level_try_renders_the_shared_friendly_hint() {
    let source = r#"enum Result { Ok(value), Err(error) }
let value = try Result::Err("boom")"#;
    let cases = [
        ("standard", Options::default()),
        (
            "sandbox",
            Options {
                sandbox: true,
                ..Options::default()
            },
        ),
    ];

    for (mode, options) in cases {
        let run = run_aot_with_opts(
            source,
            &format!("top_level_try_friendly_hint_{mode}"),
            &options,
        );
        assert_eq!(run.status, Some(1), "{mode} stderr:\n{}", run.stderr);
        assert!(
            run.stdout.is_empty(),
            "{mode} unexpected stdout: {}",
            run.stdout
        );
        assert!(
            run.stderr.contains(&format!(
                "[line 2] {}",
                bop::error_messages::TOP_LEVEL_TRY_ERROR_MESSAGE
            )),
            "{mode} missing canonical message or source line:\n{}",
            run.stderr
        );
        assert_eq!(
            run.stderr
                .matches(bop::error_messages::TOP_LEVEL_TRY_ERROR_MESSAGE)
                .count(),
            1,
            "{mode} printed the message more than once:\n{}",
            run.stderr
        );
        assert_eq!(
            run.stderr
                .matches(&format!(
                    "hint: {}",
                    bop::error_messages::TOP_LEVEL_TRY_HINT
                ))
                .count(),
            1,
            "{mode} did not print exactly one canonical hint:\n{}",
            run.stderr
        );
    }
}
