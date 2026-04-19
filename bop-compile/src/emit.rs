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

use std::collections::HashSet;
use std::fmt::Write as _;

use bop::error::BopError;
use bop::lexer::StringPart;
use bop::parser::{AssignOp, AssignTarget, BinOp, Expr, ExprKind, Stmt, StmtKind, UnaryOp};

use crate::Options;

pub(crate) fn emit(stmts: &[Stmt], opts: &Options) -> Result<String, BopError> {
    let user_fns = collect_fn_names(stmts);
    let mut emitter = Emitter::new(opts.clone(), user_fns);
    emitter.emit_program(stmts)?;
    Ok(emitter.finish())
}

/// Walk the AST collecting every `fn <name>` declaration, including
/// those nested in function bodies or inside control-flow blocks.
/// The set is used to decide whether a call to a non-builtin name
/// should emit a user-fn fallback path or drop straight to the
/// "Function X not found" error.
fn collect_fn_names(stmts: &[Stmt]) -> HashSet<String> {
    let mut out = HashSet::new();
    collect_from_block(stmts, &mut out);
    out
}

fn collect_from_block(stmts: &[Stmt], out: &mut HashSet<String>) {
    for stmt in stmts {
        match &stmt.kind {
            StmtKind::FnDecl { name, body, .. } => {
                out.insert(name.clone());
                collect_from_block(body, out);
            }
            StmtKind::If {
                body,
                else_ifs,
                else_body,
                ..
            } => {
                collect_from_block(body, out);
                for (_, b) in else_ifs {
                    collect_from_block(b, out);
                }
                if let Some(b) = else_body {
                    collect_from_block(b, out);
                }
            }
            StmtKind::While { body, .. } => collect_from_block(body, out),
            StmtKind::Repeat { body, .. } => collect_from_block(body, out),
            StmtKind::ForIn { body, .. } => collect_from_block(body, out),
            _ => {}
        }
    }
}

// ─── Emitter ───────────────────────────────────────────────────────

struct Emitter {
    out: String,
    indent: usize,
    opts: Options,
    user_fns: HashSet<String>,
    /// Counter for temporary locals (`__t0`, `__t1`, …). Reset at
    /// the start of each fn / top-level program so the names stay
    /// short.
    tmp_counter: usize,
}

impl Emitter {
    fn new(opts: Options, user_fns: HashSet<String>) -> Self {
        Self {
            out: String::new(),
            indent: 0,
            opts,
            user_fns,
            tmp_counter: 0,
        }
    }

    fn finish(self) -> String {
        self.out
    }

    fn emit_program(&mut self, stmts: &[Stmt]) -> Result<(), BopError> {
        self.emit_header();
        self.emit_runtime_preamble();
        self.emit_run_program(stmts)?;
        self.emit_public_entry();
        if self.opts.emit_main {
            self.emit_main();
        }
        Ok(())
    }

    // ─── Preamble / footer ────────────────────────────────────────

    fn emit_header(&mut self) {
        self.out.push_str(HEADER);
    }

    fn emit_runtime_preamble(&mut self) {
        self.out.push_str(RUNTIME_PREAMBLE);
    }

    fn emit_public_entry(&mut self) {
        self.out.push_str(PUBLIC_ENTRY);
    }

    fn emit_main(&mut self) {
        self.out.push_str(MAIN_FN);
    }

    fn emit_run_program(&mut self, stmts: &[Stmt]) -> Result<(), BopError> {
        writeln!(self.out, "fn run_program(ctx: &mut Ctx<'_>) -> Result<(), ::bop::error::BopError> {{").unwrap();
        self.indent = 1;
        self.tmp_counter = 0;
        self.emit_block_body(stmts, /* wants_value = */ false)?;
        self.line("Ok(())");
        self.indent = 0;
        self.out.push_str("}\n\n");
        Ok(())
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

    fn emit_block_body(&mut self, stmts: &[Stmt], _wants_value: bool) -> Result<(), BopError> {
        for stmt in stmts {
            self.emit_stmt(stmt)?;
        }
        Ok(())
    }

    fn emit_stmt(&mut self, stmt: &Stmt) -> Result<(), BopError> {
        let line = stmt.line;
        match &stmt.kind {
            StmtKind::Let { name, value } => {
                let rhs = self.expr_src(value)?;
                let ident = rust_ident(name);
                self.line(&format!("let mut {}: ::bop::value::Value = {};", ident, rhs));
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
                for s in body {
                    self.emit_stmt(s)?;
                }
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
                for s in body {
                    self.emit_stmt(s)?;
                }
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
                // Mirror the tree-walker: the loop variable is a
                // fresh binding in each iteration. Re-bind as mut so
                // the body can reassign it.
                self.line(&format!(
                    "let mut {}: ::bop::value::Value = {};",
                    ident, ident
                ));
                for s in body {
                    self.emit_stmt(s)?;
                }
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
        for s in body {
            self.emit_stmt(s)?;
        }
        self.indent -= 1;
        for (elif_cond, elif_body) in else_ifs {
            let c = self.expr_src(elif_cond)?;
            self.pad();
            self.out.push_str(&format!("}} else if ({}).is_truthy() {{\n", c));
            self.indent += 1;
            for s in elif_body {
                self.emit_stmt(s)?;
            }
            self.indent -= 1;
        }
        if let Some(else_body) = else_body {
            self.pad();
            self.out.push_str("} else {\n");
            self.indent += 1;
            for s in else_body {
                self.emit_stmt(s)?;
            }
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
        }
    }

    fn emit_fn_decl(
        &mut self,
        name: &str,
        params: &[String],
        body: &[Stmt],
        line: u32,
    ) -> Result<(), BopError> {
        let fn_name = rust_fn_name(name);
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
        let _ = line;
        self.open_block(&sig);
        let saved_tmp = self.tmp_counter;
        self.tmp_counter = 0;
        for s in body {
            self.emit_stmt(s)?;
        }
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

            ExprKind::Ident(name) => format!("{}.clone()", rust_ident(name)),

            ExprKind::StringInterp(parts) => self.string_interp_src(parts, line)?,

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
        let name = match &callee.kind {
            ExprKind::Ident(n) => n.clone(),
            _ => {
                return Err(BopError::runtime("Can only call named functions", line));
            }
        };

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
                if self.user_fns.contains(&name) {
                    let fn_name = rust_fn_name(&name);
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

/// Render a Bop user-fn name as a Rust function name. Prefixed so
/// there's no chance of clashing with built-in / stdlib fn names in
/// the generated module.
fn rust_fn_name(name: &str) -> String {
    format!("bop_fn_{}", name)
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
/// with `--sandbox` (once that lands) or call `bop_memory_init`
/// themselves before invoking `run`.
pub fn run<H: ::bop::BopHost>(host: &mut H) -> Result<(), ::bop::error::BopError> {
    ::bop::memory::bop_memory_init(usize::MAX);
    let mut ctx = Ctx {
        host: host as &mut dyn ::bop::BopHost,
        rand_state: 0,
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

