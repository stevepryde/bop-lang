//! Bytecode VM execution.
//!
//! This is step 2b of the execution-modes roadmap: a stack-based
//! dispatch loop that walks a [`Chunk`] produced by [`crate::compile`]
//! and produces the same observable behaviour as the tree-walking
//! evaluator in `bop-lang`.
//!
//! # Stack model
//!
//! All runtime values live on a single [`Slot`] stack. A [`Slot`] is
//! either a [`Value`], an in-progress `for`-loop iterator, or an
//! in-progress `repeat` counter. Iterators and counters are sidecar
//! items pushed by [`Instr::MakeIter`] / [`Instr::MakeRepeatCount`]
//! and consumed by [`Instr::IterNext`] / [`Instr::RepeatNext`]; only
//! bytecode that participates in iteration ever sees them, so the
//! rest of the dispatch loop treats the stack as a stack of `Value`.
//!
//! # Frames
//!
//! Each function call — including the top-level program — runs in its
//! own [`Frame`]. A frame owns its chunk (wrapped in `Rc` so repeated
//! calls share the compiled code), its instruction pointer, and its
//! lexical scope stack. Returning from a function pops the frame and
//! truncates the value stack back to the frame's base in case the body
//! left anything behind.
//!
//! # Resource limits
//!
//! [`BopLimits`] is enforced exactly as in the tree-walker:
//!
//! - A tick fires at every bytecode dispatch. It bumps `steps`,
//!   checks `max_steps`, checks [`bop::memory::bop_memory_exceeded`],
//!   and invokes [`BopHost::on_tick`]. `max_memory` is shared with the
//!   tree-walker via the per-value allocation tracking in
//!   [`bop::memory`], so no VM-specific bookkeeping is needed.
//! - The VM scales `max_steps` internally by [`STEP_SCALE`] so a single
//!   source-level step, which typically maps to several bytecode ops,
//!   still fits under the tree-walker's calibration of
//!   `BopLimits::standard()` / `BopLimits::demo()`.

#[cfg(not(feature = "std"))]
use alloc::{
    format,
    rc::Rc,
    string::{String, ToString},
    vec,
    vec::Vec,
};

#[cfg(feature = "std")]
use std::rc::Rc;

#[cfg(feature = "std")]
use std::collections::BTreeMap;
#[cfg(not(feature = "std"))]
use alloc::collections::BTreeMap;

use bop::builtins::{self, error, error_with_hint};
use bop::error::BopError;
use bop::lexer::StringPart;
use bop::methods;
use bop::ops;
use bop::value::{BopFn, FnBody, Value};
use bop::{BopHost, BopLimits};

use crate::chunk::{Chunk, CodeOffset, Constant, FnIdx, Instr, NameIdx};

/// Hard cap on call depth; matches the tree-walker.
const MAX_CALL_DEPTH: usize = 64;

/// `max_steps` is a source-level budget. One source-level statement
/// typically maps to several bytecode instructions, so the dispatch
/// loop gets a proportionally larger internal budget. Calibrated so
/// that `while true { }` still halts under `BopLimits::demo()` and
/// small programs like fizzbuzz don't exhaust `standard()`.
const STEP_SCALE: u64 = 8;

// ─── Stack slot ────────────────────────────────────────────────────

enum Slot {
    Value(Value),
    /// Remaining items in reverse order — `pop()` yields the next one.
    Iter(Vec<Value>),
    /// Remaining iterations for a `repeat` loop.
    Repeat(i64),
}

// ─── Frame ─────────────────────────────────────────────────────────

struct Frame {
    chunk: Rc<Chunk>,
    ip: usize,
    scopes: Vec<BTreeMap<String, Value>>,
    stack_base: usize,
    is_function: bool,
}

#[derive(Clone)]
struct FnEntry {
    params: Vec<String>,
    chunk: Rc<Chunk>,
}

// ─── Next action ───────────────────────────────────────────────────

enum Next {
    /// Keep fetching the next instruction.
    Continue,
    /// End the program (top-level `Halt`).
    Halt,
}

// ─── VM ────────────────────────────────────────────────────────────

/// Stack machine that executes a compiled [`Chunk`].
pub struct Vm<'h, H: BopHost> {
    frames: Vec<Frame>,
    stack: Vec<Slot>,
    functions: BTreeMap<String, FnEntry>,
    host: &'h mut H,
    steps: u64,
    step_budget: u64,
    rand_state: u64,
}

impl<'h, H: BopHost> Vm<'h, H> {
    pub fn new(chunk: Chunk, host: &'h mut H, limits: BopLimits) -> Self {
        bop::memory::bop_memory_init(limits.max_memory);
        let top = Frame {
            chunk: Rc::new(chunk),
            ip: 0,
            scopes: vec![BTreeMap::new()],
            stack_base: 0,
            is_function: false,
        };
        let step_budget = limits.max_steps.saturating_mul(STEP_SCALE);
        Self {
            frames: vec![top],
            stack: Vec::new(),
            functions: BTreeMap::new(),
            host,
            steps: 0,
            step_budget,
            rand_state: 0,
        }
    }

    pub fn run(mut self) -> Result<(), BopError> {
        loop {
            let (instr, line) = match self.fetch() {
                Some(x) => x,
                None => break,
            };
            self.tick(line)?;
            match self.dispatch(instr, line)? {
                Next::Continue => {}
                Next::Halt => break,
            }
        }
        Ok(())
    }

    // ─── Fetch / ip ──────────────────────────────────────────────

    fn fetch(&mut self) -> Option<(Instr, u32)> {
        let frame = self.frames.last_mut()?;
        if frame.ip >= frame.chunk.code.len() {
            return None;
        }
        let instr = frame.chunk.code[frame.ip].clone();
        let line = frame.chunk.lines[frame.ip];
        frame.ip += 1;
        Some((instr, line))
    }

    fn jump(&mut self, target: CodeOffset) {
        if let Some(frame) = self.frames.last_mut() {
            frame.ip = target.0 as usize;
        }
    }

    // ─── Tick ────────────────────────────────────────────────────

    fn tick(&mut self, line: u32) -> Result<(), BopError> {
        self.steps += 1;
        if self.steps > self.step_budget {
            return Err(error_with_hint(
                line,
                "Your code took too many steps (possible infinite loop)",
                "Check your loops — make sure they have a condition that eventually stops them.",
            ));
        }
        if bop::memory::bop_memory_exceeded() {
            return Err(error_with_hint(
                line,
                "Memory limit exceeded",
                "Your code is using too much memory. Check for large strings or arrays growing in loops.",
            ));
        }
        self.host.on_tick()?;
        Ok(())
    }

    // ─── Stack helpers ───────────────────────────────────────────

    fn push_value(&mut self, v: Value) {
        self.stack.push(Slot::Value(v));
    }

    fn pop_value(&mut self, line: u32) -> Result<Value, BopError> {
        match self.stack.pop() {
            Some(Slot::Value(v)) => Ok(v),
            Some(_) => Err(error(line, "VM: expected value on stack")),
            None => Err(error(line, "VM: stack underflow")),
        }
    }

    fn peek_value(&self, line: u32) -> Result<&Value, BopError> {
        match self.stack.last() {
            Some(Slot::Value(v)) => Ok(v),
            Some(_) => Err(error(line, "VM: expected value on stack")),
            None => Err(error(line, "VM: stack underflow")),
        }
    }

    fn pop_n_values(&mut self, n: usize, line: u32) -> Result<Vec<Value>, BopError> {
        if self.stack.len() < n {
            return Err(error(line, "VM: stack underflow"));
        }
        let start = self.stack.len() - n;
        let mut out = Vec::with_capacity(n);
        for slot in self.stack.drain(start..) {
            match slot {
                Slot::Value(v) => out.push(v),
                _ => return Err(error(line, "VM: expected value on stack")),
            }
        }
        Ok(out)
    }

    // ─── Scope ───────────────────────────────────────────────────

    fn current_scopes_mut(&mut self) -> &mut Vec<BTreeMap<String, Value>> {
        &mut self.frames.last_mut().expect("frame present").scopes
    }

    fn current_scopes(&self) -> &Vec<BTreeMap<String, Value>> {
        &self.frames.last().expect("frame present").scopes
    }

    fn push_scope(&mut self) {
        self.current_scopes_mut().push(BTreeMap::new());
    }

    fn pop_scope(&mut self) {
        let scopes = self.current_scopes_mut();
        if scopes.len() > 1 {
            scopes.pop();
        }
    }

    fn define_local(&mut self, name: String, value: Value) {
        if let Some(scope) = self.current_scopes_mut().last_mut() {
            scope.insert(name, value);
        }
    }

    fn lookup_var(&self, name: &str) -> Option<&Value> {
        for scope in self.current_scopes().iter().rev() {
            if let Some(v) = scope.get(name) {
                return Some(v);
            }
        }
        None
    }

    fn set_existing(&mut self, name: &str, value: Value) -> bool {
        for scope in self.current_scopes_mut().iter_mut().rev() {
            if scope.contains_key(name) {
                scope.insert(name.to_string(), value);
                return true;
            }
        }
        false
    }

    // ─── Dispatch ────────────────────────────────────────────────

    fn dispatch(&mut self, instr: Instr, line: u32) -> Result<Next, BopError> {
        match instr {
            // ─── Literals ─────────────────────────────────────────
            Instr::LoadConst(idx) => {
                let value = match self.current_chunk().constant(idx) {
                    Constant::Number(n) => Value::Number(*n),
                    Constant::Str(s) => Value::new_str(s.clone()),
                };
                self.push_value(value);
            }
            Instr::LoadNone => self.push_value(Value::None),
            Instr::LoadTrue => self.push_value(Value::Bool(true)),
            Instr::LoadFalse => self.push_value(Value::Bool(false)),

            // ─── Variables ────────────────────────────────────────
            Instr::LoadVar(n) => {
                let name = self.current_chunk().name(n).to_string();
                // Lexical scope first, then fall back to the
                // named-fn registry so `fn fib(...) {...}; let g =
                // fib` yields a real `Value::Fn` — same synthesis
                // the walker does via `self.functions`.
                if let Some(v) = self.lookup_var(&name).cloned() {
                    self.push_value(v);
                } else if let Some(entry) = self.functions.get(&name) {
                    let params = entry.params.clone();
                    let chunk_rc: Rc<Chunk> = entry.chunk.clone();
                    // Explicit two-step to drive the `Rc<Chunk>`
                    // → `Rc<dyn Any>` unsized coercion at assign
                    // time — `Rc::clone` through an expected
                    // `&Rc<dyn Any>` doesn't infer through.
                    let body: Rc<dyn core::any::Any + 'static> = chunk_rc;
                    let v = Value::new_compiled_fn(
                        params,
                        Vec::new(),
                        body,
                        Some(name.clone()),
                    );
                    self.push_value(v);
                } else {
                    return Err(error_with_hint(
                        line,
                        format!("Variable `{}` not found", name),
                        "Did you forget to create it with `let`?",
                    ));
                }
            }
            Instr::DefineLocal(n) => {
                let name = self.current_chunk().name(n).to_string();
                let v = self.pop_value(line)?;
                self.define_local(name, v);
            }
            Instr::StoreVar(n) => {
                let name = self.current_chunk().name(n).to_string();
                let v = self.pop_value(line)?;
                if !self.set_existing(&name, v) {
                    return Err(error_with_hint(
                        line,
                        format!("Variable `{}` doesn't exist yet", name),
                        format!("Use `let` to create a new variable: let {} = ...", name),
                    ));
                }
            }

            // ─── Scope ────────────────────────────────────────────
            Instr::PushScope => self.push_scope(),
            Instr::PopScope => self.pop_scope(),

            // ─── Stack ────────────────────────────────────────────
            Instr::Pop => {
                if self.stack.pop().is_none() {
                    return Err(error(line, "VM: stack underflow"));
                }
            }
            Instr::Dup => {
                let v = self.peek_value(line)?.clone();
                self.push_value(v);
            }
            Instr::Dup2 => {
                let len = self.stack.len();
                if len < 2 {
                    return Err(error(line, "VM: stack underflow"));
                }
                let b = match &self.stack[len - 1] {
                    Slot::Value(v) => v.clone(),
                    _ => return Err(error(line, "VM: expected value on stack")),
                };
                let a = match &self.stack[len - 2] {
                    Slot::Value(v) => v.clone(),
                    _ => return Err(error(line, "VM: expected value on stack")),
                };
                self.push_value(a);
                self.push_value(b);
            }

            // ─── Binary ops ───────────────────────────────────────
            Instr::Add => self.binary(line, ops::add)?,
            Instr::Sub => self.binary(line, ops::sub)?,
            Instr::Mul => self.binary(line, ops::mul)?,
            Instr::Div => self.binary(line, ops::div)?,
            Instr::Rem => self.binary(line, ops::rem)?,
            Instr::Eq => self.binary_infallible(line, |a, b, _| Ok(ops::eq(a, b)))?,
            Instr::NotEq => self.binary_infallible(line, |a, b, _| Ok(ops::not_eq(a, b)))?,
            Instr::Lt => self.binary(line, ops::lt)?,
            Instr::Gt => self.binary(line, ops::gt)?,
            Instr::LtEq => self.binary(line, ops::lt_eq)?,
            Instr::GtEq => self.binary(line, ops::gt_eq)?,

            // ─── Unary ops ────────────────────────────────────────
            Instr::Neg => {
                let v = self.pop_value(line)?;
                self.push_value(ops::neg(&v, line)?);
            }
            Instr::Not => {
                let v = self.pop_value(line)?;
                self.push_value(ops::not(&v));
            }

            Instr::TruthyToBool => {
                let v = self.pop_value(line)?;
                self.push_value(Value::Bool(v.is_truthy()));
            }

            // ─── Indexing ─────────────────────────────────────────
            Instr::GetIndex => {
                let idx = self.pop_value(line)?;
                let obj = self.pop_value(line)?;
                self.push_value(ops::index_get(&obj, &idx, line)?);
            }
            Instr::SetIndex => {
                let val = self.pop_value(line)?;
                let idx = self.pop_value(line)?;
                let mut obj = self.pop_value(line)?;
                ops::index_set(&mut obj, &idx, val, line)?;
                self.push_value(obj);
            }

            // ─── String interpolation ────────────────────────────
            Instr::StringInterp(idx) => {
                let recipe_parts = {
                    let recipe = self.current_chunk().interp(idx);
                    recipe.parts.clone()
                };
                self.push_value(self.build_interp(&recipe_parts, line)?);
            }

            // ─── Collections ──────────────────────────────────────
            Instr::MakeArray(n) => {
                let items = self.pop_n_values(n as usize, line)?;
                self.push_value(Value::new_array(items));
            }
            Instr::MakeDict(n) => {
                let flat = self.pop_n_values((n as usize) * 2, line)?;
                let mut entries: Vec<(String, Value)> = Vec::with_capacity(n as usize);
                let mut iter = flat.into_iter();
                while let (Some(key), Some(val)) = (iter.next(), iter.next()) {
                    let key_str = match &key {
                        Value::Str(s) => s.as_str().to_string(),
                        other => {
                            return Err(error(
                                line,
                                format!("Dict keys must be strings, got {}", other.type_name()),
                            ));
                        }
                    };
                    drop(key);
                    entries.push((key_str, val));
                }
                self.push_value(Value::new_dict(entries));
            }

            // ─── Calls ────────────────────────────────────────────
            Instr::Call { name, argc } => {
                return self.call(name, argc as usize, line);
            }
            Instr::CallValue { argc } => {
                return self.call_value(argc as usize, line);
            }
            Instr::CallMethod {
                method,
                argc,
                assign_back_to,
            } => {
                self.call_method(method, argc as usize, assign_back_to, line)?;
            }

            // ─── Functions ────────────────────────────────────────
            Instr::DefineFn(idx) => {
                self.define_fn(idx);
            }
            Instr::MakeLambda(idx) => {
                self.make_lambda(idx);
            }
            Instr::Return => {
                let v = self.pop_value(line)?;
                return self.do_return(v);
            }
            Instr::ReturnNone => {
                return self.do_return(Value::None);
            }

            // ─── Iteration / repeat ──────────────────────────────
            Instr::MakeIter => {
                let mut v = self.pop_value(line)?;
                let mut items: Vec<Value> = match &mut v {
                    Value::Array(arr) => arr.take(),
                    Value::Str(s) => s
                        .chars()
                        .map(|c| Value::new_str(c.to_string()))
                        .collect(),
                    other => {
                        return Err(error(
                            line,
                            format!("Can't iterate over {}", other.type_name()),
                        ));
                    }
                };
                drop(v);
                items.reverse(); // so pop() yields items in order
                self.stack.push(Slot::Iter(items));
            }
            Instr::IterNext { target } => {
                let next = match self.stack.last_mut() {
                    Some(Slot::Iter(items)) => items.pop(),
                    _ => return Err(error(line, "VM: expected iterator on stack")),
                };
                match next {
                    Some(item) => self.push_value(item),
                    None => {
                        self.stack.pop();
                        self.jump(target);
                    }
                }
            }
            Instr::MakeRepeatCount => {
                let v = self.pop_value(line)?;
                let n = match v {
                    Value::Number(n) => n as i64,
                    other => {
                        return Err(error(
                            line,
                            format!("repeat needs a number, but got {}", other.type_name()),
                        ));
                    }
                };
                self.stack.push(Slot::Repeat(n.max(0)));
            }
            Instr::RepeatNext { target } => {
                let done = match self.stack.last_mut() {
                    Some(Slot::Repeat(n)) => {
                        if *n > 0 {
                            *n -= 1;
                            false
                        } else {
                            true
                        }
                    }
                    _ => return Err(error(line, "VM: expected repeat counter on stack")),
                };
                if done {
                    self.stack.pop();
                    self.jump(target);
                }
            }

            // ─── Control flow ─────────────────────────────────────
            Instr::Jump(t) => self.jump(t),
            Instr::JumpIfFalse(t) => {
                let v = self.pop_value(line)?;
                if !v.is_truthy() {
                    self.jump(t);
                }
            }
            Instr::JumpIfFalsePeek(t) => {
                let truthy = self.peek_value(line)?.is_truthy();
                if !truthy {
                    self.jump(t);
                }
            }
            Instr::JumpIfTruePeek(t) => {
                let truthy = self.peek_value(line)?.is_truthy();
                if truthy {
                    self.jump(t);
                }
            }

            // ─── Termination ──────────────────────────────────────
            Instr::Halt => return Ok(Next::Halt),
        }

        Ok(Next::Continue)
    }

    // ─── Binary helpers ──────────────────────────────────────────

    fn binary(
        &mut self,
        line: u32,
        op: fn(&Value, &Value, u32) -> Result<Value, BopError>,
    ) -> Result<(), BopError> {
        let b = self.pop_value(line)?;
        let a = self.pop_value(line)?;
        self.push_value(op(&a, &b, line)?);
        Ok(())
    }

    fn binary_infallible(
        &mut self,
        line: u32,
        op: fn(&Value, &Value, u32) -> Result<Value, BopError>,
    ) -> Result<(), BopError> {
        let b = self.pop_value(line)?;
        let a = self.pop_value(line)?;
        self.push_value(op(&a, &b, line)?);
        Ok(())
    }

    // ─── String interpolation ────────────────────────────────────

    fn build_interp(&self, parts: &[StringPart], line: u32) -> Result<Value, BopError> {
        let mut result = String::new();
        for part in parts {
            match part {
                StringPart::Literal(s) => result.push_str(s),
                StringPart::Variable(name) => {
                    let v = self.lookup_var(name).ok_or_else(|| {
                        error(line, format!("Variable `{}` not found", name))
                    })?;
                    result.push_str(&format!("{}", v));
                }
            }
        }
        Ok(Value::new_str(result))
    }

    // ─── Chunk accessor ──────────────────────────────────────────

    fn current_chunk(&self) -> &Chunk {
        &self.frames.last().expect("frame present").chunk
    }

    // ─── Calls ───────────────────────────────────────────────────

    fn call(&mut self, name_idx: NameIdx, argc: usize, line: u32) -> Result<Next, BopError> {
        let name = self.current_chunk().name(name_idx).to_string();

        // Pop args (in source order).
        let args = self.pop_n_values(argc, line)?;

        // 0. Lexical callable: if the name is bound in the
        // current frame's scopes (e.g. `let f = fn() {...}`), call
        // it as a closure. Matches the tree-walker's
        // "let-binding shadows everything" behaviour.
        if let Some(value) = self.lookup_var(&name).cloned() {
            return match &value {
                Value::Fn(f) => {
                    let f = Rc::clone(f);
                    drop(value);
                    self.call_closure(&f, args, line)
                }
                other => Err(error(
                    line,
                    format!(
                        "`{}` is a {}, not a function",
                        name,
                        other.type_name()
                    ),
                )),
            };
        }

        // 1. Standard-library builtins.
        match name.as_str() {
            "range" => {
                let v = builtins::builtin_range(&args, line, &mut self.rand_state)?;
                self.push_value(v);
                return Ok(Next::Continue);
            }
            "str" => {
                let v = builtins::builtin_str(&args, line)?;
                self.push_value(v);
                return Ok(Next::Continue);
            }
            "int" => {
                let v = builtins::builtin_int(&args, line)?;
                self.push_value(v);
                return Ok(Next::Continue);
            }
            "type" => {
                let v = builtins::builtin_type(&args, line)?;
                self.push_value(v);
                return Ok(Next::Continue);
            }
            "abs" => {
                let v = builtins::builtin_abs(&args, line)?;
                self.push_value(v);
                return Ok(Next::Continue);
            }
            "min" => {
                let v = builtins::builtin_min(&args, line)?;
                self.push_value(v);
                return Ok(Next::Continue);
            }
            "max" => {
                let v = builtins::builtin_max(&args, line)?;
                self.push_value(v);
                return Ok(Next::Continue);
            }
            "rand" => {
                let v = builtins::builtin_rand(&args, line, &mut self.rand_state)?;
                self.push_value(v);
                return Ok(Next::Continue);
            }
            "len" => {
                let v = builtins::builtin_len(&args, line)?;
                self.push_value(v);
                return Ok(Next::Continue);
            }
            "inspect" => {
                let v = builtins::builtin_inspect(&args, line)?;
                self.push_value(v);
                return Ok(Next::Continue);
            }
            "print" => {
                let message = args
                    .iter()
                    .map(|a| format!("{}", a))
                    .collect::<Vec<_>>()
                    .join(" ");
                self.host.on_print(&message);
                self.push_value(Value::None);
                return Ok(Next::Continue);
            }
            _ => {}
        }

        // 2. Host-provided builtins.
        if let Some(result) = self.host.call(&name, &args, line) {
            let v = result?;
            self.push_value(v);
            return Ok(Next::Continue);
        }

        // 3. User-defined functions.
        let entry = match self.functions.get(&name) {
            Some(e) => e.clone(),
            None => {
                let hint = self.host.function_hint().to_string();
                return Err(if hint.is_empty() {
                    error(line, format!("Function `{}` not found", name))
                } else {
                    error_with_hint(line, format!("Function `{}` not found", name), hint)
                });
            }
        };

        if args.len() != entry.params.len() {
            return Err(error(
                line,
                format!(
                    "`{}` expects {} argument{}, but got {}",
                    name,
                    entry.params.len(),
                    if entry.params.len() == 1 { "" } else { "s" },
                    args.len()
                ),
            ));
        }

        if self.frames.len() >= MAX_CALL_DEPTH {
            return Err(error_with_hint(
                line,
                "Too many nested function calls (possible infinite recursion)",
                "Check that your recursive function has a base case that stops calling itself.",
            ));
        }

        // Build the callee frame with a fresh scope containing the
        // parameters.
        let mut scope = BTreeMap::new();
        for (param, arg) in entry.params.iter().zip(args) {
            scope.insert(param.clone(), arg);
        }
        let frame = Frame {
            chunk: entry.chunk.clone(),
            ip: 0,
            scopes: vec![scope],
            stack_base: self.stack.len(),
            is_function: true,
        };
        self.frames.push(frame);
        Ok(Next::Continue)
    }

    /// Dispatch a value-based call: `argc` args sit on top, the
    /// callee sits directly under them. Pops all `argc + 1` slots,
    /// expects the callee to be a `Value::Fn`, and delegates to
    /// `call_closure`.
    fn call_value(&mut self, argc: usize, line: u32) -> Result<Next, BopError> {
        let args = self.pop_n_values(argc, line)?;
        let callee = self.pop_value(line)?;
        match &callee {
            Value::Fn(f) => {
                let f = Rc::clone(f);
                drop(callee);
                self.call_closure(&f, args, line)
            }
            other => Err(error(
                line,
                format!("Can't call a {}", other.type_name()),
            )),
        }
    }

    fn call_method(
        &mut self,
        method_idx: NameIdx,
        argc: usize,
        assign_back_to: Option<NameIdx>,
        line: u32,
    ) -> Result<(), BopError> {
        let method = self.current_chunk().name(method_idx).to_string();

        let args = self.pop_n_values(argc, line)?;
        let obj = self.pop_value(line)?;

        let (ret, mutated) = match &obj {
            Value::Array(arr) => methods::array_method(arr, &method, &args, line)?,
            Value::Str(s) => methods::string_method(s.as_str(), &method, &args, line)?,
            Value::Dict(entries) => methods::dict_method(entries, &method, &args, line)?,
            _ => {
                return Err(error(
                    line,
                    format!("{} doesn't have a .{}() method", obj.type_name(), method),
                ));
            }
        };

        if methods::is_mutating_method(&method) {
            if let (Some(var_idx), Some(new_obj)) = (assign_back_to, mutated) {
                let var_name = self.current_chunk().name(var_idx).to_string();
                self.set_existing(&var_name, new_obj);
            }
        }
        self.push_value(ret);
        Ok(())
    }

    fn define_fn(&mut self, idx: FnIdx) {
        let fn_def = self.current_chunk().function(idx).clone();
        let entry = FnEntry {
            params: fn_def.params,
            chunk: Rc::new(fn_def.chunk),
        };
        self.functions.insert(fn_def.name, entry);
    }

    /// Materialise a lambda expression as a `Value::Fn`. Captures
    /// the flattened current scope at runtime (matching the
    /// tree-walker's snapshot semantics) and wraps the
    /// pre-compiled chunk as the closure's opaque body.
    fn make_lambda(&mut self, idx: FnIdx) {
        let fn_def = self.current_chunk().function(idx).clone();
        let captures = self.snapshot_captures();
        let body: Rc<dyn core::any::Any + 'static> = Rc::new(fn_def.chunk);
        let value = Value::new_compiled_fn(fn_def.params, captures, body, None);
        self.push_value(value);
    }

    /// Flatten the current frame's scope stack into a
    /// `(name, value)` list — inner scopes shadow outer ones. Used
    /// only by `make_lambda`.
    fn snapshot_captures(&self) -> Vec<(String, Value)> {
        let mut flat = BTreeMap::new();
        for scope in self.current_scopes() {
            for (k, v) in scope {
                flat.insert(k.clone(), v.clone());
            }
        }
        flat.into_iter().collect()
    }

    /// Call a `Value::Fn` by pushing a new frame whose scope holds
    /// the closure's captures plus its parameters (plus the
    /// closure itself under `self_name`, when present, so
    /// self-reference works without a separate pathway).
    fn call_closure(
        &mut self,
        func: &Rc<BopFn>,
        args: Vec<Value>,
        line: u32,
    ) -> Result<Next, BopError> {
        // The body must be a VM-compiled chunk. Walker-created
        // `Value::Fn`s would carry `FnBody::Ast` and don't belong
        // in the VM.
        let chunk: Rc<Chunk> = match &func.body {
            FnBody::Compiled(any) => match Rc::clone(any).downcast::<Chunk>() {
                Ok(c) => c,
                Err(_) => {
                    return Err(error(
                        line,
                        "Closure body wasn't compiled by the bytecode VM",
                    ));
                }
            },
            FnBody::Ast(_) => {
                return Err(error(
                    line,
                    "Closure body wasn't compiled for the VM — use `bop::run` to execute tree-walker closures",
                ));
            }
        };

        if args.len() != func.params.len() {
            let display_name = func.self_name.as_deref().unwrap_or("fn");
            return Err(error(
                line,
                format!(
                    "`{}` expects {} argument{}, but got {}",
                    display_name,
                    func.params.len(),
                    if func.params.len() == 1 { "" } else { "s" },
                    args.len()
                ),
            ));
        }

        if self.frames.len() >= MAX_CALL_DEPTH {
            return Err(error_with_hint(
                line,
                "Too many nested function calls (possible infinite recursion)",
                "Check that your recursive function has a base case that stops calling itself.",
            ));
        }

        // Seed the scope: captures first so params shadow on
        // collision, self-reference wins over everything so the
        // closure can find itself in the body.
        let mut scope = BTreeMap::new();
        for (name, value) in &func.captures {
            scope.insert(name.clone(), value.clone());
        }
        if let Some(self_name) = &func.self_name {
            scope.insert(self_name.clone(), Value::Fn(Rc::clone(func)));
        }
        for (param, arg) in func.params.iter().zip(args) {
            scope.insert(param.clone(), arg);
        }

        self.frames.push(Frame {
            chunk,
            ip: 0,
            scopes: vec![scope],
            stack_base: self.stack.len(),
            is_function: true,
        });
        Ok(Next::Continue)
    }

    fn do_return(&mut self, value: Value) -> Result<Next, BopError> {
        // Pop the current frame, truncate any frame-local stack
        // residue, and push the return value for the caller.
        let frame = self.frames.pop().expect("frame present");
        self.stack.truncate(frame.stack_base);
        if frame.is_function {
            self.push_value(value);
            Ok(Next::Continue)
        } else {
            // Return at top level: behave like Halt (matches tree-walker,
            // which silently accepts Signal::Return at program scope).
            drop(value);
            Ok(Next::Halt)
        }
    }
}

// ─── Public entry points ──────────────────────────────────────────

/// Execute a pre-compiled [`Chunk`] against the supplied host.
pub fn execute<H: BopHost>(
    chunk: Chunk,
    host: &mut H,
    limits: &BopLimits,
) -> Result<(), BopError> {
    let vm = Vm::new(chunk, host, limits.clone());
    vm.run()
}

/// Parse, compile, and run Bop source.
///
/// This mirrors [`bop::run`] but routes through the bytecode VM.
pub fn run<H: BopHost>(
    source: &str,
    host: &mut H,
    limits: &BopLimits,
) -> Result<(), BopError> {
    let stmts = bop::parse(source)?;
    let chunk = crate::compile(&stmts)?;
    execute(chunk, host, limits)
}
