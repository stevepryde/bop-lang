#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc;

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

pub mod error;
pub mod value;
pub mod lexer;
pub mod parser;
pub mod memory;
pub mod ops;
pub mod precheck;
pub mod builtins;
pub mod methods;

mod evaluator;

pub use error::BopError;
pub use parser::{Stmt, count_instructions};
pub use value::Value;

/// The core pattern matcher. Re-exported so engines beyond the
/// tree-walker (the bytecode VM, the AOT runtime) can apply the
/// same structural rules without re-implementing them.
pub use evaluator::pattern_matches;

// ─── BopLimits ─────────────────────────────────────────────────────────────

/// Resource limits enforced during execution.
#[derive(Debug, Clone)]
pub struct BopLimits {
    /// Max interpreter ticks (loop iterations, statements, etc.)
    pub max_steps: u64,
    /// Max total tracked memory (bytes) for strings + arrays
    pub max_memory: usize,
}

impl BopLimits {
    pub fn standard() -> Self {
        Self {
            max_steps: 10_000,
            max_memory: 10 * 1024 * 1024, // 10 MB
        }
    }

    pub fn demo() -> Self {
        Self {
            max_steps: 1_000,
            max_memory: 1024 * 1024, // 1 MB
        }
    }
}

impl Default for BopLimits {
    fn default() -> Self {
        Self::standard()
    }
}

// ─── BopHost trait ─────────────────────────────────────────────────────────

/// Extension point for embedders to add custom built-in functions.
pub trait BopHost {
    /// Called for unknown function names. Return `None` = not handled.
    fn call(&mut self, name: &str, args: &[Value], line: u32) -> Option<Result<Value, BopError>>;

    /// Called by `print()`.
    fn on_print(&mut self, message: &str) {
        let _ = message;
    }

    /// Hint text for "function not found" errors.
    fn function_hint(&self) -> &str {
        ""
    }

    /// Called each tick. Return `Err` to halt execution.
    fn on_tick(&mut self) -> Result<(), BopError> {
        Ok(())
    }

    /// Resolve an `import` target to Bop source.
    ///
    /// The core language doesn't know where modules live — a
    /// filesystem embedder reads `.bop` files, a browser embedder
    /// might fetch a URL, an embedded host might look up bundled
    /// string assets. Returning:
    ///
    /// - `None` — "I don't handle this module path": the runtime
    ///   raises a *module not found* error.
    /// - `Some(Ok(source))` — the module's source text, to be
    ///   parsed and executed by the engine.
    /// - `Some(Err(e))` — the resolver itself failed (I/O error,
    ///   bad path, …); the engine propagates the error as-is.
    ///
    /// The default impl returns `None`, so by default a program
    /// that imports anything halts with *module not found*.
    fn resolve_module(&mut self, name: &str) -> Option<Result<String, BopError>> {
        let _ = name;
        None
    }
}

// ─── Public API ────────────────────────────────────────────────────────────

/// Run a Bop program with the given host and limits.
pub fn run<H: BopHost>(source: &str, host: &mut H, limits: &BopLimits) -> Result<(), BopError> {
    let tokens = lexer::lex(source)?;
    let stmts = parser::parse(tokens)?;
    let eval = evaluator::Evaluator::new(host, limits.clone());
    eval.run(&stmts)
}

/// Parse Bop source into an AST (useful for instruction counting).
pub fn parse(source: &str) -> Result<Vec<Stmt>, BopError> {
    let tokens = lexer::lex(source)?;
    parser::parse(tokens)
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    // ─── Test host ─────────────────────────────────────────────────

    struct TestHost {
        prints: RefCell<Vec<String>>,
    }

    impl TestHost {
        fn new() -> Self {
            Self {
                prints: RefCell::new(Vec::new()),
            }
        }

        fn last_print(&self) -> String {
            self.prints.borrow().last().cloned().expect("no print output")
        }
    }

    impl BopHost for TestHost {
        fn call(&mut self, _name: &str, _args: &[Value], _line: u32) -> Option<Result<Value, BopError>> {
            None
        }

        fn on_print(&mut self, message: &str) {
            self.prints.borrow_mut().push(message.to_string());
        }
    }

    // ─── Test helpers ──────────────────────────────────────────────

    fn test_limits() -> BopLimits {
        BopLimits::standard()
    }

    /// Run code, return last print output
    fn say(code: &str) -> String {
        // Change say() -> print() in test code
        let mut host = TestHost::new();
        run(code, &mut host, &test_limits()).unwrap();
        host.last_print()
    }

    /// Run code, expect runtime error, return message
    fn run_err(code: &str) -> String {
        let mut host = TestHost::new();
        run(code, &mut host, &test_limits()).unwrap_err().message
    }

    /// Expect a lex or parse error, return message
    fn parse_err(code: &str) -> String {
        parse(code).unwrap_err().message
    }

    /// Run code with custom limits, expect a runtime error, return message
    fn run_err_with_limits(code: &str, limits: BopLimits) -> String {
        let mut host = TestHost::new();
        run(code, &mut host, &limits).unwrap_err().message
    }

    /// Tight limits for safety tests
    fn tight_limits() -> BopLimits {
        BopLimits {
            max_steps: 500,
            max_memory: 64 * 1024,
        }
    }

    // ─── Arithmetic ────────────────────────────────────────────────

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

    // ─── Strings ───────────────────────────────────────────────────

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

    // ─── Comparisons & Logic ───────────────────────────────────────

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

    // ─── Variables ─────────────────────────────────────────────────

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

    // ─── If / Else ─────────────────────────────────────────────────

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

    // ─── While ─────────────────────────────────────────────────────

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

    // ─── For ───────────────────────────────────────────────────────

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

    // ─── Repeat ────────────────────────────────────────────────────

    #[test]
    fn repeat_loop() {
        assert_eq!(say("let n = 0\nrepeat 4 { n += 1 }\nprint(n)"), "4");
    }

    #[test]
    fn repeat_zero() {
        assert_eq!(say("let n = 99\nrepeat 0 { n = 0 }\nprint(n)"), "99");
    }

    // ─── Functions ─────────────────────────────────────────────────

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

    // ─── Closures / first-class functions ──────────────────────────

    #[test]
    fn lambda_basic() {
        assert_eq!(
            say(r#"let double = fn(x) { return x * 2 }
print(double(5))"#),
            "10"
        );
    }

    #[test]
    fn lambda_captures_value() {
        assert_eq!(
            say(r#"let n = 5
let add_n = fn(x) { return x + n }
print(add_n(3))"#),
            "8"
        );
    }

    #[test]
    fn lambda_captures_are_snapshot() {
        // Mutating `n` after the lambda is built should not
        // affect the captured value — the snapshot semantics are
        // deliberate.
        assert_eq!(
            say(r#"let n = 5
let add_n = fn(x) { return x + n }
n = 100
print(add_n(3))"#),
            "8"
        );
    }

    #[test]
    fn lambda_returned_from_fn() {
        // Classic closure pattern: factory function returns a
        // specialised closure. The captured `n` outlives the
        // enclosing frame because it was cloned into the closure.
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
    fn named_fn_is_first_class_value() {
        assert_eq!(
            say(r#"fn double(x) { return x * 2 }
let f = double
print(f(7))"#),
            "14"
        );
    }

    #[test]
    fn fn_stored_in_array_and_called_via_index() {
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
        // `apply` takes any callable, proving we call through a
        // parameter, not a statically known name.
        assert_eq!(
            say(r#"fn apply(f, x) { return f(x) }
fn square(n) { return n * n }
print(apply(square, 4))
print(apply(fn(n) { return n + 1 }, 4))"#),
            "5"
        );
    }

    #[test]
    fn lambda_self_reference_via_named_fn() {
        // Named-fn decls see themselves through `self.functions`,
        // so recursion works. Let-bound lambdas are documented as
        // not supporting self-reference — this test pins the
        // working case.
        assert_eq!(
            say(r#"fn countdown(n) {
    if n <= 0 { return "done" }
    return countdown(n - 1)
}
print(countdown(3))"#),
            "done"
        );
    }

    #[test]
    fn type_of_fn_is_fn() {
        assert_eq!(say("fn f() { }\nprint(type(f))"), "fn");
        assert_eq!(say("let g = fn() { }\nprint(type(g))"), "fn");
    }

    #[test]
    fn calling_non_callable_value_errors() {
        assert!(run_err("let x = 5\nx(1)").contains("not a function"));
    }

    #[test]
    fn lambda_captures_nested_scope() {
        // Closures snapshot the full lexical scope stack, so
        // bindings from outer blocks are visible inside nested
        // lambdas.
        assert_eq!(
            say(r#"let a = 1
if true {
    let b = 2
    let f = fn() { return a + b }
    print(f())
}"#),
            "3"
        );
    }

    #[test]
    fn iife() {
        // Immediately-invoked lambda: `(fn() { ... })()`. Falls
        // out of the parser for free once lambdas are expressions.
        assert_eq!(say("print((fn(x) { return x * 3 })(4))"), "12");
    }

    // ─── Arrays ────────────────────────────────────────────────────

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

    // ─── Strings (methods) ─────────────────────────────────────────

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

    // ─── Dicts ─────────────────────────────────────────────────────

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

    // ─── Built-in functions ────────────────────────────────────────

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
        // Phase 6 split numeric types: `42` is an int, `42.0`
        // is a number.
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
        let mut host = TestHost::new();
        run(r#"print("a", "b", "c")"#, &mut host, &test_limits()).unwrap();
        assert_eq!(host.prints.borrow().as_slice(), &["a b c"]);
    }

    #[test]
    fn builtin_rand_deterministic() {
        let a = say("print(rand(100))");
        let b = say("print(rand(100))");
        assert_eq!(a, b);
    }

    // ─── Error cases ───────────────────────────────────────────────

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
        assert!(run_err("break").contains("outside of a loop"));
    }

    #[test]
    fn error_continue_outside_loop() {
        assert!(run_err("continue").contains("outside of a loop"));
    }

    // ─── Parse errors ──────────────────────────────────────────────

    #[test]
    fn parse_error_missing_rparen() {
        assert!(parse_err("print(1").contains("Expected `)`"));
    }

    #[test]
    fn parse_error_missing_rbrace() {
        assert!(parse_err("if true {").contains("Expected `}`"));
    }

    // ─── Edge cases ────────────────────────────────────────────────

    #[test]
    fn empty_program() {
        let mut host = TestHost::new();
        run("", &mut host, &test_limits()).unwrap();
        assert!(host.prints.borrow().is_empty());
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

    // ─── Error diagnostics (phase 8 polish) ──────────────────────

    #[test]
    fn parse_error_carries_column_info() {
        // "let 42" is a parse error — the int `42` shows up
        // where a name was expected. Column should point at the
        // start of the `42` token (column 5).
        let err = parse_err_full("let 42");
        assert_eq!(err.line, Some(1), "err: {:?}", err);
        assert_eq!(err.column, Some(5), "err: {:?}", err);
        assert!(err.message.contains("Expected a name"));
    }

    #[test]
    fn parse_error_renders_with_snippet_and_carat() {
        let src = "let 42";
        let err = parse_err_full(src);
        let rendered = err.render(src);
        assert!(rendered.contains("--> line 1:5"), "rendered:\n{}", rendered);
        assert!(rendered.contains("let 42"));
        // Four spaces for `let ` before the carat at col 5.
        assert!(rendered.contains("    ^"), "rendered:\n{}", rendered);
    }

    #[test]
    fn parse_error_on_line_2_points_at_line_2() {
        let src = "let x = 1\nlet = 2";
        let err = parse_err_full(src);
        assert_eq!(err.line, Some(2), "err: {:?}", err);
        let rendered = err.render(src);
        assert!(
            rendered.contains("let = 2"),
            "rendered:\n{}",
            rendered
        );
    }

    #[test]
    fn runtime_error_renders_without_column() {
        // Runtime errors don't currently carry column; the
        // renderer should still produce a readable snippet.
        let src = "let x = 1 / 0";
        let err = run_err_full(src);
        assert_eq!(err.line, Some(1));
        assert!(err.column.is_none());
        let rendered = err.render(src);
        assert!(rendered.contains("--> line 1"));
        assert!(rendered.contains("let x = 1 / 0"));
        // No carat line (column unknown).
        assert!(!rendered.contains("^"), "rendered:\n{}", rendered);
    }

    /// Like `parse_err` but returns the full `BopError` so
    /// tests can inspect line / column fields directly.
    fn parse_err_full(code: &str) -> BopError {
        parse(code).unwrap_err()
    }

    /// Like `run_err` but returns the full `BopError`.
    fn run_err_full(code: &str) -> BopError {
        let mut host = TestHost::new();
        run(code, &mut host, &test_limits()).unwrap_err()
    }

    #[test]
    fn comments_in_code() {
        // Phase 6 swapped line comments from `//` (needed for
        // integer division) to `#` (Python-style). Single-line
        // only; no block-comment form.
        assert_eq!(
            say(r#"# this is a comment
let x = 42 # inline comment
print(x)"#),
            "42"
        );
    }

    // ─── Instruction counting ─────────────────────────────────────

    fn count(code: &str) -> u32 {
        let stmts = parse(code).unwrap();
        count_instructions(&stmts)
    }

    #[test]
    fn count_simple_calls() {
        assert_eq!(count("print(1)"), 1);
        assert_eq!(count("print(1); print(2); print(3)"), 3);
    }

    #[test]
    fn count_repeat() {
        assert_eq!(count("repeat 7 { print(1) }"), 2);
    }

    #[test]
    fn count_if() {
        assert_eq!(count("if true { print(1) }"), 2);
        assert_eq!(count("if true { print(1) } else { print(2) }"), 3);
    }

    #[test]
    fn count_while() {
        assert_eq!(count("while true { print(1) }"), 2);
    }

    #[test]
    fn count_fn_skips_body() {
        assert_eq!(count("fn go() { print(1); print(2); print(3) }\ngo()"), 2);
    }

    #[test]
    fn count_format_independent() {
        let one_line = count("repeat 7 { print(1) }");
        let multi_line = count("repeat 7 {\n    print(1)\n}");
        assert_eq!(one_line, multi_line);
        assert_eq!(one_line, 2);
    }

    #[test]
    fn count_nested() {
        assert_eq!(count("repeat 7 { if true { print(1) } }"), 3);
    }

    #[test]
    fn count_empty_program() {
        assert_eq!(count(""), 0);
    }

    // ─── Scope / block isolation ───────────────────────────────────

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

    // ─── Complex programs ──────────────────────────────────────────

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

    // ─── Truthiness ────────────────────────────────────────────────

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

    // ─── Number display ────────────────────────────────────────────

    #[test]
    fn display_whole_number_as_int() {
        assert_eq!(say("print(5.0)"), "5");
    }

    #[test]
    fn display_float_with_decimals() {
        assert_eq!(say("print(3.14)"), "3.14");
    }

    // ─── Safety / resource-limit tests ──────────────────────────────

    #[test]
    fn safety_infinite_loop_halts() {
        let msg = run_err_with_limits("while true { }", tight_limits());
        assert!(msg.contains("too many steps"), "got: {}", msg);
    }

    #[test]
    fn safety_memory_bomb_string_doubling() {
        let msg = run_err_with_limits(
            r#"let s = "aaaaaaaaaa"
repeat 100 { s = s + s }"#,
            tight_limits(),
        );
        assert!(msg.contains("Memory limit"), "got: {}", msg);
    }

    #[test]
    fn safety_memory_bomb_array_growth() {
        let msg = run_err_with_limits(
            r#"let arr = []
repeat 500 {
    arr.push("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
}"#,
            tight_limits(),
        );
        assert!(
            msg.contains("Memory limit") || msg.contains("too many steps"),
            "got: {}", msg
        );
    }

    #[test]
    fn safety_deep_recursion_halts() {
        let msg = run_err_with_limits("fn f() { f() }\nf()", tight_limits());
        assert!(
            msg.contains("nested function calls") || msg.contains("recursion"),
            "got: {}", msg
        );
    }

    #[test]
    fn safety_deep_parse_nesting() {
        let code = "(".repeat(200) + "1" + &")".repeat(200);
        let msg = parse(&code).unwrap_err().message;
        assert!(msg.contains("nested too deeply"), "got: {}", msg);
    }

    #[test]
    fn safety_string_repeat_bomb() {
        let msg = run_err_with_limits(r#"let s = "x" * 999999"#, tight_limits());
        assert!(msg.contains("Memory limit"), "got: {}", msg);
    }

    #[test]
    fn safety_string_concat_bomb() {
        let msg = run_err_with_limits(
            r#"let s = "x" * 1000
repeat 100 { s = s + s }"#,
            tight_limits(),
        );
        assert!(msg.contains("Memory limit"), "got: {}", msg);
    }

    #[test]
    fn safety_array_concat_bomb() {
        let msg = run_err_with_limits(
            r#"let a = range(100)
repeat 50 { a = a + a }"#,
            tight_limits(),
        );
        assert!(
            msg.contains("Memory limit") || msg.contains("too many steps"),
            "got: {}", msg
        );
    }

    #[test]
    fn safety_for_in_large_string() {
        let msg = run_err_with_limits(
            r#"let s = "x" * 10000
for c in s { }"#,
            tight_limits(),
        );
        assert!(
            msg.contains("too many steps") || msg.contains("Memory limit"),
            "got: {}", msg
        );
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
        let msg = run_err_with_limits("repeat 100 { repeat 100 { let x = 1 } }", tight_limits());
        assert!(msg.contains("too many steps"), "got: {}", msg);
    }

    #[test]
    fn safety_string_split_bomb() {
        let msg = run_err_with_limits(
            r#"let s = "abababababab" * 2000
let parts = s.split("a")
let x = 1"#,
            tight_limits(),
        );
        assert!(
            msg.contains("Memory limit") || msg.contains("too many steps"),
            "got: {}", msg
        );
    }

    #[test]
    fn safety_join_bomb() {
        let msg = run_err_with_limits(
            r#"let a = []
repeat 400 { a.push("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa") }
let s = a.join("")
let x = 1"#,
            tight_limits(),
        );
        assert!(
            msg.contains("Memory limit") || msg.contains("too many steps"),
            "got: {}", msg
        );
    }

    #[test]
    fn safety_range_hard_cap() {
        let msg = run_err_with_limits(
            r#"let a = range(100000)
let x = 1"#,
            tight_limits(),
        );
        assert!(
            msg.contains("Memory limit") || msg.contains("too many steps"),
            "got: {}", msg
        );
    }

    #[test]
    fn safety_array_method_doubling() {
        let msg = run_err_with_limits(
            r#"let a = []
repeat 400 { a.push("aaaaaaaaaaaaaaaaaaaaaa") }
a.reverse()
let x = 1"#,
            tight_limits(),
        );
        assert!(
            msg.contains("Memory limit") || msg.contains("too many steps"),
            "got: {}", msg
        );
    }

    #[test]
    fn safety_preflight_catches() {
        let limits = BopLimits {
            max_steps: 500,
            max_memory: 32 * 1024,
        };
        let msg = run_err_with_limits(r#"let s = "x" * 40000"#, limits);
        assert!(msg.contains("Memory limit"), "got: {}", msg);
    }

    #[test]
    fn safety_bounded_overshoot() {
        let limits = BopLimits {
            max_steps: 500,
            max_memory: 64 * 1024,
        };
        let mut host = TestHost::new();
        let result = run(
            r#"let s = "abababab" * 1000
let parts = s.split("a")"#,
            &mut host,
            &limits,
        );
        assert!(result.is_ok(), "Expected success (bounded overshoot), got error");
    }

    #[test]
    fn safety_dict_growth_tracked() {
        let msg = run_err_with_limits(
            r#"let d = {}
repeat 400 {
    d[str(d.len())] = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
}
let x = 1"#,
            tight_limits(),
        );
        assert!(
            msg.contains("Memory limit") || msg.contains("too many steps"),
            "got: {}", msg
        );
    }

    // ─── BopHost extension ─────────────────────────────────────────

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
        let mut host = CustomHost { prints: vec![] };
        run(r#"print(greet("world"))"#, &mut host, &BopLimits::standard()).unwrap();
        assert_eq!(host.prints, vec!["Hello, world!"]);
    }

    #[test]
    fn host_function_hint() {
        let mut host = CustomHost { prints: vec![] };
        let err = run("unknown()", &mut host, &BopLimits::standard()).unwrap_err();
        assert!(err.message.contains("not found"));
    }

    // ─── Pattern matching ─────────────────────────────────────────

    #[test]
    fn match_literal_arms() {
        assert_eq!(
            say(r#"let x = 2
let out = match x {
    1 => "one",
    2 => "two",
    3 => "three",
    _ => "other",
}
print(out)"#),
            "two"
        );
    }

    #[test]
    fn match_falls_through_to_wildcard() {
        assert_eq!(
            say(r#"let x = 42
print(match x {
    1 => "one",
    _ => "other",
})"#),
            "other"
        );
    }

    #[test]
    fn match_no_arm_errors() {
        let err = run_err(r#"let x = 5
match x { 1 => "a", 2 => "b" }"#);
        assert!(err.contains("No match arm matched"), "got: {}", err);
    }

    #[test]
    fn match_binding_captures_scrutinee() {
        assert_eq!(
            say(r#"print(match 42 {
    x => x + 1,
})"#),
            "43"
        );
    }

    #[test]
    fn match_guard_accepts() {
        assert_eq!(
            say(r#"print(match 7 {
    n if n > 10 => "big",
    n if n > 0 => "small",
    _ => "zero or less",
})"#),
            "small"
        );
    }

    #[test]
    fn match_guard_rejects_continues() {
        assert_eq!(
            say(r#"print(match 5 {
    n if n < 0 => "neg",
    n if n > 100 => "huge",
    _ => "mid",
})"#),
            "mid"
        );
    }

    #[test]
    fn match_or_pattern() {
        assert_eq!(
            say(r#"let x = 3
print(match x {
    1 | 2 | 3 => "small",
    _ => "other",
})"#),
            "small"
        );
    }

    #[test]
    fn match_enum_unit_variant() {
        assert_eq!(
            say(r#"enum E { A, B, C }
print(match E::B {
    E::A => "a",
    E::B => "b",
    E::C => "c",
})"#),
            "b"
        );
    }

    #[test]
    fn match_enum_tuple_binds() {
        assert_eq!(
            say(r#"enum Shape { Circle(r), Square(s), Empty }
let s = Shape::Circle(5)
print(match s {
    Shape::Circle(r) => r * 2,
    Shape::Square(s) => s * s,
    Shape::Empty => 0,
})"#),
            "10"
        );
    }

    #[test]
    fn match_enum_struct_variant_binds() {
        assert_eq!(
            say(r#"enum Shape { Rect { w, h }, Empty }
let r = Shape::Rect { w: 4, h: 3 }
print(match r {
    Shape::Rect { w, h } => w * h,
    Shape::Empty => 0,
})"#),
            "12"
        );
    }

    #[test]
    fn match_struct_destructure() {
        assert_eq!(
            say(r#"struct Point { x, y }
let p = Point { x: 7, y: 3 }
print(match p {
    Point { x, y } => x + y,
})"#),
            "10"
        );
    }

    #[test]
    fn match_struct_partial_with_rest() {
        // `Point { x, .. }` matches regardless of the other
        // fields. The walker's match_struct_fields looks up by
        // name; `rest` relaxes the "mention every field" rule.
        assert_eq!(
            say(r#"struct Triple { a, b, c }
let t = Triple { a: 1, b: 2, c: 3 }
print(match t {
    Triple { b, .. } => b,
})"#),
            "2"
        );
    }

    #[test]
    fn match_nested_pattern() {
        // Classic Rust-style: Err(FileError::NotFound(path)).
        assert_eq!(
            say(r#"enum FileError { NotFound(path), Permission(path), Other }
enum Result { Ok(value), Err(error) }
let r = Result::Err(FileError::NotFound("/etc/passwd"))
print(match r {
    Result::Ok(v) => v,
    Result::Err(FileError::NotFound(p)) => p,
    Result::Err(FileError::Permission(p)) => p,
    Result::Err(FileError::Other) => "other",
})"#),
            "/etc/passwd"
        );
    }

    #[test]
    fn match_array_exact() {
        assert_eq!(
            say(r#"let a = [1, 2, 3]
print(match a {
    [] => "empty",
    [x] => "one",
    [x, y] => "two",
    [x, y, z] => x + y + z,
    _ => "long",
})"#),
            "6"
        );
    }

    #[test]
    fn match_array_with_rest() {
        assert_eq!(
            say(r#"let a = [10, 20, 30, 40, 50]
print(match a {
    [head, ..rest] => rest,
    _ => [],
})"#),
            "[20, 30, 40, 50]"
        );
    }

    #[test]
    fn match_array_with_ignored_rest() {
        assert_eq!(
            say(r#"let a = [10, 20, 30]
print(match a {
    [first, ..] => first,
    _ => 0,
})"#),
            "10"
        );
    }

    #[test]
    fn match_binding_scope_limited_to_arm() {
        assert!(
            run_err(r#"let v = 5
match v { x => print(x) }
print(x)"#)
                .contains("not found")
        );
    }

    #[test]
    fn match_negative_literal() {
        assert_eq!(
            say(r#"print(match -3 {
    -3 => "neg three",
    _ => "other",
})"#),
            "neg three"
        );
    }

    #[test]
    fn match_string_literal() {
        assert_eq!(
            say(r#"let s = "hello"
print(match s {
    "hi" => 1,
    "hello" => 2,
    _ => 0,
})"#),
            "2"
        );
    }

    #[test]
    fn match_bool_none() {
        assert_eq!(
            say(r#"print(match true {
    true => "t",
    false => "f",
})"#),
            "t"
        );
        assert_eq!(
            say(r#"print(match none {
    none => "n",
    _ => "other",
})"#),
            "n"
        );
    }

    // ─── `try` operator ────────────────────────────────────────────

    #[test]
    fn try_unwraps_ok_variant() {
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
    fn try_propagates_err_variant() {
        // `try` on Err inside a fn causes the fn to return the
        // same Err variant unchanged. The caller matches it out.
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
    fn try_chains_through_nested_calls() {
        // An Err at the deepest fn short-circuits back up through
        // the whole chain, skipping each caller's remaining work.
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
    fn try_ok_with_unit_variant_yields_none() {
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
    fn try_inside_lambda_returns_from_lambda_only() {
        // The lambda's own fn-boundary catches the `try` unwind;
        // the caller keeps running.
        assert_eq!(
            say(r#"enum Result { Ok(v), Err(e) }
let f = fn() {
    let v = try Result::Err("inner")
    return Result::Ok(v)
}
let r = f()
print("after lambda")
print(match r {
    Result::Ok(_) => "ok",
    Result::Err(e) => e,
})"#),
            "inner"
        );
    }

    #[test]
    fn try_at_top_level_on_err_value_errors() {
        let msg = run_err(
            r#"enum Result { Ok(v), Err(e) }
let r = try Result::Err("boom")"#,
        );
        assert!(msg.contains("top-level"), "got: {}", msg);
    }

    #[test]
    fn try_on_non_result_errors() {
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
    fn try_ok_tuple_wrong_arity_errors() {
        // `Ok(a, b)` isn't a Result-shape for `try` — single
        // positional is the only recognised payload.
        let msg = run_err(
            r#"enum Result { Ok(a, b), Err(e) }
fn doit() {
    let v = try Result::Ok(1, 2)
    return v
}
doit()"#,
        );
        assert!(
            msg.contains("Ok variant must carry exactly one"),
            "got: {}",
            msg
        );
    }

    #[test]
    fn try_in_for_loop_short_circuits() {
        // `try` on the first Err ends the loop and the fn.
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

    #[test]
    fn try_threaded_through_nested_fn_composition() {
        // Mirrors the "try lowers to match" equivalence: a fn
        // using `try` delivers the same outcome as a hand-
        // written match+return using the same Err short-circuit.
        assert_eq!(
            say(r#"enum Result { Ok(v), Err(e) }
fn compute(input) {
    if input < 0 { return Result::Err("negative") }
    return Result::Ok(input * 2)
}
fn with_try(x) {
    let doubled = try compute(x)
    return Result::Ok(doubled + 1)
}
print(match with_try(5) { Result::Ok(v) => v, Result::Err(_) => -1 })
print(match with_try(-1) { Result::Ok(_) => "ok", Result::Err(e) => e })"#),
            "negative"
        );
    }

    // ─── `try_call` builtin ────────────────────────────────────────

    #[test]
    fn try_call_wraps_successful_return_in_ok() {
        // Plain successful call: `try_call(f)` yields
        // `Result::Ok(return_value)`. The program doesn't need
        // to declare `Result` — the value comes out pre-shaped
        // because `try_call` constructs it directly.
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
    fn try_call_wraps_non_fatal_error_in_err() {
        // Division by zero is a non-fatal runtime error, so
        // `try_call` catches it and yields
        // `Result::Err(RuntimeError { message, line })`.
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
    fn try_call_runtime_error_carries_line_number() {
        // The RuntimeError struct exposes `line` so callers can
        // report where the failure happened.
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
    fn try_call_step_limit_error_is_fatal_and_bypasses_wrap() {
        // The step-limit error is fatal — `try_call` must NOT
        // swallow it or the sandbox invariant breaks. The
        // outer `run()` sees the fatal error unchanged.
        let tight = BopLimits {
            max_steps: 200,
            max_memory: 1 << 20,
        };
        let mut host = TestHost::new();
        let err = run(
            r#"let r = try_call(fn() {
    while true { }
})
print("should never run")"#,
            &mut host,
            &tight,
        )
        .unwrap_err();
        assert!(err.is_fatal, "expected fatal: {}", err.message);
        assert!(
            err.message.contains("too many steps"),
            "got: {}",
            err.message
        );
        // The post-try_call `print` never ran because the
        // error short-circuited the program.
        assert!(host.prints.borrow().is_empty());
    }

    #[test]
    fn try_call_plays_with_try_operator_to_chain_errors() {
        // Classic "convert caught runtime error into a Result".
        // The fn uses `try` to short-circuit on Err; the outer
        // wraps the whole thing in try_call to catch anything
        // it didn't anticipate.
        assert_eq!(
            say(r#"fn risky(x) {
    let arr = [1, 2]
    return arr[x]  // out-of-bounds when x > 1
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
    fn try_call_errors_on_wrong_arg_count() {
        let msg = run_err("try_call()");
        assert!(
            msg.contains("try_call` expects 1"),
            "got: {}",
            msg
        );
    }

    #[test]
    fn try_call_errors_on_non_function_arg() {
        let msg = run_err("try_call(42)");
        assert!(
            msg.contains("try_call` expects a function"),
            "got: {}",
            msg
        );
    }

    #[test]
    fn try_call_result_ok_is_matchable_even_without_declared_type() {
        // The returned `Result::Ok(...)` carries a type_name
        // of `"Result"` and variant_name of `"Ok"` — the
        // pattern matcher uses string comparison, so the user's
        // pattern matches regardless of whether they declared
        // their own `Result` enum. Same for `RuntimeError`.
        assert_eq!(
            say(r#"let r = try_call(fn() { return "yay" })
print(match r {
    Result::Ok(v) => v + "!",
    Result::Err(_) => "bad",
})"#),
            "yay!"
        );
    }

    #[test]
    fn try_call_nested_outer_sees_ok_of_inner_err() {
        // Inner try_call catches its own error and returns
        // `Result::Err(...)`. Outer try_call sees a clean
        // return and wraps THAT in `Result::Ok(...)`.
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

    // ─── Integer type (phase 6) ────────────────────────────────────

    #[test]
    fn int_literal_produces_int_value() {
        assert_eq!(say("print(type(42))"), "int");
        assert_eq!(say("print(type(-3))"), "int");
        assert_eq!(say("print(type(0))"), "int");
    }

    #[test]
    fn float_literal_produces_number_value() {
        assert_eq!(say("print(type(42.0))"), "number");
        assert_eq!(say("print(type(3.14))"), "number");
        assert_eq!(say("print(type(-0.5))"), "number");
    }

    #[test]
    fn int_int_arithmetic_stays_int() {
        assert_eq!(say("print(type(1 + 2))"), "int");
        assert_eq!(say("print(1 + 2)"), "3");
        assert_eq!(say("print(10 - 4)"), "6");
        assert_eq!(say("print(3 * 4)"), "12");
        assert_eq!(say("print(10 % 3)"), "1");
    }

    #[test]
    fn int_division_slash_returns_number() {
        // `/` always floats, even Int/Int. Matches Python.
        assert_eq!(say("print(type(10 / 3))"), "number");
        assert_eq!(say("print(10 / 4)"), "2.5");
    }

    #[test]
    fn int_division_slash_slash_returns_int() {
        // `//` — integer division, truncating toward zero.
        assert_eq!(say("print(type(10 // 3))"), "int");
        assert_eq!(say("print(10 // 3)"), "3");
        assert_eq!(say("print(-7 // 2)"), "-3");
        assert_eq!(say("print(10 // -3)"), "-3");
    }

    #[test]
    fn int_number_mixed_widens_to_number() {
        assert_eq!(say("print(type(1 + 2.0))"), "number");
        assert_eq!(say("print(1 + 2.0)"), "3");
        assert_eq!(say("print(3 * 0.5)"), "1.5");
        assert_eq!(say("print(type(2.0 - 1))"), "number");
    }

    #[test]
    fn int_comparison_uses_exact_integer_ordering() {
        assert_eq!(say("print(10 < 20)"), "true");
        assert_eq!(say("print(10 == 10)"), "true");
        // Cross-type numeric equality: int == number when
        // numerically equal.
        assert_eq!(say("print(1 == 1.0)"), "true");
        assert_eq!(say("print(2 > 1.5)"), "true");
    }

    #[test]
    fn int_division_by_zero_errors() {
        let msg = run_err("print(10 // 0)");
        assert!(msg.contains("Division by zero"), "got: {}", msg);
    }

    #[test]
    fn int_overflow_on_add_errors() {
        // i64::MAX + 1 overflows. The message should mention
        // "overflow".
        let msg = run_err("print(9223372036854775807 + 1)");
        assert!(msg.contains("Integer overflow"), "got: {}", msg);
    }

    #[test]
    fn int_overflow_on_neg_of_i64_min_errors() {
        // `i64::MIN` can't be written as a literal (its magnitude
        // exceeds `i64::MAX`), so we build it arithmetically and
        // then negate — which overflows.
        let msg = run_err(
            "let x = -9223372036854775807 - 1\nprint(-x)",
        );
        assert!(msg.contains("overflow"), "got: {}", msg);
    }

    #[test]
    fn int_builtin_converts_to_int() {
        assert_eq!(say("print(int(3.7))"), "3");
        assert_eq!(say("print(type(int(3.7)))"), "int");
        assert_eq!(say(r#"print(int("42"))"#), "42");
        assert_eq!(say(r#"print(type(int("42")))"#), "int");
        // Truncating a string that looks float-y still works.
        assert_eq!(say(r#"print(int("3.7"))"#), "3");
    }

    #[test]
    fn float_builtin_converts_to_number() {
        assert_eq!(say("print(float(42))"), "42");
        assert_eq!(say("print(type(float(42)))"), "number");
        assert_eq!(say(r#"print(float("3.14"))"#), "3.14");
    }

    #[test]
    fn len_returns_int() {
        assert_eq!(say(r#"print(type(len("hi")))"#), "int");
        assert_eq!(say("print(type(len([1, 2, 3])))"), "int");
    }

    #[test]
    fn range_produces_int_elements() {
        assert_eq!(say("print(type(range(3)[0]))"), "int");
    }

    #[test]
    fn array_index_accepts_int_and_float() {
        // Both `arr[0]` (Int) and `arr[0.0]` (Number-via-cast)
        // should work — keeps legacy code with `0.0` running.
        assert_eq!(say("let a = [10, 20]\nprint(a[0])"), "10");
        assert_eq!(say("let a = [10, 20]\nprint(a[0.0])"), "10");
    }

    #[test]
    fn int_match_literal_pattern() {
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

    #[test]
    fn repeat_accepts_int() {
        assert_eq!(
            say(r#"let n = 0
repeat 5 { n = n + 1 }
print(n)"#),
            "5"
        );
    }

    #[test]
    fn int_overflow_literal_parse_errors() {
        // Integer literal that doesn't fit in i64 is a
        // lex-time error, not a silent downgrade to float.
        let msg = parse_err("let x = 99999999999999999999");
        assert!(msg.contains("out of range"), "got: {}", msg);
    }

    // ─── Modules / import ──────────────────────────────────────────

    /// Host that resolves modules from an in-memory map keyed by
    /// the dot-joined import path. Captures prints and tracks how
    /// many times each module was resolved so we can pin the
    /// caching behaviour.
    struct ModuleHost {
        prints: RefCell<Vec<String>>,
        modules: std::collections::HashMap<String, String>,
        resolve_counts: RefCell<std::collections::HashMap<String, u32>>,
    }

    impl ModuleHost {
        fn new(modules: &[(&str, &str)]) -> Self {
            let mut map = std::collections::HashMap::new();
            for (name, source) in modules {
                map.insert((*name).to_string(), (*source).to_string());
            }
            Self {
                prints: RefCell::new(Vec::new()),
                modules: map,
                resolve_counts: RefCell::new(std::collections::HashMap::new()),
            }
        }

        fn prints(&self) -> Vec<String> {
            self.prints.borrow().clone()
        }

        fn resolve_count(&self, name: &str) -> u32 {
            *self
                .resolve_counts
                .borrow()
                .get(name)
                .unwrap_or(&0)
        }
    }

    impl BopHost for ModuleHost {
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
            *self
                .resolve_counts
                .borrow_mut()
                .entry(name.to_string())
                .or_insert(0) += 1;
            self.modules.get(name).cloned().map(Ok)
        }
    }

    #[test]
    fn import_brings_let_binding_into_scope() {
        let mut host = ModuleHost::new(&[("math", "let pi = 3")]);
        run(
            r#"import math
print(pi)"#,
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap();
        assert_eq!(host.prints(), vec!["3"]);
    }

    #[test]
    fn import_brings_fn_into_scope() {
        let mut host = ModuleHost::new(&[(
            "math",
            r#"fn square(n) { return n * n }
let pi = 3"#,
        )]);
        run(
            r#"import math
print(square(5))
print(pi)"#,
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap();
        assert_eq!(host.prints(), vec!["25", "3"]);
    }

    #[test]
    fn import_dotted_path_passes_through_to_host() {
        let mut host = ModuleHost::new(&[("std.math", "let e = 2")]);
        run(
            r#"import std.math
print(e)"#,
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap();
        assert_eq!(host.prints(), vec!["2"]);
        // Exactly one resolve — `std.math` is the full key.
        assert_eq!(host.resolve_count("std.math"), 1);
    }

    #[test]
    fn import_module_not_found_errors() {
        let mut host = ModuleHost::new(&[]);
        let err = run("import nope", &mut host, &BopLimits::standard())
            .unwrap_err();
        assert!(
            err.message.contains("Module `nope` not found"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn import_cache_resolves_once() {
        // Two imports of the same module in the same run should
        // only hit the resolver once.
        let mut host = ModuleHost::new(&[("m", "let x = 1")]);
        run(
            r#"import m
import m
print(x)"#,
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap();
        assert_eq!(host.prints(), vec!["1"]);
        assert_eq!(host.resolve_count("m"), 1);
    }

    #[test]
    fn import_module_can_import_other_modules() {
        let mut host = ModuleHost::new(&[
            ("a", "import b\nlet doubled_pi = pi + pi"),
            ("b", "let pi = 3"),
        ]);
        run(
            r#"import a
print(doubled_pi)"#,
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap();
        assert_eq!(host.prints(), vec!["6"]);
    }

    #[test]
    fn import_circular_detected() {
        let mut host = ModuleHost::new(&[
            ("a", "import b\nlet x = 1"),
            ("b", "import a\nlet y = 2"),
        ]);
        let err = run("import a", &mut host, &BopLimits::standard())
            .unwrap_err();
        assert!(
            err.message.contains("Circular import"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn import_would_shadow_local_binding_errors() {
        let mut host = ModuleHost::new(&[("m", "let x = 99")]);
        let err = run(
            r#"let x = 1
import m"#,
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap_err();
        assert!(
            err.message.contains("would shadow"),
            "got: {}",
            err.message
        );
    }

    // ─── Structs ──────────────────────────────────────────────────

    #[test]
    fn struct_decl_and_construct() {
        assert_eq!(
            say(r#"struct Point { x, y }
let p = Point { x: 3, y: 4 }
print(p.x)
print(p.y)"#),
            "4"
        );
    }

    #[test]
    fn struct_display_shows_type_name_and_fields() {
        assert_eq!(
            say(r#"struct Point { x, y }
let p = Point { x: 3, y: 4 }
print(p)"#),
            "Point { x: 3, y: 4 }"
        );
    }

    #[test]
    fn struct_fields_respect_declaration_order() {
        // Fields specified out of declaration order should still
        // appear in declaration order in the value — stable
        // ordering matters for `print` / `inspect` / equality.
        assert_eq!(
            say(r#"struct Point { x, y }
let p = Point { y: 4, x: 3 }
print(p)"#),
            "Point { x: 3, y: 4 }"
        );
    }

    #[test]
    fn struct_equality_is_structural() {
        assert_eq!(
            say(r#"struct Point { x, y }
let a = Point { x: 1, y: 2 }
let b = Point { x: 1, y: 2 }
print(a == b)"#),
            "true"
        );
        assert_eq!(
            say(r#"struct Point { x, y }
let a = Point { x: 1, y: 2 }
let b = Point { x: 1, y: 3 }
print(a == b)"#),
            "false"
        );
    }

    #[test]
    fn struct_different_types_never_equal() {
        assert_eq!(
            say(r#"struct A { x }
struct B { x }
let a = A { x: 1 }
let b = B { x: 1 }
print(a == b)"#),
            "false"
        );
    }

    #[test]
    fn struct_type_name_is_struct() {
        // `type()` returns a generic bucket; a per-type name
        // would require `display_type_name()` which isn't wired
        // to the builtin yet.
        assert_eq!(
            say(r#"struct Foo { a }
print(type(Foo { a: 1 }))"#),
            "struct"
        );
    }

    #[test]
    fn struct_missing_field_errors() {
        let err = run_err(r#"struct Point { x, y }
let p = Point { x: 1 }"#);
        assert!(err.contains("Missing field"), "got: {}", err);
    }

    #[test]
    fn struct_extra_field_errors() {
        let err = run_err(r#"struct Point { x, y }
let p = Point { x: 1, y: 2, z: 3 }"#);
        assert!(err.contains("has no field"), "got: {}", err);
    }

    #[test]
    fn struct_duplicate_field_errors() {
        let err = run_err(r#"struct Point { x, y }
let p = Point { x: 1, x: 2, y: 3 }"#);
        assert!(err.contains("specified twice"), "got: {}", err);
    }

    #[test]
    fn struct_undeclared_type_errors() {
        let err = run_err(r#"let p = Nope { x: 1 }"#);
        assert!(err.contains("not declared"), "got: {}", err);
    }

    #[test]
    fn struct_field_access_missing_errors() {
        let err = run_err(r#"struct Point { x, y }
let p = Point { x: 1, y: 2 }
print(p.z)"#);
        assert!(err.contains("no field"), "got: {}", err);
    }

    #[test]
    fn struct_field_access_on_non_struct_errors() {
        let err = run_err("let x = 42\nprint(x.value)");
        assert!(err.contains("Can't read field"), "got: {}", err);
    }

    #[test]
    fn struct_duplicate_decl_errors() {
        let err = run_err(r#"struct Foo { x }
struct Foo { y }"#);
        assert!(err.contains("already declared"), "got: {}", err);
    }

    #[test]
    fn struct_nested() {
        assert_eq!(
            say(r#"struct Inner { v }
struct Outer { name, inner }
let o = Outer { name: "nest", inner: Inner { v: 42 } }
print(o.inner.v)"#),
            "42"
        );
    }

    #[test]
    fn struct_in_array_and_iteration() {
        assert_eq!(
            say(r#"struct Item { name, qty }
let cart = [Item { name: "apple", qty: 3 }, Item { name: "banana", qty: 2 }]
let total = 0
for i in cart { total += i.qty }
print(total)"#),
            "5"
        );
    }

    #[test]
    fn struct_literal_disallowed_in_if_condition_parses() {
        // `if Foo { body }` should parse as `if Foo` with body
        // `{ body }`, not as `if (Foo { body })`. Reading a
        // bare `Foo` ident that isn't bound fails at runtime
        // with "not found" — confirming the struct-literal
        // restriction held at parse.
        let err = run_err("if Foo { print(\"hi\") }");
        assert!(err.contains("not found"), "got: {}", err);
    }

    #[test]
    fn struct_literal_disallowed_in_for_iterable() {
        // `for x in arr { body }` — without the struct-literal
        // restriction, `arr { body }` would try to parse as a
        // struct literal where `arr` is the type name and `body`
        // is a field. The restriction ensures the `{` belongs to
        // the for body.
        assert_eq!(
            say("let arr = [1, 2, 3]\nlet sum = 0\nfor x in arr { sum += x }\nprint(sum)"),
            "6"
        );
    }

    #[test]
    fn struct_literal_ok_in_let_rhs() {
        // In a let rhs, struct literals are allowed. This
        // confirms the context flag is re-enabled outside
        // control-flow conditions.
        assert_eq!(
            say(r#"struct P { x }
let p = P { x: 7 }
print(p.x)"#),
            "7"
        );
    }

    #[test]
    fn struct_field_assign_basic() {
        assert_eq!(
            say(r#"struct Point { x, y }
let p = Point { x: 1, y: 2 }
p.x = 99
print(p.x)
print(p.y)"#),
            "2"
        );
    }

    #[test]
    fn struct_field_compound_assign() {
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
    fn struct_field_assign_unknown_field_errors() {
        let err = run_err(r#"struct P { x }
let p = P { x: 1 }
p.y = 99"#);
        assert!(err.contains("no field"), "got: {}", err);
    }

    #[test]
    fn struct_field_assign_on_non_struct_errors() {
        let err = run_err(r#"let x = 5
x.field = 1"#);
        assert!(err.contains("Can't assign to field"), "got: {}", err);
    }

    #[test]
    fn struct_field_assign_chain_via_intermediate_var() {
        // `outer.inner.v = 99` isn't supported yet (needs nested
        // writeback). Users can re-build through intermediate
        // vars instead.
        assert_eq!(
            say(r#"struct Inner { v }
struct Outer { inner }
let o = Outer { inner: Inner { v: 1 } }
let i = o.inner
i.v = 99
o.inner = i
print(o.inner.v)"#),
            "99"
        );
    }

    // ─── Enums ────────────────────────────────────────────────────

    #[test]
    fn enum_unit_variant_basic() {
        assert_eq!(
            say(r#"enum Shape { Empty, Circle(r), Square(s) }
let s = Shape::Empty
print(s)"#),
            "Shape::Empty"
        );
    }

    #[test]
    fn enum_tuple_variant() {
        assert_eq!(
            say(r#"enum Shape { Empty, Circle(r), Pair(x, y) }
let p = Shape::Pair(3, 4)
print(p)"#),
            "Shape::Pair(3, 4)"
        );
    }

    #[test]
    fn enum_struct_variant() {
        assert_eq!(
            say(r#"enum Shape { Rectangle { width, height }, Empty }
let r = Shape::Rectangle { width: 4, height: 3 }
print(r)
print(r.width)
print(r.height)"#),
            "3"
        );
    }

    #[test]
    fn enum_equality_same_variant() {
        assert_eq!(
            say(r#"enum E { A, B(x) }
print(E::A == E::A)
print(E::B(1) == E::B(1))
print(E::B(1) == E::B(2))
print(E::A == E::B(1))"#),
            "false"
        );
    }

    #[test]
    fn enum_different_types_not_equal() {
        assert_eq!(
            say(r#"enum A { X }
enum B { X }
print(A::X == B::X)"#),
            "false"
        );
    }

    #[test]
    fn enum_variant_mismatch_unit_given_args() {
        let err = run_err(r#"enum E { A }
let x = E::A(1)"#);
        assert!(err.contains("no payload"), "got: {}", err);
    }

    #[test]
    fn enum_variant_mismatch_tuple_arity() {
        let err = run_err(r#"enum E { P(x, y) }
let p = E::P(1)"#);
        assert!(err.contains("expects 2 argument"), "got: {}", err);
    }

    #[test]
    fn enum_variant_mismatch_struct_missing_field() {
        let err = run_err(r#"enum E { R { w, h } }
let r = E::R { w: 1 }"#);
        assert!(err.contains("Missing field"), "got: {}", err);
    }

    #[test]
    fn enum_variant_mismatch_struct_extra_field() {
        let err = run_err(r#"enum E { R { w, h } }
let r = E::R { w: 1, h: 2, extra: 3 }"#);
        assert!(err.contains("no field"), "got: {}", err);
    }

    #[test]
    fn enum_undeclared_variant_errors() {
        let err = run_err(r#"enum E { A }
let x = E::Z"#);
        assert!(err.contains("no variant"), "got: {}", err);
    }

    #[test]
    fn enum_undeclared_type_errors() {
        let err = run_err("let x = Nope::V");
        assert!(err.contains("not declared"), "got: {}", err);
    }

    #[test]
    fn enum_struct_variant_field_access() {
        assert_eq!(
            say(r#"enum Shape { Rect { w, h }, Empty }
let r = Shape::Rect { w: 10, h: 3 }
print(r.w * r.h)"#),
            "30"
        );
    }

    #[test]
    fn enum_used_in_if_condition() {
        // The struct-literal disambiguation flag also covers
        // enum struct-variants — `if Foo::V { body }` must
        // parse `V` as a unit variant and `{ body }` as the
        // if's block.
        assert_eq!(
            say(r#"enum E { V }
if E::V == E::V {
    print("yes")
} else {
    print("no")
}"#),
            "yes"
        );
    }

    #[test]
    fn enum_type_name_is_enum() {
        assert_eq!(
            say(r#"enum E { V }
print(type(E::V))"#),
            "enum"
        );
    }

    #[test]
    fn enum_in_array_of_values() {
        assert_eq!(
            say(r#"enum Color { Red, Green, Blue }
let palette = [Color::Red, Color::Green, Color::Blue]
print(palette)"#),
            "[Color::Red, Color::Green, Color::Blue]"
        );
    }

    #[test]
    fn enum_duplicate_decl_errors() {
        let err = run_err(r#"enum E { A }
enum E { B }"#);
        assert!(err.contains("already declared"), "got: {}", err);
    }

    // ─── User-defined methods on structs + enums ──────────────────

    #[test]
    fn method_on_struct_basic() {
        assert_eq!(
            say(r#"struct Point { x, y }
fn Point.sum(self) { return self.x + self.y }
let p = Point { x: 3, y: 4 }
print(p.sum())"#),
            "7"
        );
    }

    #[test]
    fn method_with_extra_args() {
        assert_eq!(
            say(r#"struct Counter { n }
fn Counter.add(self, delta) { return Counter { n: self.n + delta } }
let c = Counter { n: 10 }
let c2 = c.add(5)
print(c2.n)
print(c.n)"#),
            "10"
        );
    }

    #[test]
    fn method_does_not_mutate_receiver() {
        // Mutating `self` inside a method doesn't propagate —
        // Bop passes self by value like any other parameter.
        // Users who want mutation rebind the result.
        assert_eq!(
            say(r#"struct Counter { n }
fn Counter.bump(self) { self.n = self.n + 1 }
let c = Counter { n: 5 }
c.bump()
print(c.n)"#),
            "5"
        );
    }

    #[test]
    fn method_on_enum_dispatches_on_type() {
        assert_eq!(
            say(r#"enum Shape { Circle(r), Rect { w, h }, Empty }
fn Shape.name(self) { return "shape" }
print(Shape::Circle(3).name())
print(Shape::Rect { w: 4, h: 3 }.name())
print(Shape::Empty.name())"#),
            "shape"
        );
    }

    #[test]
    fn method_overrides_builtin() {
        // A user-defined method of the same name as a built-in
        // wins — matches the walker-level dispatch rule.
        assert_eq!(
            say(r#"struct Wrapper { data }
fn Wrapper.len(self) { return 99 }
let w = Wrapper { data: [1, 2, 3] }
print(w.len())"#),
            "99"
        );
    }

    #[test]
    fn method_unknown_on_struct_errors() {
        let err = run_err(r#"struct P { x }
let p = P { x: 1 }
p.nope()"#);
        assert!(err.contains(".nope()"), "got: {}", err);
    }

    #[test]
    fn method_wrong_arg_count_errors() {
        let err = run_err(r#"struct P { x }
fn P.set(self, v) { return P { x: v } }
let p = P { x: 1 }
p.set(1, 2)"#);
        assert!(err.contains("expects"), "got: {}", err);
    }

    #[test]
    fn method_chain_user_defined() {
        assert_eq!(
            say(r#"struct Adder { n }
fn Adder.then(self, m) { return Adder { n: self.n + m } }
let result = Adder { n: 1 }.then(2).then(3).then(4)
print(result.n)"#),
            "10"
        );
    }

    #[test]
    fn method_self_is_clone() {
        // `self` in the method is independent from the caller's
        // binding, even if the method returns self: structural
        // equality still holds on the returned clone.
        assert_eq!(
            say(r#"struct P { x }
fn P.identity(self) { return self }
let a = P { x: 7 }
let b = a.identity()
print(a == b)
print(b.x)"#),
            "7"
        );
    }

    #[test]
    fn method_on_enum_reads_payload_field() {
        assert_eq!(
            say(r#"enum Shape { Circle(r), Rect { w, h } }
fn Shape.label(self, prefix) {
    return prefix + "-shape"
}
let c = Shape::Circle(5)
print(c.label("small"))"#),
            "small-shape"
        );
    }

    #[test]
    fn enum_duplicate_variant_errors() {
        let err = run_err(r#"enum E { A, A }"#);
        assert!(err.contains("duplicate variant"), "got: {}", err);
    }

    #[test]
    fn struct_empty() {
        assert_eq!(
            say(r#"struct Unit { }
let u = Unit { }
print(u)"#),
            "Unit {}"
        );
    }

    #[test]
    fn import_module_does_not_see_importer_scope() {
        // `outer` is defined in the importer's scope; the module
        // must not be able to reach it.
        let mut host = ModuleHost::new(&[("m", "fn leak() { return outer }")]);
        let err = run(
            r#"let outer = 42
import m
print(leak())"#,
            &mut host,
            &BopLimits::standard(),
        )
        .unwrap_err();
        assert!(
            err.message.contains("outer"),
            "expected 'outer' not-found error, got: {}",
            err.message
        );
    }
}
