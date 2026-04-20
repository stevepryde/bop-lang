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
use bop::parser::{AssignOp, AssignTarget, BinOp, Expr, ExprKind, Stmt, StmtKind, UnaryOp};

use crate::Options;

pub(crate) fn emit(stmts: &[Stmt], opts: &Options) -> Result<String, BopError> {
    // Pre-resolve every import in the program's transitive graph.
    // Failures here (missing resolver, module not found, cycle)
    // surface before any Rust is written.
    let modules = build_module_graph(stmts, opts)?;
    let info = collect_fn_info(stmts);
    let mut emitter = Emitter::new(opts.clone(), info, modules);
    emitter.emit_program(stmts)?;
    Ok(emitter.finish())
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
    /// semantics (`import` re-exports by default).
    pub effective_exports: Vec<String>,
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
                "bop-compile: `import` requires `Options::module_resolver` to be set so the transpiler can inline the imported modules",
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

    graph.modules.insert(
        name.to_string(),
        ModuleEntry {
            ast,
            own_fns,
            own_lets,
            direct_imports: direct_imports.into_iter().map(|(n, _)| n).collect(),
            effective_exports: exports,
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
        if let StmtKind::Import { path } = &stmt.kind {
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
    /// Counter for temporary locals (`__t0`, `__t1`, …). Reset at
    /// the start of each fn / top-level program so the names stay
    /// short.
    tmp_counter: usize,
    /// Non-empty while emitting an imported module's body — the
    /// prefix (e.g. `"foo__bar__"`) is applied to every user fn
    /// name so modules can't collide on function identifiers.
    module_prefix: String,
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
}

impl Emitter {
    fn new(opts: Options, fn_info: FnInfo, modules: ModuleGraph) -> Self {
        Self {
            out: String::new(),
            indent: 0,
            opts,
            fn_info,
            modules,
            tmp_counter: 0,
            module_prefix: String::new(),
            imported_in_scope: HashSet::new(),
            scope_stack: Vec::new(),
        }
    }

    fn push_scope(&mut self) {
        self.scope_stack.push(HashSet::new());
    }

    fn pop_scope(&mut self) {
        self.scope_stack.pop();
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

    fn emit_program(&mut self, stmts: &[Stmt]) -> Result<(), BopError> {
        let module_name = self.opts.module_name.clone();
        if let Some(ref name) = module_name {
            writeln!(self.out, "pub mod {} {{", name).unwrap();
        }
        self.emit_header();
        self.emit_runtime_preamble();
        // Imported modules emit first (topo-ordered — leaves
        // first) so their fns, exports, and load fns exist by
        // the time the root program's code references them.
        self.emit_imported_modules()?;
        // Top-level fn declarations move out of `run_program`'s
        // body to module scope. That way the `__bop_fn_value_*`
        // wrappers (also at module scope) can reference them, and
        // `let g = fib` works even before `fib` is called.
        self.emit_top_level_fn_decls(stmts)?;
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
        let saved_prefix = std::mem::replace(
            &mut self.module_prefix,
            format!("{}__", module_slug(name)),
        );
        let saved_fn_info =
            std::mem::replace(&mut self.fn_info, collect_fn_info(&entry.ast));

        self.emit_top_level_fn_decls(&entry.ast)?;
        self.emit_fn_value_wrappers();
        self.emit_module_exports_struct(name, entry);
        self.emit_module_load_fn(name, entry)?;

        self.fn_info = saved_fn_info;
        self.module_prefix = saved_prefix;
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

    /// Emit an `import foo.bar` statement as Rust code that loads
    /// the module once (via its `__mod_*__load` fn, which handles
    /// caching + cycle detection) and then unpacks each of its
    /// effective exports as a local Bop binding. Matches the
    /// walker's flat-injection semantics.
    fn emit_import_stmt(&mut self, path: &str, line: u32) -> Result<(), BopError> {
        // Idempotent at the injection site: re-importing a module
        // we already injected into this scope is a no-op. Matches
        // the walker's `imported_here` short-circuit.
        if self.imported_in_scope.contains(path) {
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
        let tmp = self.fresh_tmp();
        self.line(&format!(
            "let {tmp} = {load}(ctx)?;",
            tmp = tmp,
            load = module_load_fn_name(path)
        ));
        for export in &entry.effective_exports {
            // Shadow-check: the importer can't already have this
            // name in its scope. Emitted as a transpile-time
            // guard so it mirrors the walker / VM behaviour.
            if self.is_local(export) {
                return Err(BopError::runtime(
                    format!(
                        "Import of `{}` from `{}` would shadow an existing binding",
                        export, path
                    ),
                    line,
                ));
            }
            self.line(&format!(
                "let mut {ident}: ::bop::value::Value = {tmp}.{ident}.clone();",
                ident = rust_ident(export),
                tmp = tmp
            ));
            self.bind_local(export);
        }
        self.imported_in_scope.insert(path.to_string());
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

    // ─── Preamble / footer ────────────────────────────────────────

    fn emit_header(&mut self) {
        self.out.push_str(HEADER);
    }

    fn emit_runtime_preamble(&mut self) {
        if self.opts.sandbox {
            self.out.push_str(RUNTIME_PREAMBLE_SANDBOX);
        } else {
            self.out.push_str(RUNTIME_PREAMBLE);
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
            StmtKind::Let { name, value } => {
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

            StmtKind::Import { path } => {
                self.emit_import_stmt(path, line)?;
            }

            StmtKind::StructDecl { .. } => {
                return Err(BopError::runtime(
                    "bop-compile: struct declarations are not yet supported by the AOT transpiler",
                    line,
                ));
            }

            StmtKind::EnumDecl { .. } => {
                return Err(BopError::runtime(
                    "bop-compile: enum declarations are not yet supported by the AOT transpiler",
                    line,
                ));
            }

            StmtKind::MethodDecl { .. } => {
                return Err(BopError::runtime(
                    "bop-compile: user-defined methods are not yet supported by the AOT transpiler",
                    line,
                ));
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
            AssignTarget::Field { .. } => Err(BopError::runtime(
                "bop-compile: struct field assignment is not yet supported by the AOT transpiler",
                line,
            )),
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

            ExprKind::FieldAccess { .. } => {
                return Err(BopError::runtime(
                    "bop-compile: struct field access (obj.field) is not yet supported by the AOT transpiler",
                    line,
                ));
            }

            ExprKind::StructConstruct { .. } => {
                return Err(BopError::runtime(
                    "bop-compile: struct literals are not yet supported by the AOT transpiler",
                    line,
                ));
            }

            ExprKind::EnumConstruct { .. } => {
                return Err(BopError::runtime(
                    "bop-compile: enum variant construction is not yet supported by the AOT transpiler",
                    line,
                ));
            }

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
        };
        Ok(s)
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
            "str" | "int" | "type" | "abs" | "min" | "max" | "len" | "inspect" => {
                let fn_name = format!("builtin_{}", name);
                format!(
                    "::bop::builtins::{}(&{}, {})?",
                    fn_name,
                    build_arg_array(&arg_names),
                    line
                )
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
                    let hint_fallback = format!(
                        "{{ let hint = ctx.host.function_hint(); if hint.is_empty() {{ ::bop::error::BopError::runtime(format!(\"Function `{}` not found\"), {}) }} else {{ let mut e = ::bop::error::BopError::runtime(format!(\"Function `{}` not found\"), {}); e.friendly_hint = Some(hint.to_string()); e }} }}",
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
        let args_arr = build_arg_array(&arg_tmps);

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
        match ident_target {
            Some(target) => {
                write!(
                    body,
                    "let (__ret, __mutated) = __bop_call_method(&{}, {}, &{}, {})?; \
                     if let Some(__new_obj) = __mutated {{ {} = __new_obj; }} \
                     __ret }}",
                    obj_tmp, method_lit, args_arr, line, target
                )
                .unwrap();
            }
            None => {
                write!(
                    body,
                    "let (__ret, _) = __bop_call_method(&{}, {}, &{}, {})?; __ret }}",
                    obj_tmp, method_lit, args_arr, line
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
        // the tmp counter.
        let saved_out = core::mem::take(&mut self.out);
        let saved_indent = self.indent;
        let saved_tmp = self.tmp_counter;
        self.indent = 0;
        self.tmp_counter = 0;
        for s in body {
            self.emit_stmt(s)?;
        }
        let body_src = core::mem::replace(&mut self.out, saved_out);
        self.indent = saved_indent;
        self.tmp_counter = saved_tmp;

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
        StmtKind::Let { name, value } => {
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
        StmtKind::Import { .. } => {
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

fn scan_free_vars_expr(
    expr: &Expr,
    known: &mut HashSet<String>,
    free: &mut std::collections::BTreeSet<String>,
    outer_scopes: &[HashSet<String>],
    fn_info: &FnInfo,
) {
    match &expr.kind {
        ExprKind::Number(_)
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
/// keywords with the raw-identifier prefix when needed.
fn rust_ident(name: &str) -> String {
    if is_rust_keyword(name) {
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

const RUNTIME_PREAMBLE: &str = r#"// ─── Runtime context and helpers ────────────────────────────────

pub struct Ctx<'h> {
    pub host: &'h mut dyn ::bop::BopHost,
    pub rand_state: u64,
    /// Per-program module cache keyed by the Bop module path (the
    /// dot-joined string). `load` fns use this to memoise imports
    /// and to spot circular dependencies via the `__ModuleLoading`
    /// sentinel.
    pub module_cache:
        ::std::collections::HashMap<::std::string::String, ::std::boxed::Box<dyn ::core::any::Any + 'static>>,
}

/// Sentinel type inserted into `module_cache` while a module's
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
            format!("Can't call a {}", other.type_name()),
            line,
        )),
    }
}

/// Mirror of Evaluator::call_method from bop-lang: dispatches a
/// method call to the right family for the receiver's type.
#[inline]
fn __bop_call_method(
    obj: &::bop::value::Value,
    method: &str,
    args: &[::bop::value::Value],
    line: u32,
) -> Result<(::bop::value::Value, Option<::bop::value::Value>), ::bop::error::BopError> {
    match obj {
        ::bop::value::Value::Array(arr) => ::bop::methods::array_method(arr, method, args, line),
        ::bop::value::Value::Str(s) => ::bop::methods::string_method(s.as_str(), method, args, line),
        ::bop::value::Value::Dict(d) => ::bop::methods::dict_method(d, method, args, line),
        _ => Err(::bop::error::BopError::runtime(
            format!("{} doesn't have a .{}() method", obj.type_name(), method),
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
            format!("Can't iterate over {}", v.type_name()),
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

// ─── Sandbox variants ──────────────────────────────────────────────
//
// Same shape as the non-sandbox preamble but:
// - `Ctx` carries `steps` and `max_steps`.
// - A `__bop_tick` helper enforces the step budget, re-checks the
//   allocation tracker's ceiling, and fans the tick out to the
//   host's `on_tick` hook.
// - `run` takes a `&BopLimits` so callers can dial the budget.

const RUNTIME_PREAMBLE_SANDBOX: &str = r#"// ─── Runtime context and helpers ────────────────────────────────

pub struct Ctx<'h> {
    pub host: &'h mut dyn ::bop::BopHost,
    pub rand_state: u64,
    pub steps: u64,
    pub max_steps: u64,
    pub module_cache:
        ::std::collections::HashMap<::std::string::String, ::std::boxed::Box<dyn ::core::any::Any + 'static>>,
}

/// Sentinel inserted into `module_cache` while a module's body is
/// evaluating. See the non-sandbox preamble for details.
pub struct __ModuleLoading;

/// Opaque body that a `Value::Fn` carries around in AOT-emitted
/// code. See the non-sandbox preamble for the rationale.
pub struct AotClosure {
    pub callable: ::std::rc::Rc<
        dyn for<'__a> Fn(
            &mut Ctx<'__a>,
            ::std::vec::Vec<::bop::value::Value>,
        ) -> Result<::bop::value::Value, ::bop::error::BopError>,
    >,
}

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
            format!("Can't call a {}", other.type_name()),
            line,
        )),
    }
}

/// Step / memory / on_tick checkpoint. Emitted by `bop-compile` at
/// the head of every loop iteration and at function entry when the
/// `sandbox` option is on. Mirrors `Evaluator::tick` in `bop-lang`.
#[inline]
fn __bop_tick(ctx: &mut Ctx<'_>, line: u32) -> Result<(), ::bop::error::BopError> {
    ctx.steps += 1;
    if ctx.steps > ctx.max_steps {
        return Err(::bop::builtins::error_with_hint(
            line,
            "Your code took too many steps (possible infinite loop)",
            "Check your loops — make sure they have a condition that eventually stops them.",
        ));
    }
    if ::bop::memory::bop_memory_exceeded() {
        return Err(::bop::builtins::error_with_hint(
            line,
            "Memory limit exceeded",
            "Your code is using too much memory. Check for large strings or arrays growing in loops.",
        ));
    }
    ctx.host.on_tick()?;
    Ok(())
}

/// Mirror of Evaluator::call_method from bop-lang: dispatches a
/// method call to the right family for the receiver's type.
#[inline]
fn __bop_call_method(
    obj: &::bop::value::Value,
    method: &str,
    args: &[::bop::value::Value],
    line: u32,
) -> Result<(::bop::value::Value, Option<::bop::value::Value>), ::bop::error::BopError> {
    match obj {
        ::bop::value::Value::Array(arr) => ::bop::methods::array_method(arr, method, args, line),
        ::bop::value::Value::Str(s) => ::bop::methods::string_method(s.as_str(), method, args, line),
        ::bop::value::Value::Dict(d) => ::bop::methods::dict_method(d, method, args, line),
        _ => Err(::bop::error::BopError::runtime(
            format!("{} doesn't have a .{}() method", obj.type_name(), method),
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
            format!("Can't iterate over {}", v.type_name()),
            line,
        )),
    }
}

"#;

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

