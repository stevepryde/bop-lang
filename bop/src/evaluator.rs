use std::collections::HashMap;

use crate::builtins::{self, error, error_with_hint};
use crate::error::BopError;
use crate::lexer::StringPart;
use crate::methods;
use crate::parser::*;
use crate::value::{Value, values_equal};
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
    scopes: Vec<HashMap<String, Value>>,
    functions: HashMap<String, FnDef>,
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
            scopes: vec![HashMap::new()],
            functions: HashMap::new(),
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
        self.scopes.push(HashMap::new());
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
                        let current = self.index_into(&obj, &idx, line)?;
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
                    self.set_index(&mut obj, &idx, val_to_set, line)?;
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
            AssignOp::AddEq => self.binary_op(left, &BinOp::Add, right, line),
            AssignOp::SubEq => self.binary_op(left, &BinOp::Sub, right, line),
            AssignOp::MulEq => self.binary_op(left, &BinOp::Mul, right, line),
            AssignOp::DivEq => self.binary_op(left, &BinOp::Div, right, line),
            AssignOp::ModEq => self.binary_op(left, &BinOp::Mod, right, line),
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

            ExprKind::Ident(name) => self.get_var(name).cloned().ok_or_else(|| {
                error_with_hint(
                    expr.line,
                    format!("Variable `{}` not found", name),
                    "Did you forget to create it with `let`?",
                )
            }),

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
                    UnaryOp::Neg => match val {
                        Value::Number(n) => Ok(Value::Number(-n)),
                        _ => Err(error(
                            expr.line,
                            format!("Can't negate a {}", val.type_name()),
                        )),
                    },
                    UnaryOp::Not => Ok(Value::Bool(!val.is_truthy())),
                }
            }

            ExprKind::Call { callee, args } => {
                let func_name = match &callee.kind {
                    ExprKind::Ident(name) => name.clone(),
                    _ => return Err(error(expr.line, "Can only call named functions")),
                };
                let mut eval_args = Vec::new();
                for arg in args {
                    eval_args.push(self.eval_expr(arg)?);
                }
                self.call_function(&func_name, eval_args, expr.line)
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
                self.index_into(&obj, &idx, expr.line)
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
            BinOp::Add => match (left, right) {
                (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a + b)),
                (Value::Str(a), Value::Str(b)) => {
                    builtins::check_string_concat_memory(a.len(), b.len(), line)?;
                    Ok(Value::new_str(format!("{}{}", a, b)))
                }
                (Value::Str(a), b) => {
                    let b_display = format!("{}", b);
                    builtins::check_string_concat_memory(a.len(), b_display.len(), line)?;
                    Ok(Value::new_str(format!("{}{}", a, b_display)))
                }
                (a, Value::Str(b)) => {
                    let a_display = format!("{}", a);
                    builtins::check_string_concat_memory(a_display.len(), b.len(), line)?;
                    Ok(Value::new_str(format!("{}{}", a_display, b)))
                }
                (Value::Array(a), Value::Array(b)) => {
                    builtins::check_array_concat_memory(a.len(), b.len(), line)?;
                    let mut result = a.to_vec();
                    result.extend(b.to_vec());
                    Ok(Value::new_array(result))
                }
                _ => Err(error(
                    line,
                    format!("Can't add {} and {}", left.type_name(), right.type_name()),
                )),
            },
            BinOp::Sub => self.numeric_op(left, right, |a, b| a - b, "-", line),
            BinOp::Mul => match (left, right) {
                (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a * b)),
                (Value::Str(s), Value::Number(n)) | (Value::Number(n), Value::Str(s)) => {
                    let nf = *n;
                    if nf < 0.0 || !nf.is_finite() {
                        return Err(error(line, format!("Can't repeat a string {} times", nf)));
                    }
                    let count = nf as usize;
                    builtins::check_string_repeat_memory(s.len(), count, line)?;
                    Ok(Value::new_str(s.repeat(count)))
                }
                _ => Err(error(
                    line,
                    format!(
                        "Can't multiply {} and {}",
                        left.type_name(),
                        right.type_name()
                    ),
                )),
            },
            BinOp::Div => match (left, right) {
                (Value::Number(_), Value::Number(b)) if *b == 0.0 => {
                    Err(error_with_hint(line, "Division by zero", "You can't divide by 0."))
                }
                (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a / b)),
                _ => Err(error(
                    line,
                    format!("Can't divide {} by {}", left.type_name(), right.type_name()),
                )),
            },
            BinOp::Mod => match (left, right) {
                (Value::Number(_), Value::Number(b)) if *b == 0.0 => {
                    Err(error_with_hint(line, "Modulo by zero", "You can't use % with 0."))
                }
                (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a % b)),
                _ => Err(error(
                    line,
                    format!(
                        "Can't use % with {} and {}",
                        left.type_name(),
                        right.type_name()
                    ),
                )),
            },
            BinOp::Eq => Ok(Value::Bool(values_equal(left, right))),
            BinOp::NotEq => Ok(Value::Bool(!values_equal(left, right))),
            BinOp::Lt => self.compare_op(left, right, |a, b| a < b, "<", line),
            BinOp::Gt => self.compare_op(left, right, |a, b| a > b, ">", line),
            BinOp::LtEq => self.compare_op(left, right, |a, b| a <= b, "<=", line),
            BinOp::GtEq => self.compare_op(left, right, |a, b| a >= b, ">=", line),
            BinOp::And | BinOp::Or => unreachable!("handled in eval_expr"),
        }
    }

    fn numeric_op(
        &self,
        left: &Value,
        right: &Value,
        f: impl Fn(f64, f64) -> f64,
        op_str: &str,
        line: u32,
    ) -> Result<Value, BopError> {
        match (left, right) {
            (Value::Number(a), Value::Number(b)) => Ok(Value::Number(f(*a, *b))),
            _ => Err(error(
                line,
                format!(
                    "Can't use `{}` with {} and {}",
                    op_str,
                    left.type_name(),
                    right.type_name()
                ),
            )),
        }
    }

    fn compare_op(
        &self,
        left: &Value,
        right: &Value,
        f: impl Fn(f64, f64) -> bool,
        op_str: &str,
        line: u32,
    ) -> Result<Value, BopError> {
        match (left, right) {
            (Value::Number(a), Value::Number(b)) => Ok(Value::Bool(f(*a, *b))),
            (Value::Str(a), Value::Str(b)) => {
                let result = match op_str {
                    "<" => a < b,
                    ">" => a > b,
                    "<=" => a <= b,
                    _ => a >= b,
                };
                Ok(Value::Bool(result))
            }
            _ => Err(error(
                line,
                format!(
                    "Can't compare {} and {} with `{}`",
                    left.type_name(),
                    right.type_name(),
                    op_str
                ),
            )),
        }
    }

    // ─── Indexing ──────────────────────────────────────────────────

    fn index_into(&self, obj: &Value, idx: &Value, line: u32) -> Result<Value, BopError> {
        match (obj, idx) {
            (Value::Array(arr), Value::Number(n)) => {
                let i = *n as i64;
                let actual = if i < 0 {
                    (arr.len() as i64 + i) as usize
                } else {
                    i as usize
                };
                arr.get(actual).cloned().ok_or_else(|| {
                    error(
                        line,
                        format!(
                            "Index {} is out of bounds (array has {} items)",
                            i,
                            arr.len()
                        ),
                    )
                })
            }
            (Value::Str(s), Value::Number(n)) => {
                let i = *n as i64;
                let chars: Vec<char> = s.chars().collect();
                let actual = if i < 0 {
                    (chars.len() as i64 + i) as usize
                } else {
                    i as usize
                };
                chars
                    .get(actual)
                    .map(|c| Value::new_str(c.to_string()))
                    .ok_or_else(|| {
                        error(
                            line,
                            format!(
                                "Index {} is out of bounds (string has {} characters)",
                                i,
                                chars.len()
                            ),
                        )
                    })
            }
            (Value::Dict(entries), Value::Str(key)) => entries
                .iter()
                .find(|(k, _)| k.as_str() == key.as_str())
                .map(|(_, v)| v.clone())
                .ok_or_else(|| error(line, format!("Key \"{}\" not found in dict", key))),
            _ => Err(error(
                line,
                format!("Can't index {} with {}", obj.type_name(), idx.type_name()),
            )),
        }
    }

    fn set_index(
        &self,
        obj: &mut Value,
        idx: &Value,
        val: Value,
        line: u32,
    ) -> Result<(), BopError> {
        match (obj, idx) {
            (Value::Array(arr), Value::Number(n)) => {
                let i = *n as i64;
                let len = arr.len();
                let actual = if i < 0 {
                    (len as i64 + i) as usize
                } else {
                    i as usize
                };
                if actual >= len {
                    return Err(error(
                        line,
                        format!("Index {} is out of bounds (array has {} items)", i, len),
                    ));
                }
                arr.set(actual, val);
                Ok(())
            }
            (Value::Dict(entries), Value::Str(key)) => {
                entries.set_key(key, val);
                Ok(())
            }
            _ => Err(error(line, "Can't set index with these types")),
        }
    }

    // ─── Function calls ────────────────────────────────────────────

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

        // 3. User-defined functions
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

        if args.len() != func.params.len() {
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

        // Check recursion depth
        if self.call_depth >= MAX_CALL_DEPTH {
            return Err(error_with_hint(
                line,
                "Too many nested function calls (possible infinite recursion)",
                "Check that your recursive function has a base case that stops calling itself.",
            ));
        }

        // Clean scope for function (no outer variables)
        self.call_depth += 1;
        let saved_scopes = std::mem::replace(&mut self.scopes, vec![HashMap::new()]);
        for (param, arg) in func.params.iter().zip(args) {
            self.define(param.clone(), arg);
        }

        let result = self.exec_block(&func.body);
        self.scopes = saved_scopes;
        self.call_depth -= 1;

        match result? {
            Signal::Return(val) => Ok(val),
            Signal::Break => Err(error(line, "break used outside of a loop")),
            Signal::Continue => Err(error(line, "continue used outside of a loop")),
            Signal::None => Ok(Value::None),
        }
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
