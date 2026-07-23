# Architecture

## Boundaries and ownership

- **`bop-lang` / `bop`:** owns lexer, parser, AST, static checks, tree-walker,
  shared `Value`, operators, builtins, methods, memory accounting, bundled
  source-only standard library, `BopLimits`, `BopHost`, shared entry metadata,
  and the walker `BopInstance`.
- **`bop-vm`:** owns AST-to-bytecode compilation, iterative VM execution, and
  retained VM instance state. It depends on `bop-lang` for syntax, shared entry
  metadata, values, and runtime semantics.
- **`bop-compile`:** owns Bop-to-Rust AOT transpilation and generated runtime
  glue, including the sandboxed generated `BopInstance`. Generated programs
  depend on the shared runtime rather than defining a different language.
- **`bop-sys`:** owns the optional standard OS-backed host, including file,
  stdio, environment, time, and filesystem module resolution.
- **`bop-cli`:** owns user-facing run, REPL, and compile flows and composes the
  engines with `bop-sys`.

Dependency direction flows from CLI and engine adapters toward the zero-dep
core. OS capabilities flow inward only through `BopHost`; `bop-lang` must not
depend on `bop-sys`.

## Execution flow

1. Source is lexed and parsed by `bop-lang`.
2. The walker evaluates the AST directly, the VM compiles it to `Chunk`, or the
   AOT compiler emits Rust linked to the shared runtime.
3. A one-shot API evaluates and discards its state, or `BopInstance::load`
   evaluates the top level once and retains the resulting engine state plus its
   final public-entry table.
4. The selected engine evaluates values and delegates explicit capabilities to
   the `BopHost` borrowed for that operation.
5. Limits and shared value semantics constrain execution; results, callbacks,
   output, and errors cross the host boundary.

## Invariants

- **ARCH-001:** Engine-specific optimisation must not change source semantics.
- **ARCH-002:** Runtime values must remain safe to clone, compare, format, and
  destroy for all script-constructible shapes; those operations may not consume
  unbounded native call stack.
- **ARCH-003:** Allocation tracking is owned by shared value constructors,
  clone/drop behaviour, and explicit high-risk preflight checks.
- **ARCH-004:** Module loading uses host resolution, isolated module scope,
  cycle detection, and per-run or per-instance caching with equivalent
  language behaviour across engines. A single shared step budget across
  imported modules is the target; walker and VM sub-engines currently restart
  their local step counter.
- **ARCH-005:** Walker, VM, and sandboxed AOT function call depth is bounded
  before native recursion can overflow; VM dispatch and loop control remain
  iterative.
- **ARCH-006:** Public constructors preserve memory-accounting ownership by
  keeping tracked collection storage encapsulated.
- **ARCH-007:** Differential tests are the acceptance boundary for engine
  parity; focused unit and regression tests establish component behaviour.
- **ARCH-008:** Persistent state is owned by exactly one engine instance.
  Returned callable values carry origin metadata, retained language values
  keep their instance memory account alive, and a value from one instance
  cannot be executed by another.
- **ARCH-009:** An instance stores limits and language state but never a host
  reference. A per-operation guard rejects same-instance re-entry; transient
  execution state is restored on every exit path so non-memory failures do not
  poison later calls.
- **ARCH-010:** Runtime declaration sites are discovered statically only to
  produce engine-specific descriptors or lifted adapters. Publication to type
  and method registries occurs in source execution order, preserving dead-code
  and nested-scope semantics across engines.

## Failure modes

Parse errors, resource exhaustion, missing bindings/modules/entries, invalid or
foreign callback values, same-instance re-entry, invalid bytecode passed to
public VM APIs, generated-code failures, and host I/O failures must be reported
at their owning boundary without panics or misleading substitution. Instance
calls are not transactions: transient frames unwind, but completed language
mutations remain authoritative.
