//! Round-trip tests for the bytecode compiler (step 2a).
//!
//! For each sample program we compile it through `bop-lang`'s parser,
//! feed the AST to `bop-vm`'s compiler, and assert the disassembly
//! matches an expected snapshot. This is not a semantic test — that
//! comes in step 2b via the differential harness — but it pins the
//! emitted shape so future instruction-set changes are visible in the
//! diff.

use bop::parse;
use bop_vm::{compile, disassemble};

fn disasm(source: &str) -> String {
    let ast = parse(source).expect("parse");
    let chunk = compile(&ast).expect("compile");
    disassemble(&chunk)
}

fn assert_disasm(source: &str, expected: &str) {
    let actual = disasm(source);
    let normalize = |s: &str| -> String {
        s.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let actual = normalize(&actual);
    let expected = normalize(expected);
    if actual != expected {
        panic!(
            "disassembly mismatch.\n=== actual ===\n{}\n=== expected ===\n{}\n",
            actual, expected
        );
    }
}

#[test]
fn empty_program_is_just_halt() {
    assert_disasm("", "0: Halt");
}

#[test]
fn numeric_literal_and_print() {
    assert_disasm(
        "print(42)",
        r#"
        0: LoadConst 42
        1: Call print/1
        2: Pop
        3: Halt
        "#,
    );
}

#[test]
fn let_and_use() {
    assert_disasm(
        "let x = 10\nprint(x)",
        r#"
        0: LoadConst 10
        1: DefineLocal x
        2: LoadVar x
        3: Call print/1
        4: Pop
        5: Halt
        "#,
    );
}

#[test]
fn arithmetic_chain() {
    assert_disasm(
        "print(1 + 2 * 3)",
        r#"
        0: LoadConst 1
        1: LoadConst 2
        2: LoadConst 3
        3: Mul
        4: Add
        5: Call print/1
        6: Pop
        7: Halt
        "#,
    );
}

#[test]
fn all_binary_ops_have_direct_opcodes() {
    // Smoke test: every primitive binary operator lowers to one opcode.
    // If this ever requires more than `compile_expr + compile_expr + one op`,
    // something subtle has changed in the op set.
    for (src, op) in [
        ("print(1 + 2)", "Add"),
        ("print(1 - 2)", "Sub"),
        ("print(1 * 2)", "Mul"),
        ("print(1 / 2)", "Div"),
        ("print(1 % 2)", "Rem"),
        ("print(1 == 2)", "Eq"),
        ("print(1 != 2)", "NotEq"),
        ("print(1 < 2)", "Lt"),
        ("print(1 > 2)", "Gt"),
        ("print(1 <= 2)", "LtEq"),
        ("print(1 >= 2)", "GtEq"),
    ] {
        let d = disasm(src);
        assert!(
            d.contains(&format!("{}\n", op)) || d.contains(&format!("{} ", op)),
            "expected {} in disassembly for `{}`:\n{}",
            op,
            src,
            d
        );
    }
}

#[test]
fn unary_ops() {
    assert_disasm(
        "print(-5)\nprint(!true)",
        r#"
        0: LoadConst 5
        1: Neg
        2: Call print/1
        3: Pop
        4: LoadTrue
        5: Not
        6: Call print/1
        7: Pop
        8: Halt
        "#,
    );
}

#[test]
fn short_circuit_and() {
    // `a && b` compiles to:
    //   <a>; TruthyToBool; JumpIfFalsePeek(end); Pop; <b>; TruthyToBool; end:
    assert_disasm(
        "print(true && false)",
        r#"
        0: LoadTrue
        1: TruthyToBool
        2: JumpIfFalsePeek -> 6
        3: Pop
        4: LoadFalse
        5: TruthyToBool
        6: Call print/1
        7: Pop
        8: Halt
        "#,
    );
}

#[test]
fn short_circuit_or() {
    assert_disasm(
        "print(true || false)",
        r#"
        0: LoadTrue
        1: TruthyToBool
        2: JumpIfTruePeek -> 6
        3: Pop
        4: LoadFalse
        5: TruthyToBool
        6: Call print/1
        7: Pop
        8: Halt
        "#,
    );
}

#[test]
fn if_else() {
    assert_disasm(
        r#"if true { print("yes") } else { print("no") }"#,
        r#"
        0: LoadTrue
        1: JumpIfFalse -> 8
        2: PushScope
        3: LoadConst "yes"
        4: Call print/1
        5: Pop
        6: PopScope
        7: Jump -> 13
        8: PushScope
        9: LoadConst "no"
        10: Call print/1
        11: Pop
        12: PopScope
        13: Halt
        "#,
    );
}

#[test]
fn while_with_break_and_continue() {
    assert_disasm(
        r#"while true {
            if false { continue }
            if false { break }
        }"#,
        r#"
        0: LoadTrue
        1: JumpIfFalse -> 15
        2: PushScope
        3: LoadFalse
        4: JumpIfFalse -> 8
        5: PushScope
        6: Jump -> 0
        7: PopScope
        8: LoadFalse
        9: JumpIfFalse -> 13
        10: PushScope
        11: Jump -> 15
        12: PopScope
        13: PopScope
        14: Jump -> 0
        15: Halt
        "#,
    );
}

#[test]
fn for_over_array() {
    assert_disasm(
        "for x in [1, 2] { print(x) }",
        r#"
        0: LoadConst 1
        1: LoadConst 2
        2: MakeArray 2
        3: MakeIter
        4: IterNext -> 12
        5: PushScope
        6: DefineLocal x
        7: LoadVar x
        8: Call print/1
        9: Pop
        10: PopScope
        11: Jump -> 4
        12: Halt
        "#,
    );
}

#[test]
fn repeat_loop() {
    assert_disasm(
        "repeat 3 { print(1) }",
        r#"
        0: LoadConst 3
        1: MakeRepeatCount
        2: RepeatNext -> 9
        3: PushScope
        4: LoadConst 1
        5: Call print/1
        6: Pop
        7: PopScope
        8: Jump -> 2
        9: Halt
        "#,
    );
}

#[test]
fn function_declaration_and_call() {
    let d = disasm(
        r#"fn double(x) { return x * 2 }
print(double(5))"#,
    );
    assert!(d.contains("DefineFn #0 (double)"), "top-level missing fn def:\n{}", d);
    assert!(d.contains("Call double/1"), "call site missing:\n{}", d);
    assert!(d.contains("fn #0 double(x):"), "nested body header missing:\n{}", d);
    // Param `x` resolves to the function's slot 0 (the compile-
    // time `LocalResolver` assigns params in declaration order),
    // so the body reads it via `LoadLocal @0` rather than the
    // old name-keyed `LoadVar`.
    assert!(d.contains("LoadLocal @0"), "body body missing:\n{}", d);
    assert!(d.contains("Mul"), "body op missing:\n{}", d);
    assert!(d.contains("Return"), "body return missing:\n{}", d);
}

#[test]
fn method_call_records_back_assign_for_ident() {
    // `arr.push(1)` is mutating; the compiler records the back-assign
    // target so the VM knows to write the mutated array back.
    assert_disasm(
        "let arr = []\narr.push(1)",
        r#"
        0: MakeArray 0
        1: DefineLocal arr
        2: LoadVar arr
        3: LoadConst 1
        4: CallMethod .push/1 (back to arr)
        5: Pop
        6: Halt
        "#,
    );
}

#[test]
fn method_call_on_literal_has_no_back_assign() {
    // `[1,2].len()` operates on a literal expression; no back-assign.
    let d = disasm("print([1, 2].len())");
    assert!(
        d.contains("CallMethod .len/0\n") || d.contains("CallMethod .len/0 "),
        "expected bare CallMethod .len/0 in:\n{}",
        d
    );
    assert!(!d.contains("(back to"), "literal method call shouldn't back-assign:\n{}", d);
}

#[test]
fn index_get_and_set() {
    assert_disasm(
        "let a = [1,2]\na[0] = 99\nprint(a[1])",
        r#"
        0: LoadConst 1
        1: LoadConst 2
        2: MakeArray 2
        3: DefineLocal a
        4: LoadVar a
        5: LoadConst 0
        6: LoadConst 99
        7: SetIndex
        8: StoreVar a
        9: LoadVar a
        10: LoadConst 1
        11: GetIndex
        12: Call print/1
        13: Pop
        14: Halt
        "#,
    );
}

#[test]
fn compound_assign_on_index_uses_dup2() {
    // `arr[i] += 1` needs to keep (arr, i) for the SetIndex after
    // computing the new value — that's what Dup2 is for.
    let d = disasm("let a = [1]\na[0] += 1");
    assert!(d.contains("Dup2"), "compound index-assign missing Dup2:\n{}", d);
    assert!(d.contains("GetIndex"), "should read current value:\n{}", d);
    assert!(d.contains("Add"), "should apply the compound op:\n{}", d);
    assert!(d.contains("SetIndex"), "should write back:\n{}", d);
}

#[test]
fn string_interpolation_uses_recipe() {
    let d = disasm(r#"let name = "bop"
print("hi {name}!")"#);
    assert!(
        d.contains(r#"StringInterp ["hi ", $name, "!"]"#),
        "interpolation recipe not rendered as expected:\n{}",
        d
    );
}

#[test]
fn dict_literal_layout() {
    // Keys go in as Str constants, values get compiled; MakeDict pops both.
    assert_disasm(
        r#"let d = {"a": 1, "b": 2}"#,
        r#"
        0: LoadConst "a"
        1: LoadConst 1
        2: LoadConst "b"
        3: LoadConst 2
        4: MakeDict 2
        5: DefineLocal d
        6: Halt
        "#,
    );
}

#[test]
fn if_expression_leaves_value_on_stack() {
    // Used as an expression rather than a statement: no `Pop` at the end
    // because the value is consumed by the enclosing `DefineLocal`.
    assert_disasm(
        "let x = if true { 1 } else { 2 }",
        r#"
        0: LoadTrue
        1: JumpIfFalse -> 4
        2: LoadConst 1
        3: Jump -> 5
        4: LoadConst 2
        5: DefineLocal x
        6: Halt
        "#,
    );
}

#[test]
fn nested_function_has_its_own_chunk() {
    let d = disasm(
        r#"fn outer() {
    fn inner() { return 1 }
    return inner()
}"#,
    );
    assert!(d.contains("DefineFn #0 (outer)"), "outer fn missing:\n{}", d);
    assert!(d.contains("fn #0 outer"), "outer body missing:\n{}", d);
    assert!(d.contains("DefineFn #0 (inner)"), "inner fn inside outer missing:\n{}", d);
    assert!(d.contains("fn #0 inner"), "inner body missing:\n{}", d);
}

#[test]
fn break_outside_loop_is_compile_error() {
    let ast = parse("break").expect("parse");
    let err = compile(&ast).expect_err("should reject break outside loop");
    assert!(
        err.message.contains("outside of a loop"),
        "wrong error: {}",
        err.message
    );
}

#[test]
fn continue_outside_loop_is_compile_error() {
    let ast = parse("continue").expect("parse");
    let err = compile(&ast).expect_err("should reject continue outside loop");
    assert!(
        err.message.contains("outside of a loop"),
        "wrong error: {}",
        err.message
    );
}

#[test]
fn constants_and_names_are_deduplicated() {
    let ast = parse(r#"let x = "hi"
let y = "hi"
print(x)
print(x)"#).expect("parse");
    let chunk = compile(&ast).expect("compile");
    // Only one "hi" constant: the string appears once in the pool.
    let hi_count = chunk
        .constants
        .iter()
        .filter(|c| matches!(c, bop_vm::Constant::Str(s) if s == "hi"))
        .count();
    assert_eq!(hi_count, 1, "expected deduplicated string constant");
    // Only one `x` name entry.
    let x_count = chunk.names.iter().filter(|n| n.as_str() == "x").count();
    assert_eq!(x_count, 1, "expected deduplicated name entry");
    // But `y` is its own name.
    assert!(chunk.names.iter().any(|n| n == "y"));
}

#[test]
fn instruction_and_line_tables_have_equal_length() {
    let sources = [
        "",
        "print(1)",
        "let x = 1\nx += 2",
        "if true { print(1) } else { print(2) }",
        "for x in [1, 2, 3] { print(x) }",
        "fn f() { return 1 }\nprint(f())",
    ];
    for src in sources {
        let ast = parse(src).unwrap();
        let chunk = compile(&ast).unwrap();
        assert_eq!(
            chunk.code.len(),
            chunk.lines.len(),
            "code/lines length mismatch for `{}`",
            src
        );
        for func in &chunk.functions {
            assert_eq!(
                func.chunk.code.len(),
                func.chunk.lines.len(),
                "fn `{}` code/lines length mismatch",
                func.name
            );
        }
    }
}
