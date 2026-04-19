# Bop Execution Modes & Compilation Strategy

Status: design / roadmap. Step 1 (`bop-sys`) has been started.

## Summary

Bop currently ships as a tree-walking interpreter. This document captures
the roadmap for additional execution modes and the supporting crate
reorganisation.

Decisions:

- **Yes** to `bop-sys`: a separate crate for standard host / OS integration.
- **Yes** to a **Bytecode VM**: feature-flagged, dependency-light, WASM-capable.
- **Yes** to **AOT Rust** transpilation: Bop source → Rust source → native binary.
- **No** to JIT: out of scope.

## Execution modes

### 1. Tree-walking interpreter (existing)

Today's implementation. Source → tokens → AST → direct evaluation via
`evaluator::Evaluator`. Stays as the default.

- Small, simple, and good for short scripts, the REPL, and learning.
- Keeps the `no_std` / embedded story clean.
- Resource limits (`BopLimits`) apply here first and foremost.

Status: shipping. No changes planned beyond bug fixes and new language features.

### 2. Bytecode VM (new)

A stack-based VM that compiles the AST to a compact bytecode and executes
it. Faster than tree-walking for hot loops and longer programs. Useful
when Bop is embedded in a game loop or serves as a scripting target for
workloads larger than a REPL one-liner.

Constraints:

- **Feature-flagged** behind a cargo feature (e.g. `bytecode` or `vm`). The
  tree-walker must remain available when the feature is off.
- **Minimise dependencies.** The VM is pure Rust — no LLVM, no Cranelift,
  no external runtime. At most, small well-vetted crates, and only if
  strictly needed.
- **`no_std` compatible.** Same story as the interpreter: works under
  `alloc` only.
- **WASM support is nice-to-have, not essential.** If a genuine tradeoff
  appears between WASM compatibility and VM performance / clarity, prefer
  performance. WASM should come mostly for free by keeping dependencies
  minimal and avoiding host-specific code in the VM itself.
- **Same resource-limit semantics** as the tree-walker (step count, memory
  ceiling). Limits are part of Bop's safety story, not an interpreter
  detail.

Explicitly out of scope (for v1):

- Register-based VM (stick with stack-based — simpler).
- NaN-boxing / fancy value representation.
- Stable on-disk bytecode format. Bytecode is an implementation detail;
  users should not rely on a `.bopc` file they can ship separately.
- Debugger protocols, profilers, inline caches. Nice to have, not in scope.

### 3. AOT Rust transpiler (new)

A separate tool that converts a Bop program into **Rust source code**. The
user then compiles that Rust with `rustc` / `cargo` to produce a
standalone native binary.

Why Rust output (and not C, LLVM IR, WASM, etc.):

- Lets Bop target anywhere Rust already targets, with no new backend work.
- After `rustc` gets hold of it, hot code gets real optimisation.
- No runtime VM to embed — the output is just Rust that links against
  `bop-sys` (and/or a small runtime shim) for builtins and host calls.
- The toolchain assumption (`cargo`/`rustc` installed) is acceptable
  because this path is for users who want a native binary; they're
  already in Rust territory.

Shape:

- Lives in a new crate, likely `bop-compile` or `bop-rustc`, invoked by
  `bop-cli` (e.g. `bop build foo.bop`).
- Emits **human-readable Rust**, not obfuscated codegen — easier to
  debug, easier to audit, easier to hand-tune if someone wants to.
- Links against `bop-sys` for runtime builtins so we don't reimplement
  `range`, `str`, `split`, etc. twice.
- Resource limits become optional at this layer. Native binaries
  traditionally don't get step-counted; the user can opt in if they want
  the sandbox semantics.

Non-goals for AOT v1:

- Emitting C or C++. Rust only.
- Direct native codegen (skipping the `rustc` step). That's JIT-adjacent
  and out of scope.
- Cross-compiling for the user. They drive `cargo build --target=…`
  themselves.

### 4. JIT — rejected

Explicitly out of scope. A JIT would require either:

- Embedding LLVM (huge dependency, slow compile, complex build), or
- Cranelift (smaller, but still a sizeable dep and platform-specific).

Neither is worth it for Bop's intended use cases. AOT-Rust covers the "I
want native speed" story. The bytecode VM covers "I want faster than
tree-walking but still embeddable." JIT sits awkwardly between the two,
carrying a large dependency cost for modest incremental value.

If the need ever becomes real, reopen the discussion. For now: no.

## The `bop-sys` crate

A new crate alongside `bop-lang` that provides standard host / OS
integration: file I/O, env vars, time, stdin, etc. — the things the core
language deliberately stays agnostic of.

Status: crate split implemented. `bop-sys` now provides the standard
stdout-backed host used by `bop-cli`, plus stdin, file, env, and time host
functions.

Rationale:

- Keeps `bop-lang` core **pure**: no I/O deps, no platform assumptions,
  stays clean for `no_std` and embedded use.
- Gives the AOT-Rust output a well-defined runtime surface to link
  against, instead of inlining builtins into generated code.
- Gives embedders a clear "give me the standard host" import without
  dragging I/O and std assumptions into the interpreter crate.

Shape:

- Implements `BopHost` with a standard set of builtins (print, readline,
  file ops, time, env…).
- Intentionally not feature-flagged internally. Embedded/minimal use cases
  depend on `bop-lang` directly; users that choose `bop-sys` get the full
  standard host.
- Re-used by `bop-cli` so the `bop` binary keeps its current behaviour
  without duplicating code.

## Target crate layout

```
bop-lang     core: lexer, parser, AST, tree-walking evaluator, BopHost trait
bop-vm       bytecode compiler + VM  (feature-flagged, dep-light)
bop-sys      standard host: I/O, time, env, stdin  (implements BopHost)
bop-compile  AOT: Bop source → Rust source
bop-cli      user-facing CLI: run, repl, build
```

Whether the bytecode VM lives in its own crate (`bop-vm`) or as a feature
inside `bop-lang` is a later call. The key decision is the **boundary**,
not the crate name. Start it as a feature-flagged module and promote it
to its own crate only if the dependency story demands it.

## Phasing

Rough order of work (each step is independently shippable):

1. **Split `bop-sys` out** of `bop-cli` / `bop-lang`. Lowest risk;
   unblocks later work. Pure refactor from a user's point of view.
2. **Bytecode VM** behind a feature flag. Must not break tree-walker
   users. Start with AST → bytecode compile step, stack VM, same
   semantics and limits as the tree-walker. Add a differential-fuzz
   harness (same program, both engines, compare output).
3. **AOT-Rust transpiler**. Depends on a stable AST (we have one) and
   `bop-sys` (step 1). Start with a subset of the language; grow until
   it reaches parity.

JIT is not on the roadmap. If that changes, open a new design doc.

## Non-goals

- Breaking the tree-walker API. The current public surface (`run`,
  `BopHost`, `BopLimits`) stays stable.
- Stable on-disk bytecode format. Bytecode is internal.
- Multi-language AOT targets. Rust output only for v1. C / C++ / WASM
  could come later but are not on the near-term plan.
- JIT. See above.

## Open questions

- Should `bop-vm` be its own crate from day one, or live behind a feature
  in `bop-lang` until it outgrows that? Lean toward feature-in-crate
  first; split later if needed.
- What's the smallest useful subset of the language for AOT v1? Probably:
  numbers, strings, arrays, dicts, functions, control flow, the common
  builtins. Closures can come later if the surface allows them cleanly.
- How do resource limits translate to AOT output? Options: (a) compile
  them out entirely, (b) emit optional step/memory checks behind a
  `--sandbox` flag. Lean toward (b), off by default.
- Do we want a differential testing harness that runs the test suite
  against all three engines (interpreter / VM / transpiled)? Almost
  certainly yes — it's the cheapest way to keep them honest.
