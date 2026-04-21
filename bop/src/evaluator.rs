#[cfg(not(feature = "std"))]
use alloc::{format, string::{String, ToString}, vec, vec::Vec};

use alloc_import::collections::BTreeMap;

#[cfg(feature = "std")]
use std as alloc_import;
#[cfg(not(feature = "std"))]
use alloc as alloc_import;

#[cfg(not(feature = "std"))]
use alloc::rc::Rc;

#[cfg(feature = "std")]
use std::rc::Rc;

use core::cell::RefCell;

use crate::builtins::{self, error, error_fatal_with_hint, error_with_hint};
use crate::error::BopError;
use crate::lexer::StringPart;
use crate::methods;
use crate::ops;
use crate::parser::*;
use crate::value::{BopFn, EnumPayload, FnBody, Value};
use crate::{BopHost, BopLimits};

/// What the tree-walker stores for each imported module once it
/// has been loaded. Cached by dot-joined path so the same module
/// imported twice in one `run` only evaluates once.
///
/// `Loading` is the in-progress sentinel; if an import request
/// sees a module already in this state it's a circular import and
/// halts with a clear error.
enum ImportSlot {
    Loading,
    Loaded(ModuleBindings),
}

#[derive(Clone)]
struct ModuleBindings {
    /// `(name, value)` pairs for every top-level `let` in the
    /// module (fns are handled separately — they also need to
    /// land in `self.functions` so cross-fn calls within the
    /// module resolve).
    bindings: Vec<(String, Value)>,
    /// Top-level `fn` declarations, keyed by name. The importer
    /// registers each both in its `self.functions` table
    /// (for nested call resolution) and as a scope binding
    /// (so the fn is also usable as a first-class value).
    fn_decls: Vec<(String, FnDef)>,
    /// Struct type declarations the module introduces. Copied
    /// into the importing evaluator's `struct_defs` so
    /// `MyStruct { ... }` construction in the caller resolves.
    struct_defs: Vec<(String, Vec<String>)>,
    /// Enum type declarations the module introduces. Same role
    /// as `struct_defs` but for sum types.
    enum_defs: Vec<(String, Vec<crate::parser::VariantDecl>)>,
    /// User methods the module declared. `(type_name, method_name,
    /// fn_def)` — applied to the caller's `methods` table.
    methods: Vec<(String, String, FnDef)>,
}

type ImportCache = Rc<RefCell<alloc_import::collections::BTreeMap<String, ImportSlot>>>;

const MAX_CALL_DEPTH: usize = 64;

// `try` unwinds via `BopError::try_return_signal` — a proper
// sentinel with `is_try_return = true` rather than the older
// magic-string approach. The value travels on
// `Evaluator::pending_try_return` so the `BopError` itself
// doesn't need to carry a `Value` (which would introduce a
// module cycle between `bop::error` and `bop::value`).

// ─── Control flow signals ──────────────────────────────────────────────────

enum Signal {
    None,
    Break,
    Continue,
    Return(Value),
}

#[derive(Clone)]
struct FnDef {
    params: Vec<String>,
    body: Vec<Stmt>,
}

// ─── Evaluator ─────────────────────────────────────────────────────────────

pub struct Evaluator<'h, H: BopHost> {
    scopes: Vec<BTreeMap<String, Value>>,
    functions: BTreeMap<String, FnDef>,
    /// User-defined struct types in this run. Keyed by the
    /// declared type name; value is the declared field list in
    /// declaration order.
    struct_defs: BTreeMap<String, Vec<String>>,
    /// User-defined enum types. Key is the enum name; value is
    /// the full variant list so construction sites can validate
    /// shapes.
    enum_defs: BTreeMap<String, Vec<VariantDecl>>,
    /// User-defined methods. Outer key is the receiver type name
    /// (e.g. `"Point"`); inner key is the method name; value is
    /// the fn body. Methods receive the receiver as their first
    /// parameter (conventionally `self`).
    methods: BTreeMap<String, BTreeMap<String, FnDef>>,
    host: &'h mut H,
    steps: u64,
    call_depth: usize,
    limits: BopLimits,
    rand_state: u64,
    /// Shared across nested evaluators so recursive imports see
    /// the same cache — every sub-evaluator inherits the parent's
    /// `Rc` clone in `new_nested`.
    imports: ImportCache,
    /// Paths already injected into *this* evaluator's scope (not
    /// shared with sub-evaluators). Re-importing the same path at
    /// the same level is a no-op — matches Python's `import x;
    /// import x` behaviour.
    imported_here: alloc_import::collections::BTreeSet<String>,
    /// Set by `try` when it sees an `Err(...)` variant and wants
    /// the enclosing fn to early-return with that value. The
    /// expression that raised stuffs the value here and returns
    /// a sentinel `BopError`; the fn-call wrapper detects the
    /// sentinel, takes the value, and converts it to
    /// `Signal::Return`. Always `None` outside the narrow
    /// unwinding window.
    pending_try_return: Option<Value>,
    /// Non-fatal runtime warnings accumulated during execution —
    /// currently only `use`-time name-shadowing events (glob
    /// imports bringing in a name that's already bound). Read
    /// after `run()` returns via [`Self::take_warnings`].
    runtime_warnings: Vec<crate::error::BopWarning>,
}

impl<'h, H: BopHost> Evaluator<'h, H> {
    pub fn new(host: &'h mut H, limits: BopLimits) -> Self {
        crate::memory::bop_memory_init(limits.max_memory);
        let (struct_defs, enum_defs) = seed_builtin_types();
        Self {
            scopes: vec![BTreeMap::new()],
            functions: BTreeMap::new(),
            struct_defs,
            enum_defs,
            methods: BTreeMap::new(),
            host,
            steps: 0,
            call_depth: 0,
            limits,
            rand_state: 0,
            imports: Rc::new(RefCell::new(alloc_import::collections::BTreeMap::new())),
            imported_here: alloc_import::collections::BTreeSet::new(),
            pending_try_return: None,
            runtime_warnings: Vec::new(),
        }
    }

    /// Build a sub-evaluator for loading a module — inherits the
    /// parent's import cache, memory ceiling, and step budget, but
    /// runs with a fresh scope stack so module code can't see the
    /// importer's locals.
    fn new_for_module(
        host: &'h mut H,
        limits: BopLimits,
        imports: ImportCache,
    ) -> Self {
        let (struct_defs, enum_defs) = seed_builtin_types();
        Self {
            scopes: vec![BTreeMap::new()],
            functions: BTreeMap::new(),
            struct_defs,
            enum_defs,
            methods: BTreeMap::new(),
            host,
            steps: 0,
            call_depth: 0,
            limits,
            rand_state: 0,
            imports,
            imported_here: alloc_import::collections::BTreeSet::new(),
            pending_try_return: None,
            runtime_warnings: Vec::new(),
        }
    }

    pub fn run(mut self, stmts: &[Stmt]) -> Result<(), BopError> {
        let result = self.exec_block(stmts);
        #[cfg(feature = "std")]
        {
            // Surface any runtime warnings accumulated during the
            // run (currently: glob-import shadowing). They land on
            // stderr with the standard `warning:` prefix so
            // terminal users see them — no public API change
            // needed. Embedders that want structured access can
            // use `run_with_warnings` (future) or implement their
            // own evaluation loop.
            for w in &self.runtime_warnings {
                eprintln!("warning: {}", w.message);
            }
        }
        match result? {
            Signal::Break => {
                return Err(error(0, "break used outside of a loop"));
            }
            Signal::Continue => {
                return Err(error(0, "continue used outside of a loop"));
            }
            _ => {}
        }
        Ok(())
    }

    fn tick(&mut self, line: u32) -> Result<(), BopError> {
        self.steps += 1;
        if self.steps > self.limits.max_steps {
            // Fatal — `try_call` must not swallow this, or the
            // sandbox invariant breaks.
            return Err(error_fatal_with_hint(
                line,
                "Your code took too many steps (possible infinite loop)",
                "Check your loops — make sure they have a condition that eventually stops them.",
            ));
        }
        if crate::memory::bop_memory_exceeded() {
            return Err(error_fatal_with_hint(
                line,
                "Memory limit exceeded",
                "Your code is using too much memory. Check for large strings or arrays growing in loops.",
            ));
        }
        self.host.on_tick()?;
        Ok(())
    }

    // ─── Scope ─────────────────────────────────────────────────────

    fn push_scope(&mut self) {
        self.scopes.push(BTreeMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn define(&mut self, name: String, value: Value) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name, value);
        }
    }

    fn get_var(&self, name: &str) -> Option<&Value> {
        for scope in self.scopes.iter().rev() {
            if let Some(val) = scope.get(name) {
                return Some(val);
            }
        }
        None
    }

    fn set_var(&mut self, name: &str, value: Value) -> bool {
        for scope in self.scopes.iter_mut().rev() {
            if scope.contains_key(name) {
                scope.insert(name.to_string(), value);
                return true;
            }
        }
        false
    }

    // ─── "Did you mean?" candidate collectors ─────────────────
    //
    // Every name the user could reasonably have meant, gathered
    // into a single list so `bop::suggest::did_you_mean` picks
    // the closest match. Separate methods for the "ident used
    // as a value" and "ident called as a function" paths since
    // the reachable sets differ (builtins are callable but
    // aren't scope values).

    /// Names reachable when an identifier is used as a value:
    /// any local from the enclosing scopes plus any top-level
    /// fn declaration (fns are first-class — `let g = some_fn`
    /// works, so they count as value-like).
    fn value_candidates_hint(&self, target: &str) -> Option<String> {
        let mut candidates: Vec<String> = Vec::new();
        for scope in &self.scopes {
            for k in scope.keys() {
                candidates.push(k.clone());
            }
        }
        for name in self.functions.keys() {
            candidates.push(name.clone());
        }
        crate::suggest::did_you_mean(target, candidates)
    }

    /// Names reachable in a call position: user fns, core
    /// builtins, plus `try_call`. Host builtins stay with the
    /// host's own `function_hint()` path — embedders often want
    /// to list theirs differently.
    fn callable_candidates_hint(&self, target: &str) -> Option<String> {
        let mut candidates: Vec<String> =
            self.functions.keys().cloned().collect();
        for builtin in crate::suggest::CORE_CALLABLE_BUILTINS {
            candidates.push((*builtin).to_string());
        }
        crate::suggest::did_you_mean(target, candidates)
    }

    // ─── Statements ────────────────────────────────────────────────

    fn exec_block(&mut self, stmts: &[Stmt]) -> Result<Signal, BopError> {
        for stmt in stmts {
            let signal = self.exec_stmt(stmt)?;
            match signal {
                Signal::None => {}
                other => return Ok(other),
            }
        }
        Ok(Signal::None)
    }

    fn exec_stmt(&mut self, stmt: &Stmt) -> Result<Signal, BopError> {
        self.tick(stmt.line)?;

        match &stmt.kind {
            StmtKind::Let { name, value, is_const: _ } => {
                let val = self.eval_expr(value)?;
                self.define(name.clone(), val);
                Ok(Signal::None)
            }

            StmtKind::Assign { target, op, value } => {
                let new_val = self.eval_expr(value)?;
                self.exec_assign(target, op, new_val, stmt.line)?;
                Ok(Signal::None)
            }

            StmtKind::If {
                condition,
                body,
                else_ifs,
                else_body,
            } => {
                if self.eval_expr(condition)?.is_truthy() {
                    self.push_scope();
                    let sig = self.exec_block(body)?;
                    self.pop_scope();
                    return Ok(sig);
                }
                for (elif_cond, elif_body) in else_ifs {
                    if self.eval_expr(elif_cond)?.is_truthy() {
                        self.push_scope();
                        let sig = self.exec_block(elif_body)?;
                        self.pop_scope();
                        return Ok(sig);
                    }
                }
                if let Some(else_body) = else_body {
                    self.push_scope();
                    let sig = self.exec_block(else_body)?;
                    self.pop_scope();
                    return Ok(sig);
                }
                Ok(Signal::None)
            }

            StmtKind::While { condition, body } => {
                loop {
                    self.tick(stmt.line)?;
                    if !self.eval_expr(condition)?.is_truthy() {
                        break;
                    }
                    self.push_scope();
                    let sig = self.exec_block(body)?;
                    self.pop_scope();
                    match sig {
                        Signal::Break => break,
                        Signal::Continue => continue,
                        Signal::Return(v) => return Ok(Signal::Return(v)),
                        Signal::None => {}
                    }
                }
                Ok(Signal::None)
            }

            StmtKind::Repeat { count, body } => {
                let n = match self.eval_expr(count)? {
                    Value::Int(n) => n,
                    Value::Number(n) => n as i64,
                    other => {
                        return Err(error(
                            stmt.line,
                            format!("repeat needs a number, but got {}", other.type_name()),
                        ));
                    }
                };
                for _ in 0..n.max(0) {
                    self.tick(stmt.line)?;
                    self.push_scope();
                    let sig = self.exec_block(body)?;
                    self.pop_scope();
                    match sig {
                        Signal::Break => break,
                        Signal::Continue => continue,
                        Signal::Return(v) => return Ok(Signal::Return(v)),
                        Signal::None => {}
                    }
                }
                Ok(Signal::None)
            }

            StmtKind::ForIn {
                var,
                iterable,
                body,
            } => {
                let mut val = self.eval_expr(iterable)?;
                let items = match &mut val {
                    Value::Array(arr) => arr.take(),
                    Value::Str(s) => s.chars().map(|c| Value::new_str(c.to_string())).collect(),
                    other => {
                        return Err(error(
                            stmt.line,
                            crate::error_messages::cant_iterate_over(other.type_name()),
                        ));
                    }
                };
                for item in items {
                    self.tick(stmt.line)?;
                    self.push_scope();
                    self.define(var.clone(), item);
                    let sig = self.exec_block(body)?;
                    self.pop_scope();
                    match sig {
                        Signal::Break => break,
                        Signal::Continue => continue,
                        Signal::Return(v) => return Ok(Signal::Return(v)),
                        Signal::None => {}
                    }
                }
                Ok(Signal::None)
            }

            StmtKind::FnDecl { name, params, body } => {
                self.functions.insert(
                    name.clone(),
                    FnDef {
                        params: params.clone(),
                        body: body.clone(),
                    },
                );
                Ok(Signal::None)
            }

            StmtKind::MethodDecl {
                type_name,
                method_name,
                params,
                body,
            } => {
                self.methods
                    .entry(type_name.clone())
                    .or_default()
                    .insert(
                        method_name.clone(),
                        FnDef {
                            params: params.clone(),
                            body: body.clone(),
                        },
                    );
                Ok(Signal::None)
            }

            StmtKind::Return { value } => {
                let val = match value {
                    Some(expr) => self.eval_expr(expr)?,
                    None => Value::None,
                };
                Ok(Signal::Return(val))
            }

            StmtKind::Break => Ok(Signal::Break),
            StmtKind::Continue => Ok(Signal::Continue),

            StmtKind::Use { path, items, alias } => {
                self.exec_import(
                    path,
                    items.as_deref(),
                    alias.as_deref(),
                    stmt.line,
                )?;
                Ok(Signal::None)
            }

            StmtKind::StructDecl { name, fields } => {
                // Reject duplicate field names at decl time so
                // downstream code doesn't have to re-check. Walker,
                // VM, and AOT all assume unique fields per struct.
                let mut seen = alloc_import::collections::BTreeSet::new();
                for f in fields {
                    if !seen.insert(f.clone()) {
                        return Err(error(
                            stmt.line,
                            format!("Struct `{}` has duplicate field `{}`", name, f),
                        ));
                    }
                }
                if let Some(existing) = self.struct_defs.get(name) {
                    // Same-shape redeclaration is allowed — that's
                    // the rule `use` already uses for idempotent
                    // re-imports, and it lets engine-wide builtins
                    // (`RuntimeError`) coexist with a user program
                    // that happens to redeclare them with the same
                    // fields (e.g. a legacy `use std.result`).
                    if existing == fields {
                        return Ok(Signal::None);
                    }
                    return Err(error(
                        stmt.line,
                        format!("Struct `{}` is already declared", name),
                    ));
                }
                self.struct_defs.insert(name.clone(), fields.clone());
                Ok(Signal::None)
            }

            StmtKind::EnumDecl { name, variants } => {
                if let Some(existing) = self.enum_defs.get(name) {
                    // Same rule as `StructDecl` above — a
                    // same-shape redeclaration of a builtin (or
                    // of a type a module already brought in) is a
                    // no-op, anything else is a clash.
                    if variants_equivalent(existing, variants) {
                        return Ok(Signal::None);
                    }
                    return Err(error(
                        stmt.line,
                        format!("Enum `{}` is already declared", name),
                    ));
                }
                let mut seen_variants = alloc_import::collections::BTreeSet::new();
                for v in variants {
                    if !seen_variants.insert(v.name.clone()) {
                        return Err(error(
                            stmt.line,
                            format!(
                                "Enum `{}` has duplicate variant `{}`",
                                name, v.name
                            ),
                        ));
                    }
                    if let VariantKind::Struct(fields) | VariantKind::Tuple(fields) =
                        &v.kind
                    {
                        let mut seen_fields = alloc_import::collections::BTreeSet::new();
                        for f in fields {
                            if !seen_fields.insert(f.clone()) {
                                return Err(error(
                                    stmt.line,
                                    format!(
                                        "Enum variant `{}::{}` has duplicate field `{}`",
                                        name, v.name, f
                                    ),
                                ));
                            }
                        }
                    }
                }
                self.enum_defs.insert(name.clone(), variants.clone());
                Ok(Signal::None)
            }

            StmtKind::ExprStmt(expr) => {
                self.eval_expr(expr)?;
                Ok(Signal::None)
            }
        }
    }

    /// Execute a `use` statement.
    ///
    /// Four shapes are dispatched from the parser:
    ///
    /// - `use foo`                 — glob: inject all public
    ///   (non-`_`-prefixed) exports into the caller's scope.
    ///   Name collisions emit a `BopWarning` and the first
    ///   binding wins (already-present beats newcomer).
    /// - `use foo.{a, b}`          — selective: inject only the
    ///   listed names. Missing names raise a clear error. Names
    ///   that start with `_` are accepted when listed
    ///   explicitly (the selective form is how you opt-in to
    ///   private bindings).
    /// - `use foo as m`            — aliased: every export
    ///   (including `_`-prefixed) hangs off a `Value::Module`
    ///   bound as `m`. Access via `m.binding` or
    ///   `m.Type { ... }` / `m.Type::Variant(...)`.
    /// - `use foo.{a, b} as m`     — selective + aliased: same
    ///   filtering, and the resulting module only exposes the
    ///   listed names.
    ///
    /// Struct / enum / method declarations from the imported
    /// module still register globally on every form — types
    /// are first-come-first-served across the engine. Conflicts
    /// there produce the same "clashes with existing" error as
    /// before. (Qualified type names that genuinely disambiguate
    /// same-named types across modules are a future extension.)
    fn exec_import(
        &mut self,
        path: &str,
        items: Option<&[String]>,
        alias: Option<&str>,
        line: u32,
    ) -> Result<(), BopError> {
        // Idempotent at the glob injection site: re-importing a
        // module already applied is a no-op. Aliased / selective
        // forms don't enter this cache — they always run, because
        // the same module imported with different shapes can
        // legitimately produce different scope effects.
        let is_plain_glob = items.is_none() && alias.is_none();
        if is_plain_glob && self.imported_here.contains(path) {
            return Ok(());
        }

        let bindings = self.load_module(path, line)?;

        // Types (struct / enum / method) always register globally
        // regardless of form.
        for (name, fields) in &bindings.struct_defs {
            if let Some(existing) = self.struct_defs.get(name) {
                if existing == fields {
                    continue;
                }
                return Err(error(
                    line,
                    format!(
                        "Use of `{}` from `{}` clashes with an existing struct of the same name",
                        name, path
                    ),
                ));
            }
            self.struct_defs.insert(name.clone(), fields.clone());
        }
        for (name, variants) in &bindings.enum_defs {
            if let Some(existing) = self.enum_defs.get(name) {
                if variants_equivalent(existing, variants) {
                    continue;
                }
                return Err(error(
                    line,
                    format!(
                        "Use of `{}` from `{}` clashes with an existing enum of the same name",
                        name, path
                    ),
                ));
            }
            self.enum_defs.insert(name.clone(), variants.clone());
        }
        for (type_name, method_name, fn_def) in &bindings.methods {
            let slot = self.methods.entry(type_name.clone()).or_default();
            slot.insert(method_name.clone(), fn_def.clone());
        }

        // Figure out which name-value pairs to surface based on
        // the (items, alias) combination. Fn declarations and
        // plain `let` bindings are threaded together so the
        // caller-visible order matches the module's declaration
        // order.
        let mut exports: Vec<(String, Value)> =
            Vec::with_capacity(bindings.fn_decls.len() + bindings.bindings.len());
        let mut fn_entries: Vec<(String, FnDef)> =
            Vec::with_capacity(bindings.fn_decls.len());
        for (name, fn_def) in &bindings.fn_decls {
            let value = Value::new_fn(
                fn_def.params.clone(),
                Vec::new(),
                fn_def.body.clone(),
                Some(name.clone()),
            );
            exports.push((name.clone(), value));
            fn_entries.push((name.clone(), fn_def.clone()));
        }
        for (name, value) in &bindings.bindings {
            exports.push((name.clone(), value.clone()));
        }

        // Selective filter: restrict to the listed names.
        // Missing names error loudly — a silent skip would make
        // typos hard to spot.
        if let Some(list) = items {
            let available: alloc_import::collections::BTreeSet<&str> =
                exports.iter().map(|(k, _)| k.as_str()).collect();
            for wanted in list {
                if !available.contains(wanted.as_str())
                    && !bindings
                        .struct_defs
                        .iter()
                        .any(|(k, _)| k == wanted)
                    && !bindings
                        .enum_defs
                        .iter()
                        .any(|(k, _)| k == wanted)
                {
                    return Err(error(
                        line,
                        format!(
                            "`{}` isn't exported from `{}` (selective import)",
                            wanted, path
                        ),
                    ));
                }
            }
            let listed: alloc_import::collections::BTreeSet<String> =
                list.iter().cloned().collect();
            exports.retain(|(k, _)| listed.contains(k));
            fn_entries.retain(|(k, _)| listed.contains(k));
        }

        if let Some(alias_name) = alias {
            // Aliased form: build a `Value::Module` and bind it
            // under the alias. The module carries its bindings
            // (let + fn) and the names of declared types so
            // namespaced constructors like `m.Entity { ... }`
            // can verify they're reaching for something the
            // module actually exports.
            if self
                .scopes
                .last()
                .map(|s| s.contains_key(alias_name))
                .unwrap_or(false)
                || self.functions.contains_key(alias_name)
            {
                return Err(error(
                    line,
                    format!(
                        "`{}` is already bound — can't use it as a module alias",
                        alias_name
                    ),
                ));
            }
            // Fn entries imported via alias also register in
            // `self.functions` so `m.foo()` (which lowers to
            // `Value::Fn` lookup in the module) and `foo()`
            // (bare call, never reaches the alias) stay
            // consistent when the module itself has sibling fn
            // calls inside its own body. Selective + alias:
            // only the listed fns register.
            for (name, fn_def) in fn_entries {
                if !self.functions.contains_key(&name) {
                    self.functions.insert(name, fn_def);
                }
            }
            let type_names: Vec<String> = bindings
                .struct_defs
                .iter()
                .map(|(n, _)| n.clone())
                .chain(bindings.enum_defs.iter().map(|(n, _)| n.clone()))
                .filter(|n| {
                    items.map(|list| list.iter().any(|i| i == n)).unwrap_or(true)
                })
                .collect();
            let module_value = Value::Module(Rc::new(crate::value::BopModule {
                path: path.to_string(),
                bindings: exports,
                types: type_names,
            }));
            self.define(alias_name.to_string(), module_value);
        } else {
            // Glob / selective without alias — flat injection.
            // Privacy: glob-only drops `_`-prefixed names.
            // Selective lets the user reach into private
            // bindings explicitly.
            let skip_private = items.is_none();
            for (name, fn_def) in fn_entries {
                if skip_private && crate::naming::is_private(&name) {
                    continue;
                }
                if self
                    .scopes
                    .last()
                    .map(|s| s.contains_key(&name))
                    .unwrap_or(false)
                    || self.functions.contains_key(&name)
                {
                    self.runtime_warnings.push(crate::error::BopWarning::at(
                        format!(
                            "`{}` from `{}` shadowed by an existing binding — the first definition wins",
                            name, path
                        ),
                        line,
                    ));
                    continue;
                }
                self.functions.insert(name, fn_def);
            }
            for (name, value) in exports {
                if skip_private && crate::naming::is_private(&name) {
                    continue;
                }
                if self
                    .scopes
                    .last()
                    .map(|s| s.contains_key(&name))
                    .unwrap_or(false)
                {
                    self.runtime_warnings.push(crate::error::BopWarning::at(
                        format!(
                            "`{}` from `{}` shadowed by an existing binding — the first definition wins",
                            name, path
                        ),
                        line,
                    ));
                    continue;
                }
                self.define(name, value);
            }
        }

        if is_plain_glob {
            self.imported_here.insert(path.to_string());
        }
        Ok(())
    }

    /// Resolve and evaluate a module, caching the result. Returns
    /// the module's exported bindings.
    fn load_module(&mut self, path: &str, line: u32) -> Result<ModuleBindings, BopError> {
        // Fast path: already loaded.
        {
            let cache = self.imports.borrow();
            if let Some(ImportSlot::Loaded(bindings)) = cache.get(path) {
                return Ok(bindings.clone());
            }
            if let Some(ImportSlot::Loading) = cache.get(path) {
                return Err(error(
                    line,
                    format!("Circular import: module `{}` is still loading", path),
                ));
            }
        }

        // Ask the host for source.
        let source = match self.host.resolve_module(path) {
            Some(Ok(s)) => s,
            Some(Err(e)) => return Err(e),
            None => {
                return Err(error(
                    line,
                    format!("Module `{}` not found", path),
                ));
            }
        };

        // Mark in-progress before we start evaluating so a
        // circular import surfaces as a clean error.
        self.imports
            .borrow_mut()
            .insert(path.to_string(), ImportSlot::Loading);

        let result = self.evaluate_module(&source, line);

        match result {
            Ok(bindings) => {
                self.imports
                    .borrow_mut()
                    .insert(path.to_string(), ImportSlot::Loaded(bindings.clone()));
                Ok(bindings)
            }
            Err(e) => {
                // Drop the Loading marker so a subsequent, non-
                // broken context could retry.
                self.imports.borrow_mut().remove(path);
                Err(e)
            }
        }
    }

    /// Parse and walk a module source in a fresh scope, returning
    /// its top-level bindings as a `ModuleBindings`. Reuses the
    /// parent evaluator's host, limits, and import cache.
    fn evaluate_module(
        &mut self,
        source: &str,
        line: u32,
    ) -> Result<ModuleBindings, BopError> {
        let _ = line;
        let stmts = crate::parse(source)?;
        let imports = Rc::clone(&self.imports);
        let limits = self.limits.clone();
        let mut sub = Evaluator::new_for_module(self.host, limits, imports);
        // Run the module body to top — errors propagate as-is.
        match sub.exec_block(&stmts)? {
            Signal::Return(_) | Signal::None => {}
            Signal::Break => {
                return Err(error(0, "break used outside of a loop"));
            }
            Signal::Continue => {
                return Err(error(0, "continue used outside of a loop"));
            }
        }
        // Collect top-level `let` bindings from the module's
        // one remaining scope. Fns are handled separately so
        // the importer can register them in `self.functions`
        // as well as in the scope (see `exec_import`).
        let mut bindings: Vec<(String, Value)> = Vec::new();
        if let Some(top_scope) = sub.scopes.into_iter().next() {
            for (k, v) in top_scope {
                bindings.push((k, v));
            }
        }
        let fn_decls: Vec<(String, FnDef)> =
            sub.functions.into_iter().collect();
        // Type decls and methods transfer too so an importer can
        // construct / pattern-match on the module's types
        // without re-declaring them.
        let struct_defs: Vec<(String, Vec<String>)> =
            sub.struct_defs.into_iter().collect();
        let enum_defs: Vec<(String, Vec<crate::parser::VariantDecl>)> =
            sub.enum_defs.into_iter().collect();
        let mut methods: Vec<(String, String, FnDef)> = Vec::new();
        for (type_name, by_method) in sub.methods {
            for (method_name, fn_def) in by_method {
                methods.push((type_name.clone(), method_name, fn_def));
            }
        }
        Ok(ModuleBindings {
            bindings,
            fn_decls,
            struct_defs,
            enum_defs,
            methods,
        })
    }

    /// Validate `ns.Type` — the caller wrote `ns.Type { ... }`
    /// or `ns.Type::Variant(...)`. `ns` must be a
    /// `Value::Module` in scope, and `Type` must appear in that
    /// module's list of exported type names. The return is
    /// `Ok(())` for now because types still register under their
    /// bare names globally; once qualified type names land, this
    /// is where we translate `ns.Type` into its qualified form.
    fn validate_namespaced_type(
        &self,
        ns: &str,
        type_name: &str,
        line: u32,
    ) -> Result<(), BopError> {
        let v = self.get_var(ns).ok_or_else(|| {
            error(line, format!("`{}` isn't a module alias in scope", ns))
        })?;
        let module = match v {
            Value::Module(m) => m,
            _ => {
                return Err(error(
                    line,
                    format!(
                        "`{}` is a {}, not a module alias — can't reach `{}` through it",
                        ns,
                        v.type_name(),
                        type_name
                    ),
                ));
            }
        };
        if !module.types.iter().any(|t| t == type_name) {
            return Err(error(
                line,
                format!(
                    "`{}` isn't a type exported from `{}`",
                    type_name, module.path
                ),
            ));
        }
        Ok(())
    }

    fn eval_enum_construct(
        &mut self,
        namespace: Option<&str>,
        type_name: &str,
        variant: &str,
        payload: &VariantPayload,
        line: u32,
    ) -> Result<Value, BopError> {
        if let Some(ns) = namespace {
            self.validate_namespaced_type(ns, type_name, line)?;
        }
        let variants = self.enum_defs.get(type_name).ok_or_else(|| {
            error(line, crate::error_messages::enum_not_declared(type_name))
        })?
        .clone();
        let decl = variants.iter().find(|v| v.name == variant).ok_or_else(|| {
            let msg = crate::error_messages::enum_has_no_variant(type_name, variant);
            let names = variants.iter().map(|v| v.name.as_str());
            match crate::suggest::did_you_mean(variant, names) {
                Some(hint) => error_with_hint(line, msg, hint),
                None => error(line, msg),
            }
        })?
        .clone();

        match (&decl.kind, payload) {
            (VariantKind::Unit, VariantPayload::Unit) => Ok(Value::new_enum_unit(
                type_name.to_string(),
                variant.to_string(),
            )),
            (VariantKind::Tuple(fields), VariantPayload::Tuple(args)) => {
                if args.len() != fields.len() {
                    return Err(error(
                        line,
                        format!(
                            "`{}::{}` expects {} argument{}, but got {}",
                            type_name,
                            variant,
                            fields.len(),
                            if fields.len() == 1 { "" } else { "s" },
                            args.len()
                        ),
                    ));
                }
                let mut items = Vec::with_capacity(args.len());
                for arg in args {
                    items.push(self.eval_expr(arg)?);
                }
                Ok(Value::new_enum_tuple(
                    type_name.to_string(),
                    variant.to_string(),
                    items,
                ))
            }
            (VariantKind::Struct(decl_fields), VariantPayload::Struct(provided)) => {
                let mut seen = alloc_import::collections::BTreeSet::new();
                let mut provided_map: BTreeMap<String, Value> = BTreeMap::new();
                for (fname, fexpr) in provided {
                    if !seen.insert(fname.clone()) {
                        return Err(error(
                            line,
                            format!(
                                "Field `{}` specified twice in `{}::{}`",
                                fname, type_name, variant
                            ),
                        ));
                    }
                    if !decl_fields.iter().any(|d| d == fname) {
                        return Err(error(
                            line,
                            crate::error_messages::variant_has_no_field(
                                type_name, variant, fname,
                            ),
                        ));
                    }
                    provided_map.insert(fname.clone(), self.eval_expr(fexpr)?);
                }
                let mut values: Vec<(String, Value)> =
                    Vec::with_capacity(decl_fields.len());
                for decl_field in decl_fields {
                    match provided_map.remove(decl_field) {
                        Some(v) => values.push((decl_field.clone(), v)),
                        None => {
                            return Err(error(
                                line,
                                format!(
                                    "Missing field `{}` in `{}::{}` construction",
                                    decl_field, type_name, variant
                                ),
                            ));
                        }
                    }
                }
                Ok(Value::new_enum_struct(
                    type_name.to_string(),
                    variant.to_string(),
                    values,
                ))
            }
            (VariantKind::Unit, _) => Err(error(
                line,
                format!(
                    "Variant `{}::{}` takes no payload",
                    type_name, variant
                ),
            )),
            (VariantKind::Tuple(_), _) => Err(error(
                line,
                format!(
                    "Variant `{}::{}` expects positional arguments `(…)`",
                    type_name, variant
                ),
            )),
            (VariantKind::Struct(_), _) => Err(error(
                line,
                format!(
                    "Variant `{}::{}` expects named fields `{{ … }}`",
                    type_name, variant
                ),
            )),
        }
    }

    fn exec_assign(
        &mut self,
        target: &AssignTarget,
        op: &AssignOp,
        new_val: Value,
        line: u32,
    ) -> Result<(), BopError> {
        match target {
            AssignTarget::Variable(name) => {
                let final_val = match op {
                    AssignOp::Eq => new_val,
                    _ => {
                        let current = self
                            .get_var(name)
                            .ok_or_else(|| {
                                error(line, format!("Variable `{}` doesn't exist yet", name))
                            })?
                            .clone();
                        self.apply_compound_op(&current, op, &new_val, line)?
                    }
                };
                if !self.set_var(name, final_val) {
                    return Err(error_with_hint(
                        line,
                        format!("Variable `{}` doesn't exist yet", name),
                        format!("Use `let` to create a new variable: let {} = ...", name),
                    ));
                }
                Ok(())
            }
            AssignTarget::Index { object, index } => {
                let idx = self.eval_expr(index)?;
                let val_to_set = match op {
                    AssignOp::Eq => new_val,
                    _ => {
                        let obj = self.eval_expr(object)?;
                        let current = ops::index_get(&obj, &idx, line)?;
                        self.apply_compound_op(&current, op, &new_val, line)?
                    }
                };
                if let ExprKind::Ident(name) = &object.kind {
                    let mut obj = self
                        .get_var(name)
                        .ok_or_else(|| {
                            error(line, format!("Variable `{}` doesn't exist", name))
                        })?
                        .clone();
                    ops::index_set(&mut obj, &idx, val_to_set, line)?;
                    self.set_var(name, obj);
                    Ok(())
                } else {
                    Err(error(
                        line,
                        "Can only assign to indexed variables (like `arr[0] = val`)",
                    ))
                }
            }
            AssignTarget::Field { object, field } => {
                // Support assignment into a named struct field.
                // Only bare-`Ident` objects are assignable — the
                // writeback goes through `set_var`, so chains like
                // `foo().x = 1` or `arr[0].x = 1` aren't
                // supported yet (matches index-assign behaviour).
                let name = match &object.kind {
                    ExprKind::Ident(n) => n.clone(),
                    _ => {
                        return Err(error(
                            line,
                            "Can only assign to fields of named variables (like `p.x = val`)",
                        ));
                    }
                };
                let mut obj = self
                    .get_var(&name)
                    .ok_or_else(|| {
                        error(line, format!("Variable `{}` doesn't exist", name))
                    })?
                    .clone();
                let val_to_set = match op {
                    AssignOp::Eq => new_val,
                    _ => {
                        let current = match &obj {
                            Value::Struct(s) => s.field(field).cloned().ok_or_else(|| {
                                error(
                                    line,
                                    crate::error_messages::struct_has_no_field(
                                        s.type_name(),
                                        field,
                                    ),
                                )
                            })?,
                            other => {
                                return Err(error(
                                    line,
                                    crate::error_messages::cant_assign_field(
                                        field,
                                        other.type_name(),
                                    ),
                                ));
                            }
                        };
                        self.apply_compound_op(&current, op, &new_val, line)?
                    }
                };
                match &mut obj {
                    Value::Struct(s) => {
                        let struct_type = s.type_name().to_string();
                        if !s.set_field(field, val_to_set) {
                            return Err(error(
                                line,
                                crate::error_messages::struct_has_no_field(&struct_type, field),
                            ));
                        }
                    }
                    other => {
                        return Err(error(
                            line,
                            crate::error_messages::cant_assign_field(
                                field,
                                other.type_name(),
                            ),
                        ));
                    }
                }
                self.set_var(&name, obj);
                Ok(())
            }
        }
    }

    fn apply_compound_op(
        &self,
        left: &Value,
        op: &AssignOp,
        right: &Value,
        line: u32,
    ) -> Result<Value, BopError> {
        match op {
            AssignOp::Eq => Ok(right.clone()),
            AssignOp::AddEq => ops::add(left, right, line),
            AssignOp::SubEq => ops::sub(left, right, line),
            AssignOp::MulEq => ops::mul(left, right, line),
            AssignOp::DivEq => ops::div(left, right, line),
            AssignOp::ModEq => ops::rem(left, right, line),
        }
    }

    // ─── Expressions ───────────────────────────────────────────────

    fn eval_expr(&mut self, expr: &Expr) -> Result<Value, BopError> {
        match &expr.kind {
            ExprKind::Int(n) => Ok(Value::Int(*n)),
            ExprKind::Number(n) => Ok(Value::Number(*n)),
            ExprKind::Str(s) => Ok(Value::new_str(s.clone())),
            ExprKind::Bool(b) => Ok(Value::Bool(*b)),
            ExprKind::None => Ok(Value::None),

            ExprKind::StringInterp(parts) => {
                let mut result = String::new();
                for part in parts {
                    match part {
                        StringPart::Literal(s) => result.push_str(s),
                        StringPart::Variable(name) => {
                            let val = self
                                .get_var(name)
                                .ok_or_else(|| {
                                    error(expr.line, crate::error_messages::variable_not_found(name))
                                })?
                                .clone();
                            result.push_str(&format!("{}", val));
                        }
                    }
                }
                Ok(Value::new_str(result))
            }

            ExprKind::Ident(name) => {
                // Lexical lookup first — matches the intuition that
                // `let x = ...` locally shadows everything else.
                if let Some(v) = self.get_var(name) {
                    return Ok(v.clone());
                }
                // Fall back to named `fn` declarations so they can
                // be passed around as first-class values. The
                // synthesised `Value::Fn` carries `self_name` so
                // recursive lookups inside the body still resolve
                // through `self.functions` (see `call_bop_fn`).
                if let Some(f) = self.functions.get(name) {
                    return Ok(Value::new_fn(
                        f.params.clone(),
                        Vec::new(),
                        f.body.clone(),
                        Some(name.to_string()),
                    ));
                }
                // Typo? Offer a "did you mean" hint if something
                // close is visible in the current scope / fn
                // registry. Falls back to the original "did you
                // forget `let`" when no candidate is similar
                // enough.
                let hint = self
                    .value_candidates_hint(name)
                    .unwrap_or_else(|| "Did you forget to create it with `let`?".to_string());
                Err(error_with_hint(
                    expr.line,
                    crate::error_messages::variable_not_found(name),
                    hint,
                ))
            }

            ExprKind::BinaryOp { left, op, right } => {
                // Short-circuit for && and ||
                if matches!(op, BinOp::And) {
                    let lval = self.eval_expr(left)?;
                    if !lval.is_truthy() {
                        return Ok(Value::Bool(false));
                    }
                    let rval = self.eval_expr(right)?;
                    return Ok(Value::Bool(rval.is_truthy()));
                }
                if matches!(op, BinOp::Or) {
                    let lval = self.eval_expr(left)?;
                    if lval.is_truthy() {
                        return Ok(Value::Bool(true));
                    }
                    let rval = self.eval_expr(right)?;
                    return Ok(Value::Bool(rval.is_truthy()));
                }

                let lval = self.eval_expr(left)?;
                let rval = self.eval_expr(right)?;
                self.binary_op(&lval, op, &rval, expr.line)
            }

            ExprKind::UnaryOp { op, expr: inner } => {
                let val = self.eval_expr(inner)?;
                match op {
                    UnaryOp::Neg => ops::neg(&val, expr.line),
                    UnaryOp::Not => Ok(ops::not(&val)),
                }
            }

            ExprKind::Call { callee, args } => {
                let mut eval_args = Vec::new();
                for arg in args {
                    eval_args.push(self.eval_expr(arg)?);
                }
                if let ExprKind::Ident(name) = &callee.kind {
                    // Lexical callable first: if the name is bound
                    // to a `Value::Fn` in the current scope, call
                    // it. A bound non-callable is an explicit
                    // error — "shadowing a builtin with a number
                    // then calling it" should fail loudly, not
                    // silently dispatch to the builtin.
                    if let Some(v) = self.get_var(name).cloned() {
                        return self.call_value(v, eval_args, expr.line, Some(name));
                    }
                    // Otherwise fall through to the original
                    // name-based dispatch (builtins → host → named
                    // fns). This is what keeps `print(x)` /
                    // `range(n)` / `my_user_fn(x)` working.
                    return self.call_function(name, eval_args, expr.line);
                }
                // Non-Ident callee: evaluate the expression; it
                // must produce a `Value::Fn`.
                let callee_val = self.eval_expr(callee)?;
                self.call_value(callee_val, eval_args, expr.line, None)
            }

            ExprKind::Lambda { params, body } => {
                let captures = self.snapshot_captures();
                Ok(Value::new_fn(
                    params.clone(),
                    captures,
                    body.clone(),
                    None,
                ))
            }

            ExprKind::MethodCall {
                object,
                method,
                args,
            } => {
                let mut eval_args = Vec::new();
                for arg in args {
                    eval_args.push(self.eval_expr(arg)?);
                }
                let obj_val = self.eval_expr(object)?;

                // `m.foo(args)` on a module alias: this parsed as
                // a `MethodCall`, but there's no struct/enum
                // receiver — `m` is a `Value::Module` whose
                // `foo` export is a callable value. Look it up,
                // then treat the result as a regular value call.
                if let Value::Module(m) = &obj_val {
                    if let Some((_, v)) = m.bindings.iter().find(|(k, _)| k == method) {
                        let callee = v.clone();
                        return self.call_value(callee, eval_args, expr.line, Some(method));
                    }
                    return Err(error(
                        expr.line,
                        format!(
                            "`{}` isn't exported from `{}`",
                            method, m.path
                        ),
                    ));
                }

                // User-defined method dispatch comes first — any
                // user method registered against the receiver's
                // type wins over built-in methods of the same
                // name. Enums dispatch on the enum's type, not
                // the variant's, so all variants of `Shape` share
                // `fn Shape.area(self)`.
                let type_key = match &obj_val {
                    Value::Struct(s) => Some(s.type_name().to_string()),
                    Value::EnumVariant(e) => Some(e.type_name().to_string()),
                    _ => None,
                };
                if let Some(key) = type_key {
                    let user = self
                        .methods
                        .get(&key)
                        .and_then(|ms| ms.get(method))
                        .cloned();
                    if let Some(m) = user {
                        if m.params.len() != eval_args.len() + 1 {
                            return Err(error(
                                expr.line,
                                format!(
                                    "`{}.{}` expects {} argument{} (including `self`), but got {}",
                                    key,
                                    method,
                                    m.params.len(),
                                    if m.params.len() == 1 { "" } else { "s" },
                                    eval_args.len() + 1
                                ),
                            ));
                        }
                        // Prepend receiver as the first parameter
                        // (`self` by convention).
                        let mut full_args = Vec::with_capacity(eval_args.len() + 1);
                        full_args.push(obj_val);
                        full_args.extend(eval_args);
                        let bop_fn = Rc::new(BopFn {
                            params: m.params,
                            captures: Vec::new(),
                            body: FnBody::Ast(m.body),
                            self_name: None,
                        });
                        return self.call_bop_fn(&bop_fn, full_args, expr.line);
                    }
                }

                let (ret, mutated) = self.call_method(&obj_val, method, &eval_args, expr.line)?;

                if methods::is_mutating_method(method) {
                    if let ExprKind::Ident(name) = &object.kind {
                        if let Some(new_obj) = mutated {
                            self.set_var(name, new_obj);
                        }
                    }
                }
                Ok(ret)
            }

            ExprKind::Index { object, index } => {
                let obj = self.eval_expr(object)?;
                let idx = self.eval_expr(index)?;
                ops::index_get(&obj, &idx, expr.line)
            }

            ExprKind::FieldAccess { object, field } => {
                let obj = self.eval_expr(object)?;
                match &obj {
                    Value::Struct(s) => s.field(field).cloned().ok_or_else(|| {
                        let msg =
                            crate::error_messages::struct_has_no_field(s.type_name(), field);
                        // Suggest from the struct's own declared
                        // field list — `p.z` when `Point` has `x`
                        // and `y` should point to `x`/`y`.
                        let field_names: Vec<&str> =
                            s.fields().iter().map(|(k, _)| k.as_str()).collect();
                        match crate::suggest::did_you_mean(field, field_names) {
                            Some(hint) => error_with_hint(expr.line, msg, hint),
                            None => error(expr.line, msg),
                        }
                    }),
                    Value::EnumVariant(e) => e.field(field).cloned().ok_or_else(|| {
                        error(
                            expr.line,
                            crate::error_messages::variant_has_no_field(
                                e.type_name(),
                                e.variant(),
                                field,
                            ),
                        )
                    }),
                    Value::Module(m) => {
                        // `alias.name` — look up an export in the
                        // aliased module.
                        if let Some((_, v)) = m.bindings.iter().find(|(k, _)| k == field) {
                            return Ok(v.clone());
                        }
                        if m.types.iter().any(|t| t == field) {
                            // Types aren't first-class values; the
                            // parser should have taken the namespaced
                            // struct-lit / variant-ctor path
                            // already. Reaching a FieldAccess here
                            // means the user wrote `m.Type` in a
                            // value-returning position.
                            return Err(error_with_hint(
                                expr.line,
                                format!("`{}` in `{}` is a type, not a value", field, m.path),
                                format!(
                                    "construct through the alias: `{}.{} {{ ... }}` or `{}.{}::Variant(...)`",
                                    m.path.split('.').last().unwrap_or(&m.path),
                                    field,
                                    m.path.split('.').last().unwrap_or(&m.path),
                                    field,
                                ),
                            ));
                        }
                        Err(error(
                            expr.line,
                            format!(
                                "`{}` isn't exported from `{}`",
                                field, m.path
                            ),
                        ))
                    }
                    other => Err(error(
                        expr.line,
                        crate::error_messages::cant_read_field(field, other.type_name()),
                    )),
                }
            }

            ExprKind::EnumConstruct {
                namespace,
                type_name,
                variant,
                payload,
            } => self.eval_enum_construct(
                namespace.as_deref(),
                type_name,
                variant,
                payload,
                expr.line,
            ),

            ExprKind::StructConstruct {
                namespace,
                type_name,
                fields,
            } => {
                if let Some(ns) = namespace {
                    self.validate_namespaced_type(ns, type_name, expr.line)?;
                }
                let decl_fields = self.struct_defs.get(type_name).ok_or_else(|| {
                    error(
                        expr.line,
                        crate::error_messages::struct_not_declared(type_name),
                    )
                })?
                .clone();
                // Enforce exactly-this-field-set at construction:
                // no duplicates, no unknown fields, no missing
                // fields. The per-field loops below produce the
                // specific messages the tests assert on.
                let mut seen = alloc_import::collections::BTreeSet::new();
                let mut values: Vec<(String, Value)> =
                    Vec::with_capacity(decl_fields.len());
                // Evaluate provided fields by name, then emit them
                // in *declaration* order so the `Value::Struct`'s
                // field list is stable across construction sites.
                let mut provided: BTreeMap<String, Value> = BTreeMap::new();
                for (fname, fexpr) in fields {
                    if !seen.insert(fname.clone()) {
                        return Err(error(
                            expr.line,
                            format!(
                                "Field `{}` specified twice in `{}` construction",
                                fname, type_name
                            ),
                        ));
                    }
                    if !decl_fields.iter().any(|d| d == fname) {
                        let msg = crate::error_messages::struct_has_no_field(type_name, fname);
                        let err = match crate::suggest::did_you_mean(
                            fname,
                            decl_fields.iter().map(|s| s.as_str()),
                        ) {
                            Some(hint) => error_with_hint(expr.line, msg, hint),
                            None => error(expr.line, msg),
                        };
                        return Err(err);
                    }
                    provided.insert(fname.clone(), self.eval_expr(fexpr)?);
                }
                for decl in &decl_fields {
                    match provided.remove(decl) {
                        Some(v) => values.push((decl.clone(), v)),
                        None => {
                            return Err(error(
                                expr.line,
                                format!(
                                    "Missing field `{}` in `{}` construction",
                                    decl, type_name
                                ),
                            ));
                        }
                    }
                }
                Ok(Value::new_struct(type_name.clone(), values))
            }

            ExprKind::Array(elements) => {
                let mut items = Vec::new();
                for elem in elements {
                    items.push(self.eval_expr(elem)?);
                }
                Ok(Value::new_array(items))
            }

            ExprKind::Dict(entries) => {
                let mut result = Vec::new();
                for (key, value_expr) in entries {
                    let val = self.eval_expr(value_expr)?;
                    result.push((key.clone(), val));
                }
                Ok(Value::new_dict(result))
            }

            ExprKind::IfExpr {
                condition,
                then_expr,
                else_expr,
            } => {
                if self.eval_expr(condition)?.is_truthy() {
                    self.eval_expr(then_expr)
                } else {
                    self.eval_expr(else_expr)
                }
            }

            ExprKind::Match { scrutinee, arms } => self.eval_match(scrutinee, arms, expr.line),

            ExprKind::Try(inner) => self.eval_try(inner, expr.line),
        }
    }

    /// `try` expression handler. Evaluates the inner expression,
    /// matches the conventional Result-shape (any enum variant
    /// named `Ok` or `Err`), and either unwraps `Ok` or stashes
    /// the `Err` value into `pending_try_return` so the enclosing
    /// `call_bop_fn` can convert it to `Signal::Return`.
    ///
    /// Top-level `try` on `Err` (call_depth == 0) surfaces as a
    /// real runtime error — there's no enclosing fn to return
    /// from, and the roadmap explicitly rejects the idea of
    /// swallowing it silently.
    fn eval_try(&mut self, inner: &Expr, line: u32) -> Result<Value, BopError> {
        let value = self.eval_expr(inner)?;
        // An earlier `try` in `inner` might have already fired —
        // e.g. `try try foo()` — in which case we just keep
        // propagating without poking at the unwound value.
        if self.pending_try_return.is_some() {
            return Ok(Value::None);
        }
        match &value {
            Value::EnumVariant(ev) if ev.variant() == "Ok" => {
                // Extract the single payload for `Ok(v)`, or fall
                // back to `none` for `Ok` used as a unit variant.
                match ev.payload() {
                    EnumPayload::Tuple(items) if items.len() == 1 => Ok(items[0].clone()),
                    EnumPayload::Unit => Ok(Value::None),
                    EnumPayload::Tuple(items) => Err(error(
                        line,
                        format!(
                            "try: Ok variant must carry exactly one value, got {}",
                            items.len()
                        ),
                    )),
                    EnumPayload::Struct(_) => Err(error(
                        line,
                        "try: Ok variant must carry a single positional value, not named fields",
                    )),
                }
            }
            Value::EnumVariant(ev) if ev.variant() == "Err" => {
                if self.call_depth == 0 {
                    return Err(error_with_hint(
                        line,
                        "try encountered Err at top-level",
                        "Wrap the calling code in a fn, or use `match` to handle both arms explicitly.",
                    ));
                }
                self.pending_try_return = Some(value);
                // Sentinel error whose only job is to unwind up
                // to the nearest `call_bop_fn`, which converts
                // it back into a `Signal::Return`. The `is_try_return`
                // flag on `BopError` is the only thing that
                // matters — line is kept for debug clarity;
                // message is unused.
                Err(BopError::try_return_signal(line))
            }
            other => Err(error(
                line,
                format!(
                    "try expected a Result-shaped value (Ok/Err variant), got {}",
                    other.type_name()
                ),
            )),
        }
    }

    fn eval_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        line: u32,
    ) -> Result<Value, BopError> {
        let value = self.eval_expr(scrutinee)?;
        for arm in arms {
            let mut bindings: Vec<(String, Value)> = Vec::new();
            if !pattern_matches(&arm.pattern, &value, &mut bindings) {
                continue;
            }
            // Pattern matched — open a fresh scope, bind every
            // captured name, evaluate the guard (in that scope).
            self.push_scope();
            for (name, v) in bindings {
                self.define(name, v);
            }
            let guard_ok = match &arm.guard {
                Some(g) => self.eval_expr(g)?.is_truthy(),
                None => true,
            };
            if !guard_ok {
                self.pop_scope();
                continue;
            }
            let result = self.eval_expr(&arm.body);
            self.pop_scope();
            return result;
        }
        Err(error(
            line,
            "No match arm matched the scrutinee",
        ))
    }

    // ─── Binary operations ─────────────────────────────────────────

    fn binary_op(
        &self,
        left: &Value,
        op: &BinOp,
        right: &Value,
        line: u32,
    ) -> Result<Value, BopError> {
        match op {
            BinOp::Add => ops::add(left, right, line),
            BinOp::Sub => ops::sub(left, right, line),
            BinOp::Mul => ops::mul(left, right, line),
            BinOp::Div => ops::div(left, right, line),
            BinOp::IntDiv => ops::int_div(left, right, line),
            BinOp::Mod => ops::rem(left, right, line),
            BinOp::Eq => Ok(ops::eq(left, right)),
            BinOp::NotEq => Ok(ops::not_eq(left, right)),
            BinOp::Lt => ops::lt(left, right, line),
            BinOp::Gt => ops::gt(left, right, line),
            BinOp::LtEq => ops::lt_eq(left, right, line),
            BinOp::GtEq => ops::gt_eq(left, right, line),
            BinOp::And | BinOp::Or => unreachable!("handled in eval_expr"),
        }
    }

    // ─── Function calls ────────────────────────────────────────────

    /// Collapse the current scope stack into a flat list of
    /// `(name, value)` pairs — the snapshot used as a lambda's
    /// captures. Inner scopes shadow outer ones, so the resulting
    /// list is deduplicated by name with the innermost binding
    /// winning.
    fn snapshot_captures(&self) -> Vec<(String, Value)> {
        let mut flat = BTreeMap::new();
        for scope in &self.scopes {
            for (k, v) in scope {
                flat.insert(k.clone(), v.clone());
            }
        }
        flat.into_iter().collect()
    }

    /// Call a value directly. Non-`Value::Fn` payloads are an
    /// explicit "not callable" error — this is the safety net for
    /// `let x = 5; x(1)` and friends.
    ///
    /// `name_hint` is the Ident text when the callee was a bare
    /// name, used only for error messages. Non-Ident callees pass
    /// `None`.
    fn call_value(
        &mut self,
        callee: Value,
        args: Vec<Value>,
        line: u32,
        name_hint: Option<&str>,
    ) -> Result<Value, BopError> {
        match &callee {
            Value::Fn(f) => {
                let f = Rc::clone(f);
                drop(callee);
                self.call_bop_fn(&f, args, line)
            }
            other => match name_hint {
                Some(n) => Err(error(
                    line,
                    format!(
                        "`{}` is a {}, not a function",
                        n,
                        other.type_name()
                    ),
                )),
                None => Err(error(
                    line,
                    crate::error_messages::cant_call_a(other.type_name()),
                )),
            },
        }
    }

    /// Shared call path for every `Value::Fn` — the body of both
    /// `let f = fn(...) {...}; f(x)` and the reified version of
    /// `fn name(...) {...}; name(x)` come through here.
    fn call_bop_fn(
        &mut self,
        func: &Rc<BopFn>,
        args: Vec<Value>,
        line: u32,
    ) -> Result<Value, BopError> {
        let body = match &func.body {
            FnBody::Ast(stmts) => stmts,
            FnBody::Compiled(_) => {
                return Err(error(
                    line,
                    "This function was compiled for another engine and can't be run in the tree-walker",
                ));
            }
        };
        if args.len() != func.params.len() {
            let name = func.self_name.as_deref().unwrap_or("fn");
            return Err(error(
                line,
                format!(
                    "`{}` expects {} argument{}, but got {}",
                    name,
                    func.params.len(),
                    if func.params.len() == 1 { "" } else { "s" },
                    args.len()
                ),
            ));
        }

        if self.call_depth >= MAX_CALL_DEPTH {
            return Err(error_with_hint(
                line,
                "Too many nested function calls (possible infinite recursion)",
                "Check that your recursive function has a base case that stops calling itself.",
            ));
        }

        // A function call gets a fresh scope stack: no outer
        // locals leak in. Captures and parameters seed the new
        // scope; self-reference via `self_name` lets recursive
        // lambdas see themselves.
        self.call_depth += 1;
        let saved_scopes = core::mem::replace(&mut self.scopes, vec![BTreeMap::new()]);

        // Captures go in first so parameters shadow them on
        // collision (matches the lexical snapshot semantics).
        for (name, value) in &func.captures {
            self.define(name.clone(), value.clone());
        }
        if let Some(self_name) = &func.self_name {
            // Make the reified `fn name` value visible inside its
            // own body so `fn fib(n) { return fib(n-1) ... }`
            // works without a special "named fn" path.
            self.define(self_name.clone(), Value::Fn(Rc::clone(func)));
        }
        for (param, arg) in func.params.iter().zip(args) {
            self.define(param.clone(), arg);
        }

        let result = self.exec_block(body);
        self.scopes = saved_scopes;
        self.call_depth -= 1;

        // `try` unwinds as a sentinel `BopError`; the call
        // boundary trades that error for the stashed value and
        // treats it as a normal return. Any other error
        // propagates as usual. Always clears
        // `pending_try_return` so a leftover can't contaminate
        // a later call.
        match result {
            Ok(sig) => match sig {
                Signal::Return(val) => Ok(val),
                Signal::Break => Err(error(line, "break used outside of a loop")),
                Signal::Continue => Err(error(line, "continue used outside of a loop")),
                Signal::None => Ok(Value::None),
            },
            Err(err) => {
                // Check the dedicated `is_try_return` flag
                // instead of inspecting the message — a flag
                // can't collide with a user-authored error
                // that happens to spell the same bytes.
                if err.is_try_return {
                    if let Some(val) = self.pending_try_return.take() {
                        return Ok(val);
                    }
                }
                Err(err)
            }
        }
    }

    /// Implement the `try_call(f)` builtin.
    ///
    /// Takes a zero-arg callable `f` and invokes it. On a clean
    /// return, yields `Result::Ok(value)`. On a **non-fatal**
    /// `BopError`, yields `Result::Err(RuntimeError { message,
    /// line })` — both values are constructed directly by
    /// `bop::builtins::make_try_call_ok` / `_err`, so they
    /// work even in programs that never declared `Result` or
    /// `RuntimeError` themselves.
    ///
    /// Fatal errors (resource-limit violations — see
    /// `BopError::is_fatal`) bypass the wrap and propagate
    /// unchanged, preserving the sandbox invariant.
    fn builtin_try_call(
        &mut self,
        args: Vec<Value>,
        line: u32,
    ) -> Result<Value, BopError> {
        if args.len() != 1 {
            return Err(error(
                line,
                format!(
                    "`try_call` expects 1 argument, but got {}",
                    args.len()
                ),
            ));
        }
        let callable = args.into_iter().next().unwrap();
        let func = match &callable {
            Value::Fn(f) => Rc::clone(f),
            other => {
                return Err(error(
                    line,
                    format!(
                        "`try_call` expects a function, got {}",
                        other.type_name()
                    ),
                ));
            }
        };
        drop(callable);
        match self.call_bop_fn(&func, Vec::new(), line) {
            Ok(value) => Ok(builtins::make_try_call_ok(value)),
            Err(err) => {
                if err.is_fatal {
                    Err(err)
                } else {
                    Ok(builtins::make_try_call_err(&err))
                }
            }
        }
    }

    fn call_function(
        &mut self,
        name: &str,
        args: Vec<Value>,
        line: u32,
    ) -> Result<Value, BopError> {
        // 1. Standard library builtins
        match name {
            "range" => return builtins::builtin_range(&args, line, &mut self.rand_state),
            "str" => return builtins::builtin_str(&args, line),
            "int" => return builtins::builtin_int(&args, line),
            "float" => return builtins::builtin_float(&args, line),
            "type" => return builtins::builtin_type(&args, line),
            "abs" => return builtins::builtin_abs(&args, line),
            "min" => return builtins::builtin_min(&args, line),
            "max" => return builtins::builtin_max(&args, line),
            "rand" => return builtins::builtin_rand(&args, line, &mut self.rand_state),
            "len" => return builtins::builtin_len(&args, line),
            "sqrt" => return builtins::builtin_sqrt(&args, line),
            "sin" => return builtins::builtin_sin(&args, line),
            "cos" => return builtins::builtin_cos(&args, line),
            "tan" => return builtins::builtin_tan(&args, line),
            "floor" => return builtins::builtin_floor(&args, line),
            "ceil" => return builtins::builtin_ceil(&args, line),
            "round" => return builtins::builtin_round(&args, line),
            "pow" => return builtins::builtin_pow(&args, line),
            "log" => return builtins::builtin_log(&args, line),
            "exp" => return builtins::builtin_exp(&args, line),
            "inspect" => return builtins::builtin_inspect(&args, line),
            "print" => {
                let message = args
                    .iter()
                    .map(|a| format!("{}", a))
                    .collect::<Vec<_>>()
                    .join(" ");
                self.host.on_print(&message);
                return Ok(Value::None);
            }
            "try_call" => return self.builtin_try_call(args, line),
            _ => {}
        }

        // 2. Host-provided builtins
        if let Some(result) = self.host.call(name, &args, line) {
            return result;
        }

        // 3. User-defined functions — synthesise a transient
        // `BopFn` and delegate to the shared call path. This keeps
        // the behaviour of `fn name` declarations and `Value::Fn`
        // values identical, including self-reference semantics.
        let func = self.functions.get(name).cloned().ok_or_else(|| {
            // Preference order: "did you mean" suggestion first
            // (most specific to the user's typo), then the
            // host's generic function hint (embedder-specific
            // tips like "available host functions: …"), then a
            // bare error.
            if let Some(hint) = self.callable_candidates_hint(name) {
                error_with_hint(
                    line,
                    crate::error_messages::function_not_found(name),
                    hint,
                )
            } else {
                let host_hint = self.host.function_hint();
                if host_hint.is_empty() {
                    error(line, crate::error_messages::function_not_found(name))
                } else {
                    error_with_hint(
                        line,
                        crate::error_messages::function_not_found(name),
                        host_hint,
                    )
                }
            }
        })?;

        let bop_fn = Rc::new(BopFn {
            params: func.params,
            captures: Vec::new(),
            body: FnBody::Ast(func.body),
            self_name: Some(name.to_string()),
        });
        self.call_bop_fn(&bop_fn, args, line)
    }

    // ─── Methods ───────────────────────────────────────────────────

    fn call_method(
        &self,
        obj: &Value,
        method: &str,
        args: &[Value],
        line: u32,
    ) -> Result<(Value, Option<Value>), BopError> {
        match obj {
            Value::Array(arr) => methods::array_method(arr, method, args, line),
            Value::Str(s) => methods::string_method(s, method, args, line),
            Value::Dict(entries) => methods::dict_method(entries, method, args, line),
            _ => Err(error(
                line,
                crate::error_messages::no_such_method(obj.type_name(), method),
            )),
        }
    }
}

// ─── Pattern matching ──────────────────────────────────────────────
//
// `pattern_matches` returns true if `value` fits `pattern`, filling
// `bindings` with every `Binding` name along the way. On a partial
// match it's the caller's responsibility to discard the bindings
// — `eval_match` does this by only consuming them after the match
// + guard both succeed.

/// Seed a fresh evaluator's type tables with the engine-wide
/// builtin types (`Result`, `RuntimeError`). The shapes come from
/// `crate::builtins` so walker / VM / AOT can never drift out of
/// sync — they all read the same source.
fn seed_builtin_types() -> (
    BTreeMap<String, Vec<String>>,
    BTreeMap<String, Vec<VariantDecl>>,
) {
    let mut struct_defs: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut enum_defs: BTreeMap<String, Vec<VariantDecl>> = BTreeMap::new();
    struct_defs.insert(
        String::from("RuntimeError"),
        crate::builtins::builtin_runtime_error_fields(),
    );
    enum_defs.insert(
        String::from("Result"),
        crate::builtins::builtin_result_variants(),
    );
    (struct_defs, enum_defs)
}

/// True when two enum declarations describe the same runtime
/// shape. Strictly looser than `PartialEq` on `Vec<VariantDecl>`:
/// tuple variants compare by *arity* only (their payload field
/// names are positional stubs with no runtime meaning), while
/// struct variants still require matching field names. Same for
/// the outer variant ordering — positional.
///
/// Used by the "redeclare-same-shape" rules so a user program
/// that declares `enum Result { Ok(v), Err(e) }` is compatible
/// with the engine's builtin `enum Result { Ok(value), Err(error) }`.
fn variants_equivalent(a: &[VariantDecl], b: &[VariantDecl]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    for (va, vb) in a.iter().zip(b.iter()) {
        if va.name != vb.name {
            return false;
        }
        match (&va.kind, &vb.kind) {
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

/// Attempt to match `pattern` against `value`. On success, appends
/// any captured `(name, Value)` bindings to `bindings` and returns
/// `true`; on failure, returns `false` and leaves `bindings` in an
/// undefined state — it's the caller's responsibility to discard it.
///
/// Exported so other engines (the bytecode VM, AOT transpiler) can
/// run the exact same structural matcher as the tree-walker without
/// re-implementing the rules.
pub fn pattern_matches(
    pattern: &Pattern,
    value: &Value,
    bindings: &mut Vec<(String, Value)>,
) -> bool {
    match pattern {
        Pattern::Wildcard => true,
        Pattern::Binding(name) => {
            bindings.push((name.clone(), value.clone()));
            true
        }
        Pattern::Literal(lit) => match (lit, value) {
            (LiteralPattern::Int(a), Value::Int(b)) => a == b,
            (LiteralPattern::Number(a), Value::Number(b)) => a == b,
            // Cross-type numeric literal patterns — same rule
            // as `values_equal`: `match 1 { 1.0 => ... }` and
            // `match 1.0 { 1 => ... }` both match.
            (LiteralPattern::Int(a), Value::Number(b)) => (*a as f64) == *b,
            (LiteralPattern::Number(a), Value::Int(b)) => *a == (*b as f64),
            (LiteralPattern::Str(a), Value::Str(b)) => a.as_str() == b.as_str(),
            (LiteralPattern::Bool(a), Value::Bool(b)) => a == b,
            (LiteralPattern::None, Value::None) => true,
            _ => false,
        },
        Pattern::EnumVariant {
            namespace: _,
            type_name,
            variant,
            payload,
        } => {
            let ev = match value {
                Value::EnumVariant(e) => e,
                _ => return false,
            };
            if ev.type_name() != type_name.as_str() || ev.variant() != variant.as_str() {
                return false;
            }
            match (payload, ev.payload()) {
                (VariantPatternPayload::Unit, crate::value::EnumPayload::Unit) => true,
                (
                    VariantPatternPayload::Tuple(pats),
                    crate::value::EnumPayload::Tuple(items),
                ) => {
                    if pats.len() != items.len() {
                        return false;
                    }
                    for (p, v) in pats.iter().zip(items.iter()) {
                        if !pattern_matches(p, v, bindings) {
                            return false;
                        }
                    }
                    true
                }
                (
                    VariantPatternPayload::Struct { fields, rest },
                    crate::value::EnumPayload::Struct(entries),
                ) => {
                    match_struct_fields(fields, *rest, entries, bindings)
                }
                _ => false,
            }
        }
        Pattern::Struct {
            namespace: _,
            type_name,
            fields,
            rest,
        } => {
            let st = match value {
                Value::Struct(s) => s,
                _ => return false,
            };
            if st.type_name() != type_name.as_str() {
                return false;
            }
            match_struct_fields(fields, *rest, st.fields(), bindings)
        }
        Pattern::Array { elements, rest } => {
            let items = match value {
                Value::Array(arr) => arr,
                _ => return false,
            };
            match rest {
                None => {
                    if elements.len() != items.len() {
                        return false;
                    }
                    for (p, v) in elements.iter().zip(items.iter()) {
                        if !pattern_matches(p, v, bindings) {
                            return false;
                        }
                    }
                    true
                }
                Some(rest_kind) => {
                    if items.len() < elements.len() {
                        return false;
                    }
                    for (i, p) in elements.iter().enumerate() {
                        if !pattern_matches(p, &items[i], bindings) {
                            return false;
                        }
                    }
                    if let ArrayRest::Named(name) = rest_kind {
                        let tail: Vec<Value> =
                            items[elements.len()..].iter().cloned().collect();
                        bindings.push((name.clone(), Value::new_array(tail)));
                    }
                    true
                }
            }
        }
        Pattern::Or(alts) => {
            for alt in alts {
                let mut attempt: Vec<(String, Value)> = Vec::new();
                if pattern_matches(alt, value, &mut attempt) {
                    bindings.extend(attempt);
                    return true;
                }
            }
            false
        }
    }
}

fn match_struct_fields(
    fields: &[(String, Pattern)],
    rest: bool,
    entries: &[(String, Value)],
    bindings: &mut Vec<(String, Value)>,
) -> bool {
    // Every declared field-pattern must find its value in the
    // struct. `rest` just relaxes the requirement that *every*
    // value's key must appear in the pattern — non-rest patterns
    // may still leave fields unmatched, which is fine: the walker
    // matches named-field lookup semantics, not whole-shape
    // destructuring.
    let _ = rest;
    for (fname, pat) in fields {
        let value = match entries.iter().find(|(k, _)| k == fname) {
            Some((_, v)) => v,
            None => return false,
        };
        if !pattern_matches(pat, value, bindings) {
            return false;
        }
    }
    true
}
