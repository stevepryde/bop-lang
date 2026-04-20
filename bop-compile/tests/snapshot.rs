//! Snapshot-style tests for the transpiler.
//!
//! These assert on fragments of the emitted Rust rather than the
//! full file. Exact-match snapshots would churn every time we
//! tweak formatting, and what really matters is that the emitted
//! code has the right shape — correct op dispatch, correct fn
//! signatures, correct helper calls.
//!
//! End-to-end compilation (emitted code must actually compile and
//! produce the tree-walker's output) lives in `tests/e2e.rs` behind
//! `#[ignore]`.

use bop_compile::{Options, transpile};

fn compile(code: &str) -> String {
    transpile(code, &Options::default()).expect("transpile")
}

fn contains_all(haystack: &str, needles: &[&str]) {
    for n in needles {
        assert!(
            haystack.contains(n),
            "expected fragment not found: {:?}\n---\n{}\n---",
            n,
            haystack
        );
    }
}

#[test]
fn empty_program_still_produces_runnable_shell() {
    let out = compile("");
    contains_all(
        &out,
        &[
            "fn run_program(ctx: &mut Ctx<'_>)",
            "pub fn run<H: ::bop::BopHost>",
            "fn main()",
            "::bop_sys::StandardHost::new()",
        ],
    );
}

#[test]
fn print_42_emits_on_print_call() {
    let out = compile("print(42)");
    // The body must format args via __bop_format_print and send to
    // ctx.host.on_print — mirroring the tree-walker's print impl.
    contains_all(
        &out,
        &[
            "ctx.host.on_print(&__bop_format_print(",
            "::bop::value::Value::Number(42",
        ],
    );
}

#[test]
fn let_emits_mut_binding() {
    let out = compile("let x = 10");
    contains_all(
        &out,
        &[
            "let mut x: ::bop::value::Value = ::bop::value::Value::Number(10",
        ],
    );
}

#[test]
fn compound_assign_routes_through_ops() {
    let out = compile("let x = 1\nx += 5");
    contains_all(&out, &["x = ::bop::ops::add(&x,"]);
}

#[test]
fn binary_ops_use_ops_module() {
    let programs = [
        ("print(1 + 2)", "::bop::ops::add"),
        ("print(1 - 2)", "::bop::ops::sub"),
        ("print(1 * 2)", "::bop::ops::mul"),
        ("print(1 / 2)", "::bop::ops::div"),
        ("print(1 % 2)", "::bop::ops::rem"),
        ("print(1 < 2)", "::bop::ops::lt"),
        ("print(1 > 2)", "::bop::ops::gt"),
        ("print(1 <= 2)", "::bop::ops::lt_eq"),
        ("print(1 >= 2)", "::bop::ops::gt_eq"),
        ("print(1 == 2)", "::bop::ops::eq"),
        ("print(1 != 2)", "::bop::ops::not_eq"),
    ];
    for (src, op) in programs {
        let out = compile(src);
        assert!(
            out.contains(op),
            "expected {} in output for `{}`:\n{}",
            op,
            src,
            out
        );
    }
}

#[test]
fn short_circuit_and_uses_is_truthy() {
    let out = compile("print(true && false)");
    // Short-circuit `&&` should branch on the left's truthiness
    // without running the full `ops::and` path (there isn't one).
    contains_all(
        &out,
        &[
            "if __l.is_truthy()",
            "::bop::value::Value::Bool(",
        ],
    );
}

#[test]
fn if_else_uses_is_truthy() {
    let out = compile(r#"if true { print("y") } else { print("n") }"#);
    contains_all(&out, &["if (", ").is_truthy()", "else {"]);
}

#[test]
fn while_loop_tests_is_truthy_each_iteration() {
    let out = compile("let i = 0\nwhile i < 5 { i = i + 1 }");
    contains_all(&out, &["while (", ").is_truthy()"]);
}

#[test]
fn repeat_parses_count_and_iterates() {
    let out = compile("repeat 4 { let x = 1 }");
    contains_all(&out, &["::bop::value::Value::Number(n) => n as i64", "for _ in 0.."]);
}

#[test]
fn for_in_materialises_iterable() {
    let out = compile("for x in [1, 2, 3] { print(x) }");
    contains_all(&out, &["__bop_iter_items(", "for x in "]);
}

#[test]
fn fn_decl_emits_bop_prefixed_fn() {
    let out = compile("fn double(x) { return x * 2 }\nprint(double(5))");
    contains_all(
        &out,
        &[
            "fn bop_fn_double(ctx: &mut Ctx<'_>, mut x: ::bop::value::Value)",
            "Result<::bop::value::Value, ::bop::error::BopError>",
            "bop_fn_double(ctx,",
        ],
    );
}

#[test]
fn unknown_call_falls_back_to_host() {
    // `readline` isn't in the builtin list and isn't declared in
    // the program, so the emit should route through ctx.host.call
    // and raise a "not found" error on the None branch.
    let out = compile(r#"let s = readline("> ")"#);
    contains_all(
        &out,
        &[
            "ctx.host.call(\"readline\",",
            "Function `readline` not found",
        ],
    );
    // It should NOT try to call a nonexistent bop_fn_readline.
    assert!(
        !out.contains("bop_fn_readline"),
        "unknown name should not emit a user-fn dispatch:\n{}",
        out
    );
}

#[test]
fn index_read_uses_ops_index_get() {
    let out = compile("let a = [1, 2]\nprint(a[0])");
    contains_all(&out, &["::bop::ops::index_get(&__o, &__i,"]);
}

#[test]
fn array_and_dict_literals_use_new_constructors() {
    let arr = compile("let a = [1, 2, 3]");
    contains_all(&arr, &["::bop::value::Value::new_array(vec!["]);

    let dct = compile(r#"let d = {"a": 1, "b": 2}"#);
    contains_all(&dct, &["::bop::value::Value::new_dict(vec!["]);
}

#[test]
fn rust_keyword_idents_are_raw_escaped() {
    let out = compile("let type = 5\nprint(type)");
    contains_all(&out, &["let mut r#type:"]);
}

#[test]
fn method_call_on_ident_emits_back_assign_for_mutating() {
    // `push` is mutating, so the emitted code must carry the
    // mutated array back into the source binding.
    let out = compile("let a = [1, 2]\na.push(3)");
    contains_all(
        &out,
        &[
            "__bop_call_method(&",
            "\"push\"",
            "if let Some(__new_obj) = __mutated",
            "a = __new_obj",
        ],
    );
}

#[test]
fn method_call_on_ident_skips_back_assign_for_pure() {
    // `len` is pure, so the mutated slot is discarded with `_`.
    let out = compile("let a = [1, 2, 3]\nprint(a.len())");
    assert!(
        out.contains("let (__ret, _) = __bop_call_method(&"),
        "expected pure-method discard in:\n{}",
        out
    );
    assert!(
        !out.contains("if let Some(__new_obj)"),
        "pure method shouldn't emit the back-assign branch:\n{}",
        out
    );
}

#[test]
fn method_call_on_literal_has_no_back_assign() {
    // `[1,2,3].push(...)` has no target ident — the mutation is
    // observed and then discarded, same as in the tree-walker.
    let out = compile("print([1, 2, 3].push(4))");
    assert!(
        out.contains("let (__ret, _) = __bop_call_method(&"),
        "expected literal-receiver discard in:\n{}",
        out
    );
}

#[test]
fn string_interp_builds_string_via_format() {
    let out = compile(r#"let name = "bop"
print("hi {name}!")"#);
    contains_all(
        &out,
        &[
            "::std::string::String::new()",
            "__s.push_str(\"hi \")",
            "__s.push_str(&format!(\"{}\", name.clone()))",
            "__s.push_str(\"!\")",
            "::bop::value::Value::new_str(__s)",
        ],
    );
}

#[test]
fn index_assign_routes_through_ops_index_set() {
    let out = compile("let a = [1, 2, 3]\na[0] = 99");
    contains_all(
        &out,
        &[
            "::bop::ops::index_set(&mut a,",
        ],
    );
}

#[test]
fn compound_index_assign_reads_then_writes() {
    let out = compile("let a = [1, 2]\na[0] += 5");
    contains_all(
        &out,
        &[
            "::bop::ops::index_get(&a,",
            "::bop::ops::add(",
            "::bop::ops::index_set(&mut a,",
        ],
    );
}

#[test]
fn index_assign_on_non_ident_is_rejected() {
    // The tree-walker rejects `[1,2][0] = 3` too — the error
    // message matches, for differential-harness peace of mind.
    let err = transpile("[1, 2][0] = 3", &Options::default()).unwrap_err();
    assert!(
        err.message.contains("Can only assign to indexed variables"),
        "got: {}",
        err.message
    );
}

// ─── Sandbox mode ─────────────────────────────────────────────────

fn compile_sandbox(code: &str) -> String {
    transpile(
        code,
        &Options {
            sandbox: true,
            ..Options::default()
        },
    )
    .expect("transpile")
}

#[test]
fn sandbox_off_by_default_emits_no_tick_helper() {
    let out = compile("while true { }");
    assert!(
        !out.contains("__bop_tick"),
        "non-sandbox build shouldn't emit __bop_tick:\n{}",
        out
    );
    assert!(
        out.contains("bop_memory_init(usize::MAX)"),
        "non-sandbox init should disable the memory ceiling:\n{}",
        out
    );
}

#[test]
fn sandbox_on_emits_tick_helper_and_limits_param() {
    let out = compile_sandbox("while true { }");
    let flat = norm(&out);
    for needle in [
        "fn __bop_tick(ctx: &mut Ctx<'_>, line: u32)",
        "ctx.max_steps",
        "bop_memory_init(limits.max_memory)",
        "pub fn run<H: ::bop::BopHost>( host: &mut H, limits: &::bop::BopLimits, )",
        "let limits = ::bop::BopLimits::standard();",
    ] {
        assert!(
            flat.contains(needle),
            "expected fragment not found: {:?}\n---\n{}\n---",
            needle,
            out
        );
    }
}

#[test]
fn sandbox_emits_tick_at_while_iteration() {
    let out = norm(&compile_sandbox("while true { let x = 1 }"));
    assert!(
        out.contains("while (::bop::value::Value::Bool(true)).is_truthy() { __bop_tick(ctx,"),
        "expected tick at top of while body:\n{}",
        out
    );
}

/// Normalize whitespace runs to single spaces so we can do
/// position-insensitive substring checks on the pretty-printed
/// output.
fn norm(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            out.push(c);
            last_space = false;
        }
    }
    out
}

#[test]
fn sandbox_emits_tick_at_repeat_and_for() {
    let repeat = norm(&compile_sandbox("repeat 3 { let x = 1 }"));
    assert!(
        repeat.contains(".max(0)) { __bop_tick(ctx,"),
        "expected tick at top of repeat iteration:\n{}",
        repeat
    );

    let forin = norm(&compile_sandbox("for x in [1, 2] { let y = x }"));
    // The iter-items tmp number depends on how many temporaries
    // the array literal introduced; just check that `for x in
    // <something> { __bop_tick(...)` appears somewhere.
    let forin_matches = forin.split("for x in ").any(|tail| {
        tail.split_once(' ')
            .map(|(_ident, rest)| rest.trim_start().starts_with("{ __bop_tick(ctx,"))
            .unwrap_or(false)
    });
    assert!(
        forin_matches,
        "expected tick at top of for-in iteration:\n{}",
        forin
    );
}

#[test]
fn sandbox_emits_tick_at_fn_entry() {
    let out = norm(&compile_sandbox("fn foo() { return 1 }\nprint(foo())"));
    assert!(
        out.contains(
            "fn bop_fn_foo(ctx: &mut Ctx<'_>) -> Result<::bop::value::Value, ::bop::error::BopError> { __bop_tick(ctx,"
        ),
        "expected tick at function entry:\n{}",
        out
    );
}

#[test]
fn sandbox_run_program_ticks_once_on_entry() {
    let out = norm(&compile_sandbox("print(1)"));
    // Top-level `run_program` is the program-scope equivalent of a
    // function entry, so it ticks once before doing anything else.
    assert!(
        out.contains(
            "fn run_program(ctx: &mut Ctx<'_>) -> Result<(), ::bop::error::BopError> { __bop_tick(ctx,"
        ),
        "expected tick at run_program entry:\n{}",
        out
    );
}

#[test]
fn module_name_wraps_output_and_skips_main() {
    let opts = Options {
        module_name: Some("my_prog".into()),
        ..Options::default()
    };
    let out = transpile("print(1)", &opts).unwrap();
    assert!(
        out.starts_with("pub mod my_prog {\n"),
        "expected module wrapper prefix:\n{}",
        out
    );
    assert!(
        out.trim_end().ends_with('}'),
        "expected closing `}}`:\n{}",
        out
    );
    assert!(
        !out.contains("fn main()"),
        "module mode should skip main:\n{}",
        out
    );
    // The run fn should still be there, now addressed as
    // `my_prog::run`.
    assert!(
        out.contains("pub fn run<H: ::bop::BopHost>"),
        "expected pub fn run:\n{}",
        out
    );
}

#[test]
fn options_without_main_skip_entry_point() {
    let opts = Options {
        emit_main: false,
        use_bop_sys: false,
        sandbox: false,
        module_name: None,
        module_resolver: None,
    };
    let out = transpile("print(1)", &opts).unwrap();
    assert!(
        !out.contains("fn main()"),
        "expected no main() when emit_main is false:\n{}",
        out
    );
    // But the library entry point (`run`) should still be there so
    // the emitter's output is usable.
    assert!(
        out.contains("pub fn run<H: ::bop::BopHost>"),
        "expected `run` to still be emitted:\n{}",
        out
    );
}
