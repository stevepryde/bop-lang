# Architecture

## Boundaries and ownership

- **`bop-lang` / `bop`:** owns lexer, parser, AST, static checks, tree-walker,
  shared `Value`, operators, builtins, methods, memory accounting, bundled
  source-only standard library, `BopLimits`, and `BopHost`.
- **`bop-vm`:** owns AST-to-bytecode compilation and iterative VM execution. It
  depends on `bop-lang` for syntax and shared runtime semantics.
- **`bop-compile`:** owns Bop-to-Rust AOT transpilation and generated runtime
  glue. Generated programs depend on the shared runtime rather than defining a
  different language.
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
3. The selected engine evaluates values and delegates explicit capabilities to
   `BopHost`.
4. Limits and shared value semantics constrain execution; results, output, and
   errors cross the host boundary.

## Invariants

- **ARCH-001:** Engine-specific optimisation must not change source semantics.
- **ARCH-002:** Runtime values must remain safe to clone, compare, format, and
  destroy for all script-constructible shapes; those operations may not consume
  unbounded native call stack.
- **ARCH-003:** Allocation tracking is owned by shared value constructors,
  clone/drop behaviour, and explicit high-risk preflight checks.
- **ARCH-004:** Module loading uses host resolution, isolated module scope,
  cycle detection, and per-run caching with equivalent language behaviour
  across engines. A single shared step budget across imported modules is the
  target; walker and VM sub-engines currently restart their local step counter.
- **ARCH-005:** Walker and VM function call depth is bounded before native
  recursion can overflow; VM dispatch and loop control remain iterative.
  Sandboxed AOT currently relies on step checks at function entry and does not
  yet implement a separate fixed call-depth cap.
- **ARCH-006:** Public constructors preserve memory-accounting ownership by
  keeping tracked collection storage encapsulated.
- **ARCH-007:** Differential tests are the acceptance boundary for engine
  parity; focused unit and regression tests establish component behaviour.

## Failure modes

Parse errors, resource exhaustion, missing bindings/modules, invalid bytecode
passed to public VM APIs, generated-code failures, and host I/O failures must be
reported at their owning boundary without panics or misleading substitution.
