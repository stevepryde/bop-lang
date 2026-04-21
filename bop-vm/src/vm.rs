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

use core::cell::RefCell;

#[cfg(feature = "std")]
use std::collections::{BTreeMap, BTreeSet};
#[cfg(not(feature = "std"))]
use alloc::collections::{BTreeMap, BTreeSet};

use bop::builtins::{self, error, error_fatal_with_hint, error_with_hint};
use bop::error::BopError;
use bop::lexer::StringPart;
use bop::methods;
use bop::ops;
use bop::value::{BopFn, FnBody, Value};
use bop::{BopHost, BopLimits};

use crate::chunk::{
    CaptureSource, Chunk, CodeOffset, Constant, EnumConstructShape, EnumIdx, EnumVariantShape,
    FnDef, FnIdx, Instr, NameIdx, PatternIdx, StructIdx,
};

/// Hard cap on call depth; matches the tree-walker.
const MAX_CALL_DEPTH: usize = 64;

/// `max_steps` is a source-level budget. One source-level statement
/// typically maps to several bytecode instructions, so the dispatch
/// loop gets a proportionally larger internal budget. Calibrated so
/// that `while true { }` still halts under `BopLimits::demo()` and
/// small programs like fizzbuzz don't exhaust `standard()`.
const STEP_SCALE: u64 = 8;

/// Memory-limit check cadence. Checked every `1 << N` ticks; a
/// power-of-two window lets us AND with a mask instead of
/// dividing. 256 is a sweet spot — detection still lands within
/// microseconds of the limit being breached, and a tight
/// arithmetic loop pays the TLS load once per 256 instructions
/// instead of every instruction.
const TICK_MEMCHECK_MASK: u64 = 0xFF;

/// Maximum slot vecs kept on the freelist. Deep recursion with
/// varying slot counts could otherwise grow this unboundedly.
/// `MAX_CALL_DEPTH` (64) plus a little headroom is plenty — any
/// extra allocations just go back through the global allocator
/// like before.
const SLOTS_FREELIST_CAP: usize = 128;

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
    /// Flat slot array for this function's compile-time-resolved
    /// locals (params + every `let` / `for-in` variable assigned
    /// a slot). `LoadLocal(slot)` / `StoreLocal(slot)` read and
    /// write directly into this vec — the VM's hot path for
    /// variable access. Empty for module-top-level frames and
    /// match-body sub-scopes where slot resolution doesn't apply.
    slots: Vec<Value>,
    /// Slow-path BTreeMap scope stack. Used for module-top-level
    /// bindings, match-pattern bindings, captures snapshotted
    /// into lambdas, and anything else that reaches `LoadVar` /
    /// `DefineLocal` / `StoreVar`. Function frames that stay on
    /// the fast path never push into this at runtime.
    scopes: Vec<BTreeMap<String, Value>>,
    stack_base: usize,
    is_function: bool,
    /// Marks this frame as the landing pad for a `try_call(f)`
    /// invocation. When set, a clean return wraps the value in
    /// `Result::Ok(v)` before pushing it for the caller; a
    /// non-fatal error unwinds to this frame and pushes
    /// `Result::Err(RuntimeError { message, line })` instead.
    /// Fatal errors still bypass the trap — see
    /// `BopError::is_fatal`.
    try_call_wrapper: bool,
}

struct FnEntry {
    params: Vec<String>,
    chunk: Rc<Chunk>,
}

/// Structural equality check for two enum-variant lists.
/// Used when the same module is resolved via two paths so a
/// re-import is idempotent rather than an error.
///
/// Matches the walker's `variants_equivalent` rule: variant
/// order + names must match, tuple variants compare by arity
/// only (payload names are positional stubs with no runtime
/// meaning), struct variants still require matching field
/// names.
fn shapes_match(
    a: &[(String, EnumVariantShape)],
    b: &[(String, EnumVariantShape)],
) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b.iter()).all(|((an, av), (bn, bv))| {
        if an != bn {
            return false;
        }
        match (av, bv) {
            (EnumVariantShape::Unit, EnumVariantShape::Unit) => true,
            (EnumVariantShape::Tuple(fa), EnumVariantShape::Tuple(fb)) => fa.len() == fb.len(),
            (EnumVariantShape::Struct(fa), EnumVariantShape::Struct(fb)) => fa == fb,
            _ => false,
        }
    })
}

/// Seed a VM's type tables with the engine-wide builtin shapes
/// (`Result`, `RuntimeError`). Same source of truth as the walker
/// — both engines read `bop::builtins::builtin_*` so they can
/// never drift out of sync.
///
/// Returns:
/// - `struct_defs` keyed by `(module_path, type_name)`, with the
///   builtin entry under `<builtin>`;
/// - `enum_defs` keyed the same way;
/// - an outer-scope type_bindings map pointing each builtin's
///   bare name at `<builtin>` so user code resolves `Result::Ok`
///   without an explicit `use`.
fn seed_builtin_types() -> (
    BTreeMap<(String, String), Vec<String>>,
    BTreeMap<(String, String), Vec<(String, EnumVariantShape)>>,
    BTreeMap<String, String>,
) {
    use bop::parser::VariantKind;
    let builtin_mp = bop::value::BUILTIN_MODULE_PATH.to_string();
    let mut struct_defs: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    let mut enum_defs: BTreeMap<(String, String), Vec<(String, EnumVariantShape)>> = BTreeMap::new();
    let mut bindings: BTreeMap<String, String> = BTreeMap::new();
    struct_defs.insert(
        (builtin_mp.clone(), String::from("RuntimeError")),
        bop::builtins::builtin_runtime_error_fields(),
    );
    bindings.insert(String::from("RuntimeError"), builtin_mp.clone());
    let variants: Vec<(String, EnumVariantShape)> = bop::builtins::builtin_result_variants()
        .into_iter()
        .map(|v| {
            let shape = match v.kind {
                VariantKind::Unit => EnumVariantShape::Unit,
                VariantKind::Tuple(fs) => EnumVariantShape::Tuple(fs),
                VariantKind::Struct(fs) => EnumVariantShape::Struct(fs),
            };
            (v.name, shape)
        })
        .collect();
    enum_defs.insert((builtin_mp.clone(), String::from("Result")), variants);
    bindings.insert(String::from("Result"), builtin_mp);
    (struct_defs, enum_defs, bindings)
}

/// Cached result of loading a module. `Loading` is the
/// in-progress sentinel for circular-import detection; `Loaded`
/// carries every top-level export — bindings, struct/enum
/// decls, and methods — so the importer can rebuild the
/// module's visible surface without re-evaluating it.
#[allow(clippy::large_enum_variant)]
enum ImportSlot {
    Loading,
    Loaded(ModuleArtifacts),
}

#[derive(Clone)]
struct ModuleArtifacts {
    /// Top-level `let` bindings, reified as `Value`s. Fns live
    /// in `fn_decls` instead so the importer can register them
    /// in `self.functions` for cross-fn call resolution.
    bindings: Vec<(String, Value)>,
    /// Top-level `fn` declarations. The importer copies each
    /// into its own `self.functions` table AND pushes a
    /// `Value::Fn` into the current scope so first-class use
    /// (`let g = some_imported_fn`) still works.
    fn_decls: Vec<(String, Rc<FnEntry>)>,
    /// Struct-type declarations introduced by the module,
    /// keyed by their full identity `(module_path, type_name)`.
    struct_defs: Vec<((String, String), Vec<String>)>,
    /// Enum-type declarations introduced by the module, same
    /// keying as `struct_defs`.
    enum_defs: Vec<((String, String), Vec<(String, EnumVariantShape)>)>,
    /// `((module_path, type_name), method_name, FnEntry)` for
    /// every method the module declared on its own types.
    methods: Vec<((String, String), String, Rc<FnEntry>)>,
}

/// Import cache shared across nested VMs so recursive imports
/// resolve exactly once per top-level run.
type ImportCache = Rc<RefCell<BTreeMap<String, ImportSlot>>>;

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
    /// User-declared functions, keyed by name. Wrapped in `Rc`
    /// so the call path can take a cheap handle (one refcount
    /// bump) instead of cloning the params `Vec<String>` and
    /// re-taking the chunk `Rc` on every invocation — a hot
    /// path that ran ~500 000 times in `fib(28)`.
    functions: BTreeMap<String, Rc<FnEntry>>,
    host: &'h mut H,
    steps: u64,
    step_budget: u64,
    rand_state: u64,
    imports: ImportCache,
    imported_here: BTreeSet<String>,
    limits: BopLimits,
    /// Module this VM is running — tags newly declared types
    /// with their full identity so two modules declaring the
    /// same name remain distinct. `<root>` at the top level,
    /// the dot-joined module path inside a `use`'d module's
    /// sub-VM.
    current_module: String,
    /// Declared struct types, keyed by full identity
    /// `(module_path, type_name)`. Populated by
    /// `DefineStruct` (which tags with `current_module`) and
    /// merged from imported modules via `exec_use`.
    struct_defs: BTreeMap<(String, String), Vec<String>>,
    /// Declared enum types, same `(module_path, type_name)`
    /// keying as `struct_defs`.
    enum_defs: BTreeMap<(String, String), Vec<(String, EnumVariantShape)>>,
    /// User-defined methods. Outer key is the receiver type's
    /// full identity `(module_path, type_name)`; inner is the
    /// method name. A method declared for `paint.Color` doesn't
    /// fire on `other.Color` — identity is strict.
    user_methods: BTreeMap<(String, String), BTreeMap<String, Rc<FnEntry>>>,
    /// Parallel scope-stack for bare-name type and module-alias
    /// resolution. Pushed / popped in lockstep with frame scopes,
    /// plus a fresh frame at fn-call entry so a fn's own type
    /// decls are isolated from the caller's scope. `<builtin>`
    /// types seed scope 0.
    type_bindings: Vec<BTreeMap<String, String>>,
    /// Module-level aliases. Unlike value scope, these persist
    /// across function boundaries so namespaced references
    /// inside fn bodies (`p.Color::Red` in a pattern) can still
    /// resolve to the aliased module.
    module_aliases: BTreeMap<String, Rc<bop::value::BopModule>>,
    /// Freelist of cleared slot vecs from popped frames. Every
    /// fn call needs a fresh `Vec<Value>` sized to `slot_count`
    /// — allocating a new one per call was ~500k small heap
    /// allocations under `fib(28)`. On return we truncate + park;
    /// next call grabs one, resizes in place.
    slots_freelist: Vec<Vec<Value>>,
}

impl<'h, H: BopHost> Vm<'h, H> {
    pub fn new(chunk: Chunk, host: &'h mut H, limits: BopLimits) -> Self {
        bop::memory::bop_memory_init(limits.max_memory);
        Self::new_internal(
            chunk,
            host,
            limits,
            Rc::new(RefCell::new(BTreeMap::new())),
            String::from(bop::value::ROOT_MODULE_PATH),
        )
    }

    fn new_internal(
        chunk: Chunk,
        host: &'h mut H,
        limits: BopLimits,
        imports: ImportCache,
        current_module: String,
    ) -> Self {
        let top = Frame {
            chunk: Rc::new(chunk),
            ip: 0,
            slots: Vec::new(),
            scopes: vec![BTreeMap::new()],
            stack_base: 0,
            is_function: false,
            try_call_wrapper: false,
        };
        let step_budget = limits.max_steps.saturating_mul(STEP_SCALE);
        let (struct_defs, enum_defs, builtin_bindings) = seed_builtin_types();
        Self {
            frames: vec![top],
            stack: Vec::new(),
            functions: BTreeMap::new(),
            host,
            steps: 0,
            step_budget,
            rand_state: 0,
            imports,
            imported_here: BTreeSet::new(),
            limits,
            current_module,
            struct_defs,
            enum_defs,
            user_methods: BTreeMap::new(),
            type_bindings: vec![builtin_bindings],
            module_aliases: BTreeMap::new(),
            slots_freelist: Vec::new(),
        }
    }

    /// Grab a slot vec from the freelist or allocate a fresh one,
    /// then size it to `len` with `Value::None` placeholders.
    /// Callers write their args into the first few entries after
    /// calling this.
    fn take_slots(&mut self, len: usize) -> Vec<Value> {
        let mut v = self.slots_freelist.pop().unwrap_or_default();
        if v.capacity() < len {
            v.reserve(len - v.capacity());
        }
        v.resize(len, Value::None);
        v
    }

    /// Return a slot vec to the freelist after a frame pops. We
    /// clear (not deallocate) so the next call reuses the backing
    /// allocation. Capped so a pathological call graph doesn't
    /// grow unbounded memory.
    fn return_slots(&mut self, mut slots: Vec<Value>) {
        if self.slots_freelist.len() >= SLOTS_FREELIST_CAP {
            return;
        }
        slots.clear();
        self.slots_freelist.push(slots);
    }

    pub fn run(mut self) -> Result<(), BopError> {
        let mut last_line: u32 = 0;
        loop {
            let (instr, line) = match self.fetch() {
                Some(x) => x,
                None => break,
            };
            last_line = line;
            // Tick errors (resource-limit violations) are always
            // fatal, so `unwind_to_try_call` will short-circuit
            // them. The path still goes through the helper so
            // the two error paths behave identically.
            if let Err(err) = self.tick(line) {
                self.unwind_to_try_call(err)?;
                continue;
            }
            match self.dispatch(instr, line) {
                Ok(Next::Continue) => {}
                Ok(Next::Halt) => break,
                Err(err) => {
                    self.unwind_to_try_call(err)?;
                }
            }
        }
        // Programs that allocate past the memory limit and then
        // finish in fewer ticks than the periodic memory-check
        // cadence (see `TICK_MEMCHECK_MASK`) would otherwise slip
        // through silently. Catch them here — cheap, and a
        // program ending always runs this exactly once.
        if bop::memory::bop_memory_exceeded() {
            return Err(error_fatal_with_hint(
                last_line,
                "Memory limit exceeded",
                "Your code is using too much memory. Check for large strings or arrays growing in loops.",
            ));
        }
        Ok(())
    }

    // ─── Fetch / ip ──────────────────────────────────────────────

    #[inline]
    fn fetch(&mut self) -> Option<(Instr, u32)> {
        let frame = self.frames.last_mut()?;
        if frame.ip >= frame.chunk.code.len() {
            return None;
        }
        let instr = frame.chunk.code[frame.ip];
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

    #[inline]
    fn tick(&mut self, line: u32) -> Result<(), BopError> {
        self.steps += 1;
        if self.steps > self.step_budget {
            // Fatal — `try_call` can't catch this or the
            // step-limit sandbox invariant would break.
            return Err(error_fatal_with_hint(
                line,
                "Your code took too many steps (possible infinite loop)",
                "Check your loops — make sure they have a condition that eventually stops them.",
            ));
        }
        // Memory-limit check is a thread-local (or global Cell)
        // lookup — cheap in absolute terms, but done every
        // instruction it dominates a tight arithmetic loop.
        // Allocations enter through `Value::Clone` /
        // constructors, so we only need to notice when the
        // counter crosses the limit. Checking every 256 ticks
        // caps detection latency at ~256 instructions worth of
        // allocations — negligible for a hard cap that's already
        // elastic by a few MB.
        if self.steps & TICK_MEMCHECK_MASK == 0
            && bop::memory::bop_memory_exceeded()
        {
            return Err(error_fatal_with_hint(
                line,
                "Memory limit exceeded",
                "Your code is using too much memory. Check for large strings or arrays growing in loops.",
            ));
        }
        self.host.on_tick()?;
        Ok(())
    }

    // ─── Stack helpers ───────────────────────────────────────────

    #[inline]
    fn push_value(&mut self, v: Value) {
        self.stack.push(Slot::Value(v));
    }

    #[inline]
    fn pop_value(&mut self, line: u32) -> Result<Value, BopError> {
        match self.stack.pop() {
            Some(Slot::Value(v)) => Ok(v),
            Some(_) => Err(error(line, "VM: expected value on stack")),
            None => Err(error(line, "VM: stack underflow")),
        }
    }

    #[inline]
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
        // Type bindings parallel value scopes so inline type
        // decls inside a block vanish on block exit, same rule
        // the walker applies.
        self.type_bindings.push(BTreeMap::new());
    }

    fn pop_scope(&mut self) {
        let scopes = self.current_scopes_mut();
        if scopes.len() > 1 {
            scopes.pop();
            if self.type_bindings.len() > 1 {
                self.type_bindings.pop();
            }
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

    /// `lookup_var`, but pull the name from the current chunk by
    /// index. Avoids an `Rc::clone` + separate borrow at the call
    /// site — the dispatch loop's hottest read path.
    fn lookup_var_by_idx(&self, idx: NameIdx) -> Option<&Value> {
        let frame = self.frames.last()?;
        let name = frame.chunk.name(idx);
        for scope in frame.scopes.iter().rev() {
            if let Some(v) = scope.get(name) {
                return Some(v);
            }
        }
        None
    }

    fn set_existing(&mut self, name: &str, value: Value) -> bool {
        for scope in self.current_scopes_mut().iter_mut().rev() {
            // Writing through `get_mut` keeps the existing key
            // allocation in place — we'd otherwise pay a fresh
            // `String` alloc per loop iteration for every
            // `StoreVar` in a tight loop (e.g. `i = i + 1`).
            if let Some(slot) = scope.get_mut(name) {
                *slot = value;
                return true;
            }
        }
        false
    }

    /// `set_existing`, but pull the name from the current chunk
    /// by index. Splits the frame's `&mut` into a `&Rc<Chunk>`
    /// (for the name slice) and `&mut scopes` (for the walk)
    /// using field-level borrow splitting — no `Rc::clone`.
    fn set_existing_by_idx(&mut self, idx: NameIdx, value: Value) -> bool {
        let frame = self.frames.last_mut().expect("frame present");
        let name = frame.chunk.name(idx);
        for scope in frame.scopes.iter_mut().rev() {
            if let Some(slot) = scope.get_mut(name) {
                *slot = value;
                return true;
            }
        }
        false
    }

    // ─── "Did you mean?" candidate collectors ─────────────────
    //
    // Same pattern as the walker: gather every name the user
    // could plausibly have meant so `bop::suggest::did_you_mean`
    // picks the closest one. VM-specific quirk: the scope stack
    // is per-frame, so we only scan the current frame's scopes
    // rather than the whole interpreter's.

    /// Names reachable when an identifier is used as a value —
    /// locals in the enclosing scopes plus user fn declarations.
    fn value_candidates_hint(&self, target: &str) -> Option<String> {
        let mut candidates: Vec<String> = Vec::new();
        for scope in self.current_scopes() {
            for k in scope.keys() {
                candidates.push(k.clone());
            }
        }
        for name in self.functions.keys() {
            candidates.push(name.clone());
        }
        bop::suggest::did_you_mean(target, candidates)
    }

    /// Names reachable in a call position: user fns plus core
    /// builtins. Host builtins stay with the host's own
    /// `function_hint()` (the call site falls back to that path
    /// when no user-level suggestion fits).
    fn callable_candidates_hint(&self, target: &str) -> Option<String> {
        let mut candidates: Vec<String> =
            self.functions.keys().cloned().collect();
        for b in bop::suggest::CORE_CALLABLE_BUILTINS {
            candidates.push((*b).to_string());
        }
        bop::suggest::did_you_mean(target, candidates)
    }

    // ─── Dispatch ────────────────────────────────────────────────

    #[inline]
    fn dispatch(&mut self, instr: Instr, line: u32) -> Result<Next, BopError> {
        match instr {
            // ─── Literals ─────────────────────────────────────────
            Instr::LoadConst(idx) => {
                let value = match self.current_chunk().constant(idx) {
                    Constant::Int(n) => Value::Int(*n),
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
                // Fast path: look the name up by index so both
                // the chunk borrow and the scope walk happen in
                // one helper — no `Rc::clone` and no intermediate
                // owned `String`. A tight loop with 3 `LoadVar`s
                // per iteration previously paid one `Rc::clone`
                // per access here.
                if let Some(v) = self.lookup_var_by_idx(n).cloned() {
                    self.push_value(v);
                } else {
                    // Slow path: fall back to the named-fn
                    // registry so `fn fib(...) {...}; let g = fib`
                    // yields a real `Value::Fn`. Only then do we
                    // need the name as a `&str` / `String`.
                    let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
                    let name = chunk.name(n);
                    let fn_parts = self
                        .functions
                        .get(name)
                        .map(|entry| (entry.params.clone(), entry.chunk.clone()));
                    if let Some((params, chunk_rc)) = fn_parts {
                        let body: Rc<dyn core::any::Any + 'static> = chunk_rc;
                        let v = Value::new_compiled_fn(
                            params,
                            Vec::new(),
                            body,
                            Some(name.to_string()),
                        );
                        self.push_value(v);
                    } else {
                        // "did you mean?" first, else the generic
                        // "use `let`" nudge. Mirrors the walker's
                        // behaviour for consistency across engines.
                        let hint = self
                            .value_candidates_hint(name)
                            .unwrap_or_else(|| {
                                "Did you forget to create it with `let`?".to_string()
                            });
                        return Err(error_with_hint(
                            line,
                            bop::error_messages::variable_not_found(name),
                            hint,
                        ));
                    }
                }
            }
            Instr::DefineLocal(n) => {
                // `define_local` takes an owned `String` because
                // it inserts into the scope map — can't skip the
                // allocation here.
                let name = self.current_chunk().name(n).to_string();
                let v = self.pop_value(line)?;
                self.define_local(name, v);
            }
            Instr::StoreVar(n) => {
                // Fast path: look the target up by index so we
                // neither `Rc::clone` nor allocate the name.
                let v = self.pop_value(line)?;
                if !self.set_existing_by_idx(n, v) {
                    // Cold path: synthesise the error with the
                    // name — allocation is fine here.
                    let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
                    let name = chunk.name(n);
                    return Err(error_with_hint(
                        line,
                        format!("Variable `{}` doesn't exist yet", name),
                        format!("Use `let` to create a new variable: let {} = ...", name),
                    ));
                }
            }

            // ─── Slot-based locals (fast path) ───────────────────
            Instr::LoadLocal(slot) => {
                // Direct vec index — no hashing, no string compare.
                // Slots are pre-sized at call time from the
                // enclosing `FnDef::slot_count`, so an out-of-range
                // read can only happen on miscompiled bytecode.
                let frame = self.frames.last().expect("frame present");
                let v = frame
                    .slots
                    .get(slot.0 as usize)
                    .cloned()
                    .ok_or_else(|| {
                        error(line, "VM: local slot out of range")
                    })?;
                self.push_value(v);
            }
            Instr::StoreLocal(slot) => {
                let v = self.pop_value(line)?;
                let frame = self.frames.last_mut().expect("frame present");
                let i = slot.0 as usize;
                if i < frame.slots.len() {
                    frame.slots[i] = v;
                } else {
                    return Err(error(line, "VM: local slot out of range"));
                }
            }

            // ─── Superinstructions ───────────────────────────────
            Instr::AddLocals(a, b) => {
                // Typed fast path: Int + Int → Int, no Value
                // variant match and no stack push/pop for the
                // operands. `fib`'s recursive body and every
                // `total = total + i` loop go through here.
                let frame = self.frames.last().expect("frame present");
                let av = &frame.slots[a.0 as usize];
                let bv = &frame.slots[b.0 as usize];
                let result = match (av, bv) {
                    (Value::Int(x), Value::Int(y)) => match x.checked_add(*y) {
                        Some(v) => Value::Int(v),
                        None => return Err(error(line, "integer overflow in +")),
                    },
                    // Cold path: delegate to the generic Value
                    // adder. Covers Number, String concat, array
                    // concat, etc.
                    _ => ops::add(av, bv, line)?,
                };
                self.push_value(result);
            }
            Instr::LtLocals(a, b) => {
                let frame = self.frames.last().expect("frame present");
                let av = &frame.slots[a.0 as usize];
                let bv = &frame.slots[b.0 as usize];
                let result = match (av, bv) {
                    (Value::Int(x), Value::Int(y)) => Value::Bool(x < y),
                    _ => ops::lt(av, bv, line)?,
                };
                self.push_value(result);
            }
            Instr::IncLocalInt(slot, k) => {
                // `slot += k` for small-int `k`, the `i = i + 1`
                // idiom. Fast path: Int → Int with overflow
                // check. On non-Int, build an Int value and
                // dispatch through generic add so `x = x + 1`
                // still works when `x` is a Number.
                let frame = self.frames.last_mut().expect("frame present");
                let i = slot.0 as usize;
                let current = &frame.slots[i];
                let new = match current {
                    Value::Int(x) => match x.checked_add(k as i64) {
                        Some(v) => Value::Int(v),
                        None => return Err(error(line, "integer overflow in +")),
                    },
                    _ => ops::add(current, &Value::Int(k as i64), line)?,
                };
                frame.slots[i] = new;
            }
            Instr::LoadLocalAddInt(slot, k) => {
                // Push `slots[slot] + k`. Covers `fib(n - 1)` and
                // `array[i + 1]` after the compiler folds a
                // small-int literal into the op. `Sub` falls
                // into the same opcode at compile time by
                // negating the constant.
                let frame = self.frames.last().expect("frame present");
                let v = &frame.slots[slot.0 as usize];
                let result = match v {
                    Value::Int(x) => match x.checked_add(k as i64) {
                        Some(v) => Value::Int(v),
                        None => return Err(error(line, "integer overflow in +")),
                    },
                    _ => ops::add(v, &Value::Int(k as i64), line)?,
                };
                self.push_value(result);
            }
            Instr::LtLocalInt(slot, k) => {
                // Push `slots[slot] < k` — the `n < 2` base-case
                // test in `fib` and every bounded `while i < K`
                // loop.
                let frame = self.frames.last().expect("frame present");
                let v = &frame.slots[slot.0 as usize];
                let result = match v {
                    Value::Int(x) => Value::Bool(*x < k as i64),
                    _ => ops::lt(v, &Value::Int(k as i64), line)?,
                };
                self.push_value(result);
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
                return self.call_method(method, argc as usize, assign_back_to, line);
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
                            bop::error_messages::cant_iterate_over(other.type_name()),
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
                    Value::Int(n) => n,
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

            // ─── Modules ─────────────────────────────────────────
            Instr::Use(idx) => {
                let spec = self.current_chunk().use_spec(idx).clone();
                self.exec_use(&spec, line)?;
            }

            // ─── User-defined types ──────────────────────────────
            Instr::DefineStruct(idx) => self.define_struct(idx, line)?,
            Instr::DefineEnum(idx) => self.define_enum(idx, line)?,
            Instr::DefineMethod {
                type_name,
                method_name,
                fn_idx,
            } => self.define_method(type_name, method_name, fn_idx),
            Instr::ConstructStruct {
                namespace,
                type_name,
                count,
            } => {
                self.construct_struct(namespace, type_name, count as usize, line)?;
            }
            Instr::ConstructEnum {
                namespace,
                type_name,
                variant,
                shape,
            } => {
                self.construct_enum(namespace, type_name, variant, shape, line)?;
            }
            Instr::FieldGet(n) => {
                let field = self.current_chunk().name(n).to_string();
                let obj = self.pop_value(line)?;
                self.push_value(self.field_get(&obj, &field, line)?);
            }
            Instr::FieldSet(n) => {
                let field = self.current_chunk().name(n).to_string();
                let val = self.pop_value(line)?;
                let obj = self.pop_value(line)?;
                self.push_value(self.field_set(obj, &field, val, line)?);
            }

            // ─── Pattern matching ───────────────────────────────
            Instr::MatchFail { pattern, on_fail } => {
                self.match_fail(pattern, on_fail, line)?;
            }
            Instr::MatchExhausted => {
                return Err(error(line, "No match arm matched the scrutinee"));
            }

            // ─── try ─────────────────────────────────────────────
            Instr::TryUnwrap => {
                return self.try_unwrap(line);
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
                        error(line, bop::error_messages::variable_not_found(&name))
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
        // Borrow the name out of the chunk without allocating a
        // fresh `String` per call — `fib(25)` dispatches this
        // path ~75 000 times and even one small allocation per
        // call shows up in profiles. Holding a local `Rc<Chunk>`
        // keeps the name slice valid for the whole body.
        let chunk = Rc::clone(&self.frames.last().expect("frame present").chunk);
        let name: &str = chunk.name(name_idx);

        // Check lexical shadowing first (a `let f = fn() {...}`
        // must win over a same-named builtin / user fn). We peek
        // rather than clone so the common case — no shadow —
        // pays nothing.
        if self.lookup_var(name).is_some() {
            let args = self.pop_n_values(argc, line)?;
            let value = self
                .lookup_var(name)
                .expect("just checked via peek")
                .clone();
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

        // User-fn hot path: if `name` is a declared fn and no
        // local shadow exists, pop args straight into the new
        // frame's scope — no intermediate `Vec<Value>` allocation
        // per call. Callsite profile on `fib(28)` showed this
        // path taking ~90 % of all dispatched calls; eliminating
        // the per-call `Vec` cost takes ~500 000 small heap
        // allocations off the table.
        //
        // SAFETY-of-reorder: we've already ruled out lexical
        // shadowing above, so moving the user-fn check in front
        // of builtins / host matches the "let wins over fn; fn
        // wins over builtin" ordering the walker uses. (Walker:
        // locals checked at Call site, then call_function
        // matches builtins — a user-defined `fn print` never
        // reaches this path because `print` isn't in the
        // walker's `functions` table before `fn print` runs.
        // Same here: DefineFn populates `self.functions` only
        // for user-declared names, so builtins like `range`,
        // `print` don't collide.)
        if let Some(entry_rc) = self.functions.get(name) {
            let entry = Rc::clone(entry_rc);
            // Argc check before we touch the stack so error
            // messages match the walker's `name` wording.
            if argc != entry.params.len() {
                return Err(error(
                    line,
                    format!(
                        "`{}` expects {} argument{}, but got {}",
                        name,
                        entry.params.len(),
                        if entry.params.len() == 1 { "" } else { "s" },
                        argc
                    ),
                ));
            }
            drop(chunk);
            return self.enter_user_fn(entry, argc, line);
        }

        // Everything from here down needs the args as a
        // contiguous slice, so pay the `Vec` cost once.
        let args = self.pop_n_values(argc, line)?;

        // 1. Global builtins — the narrow set that can't be
        // expressed as methods on a receiver (variadic,
        // constructor-shape, session-stateful, or takes a
        // callable). Everything that used to live here is now
        // a method; see `methods::common_method` and
        // `methods::numeric_method`.
        match name {
            "range" => {
                let v = builtins::builtin_range(&args, line, &mut self.rand_state)?;
                self.push_value(v);
                return Ok(Next::Continue);
            }
            "rand" => {
                let v = builtins::builtin_rand(&args, line, &mut self.rand_state)?;
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
            "try_call" => return self.builtin_try_call(args, line),
            _ => {}
        }

        // 2. Host-provided builtins.
        if let Some(result) = self.host.call(name, &args, line) {
            let v = result?;
            self.push_value(v);
            return Ok(Next::Continue);
        }

        // Nothing left to try — the name is unresolved. The
        // user-fn fast path above already handled the success
        // case; we only reach here when no declared fn matches
        // either, so we can go straight to the "did you mean?"
        // / host-hint path.
        if let Some(hint) = self.callable_candidates_hint(name) {
            return Err(error_with_hint(
                line,
                bop::error_messages::function_not_found(name),
                hint,
            ));
        }
        let host_hint = self.host.function_hint().to_string();
        Err(if host_hint.is_empty() {
            error(line, bop::error_messages::function_not_found(name))
        } else {
            error_with_hint(
                line,
                bop::error_messages::function_not_found(name),
                host_hint,
            )
        })
    }

    /// Fast path for user-defined function calls: pop `argc`
    /// args straight off the value stack into the new frame's
    /// parameter scope, and push the frame. No intermediate
    /// `Vec<Value>`. Assumes the caller has already validated
    /// `argc == entry.params.len()` so the per-call error
    /// message can include the target name.
    fn enter_user_fn(
        &mut self,
        entry: Rc<FnEntry>,
        argc: usize,
        line: u32,
    ) -> Result<Next, BopError> {
        if self.frames.len() >= MAX_CALL_DEPTH {
            return Err(error_with_hint(
                line,
                "Too many nested function calls (possible infinite recursion)",
                "Check that your recursive function has a base case that stops calling itself.",
            ));
        }

        if self.stack.len() < argc {
            return Err(error(line, "VM: stack underflow"));
        }

        // Build the frame's flat slot array: args go into the
        // first `argc` slots (by compile-time-assigned order),
        // the rest of `slot_count` are pre-seeded with `None` so
        // later `StoreLocal`s land in-bounds. Backing allocation
        // comes from the freelist when possible — ~500k per-call
        // heap allocs under `fib(28)` fold into a steady-state
        // set of ~MAX_CALL_DEPTH reused vecs.
        let slot_count = (entry.chunk.slot_count as usize).max(argc);
        let mut slots = self.take_slots(slot_count);
        let start = self.stack.len() - argc;
        for i in 0..argc {
            let slot = core::mem::replace(
                &mut self.stack[start + i],
                Slot::Value(Value::None),
            );
            match slot {
                Slot::Value(v) => slots[i] = v,
                _ => {
                    self.stack.truncate(start);
                    self.return_slots(slots);
                    return Err(error(line, "VM: expected value on stack"));
                }
            }
        }
        self.stack.truncate(start);

        let frame = Frame {
            chunk: entry.chunk.clone(),
            ip: 0,
            slots,
            // Function frames don't use the BTreeMap scope stack
            // on the fast path — captures are on the `Value::Fn`
            // itself (handled in `call_closure`), and all locals
            // live in `slots`. An empty `scopes` vec is cheap to
            // allocate and keeps `LoadVar` / `DefineLocal` from
            // panicking if they still get emitted (they shouldn't
            // inside a fn body, but the fallback is safe).
            scopes: Vec::new(),
            stack_base: self.stack.len(),
            is_function: true,
            try_call_wrapper: false,
        };
        self.frames.push(frame);
        // A fresh type_bindings frame scopes any type decl
        // inside this fn to the fn body itself — same rule
        // as push_scope / pop_scope for block scoping. On
        // `do_return` we pop this frame.
        self.type_bindings.push(BTreeMap::new());
        Ok(Next::Continue)
    }

    /// Dispatch a value-based call: `argc` args sit on top, the
    /// callee sits directly under them. Pops all `argc + 1` slots,
    /// expects the callee to be a `Value::Fn`, and delegates to
    /// `call_closure`.
    fn call_value(&mut self, argc: usize, line: u32) -> Result<Next, BopError> {
        let args = self.pop_n_values(argc, line)?;
        let callee = self.pop_value(line)?;
        self.invoke_value(callee, args, line)
    }

    /// Invoke an already-evaluated callable with already-popped
    /// args. Shared between `Instr::CallValue` and the
    /// `Value::Module` fast path in `call_method` (where
    /// `m.foo(...)` resolves the fn through a module field
    /// lookup rather than going through the stack callee slot).
    fn invoke_value(
        &mut self,
        callee: Value,
        args: Vec<Value>,
        line: u32,
    ) -> Result<Next, BopError> {
        match &callee {
            Value::Fn(f) => {
                let f = Rc::clone(f);
                drop(callee);
                self.call_closure(&f, args, line)
            }
            other => Err(error(
                line,
                bop::error_messages::cant_call_a(other.type_name()),
            )),
        }
    }

    fn call_method(
        &mut self,
        method_idx: NameIdx,
        argc: usize,
        assign_back_to: Option<crate::chunk::AssignBack>,
        line: u32,
    ) -> Result<Next, BopError> {
        let method = self.current_chunk().name(method_idx).to_string();

        let args = self.pop_n_values(argc, line)?;
        let obj = self.pop_value(line)?;

        // `m.foo(args)` on a module alias: there's no struct /
        // enum receiver — `m` is a `Value::Module` whose `foo`
        // export is a callable. Look it up and dispatch through
        // the regular call-by-value path.
        //
        // Before falling through to the binding lookup, check
        // whether the method name is one of the common methods
        // (`type`, `to_str`, `inspect`). Those work on every
        // value, including `Value::Module`, and shouldn't be
        // gated by whether the module happens to export that
        // name.
        if let Value::Module(ref m) = obj {
            if let Some(result) = methods::common_method(&obj, &method, &args, line)? {
                self.push_value(result.0);
                return Ok(Next::Continue);
            }
            if let Some((_, v)) = m.bindings.iter().find(|(k, _)| k == &method) {
                let callee = v.clone();
                drop(obj);
                return self.invoke_value(callee, args, line);
            }
            return Err(error(
                line,
                format!("`{}` isn't exported from `{}`", method, m.path),
            ));
        }

        // User-method dispatch comes first — any method declared
        // on the receiver's full type identity wins over the
        // built-in method of the same name, matching the walker.
        // Dispatch is strict: `fn paint.Color.shade(self)` never
        // fires on `other.Color` even though the bare name is
        // the same.
        let user_type_key: Option<(String, String)> = match &obj {
            Value::Struct(s) => Some((
                s.module_path().to_string(),
                s.type_name().to_string(),
            )),
            Value::EnumVariant(e) => Some((
                e.module_path().to_string(),
                e.type_name().to_string(),
            )),
            _ => None,
        };
        if let Some(type_key) = user_type_key {
            let entry = self
                .user_methods
                .get(&type_key)
                .and_then(|m| m.get(&method))
                .cloned();
            if let Some(entry) = entry {
                if entry.params.len() != args.len() + 1 {
                    return Err(error(
                        line,
                        format!(
                            "`{}.{}` expects {} argument{} (including `self`), but got {}",
                            type_key.1,
                            method,
                            entry.params.len(),
                            if entry.params.len() == 1 { "" } else { "s" },
                            args.len() + 1
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
                // User methods use the same slot-based frame
                // layout as regular fn calls. `self` takes slot 0,
                // remaining params take 1..entry.params.len().
                let slot_count = (entry.chunk.slot_count as usize)
                    .max(entry.params.len());
                let mut slots = self.take_slots(slot_count);
                slots[0] = obj;
                for (i, a) in args.into_iter().enumerate() {
                    slots[i + 1] = a;
                }
                self.frames.push(Frame {
                    chunk: entry.chunk.clone(),
                    ip: 0,
                    slots,
                    scopes: Vec::new(),
                    stack_base: self.stack.len(),
                    is_function: true,
                    try_call_wrapper: false,
                });
                self.type_bindings.push(BTreeMap::new());
                // User methods don't do mutation back-assign
                // — the receiver is passed by value, and the
                // method returns a fresh instance if it wants to
                // "mutate". Matches the walker's convention.
                let _ = assign_back_to;
                return Ok(Next::Continue);
            }
        }

        // `type` / `to_str` / `inspect` work on every value —
        // dispatch them ahead of the type-specific tables so
        // walker / VM / AOT agree on the common method surface.
        let (ret, mutated) = if let Some(result) =
            methods::common_method(&obj, &method, &args, line)?
        {
            result
        } else {
            match &obj {
                Value::Array(arr) => {
                    methods::array_method(arr, &method, &args, line)?
                }
                Value::Str(s) => {
                    methods::string_method(s.as_str(), &method, &args, line)?
                }
                Value::Dict(entries) => {
                    methods::dict_method(entries, &method, &args, line)?
                }
                Value::Int(_) | Value::Number(_) => {
                    methods::numeric_method(&obj, &method, &args, line)?
                }
                Value::Bool(_) => {
                    methods::bool_method(&obj, &method, &args, line)?
                }
                _ => {
                    return Err(error(
                        line,
                        bop::error_messages::no_such_method(obj.type_name(), &method),
                    ));
                }
            }
        };

        if methods::is_mutating_method(&method) {
            if let (Some(target), Some(new_obj)) = (assign_back_to, mutated) {
                match target {
                    crate::chunk::AssignBack::Slot(slot) => {
                        let frame = self.frames.last_mut().expect("frame present");
                        let i = slot.0 as usize;
                        if i < frame.slots.len() {
                            frame.slots[i] = new_obj;
                        }
                        // Out-of-range slot: silently drop the
                        // mutation. The compiler only emits
                        // `Slot(i)` for a slot it knows exists,
                        // so this branch is effectively a
                        // miscompile guard.
                    }
                    crate::chunk::AssignBack::Name(var_idx) => {
                        let var_name = self.current_chunk().name(var_idx).to_string();
                        self.set_existing(&var_name, new_obj);
                    }
                }
            }
        }
        self.push_value(ret);
        Ok(Next::Continue)
    }

    fn define_struct(&mut self, idx: StructIdx, line: u32) -> Result<(), BopError> {
        let def = self.current_chunk().struct_def(idx).clone();
        // Type identity is `(current_module, def.name)`. Two
        // different modules declaring the same name coexist at
        // distinct registry keys; same-module redecl is a no-op
        // on matching shape and an error otherwise.
        let key = (self.current_module.clone(), def.name.clone());
        if let Some(existing) = self.struct_defs.get(&key) {
            if existing == &def.fields {
                self.bind_local_type(&def.name);
                return Ok(());
            }
            return Err(error(
                line,
                format!("Struct `{}` is already declared", def.name),
            ));
        }
        self.struct_defs.insert(key, def.fields);
        self.bind_local_type(&def.name);
        Ok(())
    }

    fn define_enum(&mut self, idx: EnumIdx, line: u32) -> Result<(), BopError> {
        let def = self.current_chunk().enum_def(idx).clone();
        let variants: Vec<(String, EnumVariantShape)> = def
            .variants
            .into_iter()
            .map(|v| (v.name, v.shape))
            .collect();
        let key = (self.current_module.clone(), def.name.clone());
        if let Some(existing) = self.enum_defs.get(&key) {
            if shapes_match(existing, &variants) {
                self.bind_local_type(&def.name);
                return Ok(());
            }
            return Err(error(
                line,
                format!("Enum `{}` is already declared", def.name),
            ));
        }
        self.enum_defs.insert(key, variants);
        self.bind_local_type(&def.name);
        Ok(())
    }

    /// Bind a declared type's bare name in the current
    /// `type_bindings` frame so subsequent references resolve
    /// to *this* module's version.
    fn bind_local_type(&mut self, name: &str) {
        if let Some(scope) = self.type_bindings.last_mut() {
            scope.insert(name.to_string(), self.current_module.clone());
        }
    }

    fn define_method(
        &mut self,
        type_name: NameIdx,
        method_name: NameIdx,
        fn_idx: FnIdx,
    ) {
        let type_name_s = self.current_chunk().name(type_name).to_string();
        let method_name_s = self.current_chunk().name(method_name).to_string();
        let fn_def = self.current_chunk().function(fn_idx).clone();
        let entry = Rc::new(FnEntry {
            params: fn_def.params,
            chunk: Rc::new(fn_def.chunk),
        });
        // Methods attach to the *full* receiver-type identity.
        // A method declared inside `paint` for `Color` only
        // fires on `paint.Color` values, not on same-named
        // types from other modules.
        let key = (self.current_module.clone(), type_name_s);
        self.user_methods
            .entry(key)
            .or_default()
            .insert(method_name_s, entry);
    }

    fn construct_struct(
        &mut self,
        namespace: Option<NameIdx>,
        type_name: NameIdx,
        count: usize,
        line: u32,
    ) -> Result<(), BopError> {
        let type_name_s = self.current_chunk().name(type_name).to_string();
        // Resolve the source-level reference to its full type
        // identity before validating the shape. `namespace`
        // means `ns.Type { ... }`; bare means the scope walker
        // looks up `Type` in `type_bindings`.
        let module_path = match namespace {
            Some(ns_idx) => {
                let ns_name = self.current_chunk().name(ns_idx).to_string();
                self.validate_namespaced_type(&ns_name, &type_name_s, line)?;
                self.resolve_type_ref(Some(&ns_name), &type_name_s)
                    .unwrap_or_else(|| self.current_module.clone())
            }
            None => self
                .resolve_type_ref(None, &type_name_s)
                .ok_or_else(|| {
                    error(
                        line,
                        bop::error_messages::struct_not_declared(&type_name_s),
                    )
                })?,
        };
        let key = (module_path.clone(), type_name_s.clone());
        let decl = self
            .struct_defs
            .get(&key)
            .ok_or_else(|| {
                error(
                    line,
                    bop::error_messages::struct_not_declared(&type_name_s),
                )
            })?
            .clone();
        let flat = self.pop_n_values(count * 2, line)?;
        let mut provided: BTreeMap<String, Value> = BTreeMap::new();
        let mut iter = flat.into_iter();
        while let (Some(key), Some(val)) = (iter.next(), iter.next()) {
            let key_str = match &key {
                Value::Str(s) => s.as_str().to_string(),
                other => {
                    return Err(error(
                        line,
                        format!(
                            "Struct field names must be strings, got {}",
                            other.type_name()
                        ),
                    ));
                }
            };
            drop(key);
            if provided.contains_key(&key_str) {
                return Err(error(
                    line,
                    format!(
                        "Field `{}` specified twice in `{}` construction",
                        key_str, type_name_s
                    ),
                ));
            }
            if !decl.iter().any(|d| d == &key_str) {
                let msg = bop::error_messages::struct_has_no_field(
                    &type_name_s,
                    &key_str,
                );
                let err = match bop::suggest::did_you_mean(
                    &key_str,
                    decl.iter().map(|s| s.as_str()),
                ) {
                    Some(hint) => error_with_hint(line, msg, hint),
                    None => error(line, msg),
                };
                return Err(err);
            }
            provided.insert(key_str, val);
        }
        let mut fields: Vec<(String, Value)> = Vec::with_capacity(decl.len());
        for d in &decl {
            match provided.remove(d) {
                Some(v) => fields.push((d.clone(), v)),
                None => {
                    return Err(error(
                        line,
                        format!(
                            "Missing field `{}` in `{}` construction",
                            d, type_name_s
                        ),
                    ));
                }
            }
        }
        self.push_value(Value::new_struct(module_path, type_name_s, fields));
        Ok(())
    }

    fn construct_enum(
        &mut self,
        namespace: Option<NameIdx>,
        type_name: NameIdx,
        variant: NameIdx,
        shape: EnumConstructShape,
        line: u32,
    ) -> Result<(), BopError> {
        let type_name_s = self.current_chunk().name(type_name).to_string();
        let variant_s = self.current_chunk().name(variant).to_string();
        let module_path = match namespace {
            Some(ns_idx) => {
                let ns_name = self.current_chunk().name(ns_idx).to_string();
                self.validate_namespaced_type(&ns_name, &type_name_s, line)?;
                self.resolve_type_ref(Some(&ns_name), &type_name_s)
                    .unwrap_or_else(|| self.current_module.clone())
            }
            None => self
                .resolve_type_ref(None, &type_name_s)
                .ok_or_else(|| {
                    error(line, bop::error_messages::enum_not_declared(&type_name_s))
                })?,
        };
        let key = (module_path.clone(), type_name_s.clone());
        let decl = self
            .enum_defs
            .get(&key)
            .ok_or_else(|| {
                error(line, bop::error_messages::enum_not_declared(&type_name_s))
            })?
            .clone();
        let variant_decl = decl
            .iter()
            .find(|(n, _)| n == &variant_s)
            .cloned()
            .ok_or_else(|| {
                let msg = bop::error_messages::enum_has_no_variant(
                    &type_name_s,
                    &variant_s,
                );
                match bop::suggest::did_you_mean(
                    &variant_s,
                    decl.iter().map(|(n, _)| n.as_str()),
                ) {
                    Some(hint) => error_with_hint(line, msg, hint),
                    None => error(line, msg),
                }
            })?;
        match (&variant_decl.1, shape) {
            (EnumVariantShape::Unit, EnumConstructShape::Unit) => {
                self.push_value(Value::new_enum_unit(module_path, type_name_s, variant_s));
            }
            (EnumVariantShape::Tuple(fields), EnumConstructShape::Tuple(argc)) => {
                if fields.len() as u32 != argc {
                    return Err(error(
                        line,
                        format!(
                            "`{}::{}` expects {} argument{}, but got {}",
                            type_name_s,
                            variant_s,
                            fields.len(),
                            if fields.len() == 1 { "" } else { "s" },
                            argc
                        ),
                    ));
                }
                let items = self.pop_n_values(argc as usize, line)?;
                self.push_value(Value::new_enum_tuple(module_path, type_name_s, variant_s, items));
            }
            (EnumVariantShape::Struct(decl_fields), EnumConstructShape::Struct(count)) => {
                let flat = self.pop_n_values(count as usize * 2, line)?;
                let mut provided: BTreeMap<String, Value> = BTreeMap::new();
                let mut iter = flat.into_iter();
                while let (Some(key), Some(val)) = (iter.next(), iter.next()) {
                    let key_str = match &key {
                        Value::Str(s) => s.as_str().to_string(),
                        _ => {
                            return Err(error(
                                line,
                                "Enum struct-variant field names must be strings",
                            ));
                        }
                    };
                    drop(key);
                    if provided.contains_key(&key_str) {
                        return Err(error(
                            line,
                            format!(
                                "Field `{}` specified twice in `{}::{}`",
                                key_str, type_name_s, variant_s
                            ),
                        ));
                    }
                    if !decl_fields.iter().any(|d| d == &key_str) {
                        return Err(error(
                            line,
                            bop::error_messages::variant_has_no_field(
                                &type_name_s,
                                &variant_s,
                                &key_str,
                            ),
                        ));
                    }
                    provided.insert(key_str, val);
                }
                let mut fields: Vec<(String, Value)> =
                    Vec::with_capacity(decl_fields.len());
                for d in decl_fields {
                    match provided.remove(d) {
                        Some(v) => fields.push((d.clone(), v)),
                        None => {
                            return Err(error(
                                line,
                                format!(
                                    "Missing field `{}` in `{}::{}` construction",
                                    d, type_name_s, variant_s
                                ),
                            ));
                        }
                    }
                }
                self.push_value(Value::new_enum_struct(module_path, type_name_s, variant_s, fields));
            }
            (EnumVariantShape::Unit, _) => {
                return Err(error(
                    line,
                    format!("Variant `{}::{}` takes no payload", type_name_s, variant_s),
                ));
            }
            (EnumVariantShape::Tuple(_), _) => {
                return Err(error(
                    line,
                    format!(
                        "Variant `{}::{}` expects positional arguments `(…)`",
                        type_name_s, variant_s
                    ),
                ));
            }
            (EnumVariantShape::Struct(_), _) => {
                return Err(error(
                    line,
                    format!(
                        "Variant `{}::{}` expects named fields `{{ … }}`",
                        type_name_s, variant_s
                    ),
                ));
            }
        }
        Ok(())
    }

    fn field_get(&self, obj: &Value, field: &str, line: u32) -> Result<Value, BopError> {
        match obj {
            Value::Struct(s) => s.field(field).cloned().ok_or_else(|| {
                let msg = bop::error_messages::struct_has_no_field(
                    s.type_name(),
                    field,
                );
                let names = s.fields().iter().map(|(k, _)| k.as_str());
                match bop::suggest::did_you_mean(field, names) {
                    Some(hint) => error_with_hint(line, msg, hint),
                    None => error(line, msg),
                }
            }),
            Value::EnumVariant(e) => e.field(field).cloned().ok_or_else(|| {
                error(
                    line,
                    bop::error_messages::variant_has_no_field(
                        e.type_name(),
                        e.variant(),
                        field,
                    ),
                )
            }),
            Value::Module(m) => {
                if let Some((_, v)) = m.bindings.iter().find(|(k, _)| k == field) {
                    return Ok(v.clone());
                }
                if m.types.iter().any(|t| t == field) {
                    return Err(error(
                        line,
                        format!("`{}` in `{}` is a type, not a value", field, m.path),
                    ));
                }
                Err(error(
                    line,
                    format!("`{}` isn't exported from `{}`", field, m.path),
                ))
            }
            other => Err(error(
                line,
                bop::error_messages::cant_read_field(field, other.type_name()),
            )),
        }
    }

    fn field_set(
        &self,
        mut obj: Value,
        field: &str,
        value: Value,
        line: u32,
    ) -> Result<Value, BopError> {
        // Mutate in place — `Value::Struct` wraps a `Box` but
        // we already own `obj`, so `set_field` on the inner
        // `BopStruct` does the update and we hand the same
        // `Value` back.
        match &mut obj {
            Value::Struct(boxed) => {
                let type_name = boxed.type_name().to_string();
                if !boxed.set_field(field, value) {
                    return Err(error(
                        line,
                        bop::error_messages::struct_has_no_field(&type_name, field),
                    ));
                }
                Ok(obj)
            }
            other => Err(error(
                line,
                bop::error_messages::cant_assign_field(field, other.type_name()),
            )),
        }
    }

    /// Handle a `MatchFail` instruction: pop the scrutinee and
    /// attempt to match it against the pattern at `pattern`. On
    /// success, install the captured bindings into the current
    /// scope and fall through. On failure, jump to `on_fail`.
    ///
    /// Delegates to `bop::pattern_matches` so the VM behaves
    /// exactly like the tree-walker on every pattern shape.
    fn match_fail(
        &mut self,
        pattern: PatternIdx,
        on_fail: CodeOffset,
        line: u32,
    ) -> Result<(), BopError> {
        let value = self.pop_value(line)?;
        // `pattern` refers to a slot in the *currently executing*
        // chunk's pattern pool; we clone it out rather than hold
        // a borrow so we can mutate `self` afterwards to install
        // bindings.
        let pat = self.current_chunk().pattern(pattern).clone();
        let mut bindings: Vec<(String, Value)> = Vec::new();
        // Build a resolver snapshot from the current frame's
        // value scopes plus the VM-level type_bindings and
        // alias map. Patterns referring to `Color::Red` or
        // `m.Color::Red` thread through this so the matcher can
        // compare the value's full identity.
        let frame = self.frames.last().expect("frame present");
        let frame_scopes = &frame.scopes;
        let type_bindings = &self.type_bindings;
        let module_aliases = &self.module_aliases;
        let resolver = |ns: Option<&str>, tn: &str| -> Option<String> {
            bop::resolve_type_in(frame_scopes, type_bindings, module_aliases, ns, tn)
        };
        let matched = bop::pattern_matches(&pat, &value, &mut bindings, &resolver);
        if matched {
            for (name, v) in bindings {
                self.define_local(name, v);
            }
        } else {
            self.jump(on_fail);
        }
        Ok(())
    }

    /// Implement `try`: pop the top value and inspect the
    /// `Ok` / `Err` shape.
    ///
    /// - `Ok(v)` (single tuple payload) / `Ok` (unit) → push the
    ///   unwrapped value (`v` or `Value::None`) and continue.
    /// - `Err(...)` → act like `Return` from the current frame,
    ///   carrying the whole `Err` variant as the returned value.
    ///   If the current frame is the top-level program, raise a
    ///   runtime error instead (there's no fn to return from).
    /// - Anything else → runtime error.
    ///
    /// Mirrors the walker's `eval_try` so all three engines agree
    /// on the same shape recognition rules.
    fn try_unwrap(&mut self, line: u32) -> Result<Next, BopError> {
        let value = self.pop_value(line)?;
        match &value {
            Value::EnumVariant(ev) if ev.variant() == "Ok" => {
                use bop::value::EnumPayload;
                let payload = match ev.payload() {
                    EnumPayload::Tuple(items) if items.len() == 1 => items[0].clone(),
                    EnumPayload::Unit => Value::None,
                    EnumPayload::Tuple(items) => {
                        return Err(error(
                            line,
                            format!(
                                "try: Ok variant must carry exactly one value, got {}",
                                items.len()
                            ),
                        ));
                    }
                    EnumPayload::Struct(_) => {
                        return Err(error(
                            line,
                            "try: Ok variant must carry a single positional value, not named fields",
                        ));
                    }
                };
                self.push_value(payload);
                Ok(Next::Continue)
            }
            Value::EnumVariant(ev) if ev.variant() == "Err" => {
                let current_is_fn = self
                    .frames
                    .last()
                    .map(|f| f.is_function)
                    .unwrap_or(false);
                if !current_is_fn {
                    return Err(error_with_hint(
                        line,
                        "try encountered Err at top-level",
                        "Wrap the calling code in a fn, or use `match` to handle both arms explicitly.",
                    ));
                }
                // Fast-return with the Err value: identical path
                // to an ordinary `return err`.
                self.do_return(value)
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

    fn define_fn(&mut self, idx: FnIdx) {
        let fn_def = self.current_chunk().function(idx).clone();
        let entry = Rc::new(FnEntry {
            params: fn_def.params,
            chunk: Rc::new(fn_def.chunk),
        });
        self.functions.insert(fn_def.name, entry);
    }

    /// Materialise a lambda expression as a `Value::Fn`. Each
    /// `CaptureSource` in the lambda's `FnDef` tells us exactly
    /// where to read the captured value from the enclosing frame
    /// — no "flatten every binding in sight" pass, and no
    /// over-capture of out-of-scope slots.
    fn make_lambda(&mut self, idx: FnIdx) {
        let fn_def = self.current_chunk().function(idx).clone();
        let captures = self.snapshot_captures_for(&fn_def);
        let body: Rc<dyn core::any::Any + 'static> = Rc::new(fn_def.chunk);
        let value = Value::new_compiled_fn(fn_def.params, captures, body, None);
        self.push_value(value);
    }

    /// Package the captures for a lambda according to its
    /// compile-time `capture_sources`. `ParentSlot(n)` reads slot
    /// `n` from the enclosing frame; `ParentScope(name)` walks
    /// the enclosing frame's BTreeMap scope stack to find the
    /// binding.
    ///
    /// A `ParentScope` miss doesn't automatically become
    /// `Value::None`: if the name happens to be a globally-
    /// reachable user fn / host / builtin, we *skip* the capture
    /// entirely so the lambda body's `Call { name }` / `LoadVar`
    /// dispatches fall through to the fn registry at call time.
    /// Otherwise a shadowing `None` would turn `fn() { risky(5)
    /// }` into "`risky` is a none, not a function" — the bug this
    /// filter fixes.
    fn snapshot_captures_for(&self, fn_def: &FnDef) -> Vec<(String, Value)> {
        let frame = self.frames.last().expect("frame present");
        let mut out = Vec::with_capacity(fn_def.capture_names.len());
        for (name, source) in fn_def
            .capture_names
            .iter()
            .zip(fn_def.capture_sources.iter())
        {
            match source {
                CaptureSource::ParentSlot(slot) => {
                    let v = frame
                        .slots
                        .get(slot.0 as usize)
                        .cloned()
                        .unwrap_or(Value::None);
                    out.push((name.clone(), v));
                }
                CaptureSource::ParentScope(look_name) => {
                    let mut found = None;
                    for scope in frame.scopes.iter().rev() {
                        if let Some(v) = scope.get(look_name.as_str()) {
                            found = Some(v.clone());
                            break;
                        }
                    }
                    if let Some(v) = found {
                        out.push((name.clone(), v));
                    } else if self.is_globally_reachable_name(look_name) {
                        // Skip — the lambda body's dynamic
                        // dispatch will find it at call time.
                    } else {
                        out.push((name.clone(), Value::None));
                    }
                }
            };
        }
        out
    }

    /// A name reachable through the VM's non-scope registries at
    /// call time: declared user fns, core callable builtins, and
    /// host-provided fns. Used by capture snapshotting to avoid
    /// shadowing globals with a `None` when the defining frame
    /// doesn't itself have the binding.
    fn is_globally_reachable_name(&self, name: &str) -> bool {
        if self.functions.contains_key(name) {
            return true;
        }
        if bop::suggest::CORE_CALLABLE_BUILTINS.contains(&name) {
            return true;
        }
        // Host fn probe: there's no registry API, so we infer by
        // calling with zero args and seeing if it's handled. That
        // would mutate host state on some hosts, so keep it as
        // an overly-conservative fallback only for names that
        // look like host fns by convention. For now, return false
        // — if a host fn is being captured, the user can
        // declare a local shim.
        false
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

        // Lambda params + locals go into the flat `slots` array
        // (same fast path as named fns). Captures and the
        // `self_name` self-reference stay on the BTreeMap scope
        // stack — they're looked up by name at the compile-time
        // fallback path (`LoadVar`) because the compiler can't
        // resolve them to slots from inside the lambda body.
        let slot_count = (chunk.slot_count as usize).max(func.params.len());
        let mut slots = self.take_slots(slot_count);
        for (i, arg) in args.into_iter().enumerate() {
            slots[i] = arg;
        }

        let mut scope = BTreeMap::new();
        for (name, value) in &func.captures {
            scope.insert(name.clone(), value.clone());
        }
        if let Some(self_name) = &func.self_name {
            scope.insert(self_name.clone(), Value::Fn(Rc::clone(func)));
        }

        self.frames.push(Frame {
            chunk,
            ip: 0,
            slots,
            scopes: vec![scope],
            stack_base: self.stack.len(),
            is_function: true,
            try_call_wrapper: false,
        });
        self.type_bindings.push(BTreeMap::new());
        Ok(Next::Continue)
    }

    /// Execute a `use` statement. Dispatches the four shapes
    /// (glob / selective / aliased / selective + aliased) against
    /// the module's exported bindings. Types register globally
    /// on every form; the distinction is in how the caller sees
    /// the module's *values and fns* (flat in their scope vs.
    /// behind a namespace binding).
    fn exec_use(&mut self, spec: &crate::chunk::UseSpec, line: u32) -> Result<(), BopError> {
        let path = spec.path.as_str();
        let items = spec.items.as_deref();
        let alias = spec.alias.as_deref();

        let is_plain_glob = items.is_none() && alias.is_none();
        if is_plain_glob && self.imported_here.contains(path) {
            return Ok(());
        }
        let artifacts = self.load_module(path, line)?;

        // Types always register under their *full identity*
        // `(module_path, type_name)`. Two modules declaring the
        // same name now coexist at distinct keys; same-key
        // reinsertion is idempotent on matching shape and a
        // hard error otherwise (means the module was re-loaded
        // with different source).
        for (key, fields) in &artifacts.struct_defs {
            if let Some(existing) = self.struct_defs.get(key) {
                if existing == fields {
                    continue;
                }
                return Err(error(
                    line,
                    format!(
                        "Type `{}` from `{}` reloaded with different fields",
                        key.1, key.0
                    ),
                ));
            }
            self.struct_defs.insert(key.clone(), fields.clone());
        }
        for (key, variants) in &artifacts.enum_defs {
            if let Some(existing) = self.enum_defs.get(key) {
                if shapes_match(existing, variants) {
                    continue;
                }
                return Err(error(
                    line,
                    format!(
                        "Type `{}` from `{}` reloaded with different variants",
                        key.1, key.0
                    ),
                ));
            }
            self.enum_defs.insert(key.clone(), variants.clone());
        }
        for (type_key, method_name, entry) in &artifacts.methods {
            self.user_methods
                .entry(type_key.clone())
                .or_default()
                .insert(method_name.clone(), entry.clone());
        }

        // Gather the module's value-level exports as a single
        // name-keyed list. Fn declarations produce both a
        // scope-visible `Value::Fn` and a `self.functions` entry;
        // plain let bindings contribute just the value.
        let mut exports: Vec<(String, Value)> =
            Vec::with_capacity(artifacts.fn_decls.len() + artifacts.bindings.len());
        let mut fn_entries: Vec<(String, Rc<FnEntry>)> =
            Vec::with_capacity(artifacts.fn_decls.len());
        for (name, entry) in &artifacts.fn_decls {
            let chunk_rc: Rc<Chunk> = entry.chunk.clone();
            let body: Rc<dyn core::any::Any + 'static> = chunk_rc;
            let value = Value::new_compiled_fn(
                entry.params.clone(),
                Vec::new(),
                body,
                Some(name.clone()),
            );
            exports.push((name.clone(), value));
            fn_entries.push((name.clone(), entry.clone()));
        }
        for (name, value) in &artifacts.bindings {
            exports.push((name.clone(), value.clone()));
        }

        // Selective filter: ensure every listed name exists, then
        // retain only the listed exports.
        if let Some(list) = items {
            let available: std::collections::BTreeSet<&str> =
                exports.iter().map(|(k, _)| k.as_str()).collect();
            for wanted in list {
                if !available.contains(wanted.as_str())
                    && !artifacts
                        .struct_defs
                        .iter()
                        .any(|((_, n), _)| n == wanted)
                    && !artifacts
                        .enum_defs
                        .iter()
                        .any(|((_, n), _)| n == wanted)
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
            let listed: std::collections::BTreeSet<String> =
                list.iter().cloned().collect();
            exports.retain(|(k, _)| listed.contains(k));
            fn_entries.retain(|(k, _)| listed.contains(k));
        }

        // Decide which of the module's declared type names the
        // caller sees by bare name. Selective = exactly the
        // listed items that are types; glob = all public
        // (non-`_`-prefixed) types; aliased = none at bare name
        // (only reachable through the alias).
        let module_type_names: Vec<String> = artifacts
            .struct_defs
            .iter()
            .map(|((_, n), _)| n.clone())
            .chain(artifacts.enum_defs.iter().map(|((_, n), _)| n.clone()))
            .collect();
        let exposed_types: Vec<String> = match items {
            Some(list) => module_type_names
                .into_iter()
                .filter(|n| list.iter().any(|i| i == n))
                .collect(),
            None => module_type_names
                .into_iter()
                .filter(|n| !bop::naming::is_private(n))
                .collect(),
        };

        if let Some(alias_name) = alias {
            // Aliased form: pack the exports into a Value::Module
            // and bind it under the alias. The alias lives in the
            // current frame's top scope; `self.functions` still
            // gets the module's fn entries so sibling bare calls
            // inside module-owned code resolve.
            let frame_has = self
                .frames
                .last()
                .and_then(|f| f.scopes.last())
                .map(|s| s.contains_key(alias_name))
                .unwrap_or(false);
            if frame_has || self.functions.contains_key(alias_name) {
                return Err(error(
                    line,
                    format!(
                        "`{}` is already bound — can't use it as a module alias",
                        alias_name
                    ),
                ));
            }
            for (name, entry) in fn_entries {
                if !self.functions.contains_key(&name) {
                    self.functions.insert(name, entry);
                }
            }
            let module_rc = Rc::new(bop::value::BopModule {
                path: path.to_string(),
                bindings: exports,
                types: exposed_types,
            });
            // Bind the alias three ways:
            //   1. as Value::Module in the current value scope
            //      (immediate `m.helper(x)` at the use site);
            //   2. in `module_aliases` so it survives the
            //      fresh-scope reset at fn call boundaries;
            //   3. in `type_bindings` so patterns + construction
            //      resolve `m.Type` inside fn bodies.
            let module_value = Value::Module(Rc::clone(&module_rc));
            if let Some(frame) = self.frames.last_mut() {
                if let Some(scope) = frame.scopes.last_mut() {
                    scope.insert(alias_name.to_string(), module_value);
                }
            }
            self.module_aliases
                .insert(alias_name.to_string(), Rc::clone(&module_rc));
            if let Some(scope) = self.type_bindings.last_mut() {
                scope.insert(alias_name.to_string(), path.to_string());
            }
        } else {
            // Flat form (glob / selective). Glob skips
            // `_`-prefixed names (privacy); selective doesn't
            // (the user explicitly asked).
            let skip_private = items.is_none();
            for (name, entry) in fn_entries {
                if skip_private && bop::naming::is_private(&name) {
                    continue;
                }
                let clashes = self
                    .frames
                    .last()
                    .and_then(|f| f.scopes.last())
                    .map(|s| s.contains_key(&name))
                    .unwrap_or(false)
                    || self.functions.contains_key(&name);
                if clashes {
                    continue;
                }
                self.functions.insert(name, entry);
            }
            for (name, value) in exports {
                if skip_private && bop::naming::is_private(&name) {
                    continue;
                }
                let clashes = self
                    .frames
                    .last()
                    .and_then(|f| f.scopes.last())
                    .map(|s| s.contains_key(&name))
                    .unwrap_or(false);
                if clashes {
                    continue;
                }
                if let Some(frame) = self.frames.last_mut() {
                    if let Some(scope) = frame.scopes.last_mut() {
                        scope.insert(name, value);
                    }
                }
            }
            // Bring the module's exposed types in by bare name
            // too — `Color::Red` now resolves to *this*
            // module's Color. First-win on conflict matches the
            // value-binding rule.
            for tn in &exposed_types {
                let already_bound = self
                    .type_bindings
                    .last()
                    .map(|s| s.contains_key(tn))
                    .unwrap_or(false);
                if already_bound {
                    continue;
                }
                if let Some(scope) = self.type_bindings.last_mut() {
                    scope.insert(tn.clone(), path.to_string());
                }
            }
        }

        if is_plain_glob {
            self.imported_here.insert(path.to_string());
        }
        Ok(())
    }

    /// Validate `ns.Type` — confirm `ns` binds a `Value::Module`
    /// whose type exports include `type_name`. Used by
    /// namespaced struct-literal / variant-ctor dispatch.
    fn validate_namespaced_type(
        &self,
        ns: &str,
        type_name: &str,
        line: u32,
    ) -> Result<(), BopError> {
        // Prefer a local value-scope binding (catches
        // shadowing), but fall back to the VM-level module
        // alias map so namespaced references inside fn bodies
        // still resolve — the fn frame's value scopes don't
        // carry the caller's aliases.
        let frame = self.frames.last().ok_or_else(|| {
            error(line, "VM: no frame for namespaced type access")
        })?;
        for scope in frame.scopes.iter().rev() {
            if let Some(v) = scope.get(ns) {
                let module = match v {
                    Value::Module(m) => m,
                    other => {
                        return Err(error(
                            line,
                            format!(
                                "`{}` is a {}, not a module alias — can't reach `{}` through it",
                                ns,
                                other.type_name(),
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
                return Ok(());
            }
        }
        if let Some(module) = self.module_aliases.get(ns) {
            if !module.types.iter().any(|t| t == type_name) {
                return Err(error(
                    line,
                    format!(
                        "`{}` isn't a type exported from `{}`",
                        type_name, module.path
                    ),
                ));
            }
            return Ok(());
        }
        Err(error(
            line,
            format!("`{}` isn't a module alias in scope", ns),
        ))
    }

    /// Resolve a source-level type reference to its declaring
    /// module. Shared scope walker between construction,
    /// pattern matching, and namespace validation: keeps
    /// resolution rules centralised.
    fn resolve_type_ref(&self, namespace: Option<&str>, type_name: &str) -> Option<String> {
        if let Some(ns) = namespace {
            let frame = self.frames.last()?;
            for scope in frame.scopes.iter().rev() {
                if let Some(Value::Module(m)) = scope.get(ns) {
                    if m.types.iter().any(|t| t == type_name) {
                        return Some(m.path.clone());
                    }
                    return None;
                }
            }
            if let Some(m) = self.module_aliases.get(ns) {
                if m.types.iter().any(|t| t == type_name) {
                    return Some(m.path.clone());
                }
            }
            return None;
        }
        for scope in self.type_bindings.iter().rev() {
            if let Some(mp) = scope.get(type_name) {
                return Some(mp.clone());
            }
        }
        None
    }

    fn load_module(
        &mut self,
        path: &str,
        line: u32,
    ) -> Result<ModuleArtifacts, BopError> {
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

        self.imports
            .borrow_mut()
            .insert(path.to_string(), ImportSlot::Loading);

        let result = self.evaluate_module(path, &source);

        match result {
            Ok(bindings) => {
                self.imports
                    .borrow_mut()
                    .insert(path.to_string(), ImportSlot::Loaded(bindings.clone()));
                Ok(bindings)
            }
            Err(e) => {
                self.imports.borrow_mut().remove(path);
                Err(e)
            }
        }
    }

    /// Parse, compile, and execute a module in a nested VM that
    /// shares the import cache and limits. Returns the module's
    /// top-level `(name, Value)` bindings, with named fns reified
    /// as `Value::Fn` carrying VM-compiled chunks so the caller's
    /// `Call` / `CallValue` paths can dispatch them directly.
    fn evaluate_module(
        &mut self,
        module_path: &str,
        source: &str,
    ) -> Result<ModuleArtifacts, BopError> {
        let stmts = bop::parse(source)?;
        let chunk = crate::compile(&stmts)?;
        let imports = Rc::clone(&self.imports);
        let limits = self.limits.clone();
        let mut sub = Vm::new_internal(
            chunk,
            self.host,
            limits,
            imports,
            module_path.to_string(),
        );
        sub.run_internal()?;
        // Collect top-level lets from the module frame's one
        // remaining scope…
        let mut bindings: Vec<(String, Value)> = Vec::new();
        if let Some(frame) = sub.frames.first() {
            if let Some(scope) = frame.scopes.first() {
                for (k, v) in scope {
                    bindings.push((k.clone(), v.clone()));
                }
            }
        }
        // Named fn entries go out separately so the importer
        // can register them in `self.functions` for bare-ident
        // call resolution. Reified `Value::Fn`s for the same
        // fns are synthesised at the import site (see
        // `exec_import`).
        let fn_decls: Vec<(String, Rc<FnEntry>)> =
            sub.functions.into_iter().collect();
        // Type decls & methods come along for the ride — the
        // importer needs them so pattern-matching and
        // construction against the module's types works
        // without re-declaration. Engine builtins (<builtin>
        // module path) are already seeded in the importer, so
        // we filter them out to avoid duplicating the entries.
        let builtin_mp = bop::value::BUILTIN_MODULE_PATH;
        let struct_defs: Vec<((String, String), Vec<String>)> = sub
            .struct_defs
            .into_iter()
            .filter(|((mp, _), _)| mp != builtin_mp)
            .collect();
        let enum_defs: Vec<((String, String), Vec<(String, EnumVariantShape)>)> = sub
            .enum_defs
            .into_iter()
            .filter(|((mp, _), _)| mp != builtin_mp)
            .collect();
        let mut methods: Vec<((String, String), String, Rc<FnEntry>)> = Vec::new();
        for (type_key, by_method) in sub.user_methods {
            for (method_name, entry) in by_method {
                methods.push((type_key.clone(), method_name, entry));
            }
        }
        Ok(ModuleArtifacts {
            bindings,
            fn_decls,
            struct_defs,
            enum_defs,
            methods,
        })
    }

    /// Like `run` but keeps `self` around afterwards so the
    /// caller can inspect the module's final state. Used by
    /// `evaluate_module`.
    fn run_internal(&mut self) -> Result<(), BopError> {
        loop {
            let (instr, line) = match self.fetch() {
                Some(x) => x,
                None => break,
            };
            if let Err(err) = self.tick(line) {
                self.unwind_to_try_call(err)?;
                continue;
            }
            match self.dispatch(instr, line) {
                Ok(Next::Continue) => {}
                Ok(Next::Halt) => break,
                Err(err) => {
                    self.unwind_to_try_call(err)?;
                }
            }
        }
        Ok(())
    }

    fn do_return(&mut self, value: Value) -> Result<Next, BopError> {
        // Pop the current frame, truncate any frame-local stack
        // residue, and push the return value for the caller.
        let frame = self.frames.pop().expect("frame present");
        self.stack.truncate(frame.stack_base);
        // Drop the fn-local type_bindings frame so type decls
        // inside this fn don't leak into the caller. Only fns
        // push one; top-level frames don't, so the check keeps
        // the builtin frame on the stack.
        if frame.is_function && self.type_bindings.len() > 1 {
            self.type_bindings.pop();
        }
        // Recycle the slot vec so the next call can reuse its
        // allocation. Dropping it here would drop every `Value`
        // slot in place, which is still cheap for `Int`/`Bool`
        // but releases the vec's backing buffer — the exact
        // alloc/dealloc churn we're here to avoid.
        if !frame.slots.is_empty() {
            self.return_slots(frame.slots);
        }
        // A `try_call` wrapper frame wraps the return value in
        // `Result::Ok(v)` before handing it back to its caller.
        let final_value = if frame.try_call_wrapper {
            builtins::make_try_call_ok(value)
        } else {
            value
        };
        if frame.is_function {
            self.push_value(final_value);
            Ok(Next::Continue)
        } else {
            // Return at top level: behave like Halt (matches tree-walker,
            // which silently accepts Signal::Return at program scope).
            drop(final_value);
            Ok(Next::Halt)
        }
    }

    /// Implement the `try_call(f)` builtin. Validates the arg
    /// shape, dispatches to `call_closure` to push a frame for
    /// `f`, and flips that frame's `try_call_wrapper` flag so
    /// the outcome (whether a normal return or a non-fatal
    /// error) gets wrapped in a `Result::Ok`/`Result::Err`
    /// before returning to the original `try_call` caller.
    fn builtin_try_call(
        &mut self,
        args: Vec<Value>,
        line: u32,
    ) -> Result<Next, BopError> {
        if args.len() != 1 {
            return Err(error(
                line,
                format!(
                    "`try_call` expects 1 argument, but got {}",
                    args.len()
                ),
            ));
        }
        let mut iter = args.into_iter();
        let callable = iter.next().unwrap();
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
        self.call_closure(&func, Vec::new(), line)?;
        // The frame we just pushed is the one that should
        // participate in the try_call wrap/catch dance.
        if let Some(frame) = self.frames.last_mut() {
            frame.try_call_wrapper = true;
        }
        Ok(Next::Continue)
    }

    /// Propagate a non-fatal error up through any number of fn
    /// frames until we find a `try_call_wrapper`. On success,
    /// truncates the frame stack and value stack back to the
    /// wrapper's base, pushes a `Result::Err(RuntimeError { … })`
    /// for the outer caller, and returns `Ok(())` so the dispatch
    /// loop keeps going.
    ///
    /// Returns `Err(err)` (untouched) when:
    /// - the error is fatal (resource-limit violation), or
    /// - no enclosing `try_call` frame exists.
    fn unwind_to_try_call(&mut self, err: BopError) -> Result<(), BopError> {
        if err.is_fatal {
            return Err(err);
        }
        let wrap_idx = match self.frames.iter().rposition(|f| f.try_call_wrapper) {
            Some(i) => i,
            None => return Err(err),
        };
        let wrapper_stack_base = self.frames[wrap_idx].stack_base;
        // Drain the unwound frames through the freelist instead
        // of `truncate`, so their slot vecs get recycled.
        while self.frames.len() > wrap_idx {
            let frame = self.frames.pop().expect("frame present");
            if frame.is_function && self.type_bindings.len() > 1 {
                self.type_bindings.pop();
            }
            if !frame.slots.is_empty() {
                self.return_slots(frame.slots);
            }
        }
        self.stack.truncate(wrapper_stack_base);
        self.push_value(builtins::make_try_call_err(&err));
        Ok(())
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
