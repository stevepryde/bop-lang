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
pub mod precheck;

mod evaluator;
mod builtins;
mod methods;

pub use error::BopError;
pub use parser::{Stmt, count_instructions};
pub use value::Value;

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

    /// Called by `print()`. Default: writes to stdout (std only), panics (no-std).
    fn on_print(&mut self, message: &str) {
        #[cfg(feature = "std")]
        {
            println!("{}", message);
        }
        #[cfg(not(feature = "std"))]
        {
            let _ = message;
            panic!("BopHost::on_print must be implemented in no-std environments");
        }
    }

    /// Hint text for "function not found" errors.
    fn function_hint(&self) -> &str {
        ""
    }

    /// Called each tick. Return `Err` to halt execution.
    fn on_tick(&mut self) -> Result<(), BopError> {
        Ok(())
    }
}

// ─── StdHost ───────────────────────────────────────────────────────────────

/// Default host: no custom builtins, print to stdout.
pub struct StdHost;

impl BopHost for StdHost {
    fn call(&mut self, _name: &str, _args: &[Value], _line: u32) -> Option<Result<Value, BopError>> {
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

    #[test]
    fn comments_in_code() {
        assert_eq!(
            say(r#"// this is a comment
let x = 42 // inline comment
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
}
