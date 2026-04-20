//! AST → bytecode compilation. See `crate` docs for the instruction
//! set overview.

#[cfg(not(feature = "std"))]
use alloc::{string::{String, ToString}, vec::Vec};

use bop::error::BopError;
use bop::parser::{AssignOp, AssignTarget, BinOp, Expr, ExprKind, Stmt, StmtKind, UnaryOp};

use crate::chunk::{
    Chunk, CodeOffset, ConstIdx, Constant, FnDef, FnIdx, InterpIdx, InterpRecipe, Instr, NameIdx,
};

/// Compile a parsed program into a top-level chunk.
pub fn compile(program: &[Stmt]) -> Result<Chunk, BopError> {
    let mut compiler = Compiler::new();
    compiler.compile_block_no_scope(program)?;
    compiler.emit(Instr::Halt, 0);
    Ok(compiler.finish())
}

// ─── Compiler state ────────────────────────────────────────────────

struct Compiler {
    chunk: Chunk,
    loops: Vec<LoopCtx>,
}

struct LoopCtx {
    /// Absolute offset inside the current chunk that a `continue`
    /// should jump to.
    continue_target: CodeOffset,
    /// Offsets of `Jump` instructions that need to be back-patched to
    /// the loop's exit once it's known.
    break_patches: Vec<CodeOffset>,
}

impl Compiler {
    fn new() -> Self {
        Self {
            chunk: Chunk::new(),
            loops: Vec::new(),
        }
    }

    fn finish(self) -> Chunk {
        self.chunk
    }

    // ─── Emission helpers ─────────────────────────────────────────

    fn emit(&mut self, instr: Instr, line: u32) -> CodeOffset {
        let offset = CodeOffset(self.chunk.code.len() as u32);
        self.chunk.code.push(instr);
        self.chunk.lines.push(line);
        offset
    }

    fn current_offset(&self) -> CodeOffset {
        CodeOffset(self.chunk.code.len() as u32)
    }

    fn patch_jump(&mut self, site: CodeOffset, target: CodeOffset) {
        let idx = site.0 as usize;
        self.chunk.code[idx] = match self.chunk.code[idx].clone() {
            Instr::Jump(_) => Instr::Jump(target),
            Instr::JumpIfFalse(_) => Instr::JumpIfFalse(target),
            Instr::JumpIfFalsePeek(_) => Instr::JumpIfFalsePeek(target),
            Instr::JumpIfTruePeek(_) => Instr::JumpIfTruePeek(target),
            Instr::IterNext { .. } => Instr::IterNext { target },
            Instr::RepeatNext { .. } => Instr::RepeatNext { target },
            other => panic!("patch_jump on non-jump instruction: {:?}", other),
        };
    }

    // ─── Pool helpers ─────────────────────────────────────────────

    fn add_const(&mut self, c: Constant) -> ConstIdx {
        // Dedup numbers and strings so the pool doesn't grow quadratically
        // on programs that reuse literals heavily.
        if let Some(i) = self.chunk.constants.iter().position(|existing| {
            match (existing, &c) {
                (Constant::Number(a), Constant::Number(b)) => a.to_bits() == b.to_bits(),
                (Constant::Str(a), Constant::Str(b)) => a == b,
                _ => false,
            }
        }) {
            return ConstIdx(i as u32);
        }
        let idx = ConstIdx(self.chunk.constants.len() as u32);
        self.chunk.constants.push(c);
        idx
    }

    fn add_name(&mut self, name: &str) -> NameIdx {
        if let Some(i) = self.chunk.names.iter().position(|n| n == name) {
            return NameIdx(i as u32);
        }
        let idx = NameIdx(self.chunk.names.len() as u32);
        self.chunk.names.push(name.to_string());
        idx
    }

    fn add_interp(&mut self, recipe: InterpRecipe) -> InterpIdx {
        let idx = InterpIdx(self.chunk.interps.len() as u32);
        self.chunk.interps.push(recipe);
        idx
    }

    fn add_function(&mut self, def: FnDef) -> FnIdx {
        let idx = FnIdx(self.chunk.functions.len() as u32);
        self.chunk.functions.push(def);
        idx
    }

    // ─── Statements ───────────────────────────────────────────────

    /// Compile a sequence of statements without opening a new scope.
    /// Used for the program root and function bodies (which get their
    /// own scope from the caller).
    fn compile_block_no_scope(&mut self, stmts: &[Stmt]) -> Result<(), BopError> {
        for stmt in stmts {
            self.compile_stmt(stmt)?;
        }
        Ok(())
    }

    /// Compile a block with an enclosing `PushScope` / `PopScope`.
    fn compile_scoped_block(&mut self, stmts: &[Stmt], line: u32) -> Result<(), BopError> {
        self.emit(Instr::PushScope, line);
        self.compile_block_no_scope(stmts)?;
        self.emit(Instr::PopScope, line);
        Ok(())
    }

    fn compile_stmt(&mut self, stmt: &Stmt) -> Result<(), BopError> {
        let line = stmt.line;
        match &stmt.kind {
            StmtKind::Let { name, value } => {
                self.compile_expr(value)?;
                let n = self.add_name(name);
                self.emit(Instr::DefineLocal(n), line);
            }

            StmtKind::Assign { target, op, value } => {
                self.compile_assign(target, op, value, line)?;
            }

            StmtKind::If {
                condition,
                body,
                else_ifs,
                else_body,
            } => {
                self.compile_if_chain(condition, body, else_ifs, else_body, line)?;
            }

            StmtKind::While { condition, body } => {
                let loop_start = self.current_offset();
                self.compile_expr(condition)?;
                let exit_jmp = self.emit(Instr::JumpIfFalse(CodeOffset(0)), line);

                self.loops.push(LoopCtx {
                    continue_target: loop_start,
                    break_patches: Vec::new(),
                });
                self.compile_scoped_block(body, line)?;
                self.emit(Instr::Jump(loop_start), line);

                let end = self.current_offset();
                self.patch_jump(exit_jmp, end);
                let ctx = self.loops.pop().expect("loop ctx");
                for patch in ctx.break_patches {
                    self.patch_jump(patch, end);
                }
            }

            StmtKind::Repeat { count, body } => {
                self.compile_expr(count)?;
                self.emit(Instr::MakeRepeatCount, line);
                let loop_start = self.current_offset();
                let exit_jmp =
                    self.emit(Instr::RepeatNext { target: CodeOffset(0) }, line);

                self.loops.push(LoopCtx {
                    continue_target: loop_start,
                    break_patches: Vec::new(),
                });
                self.compile_scoped_block(body, line)?;
                self.emit(Instr::Jump(loop_start), line);

                let end = self.current_offset();
                self.patch_jump(exit_jmp, end);
                let ctx = self.loops.pop().expect("loop ctx");
                for patch in ctx.break_patches {
                    self.patch_jump(patch, end);
                }
            }

            StmtKind::ForIn {
                var,
                iterable,
                body,
            } => {
                self.compile_expr(iterable)?;
                self.emit(Instr::MakeIter, line);
                let loop_start = self.current_offset();
                let exit_jmp =
                    self.emit(Instr::IterNext { target: CodeOffset(0) }, line);
                self.emit(Instr::PushScope, line);
                let var_n = self.add_name(var);
                self.emit(Instr::DefineLocal(var_n), line);

                self.loops.push(LoopCtx {
                    continue_target: loop_start,
                    break_patches: Vec::new(),
                });
                self.compile_block_no_scope(body)?;
                self.emit(Instr::PopScope, line);
                self.emit(Instr::Jump(loop_start), line);

                let end = self.current_offset();
                self.patch_jump(exit_jmp, end);
                let ctx = self.loops.pop().expect("loop ctx");
                for patch in ctx.break_patches {
                    self.patch_jump(patch, end);
                }
            }

            StmtKind::FnDecl { name, params, body } => {
                let def = self.compile_function(name, params, body)?;
                let idx = self.add_function(def);
                self.emit(Instr::DefineFn(idx), line);
            }

            StmtKind::Return { value } => {
                // A top-level `return` is compiled the same as an
                // in-function return; the VM treats a `Return` at the
                // top frame as a halt (matching the tree-walker, which
                // silently accepts a Signal::Return at program scope).
                match value {
                    Some(expr) => {
                        self.compile_expr(expr)?;
                        self.emit(Instr::Return, line);
                    }
                    None => {
                        self.emit(Instr::ReturnNone, line);
                    }
                }
            }

            StmtKind::Break => {
                if self.loops.is_empty() {
                    return Err(err(line, "break used outside of a loop"));
                }
                let patch = self.emit(Instr::Jump(CodeOffset(0)), line);
                self.loops.last_mut().unwrap().break_patches.push(patch);
            }

            StmtKind::Continue => {
                let target = match self.loops.last() {
                    Some(ctx) => ctx.continue_target,
                    None => return Err(err(line, "continue used outside of a loop")),
                };
                self.emit(Instr::Jump(target), line);
            }

            StmtKind::Import { path } => {
                let n = self.add_name(path);
                self.emit(Instr::Import(n), line);
            }

            StmtKind::StructDecl { .. } => {
                return Err(err(
                    line,
                    "bop-vm: struct declarations are not yet supported in the bytecode VM",
                ));
            }

            StmtKind::EnumDecl { .. } => {
                return Err(err(
                    line,
                    "bop-vm: enum declarations are not yet supported in the bytecode VM",
                ));
            }

            StmtKind::MethodDecl { .. } => {
                return Err(err(
                    line,
                    "bop-vm: user-defined methods are not yet supported in the bytecode VM",
                ));
            }

            StmtKind::ExprStmt(expr) => {
                self.compile_expr(expr)?;
                self.emit(Instr::Pop, line);
            }
        }
        Ok(())
    }

    fn compile_if_chain(
        &mut self,
        condition: &Expr,
        body: &[Stmt],
        else_ifs: &[(Expr, Vec<Stmt>)],
        else_body: &Option<Vec<Stmt>>,
        line: u32,
    ) -> Result<(), BopError> {
        // Flatten into an ordered list of conditional branches plus
        // an optional trailing `else`. Each conditional branch needs
        // a `Jump(end)` *only if* something follows it (another
        // conditional branch or an `else`). The last conditional
        // branch with no trailing `else` falls through naturally.
        let mut branches: Vec<(&Expr, &[Stmt])> = Vec::with_capacity(1 + else_ifs.len());
        branches.push((condition, body));
        for (cond, body) in else_ifs {
            branches.push((cond, body));
        }
        let has_else = else_body.is_some();

        let mut end_patches: Vec<CodeOffset> = Vec::new();

        for (i, (cond, body)) in branches.iter().enumerate() {
            let is_last_conditional = i == branches.len() - 1;
            let needs_skip = !is_last_conditional || has_else;

            self.compile_expr(cond)?;
            let next_patch = self.emit(Instr::JumpIfFalse(CodeOffset(0)), line);
            self.compile_scoped_block(body, line)?;
            if needs_skip {
                end_patches.push(self.emit(Instr::Jump(CodeOffset(0)), line));
            }
            let next_target = self.current_offset();
            self.patch_jump(next_patch, next_target);
        }

        if let Some(else_body) = else_body {
            self.compile_scoped_block(else_body, line)?;
        }

        let end = self.current_offset();
        for patch in end_patches {
            self.patch_jump(patch, end);
        }
        Ok(())
    }

    fn compile_assign(
        &mut self,
        target: &AssignTarget,
        op: &AssignOp,
        value: &Expr,
        line: u32,
    ) -> Result<(), BopError> {
        match target {
            AssignTarget::Variable(name) => {
                let n = self.add_name(name);
                match op {
                    AssignOp::Eq => {
                        self.compile_expr(value)?;
                    }
                    compound => {
                        self.emit(Instr::LoadVar(n), line);
                        self.compile_expr(value)?;
                        self.emit(binop_for_compound(*compound), line);
                    }
                }
                self.emit(Instr::StoreVar(n), line);
            }

            AssignTarget::Index { object, index } => {
                // Mirror tree-walker: only bare Ident objects are
                // assignable; anything else is a compile-time error.
                let name = match &object.kind {
                    ExprKind::Ident(n) => n.clone(),
                    _ => {
                        return Err(err(
                            line,
                            "Can only assign to indexed variables (like `arr[0] = val`)",
                        ));
                    }
                };
                let name_idx = self.add_name(&name);

                match op {
                    AssignOp::Eq => {
                        self.emit(Instr::LoadVar(name_idx), line);
                        self.compile_expr(index)?;
                        self.compile_expr(value)?;
                        self.emit(Instr::SetIndex, line);
                    }
                    compound => {
                        self.emit(Instr::LoadVar(name_idx), line);
                        self.compile_expr(index)?;
                        self.emit(Instr::Dup2, line);
                        self.emit(Instr::GetIndex, line);
                        self.compile_expr(value)?;
                        self.emit(binop_for_compound(*compound), line);
                        self.emit(Instr::SetIndex, line);
                    }
                }
                self.emit(Instr::StoreVar(name_idx), line);
            }
            AssignTarget::Field { .. } => {
                return Err(err(
                    line,
                    "bop-vm: struct field assignment is not yet supported in the bytecode VM",
                ));
            }
        }
        Ok(())
    }

    // ─── Expressions ──────────────────────────────────────────────

    fn compile_expr(&mut self, expr: &Expr) -> Result<(), BopError> {
        let line = expr.line;
        match &expr.kind {
            ExprKind::Number(n) => {
                let c = self.add_const(Constant::Number(*n));
                self.emit(Instr::LoadConst(c), line);
            }
            ExprKind::Str(s) => {
                let c = self.add_const(Constant::Str(s.clone()));
                self.emit(Instr::LoadConst(c), line);
            }
            ExprKind::Bool(b) => {
                self.emit(if *b { Instr::LoadTrue } else { Instr::LoadFalse }, line);
            }
            ExprKind::None => {
                self.emit(Instr::LoadNone, line);
            }

            ExprKind::StringInterp(parts) => {
                let recipe = InterpRecipe { parts: parts.clone() };
                let idx = self.add_interp(recipe);
                self.emit(Instr::StringInterp(idx), line);
            }

            ExprKind::Ident(name) => {
                let n = self.add_name(name);
                self.emit(Instr::LoadVar(n), line);
            }

            ExprKind::BinaryOp { left, op, right } => {
                self.compile_binary(left, *op, right, line)?;
            }

            ExprKind::UnaryOp { op, expr: inner } => {
                self.compile_expr(inner)?;
                self.emit(
                    match op {
                        UnaryOp::Neg => Instr::Neg,
                        UnaryOp::Not => Instr::Not,
                    },
                    line,
                );
            }

            ExprKind::Call { callee, args } => {
                // Ident callees take the name-based fast path so
                // builtins / host / named-fn dispatch stays O(1).
                // Anything else (indexed call, nested call result,
                // if-expr returning a fn, …) evaluates the callee
                // onto the stack and goes through `CallValue`.
                if let ExprKind::Ident(name) = &callee.kind {
                    for arg in args {
                        self.compile_expr(arg)?;
                    }
                    let name_idx = self.add_name(name);
                    self.emit(
                        Instr::Call {
                            name: name_idx,
                            argc: args.len() as u32,
                        },
                        line,
                    );
                } else {
                    self.compile_expr(callee)?;
                    for arg in args {
                        self.compile_expr(arg)?;
                    }
                    self.emit(
                        Instr::CallValue {
                            argc: args.len() as u32,
                        },
                        line,
                    );
                }
            }

            ExprKind::MethodCall {
                object,
                method,
                args,
            } => {
                let assign_back_to = match &object.kind {
                    ExprKind::Ident(n) => Some(self.add_name(n)),
                    _ => None,
                };
                self.compile_expr(object)?;
                for arg in args {
                    self.compile_expr(arg)?;
                }
                let method_idx = self.add_name(method);
                self.emit(
                    Instr::CallMethod {
                        method: method_idx,
                        argc: args.len() as u32,
                        assign_back_to,
                    },
                    line,
                );
            }

            ExprKind::Index { object, index } => {
                self.compile_expr(object)?;
                self.compile_expr(index)?;
                self.emit(Instr::GetIndex, line);
            }

            ExprKind::Array(elements) => {
                for e in elements {
                    self.compile_expr(e)?;
                }
                self.emit(Instr::MakeArray(elements.len() as u32), line);
            }

            ExprKind::Dict(entries) => {
                for (key, value) in entries {
                    let c = self.add_const(Constant::Str(key.clone()));
                    self.emit(Instr::LoadConst(c), line);
                    self.compile_expr(value)?;
                }
                self.emit(Instr::MakeDict(entries.len() as u32), line);
            }

            ExprKind::IfExpr {
                condition,
                then_expr,
                else_expr,
            } => {
                self.compile_expr(condition)?;
                let else_jmp =
                    self.emit(Instr::JumpIfFalse(CodeOffset(0)), line);
                self.compile_expr(then_expr)?;
                let end_jmp = self.emit(Instr::Jump(CodeOffset(0)), line);

                let else_start = self.current_offset();
                self.patch_jump(else_jmp, else_start);
                self.compile_expr(else_expr)?;

                let end = self.current_offset();
                self.patch_jump(end_jmp, end);
            }

            ExprKind::FieldAccess { .. } => {
                return Err(err(
                    line,
                    "bop-vm: struct field access (obj.field) is not yet supported in the bytecode VM",
                ));
            }

            ExprKind::StructConstruct { .. } => {
                return Err(err(
                    line,
                    "bop-vm: struct literals are not yet supported in the bytecode VM",
                ));
            }

            ExprKind::EnumConstruct { .. } => {
                return Err(err(
                    line,
                    "bop-vm: enum variant construction is not yet supported in the bytecode VM",
                ));
            }

            ExprKind::Lambda { params, body } => {
                // Compile the body into the current chunk's fn
                // pool the same way named fn declarations do, but
                // emit `MakeLambda` instead of `DefineFn` at the
                // expression site so the VM materialises a
                // `Value::Fn` on the stack (capturing the current
                // scope at runtime) rather than binding a name.
                let def = self.compile_function("<lambda>", params, body)?;
                let idx = self.add_function(def);
                self.emit(Instr::MakeLambda(idx), line);
            }
        }
        Ok(())
    }

    fn compile_binary(
        &mut self,
        left: &Expr,
        op: BinOp,
        right: &Expr,
        line: u32,
    ) -> Result<(), BopError> {
        match op {
            BinOp::And => {
                self.compile_expr(left)?;
                self.emit(Instr::TruthyToBool, line);
                let short = self.emit(Instr::JumpIfFalsePeek(CodeOffset(0)), line);
                self.emit(Instr::Pop, line);
                self.compile_expr(right)?;
                self.emit(Instr::TruthyToBool, line);
                let end = self.current_offset();
                self.patch_jump(short, end);
                return Ok(());
            }
            BinOp::Or => {
                self.compile_expr(left)?;
                self.emit(Instr::TruthyToBool, line);
                let short = self.emit(Instr::JumpIfTruePeek(CodeOffset(0)), line);
                self.emit(Instr::Pop, line);
                self.compile_expr(right)?;
                self.emit(Instr::TruthyToBool, line);
                let end = self.current_offset();
                self.patch_jump(short, end);
                return Ok(());
            }
            _ => {}
        }

        self.compile_expr(left)?;
        self.compile_expr(right)?;
        let instr = match op {
            BinOp::Add => Instr::Add,
            BinOp::Sub => Instr::Sub,
            BinOp::Mul => Instr::Mul,
            BinOp::Div => Instr::Div,
            BinOp::Mod => Instr::Rem,
            BinOp::Eq => Instr::Eq,
            BinOp::NotEq => Instr::NotEq,
            BinOp::Lt => Instr::Lt,
            BinOp::Gt => Instr::Gt,
            BinOp::LtEq => Instr::LtEq,
            BinOp::GtEq => Instr::GtEq,
            BinOp::And | BinOp::Or => unreachable!("handled above"),
        };
        self.emit(instr, line);
        Ok(())
    }

    fn compile_function(
        &mut self,
        name: &str,
        params: &[String],
        body: &[Stmt],
    ) -> Result<FnDef, BopError> {
        let mut fn_compiler = Compiler::new();
        fn_compiler.compile_block_no_scope(body)?;
        // Implicit `return none` if control falls off the end.
        fn_compiler.emit(Instr::ReturnNone, 0);
        Ok(FnDef {
            name: name.to_string(),
            params: params.to_vec(),
            chunk: fn_compiler.finish(),
        })
    }
}

fn binop_for_compound(op: AssignOp) -> Instr {
    match op {
        AssignOp::Eq => unreachable!("caller excludes AssignOp::Eq"),
        AssignOp::AddEq => Instr::Add,
        AssignOp::SubEq => Instr::Sub,
        AssignOp::MulEq => Instr::Mul,
        AssignOp::DivEq => Instr::Div,
        AssignOp::ModEq => Instr::Rem,
    }
}

fn err(line: u32, message: &str) -> BopError {
    BopError::runtime(message, line)
}
