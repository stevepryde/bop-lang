//! Static checks that run after parse, before execution.
//!
//! Currently the only check is **match exhaustiveness**: if
//! every arm of a `match` is an enum-variant pattern on the
//! same enum type and there's no catch-all, we can tell —
//! from the declared variant list — whether the match covers
//! them all. Missing variants surface as `BopWarning`s the CLI
//! prints before running; uncovered variants *still* raise a
//! "No match arm matched" runtime error if they ever fire, so
//! the check is advisory rather than load-bearing.
//!
//! Kept deliberately narrow:
//!
//! - Only enum-shaped matches (all arms `EnumType::Variant`
//!   with the same outer `EnumType`) are analysed. Literal
//!   matches and heterogeneous matches are skipped — they'd
//!   need a different notion of "coverage".
//! - Guards on arms don't count toward coverage. `Variant(x)
//!   if x > 0` matches a *subset* of the variant, so the arm
//!   no longer fully covers `Variant`.
//!
//! Imports: `check_program` alone only sees enums declared in
//! the analysed AST, so `match` arms over an imported enum
//! would look under-covered. Callers with access to a module
//! resolver (the CLI has one via `BopHost::resolve_module`)
//! should use [`check_program_with_resolver`] instead — it
//! walks every top-level `use` statement, parses the referenced
//! module's source, and folds its (transitive) enum decls into
//! the table before running the checks. A resolver that returns
//! `None` or an error for a given module is treated as a
//! silent opacity fallback rather than a hard failure; the
//! checker is advisory.

#[cfg(not(feature = "std"))]
use alloc::{format, string::String, vec::Vec};

use crate::error::BopWarning;
use crate::parser::{
    Expr, ExprKind, MatchArm, Pattern, Stmt, StmtKind, VariantDecl,
};

#[cfg(not(feature = "std"))]
use alloc_import::collections::{BTreeMap, BTreeSet};
#[cfg(feature = "std")]
use std::collections::{BTreeMap, BTreeSet};

#[cfg(not(feature = "std"))]
use alloc as alloc_import;

/// Run every static check over `stmts` and collect the
/// resulting warnings. Never errors — warnings are the only
/// output.
///
/// See [`check_program_with_resolver`] for a variant that
/// walks `use` statements to pick up imported enum
/// declarations; this plain version treats imported enums as
/// opaque and skips exhaustiveness warnings on them.
pub fn check_program(stmts: &[Stmt]) -> Vec<BopWarning> {
    let mut warnings = Vec::new();
    let enums = collect_enum_decls(stmts);
    check_stmts(stmts, &enums, &mut warnings);
    warnings
}

/// Like [`check_program`] but follows `use` statements via a
/// module resolver so `match` arms over imported enums can be
/// exhaustiveness-checked. `resolver` has the same shape as
/// `BopHost::resolve_module`:
///
/// - `Some(Ok(source))` — module source; parsed + its enums
///   (transitively) folded into the table.
/// - `Some(Err(_))` — resolver failed; treated the same as
///   `None` (check skips the enum; advisory fallback).
/// - `None` — not our module; skip.
///
/// The checker never surfaces a failure from the resolver — it
/// simply falls back to "imported enums stay opaque" in that
/// case, the same behaviour [`check_program`] has.
pub fn check_program_with_resolver<R>(
    stmts: &[Stmt],
    resolver: &mut R,
) -> Vec<BopWarning>
where
    R: FnMut(&str) -> Option<Result<String, crate::error::BopError>>,
{
    let mut warnings = Vec::new();
    let mut enums = collect_enum_decls(stmts);
    let mut visited: BTreeSet<String> = BTreeSet::new();
    collect_imported_enum_decls(stmts, resolver, &mut enums, &mut visited);
    check_stmts(stmts, &enums, &mut warnings);
    warnings
}

/// Recursively walk `use` statements and fold each imported
/// module's enum declarations into `enums`. Silently drops
/// parse / resolver failures — the checker is advisory, not
/// load-bearing.
fn collect_imported_enum_decls<R>(
    stmts: &[Stmt],
    resolver: &mut R,
    enums: &mut BTreeMap<String, Vec<VariantDecl>>,
    visited: &mut BTreeSet<String>,
) where
    R: FnMut(&str) -> Option<Result<String, crate::error::BopError>>,
{
    for stmt in stmts {
        if let StmtKind::Use { path, .. } = &stmt.kind {
            if !visited.insert(path.clone()) {
                // Already pulled in via a shallower import.
                continue;
            }
            let source = match resolver(path) {
                Some(Ok(s)) => s,
                _ => continue,
            };
            let imported_stmts = match crate::parse(&source) {
                Ok(v) => v,
                Err(_) => continue,
            };
            // Same-name enums already declared in the root win
            // (first-write-wins). Imported modules should only
            // *supply* enums the root program doesn't already
            // know about.
            for (name, variants) in collect_enum_decls(&imported_stmts) {
                enums.entry(name).or_insert(variants);
            }
            // Recurse so a module's own imports are followed.
            collect_imported_enum_decls(&imported_stmts, resolver, enums, visited);
        }
    }
}

/// Walk every top-level `enum Foo { ... }` decl in the program
/// so exhaustiveness checks can consult the variant list
/// without re-walking the AST per match. Enums nested inside
/// fn bodies are included too — Bop lets you declare types
/// anywhere, and a match in a sibling fn can still reach them.
fn collect_enum_decls(stmts: &[Stmt]) -> BTreeMap<String, Vec<VariantDecl>> {
    let mut enums = BTreeMap::new();
    collect_enum_decls_rec(stmts, &mut enums);
    enums
}

fn collect_enum_decls_rec(stmts: &[Stmt], enums: &mut BTreeMap<String, Vec<VariantDecl>>) {
    for stmt in stmts {
        match &stmt.kind {
            StmtKind::EnumDecl { name, variants } => {
                enums.insert(name.clone(), variants.clone());
            }
            StmtKind::FnDecl { body, .. } => {
                collect_enum_decls_rec(body, enums);
            }
            StmtKind::MethodDecl { body, .. } => {
                collect_enum_decls_rec(body, enums);
            }
            StmtKind::If {
                body,
                else_ifs,
                else_body,
                ..
            } => {
                collect_enum_decls_rec(body, enums);
                for (_, b) in else_ifs {
                    collect_enum_decls_rec(b, enums);
                }
                if let Some(eb) = else_body {
                    collect_enum_decls_rec(eb, enums);
                }
            }
            StmtKind::While { body, .. }
            | StmtKind::Repeat { body, .. }
            | StmtKind::ForIn { body, .. } => {
                collect_enum_decls_rec(body, enums);
            }
            _ => {}
        }
    }
}

fn check_stmts(
    stmts: &[Stmt],
    enums: &BTreeMap<String, Vec<VariantDecl>>,
    warnings: &mut Vec<BopWarning>,
) {
    for stmt in stmts {
        check_stmt(stmt, enums, warnings);
    }
}

fn check_stmt(
    stmt: &Stmt,
    enums: &BTreeMap<String, Vec<VariantDecl>>,
    warnings: &mut Vec<BopWarning>,
) {
    match &stmt.kind {
        StmtKind::Let { value, .. } => check_expr(value, enums, warnings),
        StmtKind::Assign { value, .. } => check_expr(value, enums, warnings),
        StmtKind::ExprStmt(expr) => check_expr(expr, enums, warnings),
        StmtKind::Return { value: Some(expr) } => check_expr(expr, enums, warnings),
        StmtKind::Return { value: None } => {}
        StmtKind::If {
            condition,
            body,
            else_ifs,
            else_body,
        } => {
            check_expr(condition, enums, warnings);
            check_stmts(body, enums, warnings);
            for (c, b) in else_ifs {
                check_expr(c, enums, warnings);
                check_stmts(b, enums, warnings);
            }
            if let Some(eb) = else_body {
                check_stmts(eb, enums, warnings);
            }
        }
        StmtKind::While { condition, body } => {
            check_expr(condition, enums, warnings);
            check_stmts(body, enums, warnings);
        }
        StmtKind::Repeat { count, body } => {
            check_expr(count, enums, warnings);
            check_stmts(body, enums, warnings);
        }
        StmtKind::ForIn { iterable, body, .. } => {
            check_expr(iterable, enums, warnings);
            check_stmts(body, enums, warnings);
        }
        StmtKind::FnDecl { body, .. } => {
            check_stmts(body, enums, warnings);
        }
        StmtKind::MethodDecl { body, .. } => {
            check_stmts(body, enums, warnings);
        }
        // Declarations, imports, breaks, continues — no sub-expr
        // to check.
        _ => {}
    }
}

fn check_expr(
    expr: &Expr,
    enums: &BTreeMap<String, Vec<VariantDecl>>,
    warnings: &mut Vec<BopWarning>,
) {
    match &expr.kind {
        ExprKind::Match { scrutinee, arms } => {
            check_expr(scrutinee, enums, warnings);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    check_expr(guard, enums, warnings);
                }
                check_expr(&arm.body, enums, warnings);
            }
            check_match_exhaustive(arms, enums, expr.line, warnings);
        }
        // Recurse into every sub-expression that could contain
        // a `match`. This is a bit verbose but avoids visitor
        // boilerplate; add a variant to `walk_exprs` only if
        // the recursion list grows much.
        ExprKind::BinaryOp { left, right, .. } => {
            check_expr(left, enums, warnings);
            check_expr(right, enums, warnings);
        }
        ExprKind::UnaryOp { expr: e, .. } => check_expr(e, enums, warnings),
        ExprKind::Call { callee, args } => {
            check_expr(callee, enums, warnings);
            for a in args {
                check_expr(a, enums, warnings);
            }
        }
        ExprKind::MethodCall { object, args, .. } => {
            check_expr(object, enums, warnings);
            for a in args {
                check_expr(a, enums, warnings);
            }
        }
        ExprKind::Index { object, index } => {
            check_expr(object, enums, warnings);
            check_expr(index, enums, warnings);
        }
        ExprKind::Array(items) => {
            for item in items {
                check_expr(item, enums, warnings);
            }
        }
        ExprKind::Dict(entries) => {
            for (_, v) in entries {
                check_expr(v, enums, warnings);
            }
        }
        ExprKind::IfExpr {
            condition,
            then_expr,
            else_expr,
        } => {
            check_expr(condition, enums, warnings);
            check_expr(then_expr, enums, warnings);
            check_expr(else_expr, enums, warnings);
        }
        ExprKind::Lambda { body, .. } => {
            check_stmts(body, enums, warnings);
        }
        ExprKind::FieldAccess { object, .. } => check_expr(object, enums, warnings),
        ExprKind::StructConstruct { fields, .. } => {
            for (_, v) in fields {
                check_expr(v, enums, warnings);
            }
        }
        ExprKind::EnumConstruct { payload, .. } => {
            use crate::parser::VariantPayload;
            match payload {
                VariantPayload::Unit => {}
                VariantPayload::Tuple(args) => {
                    for a in args {
                        check_expr(a, enums, warnings);
                    }
                }
                VariantPayload::Struct(fields) => {
                    for (_, v) in fields {
                        check_expr(v, enums, warnings);
                    }
                }
            }
        }
        ExprKind::Try(inner) => check_expr(inner, enums, warnings),
        // Literals, identifiers, string interpolation, none —
        // nothing to recurse into.
        _ => {}
    }
}

/// Core of the check: given a match's arms and the declared
/// enums, determine whether the match is exhaustive and emit
/// a warning if not.
fn check_match_exhaustive(
    arms: &[MatchArm],
    enums: &BTreeMap<String, Vec<VariantDecl>>,
    match_line: u32,
    warnings: &mut Vec<BopWarning>,
) {
    // Step 1: any catch-all arm without a guard makes the
    // match trivially exhaustive (the fallback always fires).
    // A guarded catch-all doesn't count — the guard can veto.
    for arm in arms {
        if arm.guard.is_some() {
            continue;
        }
        if is_catch_all(&arm.pattern) {
            return;
        }
    }

    // Step 2: unify the enum under scrutiny. If every
    // non-guarded arm's *top-level* pattern references the
    // same `EnumType::*`, that's the enum we can check. If
    // arms are heterogeneous (literals, structs, arrays, or
    // two different enums), we bail — no coherent coverage
    // analysis applies. `target_enum` is owned (rather than a
    // borrow out of the AST) so the check pass doesn't need to
    // thread lifetimes through every helper.
    let mut target_enum: Option<String> = None;
    let mut covered: Vec<String> = Vec::new();
    for arm in arms {
        // Guarded arms narrow their variant (the body only
        // runs when the guard is truthy) so they don't
        // contribute to coverage. Unguarded arms do.
        let contributes = arm.guard.is_none();
        if !gather_variants(&arm.pattern, &mut target_enum, &mut covered, contributes) {
            return;
        }
    }

    let Some(enum_name) = target_enum else {
        // No enum-variant arm at all — pattern set is entirely
        // literals / structs / etc., which we can't
        // exhaustiveness-check at this level.
        return;
    };
    let Some(decl) = enums.get(&enum_name) else {
        // Enum isn't declared locally — could be imported;
        // bail rather than warn on a potentially-complete
        // match we can't verify.
        return;
    };

    let missing: Vec<&str> = decl
        .iter()
        .filter(|v| !covered.iter().any(|c| c == &v.name))
        .map(|v| v.name.as_str())
        .collect();
    if missing.is_empty() {
        return;
    }

    let list = missing.join(", ");
    let msg = format!(
        "non-exhaustive `match` on `{}`: missing {}",
        enum_name,
        missing
            .iter()
            .map(|v| format!("`{}::{}`", enum_name, v))
            .collect::<Vec<_>>()
            .join(", "),
    );
    let hint = format!(
        "add an arm for each missing variant, or a `_` catch-all. Missing: {}",
        list
    );
    warnings.push(BopWarning::at(msg, match_line).with_hint(hint));
}

/// A pattern that matches every value regardless of shape —
/// wildcard or a bare binding. Or-patterns made entirely of
/// catch-alls count too. Everything else is skipped for this
/// check.
fn is_catch_all(pattern: &Pattern) -> bool {
    match pattern {
        Pattern::Wildcard | Pattern::Binding(_) => true,
        Pattern::Or(alts) => alts.iter().all(is_catch_all),
        _ => false,
    }
}

/// Fold the variants an arm's pattern references into
/// `covered`. Returns `true` if the arm fits the "all arms
/// reference the same enum" precondition; `false` to bail the
/// check entirely. `contributes` is `false` for guarded arms —
/// we still want to confirm the arm's enum matches, but we
/// won't count it toward coverage.
fn gather_variants(
    pattern: &Pattern,
    target_enum: &mut Option<String>,
    covered: &mut Vec<String>,
    contributes: bool,
) -> bool {
    // Catch-all arms can't happen here — the outer scan
    // returns early on them. A guarded binding *can*, and is
    // treated as "contributes nothing, but doesn't break the
    // precondition".
    match pattern {
        Pattern::Wildcard | Pattern::Binding(_) => true,
        Pattern::EnumVariant {
            type_name,
            variant,
            ..
        } => {
            // `target_enum` is owned (`Option<String>`) so the
            // first enum-variant pattern seeds it and every
            // subsequent arm compares against the stored name.
            // No lifetimes, no leaks — the extra allocation is
            // one `String` per analysed match, which is
            // negligible next to the rest of the check pass.
            match target_enum {
                None => {
                    *target_enum = Some(type_name.clone());
                    if contributes {
                        covered.push(variant.clone());
                    }
                    true
                }
                Some(existing) if existing == type_name => {
                    if contributes {
                        covered.push(variant.clone());
                    }
                    true
                }
                _ => false, // two different enums in one match
            }
        }
        Pattern::Or(alts) => {
            for alt in alts {
                if !gather_variants(alt, target_enum, covered, contributes) {
                    return false;
                }
            }
            true
        }
        // Literal / struct / array patterns on an enum scrutinee
        // don't fit coverage analysis — bail.
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn warnings(source: &str) -> Vec<BopWarning> {
        let stmts = parse(source).unwrap();
        check_program(&stmts)
    }

    #[test]
    fn exhaustive_match_produces_no_warning() {
        let src = r#"enum Shape { Circle(r), Square(s) }
fn area(s) {
    return match s {
        Shape::Circle(r) => r * r,
        Shape::Square(s) => s * s,
    }
}"#;
        assert!(warnings(src).is_empty());
    }

    #[test]
    fn wildcard_arm_counts_as_exhaustive() {
        let src = r#"enum Shape { Circle(r), Square(s), Triangle }
let s = Shape::Circle(5)
let _ = match s {
    Shape::Circle(r) => r,
    _ => 0,
}"#;
        assert!(warnings(src).is_empty());
    }

    #[test]
    fn bare_binding_arm_counts_as_exhaustive() {
        let src = r#"enum Shape { Circle(r), Square(s) }
let s = Shape::Circle(5)
let _ = match s {
    Shape::Circle(r) => r,
    other => 0,
}"#;
        assert!(warnings(src).is_empty());
    }

    #[test]
    fn missing_variant_warns() {
        let src = r#"enum Shape { Circle(r), Square(s), Triangle }
let s = Shape::Circle(5)
let _ = match s {
    Shape::Circle(r) => r,
    Shape::Square(s) => s,
}"#;
        let ws = warnings(src);
        assert_eq!(ws.len(), 1, "expected exactly one warning, got {:?}", ws);
        assert!(
            ws[0].message.contains("non-exhaustive"),
            "msg: {}",
            ws[0].message
        );
        assert!(ws[0].message.contains("`Shape::Triangle`"), "msg: {}", ws[0].message);
    }

    #[test]
    fn guarded_arm_does_not_count_toward_coverage() {
        let src = r#"enum Light { Red, Green }
let l = Light::Red
let _ = match l {
    Light::Red if true => "stop",
    Light::Green => "go",
}"#;
        let ws = warnings(src);
        assert_eq!(ws.len(), 1, "expected a warning, got {:?}", ws);
        assert!(ws[0].message.contains("`Light::Red`"));
    }

    #[test]
    fn or_pattern_covers_multiple_variants() {
        let src = r#"enum E { A, B, C }
let e = E::A
let _ = match e {
    E::A | E::B => 1,
    E::C => 2,
}"#;
        assert!(warnings(src).is_empty());
    }

    #[test]
    fn heterogeneous_match_skips_check() {
        // A match that mixes a literal and an enum variant
        // can't be exhaustiveness-checked by this pass —
        // returning zero warnings is the correct pragmatic
        // answer.
        let src = r#"enum Tag { A, B }
let _ = match 1 {
    1 => "one",
    2 => "two",
}"#;
        // This happens to include no enum at all — still zero
        // warnings. The test locks the "no false positives on
        // literal matches" invariant.
        assert!(warnings(src).is_empty());
    }

    #[test]
    fn unknown_enum_bails_rather_than_warning() {
        // `FromAnotherModule` isn't declared here; the check
        // shouldn't warn (we can't verify coverage).
        let src = r#"fn handle(x) {
    return match x {
        FromAnotherModule::A => 1,
        FromAnotherModule::B => 2,
    }
}"#;
        assert!(warnings(src).is_empty());
    }

    #[test]
    fn warning_carries_match_line() {
        let src = r#"enum E { A, B }
let _ = match E::A {
    E::A => 1,
}"#;
        let ws = warnings(src);
        assert_eq!(ws.len(), 1);
        // The `match` keyword sits on line 2 of the source.
        assert_eq!(ws[0].line, Some(2));
    }

    #[test]
    fn match_inside_fn_body_is_checked() {
        let src = r#"enum E { A, B, C }
fn pick(e) {
    return match e {
        E::A => 1,
        E::B => 2,
    }
}"#;
        let ws = warnings(src);
        assert_eq!(ws.len(), 1);
        assert!(ws[0].message.contains("`E::C`"));
    }

    #[test]
    fn match_inside_if_branch_is_checked() {
        let src = r#"enum E { A, B, C }
let e = E::A
if true {
    let _ = match e {
        E::A => 1,
        E::B => 2,
    }
}"#;
        let ws = warnings(src);
        assert_eq!(ws.len(), 1);
        assert!(ws[0].message.contains("`E::C`"));
    }

    // ─── Cross-import exhaustiveness ──────────────────────────────

    /// Helper that wires a tiny `(&str, &str)` module map into
    /// the resolver closure shape.
    fn warnings_with_modules(
        source: &str,
        modules: &[(&str, &str)],
    ) -> Vec<BopWarning> {
        let stmts = parse(source).unwrap();
        let mut resolver = |name: &str| -> Option<Result<String, crate::error::BopError>> {
            modules
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, src)| Ok(String::from(*src)))
        };
        check_program_with_resolver(&stmts, &mut resolver)
    }

    #[test]
    fn imported_enum_missing_variant_warns_via_resolver() {
        // `use` brings `Shape` in from the `geom` module; the
        // match under-covers (`Triangle` unhandled), so the
        // checker — now that it follows the import — fires a
        // warning naming the missing variant.
        let ws = warnings_with_modules(
            r#"use geom
let s = Shape::Circle(5)
let _ = match s {
    Shape::Circle(r) => r,
    Shape::Square(s) => s,
}"#,
            &[("geom", "enum Shape { Circle(r), Square(s), Triangle }")],
        );
        assert_eq!(ws.len(), 1);
        assert!(
            ws[0].message.contains("`Shape::Triangle`"),
            "got: {}",
            ws[0].message
        );
    }

    #[test]
    fn imported_enum_exhaustive_match_produces_no_warning_via_resolver() {
        let ws = warnings_with_modules(
            r#"use geom
let s = Shape::Circle(5)
let _ = match s {
    Shape::Circle(r) => r,
    Shape::Square(s) => s,
    Shape::Triangle => 0,
}"#,
            &[("geom", "enum Shape { Circle(r), Square(s), Triangle }")],
        );
        assert!(
            ws.is_empty(),
            "expected no warnings when all variants covered, got: {:?}",
            ws
        );
    }

    #[test]
    fn transitive_imported_enum_is_picked_up() {
        // `a` re-exports via `use b`; the root's match over
        // the enum declared in `b` should still be checkable.
        let ws = warnings_with_modules(
            r#"use a
let c = Color::Red
let _ = match c {
    Color::Red => "r",
    Color::Blue => "b",
}"#,
            &[
                ("a", "use b"),
                ("b", "enum Color { Red, Blue, Green }"),
            ],
        );
        assert_eq!(ws.len(), 1);
        assert!(
            ws[0].message.contains("`Color::Green`"),
            "got: {}",
            ws[0].message
        );
    }

    #[test]
    fn unresolvable_module_is_silently_skipped() {
        // Resolver returns None → the checker treats the
        // enum as opaque and suppresses the warning rather
        // than raising a hard error. Matches the advisory
        // nature of `check_program`.
        let ws = warnings_with_modules(
            r#"use missing
let c = Color::Red
let _ = match c {
    Color::Red => 1,
}"#,
            &[],
        );
        assert!(ws.is_empty());
    }

    #[test]
    fn root_enum_shadows_imported_same_name() {
        // If the root program declares `Color` too, the
        // root's definition wins (first-write-wins). The
        // imported enum's variants don't leak into the
        // check.
        let ws = warnings_with_modules(
            r#"use paint
enum Color { Red, Blue }
let c = Color::Red
let _ = match c {
    Color::Red => 1,
    Color::Blue => 2,
}"#,
            &[("paint", "enum Color { Red, Green, Yellow }")],
        );
        assert!(
            ws.is_empty(),
            "expected no warning: root's Color is fully covered, got: {:?}",
            ws
        );
    }
}
