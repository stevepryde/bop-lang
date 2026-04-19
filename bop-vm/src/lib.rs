//! Bytecode compiler and stack VM for Bop.
//!
//! This is step 2a of the execution-modes roadmap: the compiler and
//! instruction set only. Execution (dispatch loop, resource limits,
//! differential harness) lands in 2b / 2c.
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
//! # What this crate does not yet do
//!
//! Execution lands in step 2b. This crate compiles a parsed `bop-lang`
//! AST into a [`Chunk`] and renders it back out via [`disassemble`].
//! There is no dispatch loop, no stack, no runtime state.

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc;

pub mod chunk;
pub mod compiler;
pub mod disasm;

pub use chunk::{Chunk, CodeOffset, ConstIdx, Constant, FnDef, FnIdx, InterpIdx, InterpRecipe, Instr, NameIdx};
pub use compiler::compile;
pub use disasm::disassemble;
