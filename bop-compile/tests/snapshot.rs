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

use bop_compile::{Options, modules_from_map, transpile};

fn compile(code: &str) -> String {
    transpile(code, &Options::default()).expect("transpile")
}

#[test]
fn inconsistent_or_pattern_is_rejected_before_aot_emission() {
    let error = transpile(
        "let value = match 1 { 1 | y => y, _ => 0 }",
        &Options::default(),
    )
    .expect_err("invalid pattern must fail before Rust emission");

    assert_eq!(
        error.message,
        "`or`-pattern alternative 2 binds `y`, but alternative 1 binds no names"
    );
    assert_eq!(error.line, Some(1));
    assert!(error.friendly_hint.is_some());
}

#[test]
fn targeted_parser_diagnostics_are_preserved_at_the_aot_boundary() {
    let cases = [
        (
            "let label = match 1 {\n  1 => { print(\"one\") },\n  _ => \"other\",\n}",
            "`{ ... }` after `=>` is a dictionary expression, not a match-arm block",
            2,
            8,
            "`match` arm bodies must be a single expression; put it directly after `=>`, or quote dictionary keys if you meant to return a dictionary.",
        ),
        (
            "for i in 0..3 {}",
            "`..` range syntax is not supported in expressions",
            1,
            11,
            "use `range(start, end)` instead, for example `range(0, 3)`.",
        ),
    ];

    for (source, message, line, column, hint) in cases {
        let error = transpile(source, &Options::default())
            .expect_err("parse error must stop AOT emission");
        assert_eq!(error.message, message, "source: {source}");
        assert_eq!(error.line, Some(line), "source: {source}");
        assert_eq!(error.column, Some(column), "source: {source}");
        assert_eq!(error.friendly_hint.as_deref(), Some(hint), "source: {source}");
    }
}

#[test]
fn top_level_try_uses_the_shared_diagnostic_constructor() {
    let source = r#"enum Result { Ok(value), Err(error) }
let value = try Result::Err("boom")"#;
    for sandbox in [false, true] {
        let rust = transpile(
            source,
            &Options {
                sandbox,
                ..Options::default()
            },
        )
        .expect("transpile");
        assert!(rust.contains("::bop::error_messages::top_level_try_error(2)"));
        assert!(!rust.contains("BopError::runtime(\"try encountered Err at top-level\""));
        assert!(rust.contains("if let Some(hint) = &err.friendly_hint"));
        assert!(rust.contains("eprintln!(\"hint: {}\", hint)"));
    }
}

#[test]
fn i64_min_literal_emits_exact_integer_values_and_patterns() {
    let rust = compile(
        r#"let min = -9223372036854775808
print(match min {
    -9223372036854775808 => min,
    _ => 0,
})"#,
    );
    assert!(
        rust.contains("::bop::value::Value::Int(-9223372036854775808i64)"),
        "generated Rust lost the exact expression value"
    );
    assert!(
        rust.contains("::bop::parser::LiteralPattern::Int(-9223372036854775808i64)"),
        "generated Rust lost the exact pattern value"
    );
    assert!(!rust.contains("Value::Number(-9223372036854775808"));
}

#[test]
fn multiline_if_expression_still_rejects_multiple_branch_values_before_aot_emission() {
    let error = transpile(
        "let value = if true {\n    1\n    2\n} else {\n    3\n}",
        &Options::default(),
    )
    .expect_err("a second branch expression must fail before Rust emission");
    assert_eq!(error.message, "Expected `}` but found `an integer`");
    assert_eq!(error.line, Some(3));
    assert_eq!(error.column, Some(5));
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
    // `42` is an int literal in phase 6, so the emitted value is
    // `Value::Int`.
    contains_all(
        &out,
        &[
            "ctx.host.on_print(&__bop_format_print(",
            "::bop::value::Value::Int(42",
        ],
    );
}

#[test]
fn match_block_arm_values_are_parenthesized_for_rustc() {
    let out = compile(
        r#"let flag = true
let value = 40
let unguarded = match flag { true => value + 1, _ => 0 }
let guarded = match flag { true if value > 0 => value + 2, _ => 0 }
print(unguarded, guarded)"#,
    );

    let block_arm_breaks = out
        .lines()
        .filter(|line| line.contains("break 'match_arms_") && line.contains(" ({ let "))
        .count();
    assert_eq!(
        block_arm_breaks, 2,
        "guarded and unguarded block-expression arms must parenthesize the break value:\n{out}"
    );
    assert!(
        !out.lines().any(|line| line.contains("break 'match_arms_") && line.contains(" { let ")),
        "a bare block after a labelled break triggers rustc's break_with_label_and_loop lint:\n{out}"
    );
}

#[test]
fn let_emits_mut_binding() {
    let out = compile("let x = 10");
    contains_all(
        &out,
        &[
            "let mut __bop_user_value_78: ::bop::value::Value = ::bop::value::Value::Int(10",
        ],
    );
}

#[test]
fn compound_assign_routes_through_ops() {
    let out = compile("let x = 1\nx += 5");
    contains_all(
        &out,
        &["__bop_user_value_78 = ::bop::ops::add(&__bop_user_value_78,"],
    );
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
fn for_in_uses_iter_protocol_helpers() {
    // The AOT now emits calls to `__bop_iter_start` /
    // `__bop_iter_step` so for-loops transparently handle
    // Array/Str/Dict (fast path), Value::Iter, and user types
    // with an `.iter()` method through one uniform loop body.
    let out = compile("for x in [1, 2, 3] { print(x) }");
    contains_all(
        &out,
        &["__bop_iter_start(", "__bop_iter_step(", "loop"],
    );
}

#[test]
fn fn_decl_emits_bop_prefixed_fn() {
    let out = compile("fn double(x) { return x * 2 }\nprint(double(5))");
    contains_all(
        &out,
        &[
            "fn __bop_user_fn_n646f75626c65(ctx: &mut Ctx<'_>, __bop_param_0: ::bop::value::Value)",
            "let mut __bop_user_value_78: ::bop::value::Value = __bop_param_0;",
            "Result<::bop::value::Value, ::bop::error::BopError>",
            "__bop_user_fn_n646f75626c65(ctx,",
        ],
    );
}

#[test]
fn duplicate_parameters_rebind_in_order_without_duplicate_rust_arguments() {
    let out = compile("fn pick(value, value) { return value }");
    contains_all(
        &out,
        &[
            "__bop_param_0: ::bop::value::Value",
            "__bop_param_1: ::bop::value::Value",
            "let mut __bop_user_value_76616c7565: ::bop::value::Value = __bop_param_0;",
            "let mut __bop_user_value_76616c7565: ::bop::value::Value = __bop_param_1;",
        ],
    );
    assert!(!out.contains("mut __bop_user_value_76616c7565: ::bop::value::Value,"));
}

#[test]
fn unknown_call_falls_back_to_host() {
    // `readline` isn't in the builtin list and isn't declared in
    // the program, so the emit should route through ctx.host.call
    // and raise a "not found" error on the None branch.
    //
    // Tech-debt-4: the emitted error text now flows through
    // `bop::error_messages::function_not_found`, so the snapshot
    // looks for the call site rather than a raw `format!` string.
    let out = compile(r#"let s = readline("> ")"#);
    contains_all(
        &out,
        &[
            "ctx.host.call(\"readline\",",
            "::bop::error_messages::function_not_found(\"readline\")",
        ],
    );
    // It should NOT try to call a nonexistent user-fn symbol.
    assert!(
        !out.contains("__bop_user_fn_n726561646c696e65"),
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
fn array_and_dict_literals_use_fallible_line_aware_constructors() {
    let arr = compile("let a = [1, 2, 3]");
    contains_all(
        &arr,
        &["::bop::value::Value::try_new_array(vec![", "], 1)?"],
    );

    let dct = compile("\nlet d = {\"a\": 1, \"b\": 2}");
    contains_all(&dct, &["::bop::value::Value::try_new_dict(vec![", "], 2)?"]);
}

#[test]
fn composite_values_propagate_depth_errors_with_source_lines() {
    let out = compile(
        r#"struct Point { x }
enum Shape { Circle(r), Rect { w } }
let p = Point { x: [1] }
let c = Shape::Circle({"r": 2})
let r = Shape::Rect { w: [3] }"#,
    );
    contains_all(
        &out,
        &[
            "Value::try_new_struct(",
            "], 3)?",
            "Value::try_new_enum_tuple(",
            "], 4)?",
            "Value::try_new_enum_struct(",
            "], 5)?",
        ],
    );
}

#[test]
fn lambda_records_opaque_capture_depth_before_move() {
    let out = compile("let captured = [1]\nlet f = fn() { return captured }");
    let depth = out
        .find("let __opaque_body_depth = 0u16.max(__cap_0.ownership_depth())")
        .expect("capture depth should be computed");
    let closure = out[depth..]
        .find("Rc::new(move")
        .map(|offset| depth + offset)
        .expect("capture should move into the callable");
    assert!(
        depth < closure,
        "capture depth must be computed before captures move into the opaque callable"
    );
    contains_all(
        &out,
        &[
            "__bop_wrap_callable(",
            "__opaque_body_depth, 2, __callable)?",
            "Value::try_new_compiled_fn(",
            "opaque_body_depth,",
            "line,",
        ],
    );
}

#[test]
fn lambda_captures_namespaced_constructors_for_depth_accounting() {
    let out = compile_with_modules(
        "use shapes as s\nlet f = fn() { return [s.Point { x: 1 }, s.Maybe::Some(2)] }",
        &[(
            "shapes",
            "struct Point { x }\nenum Maybe { Some(value), None }",
        )],
    )
    .expect("transpile namespaced constructors in lambda");
    assert_eq!(
        out.matches("let __cap_0 = __bop_user_value_73.clone();").count(),
        1,
        "the module alias should be captured exactly once:\n{out}"
    );
    contains_all(
        &out,
        &[
            "let __opaque_body_depth = 0u16.max(__cap_0.ownership_depth())",
            "__bop_validate_namespace_type(&__bop_user_value_73, \"s\", \"Point\", 2)?",
            "__bop_validate_namespace_type(&__bop_user_value_73, \"s\", \"Maybe\", 2)?",
        ],
    );
}

#[test]
fn nested_lambda_shadowing_does_not_capture_same_named_outer_scope_value() {
    let out = compile(
        "let x = [1]\nlet outer = fn(x) { return fn() { return x } }",
    );
    assert_eq!(
        out.matches("let __cap_0 = __bop_user_value_78.clone();").count(),
        1,
        "only the inner lambda should capture the outer lambda parameter:\n{out}"
    );
    contains_all(
        &out,
        &[
            "let __opaque_body_depth = 0u16; let __callable",
            "let __opaque_body_depth = 0u16.max(__cap_0.ownership_depth())",
        ],
    );
}

#[test]
fn rust_keyword_idents_are_mangled() {
    let out = compile("let type = 5\nprint(type)");
    contains_all(&out, &["let mut __bop_user_value_74797065:"]);
    assert!(!out.contains("let mut r#type:"));
}

#[test]
fn every_user_identifier_uses_one_collision_free_namespace() {
    let out = compile(
        r#"fn yield(crate, super, ctx, bop_self) {
    return crate + super + ctx + bop_self
}
let __t0 = 1
let __l = 2
let ctx = 3
let bop_self = 4
let crate = 5
let super = 6
let __bop_user_value_5f5f7430 = 7
print(yield(crate, super, ctx, bop_self), __t0, __l, __bop_user_value_5f5f7430)"#,
    );

    contains_all(
        &out,
        &[
            "fn __bop_user_fn_n7969656c64(",
            "mut __bop_user_value_6372617465:",
            "mut __bop_user_value_7375706572:",
            "mut __bop_user_value_637478:",
            "mut __bop_user_value_626f705f73656c66:",
            "let mut __bop_user_value_5f5f7430:",
            "let mut __bop_user_value_5f5f6c:",
            "let mut __bop_user_value_5f5f626f705f757365725f76616c75655f3566356637343330:",
        ],
    );
    assert!(!out.contains("let mut __t0:"));
    assert!(!out.contains("let mut __l:"));
    assert!(!out.contains("mut crate: ::bop::value::Value"));
    assert!(!out.contains("mut super: ::bop::value::Value"));
    assert!(!out.contains("r#yield"));
}

#[test]
fn module_paths_that_only_differ_by_dot_and_underscore_do_not_collide() {
    let out = compile_with_modules(
        "use a.b as dotted\nuse a_b as underscored\nprint(dotted.helper(), dotted.ctx, underscored.helper(), underscored.yield)",
        &[
            ("a.b", "let ctx = 10\nfn helper() { return 1 }"),
            ("a_b", "let yield = 20\nfn helper() { return 2 }"),
        ],
    )
    .expect("transpile both formerly-colliding modules");

    contains_all(
        &out,
        &[
            "fn __mod_612e62_load(",
            "fn __mod_615f62_load(",
            "struct BopModule612e62Exports {",
            "struct BopModule615f62Exports {",
            "fn __bop_user_fn_m612e62_n68656c706572(",
            "fn __bop_user_fn_m615f62_n68656c706572(",
        ],
    );
    assert!(!out.contains("__load"));
    assert!(!out.contains("__Exports"));
}

#[test]
fn method_call_on_ident_emits_back_assign_for_mutating() {
    // `push` is mutating, so the emitted code must carry the
    // mutated array back into the source binding.
    let out = compile("let a = [1, 2]\na.push(3)");
    contains_all(
        &out,
        &[
            "__bop_call_method(ctx, &",
            "\"push\"",
            "if let Some(__new_obj) = __mutated",
            "__bop_user_value_61 = __new_obj",
        ],
    );
}

#[test]
fn method_call_on_ident_skips_back_assign_for_pure() {
    // `len` is pure, so the mutated slot is discarded with `_`.
    let out = compile("let a = [1, 2, 3]\nprint(a.len())");
    assert!(
        out.contains("let (__r, _) = __bop_call_method(ctx, &"),
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
    // The emission tries user methods first, then falls through
    // to the builtin with `let (__r, _) = ...` for the
    // no-back-assign branch.
    let out = compile("print([1, 2, 3].push(4))");
    assert!(
        out.contains("let (__r, _) = __bop_call_method(ctx, &"),
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
            "__s.push_str(&format!(\"{}\", __bop_user_value_6e616d65.clone()))",
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
            "::bop::ops::index_set(&mut __bop_user_value_61,",
        ],
    );
}

#[test]
fn compound_index_assign_reads_then_writes() {
    let out = compile("let a = [1, 2]\na[0] += 5");
    contains_all(
        &out,
        &[
            "::bop::ops::index_get(&__bop_user_value_61,",
            "::bop::ops::add(",
            "::bop::ops::index_set(&mut __bop_user_value_61,",
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

#[test]
fn const_container_assignment_is_rejected_before_aot_emission() {
    let cases = [
        "const VALUES = [1, 2]\nVALUES[0] = 9",
        "const VALUES = [1, 2]\nVALUES[0] += 9",
        "const LOOKUP = {\"n\": 1}\nLOOKUP[\"n\"] = 9",
        "const LOOKUP = {\"n\": 1}\nLOOKUP[\"n\"] += 9",
        "struct Counter { n }\nconst COUNTER = Counter { n: 1 }\nCOUNTER.n = 9",
        "struct Counter { n }\nconst COUNTER = Counter { n: 1 }\n(COUNTER).n += 9",
        "const GRID = [[1]]\nGRID[0][0] = 9",
    ];

    for source in cases {
        let err = transpile(source, &Options::default()).unwrap_err();
        assert!(
            err.message.contains("can't reassign") && err.message.contains("constant"),
            "source: {source}\nerror: {err}"
        );
        assert_eq!(
            err.friendly_hint.as_deref(),
            Some("constants are immutable. Use `let` if you want a mutable binding."),
            "source: {source}"
        );
    }
}

#[test]
fn const_index_reads_in_mutable_targets_still_emit_aot() {
    let out = compile("const INDEX = 0\nlet values = [1]\nvalues[INDEX] += 2");
    contains_all(
        &out,
        &[
            "__bop_user_value_494e444558.clone()",
            "::bop::ops::index_get(&__bop_user_value_76616c756573,",
            "::bop::ops::index_set(&mut __bop_user_value_76616c756573,",
        ],
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
    // The for-in body starts with `loop { __bop_tick(ctx,` now
    // that the emitter uses the iter-protocol helpers
    // (`__bop_iter_start` / `__bop_iter_step`).
    assert!(
        forin.contains("loop { __bop_tick(ctx,"),
        "expected tick at top of for-in iteration body:\n{}",
        forin
    );
}

#[test]
fn sandbox_emits_tick_at_fn_entry() {
    let out = norm(&compile_sandbox("fn foo() { return 1 }\nprint(foo())"));
    assert!(
        out.contains(
            "fn __bop_user_fn_n666f6f(ctx: &mut Ctx<'_>) -> Result<::bop::value::Value, ::bop::error::BopError> { __bop_tick(ctx,"
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

// ─── Cross-module type clashes (tech debt #3) ─────────────────────
//
// The AOT folds struct / enum decls into a single flat registry
// (see `collect_type_registry` in `emit.rs`). Prior to the
// tech-debt-3 refactor, two modules that declared types with the
// same name would silently overwrite each other — the last one
// seen won. Walker and VM raise on conflicting imports; now AOT
// does too.

fn compile_with_modules(
    code: &str,
    modules: &[(&str, &str)],
) -> Result<String, ::bop::error::BopError> {
    let resolver = modules_from_map(modules.iter().map(|(k, v)| (*k, *v)));
    transpile(
        code,
        &Options {
            emit_main: false,
            use_bop_sys: false,
            sandbox: false,
            module_name: None,
            module_resolver: Some(resolver),
        },
    )
}

fn module_export_fields(generated: &str, module: &str) -> Vec<String> {
    let slug: String = module
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let marker = format!("struct BopModule{slug}Exports {{");
    let body = generated
        .split_once(&marker)
        .unwrap_or_else(|| panic!("missing exports struct for `{module}`:\n{generated}"))
        .1;
    body.lines()
        .skip(1)
        .take_while(|line| line.trim() != "}")
        .map(|line| {
            line.trim()
                .split_once(':')
                .expect("exports struct field")
                .0
                .to_string()
        })
        .collect()
}

#[test]
fn aliased_use_constructs_a_depth_checked_module_value() {
    let out = compile_with_modules("\nuse helpers as h", &[("helpers", "let x = [1]")])
        .expect("transpile aliased module");
    contains_all(
        &out,
        &["Value::new_module(\"helpers\".to_string()", ", 2)?;"],
    );
    assert!(
        !out.contains("BopModule {"),
        "generated code must not bypass BopModule's checked constructor:\n{out}"
    );
}

#[test]
fn diamond_imports_emit_shared_dependency_bindings_in_each_module_scope() {
    let out = compile_with_modules(
        "use left\nuse right\nprint(one, two)",
        &[
            ("shared", "fn helper(n) { return n + 10 }"),
            ("left", "use shared\nlet one = helper(1)"),
            ("right", "use shared\nlet two = helper(2)"),
        ],
    )
    .expect("transpile diamond import");

    assert_eq!(
        out.matches("= __mod_736861726564_load(ctx)?;").count(),
        2,
        "each dependent module needs its own shared-import binding declarations:\n{out}"
    );
}

#[test]
fn import_idempotency_is_local_to_the_generated_binding_scope() {
    let repeated = compile_with_modules(
        "use shared\nuse shared\nprint(helper(1))",
        &[("shared", "fn helper(n) { return n + 10 }")],
    )
    .expect("transpile repeated plain import");
    assert_eq!(
        repeated
            .matches("= __mod_736861726564_load(ctx)?;")
            .count(),
        1,
        "a repeated plain glob in one scope should remain a no-op:\n{repeated}"
    );

    let distinct_functions = compile_with_modules(
        r#"use shared as shared_module
fn one() { use shared; return helper(1) }
fn two() { use shared; return helper(2) }"#,
        &[("shared", "fn helper(n) { return n + 10 }")],
    )
    .expect("transpile function-local imports");
    assert_eq!(
        distinct_functions
            .matches("= __mod_736861726564_load(ctx)?;")
            .count(),
        3,
        "the root alias and both function scopes must each emit their own load/bind site:\n{distinct_functions}"
    );

    let distinct_lambdas = compile_with_modules(
        r#"use shared as shared_module
let one = fn() { use shared; return helper(1) }
let two = fn() { use shared; return helper(2) }"#,
        &[("shared", "fn helper(n) { return n + 10 }")],
    )
    .expect("transpile lambda-local imports");
    assert_eq!(
        distinct_lambdas
            .matches("= __mod_736861726564_load(ctx)?;")
            .count(),
        3,
        "the root alias and both lambda scopes must each emit their own load/bind site:\n{distinct_lambdas}"
    );

    let aliases = compile_with_modules(
        "use shared as first\nuse shared as second",
        &[("shared", "let value = 1")],
    )
    .expect("transpile repeated aliases");
    assert_eq!(
        aliases
            .matches("= __mod_736861726564_load(ctx)?;")
            .count(),
        2,
        "aliases are shaped bindings and must never enter the plain-glob idempotency cache:\n{aliases}"
    );
}

#[test]
fn nested_import_effective_exports_match_the_exact_use_shape() {
    let out = compile_with_modules(
        r#"use aliased as aliased_module
use selected as selected_module
use globbed as globbed_module
use selected_alias as selected_alias_module
use mixed as mixed_module
use chained as chained_module
use hygienic as hygienic_module"#,
        &[
            (
                "shared",
                r#"let public = 1
let _private = 2
fn helper(n) { return n + 1 }
fn _hidden(n) { return n - 1 }
struct Thing { value }"#,
            ),
            ("aliased", "use shared as dep\nlet value = dep.public"),
            ("selected", "use shared.{_private, helper}"),
            ("globbed", "use shared"),
            ("selected_alias", "use shared.{_private, Thing} as chosen"),
            ("mixed", "use shared.{_private}\nuse shared"),
            ("chained", "use aliased as layer"),
            ("hygienic", "use shared as ctx"),
        ],
    )
    .expect("transpile shaped nested imports");

    assert_eq!(
        module_export_fields(&out, "aliased"),
        [
            "__bop_user_value_646570",
            "__bop_user_value_76616c7565",
        ]
    );
    assert_eq!(
        module_export_fields(&out, "selected"),
        [
            "__bop_user_value_5f70726976617465",
            "__bop_user_value_68656c706572",
        ]
    );
    assert_eq!(
        module_export_fields(&out, "globbed"),
        [
            "__bop_user_value_7075626c6963",
            "__bop_user_value_68656c706572",
        ]
    );
    assert_eq!(
        module_export_fields(&out, "selected_alias"),
        ["__bop_user_value_63686f73656e"]
    );
    assert_eq!(
        module_export_fields(&out, "mixed"),
        [
            "__bop_user_value_5f70726976617465",
            "__bop_user_value_7075626c6963",
            "__bop_user_value_68656c706572",
        ]
    );
    assert_eq!(
        module_export_fields(&out, "chained"),
        ["__bop_user_value_6c61796572"]
    );
    assert_eq!(
        module_export_fields(&out, "hygienic"),
        ["__bop_user_value_637478"]
    );
}

#[test]
fn same_named_struct_same_shape_across_modules_is_ok() {
    // Two different modules both declare `struct Point { x, y }`.
    // Same shape → idempotent, mirrors the walker's re-import
    // behaviour.
    let out = compile_with_modules(
        "use geom_a\nuse geom_b",
        &[
            ("geom_a", "struct Point { x, y }"),
            ("geom_b", "struct Point { x, y }"),
        ],
    );
    assert!(out.is_ok(), "expected success, got {:?}", out.err());
}

#[test]
fn same_named_struct_different_shape_across_modules_coexist() {
    // Phase 2b — module-qualified types. Two modules may
    // independently declare `Point` with different fields; the
    // AOT transpiles fine because the resulting types live at
    // distinct identities `(geom_a, Point)` and `(geom_b, Point)`.
    let src = compile_with_modules(
        "use geom_a as a\nuse geom_b as b",
        &[
            ("geom_a", "struct Point { x, y }"),
            ("geom_b", "struct Point { x, y, z }"),
        ],
    )
    .expect("phase 2b should accept same-name, different-shape types");
    // The two module paths show up in the generated Rust as
    // string literals for the type identities.
    assert!(
        src.contains("\"geom_a\"") && src.contains("\"geom_b\""),
        "expected both module paths to surface as identity literals",
    );
}

#[test]
fn same_named_enum_different_variants_across_modules_coexist() {
    // Phase 2b — enum shapes follow the same identity rule as
    // structs. Two `Tag` enums with different variants coexist
    // at `(a, Tag)` and `(b, Tag)`; the AOT happily transpiles.
    let src = compile_with_modules(
        "use a as pa\nuse b as pb",
        &[
            ("a", "enum Tag { Red, Green }"),
            ("b", "enum Tag { Red, Green, Blue }"),
        ],
    )
    .expect("phase 2b should accept same-name, different-variant enums");
    assert!(
        src.contains("\"a\"") && src.contains("\"b\""),
        "expected both module paths to surface as identity literals",
    );
}

#[test]
fn same_named_enum_same_variants_across_modules_is_ok() {
    let out = compile_with_modules(
        "use a\nuse b",
        &[
            ("a", "enum Tag { Red, Green }"),
            ("b", "enum Tag { Red, Green }"),
        ],
    );
    assert!(out.is_ok(), "expected success, got {:?}", out.err());
}

#[test]
fn root_program_and_module_can_declare_same_name_type() {
    // Phase 2b — the root program's `Point` lives at
    // `(<root>, Point)`; the imported module's version lives
    // at `(geom, Point)`. Both constructions compile without
    // clashing, and the emitted Rust carries each identity as
    // a string literal inside its `Value::new_struct` call.
    let src = compile_with_modules(
        r#"struct Point { x, y }
use geom as g
let a = Point { x: 1, y: 2 }
let b = g.Point { x: 1, y: 2, z: 3 }"#,
        &[("geom", "struct Point { x, y, z }")],
    )
    .expect("phase 2b should accept same-name types across root and module");
    assert!(
        src.contains("\"<root>\"") && src.contains("\"geom\""),
        "expected both identities to surface as literals, got:\n{}",
        src,
    );
}
