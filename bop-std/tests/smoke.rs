//! Smoke tests for the bundled stdlib modules. Each module is
//! resolved through `bop_std::resolve` and executed against the
//! tree-walker with a minimal host that:
//!
//! - captures `print` output so tests can assert on it
//! - routes `import std.*` back through `bop_std::resolve` so
//!   stdlib modules importing each other work the same way
//!   `StandardHost` handles them.
//!
//! Keeping this in `bop-std` itself means refactors to the
//! `.bop` sources break cleanly inside this crate rather than
//! trickling into `bop-cli` or embedder tests downstream.

use std::cell::RefCell;

use bop::{BopError, BopHost, BopLimits, Value};

struct BufHost {
    prints: RefCell<Vec<String>>,
}

impl BufHost {
    fn new() -> Self {
        Self {
            prints: RefCell::new(Vec::new()),
        }
    }

    fn last(&self) -> String {
        self.prints
            .borrow()
            .last()
            .cloned()
            .expect("expected at least one print")
    }
}

impl BopHost for BufHost {
    fn call(&mut self, _name: &str, _args: &[Value], _line: u32) -> Option<Result<Value, BopError>> {
        None
    }

    fn on_print(&mut self, msg: &str) {
        self.prints.borrow_mut().push(msg.to_string());
    }

    fn resolve_module(&mut self, name: &str) -> Option<Result<String, BopError>> {
        bop_std::resolve(name).map(|src| Ok(src.to_string()))
    }
}

fn run(code: &str) -> BufHost {
    let mut host = BufHost::new();
    bop::run(code, &mut host, &BopLimits::standard())
        .unwrap_or_else(|e| panic!("program errored: {}\n\n{}", e, code));
    host
}

// ─── std.result ────────────────────────────────────────────────

#[test]
fn result_is_ok_and_is_err() {
    let host = run(
        r#"import std.result
print(is_ok(Result::Ok(1)))
print(is_err(Result::Err("boom")))
print(is_ok(Result::Err("x")))"#,
    );
    assert_eq!(host.prints.borrow().clone(), vec!["true", "true", "false"]);
}

#[test]
fn result_unwrap_or_returns_default_on_err() {
    let host = run(
        r#"import std.result
print(unwrap_or(Result::Ok(42), 0))
print(unwrap_or(Result::Err("boom"), 99))"#,
    );
    assert_eq!(host.prints.borrow().clone(), vec!["42", "99"]);
}

#[test]
fn result_map_applies_only_to_ok() {
    let host = run(
        r#"import std.result
let doubled = map(Result::Ok(5), fn(x) { return x * 2 })
print(match doubled {
    Result::Ok(v) => v,
    Result::Err(_) => -1,
})
let passed = map(Result::Err("stop"), fn(x) { return x * 2 })
print(match passed {
    Result::Ok(_) => "ok?",
    Result::Err(e) => e,
})"#,
    );
    assert_eq!(host.prints.borrow().clone(), vec!["10", "stop"]);
}

#[test]
fn result_and_then_chains_fallible_steps() {
    let host = run(
        r#"import std.result
fn halve(x) {
    if x % 2 == 0 { return Result::Ok(x // 2) }
    return Result::Err("odd")
}
let r = and_then(and_then(Result::Ok(8), halve), halve)
print(match r { Result::Ok(v) => v, Result::Err(e) => e })
let bad = and_then(Result::Ok(7), halve)
print(match bad { Result::Ok(_) => "ok?", Result::Err(e) => e })"#,
    );
    assert_eq!(host.prints.borrow().clone(), vec!["2", "odd"]);
}

// ─── std.math ──────────────────────────────────────────────────

#[test]
fn math_constants_have_expected_precision() {
    let host = run(
        r#"import std.math
print(pi)
print(e)
print(tau)"#,
    );
    let prints = host.prints.borrow();
    assert!(prints[0].starts_with("3.14159"));
    assert!(prints[1].starts_with("2.71828"));
    assert!(prints[2].starts_with("6.28318"));
}

#[test]
fn math_clamp_bounds_correctly() {
    let host = run(
        r#"import std.math
print(clamp(5, 0, 10))
print(clamp(-3, 0, 10))
print(clamp(20, 0, 10))"#,
    );
    assert_eq!(host.prints.borrow().clone(), vec!["5", "0", "10"]);
}

#[test]
fn math_sign_handles_zero_and_negatives() {
    let host = run(
        r#"import std.math
print(sign(5))
print(sign(-3))
print(sign(0))"#,
    );
    assert_eq!(host.prints.borrow().clone(), vec!["1", "-1", "0"]);
}

#[test]
fn math_factorial_and_gcd_and_lcm() {
    let host = run(
        r#"import std.math
print(factorial(5))
print(gcd(12, 18))
print(lcm(4, 6))"#,
    );
    assert_eq!(host.prints.borrow().clone(), vec!["120", "6", "12"]);
}

#[test]
fn math_sum_and_mean() {
    let host = run(
        r#"import std.math
print(sum([1, 2, 3, 4, 5]))
print(mean([2, 4, 6]))"#,
    );
    assert_eq!(host.prints.borrow().clone(), vec!["15", "4"]);
}

// ─── std.iter ──────────────────────────────────────────────────

#[test]
fn iter_map_filter_reduce() {
    let host = run(
        r#"import std.iter
let nums = [1, 2, 3, 4, 5]
let doubled = map(nums, fn(x) { return x * 2 })
print(doubled)
let evens = filter(nums, fn(x) { return x % 2 == 0 })
print(evens)
let total = reduce(nums, 0, fn(a, b) { return a + b })
print(total)"#,
    );
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "[2, 4, 6, 8, 10]");
    assert_eq!(prints[1], "[2, 4]");
    assert_eq!(prints[2], "15");
}

#[test]
fn iter_take_and_drop() {
    let host = run(
        r#"import std.iter
print(take([10, 20, 30, 40], 2))
print(drop([10, 20, 30, 40], 2))"#,
    );
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "[10, 20]");
    assert_eq!(prints[1], "[30, 40]");
}

#[test]
fn iter_zip_and_enumerate() {
    let host = run(
        r#"import std.iter
print(zip([1, 2, 3], ["a", "b", "c"]))
print(enumerate(["x", "y"]))"#,
    );
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "[[1, \"a\"], [2, \"b\"], [3, \"c\"]]");
    assert_eq!(prints[1], "[[0, \"x\"], [1, \"y\"]]");
}

#[test]
fn iter_any_all_count() {
    let host = run(
        r#"import std.iter
let is_pos = fn(x) { return x > 0 }
print(all([1, 2, 3], is_pos))
print(all([1, -2, 3], is_pos))
print(any([-1, -2, 3], is_pos))
print(count([1, 2, 3, 4], fn(x) { return x % 2 == 0 }))"#,
    );
    assert_eq!(
        host.prints.borrow().clone(),
        vec!["true", "false", "true", "2"]
    );
}

#[test]
fn iter_find_and_find_index() {
    let host = run(
        r#"import std.iter
print(find([1, 2, 3], fn(x) { return x > 1 }))
print(find_index([1, 2, 3], fn(x) { return x > 1 }))
print(find([1, 2, 3], fn(x) { return x > 99 }))
print(find_index([1, 2, 3], fn(x) { return x > 99 }))"#,
    );
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "2");
    assert_eq!(prints[1], "1");
    assert_eq!(prints[2], "none");
    assert_eq!(prints[3], "-1");
}

#[test]
fn iter_flatten_sum_product() {
    let host = run(
        r#"import std.iter
print(flatten([[1, 2], [3, 4], [5]]))
print(sum([1, 2, 3, 4]))
print(product([2, 3, 4]))"#,
    );
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "[1, 2, 3, 4, 5]");
    assert_eq!(prints[1], "10");
    assert_eq!(prints[2], "24");
}

#[test]
fn iter_min_and_max() {
    let host = run(
        r#"import std.iter
print(min_array([5, 3, 8, 1, 4]))
print(max_array([5, 3, 8, 1, 4]))"#,
    );
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "1");
    assert_eq!(prints[1], "8");
}

// ─── std.string ────────────────────────────────────────────────

#[test]
fn string_pad_left_right_and_center() {
    let host = run(
        r#"import std.string
print(pad_left("42", 5, " "))
print(pad_right("42", 5, "0"))
print(center("hi", 6, "-"))"#,
    );
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "   42");
    assert_eq!(prints[1], "42000");
    assert_eq!(prints[2], "--hi--");
}

#[test]
fn string_chars_reverse_palindrome() {
    let host = run(
        r#"import std.string
print(chars("abc"))
print(reverse("hello"))
print(is_palindrome("racecar"))
print(is_palindrome("hello"))"#,
    );
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "[\"a\", \"b\", \"c\"]");
    assert_eq!(prints[1], "olleh");
    assert_eq!(prints[2], "true");
    assert_eq!(prints[3], "false");
}

#[test]
fn string_count_substrings() {
    let host = run(
        r#"import std.string
print(count("banana", "na"))
print(count("aaaa", "aa"))
print(count("empty", ""))"#,
    );
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "2");
    // Non-overlapping: "aaaa" → "aa" matches at 0 and 2, count = 2.
    assert_eq!(prints[1], "2");
    assert_eq!(prints[2], "0");
}

// ─── std.test ──────────────────────────────────────────────────

#[test]
fn test_asserts_pass_silently() {
    // Successful asserts shouldn't produce any output or
    // error out.
    let host = run(
        r#"import std.test
assert(true, "should pass")
assert_eq(1 + 1, 2)
assert_near(0.1 + 0.2, 0.3, 0.01)
print("all good")"#,
    );
    assert_eq!(host.last(), "all good");
}

#[test]
fn test_assert_eq_failure_raises() {
    let mut host = BufHost::new();
    let err = bop::run(
        r#"import std.test
assert_eq(1, 2)"#,
        &mut host,
        &BopLimits::standard(),
    )
    .expect_err("assert_eq should raise");
    assert!(err.message.len() > 0, "got empty error message");
}

#[test]
fn test_assert_raises_catches_expected_failure() {
    let host = run(
        r#"import std.test
assert_raises(fn() { return 1 / 0 })
print("caught")"#,
    );
    assert_eq!(host.last(), "caught");
}

// ─── Core math builtins (no import needed) ────────────────────

#[test]
fn core_math_builtins_available_without_import() {
    let host = run(
        r#"print(sqrt(16))
print(floor(3.7))
print(ceil(3.2))
print(round(3.5))
print(pow(2, 10))"#,
    );
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "4");
    assert_eq!(prints[1], "3");
    assert_eq!(prints[2], "4");
    assert_eq!(prints[3], "4");
    assert_eq!(prints[4], "1024");
}

#[test]
fn core_math_floor_ceil_round_return_int_when_possible() {
    let host = run(
        r#"print(type(floor(3.7)))
print(type(ceil(3.2)))
print(type(round(3.5)))"#,
    );
    assert_eq!(
        host.prints.borrow().clone(),
        vec!["int", "int", "int"]
    );
}

// ─── std.collections ──────────────────────────────────────────

#[test]
fn collections_stack_push_top_pop() {
    let host = run(
        r#"import std.collections
let s = stack()
s = s.push(1)
s = s.push(2)
s = s.push(3)
print(s.top())
print(s.size())
s = s.pop()
print(s.top())
print(s.size())"#,
    );
    assert_eq!(
        host.prints.borrow().clone(),
        vec!["3", "3", "2", "2"]
    );
}

#[test]
fn collections_stack_pop_empty_is_noop() {
    let host = run(
        r#"import std.collections
let s = stack()
s = s.pop()
print(s.is_empty())
print(s.top())"#,
    );
    assert_eq!(
        host.prints.borrow().clone(),
        vec!["true", "none"]
    );
}

#[test]
fn collections_queue_fifo_order() {
    let host = run(
        r#"import std.collections
let q = queue()
q = q.enqueue("a")
q = q.enqueue("b")
q = q.enqueue("c")
print(q.front())
print(q.size())
q = q.dequeue()
print(q.front())
q = q.dequeue()
print(q.front())
q = q.dequeue()
print(q.is_empty())"#,
    );
    assert_eq!(
        host.prints.borrow().clone(),
        vec!["a", "3", "b", "c", "true"]
    );
}

#[test]
fn collections_set_add_remove_has() {
    let host = run(
        r#"import std.collections
let s = set()
s = s.add(1)
s = s.add(2)
s = s.add(2)  # duplicate, no-op
s = s.add(3)
print(s.size())
print(s.has(2))
print(s.has(99))
s = s.remove(2)
print(s.has(2))
print(s.size())"#,
    );
    assert_eq!(
        host.prints.borrow().clone(),
        vec!["3", "true", "false", "false", "2"]
    );
}

#[test]
fn collections_set_of_handles_duplicates() {
    let host = run(
        r#"import std.collections
let s = set_of([1, 2, 1, 3, 2])
print(s.size())
print(s.values())"#,
    );
    let prints = host.prints.borrow();
    assert_eq!(prints[0], "3");
    assert_eq!(prints[1], "[1, 2, 3]");
}

#[test]
fn collections_set_union_intersect_difference() {
    let host = run(
        r#"import std.collections
let a = set_of([1, 2, 3])
let b = set_of([2, 3, 4])
print(a.union(b).values())
print(a.intersect(b).values())
print(a.difference(b).values())"#,
    );
    let prints = host.prints.borrow();
    // Union: [1,2,3,4]. intersect: [2,3]. difference: [1].
    assert_eq!(prints[0], "[1, 2, 3, 4]");
    assert_eq!(prints[1], "[2, 3]");
    assert_eq!(prints[2], "[1]");
}

#[test]
fn collections_remove_preserves_order() {
    let host = run(
        r#"import std.collections
let s = set_of([1, 2, 3, 4, 5])
s = s.remove(3)
print(s.values())"#,
    );
    assert_eq!(host.prints.borrow()[0], "[1, 2, 4, 5]");
}

#[test]
fn collections_composes_with_std_iter() {
    // Collections + iter helpers interoperate because a Set's
    // .values() / a Queue's items are ordinary arrays.
    let host = run(
        r#"import std.collections
import std.iter
let s = set_of([1, 2, 3, 4, 5])
let evens = filter(s.values(), fn(x) { return x % 2 == 0 })
print(evens)"#,
    );
    assert_eq!(host.prints.borrow()[0], "[2, 4]");
}
