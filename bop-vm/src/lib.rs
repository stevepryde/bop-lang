//! Bytecode compiler and stack VM for Bop.
//!
//! Steps 2a (compiler + instruction set) and 2b (VM + limits) have
//! landed. The differential harness against the tree-walker is step 2c.
//!
//! # Instruction set (stack-based)
//!
//! The VM is a stack machine. All operators consume their arguments
//! from the top of the stack and push their result. Every instruction
//! carries a source line (stored in a parallel `lines` table) so
//! runtime errors can be reported against the original program.
//!
//! ## Literals
//! - `LoadConst(idx)` — push a pooled number or string.
//! - `LoadNone` / `LoadTrue` / `LoadFalse` — push the singleton.
//!
//! ## Variables (names indexed into the chunk's name pool)
//! - `LoadVar(n)` — push the value bound to name `n`.
//! - `DefineLocal(n)` — pop and bind as a new local in current scope.
//! - `StoreVar(n)` — pop and assign to an existing variable.
//!
//! ## Scope
//! - `PushScope` / `PopScope` — open / close a block scope.
//!
//! ## Stack
//! - `Pop` — discard top.
//! - `Dup` / `Dup2` — duplicate top (or top two).
//!
//! ## Operators
//! One opcode per primitive (`Add`, `Sub`, `Mul`, `Div`, `Rem`, `Eq`,
//! `NotEq`, `Lt`, `Gt`, `LtEq`, `GtEq`, `Neg`, `Not`). Short-circuit
//! `&&` / `||` compile to explicit jumps + `TruthyToBool`, not to a
//! dedicated opcode.
//!
//! ## Indexing
//! - `GetIndex` — `[obj, idx]` → `[obj[idx]]`.
//! - `SetIndex` — `[obj, idx, val]` → `[obj']` with `obj'[idx] = val`.
//!
//! ## Collections
//! - `MakeArray(n)` — pop `n` items, push an array.
//! - `MakeDict(n)` — pop `n` key-value pairs (key, then value, per
//!   entry), push a dict.
//!
//! ## String interpolation
//! - `StringInterp(idx)` — run the recipe at `idx`, looking up any
//!   variable parts in the current scope, and push the result.
//!
//! ## Calls
//! - `Call { name, argc }` — call a named function (builtin, host, or
//!   user) with `argc` args popped in reverse order.
//! - `CallMethod { method, argc, assign_back_to }` — method call on
//!   an object under the args. If the method is mutating and
//!   `assign_back_to` is set, the mutated object is written back to
//!   that variable (matching the tree-walker's semantics for
//!   `arr.push(x)` etc.).
//!
//! ## Functions
//! - `DefineFn(idx)` — register the compiled function at `idx`.
//! - `Return` / `ReturnNone` — return from the current call frame.
//!
//! ## Iteration and repeat
//! - `MakeIter` + `IterNext { target }` — iterate over the top value.
//! - `MakeRepeatCount` + `RepeatNext { target }` — counted loop.
//!
//! ## Control flow (absolute offsets within the chunk)
//! - `Jump(t)`, `JumpIfFalse(t)`, `JumpIfFalsePeek(t)`, `JumpIfTruePeek(t)`.
//!
//! ## Termination
//! - `Halt` — only emitted at the end of the top-level chunk.
//!
//! # Using the VM
//!
//! Call [`run`] with Bop source to parse, compile, and execute through
//! the VM. For finer control, compile a [`Chunk`] with [`compile`] and
//! hand it to [`execute`] (or construct [`Vm`] directly).
//!
//! The VM shares [`bop::BopHost`] / [`bop::BopLimits`] semantics with
//! the tree-walking evaluator in `bop-lang`.

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc;

pub mod chunk;
pub mod compiler;
pub mod disasm;
pub mod vm;

pub use chunk::{
    Chunk, CodeOffset, ConstIdx, Constant, EnumConstructShape, EnumDef, EnumIdx, EnumVariantDef,
    EnumVariantShape, FnDef, FnIdx, InterpIdx, InterpRecipe, Instr, NameIdx, StructDef, StructIdx,
};
pub use compiler::compile;
pub use disasm::disassemble;
pub use vm::{Vm, execute, run};
