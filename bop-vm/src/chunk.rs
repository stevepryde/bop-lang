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
#[derive(Debug, Clone, PartialEq)]
pub enum Instr {
    // ─── Literals ─────────────────────────────────────────────────
    LoadConst(ConstIdx),
    LoadNone,
    LoadTrue,
    LoadFalse,

    // ─── Variables ────────────────────────────────────────────────
    /// Push value of the named variable onto the stack.
    LoadVar(NameIdx),
    /// Pop the top value and define it as a new local in the current scope.
    DefineLocal(NameIdx),
    /// Pop the top value and assign it to an existing variable.
    StoreVar(NameIdx),

    // ─── Scope ────────────────────────────────────────────────────
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
    /// writes the mutated value back. The back-write target is
    /// captured by the immediately-preceding `LoadVar` iff
    /// `assign_back_to` carries its name; otherwise there is no
    /// back-write.
    CallMethod {
        method: NameIdx,
        argc: u32,
        assign_back_to: Option<NameIdx>,
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
    /// Resolve, parse, compile, and run the module at `name`, then
    /// inject its top-level bindings into the current scope. The
    /// VM caches by module path so re-imports are cheap.
    Import(NameIdx),

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
    /// `Value::Struct`.
    ConstructStruct {
        type_name: NameIdx,
        count: u32,
    },
    /// Enum variant construction. The `shape` tells the VM how
    /// many stack entries the payload consumes:
    /// - `Unit` — no pops
    /// - `Tuple(argc)` — pop `argc` values
    /// - `Struct(count)` — pop `2 * count` (name, value) pairs
    ConstructEnum {
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

/// Shape of an enum variant's payload at the construction site —
/// tells the VM how many stack entries to pop.
#[derive(Debug, Clone, PartialEq)]
pub enum EnumConstructShape {
    Unit,
    Tuple(u32),
    Struct(u32),
}

// ─── Constants and recipes ─────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum Constant {
    Number(f64),
    Str(String),
}

/// A compiled string-interpolation recipe. Variables are looked up by
/// name at runtime and formatted in their declared order.
#[derive(Debug, Clone, PartialEq)]
pub struct InterpRecipe {
    pub parts: Vec<StringPart>,
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
#[derive(Debug, Clone)]
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
}

/// A compiled user-defined function.
#[derive(Debug, Clone)]
pub struct FnDef {
    pub name: String,
    pub params: Vec<String>,
    pub chunk: Chunk,
}
