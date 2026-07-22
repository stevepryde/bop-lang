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
//! - Variables lower to hygienically-mangled Rust locals (always
//!   `mut` — we don't know if a later statement will reassign, and
//!   `#![allow(unused_mut)]` silences the warning).
//! - User functions take `&mut Ctx<'_>` plus their Bop parameters
//!   as `Value`. Recursion and nested fns work because Rust allows
//!   both fn-in-fn definitions and forward references within a
//!   block.
//!
//! Unsupported constructs (string interpolation, method calls,
//! indexed writes) return a `BopError::runtime` naming the feature
//! so the caller sees a clear "not yet supported" message instead of
//! broken Rust.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
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
    let iter_variants: HashMap<String, VariantKind> = bop::builtins::builtin_iter_variants()
        .into_iter()
        .map(|v| (v.name, v.kind))
        .collect();
    reg.enums.insert(
        (builtin_mp.clone(), String::from("Iter")),
        iter_variants,
    );
    enum_origins.insert(
        (builtin_mp.clone(), String::from("Iter")),
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
    // Then every module's AST. The function prefix uses the same
    // injective component encoding as the emitter. The source `name`
    // is still what shows up in errors and at runtime as the module's
    // type identity.
    for name in &modules.order {
        if let Some(entry) = modules.modules.get(name) {
            let prefix = module_fn_prefix(name);
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
/// ordered so each module comes after its eager top-level imports
/// (topological / leaves-first). Lazy imports in nested runtime
/// bodies are included in the graph without imposing an eager
/// ordering edge. Produced once per transpile and handed to the
/// emitter.
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
    /// Every exposed type name and its declaration module. Facades retain the
    /// origin rather than rebinding imported types to their own path.
    pub effective_types: BTreeMap<String, String>,
    /// Names whose final exported representation is a runtime local rather
    /// than an otherwise same-named lifted function wrapper.
    effective_value_exports: BTreeSet<String>,
    /// Exported value bindings that are module namespaces. Descriptors are
    /// static codegen evidence only; generated loaders publish the live Value.
    effective_module_exports: BTreeMap<String, ModuleValueExport>,
    /// Any module-valued binding visible while the module body executes. This
    /// lets lifted callables compile without publishing future runtime state.
    module_alias_candidates: BTreeMap<String, ModuleValueExport>,
    // Kept as analysis metadata for consumers that need the module's
    // syntactic top-level declarations; the emitter currently uses the
    // source-order-aware `effective_value_exports` set instead.
    #[allow(dead_code)]
    pub own_lets: Vec<String>,
    #[allow(dead_code)]
    pub direct_imports: Vec<String>,
}

#[derive(Clone)]
struct ModuleValueExport {
    module_path: String,
    exposed_bindings: Vec<String>,
    exposed_types: BTreeMap<String, String>,
}

#[derive(Clone)]
struct ModuleImport {
    path: String,
    items: Option<Vec<String>>,
    alias: Option<String>,
    line: u32,
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
                root_imports.first().map(|import| import.line).unwrap_or(0),
            ));
        }
    };

    // Discovery and eager-dependency analysis are deliberately
    // separate. A `use` inside a fn/block/lambda must make its
    // module available to codegen, but it neither runs while the
    // containing module loads nor contributes re-exports. Treating
    // that lazy edge like a top-level edge would also report false
    // cycles for harmless patterns such as `fn f() { use self }`.
    let mut resolved: HashMap<String, Vec<Stmt>> = HashMap::new();
    let mut discovery_order: Vec<(String, u32)> = Vec::new();
    for import in &root_imports {
        resolve_module_tree(
            &import.path,
            import.line,
            &resolver,
            &mut resolved,
            &mut discovery_order,
        )?;
    }

    let mut graph = ModuleGraph {
        order: Vec::new(),
        modules: HashMap::new(),
    };
    let mut visiting: BTreeSet<String> = BTreeSet::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    for (name, line) in discovery_order {
        analyze_module(
            &name,
            line,
            &resolved,
            &mut graph,
            &mut visiting,
            &mut visited,
        )?;
    }
    Ok(graph)
}

/// Resolve and parse every module reachable through any runtime
/// statement body. The AST is cached before following imports so a
/// lazy self/mutual reference is discovery-idempotent rather than a
/// compile-time cycle. Real load-time cycles are checked later from
/// top-level edges only.
fn resolve_module_tree(
    name: &str,
    line: u32,
    resolver: &crate::ModuleResolver,
    resolved: &mut HashMap<String, Vec<Stmt>>,
    discovery_order: &mut Vec<(String, u32)>,
) -> Result<(), BopError> {
    if resolved.contains_key(name) {
        return Ok(());
    }

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
    let imports = collect_imports_in_stmts(&ast);

    // Insert before recursion: this is both the resolver-work cache
    // and the guard against false cycles through lazy import sites.
    resolved.insert(name.to_string(), ast);
    discovery_order.push((name.to_string(), line));
    for import in &imports {
        resolve_module_tree(
            &import.path,
            import.line,
            resolver,
            resolved,
            discovery_order,
        )?;
    }
    Ok(())
}

/// Compute load order and effective exports from eager (module
/// top-level) imports. Nested imports were resolved in the discovery
/// phase, but are intentionally absent here because their bindings
/// are local to the runtime body that executes them.
fn analyze_module(
    name: &str,
    line: u32,
    resolved: &HashMap<String, Vec<Stmt>>,
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

    let ast = resolved
        .get(name)
        .cloned()
        .expect("discovered module has a parsed AST");

    // Only top-level imports execute as part of module loading and
    // can contribute bindings/types to this module's exports.
    let direct_imports = collect_top_level_imports(&ast);
    for import in &direct_imports {
        analyze_module(
            &import.path,
            import.line,
            resolved,
            graph,
            visiting,
            visited,
        )?;
    }

    let own_lets = collect_top_level_lets(&ast);
    let own_fns = collect_top_level_fn_params(&ast);
    let own_types = collect_top_level_types(&ast);

    // Effective exports mirror the bindings that each `use` shape
    // actually introduces into this module's scope. A plain glob
    // propagates public dependency names, a selective import only
    // propagates the listed names, and an aliased import contributes
    // only its alias value (never the dependency's bare exports).
    // De-dup while preserving import/declaration order.
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut exports: Vec<String> = Vec::new();
    for import in &direct_imports {
        if let Some(alias) = &import.alias {
            push_unique(&mut exports, &mut seen, alias);
            continue;
        }
        let Some(module) = graph.modules.get(&import.path) else {
            continue;
        };
        match &import.items {
            Some(items) => {
                for item in items {
                    if module.effective_exports.iter().any(|name| name == item) {
                        push_unique(&mut exports, &mut seen, item);
                    }
                }
            }
            None => {
                for name in &module.effective_exports {
                    if !name.starts_with('_') {
                        push_unique(&mut exports, &mut seen, name);
                    }
                }
            }
        }
    }
    for name in &own_lets {
        push_unique(&mut exports, &mut seen, name);
    }
    for name in own_fns.keys() {
        push_unique(&mut exports, &mut seen, name);
    }

    // Types follow the same flat glob/selective projection. Aliases
    // are value bindings and therefore never add bare type names.
    // Definitions live in the global AOT registry; this map retains
    // each exposed name's declaration origin for namespace identity.
    let mut type_exports: BTreeMap<String, String> = BTreeMap::new();
    for import in &direct_imports {
        if import.alias.is_some() {
            continue;
        }
        let Some(module) = graph.modules.get(&import.path) else {
            continue;
        };
        match &import.items {
            Some(items) => {
                for item in items {
                    if let Some(origin) = module.effective_types.get(item) {
                        type_exports
                            .entry(item.clone())
                            .or_insert_with(|| origin.clone());
                    }
                }
            }
            None => {
                for (type_name, origin) in &module.effective_types {
                    if !type_name.starts_with('_') {
                        type_exports
                            .entry(type_name.clone())
                            .or_insert_with(|| origin.clone());
                    }
                }
            }
        }
    }
    for ty in &own_types {
        type_exports.insert(ty.clone(), name.to_string());
    }

    let (module_value_exports, module_alias_candidates, effective_value_exports) =
        effective_module_value_exports(&ast, &graph.modules);

    graph.modules.insert(
        name.to_string(),
        ModuleEntry {
            ast,
            own_fns,
            own_lets,
            direct_imports: direct_imports.into_iter().map(|import| import.path).collect(),
            effective_exports: exports,
            effective_types: type_exports,
            effective_value_exports,
            effective_module_exports: module_value_exports,
            module_alias_candidates,
        },
    );
    graph.order.push(name.to_string());
    visiting.remove(name);
    visited.insert(name.to_string());
    Ok(())
}

fn effective_module_value_exports(
    stmts: &[Stmt],
    modules: &HashMap<String, ModuleEntry>,
) -> (
    BTreeMap<String, ModuleValueExport>,
    BTreeMap<String, ModuleValueExport>,
    BTreeSet<String>,
) {
    let mut value_bindings = BTreeSet::new();
    let mut module_exports = BTreeMap::new();
    let mut candidates = BTreeMap::new();
    let mut functions = BTreeSet::new();

    for stmt in stmts {
        match &stmt.kind {
            StmtKind::Use { path, items, alias } => {
                let Some(module) = modules.get(path) else {
                    continue;
                };
                if let Some(alias_name) = alias {
                    if value_bindings.contains(alias_name) || functions.contains(alias_name) {
                        continue;
                    }
                    let exposed_bindings = match items {
                        Some(items) => items
                            .iter()
                            .filter(|name| module.effective_exports.iter().any(|item| item == *name))
                            .cloned()
                            .collect(),
                        None => module.effective_exports.clone(),
                    };
                    let exposed_types = match items {
                        Some(items) => items
                            .iter()
                            .filter_map(|name| {
                                module
                                    .effective_types
                                    .get(name)
                                    .map(|origin| (name.clone(), origin.clone()))
                            })
                            .collect(),
                        None => module.effective_types.clone(),
                    };
                    value_bindings.insert(alias_name.clone());
                    let descriptor = ModuleValueExport {
                        module_path: path.clone(),
                        exposed_bindings,
                        exposed_types,
                    };
                    module_exports.insert(alias_name.clone(), descriptor.clone());
                    candidates.insert(alias_name.clone(), descriptor);
                    continue;
                }

                let exposed_names: Vec<&String> = match items {
                    Some(items) => items
                        .iter()
                        .filter(|name| module.effective_exports.iter().any(|item| item == *name))
                        .collect(),
                    None => module
                        .effective_exports
                        .iter()
                        .filter(|name| !name.starts_with('_'))
                        .collect(),
                };
                for name in exposed_names {
                    if value_bindings.contains(name) || functions.contains(name) {
                        continue;
                    }
                    value_bindings.insert(name.clone());
                    if let Some(module_export) = module.effective_module_exports.get(name) {
                        module_exports.insert(name.clone(), module_export.clone());
                        candidates.insert(name.clone(), module_export.clone());
                    }
                }
            }
            StmtKind::Let { name, value, .. } => {
                value_bindings.insert(name.clone());
                if let ExprKind::Ident(source) = &value.kind {
                    if let Some(module_export) = module_exports.get(source).cloned() {
                        module_exports.insert(name.clone(), module_export.clone());
                        candidates.insert(name.clone(), module_export);
                    } else {
                        module_exports.remove(name);
                    }
                } else {
                    module_exports.remove(name);
                }
            }
            StmtKind::FnDecl { name, .. } => {
                functions.insert(name.clone());
            }
            StmtKind::Assign {
                target: AssignTarget::Variable(name),
                value,
                ..
            } => {
                if let ExprKind::Ident(source) = &value.kind {
                    if let Some(module_export) = module_exports.get(source).cloned() {
                        module_exports.insert(name.clone(), module_export.clone());
                        candidates.insert(name.clone(), module_export);
                    } else {
                        module_exports.remove(name);
                    }
                } else {
                    module_exports.remove(name);
                }
            }
            _ => {}
        }
    }

    (module_exports, candidates, value_bindings)
}

fn module_imports_from_exports(
    exports: &BTreeMap<String, ModuleValueExport>,
) -> Vec<ModuleImport> {
    exports
        .iter()
        .map(|(alias, export)| {
            let mut items = export.exposed_bindings.clone();
            for type_name in export.exposed_types.keys() {
                if !items.contains(type_name) {
                    items.push(type_name.clone());
                }
            }
            ModuleImport {
                path: export.module_path.clone(),
                items: Some(items),
                alias: Some(alias.clone()),
                line: 0,
            }
        })
        .collect()
}

fn push_unique(names: &mut Vec<String>, seen: &mut BTreeSet<String>, name: &str) {
    if seen.insert(name.to_string()) {
        names.push(name.to_string());
    }
}

fn collect_imports_in_stmts(stmts: &[Stmt]) -> Vec<ModuleImport> {
    let mut out = Vec::new();
    for stmt in stmts {
        collect_imports_in_stmt(stmt, &mut out);
    }
    out
}

fn collect_top_level_imports(stmts: &[Stmt]) -> Vec<ModuleImport> {
    stmts
        .iter()
        .filter_map(module_import_from_stmt)
        .collect()
}

fn module_import_from_stmt(stmt: &Stmt) -> Option<ModuleImport> {
    let StmtKind::Use { path, items, alias } = &stmt.kind else {
        return None;
    };
    Some(ModuleImport {
        path: path.clone(),
        items: items.clone(),
        alias: alias.clone(),
        line: stmt.line,
    })
}

fn collect_imports_in_stmt(stmt: &Stmt, out: &mut Vec<ModuleImport>) {
    match &stmt.kind {
        StmtKind::Use { .. } => {
            out.push(module_import_from_stmt(stmt).expect("matched use statement"));
        }
        StmtKind::Let { value, .. } => collect_imports_in_expr(value, out),
        StmtKind::Assign { target, value, .. } => {
            collect_imports_in_assign_target(target, out);
            collect_imports_in_expr(value, out);
        }
        StmtKind::If {
            condition,
            body,
            else_ifs,
            else_body,
        } => {
            collect_imports_in_expr(condition, out);
            collect_imports_in_stmts_into(body, out);
            for (condition, body) in else_ifs {
                collect_imports_in_expr(condition, out);
                collect_imports_in_stmts_into(body, out);
            }
            if let Some(body) = else_body {
                collect_imports_in_stmts_into(body, out);
            }
        }
        StmtKind::While { condition, body } => {
            collect_imports_in_expr(condition, out);
            collect_imports_in_stmts_into(body, out);
        }
        StmtKind::Repeat { count, body } => {
            collect_imports_in_expr(count, out);
            collect_imports_in_stmts_into(body, out);
        }
        StmtKind::ForIn { iterable, body, .. } => {
            collect_imports_in_expr(iterable, out);
            collect_imports_in_stmts_into(body, out);
        }
        StmtKind::FnDecl { body, .. } | StmtKind::MethodDecl { body, .. } => {
            collect_imports_in_stmts_into(body, out);
        }
        StmtKind::Return { value } => {
            if let Some(value) = value {
                collect_imports_in_expr(value, out);
            }
        }
        StmtKind::ExprStmt(expr) => collect_imports_in_expr(expr, out),
        StmtKind::Break
        | StmtKind::Continue
        | StmtKind::StructDecl { .. }
        | StmtKind::EnumDecl { .. } => {}
    }
}

fn collect_imports_in_stmts_into(stmts: &[Stmt], out: &mut Vec<ModuleImport>) {
    for stmt in stmts {
        collect_imports_in_stmt(stmt, out);
    }
}

fn collect_imports_in_assign_target(target: &AssignTarget, out: &mut Vec<ModuleImport>) {
    match target {
        AssignTarget::Variable(_) => {}
        AssignTarget::Index { object, index } => {
            collect_imports_in_expr(object, out);
            collect_imports_in_expr(index, out);
        }
        AssignTarget::Field { object, .. } => collect_imports_in_expr(object, out),
    }
}

fn collect_imports_in_expr(expr: &Expr, out: &mut Vec<ModuleImport>) {
    match &expr.kind {
        ExprKind::Int(_)
        | ExprKind::Number(_)
        | ExprKind::Str(_)
        | ExprKind::StringInterp(_)
        | ExprKind::Bool(_)
        | ExprKind::None
        | ExprKind::Ident(_) => {}
        ExprKind::BinaryOp { left, right, .. } => {
            collect_imports_in_expr(left, out);
            collect_imports_in_expr(right, out);
        }
        ExprKind::UnaryOp { expr, .. } | ExprKind::Try(expr) => {
            collect_imports_in_expr(expr, out);
        }
        ExprKind::Call { callee, args } => {
            collect_imports_in_expr(callee, out);
            for arg in args {
                collect_imports_in_expr(arg, out);
            }
        }
        ExprKind::MethodCall { object, args, .. } => {
            collect_imports_in_expr(object, out);
            for arg in args {
                collect_imports_in_expr(arg, out);
            }
        }
        ExprKind::FieldAccess { object, .. } => collect_imports_in_expr(object, out),
        ExprKind::StructConstruct { fields, .. } => {
            for (_, value) in fields {
                collect_imports_in_expr(value, out);
            }
        }
        ExprKind::EnumConstruct { payload, .. } => match payload {
            VariantPayload::Unit => {}
            VariantPayload::Tuple(values) => {
                for value in values {
                    collect_imports_in_expr(value, out);
                }
            }
            VariantPayload::Struct(fields) => {
                for (_, value) in fields {
                    collect_imports_in_expr(value, out);
                }
            }
        },
        ExprKind::Index { object, index } => {
            collect_imports_in_expr(object, out);
            collect_imports_in_expr(index, out);
        }
        ExprKind::Array(values) => {
            for value in values {
                collect_imports_in_expr(value, out);
            }
        }
        ExprKind::Dict(entries) => {
            for (_, value) in entries {
                collect_imports_in_expr(value, out);
            }
        }
        ExprKind::IfExpr {
            condition,
            then_expr,
            else_expr,
        } => {
            collect_imports_in_expr(condition, out);
            collect_imports_in_expr(then_expr, out);
            collect_imports_in_expr(else_expr, out);
        }
        ExprKind::Lambda { body, .. } => collect_imports_in_stmts_into(body, out),
        ExprKind::Match { scrutinee, arms } => {
            collect_imports_in_expr(scrutinee, out);
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    collect_imports_in_expr(guard, out);
                }
                collect_imports_in_expr(&arm.body, out);
            }
        }
    }
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
/// map so aliased `use foo as m` can populate its origin-aware
/// runtime namespace surface for `m.Entity { ... }` etc.
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
#[derive(Clone)]
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

fn callable_assignment_names(stmts: &[Stmt]) -> HashSet<String> {
    fn visit(stmts: &[Stmt], names: &mut HashSet<String>) {
        for stmt in stmts {
            match &stmt.kind {
                StmtKind::Assign {
                    target: AssignTarget::Variable(name),
                    ..
                } => {
                    names.insert(name.clone());
                }
                StmtKind::If {
                    body,
                    else_ifs,
                    else_body,
                    ..
                } => {
                    visit(body, names);
                    for (_, body) in else_ifs {
                        visit(body, names);
                    }
                    if let Some(body) = else_body {
                        visit(body, names);
                    }
                }
                StmtKind::While { body, .. }
                | StmtKind::Repeat { body, .. }
                | StmtKind::ForIn { body, .. } => visit(body, names),
                // Nested named functions and lambdas are separate callable
                // mutation domains and are analysed when emitted.
                _ => {}
            }
        }
    }
    let mut names = HashSet::new();
    visit(stmts, &mut names);
    names
}

// ─── Emitter ───────────────────────────────────────────────────────

#[derive(Default)]
struct EmissionScope {
    locals: HashSet<String>,
    module_alias_locals: HashSet<String>,
    mutated_locals: HashSet<String>,
    plain_glob_imports: HashSet<String>,
}

#[derive(Clone)]
struct ModuleAliasBinding {
    exposed_types: BTreeMap<String, String>,
}

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
    /// collision-free prefix is applied to every user fn name so
    /// modules can't collide on function identifiers.
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
    /// Per-scope maps of module aliases. Each binding retains both
    /// the module path and the type surface selected by the `use`
    /// statement, so `use shapes.{Circle} as s` cannot resolve
    /// `s.Square` during construction or pattern matching. Kept in
    /// lockstep with `type_bindings` so aliases introduced in a fn,
    /// lambda, or block cannot leak into sibling/enclosing scopes.
    module_aliases: Vec<HashMap<String, ModuleAliasBinding>>,
    /// Stack of generated Rust scopes. Each frame owns both its
    /// `let`-bound Bop names and the plain-glob imports that emitted
    /// those bindings. Keeping them together prevents an import in
    /// one module, function, lambda, or block from suppressing the
    /// declarations needed by another. Used for:
    ///
    /// - Ident resolution (local vs top-level fn vs error).
    /// - Free-variable analysis for lambda capture.
    ///
    /// Each block (if / while / repeat / for / fn / lambda) pushes
    /// a fresh set on entry and pops on exit. User-fn and
    /// lambda parameters are inserted into the freshly-pushed set.
    scope_stack: Vec<EmissionScope>,
    /// True while emitting the body of `run_program` (the Rust fn
    /// that returns `Result<(), BopError>`). User fns and lambdas
    /// toggle this off for the duration of their body — inside
    /// them, the enclosing Rust fn returns `Result<Value,
    /// BopError>`, so `return Ok(value)` is the Bop-level return
    /// path. Read by `try`'s codegen to decide whether an `Err`
    /// arm should propagate via `return Ok(...)` (fn body) or
    /// raise a real error (top-level program).
    in_top_level: bool,
    /// True while emitting a named function, method, or lambda body. Combined
    /// with `scope_stack` depth to distinguish a module's direct use-site from
    /// a nested lexical import when applying named-function first-win rules.
    in_callable_body: bool,
    /// Aliased top-level imports owned by the module whose lifted callables are
    /// currently being emitted. Their runtime values are resolved lazily from
    /// `Ctx::module_aliases` because lifted Rust functions cannot capture the
    /// loader's source-ordered locals.
    declaration_aliases: Vec<ModuleImport>,
    /// Per-callable declaration-alias binding names. Each required alias gets
    /// separate call-local overlay and read-cache slots. Reads consult an
    /// assigned overlay first, then the cache and live declaration context;
    /// assignment populates only the overlay for the rest of the invocation.
    declaration_alias_overlays: Vec<HashSet<String>>,
    /// Whole-callable assignment pre-analysis. Namespace aliases assigned on
    /// any control-flow path use runtime type identity even at construction
    /// sites that appear textually before the assignment (for example, on a
    /// later loop iteration).
    callable_mutations: Vec<HashSet<String>>,
    /// Root copy restored after temporarily emitting imported modules/methods.
    root_declaration_aliases: Vec<ModuleImport>,
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
        builtin_frame.insert(
            String::from("Iter"),
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
            module_aliases: vec![HashMap::new()],
            scope_stack: Vec::new(),
            in_top_level: false,
            in_callable_body: false,
            declaration_aliases: Vec::new(),
            declaration_alias_overlays: Vec::new(),
            callable_mutations: Vec::new(),
            root_declaration_aliases: Vec::new(),
        }
    }

    fn push_scope(&mut self) {
        self.scope_stack.push(EmissionScope::default());
        self.type_bindings.push(HashMap::new());
        self.module_aliases.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scope_stack.pop();
        if self.type_bindings.len() > 1 {
            self.type_bindings.pop();
        }
        if self.module_aliases.len() > 1 {
            self.module_aliases.pop();
        }
    }

    /// Resolve a source-level type reference to its declaring
    /// module path, using the emitter's per-scope type_bindings
    /// and module_aliases state. Returns `None` if the name isn't
    /// in scope — callers treat that as a type-not-declared
    /// error at emit time.
    fn resolve_type_module(
        &self,
        namespace: Option<&str>,
        type_name: &str,
    ) -> Option<String> {
        if let Some(ns) = namespace {
            for frame in self.module_aliases.iter().rev() {
                if let Some(binding) = frame.get(ns) {
                    if let Some(origin) = binding.exposed_types.get(type_name) {
                        let key = (origin.clone(), type_name.to_string());
                        if self.types.structs.contains_key(&key)
                            || self.types.enums.contains_key(&key)
                        {
                            return Some(origin.clone());
                        }
                    }
                    return None;
                }
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

    /// Imported types are first-win within one lexical frame. A
    /// new inner frame remains free to shadow an outer binding.
    fn bind_imported_type(&mut self, name: &str, module_path: &str) {
        if let Some(frame) = self.type_bindings.last_mut() {
            frame
                .entry(name.to_string())
                .or_insert_with(|| module_path.to_string());
        }
    }

    fn bind_module_alias(
        &mut self,
        alias: &str,
        exposed_types: &[(String, String)],
    ) {
        if let Some(frame) = self.module_aliases.last_mut() {
            frame.insert(
                alias.to_string(),
                ModuleAliasBinding {
                    exposed_types: exposed_types.iter().cloned().collect(),
                },
            );
        }
    }

    fn bind_module_export(&mut self, name: &str, export: &ModuleValueExport) {
        if let Some(frame) = self.module_aliases.last_mut() {
            frame.insert(
                name.to_string(),
                ModuleAliasBinding {
                    exposed_types: export.exposed_types.clone(),
                },
            );
        }
    }

    fn has_module_alias_candidate(&self, name: &str) -> bool {
        self.module_aliases
            .iter()
            .rev()
            .any(|aliases| aliases.contains_key(name))
    }

    /// Emit Rust source for a `__resolver` closure that turns
    /// `(namespace, type_name)` pairs into the declaring
    /// module's path. Bare names consult the generated runtime
    /// binding stack so lifted bodies observe declarations and
    /// imports only after execution reaches them. Namespaces
    /// continue to resolve through live module values.
    fn emit_resolver_closure_src(
        &self,
        pattern_namespaces: &[String],
        resolve_bare: bool,
    ) -> String {
        // Module aliases resolve from runtime Values. A local/parameter binding
        // hard-shadows declaration context; otherwise a lifted callable checks
        // the defining module's source-ordered alias map.
        let mut alias_arms = String::new();
        let mut alias_prelude = String::new();
        let mut aliases: Vec<(&String, &ModuleAliasBinding)> = Vec::new();
        let mut seen_aliases: HashSet<String> = HashSet::new();
        for frame in self.module_aliases.iter().rev() {
            let mut frame_aliases: Vec<(&String, &ModuleAliasBinding)> =
                frame.iter().collect();
            frame_aliases.sort_by(|a, b| a.0.cmp(b.0));
            for (alias, binding) in frame_aliases {
                if seen_aliases.insert(alias.clone()) {
                    aliases.push((alias, binding));
                }
            }
        }
        aliases.sort_by(|a, b| a.0.cmp(b.0));
        for (alias, _) in aliases {
            let value = if self.is_local(alias) {
                format!("::std::option::Option::Some(&{})", rust_user_ident(alias))
            } else if let Some(overlay) = self.declaration_alias_overlay(alias) {
                let snapshot = declaration_alias_pattern_snapshot_ident(alias);
                alias_prelude.push_str(&format!(
                    "            let {snapshot} = __bop_declaration_alias_optional(ctx, &mut {overlay}, {module}, {alias});\n",
                    module = rust_string_literal(&self.current_module),
                    alias = rust_string_literal(alias),
                ));
                format!(
                    "{snapshot}.as_ref()",
                )
            } else if self
                .declaration_aliases
                .iter()
                .any(|import| import.alias.as_deref() == Some(alias.as_str()))
            {
                format!(
                    "ctx.module_aliases.get(&({module}.to_string(), {alias}.to_string()))",
                    module = rust_string_literal(&self.current_module),
                    alias = rust_string_literal(alias),
                )
            } else {
                "::std::option::Option::None".to_string()
            };
            alias_arms.push_str(&format!(
                "                        (::std::option::Option::Some({alias}), __tn) => match {value} {{ ::std::option::Option::Some(::bop::value::Value::Module(__module)) => __module.type_origin(__tn).map(str::to_string), _ => ::std::option::Option::None }},\n",
                alias = rust_string_literal(alias),
                value = value,
            ));
        }
        let mut dynamic_namespaces = pattern_namespaces.to_vec();
        dynamic_namespaces.sort();
        dynamic_namespaces.dedup();
        for namespace in dynamic_namespaces {
            if seen_aliases.contains(&namespace) || !self.is_local(&namespace) {
                continue;
            }
            alias_arms.push_str(&format!(
                "                        (::std::option::Option::Some({namespace}), __tn) => match ::std::option::Option::Some(&{value}) {{ ::std::option::Option::Some(::bop::value::Value::Module(__module)) => __module.type_origin(__tn).map(str::to_string), _ => ::std::option::Option::None }},\n",
                namespace = rust_string_literal(&namespace),
                value = rust_user_ident(&namespace),
            ));
        }
        let bare_arm = if resolve_bare {
            "                        (::std::option::Option::None, __tn) => __bop_type_bindings.iter().rev().find_map(|__frame| __frame.get(__tn).cloned()),\n"
        } else {
            ""
        };
        format!(
            "{prelude}            let __resolver = |__ns: ::std::option::Option<&str>, __tn: &str| -> ::std::option::Option<String> {{\n\
             \x20                   match (__ns, __tn) {{\n\
             {alias}{bare}                        _ => ::std::option::Option::None,\n\
             \x20                   }}\n\
             \x20           }};\n",
            alias = alias_arms,
            bare = bare_arm,
            prelude = alias_prelude,
        )
    }

    fn bind_local(&mut self, name: &str) {
        if let Some(top) = self.scope_stack.last_mut() {
            top.locals.insert(name.to_string());
        }
    }

    fn emit_module_alias_context_sync(&mut self, name: &str) {
        if !self.is_module_top_scope() {
            return;
        }
        self.line(&format!(
            "if matches!(&{value}, ::bop::value::Value::Module(_)) {{ ctx.module_aliases.insert(({module}.to_string(), {name}.to_string()), {value}.clone()); }} else {{ ctx.module_aliases.remove(&({module}.to_string(), {name}.to_string())); }}",
            value = rust_user_ident(name),
            module = rust_string_literal(&self.current_module),
            name = rust_string_literal(name),
        ));
    }

    fn emit_runtime_type_scope(&mut self) {
        self.line("let mut __bop_type_bindings = __bop_type_bindings.clone();");
        self.line("__bop_type_bindings.push(__BopTypeFrame::new());");
    }

    fn statements_bind_runtime_types(&self, stmts: &[Stmt]) -> bool {
        stmts.iter().any(|stmt| match &stmt.kind {
            StmtKind::StructDecl { .. } | StmtKind::EnumDecl { .. } => true,
            StmtKind::Use {
                path,
                items,
                alias: None,
            } => !self
                .imported_type_names(path, items.as_deref(), false)
                .is_empty(),
            _ => false,
        })
    }

    fn statements_use_runtime_type_bindings(&self, stmts: &[Stmt]) -> bool {
        stmts.iter().any(|stmt| match &stmt.kind {
            StmtKind::Let { value, .. } => expr_uses_runtime_type_bindings(value),
            StmtKind::Assign { target, value, .. } => {
                let target_uses_types = match target {
                    AssignTarget::Variable(_) => false,
                    AssignTarget::Index { object, index } => {
                        expr_uses_runtime_type_bindings(object)
                            || expr_uses_runtime_type_bindings(index)
                    }
                    AssignTarget::Field { object, .. } => {
                        expr_uses_runtime_type_bindings(object)
                    }
                };
                target_uses_types || expr_uses_runtime_type_bindings(value)
            }
            StmtKind::If {
                condition,
                body,
                else_ifs,
                else_body,
            } => {
                expr_uses_runtime_type_bindings(condition)
                    || self.statements_use_runtime_type_bindings(body)
                    || else_ifs.iter().any(|(condition, body)| {
                        expr_uses_runtime_type_bindings(condition)
                            || self.statements_use_runtime_type_bindings(body)
                    })
                    || else_body
                        .as_deref()
                        .is_some_and(|body| self.statements_use_runtime_type_bindings(body))
            }
            StmtKind::While { condition, body } => {
                expr_uses_runtime_type_bindings(condition)
                    || self.statements_use_runtime_type_bindings(body)
            }
            StmtKind::Repeat { count, body } => {
                expr_uses_runtime_type_bindings(count)
                    || self.statements_use_runtime_type_bindings(body)
            }
            StmtKind::ForIn { iterable, body, .. } => {
                expr_uses_runtime_type_bindings(iterable)
                    || self.statements_use_runtime_type_bindings(body)
            }
            // Nested named functions own their call-time context.
            StmtKind::FnDecl { .. } | StmtKind::MethodDecl { .. } => false,
            StmtKind::Return { value } => value
                .as_ref()
                .is_some_and(expr_uses_runtime_type_bindings),
            StmtKind::Break | StmtKind::Continue => false,
            StmtKind::Use {
                path,
                items,
                alias: None,
            } => !self
                .imported_type_names(path, items.as_deref(), false)
                .is_empty(),
            StmtKind::Use { alias: Some(_), .. } => false,
            StmtKind::StructDecl { .. } | StmtKind::EnumDecl { .. } => true,
            StmtKind::ExprStmt(expr) => expr_uses_runtime_type_bindings(expr),
        })
    }

    fn emit_runtime_type_scope_for(&mut self, stmts: &[Stmt]) {
        if self.statements_bind_runtime_types(stmts) {
            self.emit_runtime_type_scope();
        }
    }

    fn emit_type_context_publish(&mut self) {
        if !self.is_module_top_scope() {
            return;
        }
        self.line(&format!(
            "__bop_publish_type_bindings(ctx, {}, &__bop_type_bindings);",
            rust_string_literal(&self.current_module),
        ));
    }

    fn is_local(&self, name: &str) -> bool {
        self.scope_stack
            .iter()
            .rev()
            .any(|scope| scope.locals.contains(name))
    }

    fn is_local_in_current_scope(&self, name: &str) -> bool {
        self.scope_stack
            .last()
            .is_some_and(|scope| scope.locals.contains(name))
    }

    fn is_dynamic_namespace_local(&self, name: &str) -> bool {
        for scope in self.scope_stack.iter().rev() {
            if scope.locals.contains(name) {
                return !scope.module_alias_locals.contains(name)
                    || scope.mutated_locals.contains(name)
                    || self
                        .callable_mutations
                        .last()
                        .is_some_and(|names| names.contains(name));
            }
        }
        self.declaration_alias_overlay(name).is_some()
    }

    fn mark_local_mutated(&mut self, name: &str) {
        for scope in self.scope_stack.iter_mut().rev() {
            if scope.locals.contains(name) {
                scope.mutated_locals.insert(name.to_string());
                break;
            }
        }
    }

    fn declaration_alias_overlay(&self, name: &str) -> Option<String> {
        self.declaration_alias_overlays
            .last()
            .filter(|aliases| aliases.contains(name))
            .map(|_| declaration_alias_overlay_ident(name))
    }

    fn declaration_alias_read_src(&self, name: &str, line: u32) -> Option<String> {
        self.declaration_alias_overlay(name).map(|overlay| {
            format!(
                "__bop_declaration_alias_read(ctx, &mut {overlay}, {module}, {alias}, {line})?",
                module = rust_string_literal(&self.current_module),
                alias = rust_string_literal(name),
            )
        })
    }

    fn declaration_alias_namespace_src(&self, name: &str, line: u32) -> Option<String> {
        self.declaration_alias_overlay(name).map(|overlay| {
            format!(
                "__bop_declaration_alias_namespace(ctx, &mut {overlay}, {module}, {alias}, {line})?",
                module = rust_string_literal(&self.current_module),
                alias = rust_string_literal(name),
            )
        })
    }

    fn declaration_alias_optional_src(&self, name: &str) -> Option<String> {
        self.declaration_alias_overlay(name).map(|overlay| {
            format!(
                "__bop_declaration_alias_optional(ctx, &mut {overlay}, {module}, {alias})",
                module = rust_string_literal(&self.current_module),
                alias = rust_string_literal(name),
            )
        })
    }

    fn declaration_alias_mut_src(&self, name: &str, line: u32) -> Option<String> {
        self.declaration_alias_overlay(name).map(|overlay| {
            format!(
                "__bop_declaration_alias_mut(&mut {overlay}, {alias}, {line})?",
                alias = rust_string_literal(name),
            )
        })
    }

    fn declaration_alias_assign_src(
        &self,
        name: &str,
        value: &str,
        line: u32,
    ) -> Option<String> {
        self.declaration_alias_overlay(name).map(|overlay| {
            format!(
                "__bop_declaration_alias_assign(ctx, &mut {overlay}, {value}, {module}, {alias}, {line})?;",
                module = rust_string_literal(&self.current_module),
                alias = rust_string_literal(name),
            )
        })
    }

    fn ident_value_src(&self, name: &str, line: u32) -> Result<String, BopError> {
        if self.is_local(name) {
            Ok(format!("{}.clone()", rust_user_ident(name)))
        } else if let Some(alias) = self.declaration_alias_read_src(name, line) {
            Ok(alias)
        } else if self.fn_info.top_level_fns.contains(name) {
            Ok(format!("{}({})?", self.wrapper_fn_name(name), line))
        } else if self.fn_info.all_fns.contains_key(name) {
            Err(BopError::runtime(
                format!(
                    "bop-compile: nested function `{}` can't be used as a first-class value (only top-level fns are currently wrappable)",
                    name
                ),
                line,
            ))
        } else {
            Ok(format!("{}.clone()", rust_user_ident(name)))
        }
    }

    fn is_module_top_scope(&self) -> bool {
        !self.in_callable_body && self.scope_stack.len() == 1
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
                let exposed_types =
                    self.imported_type_names(path, items.as_deref(), alias.is_some());
                if let Some(alias_name) = alias {
                    self.bind_module_alias(alias_name, &exposed_types);
                } else {
                    for (name, origin) in exposed_types {
                        self.bind_imported_type(&name, &origin);
                    }
                }
            }
        }
    }

    fn seed_module_alias_candidates(
        &mut self,
        candidates: &BTreeMap<String, ModuleValueExport>,
    ) {
        if let Some(frame) = self.module_aliases.last_mut() {
            frame.clear();
        }
        for (name, export) in candidates {
            self.bind_module_export(name, export);
        }
    }

    fn declaration_aliases_for_callable(
        &self,
        params: &[String],
        body: &[Stmt],
    ) -> Vec<String> {
        let mut outer = EmissionScope::default();
        for import in &self.declaration_aliases {
            if let Some(alias) = &import.alias {
                outer.locals.insert(alias.clone());
            }
        }
        let mut known: HashSet<String> = params.iter().cloned().collect();
        let mut referenced = FreeVarDependencies::for_declaration_aliases();
        scan_free_vars_stmts(
            body,
            &mut known,
            &mut referenced,
            core::slice::from_ref(&outer),
            &self.fn_info,
        );
        referenced.required.extend(referenced.pattern_namespaces);
        let mut aliases = Vec::new();
        for import in &self.declaration_aliases {
            let Some(alias) = &import.alias else {
                continue;
            };
            if !referenced.required.contains(alias) || self.is_local(alias) {
                continue;
            }
            aliases.push(alias.clone());
        }
        aliases
    }

    fn emit_declaration_alias_overlays(
        &mut self,
        aliases: &[String],
        initializers: &HashMap<String, String>,
    ) -> HashSet<String> {
        let mut overlays = HashSet::new();
        for alias in aliases {
            overlays.insert(alias.clone());
            let initializer = initializers
                .get(alias)
                .map(String::as_str)
                .unwrap_or("::std::option::Option::None");
            self.line(&format!(
                "let mut {overlay} = __BopDeclarationAliasBinding {{ overlay: {initializer}, cached: ::std::option::Option::None }};",
                overlay = declaration_alias_overlay_ident(alias),
            ));
        }
        overlays
    }

    fn imported_type_names(
        &self,
        module_path: &str,
        items: Option<&[String]>,
        aliased: bool,
    ) -> Vec<(String, String)> {
        let Some(entry) = self.modules.modules.get(module_path) else {
            return Vec::new();
        };
        match items {
            Some(items) => items
                .iter()
                .filter_map(|name| {
                    entry
                        .effective_types
                        .get(name)
                        .map(|origin| (name.clone(), origin.clone()))
                })
                .collect(),
            None => entry
                .effective_types
                .iter()
                .filter(|(name, _)| aliased || !name.starts_with('_'))
                .map(|(name, origin)| (name.clone(), origin.clone()))
                .collect(),
        }
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
        let (_, root_alias_candidates, _) =
            effective_module_value_exports(stmts, &self.modules.modules);
        self.root_declaration_aliases = module_imports_from_exports(&root_alias_candidates);
        self.declaration_aliases = self.root_declaration_aliases.clone();
        // Seed the outermost type_bindings frame with the
        // root program's declared types so methods + top-level
        // fns (emitted ahead of the main AST walk) can resolve
        // bare type names in their bodies. Same pre-pass for
        // `use` statements so aliases + selective-type imports
        // are visible to the fn bodies we're about to emit.
        self.seed_types_for_module(bop::value::ROOT_MODULE_PATH);
        self.seed_uses(stmts);
        self.seed_module_alias_candidates(&root_alias_candidates);
        // Imported modules emit first. Eager top-level edges are
        // topo-ordered leaves-first; lazy nested edges need no Rust
        // item ordering but are present in the same resolved graph.
        self.emit_imported_modules()?;
        // Top-level fn declarations move out of `run_program`'s
        // body to module scope. That way the `__bop_user_fn_value_*`
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
            module_fn_prefix(name),
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
        let saved_aliases = std::mem::replace(
            &mut self.module_aliases,
            vec![HashMap::new()],
        );
        let saved_declaration_aliases = std::mem::replace(
            &mut self.declaration_aliases,
            module_imports_from_exports(&entry.module_alias_candidates),
        );
        // Seed this module's types too — see
        // `seed_types_for_module` for why this matters. Methods
        // inside the module need to resolve bare type names
        // before the AST walker gets to the decls. Same treatment
        // for the module's own `use` statements.
        self.seed_types_for_module(name);
        self.seed_uses(&entry.ast);
        self.seed_module_alias_candidates(&entry.module_alias_candidates);

        self.emit_top_level_fn_decls(&entry.ast)?;
        self.emit_fn_value_wrappers();
        self.emit_module_exports_struct(name, entry);
        self.emit_module_load_fn(name, entry)?;

        self.fn_info = saved_fn_info;
        self.module_prefix = saved_prefix;
        self.current_module = saved_module;
        self.type_bindings = saved_bindings;
        self.module_aliases = saved_aliases;
        self.declaration_aliases = saved_declaration_aliases;
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
                ident = rust_user_ident(export)
            )
            .unwrap();
        }
        self.out.push_str("}\n\n");
    }

    /// Emit a `use` statement in one of four forms, loading the
    /// module once (via its `__mod_*_load` fn — which handles
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
        let module_top_scope = self.is_module_top_scope();
        // Idempotency: only plain-glob re-imports are cached. The
        // other three forms can legitimately produce different
        // visible effects (different item subset, different alias
        // binding) when re-run in the same scope, so we always
        // re-emit them.
        let already_imported_here = self
            .scope_stack
            .last()
            .is_some_and(|scope| scope.plain_glob_imports.contains(path));
        if items.is_none() && alias.is_none() && already_imported_here {
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
                let is_type = entry.effective_types.contains_key(item);
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
                .filter(|n| alias.is_some() || !n.starts_with('_'))
                .cloned()
                .collect(),
        };
        let expose_types: Vec<(String, String)> = match items {
            Some(list) => list
                .iter()
                .filter_map(|name| {
                    entry
                        .effective_types
                        .get(name)
                        .map(|origin| (name.clone(), origin.clone()))
                })
                .collect(),
            None => entry
                .effective_types
                .iter()
                .filter(|(name, _)| alias.is_some() || !name.starts_with('_'))
                .map(|(name, origin)| (name.clone(), origin.clone()))
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
                            ident = rust_user_ident(n)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let types_src: String = expose_types
                    .iter()
                    .map(|(type_name, origin)| {
                        format!(
                            "({}.to_string(), {}.to_string())",
                            rust_string_literal(type_name),
                            rust_string_literal(origin),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                if self.is_local_in_current_scope(alias_name)
                    || (self.fn_info.all_fns.contains_key(alias_name)
                        && !self.has_module_alias_candidate(alias_name))
                {
                    return Err(BopError::runtime(
                        format!(
                            "Alias `{}` in `use {} as {}` would shadow an existing binding",
                            alias_name, path, alias_name
                        ),
                        line,
                    ));
                }
                self.line(&format!(
                    "let mut {alias}: ::bop::value::Value = ::bop::value::Value::new_module_with_type_exports({path_lit}.to_string(), ::std::vec![{bindings}], ::bop::value::BopTypeExports::from_origins(::std::vec![{types}]), {line})?;",
                    alias = rust_user_ident(alias_name),
                    path_lit = rust_string_literal(path),
                    bindings = bindings_src,
                    types = types_src,
                    line = line,
                ));
                if module_top_scope {
                    self.line(&format!(
                        "ctx.module_aliases.insert(({module}.to_string(), {alias_name}.to_string()), {alias}.clone());",
                        module = rust_string_literal(&self.current_module),
                        alias_name = rust_string_literal(alias_name),
                        alias = rust_user_ident(alias_name),
                    ));
                }
                self.bind_local(alias_name);
                self.scope_stack
                    .last_mut()
                    .expect("use emission has a scope")
                    .module_alias_locals
                    .insert(alias_name.clone());
                // Track the alias for compile-time resolution
                // of namespaced references (`m.Color`). Emit-
                // time resolution is sufficient here because
                // the AOT bakes module_path literals directly
                // into construction + match sites.
                self.bind_module_alias(alias_name, &expose_types);
            }
            None => {
                // Non-aliased: inject each binding as a local.
                // Types don't need a Rust local (they're
                // compile-time metadata in the AOT), but they
                // do need a `type_bindings` entry so
                // construction + pattern sites can resolve
                // the bare name to the right module path.
                for name in &expose_bindings {
                    // Every non-aliased import form is first-win in
                    // the current frame. An outer-frame binding is
                    // not a clash: this new Rust block should shadow
                    // it just like the walker and VM value scopes.
                    if self.is_local_in_current_scope(name)
                        || (self.is_module_top_scope()
                            && self.fn_info.all_fns.contains_key(name)
                            && !self.has_module_alias_candidate(name))
                    {
                        continue;
                    }
                    self.line(&format!(
                        "let mut {ident}: ::bop::value::Value = {tmp}.{ident}.clone();",
                        ident = rust_user_ident(name),
                        tmp = tmp
                    ));
                    self.bind_local(name);
                    if let Some(module_export) = entry.effective_module_exports.get(name) {
                        self.bind_module_export(name, module_export);
                        self.scope_stack
                            .last_mut()
                            .expect("use emission has a scope")
                            .module_alias_locals
                            .insert(name.clone());
                        if module_top_scope {
                            self.emit_module_alias_context_sync(name);
                        }
                    }
                }
                for (type_name, origin) in &expose_types {
                    self.line(&format!(
                        "__bop_bind_imported_type(&mut __bop_type_bindings, {}, {});",
                        rust_string_literal(type_name),
                        rust_string_literal(origin),
                    ));
                    self.bind_imported_type(type_name, origin);
                }
                if !expose_types.is_empty() {
                    self.emit_type_context_publish();
                }
                if items.is_none() {
                    self.scope_stack
                        .last_mut()
                        .expect("use statements are emitted inside a scope")
                        .plain_glob_imports
                        .insert(path.to_string());
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
        self.line(&format!(
            "ctx.module_aliases.retain(|(module, _), _| module != {});",
            rust_string_literal(name)
        ));
        self.line(&format!(
            "let __saved_type_bindings = ctx.module_type_bindings.get({}).cloned();",
            rust_string_literal(name)
        ));
        self.line("let mut __bop_type_bindings = __bop_fresh_module_type_bindings();");
        self.line(&format!(
            "__bop_publish_type_bindings(ctx, {}, &__bop_type_bindings);",
            rust_string_literal(name)
        ));
        self.line(&format!(
            "let __load_result = (|| -> Result<{exports}, ::bop::error::BopError> {{",
            exports = exports
        ));
        self.indent += 1;

        // Sandbox gets a tick at module entry too — same checkpoint
        // as any fn entry.
        self.emit_tick(0);

        // Body: emit the module's statements, skipping top-level
        // fn decls (already emitted) but handling imports /
        // lets / everything else. Track a fresh scope so the
        // emitter's Ident lookup resolves within the module.
        self.callable_mutations
            .push(callable_assignment_names(&entry.ast));
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
            if entry.own_fns.contains_key(export)
                && !entry.effective_value_exports.contains(export)
            {
                writeln!(
                    self.out,
                    "    {ident}: {wrapper}(0)?,",
                    ident = rust_user_ident(export),
                    wrapper = self.wrapper_fn_name(export)
                )
                .unwrap();
            } else {
                writeln!(
                    self.out,
                    "    {ident}: {ident}.clone(),",
                    ident = rust_user_ident(export)
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
        self.callable_mutations.pop();
        self.indent -= 1;
        self.line("})();");
        self.line("if __load_result.is_err() {");
        self.indent += 1;
        self.line(&format!(
            "ctx.module_cache.remove({});",
            rust_string_literal(name)
        ));
        self.line(&format!(
            "ctx.module_aliases.retain(|(module, _), _| module != {});",
            rust_string_literal(name)
        ));
        self.line("match __saved_type_bindings {");
        self.indent += 1;
        self.line(&format!(
            "::std::option::Option::Some(__bindings) => {{ ctx.module_type_bindings.insert({}.to_string(), __bindings); }}",
            rust_string_literal(name)
        ));
        self.line(&format!(
            "::std::option::Option::None => {{ ctx.module_type_bindings.remove({}); }}",
            rust_string_literal(name)
        ));
        self.indent -= 1;
        self.line("}");
        self.indent -= 1;
        self.line("}");
        self.line("__load_result");
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
                "fn {wrapper}(line: u32) -> Result<::bop::value::Value, ::bop::error::BopError> {{",
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
                "    __bop_wrap_callable(vec![{params}], ::std::vec::Vec::new(), Some(\"{name}\".to_string()), 0u16, line, callable)",
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
        for ((module_path, type_name, method_name), entry) in &entries {
            let method_fn_name = rust_fn_name_with(
                &method_fn_prefix(&entry.module_prefix, type_name),
                method_name,
            );
            let saved_prefix = std::mem::replace(
                &mut self.module_prefix,
                entry.module_prefix.clone(),
            );
            let saved_module_context = if module_path == bop::value::ROOT_MODULE_PATH {
                None
            } else {
                let module_entry = self
                    .modules
                    .modules
                    .get(module_path)
                    .expect("method's declaring module must be in the module graph")
                    .clone();
                let module_ast = module_entry.ast.clone();
                let saved_module = std::mem::replace(
                    &mut self.current_module,
                    module_path.clone(),
                );
                let mut builtin_types = HashMap::new();
                for name in ["Result", "RuntimeError", "Iter"] {
                    builtin_types.insert(
                        name.to_string(),
                        bop::value::BUILTIN_MODULE_PATH.to_string(),
                    );
                }
                let saved_type_bindings = std::mem::replace(
                    &mut self.type_bindings,
                    vec![builtin_types],
                );
                let saved_module_aliases = std::mem::replace(
                    &mut self.module_aliases,
                    vec![HashMap::new()],
                );
                let saved_declaration_aliases = std::mem::replace(
                    &mut self.declaration_aliases,
                    module_imports_from_exports(&module_entry.module_alias_candidates),
                );
                self.seed_types_for_module(module_path);
                self.seed_uses(&module_ast);
                self.seed_module_alias_candidates(&module_entry.module_alias_candidates);
                Some((
                    saved_module,
                    saved_type_bindings,
                    saved_module_aliases,
                    saved_declaration_aliases,
                ))
            };
            let mut method_fn_info = collect_fn_info(&entry.body);
            // The method's Rust symbol stays type-qualified, while calls in
            // its body resolve against the declaring module's functions.
            let module_fn_info = if module_path == bop::value::ROOT_MODULE_PATH {
                self.fn_info.clone()
            } else {
                let module = self
                    .modules
                    .modules
                    .get(module_path)
                    .expect("method's declaring module must be in the module graph");
                collect_fn_info(&module.ast)
            };
            for (name, params) in module_fn_info.all_fns {
                method_fn_info.all_fns.entry(name).or_insert(params);
            }
            method_fn_info
                .top_level_fns
                .extend(module_fn_info.top_level_fns);
            let saved_fn_info = std::mem::replace(
                &mut self.fn_info,
                method_fn_info,
            );
            self.emit_fn_decl_as(&method_fn_name, &entry.params, &entry.body, 0)?;
            self.fn_info = saved_fn_info;
            self.module_prefix = saved_prefix;
            if let Some((module, type_bindings, module_aliases, declaration_aliases)) =
                saved_module_context
            {
                self.current_module = module;
                self.type_bindings = type_bindings;
                self.module_aliases = module_aliases;
                self.declaration_aliases = declaration_aliases;
            }
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
            let method_prefix = method_fn_prefix(&entry.module_prefix, type_name);
            let fn_name = rust_fn_name_with(&method_prefix, method_name);
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
        self.line("let mut __bop_type_bindings = __bop_fresh_module_type_bindings();");
        self.line(&format!(
            "__bop_publish_type_bindings(ctx, {}, &__bop_type_bindings);",
            rust_string_literal(bop::value::ROOT_MODULE_PATH),
        ));
        self.callable_mutations
            .push(callable_assignment_names(stmts));
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
        self.callable_mutations.pop();
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
                let ident = rust_user_ident(name);
                self.line(&format!("let mut {}: ::bop::value::Value = {};", ident, rhs));
                self.bind_local(name);
                if let Some(scope) = self.scope_stack.last_mut() {
                    scope.module_alias_locals.remove(name);
                }
                self.emit_module_alias_context_sync(name);
            }

            StmtKind::Assign { target, op, value } => {
                self.emit_assign(target, op, value, line)?;
                if let AssignTarget::Variable(name) = target {
                    self.emit_module_alias_context_sync(name);
                }
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
                self.emit_runtime_type_scope_for(body);
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
                self.emit_runtime_type_scope_for(body);
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
                // Bind the iterable into a tmp before calling
                // `__bop_iter_start`, otherwise expressions like
                // `range(5)` — which take `&mut ctx.rand_state`
                // internally — would collide with the helper's
                // own `&mut Ctx` borrow.
                let iter_src = self.expr_src(iterable)?;
                let iter_tmp = self.fresh_tmp();
                self.line(&format!(
                    "let {}: ::bop::value::Value = {};",
                    iter_tmp, iter_src
                ));
                let state_tmp = self.fresh_tmp();
                // `__bop_iter_start` chooses between the eager
                // fast path (Array/Str/Dict) and the protocol
                // (Value::Iter or user type with `.iter()`). The
                // corresponding `__bop_iter_step` advances
                // whichever shape got picked, so the emitted
                // loop body stays uniform.
                self.line(&format!(
                    "let mut {}: __BopIterState = __bop_iter_start(ctx, {}, {})?;",
                    state_tmp, iter_tmp, line
                ));
                let ident = rust_user_ident(var);
                self.open_block("loop");
                self.emit_tick(line);
                self.line(&format!(
                    "let mut {}: ::bop::value::Value = match __bop_iter_step(ctx, &mut {}, {})? {{ Some(__v) => __v, None => break, }};",
                    ident, state_tmp, line
                ));
                self.emit_runtime_type_scope_for(body);
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
                // The whole-program registry owns static shape metadata, while
                // this runtime transition controls source-ordered availability.
                let mp = self.current_module.clone();
                self.line(&format!(
                    "__bop_bind_type(&mut __bop_type_bindings, {}, {});",
                    rust_string_literal(name),
                    rust_string_literal(&mp),
                ));
                self.bind_type(name, &mp);
                self.emit_type_context_publish();
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
        self.emit_runtime_type_scope_for(body);
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
            self.emit_runtime_type_scope_for(elif_body);
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
            self.emit_runtime_type_scope_for(else_body);
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
                let ident = rust_user_ident(name);
                let rhs_src = self.expr_src(value)?;
                if !self.is_local(name) {
                    if let Some(overlay) = self.declaration_alias_overlay(name) {
                        let rhs_tmp = self.fresh_tmp();
                        self.line(&format!("let {} = {};", rhs_tmp, rhs_src));
                        match op {
                            AssignOp::Eq => {
                                let assign = self
                                    .declaration_alias_assign_src(name, &rhs_tmp, line)
                                    .expect("overlay checked above");
                                self.line(&assign);
                            }
                            compound => {
                                let current_tmp = self.fresh_tmp();
                                let current = self
                                    .declaration_alias_read_src(name, line)
                                    .expect("overlay checked above");
                                self.line(&format!(
                                    "let {} = {};",
                                    current_tmp, current
                                ));
                                self.line(&format!(
                                    "{}.overlay = ::std::option::Option::Some({}(&{}, &{}, {})?);",
                                    overlay,
                                    compound_op_path(*compound),
                                    current_tmp,
                                    rhs_tmp,
                                    line
                                ));
                            }
                        }
                        return Ok(());
                    }
                }
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
                self.mark_local_mutated(name);
                Ok(())
            }
            AssignTarget::Index { object, index } => {
                // Tree-walker requires the object to be a bare ident;
                // anything else is a compile-time error here too.
                let target_name = match &object.kind {
                    ExprKind::Ident(n) => n,
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
                let (target_read, target_write) = if !self.is_local(target_name) {
                    if let Some(target_src) =
                        self.declaration_alias_mut_src(target_name, line)
                    {
                        let target_tmp = self.fresh_tmp();
                        self.line(&format!(
                            "let {}: &mut ::bop::value::Value = {};",
                            target_tmp, target_src
                        ));
                        (format!("&*{}", target_tmp), format!("&mut *{}", target_tmp))
                    } else {
                        let target = rust_user_ident(target_name);
                        (format!("&{}", target), format!("&mut {}", target))
                    }
                } else {
                    let target = rust_user_ident(target_name);
                    (format!("&{}", target), format!("&mut {}", target))
                };
                match op {
                    AssignOp::Eq => {
                        self.line(&format!(
                            "::bop::ops::index_set({}, &{}, {}, {})?;",
                            target_write, idx_tmp, val_tmp, line
                        ));
                    }
                    compound => {
                        let op_path = compound_op_path(*compound);
                        let cur_tmp = self.fresh_tmp();
                        let new_tmp = self.fresh_tmp();
                        self.line(&format!(
                            "let {} = ::bop::ops::index_get({}, &{}, {})?;",
                            cur_tmp, target_read, idx_tmp, line
                        ));
                        self.line(&format!(
                            "let {} = {}(&{}, &{}, {})?;",
                            new_tmp, op_path, cur_tmp, val_tmp, line
                        ));
                        self.line(&format!(
                            "::bop::ops::index_set({}, &{}, {}, {})?;",
                            target_write, idx_tmp, new_tmp, line
                        ));
                    }
                }
                Ok(())
            }
            AssignTarget::Field { object, field } => {
                let target_name = match &object.kind {
                    ExprKind::Ident(n) => n,
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
                let target_tmp = self.fresh_tmp();
                let target_src = if self.is_local(target_name) {
                    format!("&mut {}", rust_user_ident(target_name))
                } else {
                    self.declaration_alias_mut_src(target_name, line)
                        .unwrap_or_else(|| format!("&mut {}", rust_user_ident(target_name)))
                };
                self.line(&format!(
                    "let {}: &mut ::bop::value::Value = {};",
                    target_tmp, target_src
                ));
                match op {
                    AssignOp::Eq => {
                        let old_tmp = self.fresh_tmp();
                        let new_tmp = self.fresh_tmp();
                        self.line(&format!(
                            "let {} = ::core::mem::replace(&mut *{}, ::bop::value::Value::None);",
                            old_tmp, target_tmp
                        ));
                        self.line(&format!(
                            "let {} = __bop_field_set({}, {}, {}, {})?;",
                            new_tmp,
                            old_tmp,
                            rust_string_literal(field),
                            val_tmp,
                            line
                        ));
                        self.line(&format!("*{} = {};", target_tmp, new_tmp));
                    }
                    compound => {
                        let op_path = compound_op_path(*compound);
                        let cur_tmp = self.fresh_tmp();
                        let new_tmp = self.fresh_tmp();
                        self.line(&format!(
                            "let {} = __bop_field_get(&*{}, {}, {})?;",
                            cur_tmp,
                            target_tmp,
                            rust_string_literal(field),
                            line
                        ));
                        self.line(&format!(
                            "let {} = {}(&{}, &{}, {})?;",
                            new_tmp, op_path, cur_tmp, val_tmp, line
                        ));
                        let old_tmp = self.fresh_tmp();
                        let replaced_tmp = self.fresh_tmp();
                        self.line(&format!(
                            "let {} = ::core::mem::replace(&mut *{}, ::bop::value::Value::None);",
                            old_tmp, target_tmp
                        ));
                        self.line(&format!(
                            "let {} = __bop_field_set({}, {}, {}, {})?;",
                            replaced_tmp,
                            old_tmp,
                            rust_string_literal(field),
                            new_tmp,
                            line
                        ));
                        self.line(&format!("*{} = {};", target_tmp, replaced_tmp));
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
        self.emit_fn_decl_as(&fn_name, params, body, line)
    }

    fn emit_fn_decl_as(
        &mut self,
        fn_name: &str,
        params: &[String],
        body: &[Stmt],
        line: u32,
    ) -> Result<(), BopError> {
        let uses_runtime_type_bindings = self.statements_use_runtime_type_bindings(body);
        let param_list = params
            .iter()
            .enumerate()
            .map(|(index, _)| format!("__bop_param_{index}: ::bop::value::Value"))
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
        let saved_callable_body = self.in_callable_body;
        self.in_callable_body = true;
        // Function-entry checkpoint — matches the plan's "step-count
        // checks at loop backedges / function entry". No-op outside
        // sandbox mode.
        self.emit_tick(line);
        if uses_runtime_type_bindings {
            self.line(&format!(
                "let mut __bop_type_bindings = __bop_callable_type_bindings(ctx, {});",
                rust_string_literal(&self.current_module),
            ));
        }
        // Rust item functions cannot capture locals from `run_program` or a
        // module loader. Give each alias required by this callable or one of
        // its descendant lambdas a lazy call-local binding. No context lookup
        // occurs until an executed read, so an unexecuted branch cannot fail
        // merely because its alias has not been declared yet.
        let saved_scope_stack = core::mem::take(&mut self.scope_stack);
        self.push_scope();
        let declaration_aliases =
            self.declaration_aliases_for_callable(params, body);
        let declaration_alias_overlays =
            self.emit_declaration_alias_overlays(&declaration_aliases, &HashMap::new());
        self.declaration_alias_overlays
            .push(declaration_alias_overlays);
        self.callable_mutations
            .push(callable_assignment_names(body));
        // Fresh scope with the params bound; Rust-level fn scope
        // isolates outer locals anyway, but scope tracking is the
        // source of truth for lambda-capture analysis.
        self.push_scope();
        for (index, p) in params.iter().enumerate() {
            self.bind_local(p);
            self.line(&format!(
                "let mut {}: ::bop::value::Value = __bop_param_{};",
                rust_user_ident(p), index
            ));
        }
        for s in body {
            self.emit_stmt(s)?;
        }
        self.pop_scope();
        self.pop_scope();
        self.callable_mutations.pop();
        self.declaration_alias_overlays.pop();
        self.scope_stack = saved_scope_stack;
        // Implicit `return none` if control falls off the end. The
        // `allow(unreachable_code)` at the top of the file silences
        // the warning for bodies that always return explicitly.
        self.line("Ok(::bop::value::Value::None)");
        self.tmp_counter = saved_tmp;
        self.in_top_level = saved_top_level;
        self.in_callable_body = saved_callable_body;
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
                self.ident_value_src(name, line)?
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

            ExprKind::Array(items) => self.array_src(items, line)?,

            ExprKind::Dict(entries) => self.dict_src(entries, line)?,

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
                "return ::std::result::Result::Err(::bop::error_messages::top_level_try_error({}));",
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
    ///           break 'match_arms_N (<body>);
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
            let names: Vec<String> = arm.pattern.binding_names().into_iter().collect();
            let namespaces: Vec<String> = arm.pattern.namespace_names().into_iter().collect();

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
            src.push_str(&self.emit_resolver_closure_src(
                &namespaces,
                pattern_uses_bare_type(&arm.pattern),
            ));
            src.push_str(&format!(
                "            if ::bop::pattern_matches(&__pat, &{}, &mut __bindings, &__resolver) {{\n",
                sc_name
            ));

            self.push_scope();
            for name in &names {
                self.bind_local(name);
                src.push_str(&format!(
                    "                let {}: ::bop::value::Value = __bindings.iter().rev().find(|(k, _)| k == {}).map(|(_, v)| v.clone()).unwrap_or(::bop::value::Value::None);\n",
                    rust_user_ident(name),
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
                    rust_user_ident(name)
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
                    "                    break '{} ({});\n",
                    label, body_src
                ));
                src.push_str("                }\n");
            } else {
                let body_src = self.expr_src(&arm.body)?;
                src.push_str(&format!(
                    "                break '{} ({});\n",
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
        src.push('}');

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
        // Non-Ident callees go through the value-call path. Evaluate
        // arguments first, then the callee, matching the walker and the
        // language's call-dispatch order. Captures `funcs[0](x)`,
        // `make_adder(5)(3)`, `(if cond { f } else { g })(x)`, etc.
        let name = match &callee.kind {
            ExprKind::Ident(n) => n.clone(),
            _ => {
                let callee_src = self.expr_src(callee)?;
                let callee_tmp = self.fresh_tmp();
                let mut arg_lets = String::new();
                let mut arg_names = Vec::with_capacity(args.len());
                for arg in args {
                    let src = self.expr_src(arg)?;
                    let tmp = self.fresh_tmp();
                    write!(arg_lets, "let {} = {}; ", tmp, src).unwrap();
                    arg_names.push(tmp);
                }
                write!(arg_lets, "let {} = {}; ", callee_tmp, callee_src).unwrap();
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
            let callee_src = self.ident_value_src(&name, line)?;
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
                "{{ {}__bop_call_named_value(ctx, {}, {}, {}, {})? }}",
                arg_lets,
                callee_src,
                args_vec,
                rust_string_literal(&name),
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
            "panic" => format!(
                "::bop::builtins::builtin_panic(&{}, {})?",
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

        if let Some(alias) = self.declaration_alias_optional_src(&name) {
            let args_vec = if arg_names.is_empty() {
                "::std::vec::Vec::<::bop::value::Value>::new()".to_string()
            } else {
                format!("vec![{}]", arg_names.join(", "))
            };
            return Ok(format!(
                "{{ {arg_lets}match {alias} {{ ::std::option::Option::Some(__callee) => __bop_call_named_value(ctx, __callee, {args_vec}, {name}, {line})?, ::std::option::Option::None => {{ {body} }}, }} }}",
                name = rust_string_literal(&name),
            ));
        }

        Ok(format!("{{ {}{} }}", arg_lets, body))
    }

    fn array_src(&mut self, items: &[Expr], line: u32) -> Result<String, BopError> {
        if items.is_empty() {
            return Ok(format!(
                "::bop::value::Value::try_new_array(::std::vec::Vec::new(), {})?",
                line
            ));
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
            "{{ {}::bop::value::Value::try_new_array(vec![{}], {})? }}",
            lets,
            names.join(", "),
            line
        ))
    }

    fn dict_src(&mut self, entries: &[(String, Expr)], line: u32) -> Result<String, BopError> {
        if entries.is_empty() {
            return Ok(format!(
                "::bop::value::Value::try_new_dict(::std::vec::Vec::new(), {})?",
                line
            ));
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
            "{{ {}::bop::value::Value::try_new_dict(vec![{}], {})? }}",
            lets,
            pairs.join(", "),
            line
        ))
    }

    fn struct_construct_src(
        &mut self,
        namespace: Option<&str>,
        type_name: &str,
        fields: &[(String, Expr)],
        line: u32,
    ) -> Result<String, BopError> {
        if let Some(namespace) =
            namespace.filter(|namespace| self.is_dynamic_namespace_local(namespace))
        {
            let namespace_value = if self.is_local(namespace) {
                rust_user_ident(namespace)
            } else {
                self.declaration_alias_namespace_src(namespace, line)
                    .expect("dynamic declaration namespace has an overlay")
            };
            let module_path_src = format!(
                "__bop_validate_namespace_type(&{}, {}, {}, {})?",
                namespace_value,
                rust_string_literal(namespace),
                rust_string_literal(type_name),
                line,
            );
            return self.runtime_struct_construct_src(
                &module_path_src,
                type_name,
                fields,
                line,
            );
        }
        if namespace.is_none() {
            let module_path_src = format!(
                "__bop_resolve_bare_type(&__bop_type_bindings, {}, true, {})?",
                rust_string_literal(type_name),
                line,
            );
            return self.runtime_struct_construct_src(
                &module_path_src,
                type_name,
                fields,
                line,
            );
        }
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
            Some(ns) => {
                if self.is_local(ns) {
                    format!(
                        "__bop_validate_namespace_type(&{ns_ident}, {ns_lit}, {ty_lit}, {line})?; ",
                        ns_ident = rust_user_ident(ns),
                        ns_lit = rust_string_literal(ns),
                        ty_lit = rust_string_literal(type_name),
                    )
                } else {
                    let value = if let Some(value) =
                        self.declaration_alias_namespace_src(ns, line)
                    {
                        value
                    } else {
                        self.ident_value_src(ns, line)?
                    };
                    let tmp = self.fresh_tmp();
                    format!(
                        "let {tmp} = {value}; __bop_validate_namespace_type(&{tmp}, {ns_lit}, {ty_lit}, {line})?; ",
                        ns_lit = rust_string_literal(ns),
                        ty_lit = rust_string_literal(type_name),
                    )
                }
            }
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
            "{{ {ns_check}{lets}::bop::value::Value::try_new_struct({mp}.to_string(), {tn}.to_string(), vec![{fields}], {line})? }}",
            ns_check = ns_check,
            lets = lets,
            mp = rust_string_literal(&module_path),
            tn = rust_string_literal(type_name),
            fields = ordered.join(", "),
            line = line,
        ))
    }

    fn runtime_struct_construct_src(
        &mut self,
        module_path_src: &str,
        type_name: &str,
        fields: &[(String, Expr)],
        line: u32,
    ) -> Result<String, BopError> {
        let mut candidates: Vec<(&String, &Vec<String>)> = self
            .types
            .structs
            .iter()
            .filter_map(|((module, candidate), fields)| {
                (candidate == type_name).then_some((module, fields))
            })
            .collect();
        candidates.sort_by(|a, b| a.0.cmp(b.0));
        if candidates.is_empty() {
            return Err(BopError::runtime(
                bop::error_messages::struct_not_declared(type_name),
                line,
            ));
        }

        let mut shape_arms = String::new();
        for (module, declared) in candidates {
            let fields = declared
                .iter()
                .map(|field| rust_string_literal(field))
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(
                shape_arms,
                "{} => &[{}],",
                rust_string_literal(module),
                fields
            )
            .unwrap();
        }
        let provided_names = fields
            .iter()
            .map(|(name, _)| rust_string_literal(name))
            .collect::<Vec<_>>()
            .join(", ");
        let mut lets = String::new();
        let mut provided = Vec::with_capacity(fields.len());
        for (name, expr) in fields {
            let src = self.expr_src(expr)?;
            let tmp = self.fresh_tmp();
            write!(lets, "let {} = {}; ", tmp, src).unwrap();
            provided.push(format!(
                "({}.to_string(), {})",
                rust_string_literal(name),
                tmp
            ));
        }
        Ok(format!(
            "{{ let __module_path = {module_path_src}; \
                let __declared_fields: &'static [&'static str] = match __module_path.as_str() {{ {shape_arms} _ => return Err(::bop::error::BopError::runtime(::bop::error_messages::struct_not_declared({type_name}), {line})), }}; \
                __bop_validate_named_fields(__declared_fields, &[{provided_names}], {type_name}, ::std::option::Option::None, {line})?; \
                {lets}let __provided = vec![{provided}]; \
                let __ordered = __bop_order_named_fields(__declared_fields, __provided); \
                ::bop::value::Value::try_new_struct(__module_path, {type_name}.to_string(), __ordered, {line})? }}",
            module_path_src = module_path_src,
            type_name = rust_string_literal(type_name),
            shape_arms = shape_arms,
            provided_names = provided_names,
            lets = lets,
            provided = provided.join(", "),
            line = line,
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
        if let Some(namespace) =
            namespace.filter(|namespace| self.is_dynamic_namespace_local(namespace))
        {
            let namespace_value = if self.is_local(namespace) {
                rust_user_ident(namespace)
            } else {
                self.declaration_alias_namespace_src(namespace, line)
                    .expect("dynamic declaration namespace has an overlay")
            };
            let module_path_src = format!(
                "__bop_validate_namespace_type(&{}, {}, {}, {})?",
                namespace_value,
                rust_string_literal(namespace),
                rust_string_literal(type_name),
                line,
            );
            return self.runtime_enum_construct_src(
                &module_path_src,
                type_name,
                variant,
                payload,
                line,
            );
        }
        if namespace.is_none() {
            let module_path_src = format!(
                "__bop_resolve_bare_type(&__bop_type_bindings, {}, false, {})?",
                rust_string_literal(type_name),
                line,
            );
            return self.runtime_enum_construct_src(
                &module_path_src,
                type_name,
                variant,
                payload,
                line,
            );
        }
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
            Some(ns) => {
                if self.is_local(ns) {
                    format!(
                        "__bop_validate_namespace_type(&{ns_ident}, {ns_lit}, {ty_lit}, {line})?; ",
                        ns_ident = rust_user_ident(ns),
                        ns_lit = rust_string_literal(ns),
                        ty_lit = rust_string_literal(type_name),
                    )
                } else {
                    let value = if let Some(value) =
                        self.declaration_alias_namespace_src(ns, line)
                    {
                        value
                    } else {
                        self.ident_value_src(ns, line)?
                    };
                    let tmp = self.fresh_tmp();
                    format!(
                        "let {tmp} = {value}; __bop_validate_namespace_type(&{tmp}, {ns_lit}, {ty_lit}, {line})?; ",
                        ns_lit = rust_string_literal(ns),
                        ty_lit = rust_string_literal(type_name),
                    )
                }
            }
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
                    "{{ {ns_check}{lets}::bop::value::Value::try_new_enum_tuple({mp}.to_string(), {tn}.to_string(), {vn}.to_string(), vec![{items}], {line})? }}",
                    ns_check = ns_check,
                    lets = lets,
                    mp = mp_lit,
                    tn = tn_lit,
                    vn = vn_lit,
                    items = names.join(", "),
                    line = line,
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
                    "{{ {ns_check}{lets}::bop::value::Value::try_new_enum_struct({mp}.to_string(), {tn}.to_string(), {vn}.to_string(), vec![{items}], {line})? }}",
                    ns_check = ns_check,
                    lets = lets,
                    mp = mp_lit,
                    tn = tn_lit,
                    vn = vn_lit,
                    items = ordered.join(", "),
                    line = line,
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

    fn runtime_enum_construct_src(
        &mut self,
        module_path_src: &str,
        type_name: &str,
        variant: &str,
        payload: &VariantPayload,
        line: u32,
    ) -> Result<String, BopError> {
        let mut candidates: Vec<(&String, &HashMap<String, VariantKind>)> = self
            .types
            .enums
            .iter()
            .filter_map(|((module, candidate), variants)| {
                (candidate == type_name).then_some((module, variants))
            })
            .collect();
        candidates.sort_by(|a, b| a.0.cmp(b.0));
        if candidates.is_empty() {
            return Err(BopError::runtime(
                bop::error_messages::enum_not_declared(type_name),
                line,
            ));
        }

        let mut shape_arms = String::new();
        for (module, variants) in candidates {
            let shape = match variants.get(variant) {
                Some(VariantKind::Unit) => "__BopDynamicVariantShape::Unit".to_string(),
                Some(VariantKind::Tuple(fields)) => {
                    format!("__BopDynamicVariantShape::Tuple({})", fields.len())
                }
                Some(VariantKind::Struct(fields)) => {
                    let fields = fields
                        .iter()
                        .map(|field| rust_string_literal(field))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("__BopDynamicVariantShape::Struct(&[{}])", fields)
                }
                None => format!(
                    "return Err(::bop::error::BopError::runtime(::bop::error_messages::enum_has_no_variant({}, {}), {}))",
                    rust_string_literal(type_name),
                    rust_string_literal(variant),
                    line
                ),
            };
            writeln!(
                shape_arms,
                "{} => {},",
                rust_string_literal(module),
                shape
            )
            .unwrap();
        }

        let namespace_check = format!(
            "let __module_path = {}; \
             let __variant_shape = match __module_path.as_str() {{ {} _ => return Err(::bop::error::BopError::runtime(::bop::error_messages::enum_not_declared({}), {})), }}; ",
            module_path_src,
            shape_arms,
            rust_string_literal(type_name),
            line,
        );
        let type_lit = rust_string_literal(type_name);
        let variant_lit = rust_string_literal(variant);
        match payload {
            VariantPayload::Unit => Ok(format!(
                "{{ {namespace_check}match __variant_shape {{ \
                    __BopDynamicVariantShape::Unit => {{}}, \
                    __BopDynamicVariantShape::Tuple(_) => return Err(::bop::error::BopError::runtime(format!(\"Variant `{{}}::{{}}` expects positional arguments `(…)`\", {type_lit}, {variant_lit}), {line})), \
                    __BopDynamicVariantShape::Struct(_) => return Err(::bop::error::BopError::runtime(format!(\"Variant `{{}}::{{}}` expects named fields `{{{{ … }}}}`\", {type_lit}, {variant_lit}), {line})), \
                }} ::bop::value::Value::new_enum_unit(__module_path, {type_lit}.to_string(), {variant_lit}.to_string()) }}"
            )),
            VariantPayload::Tuple(args) => {
                let mut lets = String::new();
                let mut values = Vec::with_capacity(args.len());
                for arg in args {
                    let src = self.expr_src(arg)?;
                    let tmp = self.fresh_tmp();
                    write!(lets, "let {} = {}; ", tmp, src).unwrap();
                    values.push(tmp);
                }
                Ok(format!(
                    "{{ {namespace_check}match __variant_shape {{ \
                        __BopDynamicVariantShape::Unit => return Err(::bop::error::BopError::runtime(format!(\"Variant `{{}}::{{}}` takes no payload\", {type_lit}, {variant_lit}), {line})), \
                        __BopDynamicVariantShape::Tuple(__expected) if __expected == {actual} => {{}}, \
                        __BopDynamicVariantShape::Tuple(__expected) => return Err(::bop::error::BopError::runtime(format!(\"`{{}}::{{}}` expects {{}} argument{{}}, but got {{}}\", {type_lit}, {variant_lit}, __expected, if __expected == 1 {{ \"\" }} else {{ \"s\" }}, {actual}), {line})), \
                        __BopDynamicVariantShape::Struct(_) => return Err(::bop::error::BopError::runtime(format!(\"Variant `{{}}::{{}}` expects named fields `{{{{ … }}}}`\", {type_lit}, {variant_lit}), {line})), \
                    }} {lets}::bop::value::Value::try_new_enum_tuple(__module_path, {type_lit}.to_string(), {variant_lit}.to_string(), vec![{values}], {line})? }}",
                    actual = args.len(),
                    values = values.join(", "),
                ))
            }
            VariantPayload::Struct(fields) => {
                let provided_names = fields
                    .iter()
                    .map(|(name, _)| rust_string_literal(name))
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut lets = String::new();
                let mut provided = Vec::with_capacity(fields.len());
                for (name, expr) in fields {
                    let src = self.expr_src(expr)?;
                    let tmp = self.fresh_tmp();
                    write!(lets, "let {} = {}; ", tmp, src).unwrap();
                    provided.push(format!(
                        "({}.to_string(), {})",
                        rust_string_literal(name),
                        tmp
                    ));
                }
                Ok(format!(
                    "{{ {namespace_check}let __declared_fields = match __variant_shape {{ \
                        __BopDynamicVariantShape::Unit => return Err(::bop::error::BopError::runtime(format!(\"Variant `{{}}::{{}}` takes no payload\", {type_lit}, {variant_lit}), {line})), \
                        __BopDynamicVariantShape::Tuple(_) => return Err(::bop::error::BopError::runtime(format!(\"Variant `{{}}::{{}}` expects positional arguments `(…)`\", {type_lit}, {variant_lit}), {line})), \
                        __BopDynamicVariantShape::Struct(__fields) => __fields, \
                    }}; __bop_validate_named_fields(__declared_fields, &[{provided_names}], {type_lit}, ::std::option::Option::Some({variant_lit}), {line})?; \
                    {lets}let __provided = vec![{provided}]; let __ordered = __bop_order_named_fields(__declared_fields, __provided); \
                    ::bop::value::Value::try_new_enum_struct(__module_path, {type_lit}.to_string(), {variant_lit}.to_string(), __ordered, {line})? }}",
                    provided = provided.join(", "),
                ))
            }
        }
    }

    fn string_interp_src(
        &mut self,
        parts: &[StringPart],
        line: u32,
    ) -> Result<String, BopError> {
        // Mirror the tree-walker: for each Variable part, resolve the
        // current Bop value at the point where that part executes.
        let mut body = String::from("{ let mut __s = ::std::string::String::new(); ");
        for part in parts {
            match part {
                StringPart::Literal(s) => {
                    write!(body, "__s.push_str({}); ", rust_string_literal(s)).unwrap();
                }
                StringPart::Variable(name) => {
                    let value = self.ident_value_src(name, line)?;
                    // The cloned Value lives only for the duration
                    // of the format call; its Drop tracks the
                    // de-alloc correctly against `bop::memory`.
                    write!(
                        body,
                        "__s.push_str(&format!(\"{{}}\", {})); ",
                        value
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
        let nested_place = matches!(
            &object.kind,
            ExprKind::Index { .. } | ExprKind::FieldAccess { .. }
        );
        let ident_target = if mutating {
            match &object.kind {
                ExprKind::Ident(n) => Some(n.clone()),
                _ => None,
            }
        } else {
            None
        };

        // A mutating method on a bare identifier is the one shape where the
        // language writes the receiver back. Arrays are owned values, so the
        // binding itself can be mutated without violating value semantics:
        // copy-on-write detaches only when an earlier assignment / argument
        // still shares the backing store. Crucially, do this only after all
        // argument temporaries above have evaluated. Besides matching the
        // walker, that makes nested calls such as `a.push(a.pop())` observe the
        // current post-argument binding.
        //
        // The receiver is dynamically typed. A struct may define a user method
        // named `push`, so non-arrays still clone once and go through the full
        // user-method-before-builtin dispatcher. Only the built-in Array arm
        // takes the in-place path.
        if let Some(target_name) = ident_target {
            let owned_args = if arg_tmps.is_empty() {
                "::std::vec::Vec::new()".to_string()
            } else {
                format!("::std::vec![{}]", arg_tmps.join(", "))
            };
            let obj_tmp = self.fresh_tmp();
            if !self.is_local(&target_name) {
                if let Some(overlay) = self.declaration_alias_overlay(&target_name) {
                    let read = self
                        .declaration_alias_read_src(&target_name, line)
                        .expect("overlay checked above");
                    let mut body = String::new();
                    write!(
                        body,
                        "{{ {}let __ret = if let ::std::option::Option::Some(::bop::value::Value::Array(__bop_array)) = {}.overlay.as_mut() {{ \
                            ::bop::methods::array_method_mut(__bop_array, {}, {}, {})? \
                        }} else {{ \
                            let {} = {}; \
                            match __bop_try_user_method(ctx, &{}, {}, &{}, {})? {{ \
                                Some(v) => v, \
                                None => {{ \
                                    let (__r, __mutated) = __bop_call_method(ctx, &{}, {}, &{}, {})?; \
                                    if let Some(__new_obj) = __mutated {{ {}.overlay = ::std::option::Option::Some(__new_obj); }} \
                                    __r \
                                }}, \
                            }} \
                        }}; __ret }}",
                        arg_lets,
                        overlay,
                        method_lit,
                        owned_args,
                        line,
                        obj_tmp,
                        read,
                        obj_tmp,
                        method_lit,
                        args_arr,
                        line,
                        obj_tmp,
                        method_lit,
                        args_arr,
                        line,
                        overlay,
                    )
                    .unwrap();
                    return Ok(body);
                }
            }
            let target = rust_user_ident(&target_name);
            let mut body = String::new();
            write!(
                body,
                "{{ {}let __ret = if let ::bop::value::Value::Array(__bop_array) = &mut {} {{ \
                    ::bop::methods::array_method_mut(__bop_array, {}, {}, {})? \
                }} else {{ \
                    let {} = {}.clone(); \
                    match __bop_try_user_method(ctx, &{}, {}, &{}, {})? {{ \
                        Some(v) => v, \
                        None => {{ \
                            let (__r, __mutated) = __bop_call_method(ctx, &{}, {}, &{}, {})?; \
                            if let Some(__new_obj) = __mutated {{ {} = __new_obj; }} \
                            __r \
                        }}, \
                    }} \
                }}; __ret }}",
                arg_lets,
                target,
                method_lit,
                owned_args,
                line,
                obj_tmp,
                target,
                obj_tmp,
                method_lit,
                args_arr,
                line,
                obj_tmp,
                method_lit,
                args_arr,
                line,
                target,
            )
            .unwrap();
            return Ok(body);
        }

        let obj_src = self.expr_src(object)?;
        let obj_tmp = self.fresh_tmp();
        let mut body = String::new();
        write!(body, "{{ {}let {} = {}; ", arg_lets, obj_tmp, obj_src).unwrap();
        // Try user-defined methods first — the dispatcher returns
        // `Some(Value)` when a match is found, else `None`
        // (meaning "fall through to the built-in method
        // dispatch"). Matches walker / VM precedence.
        let nested_guard = if nested_place {
            format!(
                "::bop::methods::reject_nested_array_mutation(&{}, {}, {})?; ",
                obj_tmp, method_lit, line
            )
        } else {
            String::new()
        };
        write!(
            body,
            "let __ret = match __bop_try_user_method(ctx, &{}, {}, &{}, {})? {{ \
                Some(v) => v, \
                None => {{ \
                    {}let (__r, _) = __bop_call_method(ctx, &{}, {}, &{}, {})?; __r \
                }}, \
            }}; __ret }}",
            obj_tmp,
            method_lit,
            args_arr,
            line,
            nested_guard,
            obj_tmp,
            method_lit,
            args_arr,
            line
        )
        .unwrap();
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
        let uses_runtime_type_bindings = self.statements_use_runtime_type_bindings(body);
        // Free-variable analysis against the outer scope stack.
        let mut dependencies = FreeVarDependencies::default();
        let mut body_known = HashSet::new();
        for p in params {
            body_known.insert(p.clone());
        }
        scan_free_vars_stmts(
            body,
            &mut body_known,
            &mut dependencies,
            &self.scope_stack,
            &self.fn_info,
        );
        dependencies.required.extend(dependencies.pattern_namespaces);
        let captures_ordered: Vec<String> = dependencies.required.into_iter().collect();

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
        let declaration_aliases =
            self.declaration_aliases_for_callable(params, body);
        let mut alias_initializers = HashMap::new();
        let mut alias_captures = Vec::new();
        for (index, alias) in declaration_aliases.iter().enumerate() {
            if let Some(parent_overlay) = self.declaration_alias_overlay(alias) {
                let capture = format!("__alias_cap_{}", index);
                alias_initializers.insert(alias.clone(), format!("{}.clone()", capture));
                alias_captures.push((capture, parent_overlay));
            }
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
        let saved_callable_body = self.in_callable_body;
        self.indent = 0;
        self.tmp_counter = 0;
        self.in_top_level = false;
        self.in_callable_body = true;
        let declaration_alias_overlays =
            self.emit_declaration_alias_overlays(&declaration_aliases, &alias_initializers);
        self.declaration_alias_overlays
            .push(declaration_alias_overlays);
        self.callable_mutations
            .push(callable_assignment_names(body));
        for s in body {
            self.emit_stmt(s)?;
        }
        self.callable_mutations.pop();
        self.declaration_alias_overlays.pop();
        let body_src = core::mem::replace(&mut self.out, saved_out);
        self.indent = saved_indent;
        self.tmp_counter = saved_tmp;
        self.in_top_level = saved_top_level;
        self.in_callable_body = saved_callable_body;

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
                ident = rust_user_ident(cap)
            )
            .unwrap();
            // `Fn` closures can be invoked repeatedly, so captures
            // must stay owned by the closure body. Clone per
            // invocation; the outer capture is the stable source.
            writeln!(
                capture_moves,
                "let mut {ident}: ::bop::value::Value = __cap_{i}.clone();",
                ident = rust_user_ident(cap),
                i = i
            )
            .unwrap();
        }
        for (capture, parent_overlay) in &alias_captures {
            writeln!(
                capture_prelude,
                "let {capture} = {parent_overlay}.overlay.clone();"
            )
            .unwrap();
        }
        let mut opaque_body_depth = captures_ordered
            .iter()
            .enumerate()
            .fold(String::from("0u16"), |depth, (i, _)| {
                format!("{depth}.max(__cap_{i}.ownership_depth())")
            });
        for (capture, _) in &alias_captures {
            opaque_body_depth = format!(
                "{opaque_body_depth}.max({capture}.as_ref().map_or(0u16, ::bop::value::Value::ownership_depth))"
            );
        }

        let arity = params.len();
        let arity_suffix = if arity == 1 { "" } else { "s" };
        let mut param_binds = String::new();
        for p in params {
            writeln!(
                param_binds,
                "let mut {ident}: ::bop::value::Value = args.remove(0);",
                ident = rust_user_ident(p)
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

        let type_bindings_init = if uses_runtime_type_bindings {
            format!(
                "let mut __bop_type_bindings = __bop_callable_type_bindings(ctx, {}); ",
                rust_string_literal(&self.current_module),
            )
        } else {
            String::new()
        };

        Ok(format!(
            "{{ {prelude}let __opaque_body_depth = {opaque_body_depth}; let __callable: ::std::rc::Rc<dyn for<'__a> Fn(&mut Ctx<'__a>, ::std::vec::Vec<::bop::value::Value>) -> Result<::bop::value::Value, ::bop::error::BopError>> = ::std::rc::Rc::new(move |ctx, mut args| {{ if args.len() != {arity} {{ return Err(::bop::error::BopError::runtime(format!(\"lambda expects {arity} argument{suffix}, but got {{}}\", args.len()), {line})); }} {type_bindings_init}{moves}{param_binds}{body} #[allow(unreachable_code)] Ok(::bop::value::Value::None) }}); __bop_wrap_callable({params_array}, ::std::vec::Vec::new(), None, __opaque_body_depth, {line}, __callable)? }}",
            prelude = capture_prelude,
            opaque_body_depth = opaque_body_depth,
            arity = arity,
            suffix = arity_suffix,
            line = line,
            type_bindings_init = type_bindings_init,
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

#[derive(Default)]
struct FreeVarDependencies {
    /// Value/expression references that must exist when the closure or lifted
    /// function is created.
    required: std::collections::BTreeSet<String>,
    /// Namespace references used only by patterns. Declaration aliases in
    /// this set are resolved lazily by the pattern resolver, while a lambda
    /// still promotes outer locals/params from this set into real captures.
    pattern_namespaces: std::collections::BTreeSet<String>,
    /// Nested named functions have their own declaration context and do not
    /// capture a caller's alias overlay. Nested lambdas do capture it and must
    /// continue to propagate through arbitrarily deep closure chains.
    skip_nested_named_functions: bool,
}

impl FreeVarDependencies {
    fn for_declaration_aliases() -> Self {
        Self {
            skip_nested_named_functions: true,
            ..Self::default()
        }
    }
}

fn scan_free_vars_stmts(
    stmts: &[Stmt],
    known: &mut HashSet<String>,
    free: &mut FreeVarDependencies,
    outer_scopes: &[EmissionScope],
    fn_info: &FnInfo,
) {
    for stmt in stmts {
        scan_free_vars_stmt(stmt, known, free, outer_scopes, fn_info);
    }
}

fn scan_free_vars_stmt(
    stmt: &Stmt,
    known: &mut HashSet<String>,
    free: &mut FreeVarDependencies,
    outer_scopes: &[EmissionScope],
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
                        free.required.insert(n.clone());
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
            if free.skip_nested_named_functions {
                return;
            }
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
        StmtKind::Use { alias, .. } => {
            // Import bindings are created when the lambda executes,
            // so the use site itself does not capture an outer value.
            if let Some(alias) = alias {
                known.insert(alias.clone());
            }
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

fn pattern_uses_bare_type(pattern: &bop::parser::Pattern) -> bool {
    use bop::parser::{Pattern, VariantPatternPayload};
    match pattern {
        Pattern::EnumVariant {
            namespace,
            payload,
            ..
        } => {
            namespace.is_none()
                || match payload {
                    VariantPatternPayload::Unit => false,
                    VariantPatternPayload::Tuple(items) => {
                        items.iter().any(pattern_uses_bare_type)
                    }
                    VariantPatternPayload::Struct { fields, .. } => {
                        fields.iter().any(|(_, field)| pattern_uses_bare_type(field))
                    }
                }
        }
        Pattern::Struct {
            namespace, fields, ..
        } => {
            namespace.is_none()
                || fields.iter().any(|(_, field)| pattern_uses_bare_type(field))
        }
        Pattern::Array { elements, .. } | Pattern::Or(elements) => {
            elements.iter().any(pattern_uses_bare_type)
        }
        Pattern::Wildcard | Pattern::Binding(_) | Pattern::Literal(_) => false,
    }
}

fn expr_uses_runtime_type_bindings(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::Int(_)
        | ExprKind::Number(_)
        | ExprKind::Str(_)
        | ExprKind::Bool(_)
        | ExprKind::None
        | ExprKind::Ident(_)
        | ExprKind::StringInterp(_) => false,
        ExprKind::BinaryOp { left, right, .. } => {
            expr_uses_runtime_type_bindings(left) || expr_uses_runtime_type_bindings(right)
        }
        ExprKind::UnaryOp { expr, .. } | ExprKind::Try(expr) => {
            expr_uses_runtime_type_bindings(expr)
        }
        ExprKind::Call { callee, args } => {
            expr_uses_runtime_type_bindings(callee)
                || args.iter().any(expr_uses_runtime_type_bindings)
        }
        ExprKind::MethodCall { object, args, .. } => {
            expr_uses_runtime_type_bindings(object)
                || args.iter().any(expr_uses_runtime_type_bindings)
        }
        ExprKind::Index { object, index } => {
            expr_uses_runtime_type_bindings(object) || expr_uses_runtime_type_bindings(index)
        }
        ExprKind::Array(items) => items.iter().any(expr_uses_runtime_type_bindings),
        ExprKind::Dict(entries) => entries
            .iter()
            .any(|(_, value)| expr_uses_runtime_type_bindings(value)),
        ExprKind::IfExpr {
            condition,
            then_expr,
            else_expr,
        } => {
            expr_uses_runtime_type_bindings(condition)
                || expr_uses_runtime_type_bindings(then_expr)
                || expr_uses_runtime_type_bindings(else_expr)
        }
        // Nested callables initialize from their defining module when invoked;
        // their type requirements do not force an allocation in the creator.
        ExprKind::Lambda { .. } => false,
        ExprKind::FieldAccess { object, .. } => expr_uses_runtime_type_bindings(object),
        ExprKind::StructConstruct {
            namespace, fields, ..
        } => {
            namespace.is_none()
                || fields
                    .iter()
                    .any(|(_, value)| expr_uses_runtime_type_bindings(value))
        }
        ExprKind::EnumConstruct {
            namespace, payload, ..
        } => {
            namespace.is_none()
                || match payload {
                    VariantPayload::Unit => false,
                    VariantPayload::Tuple(values) => {
                        values.iter().any(expr_uses_runtime_type_bindings)
                    }
                    VariantPayload::Struct(fields) => fields
                        .iter()
                        .any(|(_, value)| expr_uses_runtime_type_bindings(value)),
                }
        }
        ExprKind::Match { scrutinee, arms } => {
            expr_uses_runtime_type_bindings(scrutinee)
                || arms.iter().any(|arm| {
                    pattern_uses_bare_type(&arm.pattern)
                        || arm
                            .guard
                            .as_ref()
                            .is_some_and(expr_uses_runtime_type_bindings)
                        || expr_uses_runtime_type_bindings(&arm.body)
                })
        }
    }
}

fn scan_free_vars_expr(
    expr: &Expr,
    known: &mut HashSet<String>,
    free: &mut FreeVarDependencies,
    outer_scopes: &[EmissionScope],
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
                free.required.insert(name.clone());
            }
        }
        ExprKind::StringInterp(parts) => {
            for part in parts {
                if let StringPart::Variable(name) = part {
                    if !known.contains(name) && is_outer_local(name, outer_scopes) {
                        free.required.insert(name.clone());
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
            // closure gets the value when it's constructed. Seed
            // the nested scan with our current bindings so a nested
            // reference to an outer-lambda param/local doesn't get
            // mistaken for a same-named binding outside both lambdas.
            let mut inner_known = known.clone();
            for p in params {
                inner_known.insert(p.clone());
            }
            scan_free_vars_stmts(body, &mut inner_known, free, outer_scopes, fn_info);
        }
        ExprKind::FieldAccess { object, .. } => {
            scan_free_vars_expr(object, known, free, outer_scopes, fn_info);
        }
        ExprKind::StructConstruct {
            namespace,
            fields,
            ..
        } => {
            // Namespaced construction emits a runtime validation
            // against the module Value. Record that otherwise-hidden
            // Rust closure capture so opaque ownership depth includes it.
            if let Some(name) = namespace {
                if !known.contains(name) && is_outer_local(name, outer_scopes) {
                    free.required.insert(name.clone());
                }
            }
            for (_, v) in fields {
                scan_free_vars_expr(v, known, free, outer_scopes, fn_info);
            }
        }
        ExprKind::EnumConstruct {
            namespace,
            payload,
            ..
        } => {
            if let Some(name) = namespace {
                if !known.contains(name) && is_outer_local(name, outer_scopes) {
                    free.required.insert(name.clone());
                }
            }
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
                for namespace in arm.pattern.namespace_names() {
                    if !known.contains(&namespace)
                        && is_outer_local(&namespace, outer_scopes)
                    {
                        free.pattern_namespaces.insert(namespace);
                    }
                }
                let mut arm_known = known.clone();
                arm_known.extend(arm.pattern.binding_names());
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

fn is_outer_local(name: &str, outer_scopes: &[EmissionScope]) -> bool {
    outer_scopes
        .iter()
        .any(|scope| scope.locals.contains(name))
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

/// Encode arbitrary UTF-8 source text into an injective,
/// Rust-identifier-safe component. Hex is deliberately used instead
/// of a character substitution: `a.b`, `a_b`, and names that already
/// look mangled must remain distinct.
fn user_name_component(name: &str) -> String {
    let mut encoded = String::with_capacity(name.len() * 2);
    for byte in name.as_bytes() {
        write!(encoded, "{byte:02x}").unwrap();
    }
    encoded
}

/// The sole lowering boundary for source-level value identifiers.
/// Every Bop binding and reference passes through this namespace, so
/// it cannot collide with emitter temporaries (`__t0`, `__l`, `ctx`,
/// ...), Rust keywords, or another source name.
fn rust_user_ident(name: &str) -> String {
    format!("__bop_user_value_{}", user_name_component(name))
}

fn declaration_alias_overlay_ident(name: &str) -> String {
    format!("__bop_declaration_alias_{}", user_name_component(name))
}

fn declaration_alias_pattern_snapshot_ident(name: &str) -> String {
    format!("__bop_pattern_alias_{}", user_name_component(name))
}

/// Render a Bop user-fn name as a Rust function name under a
/// specific module prefix (`""` for the root program). Kept
/// prefixed to avoid clashes with built-ins, and extended with
/// the module slug so `foo.bar::square` and root::square can
/// coexist in the same emitted Rust file.
fn rust_fn_name_with(module_prefix: &str, name: &str) -> String {
    format!(
        "__bop_user_fn_{}n{}",
        module_prefix,
        user_name_component(name)
    )
}

fn wrapper_fn_name_with(module_prefix: &str, name: &str) -> String {
    format!(
        "__bop_user_fn_value_{}n{}",
        module_prefix,
        user_name_component(name)
    )
}

/// Prefix user functions emitted from an imported module. The
/// leading non-hex marker keeps root, module, and method namespaces
/// structurally distinct.
fn module_fn_prefix(path: &str) -> String {
    format!("m{}_", user_name_component(path))
}

fn method_fn_prefix(module_prefix: &str, type_name: &str) -> String {
    format!("{}t{}_", module_prefix, user_name_component(type_name))
}

/// Turn a Bop module path into a collision-free Rust-safe slug.
fn module_slug(path: &str) -> String {
    user_name_component(path)
}

fn module_load_fn_name(path: &str) -> String {
    format!("__mod_{}_load", module_slug(path))
}

fn module_exports_type_name(path: &str) -> String {
    format!("BopModule{}Exports", module_slug(path))
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
    pub module_aliases: ::std::collections::HashMap<
        (::std::string::String, ::std::string::String),
        ::bop::value::Value,
    >,
    pub module_type_bindings: ::std::collections::HashMap<
        ::std::string::String,
        ::std::collections::BTreeMap<::std::string::String, ::std::string::String>,
    >,
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
    pub module_aliases: ::std::collections::HashMap<
        (::std::string::String, ::std::string::String),
        ::bop::value::Value,
    >,
    pub module_type_bindings: ::std::collections::HashMap<
        ::std::string::String,
        ::std::collections::BTreeMap<::std::string::String, ::std::string::String>,
    >,
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

type __BopTypeFrame =
    ::std::collections::BTreeMap<::std::string::String, ::std::string::String>;
type __BopTypeBindings = ::std::vec::Vec<__BopTypeFrame>;

fn __bop_builtin_type_frame() -> __BopTypeFrame {
    let mut bindings = __BopTypeFrame::new();
    bindings.insert("Result".to_string(), "<builtin>".to_string());
    bindings.insert("RuntimeError".to_string(), "<builtin>".to_string());
    bindings.insert("Iter".to_string(), "<builtin>".to_string());
    bindings
}

fn __bop_fresh_module_type_bindings() -> __BopTypeBindings {
    ::std::vec![__bop_builtin_type_frame()]
}

fn __bop_callable_type_bindings(ctx: &Ctx<'_>, module: &str) -> __BopTypeBindings {
    let committed = ctx
        .module_type_bindings
        .get(module)
        .cloned()
        .unwrap_or_else(__bop_builtin_type_frame);
    ::std::vec![committed, __BopTypeFrame::new()]
}

fn __bop_flatten_type_bindings(bindings: &__BopTypeBindings) -> __BopTypeFrame {
    let mut flattened = __BopTypeFrame::new();
    for frame in bindings {
        for (name, origin) in frame {
            flattened.insert(name.clone(), origin.clone());
        }
    }
    flattened
}

fn __bop_publish_type_bindings(
    ctx: &mut Ctx<'_>,
    module: &str,
    bindings: &__BopTypeBindings,
) {
    ctx.module_type_bindings
        .insert(module.to_string(), __bop_flatten_type_bindings(bindings));
}

fn __bop_bind_type(bindings: &mut __BopTypeBindings, name: &str, origin: &str) {
    bindings
        .last_mut()
        .expect("type bindings always have a frame")
        .insert(name.to_string(), origin.to_string());
}

fn __bop_bind_imported_type(
    bindings: &mut __BopTypeBindings,
    name: &str,
    origin: &str,
) {
    bindings
        .last_mut()
        .expect("type bindings always have a frame")
        .entry(name.to_string())
        .or_insert_with(|| origin.to_string());
}

fn __bop_resolve_bare_type(
    bindings: &__BopTypeBindings,
    type_name: &str,
    is_struct: bool,
    line: u32,
) -> Result<::std::string::String, ::bop::error::BopError> {
    for frame in bindings.iter().rev() {
        if let ::std::option::Option::Some(origin) = frame.get(type_name) {
            return Ok(origin.clone());
        }
    }
    let message = if is_struct {
        ::bop::error_messages::struct_not_declared(type_name)
    } else {
        ::bop::error_messages::enum_not_declared(type_name)
    };
    Err(::bop::error::BopError::runtime(message, line))
}

struct __BopDeclarationAliasBinding {
    overlay: ::std::option::Option<::bop::value::Value>,
    cached: ::std::option::Option<::bop::value::Value>,
}

enum __BopDynamicVariantShape {
    Unit,
    Tuple(usize),
    Struct(&'static [&'static str]),
}

fn __bop_declaration_alias_optional(
    ctx: &Ctx<'_>,
    binding: &mut __BopDeclarationAliasBinding,
    module: &str,
    alias: &str,
) -> ::std::option::Option<::bop::value::Value> {
    if let ::std::option::Option::Some(value) = &binding.overlay {
        return ::std::option::Option::Some(value.clone());
    }
    if let ::std::option::Option::Some(value) = &binding.cached {
        return ::std::option::Option::Some(value.clone());
    }
    let value = ctx
        .module_aliases
        .get(&(module.to_string(), alias.to_string()))
        .cloned()?;
    binding.cached = ::std::option::Option::Some(value.clone());
    ::std::option::Option::Some(value)
}

fn __bop_declaration_alias_read(
    ctx: &Ctx<'_>,
    binding: &mut __BopDeclarationAliasBinding,
    module: &str,
    alias: &str,
    line: u32,
) -> Result<::bop::value::Value, ::bop::error::BopError> {
    __bop_declaration_alias_optional(ctx, binding, module, alias).ok_or_else(|| {
            ::bop::error::BopError::runtime(
                ::bop::error_messages::variable_not_found(alias),
                line,
            )
        })
}

fn __bop_declaration_alias_namespace(
    ctx: &Ctx<'_>,
    binding: &mut __BopDeclarationAliasBinding,
    module: &str,
    alias: &str,
    line: u32,
) -> Result<::bop::value::Value, ::bop::error::BopError> {
    __bop_declaration_alias_optional(ctx, binding, module, alias).ok_or_else(|| {
        ::bop::error::BopError::runtime(
            format!("`{}` isn't a module alias in scope", alias),
            line,
        )
    })
}

fn __bop_declaration_alias_assign(
    ctx: &Ctx<'_>,
    binding: &mut __BopDeclarationAliasBinding,
    value: ::bop::value::Value,
    module: &str,
    alias: &str,
    line: u32,
) -> Result<(), ::bop::error::BopError> {
    if binding.overlay.is_none()
        && binding.cached.is_none()
        && !ctx
            .module_aliases
            .contains_key(&(module.to_string(), alias.to_string()))
    {
        return Err(::bop::error::BopError::runtime(
            format!("Variable `{}` doesn't exist yet", alias),
            line,
        ));
    }
    binding.overlay = ::std::option::Option::Some(value);
    Ok(())
}

fn __bop_declaration_alias_mut<'v>(
    binding: &'v mut __BopDeclarationAliasBinding,
    alias: &str,
    line: u32,
) -> Result<&'v mut ::bop::value::Value, ::bop::error::BopError> {
    binding.overlay.as_mut().ok_or_else(|| {
        ::bop::error::BopError::runtime(
            ::bop::error_messages::variable_not_found(alias),
            line,
        )
    })
}

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
/// `__bop_user_fn_value_*` wrappers and by `MakeLambda` emission.
fn __bop_wrap_callable(
    params: ::std::vec::Vec<String>,
    captures: ::std::vec::Vec<(String, ::bop::value::Value)>,
    self_name: ::std::option::Option<String>,
    opaque_body_depth: u16,
    line: u32,
    callable: ::std::rc::Rc<
        dyn for<'__a> Fn(
            &mut Ctx<'__a>,
            ::std::vec::Vec<::bop::value::Value>,
        ) -> Result<::bop::value::Value, ::bop::error::BopError>,
    >,
) -> Result<::bop::value::Value, ::bop::error::BopError> {
    let closure = ::std::rc::Rc::new(AotClosure { callable });
    let body: ::std::rc::Rc<dyn ::core::any::Any + 'static> = closure;
    ::bop::value::Value::try_new_compiled_fn(
        params,
        captures,
        body,
        self_name,
        opaque_body_depth,
        line,
    )
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

fn __bop_call_named_value(
    ctx: &mut Ctx<'_>,
    callee: ::bop::value::Value,
    args: ::std::vec::Vec<::bop::value::Value>,
    name: &str,
    line: u32,
) -> Result<::bop::value::Value, ::bop::error::BopError> {
    if !matches!(&callee, ::bop::value::Value::Fn(_)) {
        return Err(::bop::error::BopError::runtime(
            format!("`{}` is a {}, not a function", name, callee.type_name()),
            line,
        ));
    }
    __bop_call_value(ctx, callee, args, line)
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
        Ok(value) => ::bop::builtins::make_try_call_ok(value, line),
        Err(err) => {
            if err.is_fatal {
                Err(err)
            } else {
                Ok(::bop::builtins::make_try_call_err(&err))
            }
        }
    }
}

/// Handle `r.map(f)` / `r.map_err(f)` / `r.and_then(f)` for a
/// built-in `Result`. Invokes the callable synchronously via
/// the AOT closure pathway and wraps the result according to
/// the method kind. Short-circuit branches (map on Err,
/// map_err on Ok) skip the call and pass the Result through.
fn __bop_result_callable_method(
    ctx: &mut Ctx<'_>,
    receiver: &::bop::value::Value,
    kind: ::bop::methods::ResultCallableKind,
    method: &str,
    args: &[::bop::value::Value],
    line: u32,
) -> Result<::bop::value::Value, ::bop::error::BopError> {
    use ::bop::methods::{make_result_err, make_result_ok, ResultCallableKind};
    if args.len() != 1 {
        return Err(::bop::error::BopError::runtime(
            format!("`{}` expects 1 argument, but got {}", method, args.len()),
            line,
        ));
    }
    let callable = args[0].clone();
    let (is_ok, payload) = match receiver {
        ::bop::value::Value::EnumVariant(e) => {
            let payload = match e.payload() {
                ::bop::value::EnumPayload::Tuple(items) if items.len() == 1 => {
                    items[0].clone()
                }
                _ => {
                    return Err(::bop::error::BopError::runtime(
                        format!("malformed Result::{} payload", e.variant()),
                        line,
                    ));
                }
            };
            (e.variant() == "Ok", payload)
        }
        _ => {
            return Err(::bop::error::BopError::runtime(
                "Result method called on non-Result",
                line,
            ));
        }
    };

    // Short-circuit: these branches don't invoke the callable,
    // they just rebuild the Result with the existing payload.
    match kind {
        ResultCallableKind::Map if !is_ok => return make_result_err(payload, line),
        ResultCallableKind::MapErr if is_ok => return make_result_ok(payload, line),
        ResultCallableKind::AndThen if !is_ok => return make_result_err(payload, line),
        _ => {}
    }

    let func = match &callable {
        ::bop::value::Value::Fn(f) => ::std::rc::Rc::clone(f),
        other => {
            return Err(::bop::error::BopError::runtime(
                format!("`{}` expects a function, got {}", method, other.type_name()),
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
                    format!("{}: callee wasn't compiled by the AOT transpiler", method),
                    line,
                ));
            }
        },
        ::bop::value::FnBody::Ast(_) => {
            return Err(::bop::error::BopError::runtime(
                format!("{}: callee was compiled for the walker, not AOT", method),
                line,
            ));
        }
    };
    drop(func);
    let result = callable_fn(ctx, ::std::vec![payload])?;
    match kind {
        ResultCallableKind::Map => make_result_ok(result, line),
        ResultCallableKind::MapErr => make_result_err(result, line),
        // `and_then` trusts the closure to have produced a
        // Result already — pass it through untouched.
        ResultCallableKind::AndThen => Ok(result),
    }
}

/// Field read for `Value::Struct`, struct-payload enum variants,
/// and `Value::Module`. Returns a cloned value; misses surface as
/// a runtime error with the type name in the message.
///
/// Module field reads resolve against the module's `bindings`
/// list. A field name that matches the module's public type exports
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
            if m.has_type(field) {
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
/// the module's published type exports contain the name, then returns
/// the module path that defines the constructed value's runtime identity. Used by
/// struct / enum construct emission when `namespace` is `Some(...)`,
/// so the AOT surfaces the same error as walker + VM when someone
/// writes `m.MissingType { ... }` or uses a non-module as a namespace.
#[inline]
fn __bop_validate_namespace_type(
    ns: &::bop::value::Value,
    alias: &str,
    type_name: &str,
    line: u32,
) -> Result<String, ::bop::error::BopError> {
    match ns {
        ::bop::value::Value::Module(m) => {
            m.type_origin(type_name).map(str::to_string).ok_or_else(|| {
                ::bop::error::BopError::runtime(
                    format!("`{}` isn't a type exported from `{}`", type_name, m.path),
                    line,
                )
            })
        }
        other => Err(::bop::error::BopError::runtime(
            format!(
                "`{}` is a {}, not a module alias — can't reach `{}` through it",
                alias,
                other.type_name(),
                type_name
            ),
            line,
        )),
    }
}

fn __bop_validate_named_fields(
    declared: &[&str],
    provided: &[&str],
    type_name: &str,
    variant: ::std::option::Option<&str>,
    line: u32,
) -> Result<(), ::bop::error::BopError> {
    for (index, field) in provided.iter().enumerate() {
        if provided[..index].contains(field) {
            let owner = match variant {
                ::std::option::Option::Some(variant) => format!("{}::{}", type_name, variant),
                ::std::option::Option::None => type_name.to_string(),
            };
            return Err(::bop::error::BopError::runtime(
                format!("Field `{}` specified twice in `{}` construction", field, owner),
                line,
            ));
        }
        if !declared.contains(field) {
            let message = match variant {
                ::std::option::Option::Some(variant) => {
                    ::bop::error_messages::variant_has_no_field(type_name, variant, field)
                }
                ::std::option::Option::None => {
                    ::bop::error_messages::struct_has_no_field(type_name, field)
                }
            };
            return Err(::bop::error::BopError::runtime(message, line));
        }
    }
    for field in declared {
        if !provided.contains(field) {
            let owner = match variant {
                ::std::option::Option::Some(variant) => format!("{}::{}", type_name, variant),
                ::std::option::Option::None => type_name.to_string(),
            };
            return Err(::bop::error::BopError::runtime(
                format!("Missing field `{}` in `{}` construction", field, owner),
                line,
            ));
        }
    }
    Ok(())
}

fn __bop_order_named_fields(
    declared: &[&str],
    mut provided: ::std::vec::Vec<(String, ::bop::value::Value)>,
) -> ::std::vec::Vec<(String, ::bop::value::Value)> {
    let mut ordered = ::std::vec::Vec::with_capacity(declared.len());
    for field in declared {
        let index = provided
            .iter()
            .position(|(name, _)| name == field)
            .expect("dynamic construction shape was validated before payload evaluation");
        ordered.push(provided.remove(index));
    }
    ordered
}

/// Write a struct field on the owned live-binding value and return it.
/// Moving the value into this helper keeps a unique CoW backing store unique.
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
            if !s.try_set_field(field, value, line)? {
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
    // Built-in `Result` combinators — pure methods inline;
    // callable-taking variants (`map`, `map_err`, `and_then`)
    // dispatch through `__bop_result_callable_method`.
    if ::bop::methods::is_builtin_result(obj) {
        if let Some(v) = ::bop::methods::result_method(obj, method, args, line)? {
            return Ok((v, None));
        }
        if let Some(kind) = ::bop::methods::is_result_callable_method(method) {
            let value = __bop_result_callable_method(ctx, obj, kind, method, args, line)?;
            return Ok((value, None));
        }
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
        ::bop::value::Value::Iter(_) => {
            ::bop::methods::iter_method(obj, method, args, line)
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

/// Runtime state for a `for x in v` loop. Fast path for the
/// three built-in iterables (eager materialisation into a Vec
/// with a cursor); protocol path for `Value::Iter` or user
/// types with an `.iter()` method (carries the iterator value
/// and calls `.next()` on each step).
enum __BopIterState {
    Eager {
        items: ::std::vec::Vec<::bop::value::Value>,
        pos: usize,
    },
    Protocol(::bop::value::Value),
}

/// Build the iterator state for a `for x in v` loop. Matches
/// the walker/VM rule: Array/Str/Dict take the fast path,
/// `Value::Iter` is used as-is, Struct/Enum go through their
/// `.iter()` method, everything else surfaces the "can't
/// iterate over X" error.
fn __bop_iter_start(
    ctx: &mut Ctx<'_>,
    mut v: ::bop::value::Value,
    line: u32,
) -> Result<__BopIterState, ::bop::error::BopError> {
    match &mut v {
        ::bop::value::Value::Array(arr) => Ok(__BopIterState::Eager {
            items: arr.take(),
            pos: 0,
        }),
        ::bop::value::Value::Str(s) => Ok(__BopIterState::Eager {
            items: s
                .chars()
                .map(|c| ::bop::value::Value::new_str(c.to_string()))
                .collect(),
            pos: 0,
        }),
        ::bop::value::Value::Dict(d) => Ok(__BopIterState::Eager {
            items: d
                .iter()
                .map(|(k, _)| ::bop::value::Value::new_str(k.clone()))
                .collect(),
            pos: 0,
        }),
        ::bop::value::Value::Iter(_) => Ok(__BopIterState::Protocol(v)),
        ::bop::value::Value::Struct(_) | ::bop::value::Value::EnumVariant(_) => {
            // Dispatch `.iter()` through the full method
            // resolution — user-declared methods (`fn
            // Bag.iter(self) { ... }`) must win before we fall
            // back to the built-in `__bop_call_method`.
            let iter_val = match __bop_try_user_method(
                ctx, &v, "iter", &[], line,
            )? {
                Some(iter_val) => iter_val,
                None => {
                    let (iter_val, _) =
                        __bop_call_method(ctx, &v, "iter", &[], line)?;
                    iter_val
                }
            };
            Ok(__BopIterState::Protocol(iter_val))
        }
        _ => Err(::bop::error::BopError::runtime(
            ::bop::error_messages::cant_iterate_over(v.type_name()),
            line,
        )),
    }
}

/// Advance the iterator by one, returning the next value or
/// `None` on exhaustion. Protocol-path advancement calls the
/// iterator's `.next()` method and pattern-matches the
/// resulting `Iter::Next(v)` / `Iter::Done` shape.
fn __bop_iter_step(
    ctx: &mut Ctx<'_>,
    state: &mut __BopIterState,
    line: u32,
) -> Result<Option<::bop::value::Value>, ::bop::error::BopError> {
    match state {
        __BopIterState::Eager { items, pos } => {
            if *pos < items.len() {
                // `items` was already taken from the source
                // array / string / dict, so the clone is of an
                // independent Vec — no aliasing.
                let v = items[*pos].clone();
                *pos += 1;
                Ok(Some(v))
            } else {
                Ok(None)
            }
        }
        __BopIterState::Protocol(iter_val) => {
            // Same user-first dispatch: a Bag that returns
            // `self` from `.iter()` plus a user `.next()` must
            // hit the user method, not the built-in iter table.
            let step = match __bop_try_user_method(
                ctx, iter_val, "next", &[], line,
            )? {
                Some(v) => v,
                None => {
                    let (v, _) =
                        __bop_call_method(ctx, iter_val, "next", &[], line)?;
                    v
                }
            };
            match &step {
                ::bop::value::Value::EnumVariant(e)
                    if e.type_name() == "Iter" =>
                {
                    match (e.variant(), e.payload()) {
                        (
                            "Next",
                            ::bop::value::EnumPayload::Tuple(items),
                        ) if items.len() == 1 => Ok(Some(items[0].clone())),
                        ("Done", ::bop::value::EnumPayload::Unit) => Ok(None),
                        _ => Err(::bop::error::BopError::runtime(
                            format!(
                                "`.next()` on a `for` iterator must return `Iter::Next(v)` or `Iter::Done`, got {}",
                                step.inspect()
                            ),
                            line,
                        )),
                    }
                }
                _ => Err(::bop::error::BopError::runtime(
                    format!(
                        "`.next()` on a `for` iterator must return `Iter::Next(v)` or `Iter::Done`, got {}",
                        step.inspect()
                    ),
                    line,
                )),
            }
        }
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
        module_aliases: ::std::collections::HashMap::new(),
        module_type_bindings: ::std::collections::HashMap::new(),
    };
    run_program(&mut ctx)
}

"#;

const MAIN_FN: &str = r#"fn main() {
    let mut host = ::bop_sys::StandardHost::new();
    if let Err(err) = run(&mut host) {
        eprintln!("{}", err);
        if let Some(hint) = &err.friendly_hint {
            eprintln!("hint: {}", hint);
        }
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
        module_aliases: ::std::collections::HashMap::new(),
        module_type_bindings: ::std::collections::HashMap::new(),
    };
    run_program(&mut ctx)
}

"#;

const MAIN_FN_SANDBOX: &str = r#"fn main() {
    let mut host = ::bop_sys::StandardHost::new();
    let limits = ::bop::BopLimits::standard();
    if let Err(err) = run(&mut host, &limits) {
        eprintln!("{}", err);
        if let Some(hint) = &err.friendly_hint {
            eprintln!("hint: {}", hint);
        }
        ::std::process::exit(1);
    }
}
"#;

#[cfg(test)]
mod tests {
    use super::emit;
    use crate::Options;

    fn emitted(source: &str) -> String {
        let statements = bop::parse(source).expect("parse test program");
        emit(&statements, &Options::default()).expect("emit test program")
    }

    #[test]
    fn mutating_ident_array_uses_in_place_dispatch() {
        let output = emitted("let a = [1]\na.push(2)");

        assert!(
            output.contains(
                "if let ::bop::value::Value::Array(__bop_array) = &mut __bop_user_value_61"
            ),
            "missing direct mutable Array binding:\n{output}"
        );
        assert!(
            output.contains("::bop::methods::array_method_mut(__bop_array, \"push\""),
            "missing shared in-place array dispatch:\n{output}"
        );
        assert!(
            output.contains("} else { let ")
                && output.contains(
                    "= __bop_user_value_61.clone(); match __bop_try_user_method"
                ),
            "dynamic non-array fallback must retain user-method dispatch:\n{output}"
        );
    }

    #[test]
    fn nested_mutating_argument_is_emitted_before_outer_receiver_borrow() {
        let output = emitted("let a = [1, 2]\na.push(a.pop())");
        let pop = output
            .find("::bop::methods::array_method_mut(__bop_array, \"pop\"")
            .expect("inner pop fast path");
        let push = output
            .find("::bop::methods::array_method_mut(__bop_array, \"push\"")
            .expect("outer push fast path");

        assert!(
            pop < push,
            "argument expression must execute before the outer receiver borrow:\n{output}"
        );
    }

    #[test]
    fn mutating_non_ident_keeps_value_dispatch_path() {
        let output = emitted("[1].push(2)");

        assert!(
            !output.contains("::bop::methods::array_method_mut(__bop_array, \"push\""),
            "temporary receiver must not use identifier write-back fast path:\n{output}"
        );
        assert!(
            output.contains("__bop_call_method(ctx,"),
            "temporary receiver should retain generic value dispatch:\n{output}"
        );
    }
}
