# Language and runtime contract

## Purpose

This specification owns the observable contract shared by Bop's tree-walker,
bytecode VM, AOT compiler, CLI, and embedding APIs.

## Requirements

- **RUN-001 — Embeddable core.** `bop-lang` must provide an embeddable,
  dynamically typed language whose ambient capabilities are limited to those
  explicitly exposed by `BopHost`.
- **RUN-002 — Sandbox termination.** A script that exceeds a runtime resource
  boundary or exercises adversarial input must halt with a Bop diagnostic; it
  must not hang, panic, overflow the native stack, or abort the host process.
- **RUN-003 — Resource accounting.** Walker and VM execution, plus AOT output
  emitted with sandboxing enabled, must enforce their applicable step,
  tracked-memory, and call-depth boundaries. AOT sandboxing is opt-in; the CLI's
  compiled binaries are currently emitted without runtime limits.
- **RUN-004 — Engine parity.** For the same source and host behaviour, the three
  engines must agree on language-visible values, output, mutations, errors, and
  module semantics, except for explicitly documented engine API differences.
  Resource checkpoints may occur at engine-specific times, but enabled limits
  must terminate cleanly rather than changing results or terminating the host.
- **RUN-005 — Core isolation.** Core language execution must not perform
  filesystem, network, clock, environment, or other OS I/O except through a
  host capability.
- **RUN-006 — Portable core.** `bop-lang` and `bop-vm` must remain usable in
  supported `no_std` and `wasm32-unknown-unknown` embeddings.
- **RUN-007 — Stable diagnostics.** Invalid syntax and runtime failures must
  produce actionable Bop errors with source context when available; equivalent
  engine failures should retain the same error shape and helpful hints.
- **RUN-008 — General-purpose language semantics.** Functions and closures,
  collections, user-defined types, pattern matching, control flow, iterators,
  and modules must compose according to the documented grammar and reference
  material.
- **RUN-009 — Correctness over silent truncation.** Resource guards and engine
  limitations must surface an error rather than silently changing a program's
  result.
- **RUN-010 — No silent mutation loss.** A mutating method must not report
  success when an unsupported receiver place would silently discard the
  mutation. Index and field receivers that cannot yet be written back must
  raise an actionable runtime error; genuine by-value temporaries may still be
  mutated and discarded intentionally.
- **RUN-011 — Container value semantics.** Assignment, argument passing,
  capture, and return preserve independent value semantics for arrays, dicts,
  structs, and enum payloads. Implementations may share backing storage until
  mutation, but changing one value must not change another. Iterators are the
  deliberate exception: cloned iterator handles share their cursor.
- **RUN-012 — Constant bindings are immutable assignment roots.** No assignment
  target whose base binding is a constant may mutate that value, including
  direct, index, field, compound, grouped, and syntactically nested place
  forms. Reads through a constant and writes through lowercase mutable
  bindings remain valid. A new `const` declaration is a declaration, not an
  assignment target.
- **RUN-013 — Exact signed integer literals.** Decimal integer source must map
  to exact signed 64-bit values without floating-point fallback. The minimum
  value `-9223372036854775808` is valid in expression and literal-pattern
  contexts; positive or larger magnitudes are rejected with source context,
  and arithmetic at either boundary remains checked.

## Acceptance criteria

- **AC-RUN-001:** A custom host exposing no functions cannot access ambient OS
  facilities, while a host-provided function is callable through `BopHost`.
- **AC-RUN-002:** Programs that exceed step, memory, call, parse, or safe value
  processing boundaries return `Err(BopError)` or another documented clean
  termination without terminating the embedding process.
- **AC-RUN-003:** The differential suites cover representative successful and
  failing programs and report no walker/VM/AOT semantic, output, or diagnostic
  drift outside documented resource-checkpoint differences.
- **AC-RUN-004:** Core and VM builds succeed for the supported standard,
  `no_std`, and WASM configurations documented by the project.
- **AC-RUN-005:** Parser, runtime, and CLI errors identify the real failure and
  do not replace I/O, binding, or limit failures with misleading results.
- **AC-RUN-006:** Cloning a container handle does not charge a second backing
  allocation; unique mutation does not copy its backing storage; the first
  mutation of shared storage detaches exactly once; and tracked storage is
  released when its last owner drops.
- **AC-RUN-007:** Parser and cross-engine tests reject direct and compound
  Array, Dict, and Struct writes rooted at constants with the canonical
  constant diagnostic and hint, including grouped/nested targets, while the
  corresponding mutable-binding programs still execute identically.
- **AC-RUN-008:** Reserved-word binding diagnostics derive from the lexer's
  current keyword vocabulary, including `const`, while keyword-shaped text in
  strings and comments remains ordinary source content. The compatibility
  `precheck::check` API retains its narrow `let` / named-`fn` contract without
  maintaining a second keyword list.
- **AC-RUN-009:** Lexer/parser, walker/VM differential, and native AOT tests
  cover `i64::MIN` expressions and patterns, exact integer type/value
  preservation, unary-minus and subtraction precedence, checked overflow, and
  rejection of one-beyond magnitudes on both signs with line and column.

## Design notes

The grammar reference and user documentation under `docs/src/` remain the
canonical teaching material. This file owns cross-engine guarantees rather
than duplicating syntax documentation.

An inert or custom `BopHost` is capability-sandboxed by default. `bop-sys` and
the CLI deliberately grant selected OS capabilities and therefore are not an
ambient-authority sandbox, even when language resource limits are enabled.
