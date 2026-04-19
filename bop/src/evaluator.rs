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

use crate::builtins::{self, error, error_with_hint};
use crate::error::BopError;
use crate::lexer::StringPart;
use crate::methods;
use crate::ops;
use crate::parser::*;
use crate::value::{BopFn, FnBody, Value};
use crate::{BopHost, BopLimits};

const MAX_CALL_DEPTH: usize = 64;

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
    host: &'h mut H,
    steps: u64,
    call_depth: usize,
    limits: BopLimits,
    rand_state: u64,
}

impl<'h, H: BopHost> Evaluator<'h, H> {
    pub fn new(host: &'h mut H, limits: BopLimits) -> Self {
        crate::memory::bop_memory_init(limits.max_memory);
        Self {
            scopes: vec![BTreeMap::new()],
            functions: BTreeMap::new(),
            host,
            steps: 0,
            call_depth: 0,
            limits,
            rand_state: 0,
        }
    }

    pub fn run(mut self, stmts: &[Stmt]) -> Result<(), BopError> {
        match self.exec_block(stmts)? {
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
            return Err(error_with_hint(
                line,
                "Your code took too many steps (possible infinite loop)",
                "Check your loops — make sure they have a condition that eventually stops them.",
            ));
        }
        if crate::memory::bop_memory_exceeded() {
            return Err(error_with_hint(
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
            StmtKind::Let { name, value } => {
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
                            format!("Can't iterate over {}", other.type_name()),
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

            StmtKind::Return { value } => {
                let val = match value {
                    Some(expr) => self.eval_expr(expr)?,
                    None => Value::None,
                };
                Ok(Signal::Return(val))
            }

            StmtKind::Break => Ok(Signal::Break),
            StmtKind::Continue => Ok(Signal::Continue),

            StmtKind::ExprStmt(expr) => {
                self.eval_expr(expr)?;
                Ok(Signal::None)
            }
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
                                    error(expr.line, format!("Variable `{}` not found", name))
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
                Err(error_with_hint(
                    expr.line,
                    format!("Variable `{}` not found", name),
                    "Did you forget to create it with `let`?",
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
        }
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
                    format!("Can't call a {}", other.type_name()),
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

        match result? {
            Signal::Return(val) => Ok(val),
            Signal::Break => Err(error(line, "break used outside of a loop")),
            Signal::Continue => Err(error(line, "continue used outside of a loop")),
            Signal::None => Ok(Value::None),
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
            "type" => return builtins::builtin_type(&args, line),
            "abs" => return builtins::builtin_abs(&args, line),
            "min" => return builtins::builtin_min(&args, line),
            "max" => return builtins::builtin_max(&args, line),
            "rand" => return builtins::builtin_rand(&args, line, &mut self.rand_state),
            "len" => return builtins::builtin_len(&args, line),
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
            let hint = self.host.function_hint();
            if hint.is_empty() {
                error(line, format!("Function `{}` not found", name))
            } else {
                error_with_hint(
                    line,
                    format!("Function `{}` not found", name),
                    hint,
                )
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
                format!("{} doesn't have a .{}() method", obj.type_name(), method),
            )),
        }
    }
}
