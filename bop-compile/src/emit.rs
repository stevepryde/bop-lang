//! AST → Rust source emitter.
//!
//! The generated file is one self-contained module. At the top it
//! pulls in `bop-lang` (`bop` crate) for the runtime surface and
//! declares a tiny set of helpers; then each user-defined Bop
//! function becomes a Rust fn, and the top-level program becomes
//! `run_program`. When `Options::emit_main` is set, a `main()`
//! drives everything through `bop_sys::StandardHost`.
//!
//! # Codegen shape
//!
//! - Each Bop expression lowers to a Rust expression of type
//!   [`bop::value::Value`]. Fallible expressions propagate with `?`.
//! - Composite expressions (ops, calls, collection literals) wrap
//!   their sub-expressions in a `{ let __a = ...; ... }` block so
//!   that sequential evaluation is explicit and the borrow checker
//!   is happy when sub-expressions both borrow `ctx`.
//! - Variables lower to Rust locals. `let x = ...` becomes
//!   `let mut x: Value = ...;` (always `mut` — we don't know if
//!   a later statement will reassign, and `#![allow(unused_mut)]`
//!   silences the warning).
//! - User functions take `&mut Ctx<'_>` plus their Bop parameters
//!   as `Value`. Recursion and nested fns work because Rust allows
//!   both fn-in-fn definitions and forward references within a
//!   block.
//!
//! Unsupported constructs (string interpolation, method calls,
//! indexed writes) return a `BopError::runtime` naming the feature
//! so the caller sees a clear "not yet supported" message instead of
//! broken Rust.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

use bop::error::BopError;
use bop::lexer::StringPart;
use bop::parser::{
    AssignOp, AssignTarget, BinOp, Expr, ExprKind, MatchArm, Stmt, StmtKind, UnaryOp, VariantKind,
    VariantPayload,
};

use crate::Options;

pub(crate) fn emit(stmts: &[Stmt], opts: &Options) -> Result<String, BopError> {
    // Pre-resolve every import in the program's transitive graph.
    // Failures here (missing resolver, module not found, cycle)
    // surface before any Rust is written.
    let modules = build_module_graph(stmts, opts)?;
    let info = collect_fn_info(stmts);
    let types = collect_type_registry(stmts, &modules)?;
    let mut emitter = Emitter::new(opts.clone(), info, modules, types);
    emitter.emit_program(stmts)?;
    Ok(emitter.finish())
}

// ─── Type / method registry ────────────────────────────────────────
//
// User-defined types and methods are collected across the root
// program and every transitively-imported module at pre-pass time,
// then flattened into a single registry. The AOT emits a single
// Rust definition per type, so two modules that declare types with
// the *same name and shape* fold together (matching the walker's
// idempotent re-import behaviour), while *different-shape* clashes
// surface as a transpile-time error pointing at both decl sites.
//
// Per-module scoping of types would require renaming every type
// reference in the emitted Rust — possible, but much more
// intrusive. Detecting clashes up front and erroring is the
// walker's behaviour anyway, so matching it here keeps the two
// engines consistent.

pub(crate) struct TypeRegistry {
    /// Struct types keyed by full identity `(module_path,
    /// type_name)`. Two modules that declare a struct with the
    /// same name coexist at distinct keys — no forced merge, no
    /// clash unless the same module genuinely declares it twice.
    pub structs: HashMap<(String, String), Vec<String>>,
    /// Enum types, same `(module_path, type_name)` keying.
    /// Variants are stored as bare-name → payload shape.
    pub enums: HashMap<(String, String), HashMap<String, VariantKind>>,
    /// User methods, keyed by the *full receiver-type identity*
    /// `(module_path, type_name, method_name)`. A method
    /// declared for `paint.Color` doesn't fire on
    /// `other.Color`; dispatch threads the receiver's own
    /// module path into the lookup.
    pub methods: HashMap<(String, String, String), MethodEntry>,
}

#[derive(Clone)]
pub(crate) struct MethodEntry {
    pub params: Vec<String>,
    pub body: Vec<Stmt>,
    /// Module prefix the method came from (empty for root). Only
    /// used to mangle the Rust fn name; doesn't affect dispatch.
    pub module_prefix: String,
}

/// Where a struct / enum was first declared — threaded through
/// the registry so clash errors can point at the *original*
/// declaration site. Path is the dot-joined module name, or
/// `<root>` for the top-level program.
#[derive(Clone)]
struct DeclOrigin {
    path: String,
    /// Line in that module's source. Matches the usual
    /// 1-indexed Bop line numbering.
    line: u32,
}

fn collect_type_registry(
    root: &[Stmt],
    modules: &ModuleGraph,
) -> Result<TypeRegistry, BopError> {
    let mut reg = TypeRegistry {
        structs: HashMap::new(),
        enums: HashMap::new(),
        methods: HashMap::new(),
    };
    // Parallel "where was this first declared?" tables so
    // clash diagnostics can name both sites. Keyed by full
    // identity.
    let mut struct_origins: HashMap<(String, String), DeclOrigin> = HashMap::new();
    let mut enum_origins: HashMap<(String, String), DeclOrigin> = HashMap::new();

    // Engine-wide builtins (`Result`, `RuntimeError`) go into
    // the registry before any user source is inspected, keyed
    // under `<builtin>` so a user-declared `enum Result { ... }`
    // at the program root lives under `<root>.Result` instead
    // and the two coexist without clashing.
    let builtin_mp = bop::value::BUILTIN_MODULE_PATH.to_string();
    reg.structs.insert(
        (builtin_mp.clone(), String::from("RuntimeError")),
        bop::builtins::builtin_runtime_error_fields(),
    );
    struct_origins.insert(
        (builtin_mp.clone(), String::from("RuntimeError")),
        DeclOrigin {
            path: "<builtin>".to_string(),
            line: 0,
        },
    );
    let result_variants: HashMap<String, VariantKind> = bop::builtins::builtin_result_variants()
        .into_iter()
        .map(|v| (v.name, v.kind))
        .collect();
    reg.enums.insert(
        (builtin_mp.clone(), String::from("Result")),
        result_variants,
    );
    enum_origins.insert(
        (builtin_mp.clone(), String::from("Result")),
        DeclOrigin {
            path: "<builtin>".to_string(),
            line: 0,
        },
    );

    // Root program contributes under `<root>`.
    collect_types_from_stmts(
        root,
        "",
        bop::value::ROOT_MODULE_PATH,
        &mut reg,
        &mut struct_origins,
        &mut enum_origins,
    )?;
    // Then every module's AST. Module prefix matches the emitter's
    // slug scheme (dots → underscores, trailing `__`). The `name`
    // string (pre-slug) is what shows up in error messages so users
    // can find the file they wrote, and is also the runtime
    // module path for type identity.
    for name in &modules.order {
        if let Some(entry) = modules.modules.get(name) {
            let prefix = format!("{}__", module_slug(name));
            collect_types_from_stmts(
                &entry.ast,
                &prefix,
                name,
                &mut reg,
                &mut struct_origins,
                &mut enum_origins,
            )?;
        }
    }
    Ok(reg)
}

/// Shape-equivalence check for two enum variant maps. Looser than
/// `PartialEq`: tuple variants compare by arity only (payload
/// field names are positional stubs with no runtime meaning),
/// struct variants still require matching field names. Same rule
/// the walker (`variants_equivalent` in `bop::evaluator`) and VM
/// (`shapes_match` in `bop_vm::vm`) use, so a program that
/// compiles under one engine compiles under all three.
fn enum_variants_equivalent(
    a: &HashMap<String, VariantKind>,
    b: &HashMap<String, VariantKind>,
) -> bool {
    if a.len() != b.len() {
        return false;
    }
    for (name, va) in a {
        let vb = match b.get(name) {
            Some(v) => v,
            None => return false,
        };
        match (va, vb) {
            (VariantKind::Unit, VariantKind::Unit) => {}
            (VariantKind::Tuple(fa), VariantKind::Tuple(fb)) => {
                if fa.len() != fb.len() {
                    return false;
                }
            }
            (VariantKind::Struct(fa), VariantKind::Struct(fb)) => {
                if fa != fb {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

fn collect_types_from_stmts(
    stmts: &[Stmt],
    prefix: &str,
    module_path: &str,
    reg: &mut TypeRegistry,
    struct_origins: &mut HashMap<(String, String), DeclOrigin>,
    enum_origins: &mut HashMap<(String, String), DeclOrigin>,
) -> Result<(), BopError> {
    for stmt in stmts {
        match &stmt.kind {
            StmtKind::StructDecl { name, fields } => {
                // Identity is `(module_path, name)` — two
                // modules declaring the same bare name now live
                // at distinct keys. Same-identity re-insertion
                // is a no-op on matching shape, an error on
                // mismatch (would mean the same module got
                // loaded twice with different source).
                let key = (module_path.to_string(), name.clone());
                if let Some(existing) = reg.structs.get(&key) {
                    if existing == fields {
                        continue;
                    }
                    let origin = struct_origins
                        .get(&key)
                        .cloned()
                        .unwrap_or(DeclOrigin {
                            path: "<unknown>".to_string(),
                            line: 0,
                        });
                    return Err(BopError::runtime(
                        format!(
                            "struct `{}` declared with different fields in `{}` (line {}) and `{}` (line {})",
                            name, origin.path, origin.line, module_path, stmt.line
                        ),
                        stmt.line,
                    ));
                }
                reg.structs.insert(key.clone(), fields.clone());
                struct_origins.insert(
                    key,
                    DeclOrigin {
                        path: module_path.to_string(),
                        line: stmt.line,
                    },
                );
            }
            StmtKind::EnumDecl { name, variants } => {
                let mut v_map = HashMap::new();
                for v in variants {
                    v_map.insert(v.name.clone(), v.kind.clone());
                }
                let key = (module_path.to_string(), name.clone());
                if let Some(existing) = reg.enums.get(&key) {
                    if enum_variants_equivalent(existing, &v_map) {
                        continue;
                    }
                    let origin = enum_origins
                        .get(&key)
                        .cloned()
                        .unwrap_or(DeclOrigin {
                            path: "<unknown>".to_string(),
                            line: 0,
                        });
                    return Err(BopError::runtime(
                        format!(
                            "enum `{}` declared with different variants in `{}` (line {}) and `{}` (line {})",
                            name, origin.path, origin.line, module_path, stmt.line
                        ),
                        stmt.line,
                    ));
                }
                reg.enums.insert(key.clone(), v_map);
                enum_origins.insert(
                    key,
                    DeclOrigin {
                        path: module_path.to_string(),
                        line: stmt.line,
                    },
                );
            }
            StmtKind::MethodDecl {
                type_name,
                method_name,
                params,
                body,
            } => {
                // Methods attach to the *full* receiver-type
                // identity. `fn Color.area(self)` inside paint
                // registers under `(paint, Color, area)` and
                // doesn't leak to other modules' same-named
                // types. Last-write-wins on same key to match
                // walker's insert-without-shape-check rule.
                reg.methods.insert(
                    (
                        module_path.to_string(),
                        type_name.clone(),
                        method_name.clone(),
                    ),
                    MethodEntry {
                        params: params.clone(),
                        body: body.clone(),
                        module_prefix: prefix.to_string(),
                    },
                );
            }
            // Type decls inside control-flow bodies are legal in
            // the walker, but the emitter treats them as top-level
            // for registry purposes. Nested fn decls get skipped —
            // their inner struct/enum decls (if any) aren't
            // reachable from outside the fn anyway.
            _ => {}
        }
    }
    Ok(())
}

// ─── Module graph ──────────────────────────────────────────────────

/// Transitively-resolved modules, keyed by dot-joined path and
/// ordered so each module comes after the ones it imports
/// (topological / leaves-first). Produced once per transpile and
/// handed to the emitter.
pub(crate) struct ModuleGraph {
    pub order: Vec<String>,
    pub modules: HashMap<String, ModuleEntry>,
}

#[derive(Clone)]
pub(crate) struct ModuleEntry {
    pub ast: Vec<Stmt>,
    pub own_fns: HashMap<String, Vec<String>>,
    /// Every name reachable from this module's final scope —
    /// its own `let`s and `fn`s plus, transitively, its imports'
    /// effective exports. Matches the walker's injection
    /// semantics (`use` re-exports by default).
    pub effective_exports: Vec<String>,
    /// Every *type* (struct / enum) name reachable through this
    /// module: its own declarations plus the types its imports
    /// re-export. Used when packing a `Value::Module` for aliased
    /// `use`, and when validating `use foo.{SomeType}` selective
    /// items that name types rather than bindings.
    pub effective_types: Vec<String>,
    // Kept during analysis for potential future use (e.g. more
    // precise `let` vs `fn` handling in exports packing), but not
    // currently read by the emitter.
    #[allow(dead_code)]
    pub own_lets: Vec<String>,
    #[allow(dead_code)]
    pub direct_imports: Vec<String>,
}

fn build_module_graph(
    root: &[Stmt],
    opts: &Options,
) -> Result<ModuleGraph, BopError> {
    let root_imports = collect_imports_in_stmts(root);
    if root_imports.is_empty() {
        return Ok(ModuleGraph {
            order: Vec::new(),
            modules: HashMap::new(),
        });
    }
    let resolver = match &opts.module_resolver {
        Some(r) => r.clone(),
        None => {
            return Err(BopError::runtime(
                "bop-compile: `use` requires `Options::module_resolver` to be set so the transpiler can inline the imported modules",
                root_imports.first().map(|(_, line)| *line).unwrap_or(0),
            ));
        }
    };

    let mut graph = ModuleGraph {
        order: Vec::new(),
        modules: HashMap::new(),
    };
    let mut visiting: BTreeSet<String> = BTreeSet::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    for (name, line) in &root_imports {
        visit_module(
            name,
            *line,
            &resolver,
            &mut graph,
            &mut visiting,
            &mut visited,
        )?;
    }
    Ok(graph)
}

fn visit_module(
    name: &str,
    line: u32,
    resolver: &crate::ModuleResolver,
    graph: &mut ModuleGraph,
    visiting: &mut BTreeSet<String>,
    visited: &mut BTreeSet<String>,
) -> Result<(), BopError> {
    if visiting.contains(name) {
        return Err(BopError::runtime(
            format!("Circular import: module `{}`", name),
            line,
        ));
    }
    if visited.contains(name) {
        return Ok(());
    }
    visiting.insert(name.to_string());

    // Resolve + parse.
    let source = {
        let mut r = resolver.borrow_mut();
        match r(name) {
            Some(Ok(s)) => s,
            Some(Err(e)) => return Err(e),
            None => {
                return Err(BopError::runtime(
                    format!("Module `{}` not found", name),
                    line,
                ));
            }
        }
    };
    let ast = bop::parse(&source)?;

    // Collect this module's direct imports and visit them first so
    // `effective_exports` is ready when we pack ours.
    let direct_imports: Vec<(String, u32)> = collect_imports_in_stmts(&ast);
    for (child_name, child_line) in &direct_imports {
        visit_module(
            child_name,
            *child_line,
            resolver,
            graph,
            visiting,
            visited,
        )?;
    }

    let own_lets = collect_top_level_lets(&ast);
    let own_fns = collect_top_level_fn_params(&ast);
    let own_types = collect_top_level_types(&ast);

    // effective_exports = own_lets + own_fn_names + (transitively,
    // every import's effective_exports). De-dup while preserving
    // declaration order.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut exports: Vec<String> = Vec::new();
    for (imp_name, _) in &direct_imports {
        if let Some(m) = graph.modules.get(imp_name) {
            for name in &m.effective_exports {
                if seen.insert(name.clone()) {
                    exports.push(name.clone());
                }
            }
        }
    }
    for name in &own_lets {
        if seen.insert(name.clone()) {
            exports.push(name.clone());
        }
    }
    for name in own_fns.keys() {
        if seen.insert(name.clone()) {
            exports.push(name.clone());
        }
    }

    // effective_types = own_types + transitively from imports.
    // Types live in the global registry for AOT, so this list only
    // drives `Value::Module.types` and selective-import validation.
    let mut seen_types: BTreeSet<String> = BTreeSet::new();
    let mut type_exports: Vec<String> = Vec::new();
    for (imp_name, _) in &direct_imports {
        if let Some(m) = graph.modules.get(imp_name) {
            for ty in &m.effective_types {
                if seen_types.insert(ty.clone()) {
                    type_exports.push(ty.clone());
                }
            }
        }
    }
    for ty in &own_types {
        if seen_types.insert(ty.clone()) {
            type_exports.push(ty.clone());
        }
    }

    graph.modules.insert(
        name.to_string(),
        ModuleEntry {
            ast,
            own_fns,
            own_lets,
            direct_imports: direct_imports.into_iter().map(|(n, _)| n).collect(),
            effective_exports: exports,
            effective_types: type_exports,
        },
    );
    graph.order.push(name.to_string());
    visiting.remove(name);
    visited.insert(name.to_string());
    Ok(())
}

fn collect_imports_in_stmts(stmts: &[Stmt]) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    for stmt in stmts {
        if let StmtKind::Use { path, items: _, alias: _ } = &stmt.kind {
            out.push((path.clone(), stmt.line));
        }
    }
    out
}

fn collect_top_level_lets(stmts: &[Stmt]) -> Vec<String> {
    stmts
        .iter()
        .filter_map(|s| match &s.kind {
            StmtKind::Let { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect()
}

/// Gather the names of struct / enum types declared at the top
/// level of `stmts`. Used to seed a module's `effective_types`
/// list so aliased `use foo as m` can populate `m.types` for
/// runtime namespace-validation of `m.Entity { ... }` etc.
fn collect_top_level_types(stmts: &[Stmt]) -> Vec<String> {
    stmts
        .iter()
        .filter_map(|s| match &s.kind {
            StmtKind::StructDecl { name, .. } => Some(name.clone()),
            StmtKind::EnumDecl { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect()
}

fn collect_top_level_fn_params(stmts: &[Stmt]) -> HashMap<String, Vec<String>> {
    let mut out = HashMap::new();
    for stmt in stmts {
        if let StmtKind::FnDecl { name, params, .. } = &stmt.kind {
            out.insert(name.clone(), params.clone());
        }
    }
    out
}

/// Result of the pre-pass over the AST. `all_fns` maps every
/// user-defined function name (top-level or nested) to its
/// parameter list so the emitter can decide dispatch + arity
/// for each call site. `top_level_fns` is the subset that's
/// reachable from outside its defining block and therefore
/// eligible to be turned into a first-class `Value::Fn` via an
/// emitted wrapper.
struct FnInfo {
    all_fns: HashMap<String, Vec<String>>,
    top_level_fns: HashSet<String>,
}

fn collect_fn_info(stmts: &[Stmt]) -> FnInfo {
    let mut all_fns = HashMap::new();
    let mut top_level_fns = HashSet::new();
    for stmt in stmts {
        if let StmtKind::FnDecl { name, params, body } = &stmt.kind {
            all_fns.insert(name.clone(), params.clone());
            top_level_fns.insert(name.clone());
            collect_nested_fns(body, &mut all_fns);
        } else {
            collect_nested_fns_in_stmt(stmt, &mut all_fns);
        }
    }
    FnInfo {
        all_fns,
        top_level_fns,
    }
}

fn collect_nested_fns(stmts: &[Stmt], all: &mut HashMap<String, Vec<String>>) {
    for stmt in stmts {
        collect_nested_fns_in_stmt(stmt, all);
    }
}

fn collect_nested_fns_in_stmt(stmt: &Stmt, all: &mut HashMap<String, Vec<String>>) {
    match &stmt.kind {
        StmtKind::FnDecl { name, params, body } => {
            all.insert(name.clone(), params.clone());
            collect_nested_fns(body, all);
        }
        StmtKind::If {
            body,
            else_ifs,
            else_body,
            ..
        } => {
            collect_nested_fns(body, all);
            for (_, b) in else_ifs {
                collect_nested_fns(b, all);
            }
            if let Some(b) = else_body {
                collect_nested_fns(b, all);
            }
        }
        StmtKind::While { body, .. } => collect_nested_fns(body, all),
        StmtKind::Repeat { body, .. } => collect_nested_fns(body, all),
        StmtKind::ForIn { body, .. } => collect_nested_fns(body, all),
        _ => {}
    }
}

// ─── Emitter ───────────────────────────────────────────────────────

struct Emitter {
    out: String,
    indent: usize,
    opts: Options,
    fn_info: FnInfo,
    modules: ModuleGraph,
    types: TypeRegistry,
    /// Counter for temporary locals (`__t0`, `__t1`, …). Reset at
    /// the start of each fn / top-level program so the names stay
    /// short.
    tmp_counter: usize,
    /// Non-empty while emitting an imported module's body — the
    /// prefix (e.g. `"foo__bar__"`) is applied to every user fn
    /// name so modules can't collide on function identifiers.
    module_prefix: String,
    /// Source-level path of the module whose body we're currently
    /// emitting. `<root>` while emitting the top-level program,
    /// the dot-joined `use` path while inside `emit_one_module`.
    /// Mirrors the walker / VM `current_module`: newly declared
    /// types tag their runtime values with this string, so two
    /// modules declaring the same name produce distinct
    /// identities.
    current_module: String,
    /// Per-scope map of bare type names → module path. Populated
    /// at emit time whenever a type is declared (→ current
    /// module) or brought into scope by a `use` statement
    /// (→ the imported module's path). Bare-name construction
    /// (`Color::Red`) and pattern matching resolve through this
    /// stack to produce the correct module path literal in the
    /// emitted Rust.
    type_bindings: Vec<HashMap<String, String>>,
    /// Module-alias map: `alias → module_path`. Populated at
    /// emit time by aliased `use` statements; consulted when
    /// a namespaced reference (`m.Color`) needs to be resolved
    /// to a module path for construction or pattern matching.
    module_aliases: HashMap<String, String>,
    /// Paths already imported at the current scope. Re-importing
    /// the same path in the same scope is a no-op — matches the
    /// walker's `imported_here` guard.
    imported_in_scope: HashSet<String>,
    /// Stack of `let`-bound Bop names visible at the current
    /// emission position. Used for:
    ///
    /// - Ident resolution (local vs top-level fn vs error).
    /// - Free-variable analysis for lambda capture.
    ///
    /// Each block (if / while / repeat / for / fn / lambda) pushes
    /// a fresh set on entry and pops on exit. User-fn and
    /// lambda parameters are inserted into the freshly-pushed set.
    scope_stack: Vec<HashSet<String>>,
    /// True while emitting the body of `run_program` (the Rust fn
    /// that returns `Result<(), BopError>`). User fns and lambdas
    /// toggle this off for the duration of their body — inside
    /// them, the enclosing Rust fn returns `Result<Value,
    /// BopError>`, so `return Ok(value)` is the Bop-level return
    /// path. Read by `try`'s codegen to decide whether an `Err`
    /// arm should propagate via `return Ok(...)` (fn body) or
    /// raise a real error (top-level program).
    in_top_level: bool,
}

impl Emitter {
    fn new(
        opts: Options,
        fn_info: FnInfo,
        modules: ModuleGraph,
        types: TypeRegistry,
    ) -> Self {
        // Seed the outermost type_bindings frame with the
        // engine-wide builtins so bare `Result::Ok(...)` and
        // `RuntimeError { ... }` resolve to `<builtin>` without
        // an explicit `use`. Same invariant the walker / VM
        // set up at construction time.
        let mut builtin_frame: HashMap<String, String> = HashMap::new();
        builtin_frame.insert(
            String::from("Result"),
            String::from(bop::value::BUILTIN_MODULE_PATH),
        );
        builtin_frame.insert(
            String::from("RuntimeError"),
            String::from(bop::value::BUILTIN_MODULE_PATH),
        );
        Self {
            out: String::new(),
            indent: 0,
            opts,
            fn_info,
            modules,
            types,
            tmp_counter: 0,
            module_prefix: String::new(),
            current_module: String::from(bop::value::ROOT_MODULE_PATH),
            type_bindings: vec![builtin_frame],
            module_aliases: HashMap::new(),
            imported_in_scope: HashSet::new(),
            scope_stack: Vec::new(),
            in_top_level: false,
        }
    }

    fn push_scope(&mut self) {
        self.scope_stack.push(HashSet::new());
        self.type_bindings.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scope_stack.pop();
        if self.type_bindings.len() > 1 {
            self.type_bindings.pop();
        }
    }

    /// Resolve a source-level type reference to its declaring
    /// module path, using the emitter's per-scope type_bindings
    /// + module_aliases state. Returns `None` if the name isn't
    /// in scope — callers treat that as a type-not-declared
    /// error at emit time.
    fn resolve_type_module(
        &self,
        namespace: Option<&str>,
        type_name: &str,
    ) -> Option<String> {
        if let Some(ns) = namespace {
            if let Some(mp) = self.module_aliases.get(ns) {
                // Verify the alias actually exports this type.
                if self.types.structs.contains_key(&(mp.clone(), type_name.to_string()))
                    || self.types.enums.contains_key(&(mp.clone(), type_name.to_string()))
                {
                    return Some(mp.clone());
                }
                return None;
            }
            return None;
        }
        for frame in self.type_bindings.iter().rev() {
            if let Some(mp) = frame.get(type_name) {
                return Some(mp.clone());
            }
        }
        None
    }

    /// Bind a bare type name in the current scope to the
    /// module that declared it. Called from StructDecl /
    /// EnumDecl emission and from the type-import arm of `use`
    /// statement handling.
    fn bind_type(&mut self, name: &str, module_path: &str) {
        if let Some(frame) = self.type_bindings.last_mut() {
            frame.insert(name.to_string(), module_path.to_string());
        }
    }

    /// Emit Rust source for a `__resolver` closure that turns
    /// `(namespace, type_name)` pairs into the declaring
    /// module's path, baked from the emitter's current
    /// `type_bindings` + `module_aliases` state. Inlined at
    /// every `pattern_matches` call site so the matcher can
    /// compare the value's full identity against what the
    /// source-level reference resolves to *at that point in
    /// the program*.
    fn emit_resolver_closure_src(&self) -> String {
        let mut bare_arms = String::new();
        // Flatten bare-name bindings inside-out so the
        // innermost shadow wins. A HashSet tracks which names
        // we've already emitted an arm for.
        let mut seen: HashSet<String> = HashSet::new();
        for frame in self.type_bindings.iter().rev() {
            let mut keys: Vec<&String> = frame.keys().collect();
            keys.sort();
            for name in keys {
                if !seen.insert(name.clone()) {
                    continue;
                }
                let mp = frame.get(name).unwrap();
                bare_arms.push_str(&format!(
                    "                        (::std::option::Option::None, {tn}) => ::std::option::Option::Some({mp}.to_string()),\n",
                    tn = rust_string_literal(name),
                    mp = rust_string_literal(mp),
                ));
            }
        }
        // Module aliases: emit one arm per (alias, type_name in
        // module). Cross-reference the TypeRegistry to enumerate
        // the types each alias's module actually exports.
        let mut alias_arms = String::new();
        let mut aliases: Vec<(&String, &String)> = self.module_aliases.iter().collect();
        aliases.sort();
        for (alias, mp) in aliases {
            let mut exported: Vec<String> = Vec::new();
            for (key, _) in &self.types.structs {
                if &key.0 == mp {
                    exported.push(key.1.clone());
                }
            }
            for (key, _) in &self.types.enums {
                if &key.0 == mp {
                    exported.push(key.1.clone());
                }
            }
            exported.sort();
            exported.dedup();
            for tn in exported {
                alias_arms.push_str(&format!(
                    "                        (::std::option::Option::Some({alias_lit}), {tn}) => ::std::option::Option::Some({mp}.to_string()),\n",
                    alias_lit = rust_string_literal(alias),
                    tn = rust_string_literal(&tn),
                    mp = rust_string_literal(mp),
                ));
            }
        }
        format!(
            "            let __resolver = |__ns: ::std::option::Option<&str>, __tn: &str| -> ::std::option::Option<String> {{\n\
             \x20                   match (__ns, __tn) {{\n\
             {bare}{alias}                        _ => ::std::option::Option::None,\n\
             \x20                   }}\n\
             \x20           }};\n",
            bare = bare_arms,
            alias = alias_arms,
        )
    }

    fn bind_local(&mut self, name: &str) {
        if let Some(top) = self.scope_stack.last_mut() {
            top.insert(name.to_string());
        }
    }

    fn is_local(&self, name: &str) -> bool {
        self.scope_stack.iter().rev().any(|s| s.contains(name))
    }

    fn rust_fn_name(&self, name: &str) -> String {
        rust_fn_name_with(&self.module_prefix, name)
    }

    fn wrapper_fn_name(&self, name: &str) -> String {
        wrapper_fn_name_with(&self.module_prefix, name)
    }

    fn finish(self) -> String {
        self.out
    }

    /// Pre-pass over a statement list to populate
    /// `type_bindings` + `module_aliases` from every top-level
    /// `use` statement *before* any fn body is emitted. The
    /// AOT lifts fn decls out of source order, so a naïve walk
    /// would try to emit `fn label(c) { match c { p.Color::Red
    /// => ... } }` before seeing `use paint as p` and the
    /// pattern resolver would come up empty.
    fn seed_uses(&mut self, stmts: &[Stmt]) {
        for stmt in stmts {
            if let StmtKind::Use { path, items, alias } = &stmt.kind {
                match (items, alias) {
                    (_, Some(a)) => {
                        self.module_aliases.insert(a.clone(), path.clone());
                    }
                    (Some(list), None) => {
                        // Selective imports introduce each listed
                        // type by bare name.
                        for name in list {
                            if self.is_type_in_module(path, name) {
                                self.bind_type(name, path);
                            }
                        }
                    }
                    (None, None) => {
                        // Glob: bring every public type across
                        // by bare name. Collect into a temp to
                        // avoid holding an immutable borrow of
                        // `self.types` across the `bind_type`
                        // mutation.
                        let mut glob_types: Vec<String> = Vec::new();
                        for (mp, tn) in self.types.structs.keys() {
                            if mp == path && !tn.starts_with('_') {
                                glob_types.push(tn.clone());
                            }
                        }
                        for (mp, tn) in self.types.enums.keys() {
                            if mp == path && !tn.starts_with('_') {
                                glob_types.push(tn.clone());
                            }
                        }
                        for tn in glob_types {
                            self.bind_type(&tn, path);
                        }
                    }
                }
            }
        }
    }

    fn is_type_in_module(&self, module_path: &str, type_name: &str) -> bool {
        let key = (module_path.to_string(), type_name.to_string());
        self.types.structs.contains_key(&key)
            || self.types.enums.contains_key(&key)
    }

    /// Pre-seed the current `type_bindings` frame with every
    /// type registered at `module_path`. Called before methods
    /// and top-level fns get emitted so their bodies can
    /// resolve bare `MyType { ... }` even though the AST walk
    /// hasn't reached the `struct MyType` decl yet.
    fn seed_types_for_module(&mut self, module_path: &str) {
        let mut names: Vec<String> = Vec::new();
        for (mp, tn) in self.types.structs.keys() {
            if mp == module_path {
                names.push(tn.clone());
            }
        }
        for (mp, tn) in self.types.enums.keys() {
            if mp == module_path {
                names.push(tn.clone());
            }
        }
        names.sort();
        names.dedup();
        for n in names {
            self.bind_type(&n, module_path);
        }
    }

    fn emit_program(&mut self, stmts: &[Stmt]) -> Result<(), BopError> {
        let module_name = self.opts.module_name.clone();
        if let Some(ref name) = module_name {
            writeln!(self.out, "pub mod {} {{", name).unwrap();
        }
        self.emit_header();
        self.emit_runtime_preamble();
        // Seed the outermost type_bindings frame with the
        // root program's declared types so methods + top-level
        // fns (emitted ahead of the main AST walk) can resolve
        // bare type names in their bodies. Same pre-pass for
        // `use` statements so aliases + selective-type imports
        // are visible to the fn bodies we're about to emit.
        self.seed_types_for_module(bop::value::ROOT_MODULE_PATH);
        self.seed_uses(stmts);
        // Imported modules emit first (topo-ordered — leaves
        // first) so their fns, exports, and load fns exist by
        // the time the root program's code references them.
        self.emit_imported_modules()?;
        // Top-level fn declarations move out of `run_program`'s
        // body to module scope. That way the `__bop_fn_value_*`
        // wrappers (also at module scope) can reference them, and
        // `let g = fib` works even before `fib` is called.
        self.emit_top_level_fn_decls(stmts)?;
        // User methods and the runtime dispatcher go at module
        // scope. Methods can live in different Bop modules; the
        // emitter mangles their Rust fn names by the source
        // module's slug so there's never a collision.
        self.emit_user_methods()?;
        self.emit_method_dispatcher();
        self.emit_run_program(stmts)?;
        self.emit_fn_value_wrappers();
        self.emit_public_entry();
        if self.opts.emit_main && module_name.is_none() {
            self.emit_main();
        }
        if module_name.is_some() {
            self.out.push_str("}\n");
        }
        Ok(())
    }

    /// Emit every module in the pre-resolved graph, in topo
    /// order. Each module gets: its user fn decls (prefixed by
    /// the module slug), its fn-value wrappers, its exports
    /// struct, and its load fn.
    fn emit_imported_modules(&mut self) -> Result<(), BopError> {
        let order = self.modules.order.clone();
        for name in &order {
            let entry = self
                .modules
                .modules
                .get(name)
                .cloned()
                .expect("module present in graph");
            self.emit_one_module(name, &entry)?;
        }
        Ok(())
    }

    fn emit_one_module(
        &mut self,
        name: &str,
        entry: &ModuleEntry,
    ) -> Result<(), BopError> {
        // Swap in this module's scope: prefix every user fn we
        // emit with the module slug, and replace `fn_info` with
        // the module's own fns so `is_local` / Ident / Call
        // resolution inside the module body behave correctly.
        // `current_module` also switches to the module's
        // source-level path so types declared in this module's
        // body tag their values correctly.
        let saved_prefix = std::mem::replace(
            &mut self.module_prefix,
            format!("{}__", module_slug(name)),
        );
        let saved_fn_info =
            std::mem::replace(&mut self.fn_info, collect_fn_info(&entry.ast));
        let saved_module = std::mem::replace(
            &mut self.current_module,
            name.to_string(),
        );
        // Fresh type_bindings frame for this module's body —
        // bare names declared here shouldn't leak back to the
        // caller / other modules. Seeded with the builtins the
        // emitter was already carrying so `Result` / `RuntimeError`
        // stay visible.
        let mut module_frame: HashMap<String, String> = HashMap::new();
        module_frame.insert(
            String::from("Result"),
            String::from(bop::value::BUILTIN_MODULE_PATH),
        );
        module_frame.insert(
            String::from("RuntimeError"),
            String::from(bop::value::BUILTIN_MODULE_PATH),
        );
        let saved_bindings =
            std::mem::replace(&mut self.type_bindings, vec![module_frame]);
        let saved_aliases =
            std::mem::take(&mut self.module_aliases);
        // Seed this module's types too — see
        // `seed_types_for_module` for why this matters. Methods
        // inside the module need to resolve bare type names
        // before the AST walker gets to the decls. Same treatment
        // for the module's own `use` statements.
        self.seed_types_for_module(name);
        self.seed_uses(&entry.ast);

        self.emit_top_level_fn_decls(&entry.ast)?;
        self.emit_fn_value_wrappers();
        self.emit_module_exports_struct(name, entry);
        self.emit_module_load_fn(name, entry)?;

        self.fn_info = saved_fn_info;
        self.module_prefix = saved_prefix;
        self.current_module = saved_module;
        self.type_bindings = saved_bindings;
        self.module_aliases = saved_aliases;
        Ok(())
    }

    fn emit_module_exports_struct(&mut self, name: &str, entry: &ModuleEntry) {
        writeln!(self.out, "#[derive(Clone)]").unwrap();
        writeln!(
            self.out,
            "struct {type_name} {{",
            type_name = module_exports_type_name(name)
        )
        .unwrap();
        for export in &entry.effective_exports {
            writeln!(
                self.out,
                "    {ident}: ::bop::value::Value,",
                ident = rust_ident(export)
            )
            .unwrap();
        }
        self.out.push_str("}\n\n");
    }

    /// Emit a `use` statement in one of four forms, loading the
    /// module once (via its `__mod_*__load` fn — which handles
    /// caching + cycle detection) and then either:
    ///
    /// - **Glob** (`use path`) — injects every public export as a
    ///   Rust local. Private names (`_`-prefixed) are filtered out.
    ///   Idempotent: re-running the same plain-glob in the same
    ///   scope is a no-op.
    /// - **Selective** (`use path.{a, b, Type}`) — injects only the
    ///   listed items as locals. Private names *are* reachable
    ///   through this form (the list is explicit, so a leading
    ///   `_` isn't meaningful here). Items may name bindings *or*
    ///   types: types are globally registered in the AOT, so they
    ///   don't need a Rust local — the emitter just validates the
    ///   name is actually exported.
    /// - **Aliased glob** (`use path as m`) — packs every public
    ///   export into a `Value::Module` and binds it under `m`.
    /// - **Aliased selective** (`use path.{a, b} as m`) — packs
    ///   only the listed items into a `Value::Module` and binds
    ///   it under `m`.
    ///
    /// Shadow conflicts on injected names are emitted as a
    /// transpile-time error for the explicit (selective + alias)
    /// forms and silently skipped for glob — matching the walker's
    /// "first-win, warn on conflict" behaviour.
    fn emit_use_stmt(
        &mut self,
        path: &str,
        items: &Option<Vec<String>>,
        alias: &Option<String>,
        line: u32,
    ) -> Result<(), BopError> {
        // Idempotency: only plain-glob re-imports are cached. The
        // other three forms can legitimately produce different
        // visible effects (different item subset, different alias
        // binding) when re-run in the same scope, so we always
        // re-emit them.
        if items.is_none() && alias.is_none() && self.imported_in_scope.contains(path) {
            return Ok(());
        }
        let entry = self
            .modules
            .modules
            .get(path)
            .cloned()
            .ok_or_else(|| {
                BopError::runtime(
                    format!("Module `{}` not found (bop-compile)", path),
                    line,
                )
            })?;

        // Load once, regardless of form — the exports struct is
        // what every form ultimately reads from.
        let tmp = self.fresh_tmp();
        self.line(&format!(
            "let {tmp} = {load}(ctx)?;",
            tmp = tmp,
            load = module_load_fn_name(path)
        ));

        // Selective form: validate every listed item is actually
        // reachable through this module (as a binding or a type).
        // Run this up-front so the error points at the use-site.
        if let Some(list) = items {
            for item in list {
                let is_binding = entry.effective_exports.iter().any(|e| e == item);
                let is_type = entry.effective_types.iter().any(|t| t == item);
                if !is_binding && !is_type {
                    return Err(BopError::runtime(
                        format!("Module `{}` has no export `{}`", path, item),
                        line,
                    ));
                }
            }
        }

        // Decide which names to expose + in what form.
        let expose_bindings: Vec<String> = match items {
            Some(list) => list
                .iter()
                .filter(|n| entry.effective_exports.iter().any(|e| e == *n))
                .cloned()
                .collect(),
            None => entry
                .effective_exports
                .iter()
                .filter(|n| !n.starts_with('_'))
                .cloned()
                .collect(),
        };
        let expose_types: Vec<String> = match items {
            Some(list) => list
                .iter()
                .filter(|n| entry.effective_types.iter().any(|t| t == *n))
                .cloned()
                .collect(),
            None => entry
                .effective_types
                .iter()
                .filter(|n| !n.starts_with('_'))
                .cloned()
                .collect(),
        };

        match alias {
            Some(alias_name) => {
                // Aliased: build a `Value::Module` and bind it
                // under the alias. All reads through the alias
                // (`m.helper`, `m.Entity { ... }`) resolve at
                // runtime by inspecting this value.
                let bindings_src: String = expose_bindings
                    .iter()
                    .map(|n| {
                        format!(
                            "({}.to_string(), {tmp}.{ident}.clone())",
                            rust_string_literal(n),
                            tmp = tmp,
                            ident = rust_ident(n)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let types_src: String = expose_types
                    .iter()
                    .map(|t| format!("{}.to_string()", rust_string_literal(t)))
                    .collect::<Vec<_>>()
                    .join(", ");
                if self.is_local(alias_name) {
                    return Err(BopError::runtime(
                        format!(
                            "Alias `{}` in `use {} as {}` would shadow an existing binding",
                            alias_name, path, alias_name
                        ),
                        line,
                    ));
                }
                self.line(&format!(
                    "let mut {alias}: ::bop::value::Value = ::bop::value::Value::Module(::std::rc::Rc::new(::bop::value::BopModule {{ path: {path_lit}.to_string(), bindings: ::std::vec![{bindings}], types: ::std::vec![{types}] }}));",
                    alias = rust_ident(alias_name),
                    path_lit = rust_string_literal(path),
                    bindings = bindings_src,
                    types = types_src,
                ));
                self.bind_local(alias_name);
                // Track the alias for compile-time resolution
                // of namespaced references (`m.Color`). Emit-
                // time resolution is sufficient here because
                // the AOT bakes module_path literals directly
                // into construction + match sites.
                self.module_aliases
                    .insert(alias_name.to_string(), path.to_string());
            }
            None => {
                // Non-aliased: inject each binding as a local.
                // Types don't need a Rust local (they're
                // compile-time metadata in the AOT), but they
                // do need a `type_bindings` entry so
                // construction + pattern sites can resolve
                // the bare name to the right module path.
                for name in &expose_bindings {
                    if self.is_local(name) {
                        if items.is_some() {
                            // Explicit: an explicit conflict is a
                            // hard error.
                            return Err(BopError::runtime(
                                format!(
                                    "Use of `{}` from `{}` would shadow an existing binding",
                                    name, path
                                ),
                                line,
                            ));
                        } else {
                            // Glob: first-win, skip silently to
                            // match the walker's warn-and-keep
                            // behaviour. (We don't surface the
                            // warning through AOT; the walker is
                            // the canonical source for that.)
                            continue;
                        }
                    }
                    self.line(&format!(
                        "let mut {ident}: ::bop::value::Value = {tmp}.{ident}.clone();",
                        ident = rust_ident(name),
                        tmp = tmp
                    ));
                    self.bind_local(name);
                }
                for tn in &expose_types {
                    let mp = path.to_string();
                    self.bind_type(tn, &mp);
                }
                if items.is_none() {
                    self.imported_in_scope.insert(path.to_string());
                }
            }
        }
        Ok(())
    }

    /// Emit the load fn for one module: checks the cache, inserts
    /// the `Loading` sentinel, emits the module body (imports +
    /// user statements), then packs every effective export into
    /// the module's exports struct and caches the result.
    fn emit_module_load_fn(
        &mut self,
        name: &str,
        entry: &ModuleEntry,
    ) -> Result<(), BopError> {
        let load = module_load_fn_name(name);
        let exports = module_exports_type_name(name);
        writeln!(
            self.out,
            "fn {load}(ctx: &mut Ctx<'_>) -> Result<{exports}, ::bop::error::BopError> {{",
            load = load,
            exports = exports,
        )
        .unwrap();
        self.indent = 1;
        self.tmp_counter = 0;

        // Cache lookup: hit a live exports? clone + return.
        // In progress? error with cycle message. Miss? mark
        // loading and fall through to evaluate the body.
        self.line(&format!(
            r#"if let Some(entry) = ctx.module_cache.get("{key}") {{"#,
            key = name
        ));
        self.line("    if entry.is::<__ModuleLoading>() {");
        self.line(&format!(
            "        return Err(::bop::error::BopError::runtime(\"Circular import: module `{key}`\", 0));",
            key = name
        ));
        self.line("    }");
        self.line(&format!(
            "    if let Some(loaded) = entry.downcast_ref::<{exports}>() {{",
            exports = exports
        ));
        self.line("        return Ok(loaded.clone());");
        self.line("    }");
        self.line("}");
        self.line(&format!(
            r#"ctx.module_cache.insert("{key}".to_string(), ::std::boxed::Box::new(__ModuleLoading));"#,
            key = name
        ));

        // Sandbox gets a tick at module entry too — same checkpoint
        // as any fn entry.
        self.emit_tick(0);

        // Body: emit the module's statements, skipping top-level
        // fn decls (already emitted) but handling imports /
        // lets / everything else. Track a fresh scope so the
        // emitter's Ident lookup resolves within the module.
        self.push_scope();
        for stmt in &entry.ast {
            if matches!(&stmt.kind, StmtKind::FnDecl { .. }) {
                continue;
            }
            self.emit_stmt(stmt)?;
        }

        // Pack every effective export. Top-level lets are Rust
        // locals at this point. Imports injected their names as
        // Rust locals too. Top-level fns need the wrapper to get
        // a `Value::Fn`.
        writeln!(self.out).unwrap();
        self.pad();
        writeln!(
            self.out,
            "let __exports = {exports} {{",
            exports = exports
        )
        .unwrap();
        for export in &entry.effective_exports {
            self.pad();
            if entry.own_fns.contains_key(export) {
                writeln!(
                    self.out,
                    "    {ident}: {wrapper}(),",
                    ident = rust_ident(export),
                    wrapper = self.wrapper_fn_name(export)
                )
                .unwrap();
            } else {
                writeln!(
                    self.out,
                    "    {ident}: {ident}.clone(),",
                    ident = rust_ident(export)
                )
                .unwrap();
            }
        }
        self.pad();
        self.out.push_str("};\n");
        self.pad();
        writeln!(
            self.out,
            r#"ctx.module_cache.insert("{key}".to_string(), ::std::boxed::Box::new(__exports.clone()));"#,
            key = name
        )
        .unwrap();
        self.pad();
        self.out.push_str("Ok(__exports)\n");
        self.pop_scope();
        self.indent = 0;
        self.out.push_str("}\n\n");
        Ok(())
    }

    /// Emit every top-level `fn name(...) { ... }` as a module-scope
    /// Rust fn. Called before `run_program`; the top-level decls are
    /// subsequently skipped inside `emit_run_program` so they don't
    /// emit twice.
    fn emit_top_level_fn_decls(&mut self, stmts: &[Stmt]) -> Result<(), BopError> {
        for stmt in stmts {
            if let StmtKind::FnDecl { name, params, body } = &stmt.kind {
                self.emit_fn_decl(name, params, body, stmt.line)?;
            }
        }
        Ok(())
    }

    /// For each top-level user fn, emit a helper that constructs
    /// a `Value::Fn` wrapping a Rust closure that forwards into
    /// the real Rust fn. Lets `let g = foo; g(5)` work end-to-end
    /// by giving us a runtime handle on a named fn. Nested fns
    /// aren't wrapped — they're only visible inside their outer
    /// fn's Rust scope.
    fn emit_fn_value_wrappers(&mut self) {
        // Sort for deterministic output.
        let mut names: Vec<_> = self.fn_info.top_level_fns.iter().cloned().collect();
        names.sort();
        for name in names {
            let params = match self.fn_info.all_fns.get(&name) {
                Some(p) => p.clone(),
                None => continue,
            };
            let rust_fn = self.rust_fn_name(&name);
            let arity = params.len();
            let params_list = params
                .iter()
                .map(|p| format!("\"{}\".to_string()", p))
                .collect::<Vec<_>>()
                .join(", ");

            writeln!(
                self.out,
                "fn {wrapper}() -> ::bop::value::Value {{",
                wrapper = self.wrapper_fn_name(&name)
            )
            .unwrap();
            writeln!(
                self.out,
                "    let callable: ::std::rc::Rc<dyn for<'__a> Fn(&mut Ctx<'__a>, ::std::vec::Vec<::bop::value::Value>) -> Result<::bop::value::Value, ::bop::error::BopError>> = ::std::rc::Rc::new(move |ctx, mut args| {{"
            )
            .unwrap();
            writeln!(
                self.out,
                "        if args.len() != {arity} {{ return Err(::bop::error::BopError::runtime(format!(\"`{name}` expects {arity} argument{s}, but got {{}}\", args.len()), 0)); }}",
                arity = arity,
                name = name,
                s = if arity == 1 { "" } else { "s" }
            )
            .unwrap();
            // Move args into positional locals in declaration order.
            for i in 0..arity {
                writeln!(
                    self.out,
                    "        let __a{i} = args.remove(0);",
                    i = i
                )
                .unwrap();
            }
            let call_args = (0..arity)
                .map(|i| format!("__a{}", i))
                .collect::<Vec<_>>()
                .join(", ");
            if arity == 0 {
                writeln!(self.out, "        {}(ctx)", rust_fn).unwrap();
            } else {
                writeln!(
                    self.out,
                    "        {}(ctx, {})",
                    rust_fn, call_args
                )
                .unwrap();
            }
            writeln!(self.out, "    }});").unwrap();
            writeln!(
                self.out,
                "    __bop_wrap_callable(vec![{params}], ::std::vec::Vec::new(), Some(\"{name}\".to_string()), callable)",
                params = params_list,
                name = name
            )
            .unwrap();
            writeln!(self.out, "}}\n").unwrap();
        }
    }

    /// Emit each user-defined method as a Rust fn with a mangled
    /// name, so module prefix + receiver type + method name don't
    /// collide.
    fn emit_user_methods(&mut self) -> Result<(), BopError> {
        // Sort for deterministic output.
        let mut entries: Vec<((String, String, String), MethodEntry)> = self
            .types
            .methods
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for ((_module_path, type_name, method_name), entry) in &entries {
            let saved_prefix = std::mem::replace(
                &mut self.module_prefix,
                format!("{}method_{}__", entry.module_prefix, type_name),
            );
            let saved_fn_info = std::mem::replace(
                &mut self.fn_info,
                collect_fn_info(&entry.body),
            );
            self.emit_fn_decl(
                method_name,
                &entry.params,
                &entry.body,
                0,
            )?;
            self.fn_info = saved_fn_info;
            self.module_prefix = saved_prefix;
        }
        Ok(())
    }

    /// Emit a runtime dispatcher that maps `(type_name,
    /// method_name)` pairs to their compiled user-method Rust fns.
    /// The method-call emitter calls this first; on `None` it
    /// falls back to built-in method dispatch.
    fn emit_method_dispatcher(&mut self) {
        self.out.push_str(
            "\nfn __bop_try_user_method(\n",
        );
        self.out.push_str(
            "    ctx: &mut Ctx<'_>,\n    obj: &::bop::value::Value,\n    method: &str,\n    args: &[::bop::value::Value],\n    line: u32,\n) -> Result<::std::option::Option<::bop::value::Value>, ::bop::error::BopError> {\n",
        );
        self.out.push_str(
            "    let type_key: ::std::option::Option<(&str, &str)> = match obj {\n",
        );
        self.out.push_str(
            "        ::bop::value::Value::Struct(s) => Some((s.module_path(), s.type_name())),\n",
        );
        self.out.push_str(
            "        ::bop::value::Value::EnumVariant(e) => Some((e.module_path(), e.type_name())),\n",
        );
        self.out.push_str("        _ => None,\n    };\n");
        self.out.push_str(
            "    let (type_mp, type_name) = match type_key { Some(k) => k, None => return Ok(None) };\n",
        );
        self.out.push_str("    match (type_mp, type_name, method) {\n");

        let mut entries: Vec<((String, String, String), MethodEntry)> = self
            .types
            .methods
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        for ((module_path, type_name, method_name), entry) in &entries {
            let fn_name = format!(
                "bop_fn_{}method_{}__{}",
                entry.module_prefix, type_name, method_name
            );
            let arity = entry.params.len();
            let arity_minus_one = arity.saturating_sub(1);
            let arity_check = format!(
                "if args.len() != {expected} {{ return Err(::bop::error::BopError::runtime(format!(\"`{type_name}.{method_name}` expects {arity} argument{s} (including `self`), but got {{}}\", args.len() + 1), line)); }}",
                expected = arity_minus_one,
                type_name = type_name,
                method_name = method_name,
                arity = arity,
                s = if arity == 1 { "" } else { "s" },
            );
            let args_pass: String = (0..arity_minus_one)
                .map(|i| format!(", args[{}].clone()", i))
                .collect::<Vec<_>>()
                .join("");
            writeln!(
                self.out,
                "        ({mp_lit}, {type_lit}, {method_lit}) => {{ {arity_check} Ok(Some({fn_name}(ctx, obj.clone(){args_pass})?)) }}",
                mp_lit = rust_string_literal(module_path),
                type_lit = rust_string_literal(type_name),
                method_lit = rust_string_literal(method_name),
                arity_check = arity_check,
                fn_name = fn_name,
                args_pass = args_pass,
            )
            .unwrap();
        }

        self.out.push_str("        _ => Ok(None),\n    }\n}\n\n");
    }

    // ─── Preamble / footer ────────────────────────────────────────

    fn emit_header(&mut self) {
        self.out.push_str(HEADER);
    }

    /// Emit the runtime preamble by stitching together the
    /// variant-specific `Ctx` struct, the shared helpers, and
    /// — when sandbox mode is on — the `__bop_tick` step /
    /// memory / on_tick checkpoint fn. Keeping the helpers in
    /// one place means every new runtime helper lands once;
    /// prior to this refactor two near-identical preamble
    /// strings drifted every time a new helper was added.
    fn emit_runtime_preamble(&mut self) {
        self.out.push_str(RUNTIME_HEADER);
        self.out.push_str(if self.opts.sandbox {
            CTX_SANDBOX
        } else {
            CTX_BASE
        });
        self.out.push_str(RUNTIME_SHARED);
        if self.opts.sandbox {
            self.out.push_str(TICK_HELPER);
        }
    }

    fn emit_public_entry(&mut self) {
        if self.opts.sandbox {
            self.out.push_str(PUBLIC_ENTRY_SANDBOX);
        } else {
            self.out.push_str(PUBLIC_ENTRY);
        }
    }

    fn emit_main(&mut self) {
        if self.opts.sandbox {
            self.out.push_str(MAIN_FN_SANDBOX);
        } else {
            self.out.push_str(MAIN_FN);
        }
    }

    fn emit_run_program(&mut self, stmts: &[Stmt]) -> Result<(), BopError> {
        writeln!(self.out, "fn run_program(ctx: &mut Ctx<'_>) -> Result<(), ::bop::error::BopError> {{").unwrap();
        self.indent = 1;
        self.tmp_counter = 0;
        // Mark the body as top-level so `try` lowers `Err` to a
        // real runtime error rather than an impossible
        // `return Ok(value)` (run_program returns `()`).
        let saved_top_level = self.in_top_level;
        self.in_top_level = true;
        self.emit_tick(0);
        self.push_scope();
        // Top-level fn decls were already emitted at module scope;
        // skip them here. The scope_stack deliberately stays empty
        // for fn names — they're resolved through `fn_info`
        // (top_level_fns / all_fns), not via `is_local`.
        for stmt in stmts {
            if matches!(&stmt.kind, StmtKind::FnDecl { .. }) {
                continue;
            }
            self.emit_stmt(stmt)?;
        }
        self.pop_scope();
        self.line("Ok(())");
        self.indent = 0;
        self.out.push_str("}\n\n");
        self.in_top_level = saved_top_level;
        Ok(())
    }

    /// Emit a `__bop_tick` call at the current indent. No-op when
    /// sandbox mode is off, so call sites don't need their own
    /// gate.
    fn emit_tick(&mut self, line: u32) {
        if self.opts.sandbox {
            self.line(&format!("__bop_tick(ctx, {})?;", line));
        }
    }

    // ─── Indentation helpers ──────────────────────────────────────

    fn pad(&mut self) {
        for _ in 0..self.indent {
            self.out.push_str("    ");
        }
    }

    fn line(&mut self, s: &str) {
        self.pad();
        self.out.push_str(s);
        self.out.push('\n');
    }

    fn open_block(&mut self, header: &str) {
        self.pad();
        self.out.push_str(header);
        self.out.push_str(" {\n");
        self.indent += 1;
    }

    fn close_block(&mut self) {
        self.indent -= 1;
        self.pad();
        self.out.push_str("}\n");
    }

    fn fresh_tmp(&mut self) -> String {
        let t = format!("__t{}", self.tmp_counter);
        self.tmp_counter += 1;
        t
    }

    // ─── Statements ───────────────────────────────────────────────

    fn emit_stmt(&mut self, stmt: &Stmt) -> Result<(), BopError> {
        let line = stmt.line;
        match &stmt.kind {
            StmtKind::Let { name, value, is_const: _ } => {
                let rhs = self.expr_src(value)?;
                let ident = rust_ident(name);
                self.line(&format!("let mut {}: ::bop::value::Value = {};", ident, rhs));
                self.bind_local(name);
            }

            StmtKind::Assign { target, op, value } => {
                self.emit_assign(target, op, value, line)?;
            }

            StmtKind::If {
                condition,
                body,
                else_ifs,
                else_body,
            } => {
                self.emit_if_statement(condition, body, else_ifs, else_body)?;
            }

            StmtKind::While { condition, body } => {
                let cond_src = self.expr_src(condition)?;
                self.open_block(&format!("while ({}).is_truthy()", cond_src));
                self.emit_tick(line);
                self.push_scope();
                for s in body {
                    self.emit_stmt(s)?;
                }
                self.pop_scope();
                self.close_block();
            }

            StmtKind::Repeat { count, body } => {
                let count_src = self.expr_src(count)?;
                let count_tmp = self.fresh_tmp();
                let n_tmp = self.fresh_tmp();
                self.line(&format!("let {} = {};", count_tmp, count_src));
                self.open_block(&format!("let {}: i64 = match {}", n_tmp, count_tmp));
                self.line("::bop::value::Value::Int(n) => n,");
                self.line("::bop::value::Value::Number(n) => n as i64,");
                self.line(&format!(
                    "other => return Err(::bop::error::BopError::runtime(format!(\"repeat needs a number, but got {{}}\", other.type_name()), {})),",
                    line
                ));
                self.indent -= 1;
                self.pad();
                self.out.push_str("};\n");
                self.open_block(&format!("for _ in 0..({}.max(0))", n_tmp));
                self.emit_tick(line);
                self.push_scope();
                for s in body {
                    self.emit_stmt(s)?;
                }
                self.pop_scope();
                self.close_block();
            }

            StmtKind::ForIn {
                var,
                iterable,
                body,
            } => {
                let iter_src = self.expr_src(iterable)?;
                let items_tmp = self.fresh_tmp();
                self.line(&format!(
                    "let {}: ::std::vec::Vec<::bop::value::Value> = __bop_iter_items({}, {})?;",
                    items_tmp, iter_src, line
                ));
                let ident = rust_ident(var);
                self.open_block(&format!("for {} in {}", ident, items_tmp));
                self.emit_tick(line);
                // Mirror the tree-walker: the loop variable is a
                // fresh binding in each iteration. Re-bind as mut so
                // the body can reassign it.
                self.line(&format!(
                    "let mut {}: ::bop::value::Value = {};",
                    ident, ident
                ));
                self.push_scope();
                self.bind_local(var);
                for s in body {
                    self.emit_stmt(s)?;
                }
                self.pop_scope();
                self.close_block();
            }

            StmtKind::FnDecl { name, params, body } => {
                self.emit_fn_decl(name, params, body, line)?;
            }

            StmtKind::Return { value } => {
                match value {
                    Some(v) => {
                        let src = self.expr_src(v)?;
                        self.line(&format!("return Ok({});", src));
                    }
                    None => {
                        self.line("return Ok(::bop::value::Value::None);");
                    }
                }
            }

            StmtKind::Break => self.line("break;"),
            StmtKind::Continue => self.line("continue;"),

            StmtKind::Use { path, items, alias } => {
                self.emit_use_stmt(path, items, alias, line)?;
            }

            StmtKind::StructDecl { name, .. }
            | StmtKind::EnumDecl { name, .. } => {
                // Type declarations are compile-time only in the
                // AOT. The pre-pass collected them into
                // `self.types` keyed by full identity; here we
                // just record the bare name → module path
                // mapping in the current scope so bare
                // construction / pattern sites further down
                // resolve correctly.
                let mp = self.current_module.clone();
                self.bind_type(name, &mp);
                let _ = line;
            }
            StmtKind::MethodDecl { .. } => {
                // Methods are compile-time only too — the
                // pre-pass registered them by full receiver
                // identity. Nothing to emit at the decl site.
                let _ = line;
            }

            StmtKind::ExprStmt(expr) => {
                let src = self.expr_src(expr)?;
                self.line(&format!("let _ = {};", src));
            }
        }
        Ok(())
    }

    fn emit_if_statement(
        &mut self,
        cond: &Expr,
        body: &[Stmt],
        else_ifs: &[(Expr, Vec<Stmt>)],
        else_body: &Option<Vec<Stmt>>,
    ) -> Result<(), BopError> {
        let cond_src = self.expr_src(cond)?;
        self.open_block(&format!("if ({}).is_truthy()", cond_src));
        self.push_scope();
        for s in body {
            self.emit_stmt(s)?;
        }
        self.pop_scope();
        self.indent -= 1;
        for (elif_cond, elif_body) in else_ifs {
            let c = self.expr_src(elif_cond)?;
            self.pad();
            self.out.push_str(&format!("}} else if ({}).is_truthy() {{\n", c));
            self.indent += 1;
            self.push_scope();
            for s in elif_body {
                self.emit_stmt(s)?;
            }
            self.pop_scope();
            self.indent -= 1;
        }
        if let Some(else_body) = else_body {
            self.pad();
            self.out.push_str("} else {\n");
            self.indent += 1;
            self.push_scope();
            for s in else_body {
                self.emit_stmt(s)?;
            }
            self.pop_scope();
            self.indent -= 1;
        }
        self.pad();
        self.out.push_str("}\n");
        Ok(())
    }

    fn emit_assign(
        &mut self,
        target: &AssignTarget,
        op: &AssignOp,
        value: &Expr,
        line: u32,
    ) -> Result<(), BopError> {
        match target {
            AssignTarget::Variable(name) => {
                let ident = rust_ident(name);
                let rhs_src = self.expr_src(value)?;
                match op {
                    AssignOp::Eq => {
                        self.line(&format!("{} = {};", ident, rhs_src));
                    }
                    compound => {
                        let op_path = compound_op_path(*compound);
                        let rhs_tmp = self.fresh_tmp();
                        self.line(&format!("let {} = {};", rhs_tmp, rhs_src));
                        self.line(&format!(
                            "{} = {}(&{}, &{}, {})?;",
                            ident, op_path, ident, rhs_tmp, line
                        ));
                    }
                }
                Ok(())
            }
            AssignTarget::Index { object, index } => {
                // Tree-walker requires the object to be a bare ident;
                // anything else is a compile-time error here too.
                let target = match &object.kind {
                    ExprKind::Ident(n) => rust_ident(n),
                    _ => {
                        return Err(BopError::runtime(
                            "Can only assign to indexed variables (like `arr[0] = val`)",
                            line,
                        ));
                    }
                };
                // Eval order mirrors the tree-walker: rhs value,
                // then index, then (for compound) the current
                // indexed value, then apply the op, then write back.
                let val_src = self.expr_src(value)?;
                let idx_src = self.expr_src(index)?;
                let val_tmp = self.fresh_tmp();
                let idx_tmp = self.fresh_tmp();
                self.line(&format!("let {} = {};", val_tmp, val_src));
                self.line(&format!("let {} = {};", idx_tmp, idx_src));
                match op {
                    AssignOp::Eq => {
                        self.line(&format!(
                            "::bop::ops::index_set(&mut {}, &{}, {}, {})?;",
                            target, idx_tmp, val_tmp, line
                        ));
                    }
                    compound => {
                        let op_path = compound_op_path(*compound);
                        let cur_tmp = self.fresh_tmp();
                        let new_tmp = self.fresh_tmp();
                        self.line(&format!(
                            "let {} = ::bop::ops::index_get(&{}, &{}, {})?;",
                            cur_tmp, target, idx_tmp, line
                        ));
                        self.line(&format!(
                            "let {} = {}(&{}, &{}, {})?;",
                            new_tmp, op_path, cur_tmp, val_tmp, line
                        ));
                        self.line(&format!(
                            "::bop::ops::index_set(&mut {}, &{}, {}, {})?;",
                            target, idx_tmp, new_tmp, line
                        ));
                    }
                }
                Ok(())
            }
            AssignTarget::Field { object, field } => {
                let target = match &object.kind {
                    ExprKind::Ident(n) => rust_ident(n),
                    _ => {
                        return Err(BopError::runtime(
                            "Can only assign to fields of named variables (like `p.x = val`)",
                            line,
                        ));
                    }
                };
                let val_src = self.expr_src(value)?;
                let val_tmp = self.fresh_tmp();
                self.line(&format!("let {} = {};", val_tmp, val_src));
                match op {
                    AssignOp::Eq => {
                        self.line(&format!(
                            "{} = __bop_field_set({}, {}, {}, {})?;",
                            target,
                            target,
                            rust_string_literal(field),
                            val_tmp,
                            line
                        ));
                    }
                    compound => {
                        let op_path = compound_op_path(*compound);
                        let cur_tmp = self.fresh_tmp();
                        let new_tmp = self.fresh_tmp();
                        self.line(&format!(
                            "let {} = __bop_field_get(&{}, {}, {})?;",
                            cur_tmp,
                            target,
                            rust_string_literal(field),
                            line
                        ));
                        self.line(&format!(
                            "let {} = {}(&{}, &{}, {})?;",
                            new_tmp, op_path, cur_tmp, val_tmp, line
                        ));
                        self.line(&format!(
                            "{} = __bop_field_set({}, {}, {}, {})?;",
                            target,
                            target,
                            rust_string_literal(field),
                            new_tmp,
                            line
                        ));
                    }
                }
                Ok(())
            }
        }
    }

    fn emit_fn_decl(
        &mut self,
        name: &str,
        params: &[String],
        body: &[Stmt],
        line: u32,
    ) -> Result<(), BopError> {
        let fn_name = self.rust_fn_name(name);
        let param_list = params
            .iter()
            .map(|p| format!("mut {}: ::bop::value::Value", rust_ident(p)))
            .collect::<Vec<_>>()
            .join(", ");
        let sig = if params.is_empty() {
            format!(
                "fn {}(ctx: &mut Ctx<'_>) -> Result<::bop::value::Value, ::bop::error::BopError>",
                fn_name
            )
        } else {
            format!(
                "fn {}(ctx: &mut Ctx<'_>, {}) -> Result<::bop::value::Value, ::bop::error::BopError>",
                fn_name, param_list
            )
        };
        self.open_block(&sig);
        let saved_tmp = self.tmp_counter;
        self.tmp_counter = 0;
        // Inside a user fn: the enclosing Rust fn returns
        // `Result<Value, BopError>`, so a Bop-level return from
        // `try` on `Err` is `return Ok(err_value)` rather than a
        // real error.
        let saved_top_level = self.in_top_level;
        self.in_top_level = false;
        // Function-entry checkpoint — matches the plan's "step-count
        // checks at loop backedges / function entry". No-op outside
        // sandbox mode.
        self.emit_tick(line);
        // Fresh scope with the params bound; Rust-level fn scope
        // isolates outer locals anyway, but scope tracking is the
        // source of truth for lambda-capture analysis.
        self.push_scope();
        for p in params {
            self.bind_local(p);
        }
        for s in body {
            self.emit_stmt(s)?;
        }
        self.pop_scope();
        // Implicit `return none` if control falls off the end. The
        // `allow(unreachable_code)` at the top of the file silences
        // the warning for bodies that always return explicitly.
        self.line("Ok(::bop::value::Value::None)");
        self.tmp_counter = saved_tmp;
        self.in_top_level = saved_top_level;
        self.close_block();
        Ok(())
    }

    // ─── Expressions ──────────────────────────────────────────────

    /// Render `expr` as a Rust expression of type
    /// `::bop::value::Value`. Fallible sub-expressions propagate via
    /// `?`, so the result must be used inside a function returning
    /// `Result<_, BopError>`.
    fn expr_src(&mut self, expr: &Expr) -> Result<String, BopError> {
        let line = expr.line;
        let s = match &expr.kind {
            ExprKind::Int(n) => format!("::bop::value::Value::Int({}i64)", n),
            ExprKind::Number(n) => format!("::bop::value::Value::Number({}f64)", rust_f64(*n)),
            ExprKind::Str(s) => format!(
                "::bop::value::Value::new_str({}.to_string())",
                rust_string_literal(s)
            ),
            ExprKind::Bool(b) => {
                format!("::bop::value::Value::Bool({})", b)
            }
            ExprKind::None => "::bop::value::Value::None".to_string(),

            ExprKind::Ident(name) => {
                if self.is_local(name) {
                    format!("{}.clone()", rust_ident(name))
                } else if self.fn_info.top_level_fns.contains(name) {
                    // Top-level fn used as a value — hand back the
                    // wrapper that reifies the Rust fn as a
                    // `Value::Fn`.
                    format!("{}()", self.wrapper_fn_name(name))
                } else if self.fn_info.all_fns.contains_key(name) {
                    // A nested fn isn't reachable as a value from
                    // outside its outer fn's Rust scope — document
                    // and bail explicitly rather than emit broken
                    // Rust.
                    return Err(BopError::runtime(
                        format!(
                            "bop-compile: nested function `{}` can't be used as a first-class value (only top-level fns are currently wrappable)",
                            name
                        ),
                        line,
                    ));
                } else {
                    // Fall through: treat as a local binding and
                    // let rustc flag it if the name doesn't
                    // actually resolve. Matches the existing
                    // "undefined at compile time" behaviour for
                    // plain `print(nope)` and the like.
                    format!("{}.clone()", rust_ident(name))
                }
            }

            ExprKind::StringInterp(parts) => self.string_interp_src(parts, line)?,

            ExprKind::FieldAccess { object, field } => {
                let obj_src = self.expr_src(object)?;
                let obj_tmp = self.fresh_tmp();
                format!(
                    "{{ let {tmp} = {obj}; __bop_field_get(&{tmp}, {field_lit}, {line})? }}",
                    tmp = obj_tmp,
                    obj = obj_src,
                    field_lit = rust_string_literal(field),
                    line = line,
                )
            }

            ExprKind::StructConstruct { namespace, type_name, fields } => {
                self.struct_construct_src(namespace.as_deref(), type_name, fields, line)?
            }

            ExprKind::EnumConstruct {
                namespace,
                type_name,
                variant,
                payload,
            } => self.enum_construct_src(
                namespace.as_deref(),
                type_name,
                variant,
                payload,
                line,
            )?,

            ExprKind::Lambda { params, body } => self.lambda_src(params, body, line)?,

            ExprKind::BinaryOp { left, op, right } => self.binary_src(left, *op, right, line)?,

            ExprKind::UnaryOp { op, expr: inner } => {
                let inner_src = self.expr_src(inner)?;
                match op {
                    UnaryOp::Neg => format!(
                        "{{ let __v = {}; ::bop::ops::neg(&__v, {})? }}",
                        inner_src, line
                    ),
                    UnaryOp::Not => {
                        format!("{{ let __v = {}; ::bop::ops::not(&__v) }}", inner_src)
                    }
                }
            }

            ExprKind::Call { callee, args } => self.call_src(callee, args, line)?,

            ExprKind::MethodCall {
                object,
                method,
                args,
            } => self.method_call_src(object, method, args, line)?,

            ExprKind::Index { object, index } => {
                let obj_src = self.expr_src(object)?;
                let idx_src = self.expr_src(index)?;
                format!(
                    "{{ let __o = {}; let __i = {}; ::bop::ops::index_get(&__o, &__i, {})? }}",
                    obj_src, idx_src, line
                )
            }

            ExprKind::Array(items) => self.array_src(items)?,

            ExprKind::Dict(entries) => self.dict_src(entries)?,

            ExprKind::IfExpr {
                condition,
                then_expr,
                else_expr,
            } => {
                let cond = self.expr_src(condition)?;
                let then_s = self.expr_src(then_expr)?;
                let else_s = self.expr_src(else_expr)?;
                format!(
                    "(if ({}).is_truthy() {{ {} }} else {{ {} }})",
                    cond, then_s, else_s
                )
            }

            ExprKind::Match { scrutinee, arms } => self.match_src(scrutinee, arms, line)?,

            ExprKind::Try(inner) => self.try_src(inner, line)?,
        };
        Ok(s)
    }

    /// Lower `try <expr>` to a Rust block expression. The shape
    /// matches the walker / VM: inspect the variant name, unwrap
    /// `Ok`, short-circuit on `Err`, raise on anything else.
    ///
    /// `Err` short-circuits differently depending on whether
    /// we're inside a user fn (in which case the enclosing Rust
    /// fn returns `Result<Value, BopError>` and `return Ok(v)`
    /// propagates the Err variant as that fn's result) or
    /// top-level `run_program` (which returns `Result<(),
    /// BopError>` — no way to thread a value through, so we raise
    /// a real runtime error instead, matching the walker's
    /// "try at top-level" rule).
    fn try_src(&mut self, inner: &Expr, line: u32) -> Result<String, BopError> {
        let inner_src = self.expr_src(inner)?;
        let id = self.tmp_counter;
        self.tmp_counter += 1;
        let v_name = format!("__try_v_{}", id);
        let err_arm = if self.in_top_level {
            format!(
                "return ::std::result::Result::Err(::bop::error::BopError::runtime(\"try encountered Err at top-level\", {}));",
                line
            )
        } else {
            // Inside a user fn: a Bop-level `return err` is
            // spelled `return Ok(err_value)` at the Rust level.
            format!("return ::std::result::Result::Ok({}.clone());", v_name)
        };
        // We construct a block that:
        //   1. evaluates the scrutinee once into a local;
        //   2. inspects its variant;
        //   3. either yields the unwrapped value, returns early,
        //      or raises.
        //
        // The block's type is `::bop::value::Value` — the `Ok`
        // arm's unwrapped payload, or one of two `!`-typed
        // returns. `#[allow(unreachable_code)]` guards the
        // pattern where the Rust compiler can prove the block
        // always returns (e.g. a bare `try` at the end of a fn).
        Ok(format!(
            "{{\n    \
             let {v}: ::bop::value::Value = {inner};\n    \
             match &{v} {{\n        \
                 ::bop::value::Value::EnumVariant(ev) if ev.variant() == \"Ok\" => {{\n            \
                     match ev.payload() {{\n                \
                         ::bop::value::EnumPayload::Tuple(items) if items.len() == 1 => items[0].clone(),\n                \
                         ::bop::value::EnumPayload::Unit => ::bop::value::Value::None,\n                \
                         ::bop::value::EnumPayload::Tuple(items) => {{\n                    \
                             return ::std::result::Result::Err(::bop::error::BopError::runtime(format!(\"try: Ok variant must carry exactly one value, got {{}}\", items.len()), {line}));\n                \
                         }}\n                \
                         ::bop::value::EnumPayload::Struct(_) => {{\n                    \
                             return ::std::result::Result::Err(::bop::error::BopError::runtime(\"try: Ok variant must carry a single positional value, not named fields\", {line}));\n                \
                         }}\n            \
                     }}\n        \
                 }}\n        \
                 ::bop::value::Value::EnumVariant(ev) if ev.variant() == \"Err\" => {{\n            \
                     {err_arm}\n        \
                 }}\n        \
                 other => {{\n            \
                     return ::std::result::Result::Err(::bop::error::BopError::runtime(format!(\"try expected a Result-shaped value (Ok/Err variant), got {{}}\", other.type_name()), {line}));\n        \
                 }}\n    \
             }}\n\
             }}",
            v = v_name,
            inner = inner_src,
            err_arm = err_arm,
            line = line,
        ))
    }

    /// Lower a `match` expression to a Rust block expression.
    ///
    /// Emitted shape (trimmed for clarity):
    ///
    /// ```text
    /// {
    ///   let __match_sc_N: Value = <scrutinee>;
    ///   'match_arms_N: loop {
    ///     {
    ///       let __pat: Pattern = <pattern ctor>;
    ///       let mut __bindings = Vec::new();
    ///       if bop::pattern_matches(&__pat, &__match_sc_N, &mut __bindings) {
    ///         let <b1>: Value = __bindings.iter().rev().find(...)...;
    ///         // ...
    ///         if (<guard>).is_truthy() {
    ///           break 'match_arms_N <body>;
    ///         }
    ///       }
    ///     }
    ///     // ... next arm ...
    ///     return Err(BopError::runtime("No match arm matched the scrutinee", line));
    ///   }
    /// }
    /// ```
    ///
    /// The pattern itself is constructed as a `bop::parser::Pattern`
    /// value in the emitted Rust and fed through the shared
    /// `bop::pattern_matches` helper so walker, VM, and AOT all
    /// apply identical matching rules. Each arm pushes a fresh
    /// emitter scope so binding names are visible as Rust locals
    /// inside `expr_src` calls for guard and body.
    fn match_src(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        line: u32,
    ) -> Result<String, BopError> {
        let scrutinee_src = self.expr_src(scrutinee)?;
        let id = self.tmp_counter;
        self.tmp_counter += 1;
        let sc_name = format!("__match_sc_{}", id);
        let label = format!("match_arms_{}", id);

        let mut src = String::new();
        src.push_str("{\n");
        src.push_str(&format!(
            "    let {}: ::bop::value::Value = {};\n",
            sc_name, scrutinee_src
        ));
        src.push_str(&format!("    '{}: loop {{\n", label));

        for arm in arms {
            let arm_line = arm.line;
            let pat_src = pattern_rust(&arm.pattern);
            // Collect every name this arm's pattern binds. We sort
            // for deterministic emission and register them as
            // locals on the emitter's scope stack so `expr_src`
            // treats them as locals (not free captures) inside the
            // guard and body.
            let mut names_set: HashSet<String> = HashSet::new();
            collect_pattern_bindings(&arm.pattern, &mut names_set);
            let mut names: Vec<String> = names_set.into_iter().collect();
            names.sort();

            src.push_str("        {\n");
            src.push_str(&format!(
                "            let __pat: ::bop::parser::Pattern = {};\n",
                pat_src
            ));
            src.push_str("            let mut __bindings: ::std::vec::Vec<(::std::string::String, ::bop::value::Value)> = ::std::vec::Vec::new();\n");
            // Emit a per-site resolver closure that encodes the
            // emit-time view of type_bindings + module_aliases.
            // Patterns reaching this point use the same lexical
            // scope we're in *right now*, so statically baking
            // the mapping is both correct and efficient.
            src.push_str(&self.emit_resolver_closure_src());
            src.push_str(&format!(
                "            if ::bop::pattern_matches(&__pat, &{}, &mut __bindings, &__resolver) {{\n",
                sc_name
            ));

            self.push_scope();
            for name in &names {
                self.bind_local(name);
                src.push_str(&format!(
                    "                let {}: ::bop::value::Value = __bindings.iter().rev().find(|(k, _)| k == {}).map(|(_, v)| v.clone()).unwrap_or(::bop::value::Value::None);\n",
                    rust_ident(name),
                    rust_string_literal(name)
                ));
            }

            // Rust warns if a binding name isn't used in the body
            // — a common case for wildcard-ish matches where the
            // programmer bound a name for clarity but didn't need
            // it. Prefix the lets with `#[allow(unused_variables)]`
            // scoped blocks? Simpler: emit `let _ = <name>;` to
            // silence the lint without changing semantics.
            for name in &names {
                src.push_str(&format!(
                    "                let _ = &{};\n",
                    rust_ident(name)
                ));
            }

            if let Some(guard) = &arm.guard {
                let guard_src = self.expr_src(guard)?;
                src.push_str(&format!(
                    "                if ({}).is_truthy() {{\n",
                    guard_src
                ));
                let body_src = self.expr_src(&arm.body)?;
                src.push_str(&format!(
                    "                    break '{} {};\n",
                    label, body_src
                ));
                src.push_str("                }\n");
            } else {
                let body_src = self.expr_src(&arm.body)?;
                src.push_str(&format!(
                    "                break '{} {};\n",
                    label, body_src
                ));
            }
            self.pop_scope();

            src.push_str("            }\n");
            src.push_str("        }\n");

            // Keep the per-arm line variable even if unused for
            // now — ready for future diagnostics that want to
            // point at a specific arm.
            let _ = arm_line;
        }

        src.push_str(&format!(
            "        #[allow(unreachable_code)]\n        return ::std::result::Result::Err(::bop::error::BopError::runtime(\"No match arm matched the scrutinee\", {}));\n",
            line
        ));
        src.push_str("    }\n");
        src.push_str("}");

        Ok(src)
    }

    fn binary_src(
        &mut self,
        left: &Expr,
        op: BinOp,
        right: &Expr,
        line: u32,
    ) -> Result<String, BopError> {
        match op {
            BinOp::And => {
                let l = self.expr_src(left)?;
                let r = self.expr_src(right)?;
                Ok(format!(
                    "{{ let __l = {}; if __l.is_truthy() {{ ::bop::value::Value::Bool(({}).is_truthy()) }} else {{ ::bop::value::Value::Bool(false) }} }}",
                    l, r
                ))
            }
            BinOp::Or => {
                let l = self.expr_src(left)?;
                let r = self.expr_src(right)?;
                Ok(format!(
                    "{{ let __l = {}; if __l.is_truthy() {{ ::bop::value::Value::Bool(true) }} else {{ ::bop::value::Value::Bool(({}).is_truthy()) }} }}",
                    l, r
                ))
            }
            _ => {
                let l = self.expr_src(left)?;
                let r = self.expr_src(right)?;
                let op_path = bin_op_path(op);
                // Eq / NotEq are infallible; ops::eq / ops::not_eq
                // return Value directly. Everything else is
                // Result<Value, BopError>. Emit the `?` only where
                // needed.
                let needs_try = !matches!(op, BinOp::Eq | BinOp::NotEq);
                let suffix_line = if matches!(op, BinOp::Eq | BinOp::NotEq) {
                    String::new()
                } else {
                    format!(", {}", line)
                };
                let trailing = if needs_try { "?" } else { "" };
                Ok(format!(
                    "{{ let __l = {}; let __r = {}; {}(&__l, &__r{}){} }}",
                    l, r, op_path, suffix_line, trailing
                ))
            }
        }
    }

    fn call_src(&mut self, callee: &Expr, args: &[Expr], line: u32) -> Result<String, BopError> {
        // Non-Ident callees go through the value-call path:
        // evaluate the callee onto the stack, then dispatch via
        // `__bop_call_value`. Captures `funcs[0](x)`,
        // `make_adder(5)(3)`, `(if cond { f } else { g })(x)`, etc.
        let name = match &callee.kind {
            ExprKind::Ident(n) => n.clone(),
            _ => {
                let callee_src = self.expr_src(callee)?;
                let callee_tmp = self.fresh_tmp();
                let mut arg_lets = format!("let {} = {}; ", callee_tmp, callee_src);
                let mut arg_names = Vec::with_capacity(args.len());
                for arg in args {
                    let src = self.expr_src(arg)?;
                    let tmp = self.fresh_tmp();
                    write!(arg_lets, "let {} = {}; ", tmp, src).unwrap();
                    arg_names.push(tmp);
                }
                let args_vec = if arg_names.is_empty() {
                    "::std::vec::Vec::<::bop::value::Value>::new()".to_string()
                } else {
                    format!("vec![{}]", arg_names.join(", "))
                };
                return Ok(format!(
                    "{{ {}__bop_call_value(ctx, {}, {}, {})? }}",
                    arg_lets, callee_tmp, args_vec, line
                ));
            }
        };

        // A locally-bound Ident (e.g. `let f = fn() {...}; f(x)`)
        // becomes a value call. Matches the walker / VM rule that
        // local shadowing wins over builtin / host / named-fn.
        if self.is_local(&name) {
            let mut arg_lets = String::new();
            let mut arg_names = Vec::with_capacity(args.len());
            for arg in args {
                let src = self.expr_src(arg)?;
                let tmp = self.fresh_tmp();
                write!(arg_lets, "let {} = {}; ", tmp, src).unwrap();
                arg_names.push(tmp);
            }
            let args_vec = if arg_names.is_empty() {
                "::std::vec::Vec::<::bop::value::Value>::new()".to_string()
            } else {
                format!("vec![{}]", arg_names.join(", "))
            };
            return Ok(format!(
                "{{ {}__bop_call_value(ctx, {}.clone(), {}, {})? }}",
                arg_lets,
                rust_ident(&name),
                args_vec,
                line
            ));
        }

        // Evaluate args into locals up-front so the resulting block
        // has a predictable evaluation order and doesn't reborrow
        // `ctx` inside nested sub-expressions.
        let mut arg_names = Vec::with_capacity(args.len());
        let mut arg_lets = String::new();
        for arg in args {
            let src = self.expr_src(arg)?;
            let tmp = self.fresh_tmp();
            write!(arg_lets, "let {} = {}; ", tmp, src).unwrap();
            arg_names.push(tmp);
        }

        let body = match name.as_str() {
            "print" => {
                let args_expr = build_arg_array(&arg_names);
                format!(
                    "ctx.host.on_print(&__bop_format_print(&{})); ::bop::value::Value::None",
                    args_expr
                )
            }
            "range" => format!(
                "::bop::builtins::builtin_range(&{}, {}, &mut ctx.rand_state)?",
                build_arg_array(&arg_names),
                line
            ),
            "rand" => format!(
                "::bop::builtins::builtin_rand(&{}, {}, &mut ctx.rand_state)?",
                build_arg_array(&arg_names),
                line
            ),
            "try_call" => {
                // `try_call(f)` takes a single callable and
                // dispatches through the preamble's
                // `__bop_try_call`. We validate the arity at
                // runtime for parity with walker / VM (which
                // both raise there too).
                if arg_names.len() != 1 {
                    format!(
                        "return Err(::bop::error::BopError::runtime(format!(\"`try_call` expects 1 argument, but got {{}}\", {}usize), {}))",
                        arg_names.len(),
                        line,
                    )
                } else {
                    format!(
                        "__bop_try_call(ctx, {}, {})?",
                        arg_names[0],
                        line,
                    )
                }
            }
            _ => {
                // Build a separate cloned-args slice for host.call
                // so we can still pass the originals to the user-fn
                // fallback without a double-clone. When there's no
                // user-fn fallback, the originals just drop at end
                // of block.
                let cloned_args = if arg_names.is_empty() {
                    "[]".to_string()
                } else {
                    format!(
                        "[{}]",
                        arg_names
                            .iter()
                            .map(|n| format!("{}.clone()", n))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                if self.fn_info.all_fns.contains_key(&name) {
                    let fn_name = self.rust_fn_name(&name);
                    let fn_args = if arg_names.is_empty() {
                        "ctx".to_string()
                    } else {
                        format!("ctx, {}", arg_names.join(", "))
                    };
                    format!(
                        "match ctx.host.call({:?}, &{}, {}) {{ Some(r) => r?, None => {}({})?, }}",
                        name, cloned_args, line, fn_name, fn_args
                    )
                } else {
                    // No fn of that name in scope. Still try the
                    // host first so embedders (e.g. bop-sys's
                    // readline / file / env builtins) keep working.
                    // Route the generated error through
                    // `bop::error_messages::function_not_found` so
                    // the text stays in lockstep with walker / VM
                    // without any per-engine string duplication.
                    let hint_fallback = format!(
                        "{{ let hint = ctx.host.function_hint(); if hint.is_empty() {{ ::bop::error::BopError::runtime(::bop::error_messages::function_not_found({:?}), {}) }} else {{ let mut e = ::bop::error::BopError::runtime(::bop::error_messages::function_not_found({:?}), {}); e.friendly_hint = Some(hint.to_string()); e }} }}",
                        name, line, name, line
                    );
                    format!(
                        "match ctx.host.call({:?}, &{}, {}) {{ Some(r) => r?, None => return Err({}), }}",
                        name, cloned_args, line, hint_fallback
                    )
                }
            }
        };

        Ok(format!("{{ {}{} }}", arg_lets, body))
    }

    fn array_src(&mut self, items: &[Expr]) -> Result<String, BopError> {
        if items.is_empty() {
            return Ok("::bop::value::Value::new_array(::std::vec::Vec::new())".to_string());
        }
        let mut lets = String::new();
        let mut names = Vec::with_capacity(items.len());
        for item in items {
            let src = self.expr_src(item)?;
            let tmp = self.fresh_tmp();
            write!(lets, "let {} = {}; ", tmp, src).unwrap();
            names.push(tmp);
        }
        Ok(format!(
            "{{ {}::bop::value::Value::new_array(vec![{}]) }}",
            lets,
            names.join(", ")
        ))
    }

    fn dict_src(&mut self, entries: &[(String, Expr)]) -> Result<String, BopError> {
        if entries.is_empty() {
            return Ok(
                "::bop::value::Value::new_dict(::std::vec::Vec::new())".to_string(),
            );
        }
        let mut lets = String::new();
        let mut pairs = Vec::with_capacity(entries.len());
        for (key, value) in entries {
            let src = self.expr_src(value)?;
            let tmp = self.fresh_tmp();
            write!(lets, "let {} = {}; ", tmp, src).unwrap();
            pairs.push(format!("({}.to_string(), {})", rust_string_literal(key), tmp));
        }
        Ok(format!(
            "{{ {}::bop::value::Value::new_dict(vec![{}]) }}",
            lets,
            pairs.join(", ")
        ))
    }

    fn struct_construct_src(
        &mut self,
        namespace: Option<&str>,
        type_name: &str,
        fields: &[(String, Expr)],
        line: u32,
    ) -> Result<String, BopError> {
        // Resolve the source-level type reference to its full
        // identity `(module_path, type_name)`. This is what
        // drives both the compile-time shape lookup *and* the
        // module_path literal emitted into the Value::new_struct
        // call, so two modules declaring `Color` with different
        // fields end up producing distinct runtime types.
        let module_path = self
            .resolve_type_module(namespace, type_name)
            .ok_or_else(|| {
                BopError::runtime(
                    bop::error_messages::struct_not_declared(type_name),
                    line,
                )
            })?;
        let key = (module_path.clone(), type_name.to_string());
        let decl = self.types.structs.get(&key).cloned().ok_or_else(|| {
            BopError::runtime(
                bop::error_messages::struct_not_declared(type_name),
                line,
            )
        })?;
        // Namespaced path (`m.Entity { ... }`) — still validate
        // at runtime that `m` is in fact a Module exporting the
        // type, so a shadowed-value surface error (`let m = 3`
        // after `use foo as m`) surfaces with a clear runtime
        // message instead of compiling fine but panicking.
        let ns_check = match namespace {
            Some(ns) => format!(
                "__bop_validate_namespace_type(&{ns_ident}, {ns_lit}, {ty_lit}, {line})?; ",
                ns_ident = rust_ident(ns),
                ns_lit = rust_string_literal(ns),
                ty_lit = rust_string_literal(type_name),
                line = line,
            ),
            None => String::new(),
        };
        // Compile-time validation: set matches exactly, no dups.
        let mut seen = HashSet::new();
        for (fname, _) in fields {
            if !seen.insert(fname.clone()) {
                return Err(BopError::runtime(
                    format!(
                        "Field `{}` specified twice in `{}` construction",
                        fname, type_name
                    ),
                    line,
                ));
            }
            if !decl.iter().any(|d| d == fname) {
                let mut err = BopError::runtime(
                    bop::error_messages::struct_has_no_field(type_name, fname),
                    line,
                );
                if let Some(hint) = bop::suggest::did_you_mean(
                    fname,
                    decl.iter().map(|s| s.as_str()),
                ) {
                    err.friendly_hint = Some(hint);
                }
                return Err(err);
            }
        }
        for declared in &decl {
            if !seen.contains(declared) {
                return Err(BopError::runtime(
                    format!(
                        "Missing field `{}` in `{}` construction",
                        declared, type_name
                    ),
                    line,
                ));
            }
        }
        // Emit field bindings in provided order so any side
        // effects in sub-expressions happen source-order, then
        // assemble the fields vec in declaration order.
        let mut lets = String::new();
        let mut provided_tmps: HashMap<String, String> = HashMap::new();
        for (fname, fexpr) in fields {
            let src = self.expr_src(fexpr)?;
            let tmp = self.fresh_tmp();
            write!(lets, "let {} = {}; ", tmp, src).unwrap();
            provided_tmps.insert(fname.clone(), tmp);
        }
        let ordered: Vec<String> = decl
            .iter()
            .map(|name| {
                let tmp = provided_tmps.remove(name).unwrap();
                format!("({}.to_string(), {})", rust_string_literal(name), tmp)
            })
            .collect();
        Ok(format!(
            "{{ {ns_check}{lets}::bop::value::Value::new_struct({mp}.to_string(), {tn}.to_string(), vec![{fields}]) }}",
            ns_check = ns_check,
            lets = lets,
            mp = rust_string_literal(&module_path),
            tn = rust_string_literal(type_name),
            fields = ordered.join(", ")
        ))
    }

    fn enum_construct_src(
        &mut self,
        namespace: Option<&str>,
        type_name: &str,
        variant: &str,
        payload: &VariantPayload,
        line: u32,
    ) -> Result<String, BopError> {
        // Resolve the source-level reference to its declaring
        // module so the resulting `Value::EnumVariant` is tagged
        // with the right identity. Two modules declaring the
        // same enum shape produce distinct values.
        let module_path = self
            .resolve_type_module(namespace, type_name)
            .ok_or_else(|| {
                BopError::runtime(
                    bop::error_messages::enum_not_declared(type_name),
                    line,
                )
            })?;
        let key = (module_path.clone(), type_name.to_string());
        let variants = self
            .types
            .enums
            .get(&key)
            .cloned()
            .ok_or_else(|| {
                BopError::runtime(
                    bop::error_messages::enum_not_declared(type_name),
                    line,
                )
            })?;
        let declared = variants.get(variant).cloned().ok_or_else(|| {
            let mut err = BopError::runtime(
                bop::error_messages::enum_has_no_variant(type_name, variant),
                line,
            );
            if let Some(hint) = bop::suggest::did_you_mean(
                variant,
                variants.keys().map(|s| s.as_str()),
            ) {
                err.friendly_hint = Some(hint);
            }
            err
        })?;
        let tn_lit = rust_string_literal(type_name);
        let vn_lit = rust_string_literal(variant);
        // Namespaced path (`m.Result::Ok(v)`) — validate at runtime
        // that `m` is a Module actually exporting this type. The
        // variant itself resolves through the global type registry
        // same as a bare construct, so the check is purely a guard
        // matching walker + VM semantics.
        let ns_check = match namespace {
            Some(ns) => format!(
                "__bop_validate_namespace_type(&{ns_ident}, {ns_lit}, {ty_lit}, {line})?; ",
                ns_ident = rust_ident(ns),
                ns_lit = rust_string_literal(ns),
                ty_lit = rust_string_literal(type_name),
                line = line,
            ),
            None => String::new(),
        };
        let mp_lit = rust_string_literal(&module_path);
        match (&declared, payload) {
            (VariantKind::Unit, VariantPayload::Unit) => Ok(format!(
                "{{ {ns_check}::bop::value::Value::new_enum_unit({mp}.to_string(), {tn}.to_string(), {vn}.to_string()) }}",
                ns_check = ns_check,
                mp = mp_lit,
                tn = tn_lit,
                vn = vn_lit
            )),
            (VariantKind::Tuple(decl_fields), VariantPayload::Tuple(args)) => {
                if decl_fields.len() != args.len() {
                    return Err(BopError::runtime(
                        format!(
                            "`{}::{}` expects {} argument{}, but got {}",
                            type_name,
                            variant,
                            decl_fields.len(),
                            if decl_fields.len() == 1 { "" } else { "s" },
                            args.len()
                        ),
                        line,
                    ));
                }
                let mut lets = String::new();
                let mut names = Vec::with_capacity(args.len());
                for a in args {
                    let src = self.expr_src(a)?;
                    let tmp = self.fresh_tmp();
                    write!(lets, "let {} = {}; ", tmp, src).unwrap();
                    names.push(tmp);
                }
                Ok(format!(
                    "{{ {ns_check}{lets}::bop::value::Value::new_enum_tuple({mp}.to_string(), {tn}.to_string(), {vn}.to_string(), vec![{items}]) }}",
                    ns_check = ns_check,
                    lets = lets,
                    mp = mp_lit,
                    tn = tn_lit,
                    vn = vn_lit,
                    items = names.join(", ")
                ))
            }
            (VariantKind::Struct(decl_fields), VariantPayload::Struct(provided)) => {
                let mut seen = HashSet::new();
                for (fname, _) in provided {
                    if !seen.insert(fname.clone()) {
                        return Err(BopError::runtime(
                            format!(
                                "Field `{}` specified twice in `{}::{}`",
                                fname, type_name, variant
                            ),
                            line,
                        ));
                    }
                    if !decl_fields.iter().any(|d| d == fname) {
                        return Err(BopError::runtime(
                            format!(
                                "Variant `{}::{}` has no field `{}`",
                                type_name, variant, fname
                            ),
                            line,
                        ));
                    }
                }
                for declared in decl_fields {
                    if !seen.contains(declared) {
                        return Err(BopError::runtime(
                            format!(
                                "Missing field `{}` in `{}::{}` construction",
                                declared, type_name, variant
                            ),
                            line,
                        ));
                    }
                }
                let mut lets = String::new();
                let mut provided_tmps: HashMap<String, String> = HashMap::new();
                for (fname, fexpr) in provided {
                    let src = self.expr_src(fexpr)?;
                    let tmp = self.fresh_tmp();
                    write!(lets, "let {} = {}; ", tmp, src).unwrap();
                    provided_tmps.insert(fname.clone(), tmp);
                }
                let ordered: Vec<String> = decl_fields
                    .iter()
                    .map(|name| {
                        let tmp = provided_tmps.remove(name).unwrap();
                        format!(
                            "({}.to_string(), {})",
                            rust_string_literal(name),
                            tmp
                        )
                    })
                    .collect();
                Ok(format!(
                    "{{ {ns_check}{lets}::bop::value::Value::new_enum_struct({mp}.to_string(), {tn}.to_string(), {vn}.to_string(), vec![{items}]) }}",
                    ns_check = ns_check,
                    lets = lets,
                    mp = mp_lit,
                    tn = tn_lit,
                    vn = vn_lit,
                    items = ordered.join(", ")
                ))
            }
            (VariantKind::Unit, _) => Err(BopError::runtime(
                format!("Variant `{}::{}` takes no payload", type_name, variant),
                line,
            )),
            (VariantKind::Tuple(_), _) => Err(BopError::runtime(
                format!(
                    "Variant `{}::{}` expects positional arguments `(…)`",
                    type_name, variant
                ),
                line,
            )),
            (VariantKind::Struct(_), _) => Err(BopError::runtime(
                format!(
                    "Variant `{}::{}` expects named fields `{{ … }}`",
                    type_name, variant
                ),
                line,
            )),
        }
    }

    fn string_interp_src(
        &mut self,
        parts: &[StringPart],
        line: u32,
    ) -> Result<String, BopError> {
        // Mirror the tree-walker: for each Variable part, format the
        // current value of the Bop ident into the buffer. Missing
        // idents surface as a Rust compile error ("cannot find value
        // X"), which is strictly sooner than the tree-walker's
        // runtime "Variable X not found" — acceptable; the program
        // still fails with a clear message.
        let _ = line;
        let mut body = String::from("{ let mut __s = ::std::string::String::new(); ");
        for part in parts {
            match part {
                StringPart::Literal(s) => {
                    write!(body, "__s.push_str({}); ", rust_string_literal(s)).unwrap();
                }
                StringPart::Variable(name) => {
                    // The cloned Value lives only for the duration
                    // of the format call; its Drop tracks the
                    // de-alloc correctly against `bop::memory`.
                    write!(
                        body,
                        "__s.push_str(&format!(\"{{}}\", {}.clone())); ",
                        rust_ident(name)
                    )
                    .unwrap();
                }
            }
        }
        body.push_str("::bop::value::Value::new_str(__s) }");
        Ok(body)
    }

    fn method_call_src(
        &mut self,
        object: &Expr,
        method: &str,
        args: &[Expr],
        line: u32,
    ) -> Result<String, BopError> {
        // Tree-walker evaluates args first, then the object. We
        // match that here so any future programs with side-effecting
        // sub-expressions behave identically.
        let mut arg_tmps = Vec::with_capacity(args.len());
        let mut arg_lets = String::new();
        for arg in args {
            let src = self.expr_src(arg)?;
            let tmp = self.fresh_tmp();
            write!(arg_lets, "let {} = {}; ", tmp, src).unwrap();
            arg_tmps.push(tmp);
        }

        let obj_src = self.expr_src(object)?;
        let obj_tmp = self.fresh_tmp();
        // Two slices of the same args: one for the user
        // dispatcher, one for the builtin fallback. Cloning lets
        // each site own its own copy and matches the walker's
        // value-semantics (args are already cloned when passed to
        // user methods).
        let args_arr = if arg_tmps.is_empty() {
            "[]".to_string()
        } else {
            format!(
                "[{}]",
                arg_tmps
                    .iter()
                    .map(|t| format!("{}.clone()", t))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };

        // Method name goes into a Rust string literal; we also look
        // up "is this mutating?" up-front so we only emit the
        // back-assign branch when it's actually needed.
        let method_lit = rust_string_literal(method);
        let mutating = is_mutating_method(method);
        let ident_target = if mutating {
            match &object.kind {
                ExprKind::Ident(n) => Some(rust_ident(n)),
                _ => None,
            }
        } else {
            None
        };

        let mut body = String::new();
        write!(body, "{{ {}let {} = {}; ", arg_lets, obj_tmp, obj_src).unwrap();
        // Try user-defined methods first — the dispatcher returns
        // `Some(Value)` when a match is found, else `None`
        // (meaning "fall through to the built-in method
        // dispatch"). Matches walker / VM precedence.
        match ident_target {
            Some(target) => {
                write!(
                    body,
                    "let __ret = match __bop_try_user_method(ctx, &{}, {}, &{}, {})? {{ \
                        Some(v) => v, \
                        None => {{ \
                            let (__r, __mutated) = __bop_call_method(ctx, &{}, {}, &{}, {})?; \
                            if let Some(__new_obj) = __mutated {{ {} = __new_obj; }} \
                            __r \
                        }}, \
                    }}; __ret }}",
                    obj_tmp, method_lit, args_arr, line, obj_tmp, method_lit, args_arr, line, target
                )
                .unwrap();
            }
            None => {
                write!(
                    body,
                    "let __ret = match __bop_try_user_method(ctx, &{}, {}, &{}, {})? {{ \
                        Some(v) => v, \
                        None => {{ \
                            let (__r, _) = __bop_call_method(ctx, &{}, {}, &{}, {})?; __r \
                        }}, \
                    }}; __ret }}",
                    obj_tmp, method_lit, args_arr, line, obj_tmp, method_lit, args_arr, line
                )
                .unwrap();
            }
        }
        Ok(body)
    }

    /// Emit a lambda expression as an `AotClosure`-wrapped
    /// `Value::Fn`. Free variables that resolve to outer locals
    /// are cloned into the closure via `move`; anything else
    /// (top-level fns, builtins) stays reachable through the
    /// usual Call / Ident dispatch inside the body.
    fn lambda_src(
        &mut self,
        params: &[String],
        body: &[Stmt],
        line: u32,
    ) -> Result<String, BopError> {
        // Free-variable analysis against the outer scope stack.
        let mut captures = std::collections::BTreeSet::<String>::new();
        let mut body_known = HashSet::new();
        for p in params {
            body_known.insert(p.clone());
        }
        scan_free_vars_stmts(
            body,
            &mut body_known,
            &mut captures,
            &self.scope_stack,
            &self.fn_info,
        );
        let captures_ordered: Vec<String> = captures.into_iter().collect();

        // Switch into the lambda's lexical context before emitting
        // its body: outer scope is hidden (so Ident lookups inside
        // resolve against the closure's scope, not the outer fn's
        // Rust locals), and the new scope holds both the params
        // and the names we'll re-introduce as moved captures.
        let saved_scope_stack = core::mem::take(&mut self.scope_stack);
        self.push_scope();
        for p in params {
            self.bind_local(p);
        }
        for cap in &captures_ordered {
            self.bind_local(cap);
        }

        // Emit body into a side buffer so we can splice it into
        // the closure literal without disturbing indentation or
        // the tmp counter. The closure expands to a Rust move
        // closure that returns `Result<Value, BopError>`, so
        // `try` inside the body propagates via `return Ok(...)`
        // like any other user fn — never via the top-level
        // raise path.
        let saved_out = core::mem::take(&mut self.out);
        let saved_indent = self.indent;
        let saved_tmp = self.tmp_counter;
        let saved_top_level = self.in_top_level;
        self.indent = 0;
        self.tmp_counter = 0;
        self.in_top_level = false;
        for s in body {
            self.emit_stmt(s)?;
        }
        let body_src = core::mem::replace(&mut self.out, saved_out);
        self.indent = saved_indent;
        self.tmp_counter = saved_tmp;
        self.in_top_level = saved_top_level;

        self.pop_scope();
        self.scope_stack = saved_scope_stack;

        // Build the capture prelude (outer-side clones) and the
        // in-closure rebinding (moved values shadowed under the
        // original Bop names so the body references them
        // naturally).
        let mut capture_prelude = String::new();
        let mut capture_moves = String::new();
        for (i, cap) in captures_ordered.iter().enumerate() {
            writeln!(
                capture_prelude,
                "let __cap_{i} = {ident}.clone();",
                i = i,
                ident = rust_ident(cap)
            )
            .unwrap();
            // `Fn` closures can be invoked repeatedly, so captures
            // must stay owned by the closure body. Clone per
            // invocation; the outer capture is the stable source.
            writeln!(
                capture_moves,
                "let mut {ident}: ::bop::value::Value = __cap_{i}.clone();",
                ident = rust_ident(cap),
                i = i
            )
            .unwrap();
        }

        let arity = params.len();
        let arity_suffix = if arity == 1 { "" } else { "s" };
        let mut param_binds = String::new();
        for p in params {
            writeln!(
                param_binds,
                "let mut {ident}: ::bop::value::Value = args.remove(0);",
                ident = rust_ident(p)
            )
            .unwrap();
        }
        let params_array = if params.is_empty() {
            "::std::vec::Vec::<String>::new()".to_string()
        } else {
            format!(
                "vec![{}]",
                params
                    .iter()
                    .map(|p| format!("\"{}\".to_string()", p))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };

        Ok(format!(
            "{{ {prelude}let __callable: ::std::rc::Rc<dyn for<'__a> Fn(&mut Ctx<'__a>, ::std::vec::Vec<::bop::value::Value>) -> Result<::bop::value::Value, ::bop::error::BopError>> = ::std::rc::Rc::new(move |ctx, mut args| {{ if args.len() != {arity} {{ return Err(::bop::error::BopError::runtime(format!(\"lambda expects {arity} argument{suffix}, but got {{}}\", args.len()), {line})); }} {moves}{param_binds}{body} #[allow(unreachable_code)] Ok(::bop::value::Value::None) }}); __bop_wrap_callable({params_array}, ::std::vec::Vec::new(), None, __callable) }}",
            prelude = capture_prelude,
            arity = arity,
            suffix = arity_suffix,
            line = line,
            moves = capture_moves,
            param_binds = param_binds,
            body = body_src,
            params_array = params_array,
        ))
    }
}

// ─── Free-variable analysis for lambdas ────────────────────────────
//
// Walks a lambda's body collecting every Ident that resolves to an
// *outer* local — i.e. something present in `outer_scopes` and not
// shadowed by a param / local inside the lambda. Top-level fns
// stay callable without capture (they're globally reachable Rust
// fns) and unknown identifiers are left for rustc to flag.

fn scan_free_vars_stmts(
    stmts: &[Stmt],
    known: &mut HashSet<String>,
    free: &mut std::collections::BTreeSet<String>,
    outer_scopes: &[HashSet<String>],
    fn_info: &FnInfo,
) {
    for stmt in stmts {
        scan_free_vars_stmt(stmt, known, free, outer_scopes, fn_info);
    }
}

fn scan_free_vars_stmt(
    stmt: &Stmt,
    known: &mut HashSet<String>,
    free: &mut std::collections::BTreeSet<String>,
    outer_scopes: &[HashSet<String>],
    fn_info: &FnInfo,
) {
    match &stmt.kind {
        StmtKind::Let { name, value, is_const: _ } => {
            scan_free_vars_expr(value, known, free, outer_scopes, fn_info);
            known.insert(name.clone());
        }
        StmtKind::Assign { target, op: _, value } => {
            match target {
                AssignTarget::Variable(n) => {
                    if !known.contains(n) && is_outer_local(n, outer_scopes) {
                        free.insert(n.clone());
                    }
                }
                AssignTarget::Index { object, index } => {
                    scan_free_vars_expr(object, known, free, outer_scopes, fn_info);
                    scan_free_vars_expr(index, known, free, outer_scopes, fn_info);
                }
                AssignTarget::Field { object, .. } => {
                    scan_free_vars_expr(object, known, free, outer_scopes, fn_info);
                }
            }
            scan_free_vars_expr(value, known, free, outer_scopes, fn_info);
        }
        StmtKind::If {
            condition,
            body,
            else_ifs,
            else_body,
        } => {
            scan_free_vars_expr(condition, known, free, outer_scopes, fn_info);
            let saved = known.clone();
            scan_free_vars_stmts(body, known, free, outer_scopes, fn_info);
            *known = saved.clone();
            for (c, b) in else_ifs {
                scan_free_vars_expr(c, known, free, outer_scopes, fn_info);
                scan_free_vars_stmts(b, known, free, outer_scopes, fn_info);
                *known = saved.clone();
            }
            if let Some(b) = else_body {
                scan_free_vars_stmts(b, known, free, outer_scopes, fn_info);
                *known = saved;
            }
        }
        StmtKind::While { condition, body } => {
            scan_free_vars_expr(condition, known, free, outer_scopes, fn_info);
            let saved = known.clone();
            scan_free_vars_stmts(body, known, free, outer_scopes, fn_info);
            *known = saved;
        }
        StmtKind::Repeat { count, body } => {
            scan_free_vars_expr(count, known, free, outer_scopes, fn_info);
            let saved = known.clone();
            scan_free_vars_stmts(body, known, free, outer_scopes, fn_info);
            *known = saved;
        }
        StmtKind::ForIn {
            var,
            iterable,
            body,
        } => {
            scan_free_vars_expr(iterable, known, free, outer_scopes, fn_info);
            let saved = known.clone();
            known.insert(var.clone());
            scan_free_vars_stmts(body, known, free, outer_scopes, fn_info);
            *known = saved;
        }
        StmtKind::FnDecl { name, params, body } => {
            // A nested fn decl introduces `name` into the body's
            // scope (so recursive refs work) but its own body is
            // analysed with only its params in scope — matches
            // how the walker / VM treat nested fns.
            known.insert(name.clone());
            let mut inner_known = HashSet::new();
            for p in params {
                inner_known.insert(p.clone());
            }
            inner_known.insert(name.clone());
            scan_free_vars_stmts(body, &mut inner_known, free, outer_scopes, fn_info);
        }
        StmtKind::Return { value } => {
            if let Some(v) = value {
                scan_free_vars_expr(v, known, free, outer_scopes, fn_info);
            }
        }
        StmtKind::Break | StmtKind::Continue => {}
        StmtKind::Use { .. } => {
            // Imports inside lambda bodies are already rejected at
            // emit time for the AOT — leave the scan a no-op
            // rather than inventing phantom captures.
        }
        StmtKind::StructDecl { .. } => {
            // Struct declarations don't reference any identifiers
            // — only field names, which are lookup keys, not
            // bindings. Nothing to capture.
        }
        StmtKind::EnumDecl { .. } => {
            // Enum declarations are purely type-level — no free
            // variables. Treat identically to struct decls.
        }
        StmtKind::MethodDecl { .. } => {
            // Method declarations — the AOT rejects them at emit
            // time; no free-var analysis needed here.
        }
        StmtKind::ExprStmt(e) => {
            scan_free_vars_expr(e, known, free, outer_scopes, fn_info);
        }
    }
}

/// Walks a `Pattern` and records every name it binds into `known`
/// so the arm's free-var scan treats those bindings as locals
/// rather than captures. Kept free-standing so the AOT emitter
/// can call it from the exhaustive `ExprKind` match without
/// pulling the entire pattern-matching path through `impl` state.
fn collect_pattern_bindings(pattern: &bop::parser::Pattern, known: &mut HashSet<String>) {
    use bop::parser::{ArrayRest, Pattern, VariantPatternPayload};
    match pattern {
        Pattern::Literal(_) | Pattern::Wildcard => {}
        Pattern::Binding(name) => {
            known.insert(name.clone());
        }
        Pattern::EnumVariant { payload, .. } => match payload {
            VariantPatternPayload::Unit => {}
            VariantPatternPayload::Tuple(items) => {
                for item in items {
                    collect_pattern_bindings(item, known);
                }
            }
            VariantPatternPayload::Struct { fields, .. } => {
                for (_, p) in fields {
                    collect_pattern_bindings(p, known);
                }
            }
        },
        Pattern::Struct { fields, .. } => {
            for (_, p) in fields {
                collect_pattern_bindings(p, known);
            }
        }
        Pattern::Array { elements, rest } => {
            for e in elements {
                collect_pattern_bindings(e, known);
            }
            if let Some(ArrayRest::Named(name)) = rest {
                known.insert(name.clone());
            }
        }
        Pattern::Or(alts) => {
            // Every alternative is required to bind the same set
            // of names, so scanning the first one is sufficient.
            if let Some(first) = alts.first() {
                collect_pattern_bindings(first, known);
            }
        }
    }
}

/// Emit Rust source that constructs a `bop::parser::Pattern`
/// value equivalent to `pat`. The emitted expression is used at
/// runtime by `bop::pattern_matches`, so walker / VM / AOT all
/// share one matching implementation.
fn pattern_rust(pat: &bop::parser::Pattern) -> String {
    use bop::parser::{ArrayRest, Pattern};
    match pat {
        Pattern::Wildcard => "::bop::parser::Pattern::Wildcard".to_string(),
        Pattern::Binding(name) => format!(
            "::bop::parser::Pattern::Binding({}.to_string())",
            rust_string_literal(name)
        ),
        Pattern::Literal(lit) => format!(
            "::bop::parser::Pattern::Literal({})",
            literal_pattern_rust(lit)
        ),
        Pattern::EnumVariant {
            namespace,
            type_name,
            variant,
            payload,
        } => format!(
            "::bop::parser::Pattern::EnumVariant {{ namespace: {}, type_name: {}.to_string(), variant: {}.to_string(), payload: {} }}",
            optional_string_rust(namespace.as_deref()),
            rust_string_literal(type_name),
            rust_string_literal(variant),
            variant_payload_rust(payload),
        ),
        Pattern::Struct {
            namespace,
            type_name,
            fields,
            rest,
        } => {
            let fields_src = if fields.is_empty() {
                "::std::vec::Vec::new()".to_string()
            } else {
                let parts: Vec<String> = fields
                    .iter()
                    .map(|(k, p)| {
                        format!(
                            "({}.to_string(), {})",
                            rust_string_literal(k),
                            pattern_rust(p)
                        )
                    })
                    .collect();
                format!("::std::vec::Vec::from([{}])", parts.join(", "))
            };
            format!(
                "::bop::parser::Pattern::Struct {{ namespace: {}, type_name: {}.to_string(), fields: {}, rest: {} }}",
                optional_string_rust(namespace.as_deref()),
                rust_string_literal(type_name),
                fields_src,
                rest,
            )
        }
        Pattern::Array { elements, rest } => {
            let elems_src = if elements.is_empty() {
                "::std::vec::Vec::new()".to_string()
            } else {
                let parts: Vec<String> = elements.iter().map(pattern_rust).collect();
                format!("::std::vec::Vec::from([{}])", parts.join(", "))
            };
            let rest_src = match rest {
                None => "::std::option::Option::None".to_string(),
                Some(ArrayRest::Ignored) => {
                    "::std::option::Option::Some(::bop::parser::ArrayRest::Ignored)".to_string()
                }
                Some(ArrayRest::Named(n)) => format!(
                    "::std::option::Option::Some(::bop::parser::ArrayRest::Named({}.to_string()))",
                    rust_string_literal(n)
                ),
            };
            format!(
                "::bop::parser::Pattern::Array {{ elements: {}, rest: {} }}",
                elems_src, rest_src,
            )
        }
        Pattern::Or(alts) => {
            let parts: Vec<String> = alts.iter().map(pattern_rust).collect();
            let alts_src = if parts.is_empty() {
                "::std::vec::Vec::new()".to_string()
            } else {
                format!("::std::vec::Vec::from([{}])", parts.join(", "))
            };
            format!("::bop::parser::Pattern::Or({})", alts_src)
        }
    }
}

fn literal_pattern_rust(lit: &bop::parser::LiteralPattern) -> String {
    use bop::parser::LiteralPattern;
    match lit {
        LiteralPattern::Int(n) => format!(
            "::bop::parser::LiteralPattern::Int({}i64)",
            n
        ),
        LiteralPattern::Number(n) => format!(
            "::bop::parser::LiteralPattern::Number({})",
            rust_f64(*n)
        ),
        LiteralPattern::Str(s) => format!(
            "::bop::parser::LiteralPattern::Str({}.to_string())",
            rust_string_literal(s)
        ),
        LiteralPattern::Bool(b) => format!("::bop::parser::LiteralPattern::Bool({})", b),
        LiteralPattern::None => "::bop::parser::LiteralPattern::None".to_string(),
    }
}

fn variant_payload_rust(payload: &bop::parser::VariantPatternPayload) -> String {
    use bop::parser::VariantPatternPayload;
    match payload {
        VariantPatternPayload::Unit => {
            "::bop::parser::VariantPatternPayload::Unit".to_string()
        }
        VariantPatternPayload::Tuple(items) => {
            let parts: Vec<String> = items.iter().map(pattern_rust).collect();
            let inner = if parts.is_empty() {
                "::std::vec::Vec::new()".to_string()
            } else {
                format!("::std::vec::Vec::from([{}])", parts.join(", "))
            };
            format!("::bop::parser::VariantPatternPayload::Tuple({})", inner)
        }
        VariantPatternPayload::Struct { fields, rest } => {
            let fields_src = if fields.is_empty() {
                "::std::vec::Vec::new()".to_string()
            } else {
                let parts: Vec<String> = fields
                    .iter()
                    .map(|(k, p)| {
                        format!(
                            "({}.to_string(), {})",
                            rust_string_literal(k),
                            pattern_rust(p)
                        )
                    })
                    .collect();
                format!("::std::vec::Vec::from([{}])", parts.join(", "))
            };
            format!(
                "::bop::parser::VariantPatternPayload::Struct {{ fields: {}, rest: {} }}",
                fields_src, rest,
            )
        }
    }
}

fn scan_free_vars_expr(
    expr: &Expr,
    known: &mut HashSet<String>,
    free: &mut std::collections::BTreeSet<String>,
    outer_scopes: &[HashSet<String>],
    fn_info: &FnInfo,
) {
    match &expr.kind {
        ExprKind::Int(_)
        | ExprKind::Number(_)
        | ExprKind::Str(_)
        | ExprKind::Bool(_)
        | ExprKind::None => {}
        ExprKind::Ident(name) => {
            // If the ident resolves against the enclosing Bop
            // scope (and isn't shadowed by a binding inside the
            // lambda), record it as a capture. Top-level fns and
            // the `self_name` of the enclosing decl don't need to
            // be captured — they stay callable directly.
            if !known.contains(name)
                && !fn_info.top_level_fns.contains(name)
                && is_outer_local(name, outer_scopes)
            {
                free.insert(name.clone());
            }
        }
        ExprKind::StringInterp(parts) => {
            for part in parts {
                if let StringPart::Variable(name) = part {
                    if !known.contains(name) && is_outer_local(name, outer_scopes) {
                        free.insert(name.clone());
                    }
                }
            }
        }
        ExprKind::BinaryOp { left, right, .. } => {
            scan_free_vars_expr(left, known, free, outer_scopes, fn_info);
            scan_free_vars_expr(right, known, free, outer_scopes, fn_info);
        }
        ExprKind::UnaryOp { expr: inner, .. } => {
            scan_free_vars_expr(inner, known, free, outer_scopes, fn_info);
        }
        ExprKind::Call { callee, args } => {
            // For Ident callees, the call site either resolves to
            // a local (capture-worthy) or to a builtin/host/user
            // fn (not captured). Same logic as the Ident arm.
            scan_free_vars_expr(callee, known, free, outer_scopes, fn_info);
            for arg in args {
                scan_free_vars_expr(arg, known, free, outer_scopes, fn_info);
            }
        }
        ExprKind::MethodCall { object, args, .. } => {
            scan_free_vars_expr(object, known, free, outer_scopes, fn_info);
            for arg in args {
                scan_free_vars_expr(arg, known, free, outer_scopes, fn_info);
            }
        }
        ExprKind::Index { object, index } => {
            scan_free_vars_expr(object, known, free, outer_scopes, fn_info);
            scan_free_vars_expr(index, known, free, outer_scopes, fn_info);
        }
        ExprKind::Array(items) => {
            for item in items {
                scan_free_vars_expr(item, known, free, outer_scopes, fn_info);
            }
        }
        ExprKind::Dict(entries) => {
            for (_, v) in entries {
                scan_free_vars_expr(v, known, free, outer_scopes, fn_info);
            }
        }
        ExprKind::IfExpr {
            condition,
            then_expr,
            else_expr,
        } => {
            scan_free_vars_expr(condition, known, free, outer_scopes, fn_info);
            scan_free_vars_expr(then_expr, known, free, outer_scopes, fn_info);
            scan_free_vars_expr(else_expr, known, free, outer_scopes, fn_info);
        }
        ExprKind::Lambda { params, body } => {
            // A nested lambda's captures are *its* concern — but
            // anything it references from *our* outer scope still
            // needs to bubble up to be captured here so the inner
            // closure gets the value when it's constructed.
            let mut inner_known = HashSet::new();
            for p in params {
                inner_known.insert(p.clone());
            }
            scan_free_vars_stmts(body, &mut inner_known, free, outer_scopes, fn_info);
        }
        ExprKind::FieldAccess { object, .. } => {
            scan_free_vars_expr(object, known, free, outer_scopes, fn_info);
        }
        ExprKind::StructConstruct { fields, .. } => {
            for (_, v) in fields {
                scan_free_vars_expr(v, known, free, outer_scopes, fn_info);
            }
        }
        ExprKind::EnumConstruct { payload, .. } => {
            use bop::parser::VariantPayload;
            match payload {
                VariantPayload::Unit => {}
                VariantPayload::Tuple(args) => {
                    for a in args {
                        scan_free_vars_expr(a, known, free, outer_scopes, fn_info);
                    }
                }
                VariantPayload::Struct(fields) => {
                    for (_, v) in fields {
                        scan_free_vars_expr(v, known, free, outer_scopes, fn_info);
                    }
                }
            }
        }
        ExprKind::Match { scrutinee, arms } => {
            // Recurse into scrutinee, and each arm treats pattern
            // bindings as locals so guard/body references don't
            // bubble up as captures.
            scan_free_vars_expr(scrutinee, known, free, outer_scopes, fn_info);
            for arm in arms {
                let mut arm_known = known.clone();
                collect_pattern_bindings(&arm.pattern, &mut arm_known);
                if let Some(guard) = &arm.guard {
                    scan_free_vars_expr(guard, &mut arm_known, free, outer_scopes, fn_info);
                }
                scan_free_vars_expr(&arm.body, &mut arm_known, free, outer_scopes, fn_info);
            }
        }
        ExprKind::Try(inner) => {
            scan_free_vars_expr(inner, known, free, outer_scopes, fn_info);
        }
    }
}

fn is_outer_local(name: &str, outer_scopes: &[HashSet<String>]) -> bool {
    outer_scopes.iter().any(|s| s.contains(name))
}

// ─── Free helpers ──────────────────────────────────────────────────

fn bin_op_path(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "::bop::ops::add",
        BinOp::Sub => "::bop::ops::sub",
        BinOp::Mul => "::bop::ops::mul",
        BinOp::Div => "::bop::ops::div",
        BinOp::Mod => "::bop::ops::rem",
        BinOp::Eq => "::bop::ops::eq",
        BinOp::NotEq => "::bop::ops::not_eq",
        BinOp::Lt => "::bop::ops::lt",
        BinOp::Gt => "::bop::ops::gt",
        BinOp::LtEq => "::bop::ops::lt_eq",
        BinOp::GtEq => "::bop::ops::gt_eq",
        BinOp::And | BinOp::Or => unreachable!("short-circuit handled separately"),
    }
}

fn compound_op_path(op: AssignOp) -> &'static str {
    match op {
        AssignOp::Eq => unreachable!("caller filters out AssignOp::Eq"),
        AssignOp::AddEq => "::bop::ops::add",
        AssignOp::SubEq => "::bop::ops::sub",
        AssignOp::MulEq => "::bop::ops::mul",
        AssignOp::DivEq => "::bop::ops::div",
        AssignOp::ModEq => "::bop::ops::rem",
    }
}

fn build_arg_array(arg_names: &[String]) -> String {
    if arg_names.is_empty() {
        "[]".to_string()
    } else {
        format!("[{}]", arg_names.join(", "))
    }
}

/// Render a Bop identifier as a Rust identifier, escaping Rust
/// keywords with the raw-identifier prefix when needed. `self` is
/// special-cased: Rust reserves it for method receivers and
/// refuses to raw-escape it, so we remap to `bop_self`
/// consistently across param lists and body references.
fn rust_ident(name: &str) -> String {
    if name == "self" {
        "bop_self".to_string()
    } else if is_rust_keyword(name) {
        format!("r#{}", name)
    } else {
        name.to_string()
    }
}

/// Render a Bop user-fn name as a Rust function name under a
/// specific module prefix (`""` for the root program). Kept
/// prefixed to avoid clashes with built-ins, and extended with
/// the module slug so `foo.bar::square` and root::square can
/// coexist in the same emitted Rust file.
fn rust_fn_name_with(module_prefix: &str, name: &str) -> String {
    format!("bop_fn_{}{}", module_prefix, name)
}

fn wrapper_fn_name_with(module_prefix: &str, name: &str) -> String {
    format!("__bop_fn_value_{}{}", module_prefix, name)
}

/// Turn a Bop module path (`std.math.extra`) into a Rust-safe
/// identifier slug (`std_math_extra`). Used as the prefix for
/// everything a module emits — fns, wrappers, the load fn, the
/// exports struct.
fn module_slug(path: &str) -> String {
    path.replace('.', "_")
}

fn module_load_fn_name(path: &str) -> String {
    format!("__mod_{}__load", module_slug(path))
}

fn module_exports_type_name(path: &str) -> String {
    format!("__mod_{}__Exports", module_slug(path))
}

/// Kept in sync with `bop::methods::is_mutating_method` —
/// duplicated here so the emitter can make the decision at compile
/// time and skip the back-assign boilerplate for pure methods.
fn is_mutating_method(method: &str) -> bool {
    matches!(
        method,
        "push" | "pop" | "insert" | "remove" | "reverse" | "sort"
    )
}

fn is_rust_keyword(s: &str) -> bool {
    // Raw-identifier-escapable Rust keywords. `self`, `Self`,
    // `crate`, `super`, `extern` can't be raw-escaped, but those
    // wouldn't survive a reasonable Bop program either.
    matches!(
        s,
        "as" | "async"
            | "await"
            | "const"
            | "dyn"
            | "enum"
            | "gen"
            | "impl"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "static"
            | "struct"
            | "trait"
            | "try"
            | "type"
            | "unsafe"
            | "use"
            | "where"
    )
}

/// Render `f64` such that the emitted Rust preserves bit-exactness
/// for whole numbers (no trailing `.0` surprises) while falling back
/// to `{:?}` for anything non-finite or non-integral.
fn rust_f64(n: f64) -> String {
    if n.is_nan() {
        "f64::NAN".to_string()
    } else if n.is_infinite() {
        if n > 0.0 {
            "f64::INFINITY".to_string()
        } else {
            "f64::NEG_INFINITY".to_string()
        }
    } else {
        // Rust's `{:?}` on f64 always includes a decimal point,
        // which is exactly what we want so the literal parses back
        // as `f64` and not `i32`.
        format!("{:?}", n)
    }
}

/// Lower an `Option<&str>` to a Rust expression of type
/// `Option<String>`. Used when emitting `Pattern::EnumVariant` /
/// `Pattern::Struct` with their `namespace` field: runtime
/// pattern matching doesn't consult the namespace (types
/// register globally), but we still have to populate the field
/// so the struct literal type-checks.
fn optional_string_rust(s: Option<&str>) -> String {
    match s {
        Some(v) => format!(
            "::std::option::Option::Some({}.to_string())",
            rust_string_literal(v)
        ),
        None => "::std::option::Option::None".to_string(),
    }
}

/// Escape a Bop string literal for embedding in Rust source.
fn rust_string_literal(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                write!(out, "\\u{{{:x}}}", c as u32).unwrap();
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ─── Program skeleton ──────────────────────────────────────────────

const HEADER: &str = r#"// Auto-generated by bop-compile — do not edit.
// Regenerate from Bop source rather than editing this file.
#![allow(unused_mut, unused_variables, unused_parens, unused_braces, unreachable_code, dead_code)]

"#;

// ─── Runtime preamble, split into composable pieces ───────────────
//
// Prior to the phase-9 dedup this crate shipped two near-identical
// preamble strings (`RUNTIME_PREAMBLE` and `RUNTIME_PREAMBLE_SANDBOX`)
// that diverged every time a new helper landed. The four constants
// below are stitched together at emit time in `emit_runtime_preamble`:
//
//   RUNTIME_HEADER — `// ── Runtime context and helpers ──` banner.
//   CTX_BASE / CTX_SANDBOX — just the `Ctx` struct. Sandbox variant
//       adds `steps` and `max_steps`.
//   RUNTIME_SHARED — every type decl + helper fn that doesn't depend
//       on tick accounting. Same source for both variants.
//   TICK_HELPER — the `__bop_tick` fn. Appended only in sandbox mode;
//       it references `ctx.steps` / `ctx.max_steps` so it'd fail to
//       compile against `CTX_BASE`.
//
// Order between `RUNTIME_SHARED` and `TICK_HELPER` doesn't matter for
// Rust fn visibility — `__bop_tick` is referenced by the generated
// `run_program` body, which lives after the preamble.

const RUNTIME_HEADER: &str = "// ─── Runtime context and helpers ────────────────────────────────\n\n";

const CTX_BASE: &str = r#"pub struct Ctx<'h> {
    pub host: &'h mut dyn ::bop::BopHost,
    pub rand_state: u64,
    /// Per-program module cache keyed by the Bop module path (the
    /// dot-joined string). `load` fns use this to memoise imports
    /// and to spot circular dependencies via the `__ModuleLoading`
    /// sentinel.
    pub module_cache:
        ::std::collections::HashMap<::std::string::String, ::std::boxed::Box<dyn ::core::any::Any + 'static>>,
}

"#;

const CTX_SANDBOX: &str = r#"pub struct Ctx<'h> {
    pub host: &'h mut dyn ::bop::BopHost,
    pub rand_state: u64,
    /// Tick counter — bumped by `__bop_tick` at every loop
    /// backedge and fn entry so runaway programs hit the step
    /// budget before they exhaust the host.
    pub steps: u64,
    /// Upper bound on `steps`, populated from
    /// `BopLimits::max_steps` at `run()` entry.
    pub max_steps: u64,
    /// Per-program module cache keyed by the Bop module path (the
    /// dot-joined string). `load` fns use this to memoise imports
    /// and to spot circular dependencies via the `__ModuleLoading`
    /// sentinel.
    pub module_cache:
        ::std::collections::HashMap<::std::string::String, ::std::boxed::Box<dyn ::core::any::Any + 'static>>,
}

"#;

const TICK_HELPER: &str = r#"/// Step / memory / on_tick checkpoint. Emitted by `bop-compile` at
/// the head of every loop iteration and at function entry when the
/// `sandbox` option is on. Mirrors `Evaluator::tick` in `bop-lang`.
#[inline]
fn __bop_tick(ctx: &mut Ctx<'_>, line: u32) -> Result<(), ::bop::error::BopError> {
    ctx.steps += 1;
    if ctx.steps > ctx.max_steps {
        return Err(::bop::builtins::error_fatal_with_hint(
            line,
            "Your code took too many steps (possible infinite loop)",
            "Check your loops — make sure they have a condition that eventually stops them.",
        ));
    }
    if ::bop::memory::bop_memory_exceeded() {
        return Err(::bop::builtins::error_fatal_with_hint(
            line,
            "Memory limit exceeded",
            "Your code is using too much memory. Check for large strings or arrays growing in loops.",
        ));
    }
    ctx.host.on_tick()?;
    Ok(())
}

"#;

const RUNTIME_SHARED: &str = r#"/// Sentinel type inserted into `module_cache` while a module's
/// body is evaluating. If a load fn hits its own entry in this
/// state it means a circular import — the runtime returns a clear
/// error and halts.
pub struct __ModuleLoading;

/// Opaque body that a `Value::Fn` carries around in AOT-emitted
/// code. The callable is a higher-ranked `Fn` so the same Rc can
/// satisfy any lifetime of `Ctx`, which lets us store closures in
/// `Value::Fn` (whose body is `Rc<dyn Any + 'static>`) regardless
/// of where they're eventually called from.
pub struct AotClosure {
    pub callable: ::std::rc::Rc<
        dyn for<'__a> Fn(
            &mut Ctx<'__a>,
            ::std::vec::Vec<::bop::value::Value>,
        ) -> Result<::bop::value::Value, ::bop::error::BopError>,
    >,
}

/// Build a `Value::Fn` around an `AotClosure`. Used by the emitted
/// `__bop_fn_value_<name>` wrappers and by `MakeLambda` emission.
fn __bop_wrap_callable(
    params: ::std::vec::Vec<String>,
    captures: ::std::vec::Vec<(String, ::bop::value::Value)>,
    self_name: ::std::option::Option<String>,
    callable: ::std::rc::Rc<
        dyn for<'__a> Fn(
            &mut Ctx<'__a>,
            ::std::vec::Vec<::bop::value::Value>,
        ) -> Result<::bop::value::Value, ::bop::error::BopError>,
    >,
) -> ::bop::value::Value {
    let closure = ::std::rc::Rc::new(AotClosure { callable });
    let body: ::std::rc::Rc<dyn ::core::any::Any + 'static> = closure;
    ::bop::value::Value::new_compiled_fn(params, captures, body, self_name)
}

/// Runtime dispatch for a value-based call: the callee is a
/// `Value::Fn` whose body must be an `AotClosure` we recognise.
/// Walker-created closures carrying an AST body come back as a
/// clear error rather than silently misbehaving.
fn __bop_call_value(
    ctx: &mut Ctx<'_>,
    callee: ::bop::value::Value,
    args: ::std::vec::Vec<::bop::value::Value>,
    line: u32,
) -> Result<::bop::value::Value, ::bop::error::BopError> {
    match &callee {
        ::bop::value::Value::Fn(f) => match &f.body {
            ::bop::value::FnBody::Compiled(body) => {
                if let Some(aot) = body.downcast_ref::<AotClosure>() {
                    let callable = ::std::rc::Rc::clone(&aot.callable);
                    drop(callee);
                    callable(ctx, args)
                } else {
                    Err(::bop::error::BopError::runtime(
                        "Closure body wasn't compiled by the AOT transpiler",
                        line,
                    ))
                }
            }
            ::bop::value::FnBody::Ast(_) => Err(::bop::error::BopError::runtime(
                "Closure wasn't compiled for the AOT — use `bop::run` for tree-walker closures",
                line,
            )),
        },
        other => Err(::bop::error::BopError::runtime(
            ::bop::error_messages::cant_call_a(other.type_name()),
            line,
        )),
    }
}

/// `try_call(f)` implementation. Invokes `f` with no args via
/// the existing `AotClosure` pathway, then wraps the outcome
/// for the caller: `Ok(value)` on normal return, `Err(RuntimeError
/// { message, line })` on a non-fatal error, and re-raised on
/// fatal errors (resource-limit violations). `Result` and
/// `RuntimeError` values are built directly by
/// `bop::builtins::make_try_call_ok` / `_err`, so the
/// program doesn't need to have declared those types itself.
fn __bop_try_call(
    ctx: &mut Ctx<'_>,
    callable: ::bop::value::Value,
    line: u32,
) -> Result<::bop::value::Value, ::bop::error::BopError> {
    let func = match &callable {
        ::bop::value::Value::Fn(f) => ::std::rc::Rc::clone(f),
        other => {
            return Err(::bop::error::BopError::runtime(
                format!(
                    "`try_call` expects a function, got {}",
                    other.type_name()
                ),
                line,
            ));
        }
    };
    drop(callable);
    let callable_fn = match &func.body {
        ::bop::value::FnBody::Compiled(body) => match body.downcast_ref::<AotClosure>() {
            Some(aot) => ::std::rc::Rc::clone(&aot.callable),
            None => {
                return Err(::bop::error::BopError::runtime(
                    "try_call: callee wasn't compiled by the AOT transpiler",
                    line,
                ));
            }
        },
        ::bop::value::FnBody::Ast(_) => {
            return Err(::bop::error::BopError::runtime(
                "try_call: callee was compiled for the walker, not AOT",
                line,
            ));
        }
    };
    drop(func);
    match callable_fn(ctx, ::std::vec::Vec::new()) {
        Ok(value) => Ok(::bop::builtins::make_try_call_ok(value)),
        Err(err) => {
            if err.is_fatal {
                Err(err)
            } else {
                Ok(::bop::builtins::make_try_call_err(&err))
            }
        }
    }
}

/// Field read for `Value::Struct`, struct-payload enum variants,
/// and `Value::Module`. Returns a cloned value; misses surface as
/// a runtime error with the type name in the message.
///
/// Module field reads resolve against the module's `bindings`
/// list. A field name that matches the module's own `types` list
/// raises a targeted error — types aren't first-class values at
/// this stage, so `m.MyType` by itself is a programming mistake
/// (callers reach types through `m.MyType { ... }` or
/// `m.MyEnum::Variant(...)`, which go through namespace-aware
/// construct codegen, not `__bop_field_get`).
#[inline]
fn __bop_field_get(
    obj: &::bop::value::Value,
    field: &str,
    line: u32,
) -> Result<::bop::value::Value, ::bop::error::BopError> {
    match obj {
        ::bop::value::Value::Struct(s) => s.field(field).cloned().ok_or_else(|| {
            let mut err = ::bop::error::BopError::runtime(
                ::bop::error_messages::struct_has_no_field(s.type_name(), field),
                line,
            );
            let names = s.fields().iter().map(|(k, _)| k.as_str());
            if let Some(hint) = ::bop::suggest::did_you_mean(field, names) {
                err.friendly_hint = Some(hint);
            }
            err
        }),
        ::bop::value::Value::EnumVariant(e) => e.field(field).cloned().ok_or_else(|| {
            ::bop::error::BopError::runtime(
                ::bop::error_messages::variant_has_no_field(
                    e.type_name(),
                    e.variant(),
                    field,
                ),
                line,
            )
        }),
        ::bop::value::Value::Module(m) => {
            if let Some((_, v)) = m.bindings.iter().find(|(k, _)| k == field) {
                return Ok(v.clone());
            }
            if m.types.iter().any(|t| t == field) {
                return Err(::bop::error::BopError::runtime(
                    format!(
                        "`{}.{}` is a type, not a value — construct it with `{{ ... }}` or `::Variant(...)`",
                        m.path, field
                    ),
                    line,
                ));
            }
            Err(::bop::error::BopError::runtime(
                format!("Module `{}` has no export `{}`", m.path, field),
                line,
            ))
        }
        other => Err(::bop::error::BopError::runtime(
            ::bop::error_messages::cant_read_field(field, other.type_name()),
            line,
        )),
    }
}

/// Runtime guard for namespaced type access: verifies that the
/// value behind the alias is actually a `Value::Module`, and that
/// the module's published `types` list contains the name. Used by
/// struct / enum construct emission when `namespace` is `Some(...)`,
/// so the AOT surfaces the same error as walker + VM when someone
/// writes `m.MissingType { ... }` or uses a non-module as a namespace.
#[inline]
fn __bop_validate_namespace_type(
    ns: &::bop::value::Value,
    alias: &str,
    type_name: &str,
    line: u32,
) -> Result<(), ::bop::error::BopError> {
    match ns {
        ::bop::value::Value::Module(m) => {
            if m.types.iter().any(|t| t == type_name) {
                Ok(())
            } else {
                Err(::bop::error::BopError::runtime(
                    format!(
                        "Module `{}` (bound as `{}`) has no type `{}`",
                        m.path, alias, type_name
                    ),
                    line,
                ))
            }
        }
        other => Err(::bop::error::BopError::runtime(
            format!(
                "`{}` is not a module (got {})",
                alias,
                other.type_name()
            ),
            line,
        )),
    }
}

/// Write a struct field in place (on the owned Value), returning
/// the modified struct. Mirrors the walker's clone-mutate-store
/// pattern.
#[inline]
fn __bop_field_set(
    mut obj: ::bop::value::Value,
    field: &str,
    value: ::bop::value::Value,
    line: u32,
) -> Result<::bop::value::Value, ::bop::error::BopError> {
    match &mut obj {
        ::bop::value::Value::Struct(s) => {
            let type_name = s.type_name().to_string();
            if !s.set_field(field, value) {
                return Err(::bop::error::BopError::runtime(
                    ::bop::error_messages::struct_has_no_field(&type_name, field),
                    line,
                ));
            }
            Ok(obj)
        }
        other => Err(::bop::error::BopError::runtime(
            ::bop::error_messages::cant_assign_field(field, other.type_name()),
            line,
        )),
    }
}

/// Mirror of Evaluator::call_method from bop-lang: dispatches a
/// method call to the right family for the receiver's type.
///
/// For `Value::Module`, `m.helper(args)` reads the binding and
/// invokes it via the value-call path — the resulting value has
/// no "self" to write back, so the back-assign slot is always
/// `None`.
#[inline]
fn __bop_call_method(
    ctx: &mut Ctx<'_>,
    obj: &::bop::value::Value,
    method: &str,
    args: &[::bop::value::Value],
    line: u32,
) -> Result<(::bop::value::Value, Option<::bop::value::Value>), ::bop::error::BopError> {
    // `type` / `to_str` / `inspect` work on every value —
    // check them first so walker / VM / AOT agree on the
    // common method surface without duplicating entries per
    // type-specific dispatcher.
    if let Some(result) = ::bop::methods::common_method(obj, method, args, line)? {
        return Ok(result);
    }
    match obj {
        ::bop::value::Value::Array(arr) => ::bop::methods::array_method(arr, method, args, line),
        ::bop::value::Value::Str(s) => ::bop::methods::string_method(s.as_str(), method, args, line),
        ::bop::value::Value::Dict(d) => ::bop::methods::dict_method(d, method, args, line),
        ::bop::value::Value::Int(_) | ::bop::value::Value::Number(_) => {
            ::bop::methods::numeric_method(obj, method, args, line)
        }
        ::bop::value::Value::Bool(_) => {
            ::bop::methods::bool_method(obj, method, args, line)
        }
        ::bop::value::Value::Module(m) => {
            let binding = m
                .bindings
                .iter()
                .find(|(k, _)| k == method)
                .map(|(_, v)| v.clone())
                .ok_or_else(|| {
                    ::bop::error::BopError::runtime(
                        format!("Module `{}` has no export `{}`", m.path, method),
                        line,
                    )
                })?;
            let result = __bop_call_value(ctx, binding, args.to_vec(), line)?;
            Ok((result, None))
        }
        _ => Err(::bop::error::BopError::runtime(
            ::bop::error_messages::no_such_method(obj.type_name(), method),
            line,
        )),
    }
}

/// Format the argument list the way Bop's built-in `print` does:
/// values space-separated via their Display impls.
#[inline]
fn __bop_format_print(args: &[::bop::value::Value]) -> String {
    args.iter()
        .map(|v| format!("{}", v))
        .collect::<::std::vec::Vec<_>>()
        .join(" ")
}

/// Materialise an iterable into a Vec of items (mirrors the
/// tree-walker's handling of `for x in ...`).
#[inline]
fn __bop_iter_items(
    mut v: ::bop::value::Value,
    line: u32,
) -> Result<::std::vec::Vec<::bop::value::Value>, ::bop::error::BopError> {
    match &mut v {
        ::bop::value::Value::Array(arr) => Ok(arr.take()),
        ::bop::value::Value::Str(s) => Ok(s
            .chars()
            .map(|c| ::bop::value::Value::new_str(c.to_string()))
            .collect()),
        _ => Err(::bop::error::BopError::runtime(
            ::bop::error_messages::cant_iterate_over(v.type_name()),
            line,
        )),
    }
}

"#;

const PUBLIC_ENTRY: &str = r#"// ─── Public entry points ────────────────────────────────────────

/// Run the compiled program with the supplied host. The memory
/// tracker is initialised to a permissive ceiling so the
/// `bop::memory` allocation hooks on `Value` don't fire spurious
/// limit errors. Embedders that want a real sandbox should compile
/// with `Options::sandbox = true` or call `bop_memory_init`
/// themselves before invoking `run`.
pub fn run<H: ::bop::BopHost>(host: &mut H) -> Result<(), ::bop::error::BopError> {
    ::bop::memory::bop_memory_init(usize::MAX);
    let mut ctx = Ctx {
        host: host as &mut dyn ::bop::BopHost,
        rand_state: 0,
        module_cache: ::std::collections::HashMap::new(),
    };
    run_program(&mut ctx)
}

"#;

const MAIN_FN: &str = r#"fn main() {
    let mut host = ::bop_sys::StandardHost::new();
    if let Err(err) = run(&mut host) {
        eprintln!("{}", err);
        ::std::process::exit(1);
    }
}
"#;

// ─── Sandbox-specific public entry + main fn ─────────────────────
//
// The sandbox runtime itself is built by `emit_runtime_preamble`
// (see `RUNTIME_HEADER` / `CTX_SANDBOX` / `RUNTIME_SHARED` /
// `TICK_HELPER` above). What's below is just the variant-specific
// `run()` signature — sandbox mode takes `&BopLimits` and wires it
// into the memory / step accounting — and the sandbox `main()`
// that calls it.

const PUBLIC_ENTRY_SANDBOX: &str = r#"// ─── Public entry points ────────────────────────────────────────

/// Run the compiled program with the supplied host and resource
/// limits. `limits.max_memory` wires into `bop::memory`'s
/// allocation tracker; `limits.max_steps` caps the number of
/// tick-points (loop iterations + function entries) before the
/// program halts with `Your code took too many steps`.
pub fn run<H: ::bop::BopHost>(
    host: &mut H,
    limits: &::bop::BopLimits,
) -> Result<(), ::bop::error::BopError> {
    ::bop::memory::bop_memory_init(limits.max_memory);
    let mut ctx = Ctx {
        host: host as &mut dyn ::bop::BopHost,
        rand_state: 0,
        steps: 0,
        max_steps: limits.max_steps,
        module_cache: ::std::collections::HashMap::new(),
    };
    run_program(&mut ctx)
}

"#;

const MAIN_FN_SANDBOX: &str = r#"fn main() {
    let mut host = ::bop_sys::StandardHost::new();
    let limits = ::bop::BopLimits::standard();
    if let Err(err) = run(&mut host, &limits) {
        eprintln!("{}", err);
        ::std::process::exit(1);
    }
}
"#;

