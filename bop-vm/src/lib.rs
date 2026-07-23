//! Bytecode compiler and stack VM for the Bop programming language.
//!
//! `bop-vm` implements the same language, host interface, values, limits, and
//! diagnostics as the tree-walker in [`bop-lang`](https://docs.rs/bop-lang),
//! while compiling source to reusable bytecode. It is the usual choice for
//! scripts that run hot loops or are executed repeatedly at runtime.
//!
//! # One-shot execution
//!
//! [`run`] parses, compiles, validates, and executes one isolated program:
//!
//! ```
//! use bop::{BopError, BopHost, BopLimits, Value};
//!
//! struct Host;
//! impl BopHost for Host {
//!     fn call(
//!         &mut self,
//!         _: &str,
//!         _: &[Value],
//!         _: u32,
//!     ) -> Option<Result<Value, BopError>> {
//!         None
//!     }
//!
//!     fn on_print(&mut self, message: &str) {
//!         assert_eq!(message, "42");
//!     }
//! }
//!
//! let mut host = Host;
//! bop_vm::run(
//!     "print(6 * 7)",
//!     &mut host,
//!     &BopLimits::standard(),
//! ).unwrap();
//! ```
//!
//! # Compile once, execute with fresh state
//!
//! Use [`compile`] and [`execute`] when the bytecode should be reused but each
//! run should start with fresh globals:
//!
//! ```
//! # use bop::{BopError, BopHost, BopLimits, Value};
//! # struct Host;
//! # impl BopHost for Host {
//! #     fn call(&mut self, _: &str, _: &[Value], _: u32)
//! #         -> Option<Result<Value, BopError>> { None }
//! # }
//! let statements = bop::parse("let answer = 6 * 7").unwrap();
//! let chunk = bop_vm::compile(&statements).unwrap();
//! bop_vm::validate_chunk(&chunk).unwrap();
//!
//! let mut host = Host;
//! bop_vm::execute(
//!     chunk,
//!     &mut host,
//!     &BopLimits::standard(),
//! ).unwrap();
//! ```
//!
//! [`validate_chunk`] rejects malformed control flow, operands, pool indices,
//! and nested chunks before execution. [`disassemble`] provides a readable
//! representation for tooling and diagnostics.
//!
//! # Persistent programs
//!
//! [`BopInstance`] loads and compiles once, then retains globals, modules,
//! functions, types, methods, callbacks, and random-number state across host
//! calls. Root-level `pub fn` declarations define the callable ABI.
//!
//! ```
//! # use bop::{BopError, BopHost, BopLimits, Value};
//! # struct Host;
//! # impl BopHost for Host {
//! #     fn call(&mut self, _: &str, _: &[Value], _: u32)
//! #         -> Option<Result<Value, BopError>> { None }
//! # }
//! let mut host = Host;
//! let mut instance = bop_vm::BopInstance::load(
//!     "let count = 0\npub fn next() { count += 1; return count }",
//!     &mut host,
//!     &BopLimits::standard(),
//! ).unwrap();
//! assert_eq!(
//!     instance.call("next", &[], &mut host).unwrap().inspect(),
//!     "1",
//! );
//! ```
//!
//! # Features
//!
//! - `bop-std` (default) forwards to `bop-lang/bop-std` so hosts can
//!   resolve the bundled `std.*` modules.
//! - `no_std` forwards to `bop-lang/no_std`; combine it with
//!   `default-features = false` for bare-metal or
//!   `wasm32-unknown-unknown` builds.
//!
//! See the [embedding guide](https://bop-lang.com/docs/embedding/) and
//! [stateful instance guide](https://bop-lang.com/docs/embedding/instances/)
//! for lifecycle, limits, callback affinity, and engine-selection details.

#![cfg_attr(feature = "no_std", no_std)]

#[cfg(feature = "no_std")]
extern crate alloc;

// The standard test harness remains available when `--all-features` selects
// the library's `no_std` surface; make that test-only dependency explicit.
#[cfg(all(test, feature = "no_std"))]
extern crate std;

pub mod chunk;
pub mod compiler;
pub mod disasm;
pub mod validate;
pub mod vm;

pub use chunk::{
    Chunk, CodeOffset, ConstIdx, Constant, EnumConstructShape, EnumDef, EnumIdx, EnumVariantDef,
    EnumVariantShape, FnDef, FnIdx, InterpIdx, InterpPart, InterpRecipe, Instr, LoopStateKind,
    NameIdx, StructDef, StructIdx,
};
pub use compiler::compile;
pub use disasm::disassemble;
pub use validate::validate_chunk;
pub use vm::{BopInstance, Vm, execute, run};
