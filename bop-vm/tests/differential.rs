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
    assert_eq!(say("print(type(42))"), "number");
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
    assert_eq!(
        say(r#"// this is a comment
let x = 42 // inline comment
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
