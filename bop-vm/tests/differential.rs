//! Differential tests — every program runs through both the
//! tree-walking evaluator in `bop-lang` and the bytecode VM in
//! `bop-vm`, and the two engines must agree on prints and on whether
//! (and why) the program errored.
//!
//! This is step 2c of the execution-modes plan: the engines are
//! compared against each other on every test, so the second one can
//! never silently drift from the first.
//!
//! # Comparison rules
//!
//! - Prints: strict `Vec<String>` equality.
//! - Success/failure: must agree. If one engine errors and the other
//!   succeeds, the harness panics with both outcomes side by side.
//! - Error messages: compared on `BopError::message` text only.
//!   Line numbers legitimately diverge (the walker emits `line: 0`
//!   for `Signal::Break` / `Signal::Continue` that bubble up to
//!   `run()`; the VM compiler catches the same cases with the real
//!   source line). Columns and friendly hints are not part of the
//!   contract.
//! - Resource-limit errors (`too many steps`, `Memory limit`) count
//!   the same way — the existing safety tests assert via substring,
//!   so the slightly different step-counts between engines don't
//!   matter as long as *some* limit-based error fires on both.
//!
//! # Fuzzer
//!
//! At the bottom of this file, [`fuzz_smoke_diff`] generates random
//! programs from a constrained grammar (no functions, no `while`, no
//! methods — just literals / arithmetic / logic / `let` / assign /
//! `print` / `if` / `repeat` / arrays) and runs them through both
//! engines. Seeded RNG, deterministic, 100 programs per run. The
//! extended budget is behind `#[ignore]` for local / nightly use.

use std::cell::RefCell;

use bop::{BopError, BopHost, BopLimits, Value};

// ─── Harness ───────────────────────────────────────────────────────

/// Result of running a single program through one engine.
#[derive(Debug, Clone)]
struct Outcome {
    prints: Vec<String>,
    /// `None` = success; `Some(message)` = runtime or compile error.
    error: Option<String>,
}

impl Outcome {
    fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

struct RecordHost {
    prints: RefCell<Vec<String>>,
}

impl RecordHost {
    fn new() -> Self {
        Self {
            prints: RefCell::new(Vec::new()),
        }
    }
}

impl BopHost for RecordHost {
    fn call(&mut self, _name: &str, _args: &[Value], _line: u32) -> Option<Result<Value, BopError>> {
        None
    }

    fn on_print(&mut self, message: &str) {
        self.prints.borrow_mut().push(message.to_string());
    }

    fn resolve_module(&mut self, name: &str) -> Option<Result<String, BopError>> {
        MODULES.with(|m| m.borrow().get(name).cloned().map(Ok))
    }
}

// Per-test module table — tests that use imports populate it via
// `set_modules` and the `resolve_module` impl above reads from it.
// Kept in a thread-local so the simple `RecordHost` struct stays
// shareable between walker and VM runs.
thread_local! {
    static MODULES: std::cell::RefCell<std::collections::HashMap<String, String>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

fn set_modules(modules: &[(&str, &str)]) {
    MODULES.with(|m| {
        let mut map = m.borrow_mut();
        map.clear();
        for (name, src) in modules {
            map.insert((*name).to_string(), (*src).to_string());
        }
    });
}

/// Run `code` through both engines with the given limits. Both
/// engines must agree on prints and on success/failure; otherwise
/// the harness panics with a diff. Returns the shared outcome so
/// callers can inspect prints or the error message.
fn run_both(code: &str, limits: &BopLimits) -> Outcome {
    let tw = {
        let mut host = RecordHost::new();
        let result = bop::run(code, &mut host, limits);
        Outcome {
            prints: host.prints.borrow().clone(),
            error: result.err().map(|e| e.message),
        }
    };
    let vm = {
        let mut host = RecordHost::new();
        let result = bop_vm::run(code, &mut host, limits);
        Outcome {
            prints: host.prints.borrow().clone(),
            error: result.err().map(|e| e.message),
        }
    };

    if tw.prints != vm.prints {
        panic!(
            "prints diverged on:\n{}\n\ntree-walker: {:?}\nbytecode vm: {:?}",
            code, tw.prints, vm.prints
        );
    }

    match (&tw.error, &vm.error) {
        (None, None) => {}
        (Some(a), Some(b)) if a == b => {}
        (Some(a), Some(b)) => panic!(
            "error messages diverged on:\n{}\n\ntree-walker: {}\nbytecode vm: {}",
            code, a, b
        ),
        (Some(a), None) => panic!(
            "tree-walker errored but VM succeeded on:\n{}\n\ntree-walker error: {}\nvm prints: {:?}",
            code, a, vm.prints
        ),
        (None, Some(b)) => panic!(
            "VM errored but tree-walker succeeded on:\n{}\n\nvm error: {}\ntw prints: {:?}",
            code, b, tw.prints
        ),
    }

    tw
}

fn standard() -> BopLimits {
    BopLimits::standard()
}

fn tight() -> BopLimits {
    BopLimits {
        max_steps: 500,
        max_memory: 64 * 1024,
    }
}

/// Run both engines, assert success, return last print.
fn say(code: &str) -> String {
    let out = run_both(code, &standard());
    assert!(
        out.is_ok(),
        "program errored ({}): {}",
        out.error.as_deref().unwrap_or(""),
        code
    );
    out.prints
        .last()
        .cloned()
        .unwrap_or_else(|| panic!("no print output for: {}", code))
}

/// Run both engines, assert failure, return error message.
fn run_err(code: &str) -> String {
    let out = run_both(code, &standard());
    out.error
        .unwrap_or_else(|| panic!("expected an error, but program succeeded: {}", code))
}

fn run_err_with_limits(code: &str, limits: BopLimits) -> String {
    let out = run_both(code, &limits);
    out.error
        .unwrap_or_else(|| panic!("expected an error, but program succeeded: {}", code))
}

/// Run both engines with the given limits, assert both errored, and
/// return `(tree_walker_message, vm_message)`.
///
/// Use this for resource-limit tests where the engines legitimately
/// halt on different error classes — e.g. a memory bomb that also
/// churns through enough bytecode ops to trip the step budget. The
/// existing safety tests already tolerate either error class via
/// `contains("Memory limit") || contains("too many steps")`, so we
/// just apply that check to both messages independently rather than
/// requiring exact equality.
fn run_err_loose(code: &str, limits: BopLimits) -> (String, String) {
    let tw = {
        let mut host = RecordHost::new();
        bop::run(code, &mut host, &limits).err().map(|e| e.message)
    };
    let vm = {
        let mut host = RecordHost::new();
        bop_vm::run(code, &mut host, &limits)
            .err()
            .map(|e| e.message)
    };
    match (tw, vm) {
        (Some(a), Some(b)) => (a, b),
        (None, Some(_)) => panic!("tree-walker succeeded on:\n{}", code),
        (Some(_), None) => panic!("bytecode vm succeeded on:\n{}", code),
        (None, None) => panic!("both engines succeeded on:\n{}", code),
    }
}

/// Helper for safety tests: assert the `"Memory limit"` OR
/// `"too many steps"` bailout fires on both engines.
#[track_caller]
fn assert_both_resource_limit(tw: &str, vm: &str) {
    for (engine, msg) in [("tree-walker", tw), ("bytecode vm", vm)] {
        assert!(
            msg.contains("Memory limit") || msg.contains("too many steps"),
            "{} did not hit a resource limit; got: {}",
            engine,
            msg
        );
    }
}

// ─── Arithmetic ───────────────────────────────────────────────────

#[test]
fn add_numbers() {
    assert_eq!(say("print(1 + 2)"), "3");
}

#[test]
fn subtract() {
    assert_eq!(say("print(10 - 3)"), "7");
}

#[test]
fn multiply() {
    assert_eq!(say("print(4 * 5)"), "20");
}

#[test]
fn divide_float() {
    assert_eq!(say("print(7 / 2)"), "3.5");
}

#[test]
fn divide_whole() {
    assert_eq!(say("print(6 / 2)"), "3");
}

#[test]
fn modulo() {
    assert_eq!(say("print(10 % 3)"), "1");
}

#[test]
fn precedence() {
    assert_eq!(say("print(2 + 3 * 4)"), "14");
}

#[test]
fn parentheses() {
    assert_eq!(say("print((2 + 3) * 4)"), "20");
}

#[test]
fn unary_neg() {
    assert_eq!(say("print(-5)"), "-5");
}

#[test]
fn unary_not() {
    assert_eq!(say("print(!true)"), "false");
}

// ─── Strings ──────────────────────────────────────────────────────

#[test]
fn string_concat() {
    assert_eq!(say(r#"print("hello" + " " + "world")"#), "hello world");
}

#[test]
fn string_repeat() {
    assert_eq!(say(r#"print("ab" * 3)"#), "ababab");
}

#[test]
fn string_interpolation() {
    assert_eq!(
        say(r#"let name = "bop"
print("hi {name}!")"#),
        "hi bop!"
    );
}

#[test]
fn string_auto_coerce_in_add() {
    assert_eq!(say(r#"print("val=" + 42)"#), "val=42");
}

// ─── Comparisons & Logic ──────────────────────────────────────────

#[test]
fn equality() {
    assert_eq!(say("print(1 == 1)"), "true");
    assert_eq!(say("print(1 == 2)"), "false");
    assert_eq!(say("print(1 != 2)"), "true");
}

#[test]
fn ordering() {
    assert_eq!(say("print(3 < 5)"), "true");
    assert_eq!(say("print(5 <= 5)"), "true");
    assert_eq!(say("print(6 > 5)"), "true");
    assert_eq!(say("print(5 >= 6)"), "false");
}

#[test]
fn logical_and_or() {
    assert_eq!(say("print(true && false)"), "false");
    assert_eq!(say("print(true || false)"), "true");
}

#[test]
fn short_circuit_and() {
    assert_eq!(say("print(false && x)"), "false");
}

#[test]
fn short_circuit_or() {
    assert_eq!(say("print(true || x)"), "true");
}

// ─── Variables ────────────────────────────────────────────────────

#[test]
fn let_and_use() {
    assert_eq!(say("let x = 10\nprint(x)"), "10");
}

#[test]
fn assign() {
    assert_eq!(say("let x = 1\nx = 5\nprint(x)"), "5");
}

#[test]
fn compound_assign() {
    assert_eq!(say("let x = 10\nx += 5\nprint(x)"), "15");
    assert_eq!(say("let x = 10\nx -= 3\nprint(x)"), "7");
    assert_eq!(say("let x = 4\nx *= 3\nprint(x)"), "12");
    assert_eq!(say("let x = 10\nx /= 4\nprint(x)"), "2.5");
    assert_eq!(say("let x = 10\nx %= 3\nprint(x)"), "1");
}

#[test]
fn undefined_variable_error() {
    assert!(run_err("print(nope)").contains("not found"));
}

#[test]
fn assign_undeclared_error() {
    assert!(run_err("x = 5").contains("doesn't exist"));
}

// ─── If / Else ────────────────────────────────────────────────────

#[test]
fn if_true_branch() {
    assert_eq!(say("if true { print(\"yes\") } else { print(\"no\") }"), "yes");
}

#[test]
fn if_false_branch() {
    assert_eq!(say("if false { print(\"yes\") } else { print(\"no\") }"), "no");
}

#[test]
fn if_else_if() {
    assert_eq!(
        say(r#"let x = 2
if x == 1 { print("one") } else if x == 2 { print("two") } else { print("other") }"#),
        "two"
    );
}

#[test]
fn if_expression() {
    assert_eq!(say("let x = if true { 1 } else { 2 }\nprint(x)"), "1");
}

// ─── While ────────────────────────────────────────────────────────

#[test]
fn while_loop() {
    assert_eq!(say("let i = 0\nwhile i < 5 { i += 1 }\nprint(i)"), "5");
}

#[test]
fn while_break() {
    assert_eq!(
        say("let i = 0\nwhile true { i += 1\nif i == 3 { break } }\nprint(i)"),
        "3"
    );
}

#[test]
fn while_continue() {
    assert_eq!(
        say(r#"let sum = 0
let i = 0
while i < 10 {
    i += 1
    if i % 2 == 0 { continue }
    sum += i
}
print(sum)"#),
        "25"
    );
}

// ─── For ──────────────────────────────────────────────────────────

#[test]
fn for_over_array() {
    assert_eq!(
        say(r#"let sum = 0
for x in [10, 20, 30] { sum += x }
print(sum)"#),
        "60"
    );
}

#[test]
fn for_over_range() {
    assert_eq!(
        say("let sum = 0\nfor i in range(5) { sum += i }\nprint(sum)"),
        "10"
    );
}

#[test]
fn for_over_string() {
    assert_eq!(
        say(r#"let out = ""
for ch in "abc" { out += ch + "-" }
print(out)"#),
        "a-b-c-"
    );
}

#[test]
fn for_with_break() {
    assert_eq!(
        say("let last = 0\nfor i in range(100) { if i == 3 { break }\nlast = i }\nprint(last)"),
        "2"
    );
}

// ─── Repeat ───────────────────────────────────────────────────────

#[test]
fn repeat_loop() {
    assert_eq!(say("let n = 0\nrepeat 4 { n += 1 }\nprint(n)"), "4");
}

#[test]
fn repeat_zero() {
    assert_eq!(say("let n = 99\nrepeat 0 { n = 0 }\nprint(n)"), "99");
}

// ─── Functions ────────────────────────────────────────────────────

#[test]
fn fn_basic() {
    assert_eq!(say("fn double(x) { return x * 2 }\nprint(double(5))"), "10");
}

#[test]
fn fn_implicit_return_none() {
    assert_eq!(
        say(r#"fn noop() { let x = 1 }
print(type(noop()))"#),
        "none"
    );
}

#[test]
fn fn_multiple_params() {
    assert_eq!(say("fn add(a, b) { return a + b }\nprint(add(3, 7))"), "10");
}

#[test]
fn fn_scope_isolation() {
    assert!(
        run_err(
            r#"let secret = 42
fn peek() { return secret }
peek()"#
        )
        .contains("not found")
    );
}

#[test]
fn fn_wrong_arg_count() {
    assert!(run_err("fn f(a, b) { return a }\nf(1)").contains("expects 2"));
}

#[test]
fn fn_recursion() {
    assert_eq!(
        say(r#"fn fib(n) {
    if n <= 1 { return n }
    return fib(n - 1) + fib(n - 2)
}
print(fib(10))"#),
        "55"
    );
}

// ─── Structs ───────────────────────────────────────────────────────

#[test]
fn struct_basic_diff() {
    assert_eq!(
        say(r#"struct Point { x, y }
let p = Point { x: 3, y: 4 }
print(p.x + p.y)"#),
        "7"
    );
}

#[test]
fn struct_display_and_equality_diff() {
    assert_eq!(
        say(r#"struct Point { x, y }
let a = Point { x: 1, y: 2 }
let b = Point { x: 1, y: 2 }
print(a)
print(a == b)"#),
        "true"
    );
}

#[test]
fn struct_field_assign_diff() {
    assert_eq!(
        say(r#"struct Counter { n }
let c = Counter { n: 10 }
c.n += 5
c.n *= 2
print(c.n)"#),
        "30"
    );
}

#[test]
fn struct_nested_diff() {
    assert_eq!(
        say(r#"struct Inner { v }
struct Outer { name, inner }
let o = Outer { name: "a", inner: Inner { v: 42 } }
print(o.inner.v)"#),
        "42"
    );
}

#[test]
fn struct_missing_field_error_diff() {
    let msg = run_err(r#"struct P { x, y }
let p = P { x: 1 }"#);
    assert!(msg.contains("Missing field"), "got: {}", msg);
}

// ─── Enums ─────────────────────────────────────────────────────────

#[test]
fn enum_unit_variant_diff() {
    assert_eq!(
        say(r#"enum E { A, B }
print(E::A == E::A)
print(E::A == E::B)"#),
        "false"
    );
}

#[test]
fn enum_tuple_variant_diff() {
    assert_eq!(
        say(r#"enum Pair { Pt(x, y) }
let p = Pair::Pt(3, 4)
print(p)"#),
        "Pair::Pt(3, 4)"
    );
}

#[test]
fn enum_struct_variant_with_field_access_diff() {
    assert_eq!(
        say(r#"enum Shape { Rect { w, h } }
let r = Shape::Rect { w: 4, h: 3 }
print(r.w * r.h)"#),
        "12"
    );
}

#[test]
fn enum_structural_equality_diff() {
    assert_eq!(
        say(r#"enum E { A, B(n) }
print(E::B(1) == E::B(1))
print(E::B(1) == E::B(2))
print(E::A == E::B(1))"#),
        "false"
    );
}

#[test]
fn enum_variant_arity_mismatch_diff() {
    let msg = run_err(r#"enum E { P(a, b) }
let x = E::P(1)"#);
    assert!(msg.contains("expects 2"), "got: {}", msg);
}

// ─── User-defined methods ─────────────────────────────────────────

#[test]
fn method_on_struct_diff() {
    assert_eq!(
        say(r#"struct Point { x, y }
fn Point.sum(self) { return self.x + self.y }
let p = Point { x: 3, y: 4 }
print(p.sum())"#),
        "7"
    );
}

#[test]
fn method_chain_diff() {
    assert_eq!(
        say(r#"struct Adder { n }
fn Adder.then(self, m) { return Adder { n: self.n + m } }
let r = Adder { n: 1 }.then(2).then(3).then(4)
print(r.n)"#),
        "10"
    );
}

#[test]
fn method_on_enum_diff() {
    assert_eq!(
        say(r#"enum Shape { Circle(r), Rect { w, h } }
fn Shape.label(self) { return "shape" }
print(Shape::Circle(5).label())
print(Shape::Rect { w: 4, h: 3 }.label())"#),
        "shape"
    );
}

#[test]
fn method_overrides_builtin_diff() {
    assert_eq!(
        say(r#"struct Wrapper { data }
fn Wrapper.len(self) { return 99 }
let w = Wrapper { data: [1, 2, 3] }
print(w.len())"#),
        "99"
    );
}

#[test]
fn method_self_value_semantics_diff() {
    // Mutations to `self` inside a method don't propagate;
    // matching behaviour across walker and VM is the contract.
    assert_eq!(
        say(r#"struct Counter { n }
fn Counter.bump(self) { self.n = self.n + 1 }
let c = Counter { n: 5 }
c.bump()
print(c.n)"#),
        "5"
    );
}

// ─── Pattern matching ─────────────────────────────────────────────

#[test]
fn match_literal_number_diff() {
    assert_eq!(
        say(r#"let x = 2
let r = match x {
  1 => "one",
  2 => "two",
  _ => "other",
}
print(r)"#),
        "two"
    );
}

#[test]
fn match_wildcard_catches_all_diff() {
    assert_eq!(
        say(r#"let x = 42
let r = match x {
  0 => "zero",
  _ => "big",
}
print(r)"#),
        "big"
    );
}

#[test]
fn match_binding_captures_scrutinee_diff() {
    assert_eq!(
        say(r#"let x = 7
let r = match x {
  n => n * 2,
}
print(r)"#),
        "14"
    );
}

#[test]
fn match_guard_selects_arm_diff() {
    assert_eq!(
        say(r#"let x = 10
let r = match x {
  n if n < 5 => "small",
  n if n < 20 => "medium",
  _ => "big",
}
print(r)"#),
        "medium"
    );
}

#[test]
fn match_or_pattern_diff() {
    assert_eq!(
        say(r#"let x = 3
let r = match x {
  1 | 2 | 3 => "low",
  _ => "other",
}
print(r)"#),
        "low"
    );
}

#[test]
fn match_enum_unit_variant_diff() {
    assert_eq!(
        say(r#"enum Light { Red, Green }
let l = Light::Green
let r = match l {
  Light::Red => "stop",
  Light::Green => "go",
}
print(r)"#),
        "go"
    );
}

#[test]
fn match_enum_tuple_binds_diff() {
    assert_eq!(
        say(r#"enum Result { Ok(v), Err(m) }
let r = Result::Ok(42)
let out = match r {
  Result::Ok(v) => v,
  Result::Err(_) => -1,
}
print(out)"#),
        "42"
    );
}

#[test]
fn match_enum_struct_variant_binds_diff() {
    assert_eq!(
        say(r#"enum Shape { Rect { w, h } }
let s = Shape::Rect { w: 4, h: 3 }
let a = match s {
  Shape::Rect { w, h } => w * h,
}
print(a)"#),
        "12"
    );
}

#[test]
fn match_struct_destructure_diff() {
    assert_eq!(
        say(r#"struct Point { x, y }
let p = Point { x: 3, y: 4 }
let r = match p {
  Point { x, y } => x + y,
}
print(r)"#),
        "7"
    );
}

#[test]
fn match_struct_partial_rest_diff() {
    assert_eq!(
        say(r#"struct Point { x, y, z }
let p = Point { x: 1, y: 2, z: 3 }
let r = match p {
  Point { y, .. } => y * 10,
}
print(r)"#),
        "20"
    );
}

#[test]
fn match_nested_enum_struct_diff() {
    assert_eq!(
        say(r#"enum FileError { NotFound(path) }
enum Result { Ok(v), Err(e) }
let r = Result::Err(FileError::NotFound("missing.txt"))
let msg = match r {
  Result::Ok(_) => "ok",
  Result::Err(FileError::NotFound(p)) => p,
}
print(msg)"#),
        "missing.txt"
    );
}

#[test]
fn match_array_exact_diff() {
    assert_eq!(
        say(r#"let xs = [1, 2, 3]
let r = match xs {
  [a, b, c] => a + b + c,
  _ => -1,
}
print(r)"#),
        "6"
    );
}

#[test]
fn match_array_with_rest_diff() {
    assert_eq!(
        say(r#"let xs = [10, 20, 30, 40]
let r = match xs {
  [first, ..rest] => first,
  _ => -1,
}
print(r)"#),
        "10"
    );
}

#[test]
fn match_no_arm_errors_diff() {
    let msg = run_err(
        r#"let x = 99
match x {
  1 => print("one"),
  2 => print("two"),
}"#,
    );
    assert!(
        msg.contains("No match arm matched the scrutinee"),
        "got: {}",
        msg
    );
}

#[test]
fn match_guard_rejects_then_next_arm_diff() {
    // Guard rejects the first arm even though the pattern matched,
    // so the scrutinee falls through to the wildcard.
    assert_eq!(
        say(r#"let x = 3
let r = match x {
  n if n > 100 => "huge",
  _ => "small",
}
print(r)"#),
        "small"
    );
}

#[test]
fn match_binding_scope_isolated_diff() {
    // `n` bound inside the arm doesn't leak to outer scope —
    // both engines must agree this errors cleanly.
    let msg = run_err(
        r#"let x = 5
match x {
  n => print(n),
}
print(n)"#,
    );
    assert!(msg.contains("not found") || msg.contains("Variable"), "got: {}", msg);
}

// ─── `try` operator ───────────────────────────────────────────────

#[test]
fn try_unwraps_ok_diff() {
    assert_eq!(
        say(r#"enum Result { Ok(v), Err(e) }
fn doit() {
    let v = try Result::Ok(42)
    return v
}
print(doit())"#),
        "42"
    );
}

#[test]
fn try_propagates_err_diff() {
    assert_eq!(
        say(r#"enum Result { Ok(v), Err(e) }
fn doit() {
    let v = try Result::Err("boom")
    return Result::Ok(v)
}
let r = doit()
print(match r {
    Result::Ok(v) => v,
    Result::Err(e) => e,
})"#),
        "boom"
    );
}

#[test]
fn try_chains_through_nested_calls_diff() {
    assert_eq!(
        say(r#"enum Result { Ok(v), Err(e) }
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
})"#),
        "leaf-err"
    );
}

#[test]
fn try_ok_unit_variant_diff() {
    assert_eq!(
        say(r#"enum Result { Ok, Err(e) }
fn doit() {
    let v = try Result::Ok
    return type(v)
}
print(doit())"#),
        "none"
    );
}

#[test]
fn try_inside_lambda_returns_from_lambda_diff() {
    assert_eq!(
        say(r#"enum Result { Ok(v), Err(e) }
let f = fn() {
    let v = try Result::Err("inner")
    return Result::Ok(v)
}
let r = f()
print(match r {
    Result::Ok(_) => "ok",
    Result::Err(e) => e,
})"#),
        "inner"
    );
}

#[test]
fn try_on_non_result_errors_diff() {
    let msg = run_err(
        r#"fn doit() {
    let v = try 42
    return v
}
doit()"#,
    );
    assert!(msg.contains("Result-shaped"), "got: {}", msg);
}

#[test]
fn try_at_top_level_on_err_errors_diff() {
    let msg = run_err(
        r#"enum Result { Ok(v), Err(e) }
let r = try Result::Err("boom")"#,
    );
    assert!(msg.contains("top-level"), "got: {}", msg);
}

#[test]
fn try_in_for_loop_short_circuits_diff() {
    assert_eq!(
        say(r#"enum Result { Ok(v), Err(e) }
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
})"#),
        "stop"
    );
}

// ─── Integer type (phase 6) ───────────────────────────────────────

#[test]
fn int_literal_type_diff() {
    assert_eq!(say("print(type(42))"), "int");
}

#[test]
fn float_literal_type_diff() {
    assert_eq!(say("print(type(42.0))"), "number");
}

#[test]
fn int_arithmetic_stays_int_diff() {
    assert_eq!(say("print(1 + 2)"), "3");
    assert_eq!(say("print(type(1 + 2))"), "int");
    assert_eq!(say("print(10 - 4)"), "6");
    assert_eq!(say("print(3 * 4)"), "12");
}

#[test]
fn int_div_slash_returns_number_diff() {
    assert_eq!(say("print(type(10 / 3))"), "number");
    assert_eq!(say("print(10 / 4)"), "2.5");
}

#[test]
fn int_div_slash_slash_returns_int_diff() {
    assert_eq!(say("print(type(10 // 3))"), "int");
    assert_eq!(say("print(10 // 3)"), "3");
    assert_eq!(say("print(-7 // 2)"), "-3");
}

#[test]
fn int_mixed_widens_to_number_diff() {
    assert_eq!(say("print(type(1 + 2.0))"), "number");
    assert_eq!(say("print(1 + 2.0)"), "3");
}

#[test]
fn int_number_equality_is_numeric_diff() {
    assert_eq!(say("print(1 == 1.0)"), "true");
    assert_eq!(say("print(2 > 1.5)"), "true");
}

#[test]
fn int_div_by_zero_errors_diff() {
    let msg = run_err("print(10 // 0)");
    assert!(msg.contains("Division by zero"), "got: {}", msg);
}

#[test]
fn int_overflow_errors_diff() {
    let msg = run_err("print(9223372036854775807 + 1)");
    assert!(msg.contains("Integer overflow"), "got: {}", msg);
}

#[test]
fn len_returns_int_diff() {
    assert_eq!(say(r#"print(type(len("hi")))"#), "int");
}

#[test]
fn range_int_elements_diff() {
    assert_eq!(say("print(type(range(3)[0]))"), "int");
}

#[test]
fn int_builtin_diff() {
    assert_eq!(say("print(int(3.7))"), "3");
    assert_eq!(say("print(type(int(3.7)))"), "int");
}

#[test]
fn float_builtin_diff() {
    assert_eq!(say("print(float(42))"), "42");
    assert_eq!(say("print(type(float(42)))"), "number");
}

#[test]
fn int_match_literal_diff() {
    assert_eq!(
        say(r#"let x = 2
print(match x {
    1 => "one",
    2 => "two",
    _ => "other",
})"#),
        "two"
    );
}

// ─── `try_call` builtin ───────────────────────────────────────────

#[test]
fn try_call_wraps_ok_diff() {
    assert_eq!(
        say(r#"let r = try_call(fn() { return 42 })
print(match r {
    Result::Ok(v) => v,
    Result::Err(_) => -1,
})"#),
        "42"
    );
}

#[test]
fn try_call_wraps_non_fatal_err_diff() {
    assert_eq!(
        say(r#"let r = try_call(fn() { return 1 / 0 })
print(match r {
    Result::Ok(_) => "ok",
    Result::Err(e) => e.message,
})"#),
        "Division by zero"
    );
}

#[test]
fn try_call_err_carries_line_diff() {
    assert_eq!(
        say(r#"let r = try_call(fn() {
    let x = 1
    return x / 0
})
print(match r {
    Result::Ok(_) => -1,
    Result::Err(e) => e.line,
})"#),
        "3"
    );
}

#[test]
fn try_call_composes_with_try_operator_diff() {
    assert_eq!(
        say(r#"fn risky(x) {
    let arr = [1, 2]
    return arr[x]
}
let r = try_call(fn() { return risky(5) })
print(match r {
    Result::Ok(_) => "ok",
    Result::Err(e) => e.message,
})"#),
        "Index 5 is out of bounds (array has 2 items)"
    );
}

#[test]
fn try_call_wrong_arg_count_errors_diff() {
    let msg = run_err("try_call()");
    assert!(msg.contains("try_call` expects 1"), "got: {}", msg);
}

#[test]
fn try_call_non_function_errors_diff() {
    let msg = run_err("try_call(42)");
    assert!(
        msg.contains("try_call` expects a function"),
        "got: {}",
        msg
    );
}

#[test]
fn try_call_nested_outer_catches_inner_err_as_ok_diff() {
    assert_eq!(
        say(r#"let r = try_call(fn() {
    let inner = try_call(fn() { return 1 / 0 })
    return inner
})
print(match r {
    Result::Ok(Result::Err(e)) => e.message,
    Result::Ok(Result::Ok(_)) => "inner ok?",
    Result::Err(_) => "outer caught",
})"#),
        "Division by zero"
    );
}

#[test]
fn try_call_step_limit_is_fatal_diff() {
    // Run through both engines with a tight step budget;
    // both must report the fatal step-limit error rather
    // than swallowing it via `try_call`.
    let tight = BopLimits {
        max_steps: 200,
        max_memory: 1 << 20,
    };
    let code = r#"let r = try_call(fn() {
    while true { }
})
print("should never run")"#;

    let tw = {
        let mut host = RecordHost::new();
        let result = bop::run(code, &mut host, &tight);
        (
            host.prints.borrow().clone(),
            result.err().map(|e| (e.message, e.is_fatal)),
        )
    };
    let vm = {
        let mut host = RecordHost::new();
        let result = bop_vm::run(code, &mut host, &tight);
        (
            host.prints.borrow().clone(),
            result.err().map(|e| (e.message, e.is_fatal)),
        )
    };
    assert!(tw.0.is_empty(), "walker printed: {:?}", tw.0);
    assert!(vm.0.is_empty(), "vm printed: {:?}", vm.0);
    let (tw_msg, tw_fatal) = tw.1.expect("walker should error");
    let (vm_msg, vm_fatal) = vm.1.expect("vm should error");
    assert!(tw_fatal, "walker non-fatal: {}", tw_msg);
    assert!(vm_fatal, "vm non-fatal: {}", vm_msg);
    assert!(
        tw_msg.contains("too many steps"),
        "walker msg: {}",
        tw_msg
    );
    assert!(vm_msg.contains("too many steps"), "vm msg: {}", vm_msg);
}

// ─── Modules / import ─────────────────────────────────────────────

#[test]
fn import_basic_let_binding() {
    set_modules(&[("greet", r#"let hello = "hi""#)]);
    assert_eq!(
        say(r#"import greet
print(hello)"#),
        "hi"
    );
}

#[test]
fn import_named_fn_callable() {
    set_modules(&[("math", "fn square(n) { return n * n }")]);
    assert_eq!(
        say(r#"import math
print(square(7))"#),
        "49"
    );
}

#[test]
fn import_named_fn_as_value() {
    // Proves the module's named fn survives import as a
    // first-class `Value::Fn`. Matters because the VM needs to
    // carry VM-compiled chunks in `Value::Fn`, and an imported
    // fn is loaded via a sub-VM.
    set_modules(&[("ops", "fn double(n) { return n * 2 }")]);
    assert_eq!(
        say(r#"import ops
let f = double
print(f(21))"#),
        "42"
    );
}

#[test]
fn import_dotted_path() {
    set_modules(&[("std.math", "let pi = 3")]);
    assert_eq!(
        say(r#"import std.math
print(pi)"#),
        "3"
    );
}

#[test]
fn import_missing_module_errors() {
    set_modules(&[]);
    let msg = run_err("import nope");
    assert!(msg.contains("Module `nope` not found"), "got: {}", msg);
}

#[test]
fn import_transitive_modules() {
    set_modules(&[
        ("a", "import b\nlet doubled_pi = pi + pi"),
        ("b", "let pi = 3"),
    ]);
    assert_eq!(
        say(r#"import a
print(doubled_pi)"#),
        "6"
    );
}

#[test]
fn import_circular_detected() {
    set_modules(&[
        ("a", "import b\nlet x = 1"),
        ("b", "import a\nlet y = 2"),
    ]);
    let msg = run_err("import a");
    assert!(msg.contains("Circular import"), "got: {}", msg);
}

#[test]
fn import_is_idempotent_at_injection_site() {
    set_modules(&[("m", "let x = 1")]);
    assert_eq!(
        say(r#"import m
import m
print(x)"#),
        "1"
    );
}

// ─── Closures / first-class functions ─────────────────────────────
//
// These run through both the tree-walker and the bytecode VM to
// prove the VM's `MakeLambda` / `CallValue` machinery matches the
// walker's snapshot-and-invoke model.

#[test]
fn closure_lambda_basic() {
    assert_eq!(
        say(r#"let double = fn(x) { return x * 2 }
print(double(5))"#),
        "10"
    );
}

#[test]
fn closure_captures_value() {
    assert_eq!(
        say(r#"let n = 5
let add_n = fn(x) { return x + n }
print(add_n(3))"#),
        "8"
    );
}

#[test]
fn closure_captures_are_snapshot() {
    assert_eq!(
        say(r#"let n = 5
let add_n = fn(x) { return x + n }
n = 100
print(add_n(3))"#),
        "8"
    );
}

#[test]
fn closure_factory_pattern() {
    assert_eq!(
        say(r#"fn make_adder(n) { return fn(x) { return x + n } }
let add5 = make_adder(5)
let add10 = make_adder(10)
print(add5(3))
print(add10(3))"#),
        "13"
    );
}

#[test]
fn named_fn_as_first_class_value() {
    assert_eq!(
        say(r#"fn double(x) { return x * 2 }
let f = double
print(f(7))"#),
        "14"
    );
}

#[test]
fn fn_in_array_indexed_call() {
    assert_eq!(
        say(r#"fn add(x, y) { return x + y }
fn mul(x, y) { return x * y }
let ops = [add, mul]
print(ops[0](2, 3))
print(ops[1](2, 3))"#),
        "6"
    );
}

#[test]
fn higher_order_apply() {
    assert_eq!(
        say(r#"fn apply(f, x) { return f(x) }
fn square(n) { return n * n }
print(apply(square, 4))
print(apply(fn(n) { return n + 1 }, 4))"#),
        "5"
    );
}

#[test]
fn type_of_fn_is_fn() {
    assert_eq!(say("fn f() { }\nprint(type(f))"), "fn");
    assert_eq!(say("let g = fn() { }\nprint(type(g))"), "fn");
}

#[test]
fn calling_non_callable_errors() {
    assert!(run_err("let x = 5\nx(1)").contains("not a function"));
}

#[test]
fn iife_value_call() {
    assert_eq!(say("print((fn(x) { return x * 3 })(4))"), "12");
}

// ─── Arrays ───────────────────────────────────────────────────────

#[test]
fn array_literal_and_index() {
    assert_eq!(say("let a = [10, 20, 30]\nprint(a[1])"), "20");
}

#[test]
fn array_negative_index() {
    assert_eq!(say("let a = [10, 20, 30]\nprint(a[-1])"), "30");
}

#[test]
fn array_assign_index() {
    assert_eq!(say("let a = [1, 2, 3]\na[1] = 99\nprint(a[1])"), "99");
}

#[test]
fn array_push_pop() {
    assert_eq!(
        say(r#"let a = [1, 2]
a.push(3)
print(a.len())"#),
        "3"
    );
    assert_eq!(
        say(r#"let a = [1, 2, 3]
let last = a.pop()
print(last)"#),
        "3"
    );
}

#[test]
fn array_has() {
    assert_eq!(say("print([1, 2, 3].has(2))"), "true");
    assert_eq!(say("print([1, 2, 3].has(9))"), "false");
}

#[test]
fn array_index_of() {
    assert_eq!(say("print([10, 20, 30].index_of(20))"), "1");
    assert_eq!(say("print([10, 20, 30].index_of(99))"), "-1");
}

#[test]
fn array_slice() {
    assert_eq!(say("print([1, 2, 3, 4, 5].slice(1, 4))"), "[2, 3, 4]");
}

#[test]
fn array_join() {
    assert_eq!(say(r#"print([1, 2, 3].join("-"))"#), "1-2-3");
}

#[test]
fn array_sort() {
    assert_eq!(say("let a = [3, 1, 2]\na.sort()\nprint(a)"), "[1, 2, 3]");
}

#[test]
fn array_reverse() {
    assert_eq!(say("let a = [1, 2, 3]\na.reverse()\nprint(a)"), "[3, 2, 1]");
}

#[test]
fn array_insert_remove() {
    assert_eq!(
        say(r#"let a = [1, 3]
a.insert(1, 2)
print(a)"#),
        "[1, 2, 3]"
    );
    assert_eq!(
        say(r#"let a = [1, 2, 3]
let removed = a.remove(1)
print(removed)"#),
        "2"
    );
}

#[test]
fn array_concat() {
    assert_eq!(say("print([1, 2] + [3, 4])"), "[1, 2, 3, 4]");
}

#[test]
fn array_out_of_bounds() {
    assert!(run_err("let a = [1]\nprint(a[5])").contains("out of bounds"));
}

// ─── Strings (methods) ────────────────────────────────────────────

#[test]
fn string_len() {
    assert_eq!(say(r#"print("hello".len())"#), "5");
}

#[test]
fn string_contains() {
    assert_eq!(say(r#"print("abcdef".contains("cd"))"#), "true");
    assert_eq!(say(r#"print("abcdef".contains("zz"))"#), "false");
}

#[test]
fn string_starts_ends_with() {
    assert_eq!(say(r#"print("hello".starts_with("he"))"#), "true");
    assert_eq!(say(r#"print("hello".ends_with("lo"))"#), "true");
}

#[test]
fn string_split() {
    assert_eq!(say(r#"print("a,b,c".split(","))"#), r#"["a", "b", "c"]"#);
}

#[test]
fn string_replace() {
    assert_eq!(
        say(r#"print("hello world".replace("world", "bop"))"#),
        "hello bop"
    );
}

#[test]
fn string_upper_lower_trim() {
    assert_eq!(say(r#"print("Hello".upper())"#), "HELLO");
    assert_eq!(say(r#"print("Hello".lower())"#), "hello");
    assert_eq!(say(r#"print("  hi  ".trim())"#), "hi");
}

#[test]
fn string_slice() {
    assert_eq!(say(r#"print("hello".slice(1, 4))"#), "ell");
}

#[test]
fn string_index_of() {
    assert_eq!(say(r#"print("hello".index_of("ll"))"#), "2");
    assert_eq!(say(r#"print("hello".index_of("zz"))"#), "-1");
}

#[test]
fn string_index_char() {
    assert_eq!(say(r#"print("abc"[1])"#), "b");
}

// ─── Dicts ────────────────────────────────────────────────────────

#[test]
fn dict_literal_and_access() {
    assert_eq!(
        say(r#"let d = {"name": "bop", "hp": 100}
print(d["name"])"#),
        "bop"
    );
}

#[test]
fn dict_assign_key() {
    assert_eq!(
        say(r#"let d = {"a": 1}
d["b"] = 2
print(d["b"])"#),
        "2"
    );
}

#[test]
fn dict_methods() {
    assert_eq!(
        say(r#"let d = {"x": 1, "y": 2}
print(d.len())"#),
        "2"
    );
    assert_eq!(say(r#"print({"a": 1, "b": 2}.has("a"))"#), "true");
    assert_eq!(say(r#"print({"a": 1, "b": 2}.has("z"))"#), "false");
}

#[test]
fn dict_keys_values() {
    assert_eq!(say(r#"print({"a": 1, "b": 2}.keys())"#), r#"["a", "b"]"#);
    assert_eq!(say(r#"print({"a": 1, "b": 2}.values())"#), "[1, 2]");
}

// ─── Built-in functions ───────────────────────────────────────────

#[test]
fn builtin_range_1arg() {
    assert_eq!(say("print(range(5))"), "[0, 1, 2, 3, 4]");
}

#[test]
fn builtin_range_2args() {
    assert_eq!(say("print(range(2, 5))"), "[2, 3, 4]");
}

#[test]
fn builtin_range_3args() {
    assert_eq!(say("print(range(0, 10, 3))"), "[0, 3, 6, 9]");
}

#[test]
fn builtin_range_reverse() {
    assert_eq!(say("print(range(5, 0))"), "[5, 4, 3, 2, 1]");
}

#[test]
fn builtin_str() {
    assert_eq!(say(r#"print(str(42))"#), "42");
    assert_eq!(say(r#"print(str(true))"#), "true");
}

#[test]
fn builtin_int() {
    assert_eq!(say("print(int(3.7))"), "3");
    assert_eq!(say("print(int(-2.9))"), "-2");
}

#[test]
fn builtin_type() {
    // Phase 6 split numeric types.
    assert_eq!(say("print(type(42))"), "int");
    assert_eq!(say("print(type(42.0))"), "number");
    assert_eq!(say(r#"print(type("hi"))"#), "string");
    assert_eq!(say("print(type(true))"), "bool");
    assert_eq!(say("print(type(none))"), "none");
    assert_eq!(say("print(type([]))"), "array");
}

#[test]
fn builtin_abs_min_max() {
    assert_eq!(say("print(abs(-5))"), "5");
    assert_eq!(say("print(min(3, 7))"), "3");
    assert_eq!(say("print(max(3, 7))"), "7");
}

#[test]
fn builtin_len() {
    assert_eq!(say(r#"print(len("hello"))"#), "5");
    assert_eq!(say("print(len([1, 2, 3]))"), "3");
}

#[test]
fn builtin_inspect() {
    assert_eq!(say(r#"print(inspect("hi"))"#), r#""hi""#);
    assert_eq!(say("print(inspect(42))"), "42");
}

#[test]
fn builtin_print_multi_args() {
    let out = run_both(r#"print("a", "b", "c")"#, &standard());
    assert!(out.is_ok());
    assert_eq!(out.prints.as_slice(), &["a b c"]);
}

#[test]
fn builtin_rand_deterministic() {
    // Both engines start `rand_state` at 0, so the runs inside the
    // same harness call also agree. This test pins that the same
    // input produces the same output on repeated top-level runs.
    let a = say("print(rand(100))");
    let b = say("print(rand(100))");
    assert_eq!(a, b);
}

// ─── Error cases ──────────────────────────────────────────────────

#[test]
fn error_division_by_zero() {
    assert!(run_err("print(1 / 0)").contains("Division by zero"));
}

#[test]
fn error_type_mismatch_subtract() {
    let msg = run_err(r#"print("a" - 1)"#);
    assert!(msg.contains("Can't use `-`"));
}

#[test]
fn error_unknown_function() {
    assert!(run_err("nope()").contains("not found"));
}

#[test]
fn error_infinite_loop_protection() {
    let msg = run_err("while true { }");
    assert!(msg.contains("too many steps"));
}

#[test]
fn error_break_outside_loop() {
    // Walker catches this at runtime (`line: 0`), VM compiler
    // catches it at compile time (real line). Message text matches
    // either way — that's the contract we care about.
    assert!(run_err("break").contains("outside of a loop"));
}

#[test]
fn error_continue_outside_loop() {
    assert!(run_err("continue").contains("outside of a loop"));
}

// ─── Edge cases ───────────────────────────────────────────────────

#[test]
fn empty_program() {
    let out = run_both("", &standard());
    assert!(out.is_ok());
    assert!(out.prints.is_empty());
}

#[test]
fn trailing_comma_in_array() {
    assert_eq!(say("print([1, 2, 3,])"), "[1, 2, 3]");
}

#[test]
fn trailing_comma_in_dict() {
    assert_eq!(say(r#"print({"a": 1,}.len())"#), "1");
}

#[test]
fn none_value() {
    assert_eq!(say("print(none)"), "none");
    assert_eq!(say("print(none == none)"), "true");
}

#[test]
fn equality_across_types() {
    assert_eq!(say("print(1 == true)"), "false");
    assert_eq!(say(r#"print(0 == "")"#), "false");
    assert_eq!(say("print(none == false)"), "false");
}

#[test]
fn dict_equality() {
    assert_eq!(say(r#"print({"a": 1, "b": 2} == {"b": 2, "a": 1})"#), "true");
    assert_eq!(say(r#"print({"a": 1} == {"a": 2})"#), "false");
    assert_eq!(say(r#"print({"a": 1} == {"b": 1})"#), "false");
    assert_eq!(say(r#"print({"a": 1} == {"a": 1, "b": 2})"#), "false");
    assert_eq!(say(r#"print({"a": {"x": 1}} == {"a": {"x": 1}})"#), "true");
}

#[test]
fn nested_array_access() {
    assert_eq!(say("let m = [[1, 2], [3, 4]]\nprint(m[1][0])"), "3");
}

#[test]
fn method_chain() {
    assert_eq!(say(r#"print("  HELLO  ".trim().lower())"#), "hello");
}

#[test]
fn comments_in_code() {
    // Phase 6: `#` is the line-comment leader; `//` is integer
    // division.
    assert_eq!(
        say(r#"# this is a comment
let x = 42 # inline comment
print(x)"#),
        "42"
    );
}

// ─── Scope / block isolation ──────────────────────────────────────

#[test]
fn if_block_scope() {
    assert!(
        run_err(
            r#"if true { let inner = 1 }
print(inner)"#
        )
        .contains("not found")
    );
}

#[test]
fn for_loop_var_scoped() {
    assert!(
        run_err(
            r#"for item in [1, 2] { let x = item }
print(item)"#
        )
        .contains("not found")
    );
}

// ─── Complex programs ─────────────────────────────────────────────

#[test]
fn fizzbuzz() {
    assert_eq!(
        say(r#"let result = []
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
print(result.join(", "))"#),
        "1, 2, Fizz, 4, Buzz, Fizz, 7, 8, Fizz, Buzz, 11, Fizz, 13, 14, FizzBuzz"
    );
}

#[test]
fn nested_function_calls() {
    assert_eq!(
        say(r#"fn square(n) { return n * n }
fn sum_squares(a, b) { return square(a) + square(b) }
print(sum_squares(3, 4))"#),
        "25"
    );
}

#[test]
fn array_manipulation_program() {
    assert_eq!(
        say(r#"let data = [5, 2, 8, 1, 9, 3]
data.sort()
let top3 = data.slice(3, 6)
print(top3.join(", "))"#),
        "5, 8, 9"
    );
}

// ─── Truthiness ───────────────────────────────────────────────────

#[test]
fn truthy_values() {
    assert_eq!(say("print(if 1 { \"yes\" } else { \"no\" })"), "yes");
    assert_eq!(say(r#"print(if "x" { "yes" } else { "no" })"#), "yes");
    assert_eq!(say("print(if [1] { \"yes\" } else { \"no\" })"), "yes");
}

#[test]
fn falsy_values() {
    assert_eq!(say("print(if 0 { \"yes\" } else { \"no\" })"), "no");
    assert_eq!(say("print(if false { \"yes\" } else { \"no\" })"), "no");
    assert_eq!(say("print(if none { \"yes\" } else { \"no\" })"), "no");
    assert_eq!(say(r#"print(if "" { "yes" } else { "no" })"#), "no");
}

// ─── Number display ──────────────────────────────────────────────

#[test]
fn display_whole_number_as_int() {
    assert_eq!(say("print(5.0)"), "5");
}

#[test]
fn display_float_with_decimals() {
    assert_eq!(say("print(3.14)"), "3.14");
}

// ─── Safety / resource-limit tests ───────────────────────────────

#[test]
fn safety_infinite_loop_halts() {
    let msg = run_err_with_limits("while true { }", tight());
    assert!(msg.contains("too many steps"), "got: {}", msg);
}

#[test]
fn safety_memory_bomb_string_doubling() {
    let msg = run_err_with_limits(
        r#"let s = "aaaaaaaaaa"
repeat 100 { s = s + s }"#,
        tight(),
    );
    assert!(msg.contains("Memory limit"), "got: {}", msg);
}

#[test]
fn safety_memory_bomb_array_growth() {
    let (tw, vm) = run_err_loose(
        r#"let arr = []
repeat 500 {
    arr.push("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
}"#,
        tight(),
    );
    assert_both_resource_limit(&tw, &vm);
}

#[test]
fn safety_deep_recursion_halts() {
    let msg = run_err_with_limits("fn f() { f() }\nf()", tight());
    assert!(
        msg.contains("nested function calls") || msg.contains("recursion"),
        "got: {}",
        msg
    );
}

#[test]
fn safety_string_repeat_bomb() {
    let msg = run_err_with_limits(r#"let s = "x" * 999999"#, tight());
    assert!(msg.contains("Memory limit"), "got: {}", msg);
}

#[test]
fn safety_string_concat_bomb() {
    let msg = run_err_with_limits(
        r#"let s = "x" * 1000
repeat 100 { s = s + s }"#,
        tight(),
    );
    assert!(msg.contains("Memory limit"), "got: {}", msg);
}

#[test]
fn safety_array_concat_bomb() {
    let (tw, vm) = run_err_loose(
        r#"let a = range(100)
repeat 50 { a = a + a }"#,
        tight(),
    );
    assert_both_resource_limit(&tw, &vm);
}

#[test]
fn safety_for_in_large_string() {
    let (tw, vm) = run_err_loose(
        r#"let s = "x" * 10000
for c in s { }"#,
        tight(),
    );
    assert_both_resource_limit(&tw, &vm);
}

#[test]
fn safety_demo_limits_step_bound() {
    let msg = run_err_with_limits(
        "let i = 0\nwhile true { i = i + 1 }",
        BopLimits::demo(),
    );
    assert!(msg.contains("too many steps"), "got: {}", msg);
}

#[test]
fn safety_demo_limits_memory_bound() {
    let msg = run_err_with_limits(
        r#"let s = "x" * 1100000
print(s)"#,
        BopLimits::demo(),
    );
    assert!(msg.contains("Memory limit"), "got: {}", msg);
}

#[test]
fn safety_nested_loop_step_bound() {
    let msg = run_err_with_limits(
        "repeat 100 { repeat 100 { let x = 1 } }",
        tight(),
    );
    assert!(msg.contains("too many steps"), "got: {}", msg);
}

#[test]
fn safety_range_hard_cap() {
    let (tw, vm) = run_err_loose(
        r#"let a = range(100000)
let x = 1"#,
        tight(),
    );
    assert_both_resource_limit(&tw, &vm);
}

// ─── BopHost extension ────────────────────────────────────────────
//
// Custom hosts can't share state across the walker / VM calls, so
// each test builds both independently and compares afterwards.

struct CustomHost {
    prints: Vec<String>,
}

impl BopHost for CustomHost {
    fn call(&mut self, name: &str, args: &[Value], line: u32) -> Option<Result<Value, BopError>> {
        match name {
            "greet" => {
                if args.len() != 1 {
                    return Some(Err(BopError {
                        line: Some(line),
                        column: None,
                        message: "greet() needs 1 argument".into(),
                        friendly_hint: None,
                        is_fatal: false,
                    }));
                }
                Some(Ok(Value::new_str(format!("Hello, {}!", args[0]))))
            }
            _ => None,
        }
    }

    fn on_print(&mut self, message: &str) {
        self.prints.push(message.to_string());
    }

    fn function_hint(&self) -> &str {
        "Available: greet(name)"
    }
}

#[test]
fn host_custom_builtin() {
    let code = r#"print(greet("world"))"#;
    let mut tw_host = CustomHost { prints: vec![] };
    bop::run(code, &mut tw_host, &standard()).unwrap();
    let mut vm_host = CustomHost { prints: vec![] };
    bop_vm::run(code, &mut vm_host, &standard()).unwrap();
    assert_eq!(tw_host.prints, vm_host.prints);
    assert_eq!(tw_host.prints, vec!["Hello, world!"]);
}

#[test]
fn host_function_hint() {
    let code = "unknown()";
    let mut tw_host = CustomHost { prints: vec![] };
    let tw_err = bop::run(code, &mut tw_host, &standard()).unwrap_err();
    let mut vm_host = CustomHost { prints: vec![] };
    let vm_err = bop_vm::run(code, &mut vm_host, &standard()).unwrap_err();
    assert_eq!(tw_err.message, vm_err.message);
    assert!(tw_err.message.contains("not found"));
}

// ─── Fuzzer ───────────────────────────────────────────────────────
//
// Random programs from a constrained grammar. Every generated
// program is run through `run_both`, which panics on any divergence.
// Seeds are deterministic so a failure is trivially reproducible:
// the panic prints the generated source.

/// Deterministic xorshift64* RNG. Kept inline — the harness doesn't
/// want a dep on `rand` just for a smoke-fuzz.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn pick(&mut self, n: u32) -> u32 {
        debug_assert!(n > 0);
        (self.next_u64() % u64::from(n)) as u32
    }
}

/// Program generator. Builds statement-level code with a bounded
/// expression tree. Grammar intentionally avoids `while`, `fn`, and
/// methods — each widens the divergence surface beyond what this
/// smoke fuzz is trying to stress-test (and `while` can't be bounded
/// for guaranteed termination without also generating a decrementing
/// counter, which the walker-vs-VM comparison doesn't care about).
struct Generator {
    rng: Rng,
    vars: Vec<String>,
}

impl Generator {
    fn new(seed: u64) -> Self {
        Self {
            rng: Rng::new(seed),
            vars: Vec::new(),
        }
    }

    fn gen_program(&mut self, stmt_count: usize) -> String {
        let mut out = String::new();
        for _ in 0..stmt_count {
            self.gen_stmt(&mut out, 0);
        }
        out
    }

    fn gen_stmt(&mut self, out: &mut String, nest: u32) {
        // Bias toward `let` and `print` so generated programs
        // actually do something observable to compare.
        let pick = self.rng.pick(8);
        if pick < 2 || (pick == 2 && self.vars.is_empty()) {
            let name = format!("v{}", self.vars.len());
            let expr = self.gen_expr(3);
            out.push_str(&format!("let {} = {}\n", name, expr));
            self.vars.push(name);
        } else if pick == 2 {
            let idx = self.rng.pick(self.vars.len() as u32) as usize;
            let name = self.vars[idx].clone();
            let expr = self.gen_expr(3);
            out.push_str(&format!("{} = {}\n", name, expr));
        } else if pick == 3 || pick == 4 {
            let expr = self.gen_expr(3);
            out.push_str(&format!("print({})\n", expr));
        } else if pick == 5 && nest < 2 {
            let cond = self.gen_expr(2);
            out.push_str(&format!("if {} {{\n", cond));
            self.gen_stmt(out, nest + 1);
            out.push_str("} else {\n");
            self.gen_stmt(out, nest + 1);
            out.push_str("}\n");
        } else if pick == 6 && nest < 2 {
            // `repeat n { ... }` with n clamped to 0..=8 so nested
            // loops can't blow the step budget.
            let n = self.rng.pick(9);
            out.push_str(&format!("repeat {} {{\n", n));
            self.gen_stmt(out, nest + 1);
            out.push_str("}\n");
        } else {
            // fallback: bind a fresh temp so subsequent stmts have
            // more idents to pick from.
            let expr = self.gen_expr(2);
            let name = format!("t{}", self.vars.len());
            out.push_str(&format!("let {} = {}\n", name, expr));
            self.vars.push(name);
        }
    }

    fn gen_expr(&mut self, depth: u32) -> String {
        if depth == 0 || self.rng.pick(3) == 0 {
            return self.gen_leaf();
        }
        match self.rng.pick(4) {
            0 => {
                let l = self.gen_expr(depth - 1);
                let r = self.gen_expr(depth - 1);
                let op = match self.rng.pick(10) {
                    0 => "+",
                    1 => "-",
                    2 => "*",
                    3 => "%",
                    4 => "==",
                    5 => "!=",
                    6 => "<",
                    7 => ">",
                    8 => "&&",
                    _ => "||",
                };
                format!("({} {} {})", l, op, r)
            }
            1 => {
                let e = self.gen_expr(depth - 1);
                if self.rng.pick(2) == 0 {
                    format!("(-{})", e)
                } else {
                    format!("(!{})", e)
                }
            }
            2 => {
                let n = self.rng.pick(4);
                let mut parts = Vec::new();
                for _ in 0..n {
                    parts.push(self.gen_expr(depth - 1));
                }
                format!("[{}]", parts.join(", "))
            }
            _ => {
                let c = self.gen_expr(depth - 1);
                let t = self.gen_expr(depth - 1);
                let e = self.gen_expr(depth - 1);
                format!("(if {} {{ {} }} else {{ {} }})", c, t, e)
            }
        }
    }

    fn gen_leaf(&mut self) -> String {
        match self.rng.pick(7) {
            0 => format!("{}", self.rng.pick(100) as i32 - 50),
            1 => "true".into(),
            2 => "false".into(),
            3 => "none".into(),
            4 if !self.vars.is_empty() => {
                let idx = self.rng.pick(self.vars.len() as u32) as usize;
                self.vars[idx].clone()
            }
            _ => format!("\"s{}\"", self.rng.pick(10)),
        }
    }
}

#[test]
fn fuzz_smoke_diff() {
    // Fixed seed list. If this flakes, it's a divergence bug, not
    // a fuzz bug — repro by using the printed seed and stmt count.
    for seed in 0..100u64 {
        let mut g = Generator::new(seed.wrapping_add(1));
        let stmt_count = 6 + (seed as usize % 8);
        let program = g.gen_program(stmt_count);
        // `run_both` asserts walker == vm internally. On a diff it
        // panics with the full program printed.
        let _ = run_both(&program, &standard());
    }
}

#[test]
#[ignore] // opt-in: `cargo test --test differential -- --ignored fuzz_extended_diff`
fn fuzz_extended_diff() {
    let seed_base: u64 = std::env::var("BOP_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0xB0B_C0DE_u64);
    for i in 0..10_000u64 {
        let mut g = Generator::new(seed_base.wrapping_add(i).wrapping_add(1));
        let stmt_count = 8 + ((i as usize) % 16);
        let program = g.gen_program(stmt_count);
        let _ = run_both(&program, &standard());
    }
}
