//! AST → bytecode compilation. See `crate` docs for the instruction
//! set overview.

#[cfg(feature = "no_std")]
use alloc::{string::{String, ToString}, vec, vec::Vec};

use bop::error::BopError;
use bop::parser::{AssignOp, AssignTarget, BinOp, Expr, ExprKind, Stmt, StmtKind, UnaryOp};

use crate::chunk::{
    CaptureSource, Chunk, CodeOffset, ConstIdx, Constant, EnumConstructShape, EnumDef, EnumIdx,
    EnumVariantDef, EnumVariantShape, FnDef, FnIdx, InterpIdx, InterpRecipe, Instr, NameIdx,
    PatternIdx, SlotIdx, StructDef, StructIdx,
};
use bop::parser::{MatchArm, Pattern, VariantKind};

// ─── Local slot resolver ───────────────────────────────────────────
//
// Tracks the name → slot mapping for the function currently being
// compiled. Each nested block (if / while / for-in body, match
// arm, etc.) pushes a fresh scope; exiting a block pops it. Slot
// indices only ever grow — a popped block's slots stay allocated
// in `next_slot`, so blocks never reuse slot numbers. That's
// slightly wasteful on memory (an unused `Value::None` per
// abandoned slot) but keeps `LoadLocal(i)` a trivial `Vec` read
// with no per-call setup beyond the one initial resize. `max_slot`
// records the high-water mark so the VM can pre-size its slot
// array exactly once at call time.

struct LocalResolver {
    /// Stack of scopes. Each scope holds the names that `let` /
    /// `for-in` / parameter introduced at this depth, paired with
    /// the slot index they claimed. Inner scopes shadow outer ones
    /// during name lookup (walked `.iter().rev()`).
    scopes: Vec<Vec<(String, SlotIdx)>>,
    /// Next slot number to hand out. Increments on every new
    /// binding and never rolls back.
    next_slot: u32,
    /// High-water mark across the whole function body. Becomes
    /// `FnDef::slot_count`.
    max_slot: u32,
}

impl LocalResolver {
    fn new(params: &[String]) -> Self {
        let mut scopes: Vec<Vec<(String, SlotIdx)>> = vec![Vec::with_capacity(params.len())];
        let mut next_slot = 0u32;
        for p in params {
            scopes[0].push((p.clone(), SlotIdx(next_slot)));
            next_slot += 1;
        }
        Self {
            scopes,
            next_slot,
            max_slot: next_slot,
        }
    }

    /// Allocate a fresh slot for `name` in the innermost scope.
    /// Returns the slot so the caller can emit a matching
    /// `StoreLocal(slot)`. If the name is already bound at this
    /// depth, it shadows — matches Bop's `let x = 1; let x = 2`
    /// semantics where the second binding wins.
    fn declare(&mut self, name: &str) -> SlotIdx {
        let slot = SlotIdx(self.next_slot);
        self.next_slot += 1;
        if self.next_slot > self.max_slot {
            self.max_slot = self.next_slot;
        }
        self.scopes
            .last_mut()
            .expect("resolver always has a scope")
            .push((name.to_string(), slot));
        slot
    }

    /// Resolve `name` to its slot by walking scopes inner-to-outer.
    /// Returns `None` if the name isn't a function-level local —
    /// the caller then falls back to the name-based `LoadVar` /
    /// `StoreVar` machinery so captures, imports, fn declarations
    /// and builtins still resolve.
    fn resolve(&self, name: &str) -> Option<SlotIdx> {
        for scope in self.scopes.iter().rev() {
            for (n, slot) in scope.iter().rev() {
                if n == name {
                    return Some(*slot);
                }
            }
        }
        None
    }

    fn push_scope(&mut self) {
        self.scopes.push(Vec::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
        // next_slot intentionally unchanged: a slot allocated
        // inside a now-dead block stays alive in the frame's
        // vec. Re-using the index would require tracking liveness
        // across control flow — not worth the complexity for a
        // few extra `Value::None` slots per function.
    }
}

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
    /// Stack of active resolvers. Empty at module top-level. A
    /// fn/lambda compile pushes a fresh resolver; nested fn/lambda
    /// compiles push another on top. The innermost one governs
    /// slot allocation and name-to-slot lookup; the rest are
    /// consulted only for capture resolution when an identifier
    /// inside the innermost body doesn't resolve locally.
    resolvers: Vec<LocalResolver>,
    /// Free variables seen by the innermost function body so far
    /// — names referenced that didn't resolve in the innermost
    /// resolver. Deduped (first occurrence wins), ordered so each
    /// name's index is its capture slot in the final `FnDef`.
    /// `None` at module top-level where there's nothing to
    /// capture into.
    free_vars: Option<Vec<String>>,
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
            resolvers: Vec::new(),
            free_vars: None,
        }
    }

    /// `Some(innermost_resolver)` iff we're inside a function body.
    fn current_resolver(&self) -> Option<&LocalResolver> {
        self.resolvers.last()
    }

    fn current_resolver_mut(&mut self) -> Option<&mut LocalResolver> {
        self.resolvers.last_mut()
    }

    /// Record an identifier that didn't resolve to a slot in the
    /// innermost function's resolver — it's either a capture
    /// (resolved when the lambda is built) or a reference to
    /// something reachable only at runtime (named fn, import).
    /// No-op at module top-level.
    fn note_free_var(&mut self, name: &str) {
        if let Some(list) = self.free_vars.as_mut() {
            if !list.iter().any(|n| n == name) {
                list.push(name.to_string());
            }
        }
    }

    fn finish(self) -> Chunk {
        self.chunk
    }

    // ─── Emission helpers ─────────────────────────────────────────

    fn emit(&mut self, instr: Instr, line: u32) -> CodeOffset {
        // Peephole: fuse a short trailing sequence with the
        // instruction we're about to emit when it matches a
        // known hot pattern. Rewriting is always at the current
        // tail, so no jump target can be pointing into the
        // collapsed range (loop / `&&` / `||` patch sites target
        // either before the sub-expression or after the
        // collapse, never inside it).
        if let Some(folded) = self.try_peephole(&instr) {
            self.chunk.code.push(folded);
            self.chunk.lines.push(line);
            return CodeOffset((self.chunk.code.len() - 1) as u32);
        }
        let offset = CodeOffset(self.chunk.code.len() as u32);
        self.chunk.code.push(instr);
        self.chunk.lines.push(line);
        offset
    }

    /// Trailing-sequence peephole. Returns `Some(fused)` if the
    /// incoming instruction collapses with the last few already
    /// in `chunk.code`. On a match we pop the matched tail and
    /// the caller pushes the fused op (keeping all lines /
    /// offsets consistent).
    fn try_peephole(&mut self, incoming: &Instr) -> Option<Instr> {
        let code = &self.chunk.code;
        match incoming {
            Instr::Add => {
                // `LoadLocal a; LoadLocal b; Add` →
                // `AddLocals(a, b)`
                if code.len() >= 2 {
                    if let (Instr::LoadLocal(a), Instr::LoadLocal(b)) =
                        (code[code.len() - 2], code[code.len() - 1])
                    {
                        self.chunk.code.truncate(code.len() - 2);
                        self.chunk.lines.truncate(self.chunk.lines.len() - 2);
                        return Some(Instr::AddLocals(a, b));
                    }
                    // `LoadLocal s; LoadConst(Int k); Add` →
                    // `LoadLocalAddInt(s, k)`. The `fib(n - 1)`
                    // / `array[i + 1]` pattern — one of the
                    // hottest sites in recursive benchmarks.
                    if let (Instr::LoadLocal(s), Instr::LoadConst(c)) =
                        (code[code.len() - 2], code[code.len() - 1])
                    {
                        if let crate::chunk::Constant::Int(k) =
                            self.chunk.constants[c.0 as usize]
                        {
                            if let Ok(k32) = i32::try_from(k) {
                                self.chunk.code.truncate(code.len() - 2);
                                self.chunk
                                    .lines
                                    .truncate(self.chunk.lines.len() - 2);
                                return Some(Instr::LoadLocalAddInt(s, k32));
                            }
                        }
                    }
                }
                None
            }
            Instr::Sub => {
                // `LoadLocal s; LoadConst(Int k); Sub` →
                // `LoadLocalAddInt(s, -k)`.
                if code.len() >= 2 {
                    if let (Instr::LoadLocal(s), Instr::LoadConst(c)) =
                        (code[code.len() - 2], code[code.len() - 1])
                    {
                        if let crate::chunk::Constant::Int(k) =
                            self.chunk.constants[c.0 as usize]
                        {
                            if let Some(neg) = k.checked_neg() {
                                if let Ok(k32) = i32::try_from(neg) {
                                    self.chunk.code.truncate(code.len() - 2);
                                    self.chunk
                                        .lines
                                        .truncate(self.chunk.lines.len() - 2);
                                    return Some(Instr::LoadLocalAddInt(s, k32));
                                }
                            }
                        }
                    }
                }
                None
            }
            Instr::Lt => {
                // `LoadLocal a; LoadLocal b; Lt` → `LtLocals(a, b)`
                if code.len() >= 2 {
                    if let (Instr::LoadLocal(a), Instr::LoadLocal(b)) =
                        (code[code.len() - 2], code[code.len() - 1])
                    {
                        self.chunk.code.truncate(code.len() - 2);
                        self.chunk.lines.truncate(self.chunk.lines.len() - 2);
                        return Some(Instr::LtLocals(a, b));
                    }
                    // `LoadLocal s; LoadConst(Int k); Lt` →
                    // `LtLocalInt(s, k)` — every `n < 2` base
                    // case in recursion lands here.
                    if let (Instr::LoadLocal(s), Instr::LoadConst(c)) =
                        (code[code.len() - 2], code[code.len() - 1])
                    {
                        if let crate::chunk::Constant::Int(k) =
                            self.chunk.constants[c.0 as usize]
                        {
                            if let Ok(k32) = i32::try_from(k) {
                                self.chunk.code.truncate(code.len() - 2);
                                self.chunk
                                    .lines
                                    .truncate(self.chunk.lines.len() - 2);
                                return Some(Instr::LtLocalInt(s, k32));
                            }
                        }
                    }
                }
                None
            }
            Instr::StoreLocal(store_slot) => {
                // `AddLocals(slot, other); StoreLocal(slot)` and
                // `LoadLocal(slot); LoadConst(k:Int); Add;
                // StoreLocal(slot)` both collapse to
                // `IncLocalInt(slot, k)` when the constant is a
                // small int — the `i = i + k` idiom.
                //
                // Pattern A (post-AddLocals fusion): detect
                // `AddLocals(slot, other) + StoreLocal(slot)`
                // only when `other` resolves to a constant via
                // the preceding `LoadLocal` — we don't handle
                // that here yet; stick with the direct 3-step
                // match instead.
                //
                // Pattern B: `LoadLocal(slot), LoadConst(Int k),
                // Add, StoreLocal(slot)`. The `Add` has already
                // been peephole-collapsed to `AddLocals(slot,
                // load_slot)` when both are locals — but for
                // `LoadLocal + LoadConst + Add` the peephole
                // above didn't fire, so the trailing sequence is
                // still `LoadLocal(slot), LoadConst(k), Add`.
                if code.len() >= 3 {
                    let n = code.len();
                    if let (
                        Instr::LoadLocal(ls),
                        Instr::LoadConst(c),
                        Instr::Add,
                    ) = (code[n - 3], code[n - 2], code[n - 1])
                    {
                        if ls == *store_slot {
                            if let crate::chunk::Constant::Int(k) = self
                                .chunk
                                .constants[c.0 as usize]
                            {
                                if let Ok(k32) = i32::try_from(k) {
                                    self.chunk.code.truncate(n - 3);
                                    self.chunk
                                        .lines
                                        .truncate(self.chunk.lines.len() - 3);
                                    return Some(Instr::IncLocalInt(*store_slot, k32));
                                }
                            }
                        }
                    }
                }
                None
            }
            _ => None,
        }
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
                (Constant::Int(a), Constant::Int(b)) => a == b,
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

    fn add_struct(&mut self, def: StructDef) -> StructIdx {
        let idx = StructIdx(self.chunk.struct_defs.len() as u32);
        self.chunk.struct_defs.push(def);
        idx
    }

    fn add_enum(&mut self, def: EnumDef) -> EnumIdx {
        let idx = EnumIdx(self.chunk.enum_defs.len() as u32);
        self.chunk.enum_defs.push(def);
        idx
    }

    fn add_pattern(&mut self, pat: Pattern) -> PatternIdx {
        let idx = PatternIdx(self.chunk.patterns.len() as u32);
        self.chunk.patterns.push(pat);
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

    /// Compile a block that introduces its own lexical scope.
    /// Inside a function body the scope lives purely in the
    /// compiler's `LocalResolver` (slot allocation) — no runtime
    /// opcode needed. At module top-level we fall back to
    /// `PushScope` / `PopScope` so the VM's BTreeMap scope stack
    /// still tracks block-local bindings.
    fn compile_scoped_block(&mut self, stmts: &[Stmt], line: u32) -> Result<(), BopError> {
        let fast = self.current_resolver().is_some();
        if fast {
            self.current_resolver_mut().unwrap().push_scope();
        } else {
            self.emit(Instr::PushScope, line);
        }
        self.compile_block_no_scope(stmts)?;
        if fast {
            self.current_resolver_mut().unwrap().pop_scope();
        } else {
            self.emit(Instr::PopScope, line);
        }
        Ok(())
    }

    fn compile_stmt(&mut self, stmt: &Stmt) -> Result<(), BopError> {
        let line = stmt.line;
        match &stmt.kind {
            StmtKind::Let { name, value, is_const: _ } => {
                self.compile_expr(value)?;
                if let Some(resolver) = self.current_resolver_mut() {
                    // Inside a function body: bind to a slot so
                    // subsequent reads compile to `LoadLocal`.
                    let slot = resolver.declare(name);
                    self.emit(Instr::StoreLocal(slot), line);
                } else {
                    // Module top-level: stay on the named-scope
                    // slow path so imports / dynamic injection
                    // keep working.
                    let n = self.add_name(name);
                    self.emit(Instr::DefineLocal(n), line);
                }
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

                // Inside a fn body the loop variable gets its own
                // slot and the body's nested lets get fresh slots
                // too (unique per iteration in the compiler's
                // accounting, but the VM reuses the same
                // underlying slot across iterations since control
                // flow reaches the same `StoreLocal`).
                let fast = self.current_resolver().is_some();
                if fast {
                    let resolver = self.current_resolver_mut().unwrap();
                    resolver.push_scope();
                    let slot = resolver.declare(var);
                    self.emit(Instr::StoreLocal(slot), line);
                } else {
                    self.emit(Instr::PushScope, line);
                    let var_n = self.add_name(var);
                    self.emit(Instr::DefineLocal(var_n), line);
                }

                self.loops.push(LoopCtx {
                    continue_target: loop_start,
                    break_patches: Vec::new(),
                });
                self.compile_block_no_scope(body)?;
                if fast {
                    self.current_resolver_mut().unwrap().pop_scope();
                } else {
                    self.emit(Instr::PopScope, line);
                }
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

            StmtKind::Use { path, items, alias } => {
                let spec = crate::chunk::UseSpec {
                    path: path.clone(),
                    items: items.clone(),
                    alias: alias.clone(),
                };
                let idx = crate::chunk::UseIdx(self.chunk.use_specs.len() as u32);
                self.chunk.use_specs.push(spec);
                self.emit(Instr::Use(idx), line);
            }

            StmtKind::StructDecl { name, fields } => {
                let def = StructDef {
                    name: name.clone(),
                    fields: fields.clone(),
                };
                let idx = self.add_struct(def);
                self.emit(Instr::DefineStruct(idx), line);
            }

            StmtKind::EnumDecl { name, variants } => {
                let def = EnumDef {
                    name: name.clone(),
                    variants: variants
                        .iter()
                        .map(|v| EnumVariantDef {
                            name: v.name.clone(),
                            shape: match &v.kind {
                                VariantKind::Unit => EnumVariantShape::Unit,
                                VariantKind::Tuple(fs) => EnumVariantShape::Tuple(fs.clone()),
                                VariantKind::Struct(fs) => {
                                    EnumVariantShape::Struct(fs.clone())
                                }
                            },
                        })
                        .collect(),
                };
                let idx = self.add_enum(def);
                self.emit(Instr::DefineEnum(idx), line);
            }

            StmtKind::MethodDecl {
                type_name,
                method_name,
                params,
                body,
            } => {
                let def = self.compile_function(method_name, params, body)?;
                let fn_idx = self.add_function(def);
                let type_name_idx = self.add_name(type_name);
                let method_name_idx = self.add_name(method_name);
                self.emit(
                    Instr::DefineMethod {
                        type_name: type_name_idx,
                        method_name: method_name_idx,
                        fn_idx,
                    },
                    line,
                );
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
        // Small helpers: emit a load / store against the same
        // binding, picking the slot fast path when the resolver
        // recognises the name and otherwise falling back to the
        // name-keyed slow path. Keeps each target arm from
        // re-doing the resolver dance by hand.
        match target {
            AssignTarget::Variable(name) => {
                let slot = self.current_resolver().and_then(|r| r.resolve(name));
                let n = self.add_name(name);
                match op {
                    AssignOp::Eq => {
                        self.compile_expr(value)?;
                    }
                    compound => {
                        self.emit_load_var(slot, n, line);
                        self.compile_expr(value)?;
                        self.emit(binop_for_compound(*compound), line);
                    }
                }
                self.emit_store_var(slot, n, line);
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
                let slot = self.current_resolver().and_then(|r| r.resolve(&name));
                let name_idx = self.add_name(&name);

                match op {
                    AssignOp::Eq => {
                        self.emit_load_var(slot, name_idx, line);
                        self.compile_expr(index)?;
                        self.compile_expr(value)?;
                        self.emit(Instr::SetIndex, line);
                    }
                    compound => {
                        self.emit_load_var(slot, name_idx, line);
                        self.compile_expr(index)?;
                        self.emit(Instr::Dup2, line);
                        self.emit(Instr::GetIndex, line);
                        self.compile_expr(value)?;
                        self.emit(binop_for_compound(*compound), line);
                        self.emit(Instr::SetIndex, line);
                    }
                }
                self.emit_store_var(slot, name_idx, line);
            }
            AssignTarget::Field { object, field } => {
                // Only bare-`Ident` objects are assignable — the
                // write-back goes through the same fast/slow
                // fork as regular assignment.
                let name = match &object.kind {
                    ExprKind::Ident(n) => n.clone(),
                    _ => {
                        return Err(err(
                            line,
                            "Can only assign to fields of named variables (like `p.x = val`)",
                        ));
                    }
                };
                let slot = self.current_resolver().and_then(|r| r.resolve(&name));
                let name_idx = self.add_name(&name);
                let field_idx = self.add_name(field);
                match op {
                    AssignOp::Eq => {
                        self.emit_load_var(slot, name_idx, line);
                        self.compile_expr(value)?;
                        self.emit(Instr::FieldSet(field_idx), line);
                    }
                    compound => {
                        self.emit_load_var(slot, name_idx, line);
                        self.emit(Instr::Dup, line);
                        self.emit(Instr::FieldGet(field_idx), line);
                        self.compile_expr(value)?;
                        self.emit(binop_for_compound(*compound), line);
                        self.emit(Instr::FieldSet(field_idx), line);
                    }
                }
                self.emit_store_var(slot, name_idx, line);
            }
        }
        Ok(())
    }

    /// Emit a variable load that picks the slot fast path when
    /// the caller already resolved it; otherwise falls back to
    /// the name-based `LoadVar` and notes the name as a free
    /// variable for the enclosing lambda's capture list.
    fn emit_load_var(&mut self, slot: Option<SlotIdx>, name: NameIdx, line: u32) {
        match slot {
            Some(s) => {
                self.emit(Instr::LoadLocal(s), line);
            }
            None => {
                let name_str = self.chunk.names[name.0 as usize].clone();
                self.note_free_var(&name_str);
                self.emit(Instr::LoadVar(name), line);
            }
        };
    }

    /// Emit a variable store — slot fast path when resolved, name-
    /// keyed `StoreVar` when the binding's in the BTreeMap scope
    /// slow path (module top-level, captures, match bindings).
    fn emit_store_var(&mut self, slot: Option<SlotIdx>, name: NameIdx, line: u32) {
        match slot {
            Some(s) => {
                self.emit(Instr::StoreLocal(s), line);
            }
            None => {
                let name_str = self.chunk.names[name.0 as usize].clone();
                self.note_free_var(&name_str);
                self.emit(Instr::StoreVar(name), line);
            }
        };
    }

    // ─── Expressions ──────────────────────────────────────────────

    fn compile_expr(&mut self, expr: &Expr) -> Result<(), BopError> {
        let line = expr.line;
        match &expr.kind {
            ExprKind::Int(n) => {
                let c = self.add_const(Constant::Int(*n));
                self.emit(Instr::LoadConst(c), line);
            }
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
                // Slot resolution first — inside a function body
                // this is the fast path. Falls through to the
                // name-based `LoadVar` for captures, imports,
                // named fns, and module-level bindings; the
                // fallback also records the name as a free
                // variable so `compile_function` can lift it into
                // the enclosing function's captures list when
                // this happens inside a lambda body.
                if let Some(slot) = self.current_resolver().and_then(|r| r.resolve(name)) {
                    self.emit(Instr::LoadLocal(slot), line);
                } else {
                    self.note_free_var(name);
                    let n = self.add_name(name);
                    self.emit(Instr::LoadVar(n), line);
                }
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
                // Three cases for bare-ident callees:
                //  1. The ident resolves to a function-level slot
                //     — it's a `Value::Fn` parameter or a lambda
                //     stored in a local. Load it onto the stack
                //     and go through `CallValue` so the VM
                //     dispatches on the `Value::Fn` directly.
                //  2. The ident is some other name (builtin,
                //     host fn, declared user fn, captured value)
                //     — the fast `Call { name }` path does the
                //     dynamic resolution.
                //  3. Non-ident callee (`funcs[0](x)`,
                //     `make_adder(5)(3)`) — evaluate the callee
                //     onto the stack, then `CallValue`.
                if let ExprKind::Ident(name) = &callee.kind {
                    if let Some(slot) = self.current_resolver().and_then(|r| r.resolve(name)) {
                        self.emit(Instr::LoadLocal(slot), line);
                        for arg in args {
                            self.compile_expr(arg)?;
                        }
                        self.emit(
                            Instr::CallValue {
                                argc: args.len() as u32,
                            },
                            line,
                        );
                    } else {
                        for arg in args {
                            self.compile_expr(arg)?;
                        }
                        let name_idx = self.add_name(name);
                        // `Call { name }` may end up consulting
                        // captures / scopes at runtime, so a
                        // bare-ident callee is effectively a free
                        // variable from the lambda's point of
                        // view. Record so the enclosing fn
                        // packages the binding at `MakeLambda`
                        // time (covers cases like `fn f() { let
                        // g = fn() { ... }; return fn() { g() }
                        // }` where the inner lambda calls a
                        // captured local by name).
                        self.note_free_var(name);
                        self.emit(
                            Instr::Call {
                                name: name_idx,
                                argc: args.len() as u32,
                            },
                            line,
                        );
                    }
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
                // A bare-ident receiver gets a write-back target
                // so mutating methods (`arr.push(v)`, etc.) update
                // the original binding. Slot-resolved locals go
                // through `AssignBack::Slot` for a direct frame
                // write; everything else keeps the name-keyed
                // fallback via `AssignBack::Name`.
                let assign_back_to = match &object.kind {
                    ExprKind::Ident(n) => {
                        if let Some(slot) = self.current_resolver().and_then(|r| r.resolve(n)) {
                            Some(crate::chunk::AssignBack::Slot(slot))
                        } else {
                            Some(crate::chunk::AssignBack::Name(self.add_name(n)))
                        }
                    }
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

            ExprKind::FieldAccess { object, field } => {
                self.compile_expr(object)?;
                let n = self.add_name(field);
                self.emit(Instr::FieldGet(n), line);
            }

            ExprKind::StructConstruct {
                namespace,
                type_name,
                fields,
            } => {
                // Push each (name, value) pair in the order
                // provided — the VM's `ConstructStruct` handler
                // does the matching against the declared fields,
                // so the compiler doesn't have to know the struct
                // shape at emit time.
                for (fname, fexpr) in fields {
                    let c = self.add_const(Constant::Str(fname.clone()));
                    self.emit(Instr::LoadConst(c), line);
                    self.compile_expr(fexpr)?;
                }
                let type_idx = self.add_name(type_name);
                let ns_idx = namespace.as_ref().map(|ns| self.add_name(ns));
                self.emit(
                    Instr::ConstructStruct {
                        namespace: ns_idx,
                        type_name: type_idx,
                        count: fields.len() as u32,
                    },
                    line,
                );
            }

            ExprKind::EnumConstruct {
                namespace,
                type_name,
                variant,
                payload,
            } => {
                use bop::parser::VariantPayload;
                let shape = match payload {
                    VariantPayload::Unit => EnumConstructShape::Unit,
                    VariantPayload::Tuple(args) => {
                        for a in args {
                            self.compile_expr(a)?;
                        }
                        EnumConstructShape::Tuple(args.len() as u32)
                    }
                    VariantPayload::Struct(fields) => {
                        for (fname, fexpr) in fields {
                            let c = self.add_const(Constant::Str(fname.clone()));
                            self.emit(Instr::LoadConst(c), line);
                            self.compile_expr(fexpr)?;
                        }
                        EnumConstructShape::Struct(fields.len() as u32)
                    }
                };
                let type_idx = self.add_name(type_name);
                let var_idx = self.add_name(variant);
                let ns_idx = namespace.as_ref().map(|ns| self.add_name(ns));
                self.emit(
                    Instr::ConstructEnum {
                        namespace: ns_idx,
                        type_name: type_idx,
                        variant: var_idx,
                        shape,
                    },
                    line,
                );
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

            ExprKind::Match { scrutinee, arms } => {
                self.compile_match(scrutinee, arms, line)?;
            }

            ExprKind::Try(inner) => {
                // Compile the scrutinee, then a single `TryUnwrap`
                // opcode inspects the result variant: unwraps
                // `Ok`, fast-returns on `Err`, or raises on any
                // other shape. Bundling the logic into one
                // instruction keeps the dispatch predictable and
                // lines up with walker / AOT behaviour.
                self.compile_expr(inner)?;
                self.emit(Instr::TryUnwrap, line);
            }
        }
        Ok(())
    }

    /// Emit bytecode for a `match` expression. The scrutinee is
    /// compiled once and kept on the value stack; each arm tests
    /// it with `MatchFail`, and falls through to the next arm on
    /// failure. A successful arm's body produces the match's
    /// result; `MatchExhausted` at the end raises a runtime error
    /// if every arm rejects.
    ///
    /// Stack shape across an arm (scope-deltas shown for clarity):
    ///
    /// ```text
    /// pre-arm:  [..., sc]
    /// PushScope                 [..., sc]           +scope
    /// Dup                       [..., sc, sc]
    /// MatchFail(pat, fail)      [..., sc]  on match: bindings applied
    ///                           [..., sc]  on fail : jumps, scope still open
    /// <guard>                   [..., sc, bool]
    /// JumpIfFalse(guard_fail)   [..., sc]           (pops the bool)
    /// Pop                       [...]
    /// <body>                    [..., result]
    /// PopScope                                       -scope
    /// Jump(end)
    /// guard_fail:
    /// PopScope                                       -scope
    /// (fall through to next arm)
    /// fail:
    /// PopScope                                       -scope
    /// (fall through to next arm)
    /// ```
    fn compile_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        line: u32,
    ) -> Result<(), BopError> {
        self.compile_expr(scrutinee)?;

        // Jump sites that each arm emits once it has produced the
        // match result; they all converge on the instruction after
        // `MatchExhausted`.
        let mut end_patches: Vec<CodeOffset> = Vec::with_capacity(arms.len());

        for arm in arms {
            let arm_line = arm.line;
            self.emit(Instr::PushScope, arm_line);
            self.emit(Instr::Dup, arm_line);
            let pat_idx = self.add_pattern(arm.pattern.clone());
            let match_fail_site = self.emit(
                Instr::MatchFail {
                    pattern: pat_idx,
                    on_fail: CodeOffset(0),
                },
                arm_line,
            );

            // Guard (if present) runs with bindings already in
            // scope; its failure unwinds the scope just like a
            // pattern mismatch.
            let guard_fail_site = if let Some(guard) = &arm.guard {
                self.compile_expr(guard)?;
                Some(self.emit(Instr::JumpIfFalse(CodeOffset(0)), arm_line))
            } else {
                None
            };

            // Committed to this arm: drop the scrutinee, emit the
            // body, unwind the arm scope, jump past the rest.
            self.emit(Instr::Pop, arm_line);
            self.compile_expr(&arm.body)?;
            self.emit(Instr::PopScope, arm_line);
            let end_jump = self.emit(Instr::Jump(CodeOffset(0)), arm_line);
            end_patches.push(end_jump);

            // Guard-failure landing pad: scope is still open with
            // the pattern bindings, so we unwind it before falling
            // through to the next arm. The scrutinee is still on
            // the stack because `MatchFail` consumed the `Dup`'d
            // copy, not the original.
            if let Some(gf) = guard_fail_site {
                let here = self.current_offset();
                self.patch_jump(gf, here);
                self.emit(Instr::PopScope, arm_line);
            }

            // Pattern-mismatch landing pad: same unwind, then
            // fall through to the next arm.
            let fail_target = self.current_offset();
            self.patch_match_fail(match_fail_site, fail_target);
            self.emit(Instr::PopScope, arm_line);
        }

        // Every arm rejected: drop the scrutinee and raise.
        self.emit(Instr::Pop, line);
        self.emit(Instr::MatchExhausted, line);

        let end = self.current_offset();
        for site in end_patches {
            self.patch_jump(site, end);
        }
        Ok(())
    }

    fn patch_match_fail(&mut self, site: CodeOffset, target: CodeOffset) {
        let idx = site.0 as usize;
        self.chunk.code[idx] = match self.chunk.code[idx].clone() {
            Instr::MatchFail { pattern, .. } => Instr::MatchFail {
                pattern,
                on_fail: target,
            },
            other => panic!("patch_match_fail on non-MatchFail instruction: {:?}", other),
        };
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

    /// Compile a function / lambda body. The body gets its own
    /// chunk, its own slot-based resolver, and its own free-var
    /// tracker — all saved/restored on `self` so the parent
    /// compile state isn't disturbed.
    ///
    /// Captures are resolved here: any free variable the body
    /// referenced becomes a `CaptureSource`. If the enclosing
    /// function's resolver has the name as a local, the source is
    /// `ParentSlot`; otherwise it's `ParentScope` (looked up at
    /// `MakeLambda` time via the enclosing frame's BTreeMap scope
    /// stack — so module-top-level bindings, captures-of-captures,
    /// and named fn references all keep working).
    fn compile_function(
        &mut self,
        name: &str,
        params: &[String],
        body: &[Stmt],
    ) -> Result<FnDef, BopError> {
        // Save outer compile state so the body's chunk / loops /
        // free-var list stays isolated.
        let saved_chunk = core::mem::take(&mut self.chunk);
        let saved_loops = core::mem::take(&mut self.loops);
        let saved_free = self.free_vars.take();

        // Enter the new function: push a resolver + start a
        // fresh free-var collector.
        self.resolvers.push(LocalResolver::new(params));
        self.free_vars = Some(Vec::new());

        // Compile the body into the new chunk. Catch errors so we
        // still restore state on the way out.
        let result = (|| {
            self.compile_block_no_scope(body)?;
            self.emit(Instr::ReturnNone, 0);
            Ok::<(), BopError>(())
        })();

        // Snapshot what we need from the function's compile
        // state before restoring the outer one.
        let resolver = self.resolvers.pop().expect("resolver pushed above");
        let free = self.free_vars.take().expect("free-vars set above");
        let mut chunk = core::mem::replace(&mut self.chunk, saved_chunk);
        self.loops = saved_loops;
        self.free_vars = saved_free;

        result?;

        // Resolve each free variable against the enclosing
        // resolver (if any) to decide whether it's a direct slot
        // read or a by-name scope read at `MakeLambda` time.
        //
        // Two-pass so the enclosing-frame read doesn't fight the
        // mut-borrow of `note_free_var`.
        let parent_is_function = !self.resolvers.is_empty();
        let resolutions: Vec<(String, CaptureSource)> = free
            .into_iter()
            .map(|name| {
                let slot = self
                    .resolvers
                    .last()
                    .and_then(|p| p.resolve(&name));
                let source = match slot {
                    Some(slot) => CaptureSource::ParentSlot(slot),
                    None => CaptureSource::ParentScope(name.clone()),
                };
                (name, source)
            })
            .collect();

        let mut capture_names: Vec<String> = Vec::with_capacity(resolutions.len());
        let mut capture_sources: Vec<CaptureSource> = Vec::with_capacity(resolutions.len());
        for (name, source) in resolutions {
            // If the enclosing scope is itself a function (so
            // `ParentScope` means "look at the outer fn's
            // captures"), make sure the outer fn knows to package
            // the name too — otherwise a nested lambda's
            // capture-of-capture would never reach its ultimate
            // source. Example: `fn f() { let x = 1; return fn() {
            // return fn() { return x } } }` — the inner lambda's
            // capture of x propagates outward via this re-note.
            if parent_is_function
                && matches!(source, CaptureSource::ParentScope(_))
            {
                self.note_free_var(&name);
            }
            capture_names.push(name);
            capture_sources.push(source);
        }

        chunk.slot_count = resolver.max_slot;
        Ok(FnDef {
            name: name.to_string(),
            params: params.to_vec(),
            chunk,
            slot_count: resolver.max_slot,
            capture_names,
            capture_sources,
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
