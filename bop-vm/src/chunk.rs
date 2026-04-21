//! Bytecode chunk layout: instructions, constants, and nested function
//! bodies. See the crate root for the textual description of the
//! instruction set.

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec::Vec};

use bop::lexer::StringPart;

/// One bytecode operation.
///
/// Operand indices (`ConstIdx`, `NameIdx`, `FnIdx`, `InterpIdx`) are
/// indices into the owning chunk's pools. Jump targets are absolute
/// instruction indices inside the same chunk.
///
/// `Copy` is load-bearing for performance: the dispatch loop
/// reads one `Instr` per step by value out of the chunk's code
/// vec. Before `Copy` was added the read compiled to a full
/// `.clone()` call dispatching through the `Clone` impl's
/// match arm — surprisingly pricey in the hot path. With
/// `Copy`, the load is a trivial register-sized memcpy that
/// LLVM can fold into downstream dispatch.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Instr {
    // ─── Literals ─────────────────────────────────────────────────
    LoadConst(ConstIdx),
    LoadNone,
    LoadTrue,
    LoadFalse,

    // ─── Variables ────────────────────────────────────────────────
    /// Push value of the named variable onto the stack. Slow path —
    /// walks the current frame's `scopes` BTreeMap stack, then falls
    /// through to module-level / function-registry lookup. Emitted
    /// for captures, imports, and anything the compiler's
    /// `LocalResolver` couldn't pin to a slot.
    LoadVar(NameIdx),
    /// Pop the top value and define it as a new block-scoped local
    /// in the current BTreeMap scope. Used for match bindings,
    /// for-in variables at module top-level, and module-top-level
    /// `let` statements — everywhere slot resolution isn't active.
    DefineLocal(NameIdx),
    /// Pop the top value and assign it to an existing BTreeMap-scoped
    /// variable (companion to the `LoadVar` / `DefineLocal` slow
    /// path).
    StoreVar(NameIdx),

    /// Push the value of the local at `slot`. Fast path: a single
    /// `Vec::get_unchecked` into the current frame's slot array.
    /// Emitted by the compiler for every identifier reference that
    /// resolves to a function-level local (parameter, `let`, or
    /// `for-in` variable).
    LoadLocal(SlotIdx),
    /// Pop the top value and assign it to the local at `slot`.
    /// Used for both `let x = ...` initialisation and `x = ...`
    /// assignment; the VM treats them identically once slots are
    /// pre-sized at call time.
    StoreLocal(SlotIdx),

    // ─── Superinstructions ──────────────────────────────────────
    //
    // Fused opcodes that collapse a common 3-4 instruction
    // sequence into a single dispatch step. The compiler emits
    // them via peephole detection; the VM handles them with a
    // direct slot read + typed fast path, falling back to the
    // equivalent generic opcodes on type mismatch.

    /// Push `frame.slots[a] + frame.slots[b]` without touching
    /// the value stack for the operands — fuses `LoadLocal(a) +
    /// LoadLocal(b) + Add`. Fast path specialises on both
    /// operands being `Int`; the fallback delegates to
    /// `ops::add` with the slot values by reference.
    AddLocals(SlotIdx, SlotIdx),
    /// Push `frame.slots[a] < frame.slots[b]` — fuses
    /// `LoadLocal(a) + LoadLocal(b) + Lt`. Same
    /// Int-fast-path / generic-fallback split as `AddLocals`.
    LtLocals(SlotIdx, SlotIdx),
    /// `frame.slots[slot] += k` for a small `i32` `k`, fuses
    /// `LoadLocal(slot) + LoadConst(k) + Add + StoreLocal(slot)`.
    /// Specialised for `Int` slots — the `i = i + 1` and
    /// `total = total + small_k` idioms in tight loops. If the
    /// slot isn't an `Int`, falls back to generic add via the
    /// runtime's `ops::add`.
    IncLocalInt(SlotIdx, i32),
    /// Push `frame.slots[slot] + k` (as `Int`), fuses
    /// `LoadLocal(slot) + LoadConst(k) + Add`. Covers the
    /// `fib(n - 1)` / `array[i + 1]` patterns — `Sub` compiles
    /// as `Add` with a negated const so this one opcode captures
    /// both. Non-Int fallback delegates to `ops::add`.
    LoadLocalAddInt(SlotIdx, i32),
    /// Push `frame.slots[slot] < k` (as `Bool`), fuses
    /// `LoadLocal(slot) + LoadConst(k:Int) + Lt`. The `n < 2`
    /// base-case test in recursive functions.
    LtLocalInt(SlotIdx, i32),

    // ─── Scope ────────────────────────────────────────────────────
    /// Push a fresh BTreeMap onto the current frame's scopes stack.
    /// Only relevant when the slow-path `LoadVar` / `DefineLocal`
    /// machinery is in play; compiler omits these around blocks
    /// whose locals live in slots.
    PushScope,
    PopScope,

    // ─── Stack ────────────────────────────────────────────────────
    /// Discard the top of stack.
    Pop,
    /// Duplicate the top of stack.
    Dup,
    /// Duplicate the top two items: `[.., a, b]` → `[.., a, b, a, b]`.
    Dup2,

    // ─── Binary ops ───────────────────────────────────────────────
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,

    // ─── Unary ops ────────────────────────────────────────────────
    Neg,
    Not,

    /// Replace top with `Value::Bool(top.is_truthy())`. Used for
    /// short-circuit `&&` / `||`, which must return a Bool.
    TruthyToBool,

    // ─── Indexing ─────────────────────────────────────────────────
    /// `[.., obj, idx]` → `[.., obj[idx]]`.
    GetIndex,
    /// `[.., obj, idx, val]` → `[.., obj']` with `obj'[idx] == val`.
    SetIndex,

    // ─── String interpolation ─────────────────────────────────────
    /// Interpolate using a recipe from the chunk's interp pool. The
    /// variables named in the recipe are looked up by name in the
    /// current scope and formatted in order; the resulting string is
    /// pushed.
    StringInterp(InterpIdx),

    // ─── Collections ──────────────────────────────────────────────
    /// Pop `n` items, push an array.
    MakeArray(u32),
    /// Pop `n` (key, value) pairs (value on top), push a dict.
    /// Keys come from the name pool via [`Self::DictKey`] entries
    /// immediately preceding this op? No — simpler: keys are pushed
    /// as string values on the stack, interleaved with values.
    MakeDict(u32),

    // ─── Calls ────────────────────────────────────────────────────
    /// Call `name` with `argc` arguments popped from the stack.
    /// Resolution order: locally-bound closure → builtin → host →
    /// named user fn → error.
    Call { name: NameIdx, argc: u32 },
    /// Call whatever sits under the `argc` args on the stack. The
    /// callee must be a `Value::Fn`; anything else is a runtime
    /// error. Emitted when the call's callee expression isn't a
    /// bare ident (e.g. `funcs[0](x)` or `make_adder(5)(3)`).
    CallValue { argc: u32 },
    /// Method call: `[.., obj, args...]` → `[.., ret]`, and if the
    /// method is mutating and `obj` came from a variable, the VM
    /// writes the mutated value back to the original binding. The
    /// back-write target is the binding that produced `obj` —
    /// `Slot(idx)` for compile-time-resolved locals, `Name(idx)`
    /// for the scope-map fallback, or `None` when the receiver is
    /// a transient (e.g. `[1,2].push(3)` — nothing to update).
    CallMethod {
        method: NameIdx,
        argc: u32,
        assign_back_to: Option<AssignBack>,
    },

    // ─── Functions ────────────────────────────────────────────────
    /// Register the function at `FnIdx` in the current scope.
    DefineFn(FnIdx),
    /// Build a `Value::Fn` for the lambda at `FnIdx`, capturing
    /// every variable currently visible in the frame's scope
    /// stack. Pushes the resulting closure onto the value stack.
    MakeLambda(FnIdx),
    /// Pop the top value and return from the current call frame.
    Return,
    /// Return with `Value::None`.
    ReturnNone,

    // ─── Iteration / repeat ───────────────────────────────────────
    /// Pop iterable, push an internal iterator value. The VM owns
    /// the representation.
    MakeIter,
    /// If the iterator at the top of stack has another item, push
    /// it. Otherwise pop the iterator and jump to `target`.
    IterNext { target: CodeOffset },
    /// Pop a number, validate it, push an internal repeat counter.
    MakeRepeatCount,
    /// If the repeat counter at the top is non-zero, decrement it
    /// and fall through. Otherwise pop it and jump to `target`.
    RepeatNext { target: CodeOffset },

    // ─── Control flow ─────────────────────────────────────────────
    Jump(CodeOffset),
    /// Pop top; jump if falsy.
    JumpIfFalse(CodeOffset),
    /// Peek top; jump if falsy (don't pop). For `&&` short-circuit.
    JumpIfFalsePeek(CodeOffset),
    /// Peek top; jump if truthy (don't pop). For `||` short-circuit.
    JumpIfTruePeek(CodeOffset),

    // ─── Modules ─────────────────────────────────────────────────
    /// Resolve, parse, compile, and run the module at the path
    /// stored in `chunk.use_specs[idx]`, then inject its exports
    /// per the spec — glob (default), selective (`.{a, b}`),
    /// aliased (`as m`), or both. The VM caches by module path
    /// so re-uses are cheap. Instruction stays `Copy` by keeping
    /// the variable-length `items` / `alias` data in a side pool.
    Use(UseIdx),

    // ─── User-defined types ─────────────────────────────────────
    /// Register the struct type at `StructIdx` (declared fields
    /// live in the chunk's `struct_defs` pool). Subsequent
    /// `ConstructStruct` / `FieldGet` / `FieldSet` opcodes
    /// reference it by type name.
    DefineStruct(StructIdx),
    /// Register the enum type at `EnumIdx` (variants + their
    /// payload shapes live in the chunk's `enum_defs` pool).
    DefineEnum(EnumIdx),
    /// Register a user method. Receiver type is looked up by
    /// `type_name`; the body lives in `chunk.functions[fn_idx]`
    /// (same pool as user fns / lambdas).
    DefineMethod {
        type_name: NameIdx,
        method_name: NameIdx,
        fn_idx: FnIdx,
    },
    /// Struct literal: pop `2 * count` stack entries (field-name
    /// string + value, alternating — same layout as `MakeDict`),
    /// validate against the struct declaration, push a
    /// `Value::Struct`. `namespace` (when present) is the alias
    /// for a `m.Type { ... }` access — the VM checks that the
    /// name resolves to a `Value::Module` whose exports include
    /// this type before falling through to the normal registry.
    ConstructStruct {
        namespace: Option<NameIdx>,
        type_name: NameIdx,
        count: u32,
    },
    /// Enum variant construction. The `shape` tells the VM how
    /// many stack entries the payload consumes:
    /// - `Unit` — no pops
    /// - `Tuple(argc)` — pop `argc` values
    /// - `Struct(count)` — pop `2 * count` (name, value) pairs
    /// `namespace` mirrors the `ConstructStruct` field — set for
    /// `m.Result::Ok(v)` forms.
    ConstructEnum {
        namespace: Option<NameIdx>,
        type_name: NameIdx,
        variant: NameIdx,
        shape: EnumConstructShape,
    },
    /// Pop the object, push the value of the named field. Works
    /// on `Value::Struct` and on `Value::EnumVariant` with
    /// struct-shaped payloads.
    FieldGet(NameIdx),
    /// `[.., obj, val]` → `[.., obj']` with `obj'.field = val`.
    /// Used as the building block for `foo.field = v` — the
    /// compiler emits a `StoreVar` after to write the modified
    /// struct back.
    FieldSet(NameIdx),

    // ─── Pattern matching ───────────────────────────────────────
    /// Pattern-match the top-of-stack value against `pattern`.
    /// The value is popped unconditionally. On success, every
    /// binding introduced by the pattern is installed in the
    /// current scope and execution falls through. On failure,
    /// nothing is bound and execution jumps to `on_fail`.
    ///
    /// The compiler pairs each `match` arm with its own
    /// `PushScope` / `PopScope` bracket so bindings from a
    /// failed arm (e.g. when a guard rejects the candidate) are
    /// discarded before the next arm runs.
    MatchFail {
        pattern: PatternIdx,
        on_fail: CodeOffset,
    },
    /// Raise "No match arm matched the scrutinee". Emitted after
    /// the last arm's failure path so exhausting a `match` with
    /// no arm that applies is observable as a runtime error.
    MatchExhausted,

    /// `try <expr>` handler. Pops the top value and inspects it:
    /// - `Ok(v)` (single tuple payload)  → push `v`, fall through
    /// - `Ok`    (unit variant)          → push `none`, fall through
    /// - `Err(...)`                       → act like `Return` from
    ///   the current frame, carrying the whole `Err` variant as
    ///   the returned value. Raises at the engine boundary if
    ///   the current frame is the top-level program (no fn to
    ///   return from).
    /// - any other shape → raise a runtime error.
    TryUnwrap,

    // ─── Termination ──────────────────────────────────────────────
    /// End the current chunk (top-level program only).
    Halt,
}

// ─── Index newtypes ────────────────────────────────────────────────

/// Index into a chunk's constant pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConstIdx(pub u32);

/// Index into a chunk's name pool (used for variable and function names).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NameIdx(pub u32);

/// Index into a chunk's nested-function table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FnIdx(pub u32);

/// Slot index inside a function frame's flat `slots: Vec<Value>`
/// array. Assigned at compile time by the scope resolver so the
/// dispatch loop can read/write locals via direct `Vec` indexing
/// instead of name-keyed BTreeMap lookups. Slot numbers are
/// unique within a function body — blocks don't reuse them, so
/// `FnDef::slot_count` is the maximum index ever emitted plus
/// one, which pre-sizes the `slots` vec at call time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SlotIdx(pub u32);

/// Index into a chunk's string-interpolation recipe table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InterpIdx(pub u32);

/// Absolute instruction index within the same chunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CodeOffset(pub u32);

/// Index into a chunk's struct-definition pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StructIdx(pub u32);

/// Index into a chunk's enum-definition pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EnumIdx(pub u32);

/// Index into a chunk's pattern pool. Each `match` arm points at
/// one `Pattern` here; `MatchFail` consults it at runtime via the
/// shared `bop::pattern_matches` helper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PatternIdx(pub u32);

/// Index into a chunk's `use_specs` pool. Each `use` statement
/// emits one [`UseSpec`] describing the shape of the import
/// (path + optional `items` + optional `alias`); `Instr::Use`
/// stays `Copy` by holding this side-table index rather than the
/// variable-length spec inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UseIdx(pub u32);

/// Shape of an enum variant's payload at the construction site —
/// tells the VM how many stack entries to pop.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EnumConstructShape {
    Unit,
    Tuple(u32),
    Struct(u32),
}

/// Target for a mutating-method write-back. A call like
/// `arr.push(v)` needs the VM to re-bind `arr` to the mutated
/// array, but *where* that binding lives depends on whether the
/// receiver was a slot-resolved local or a name-scoped variable
/// (captures, module top-level, match bindings). The compiler
/// picks the right form at emit time so the runtime doesn't have
/// to probe both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AssignBack {
    /// Write the mutated value back into `slots[slot]` on the
    /// current frame.
    Slot(SlotIdx),
    /// Walk the current frame's BTreeMap scope stack, find the
    /// first entry named `name`, overwrite it. Matches the
    /// pre-slot `CallMethod` behaviour.
    Name(NameIdx),
}

// ─── Constants and recipes ─────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Constant {
    /// Exact integer constant (phase 6). Lowered from
    /// `ExprKind::Int` and materialised as `Value::Int` at
    /// `LoadConst` time.
    Int(i64),
    Number(f64),
    Str(String),
}

/// A compiled string-interpolation recipe. Variables are looked up by
/// name at runtime and formatted in their declared order.
#[derive(Debug, Clone, PartialEq)]
pub struct InterpRecipe {
    pub parts: Vec<StringPart>,
}

/// Compiled descriptor for a `use` statement. Parallel to the
/// parser's `StmtKind::Use` fields — the VM's `Use` opcode picks
/// this up by [`UseIdx`] at runtime so the hot-path instruction
/// stays `Copy`.
#[derive(Debug, Clone)]
pub struct UseSpec {
    pub path: String,
    pub items: Option<Vec<String>>,
    pub alias: Option<String>,
}

// ─── Chunk ─────────────────────────────────────────────────────────

/// A single compiled unit: either the top-level program or one
/// user-defined function body.
#[derive(Debug, Clone, Default)]
pub struct Chunk {
    pub code: Vec<Instr>,
    /// Source line for each instruction; parallel to `code`.
    pub lines: Vec<u32>,
    pub constants: Vec<Constant>,
    pub names: Vec<String>,
    pub interps: Vec<InterpRecipe>,
    pub functions: Vec<FnDef>,
    pub struct_defs: Vec<StructDef>,
    pub enum_defs: Vec<EnumDef>,
    /// Match patterns referenced by `MatchFail` instructions.
    pub patterns: Vec<bop::parser::Pattern>,
    /// Side-table of `use` descriptors referenced by
    /// `Instr::Use`. Holds variable-length fields (path, items,
    /// alias) so the instruction payload stays `Copy`.
    pub use_specs: Vec<UseSpec>,
    /// Slot count for this chunk when it serves as a function /
    /// lambda body. Zero at the top-level program chunk (where
    /// bindings live in the BTreeMap scope). The VM uses this
    /// to pre-size each call frame's `slots` vec exactly once.
    pub slot_count: u32,
}

/// Compiled struct type record.
#[derive(Debug, Clone)]
pub struct StructDef {
    pub name: String,
    pub fields: Vec<String>,
}

/// Compiled enum type record.
#[derive(Debug, Clone)]
pub struct EnumDef {
    pub name: String,
    pub variants: Vec<EnumVariantDef>,
}

#[derive(Debug, Clone)]
pub struct EnumVariantDef {
    pub name: String,
    pub shape: EnumVariantShape,
}

/// Payload shape of a declared enum variant.
#[derive(Debug, Clone, PartialEq)]
pub enum EnumVariantShape {
    Unit,
    Tuple(Vec<String>),
    Struct(Vec<String>),
}

impl Chunk {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.code.len()
    }

    pub fn is_empty(&self) -> bool {
        self.code.is_empty()
    }

    pub fn constant(&self, idx: ConstIdx) -> &Constant {
        &self.constants[idx.0 as usize]
    }

    pub fn name(&self, idx: NameIdx) -> &str {
        &self.names[idx.0 as usize]
    }

    pub fn interp(&self, idx: InterpIdx) -> &InterpRecipe {
        &self.interps[idx.0 as usize]
    }

    pub fn function(&self, idx: FnIdx) -> &FnDef {
        &self.functions[idx.0 as usize]
    }

    pub fn struct_def(&self, idx: StructIdx) -> &StructDef {
        &self.struct_defs[idx.0 as usize]
    }

    pub fn enum_def(&self, idx: EnumIdx) -> &EnumDef {
        &self.enum_defs[idx.0 as usize]
    }

    pub fn pattern(&self, idx: PatternIdx) -> &bop::parser::Pattern {
        &self.patterns[idx.0 as usize]
    }

    pub fn use_spec(&self, idx: UseIdx) -> &UseSpec {
        &self.use_specs[idx.0 as usize]
    }
}

/// A compiled user-defined function.
#[derive(Debug, Clone)]
pub struct FnDef {
    pub name: String,
    pub params: Vec<String>,
    pub chunk: Chunk,
    /// Total slot count for this function's frame (params + every
    /// `let` / `for-in` variable assigned a slot by the compiler).
    /// The VM resizes the frame's `slots` vec to this length
    /// exactly once at call time, so `LoadLocal` / `StoreLocal`
    /// can index it without bounds-check surprises.
    pub slot_count: u32,
    /// Names this function body references that don't resolve to
    /// its own locals. Filled in by the compiler for lambdas and
    /// nested fn bodies; empty for named fn declarations (which
    /// don't capture — their bodies see only params + the global
    /// function registry, matching the walker's FnDecl semantics).
    ///
    /// Paired with `capture_sources` positionally: `captures[i]`
    /// tells the VM *where* in the enclosing frame to read
    /// `capture_names[i]`'s value at `MakeLambda` time.
    pub capture_names: Vec<String>,
    pub capture_sources: Vec<CaptureSource>,
}

/// Where a captured name's value comes from when a lambda is
/// materialised. Resolved at compile time by walking the enclosing
/// function's slot table, falling back to the BTreeMap scope stack
/// for captures-of-captures and module-top-level bindings.
#[derive(Debug, Clone)]
pub enum CaptureSource {
    /// Read `enclosing_frame.slots[SlotIdx]`. Covers the common
    /// case: a lambda referencing a local of its immediate
    /// enclosing function.
    ParentSlot(SlotIdx),
    /// Look the name up in `enclosing_frame.scopes` (walked inner
    /// -> outer). Covers nested-lambda captures-of-captures and
    /// module-top-level references. Carries the name again
    /// because the runtime needs it for the BTreeMap lookup.
    ParentScope(String),
}
