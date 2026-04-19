# Bop Execution Modes & Compilation Strategy

Status: design / roadmap. Step 1 (`bop-sys`), step 2a (bytecode
compiler in `bop-vm`), and step 2b (VM dispatch + limits) have all
landed. Step 2c (differential harness + fuzzing) is next.
`bop-lang` stays self-contained — the VM and AOT crates depend on it
directly for `Value` / builtins / operator primitives, rather than a
separate runtime crate.

## Summary

Bop currently ships as a tree-walking interpreter. This document captures
the roadmap for additional execution modes and the supporting crate
reorganisation.

Decisions:

- **Yes** to `bop-sys`: a separate crate for standard host / OS integration.
- **No** to a separate `bop-runtime` crate. `bop-lang` stays
  self-contained (no internal crate deps) so it publishes cleanly on
  crates.io. The "shared runtime surface" the VM and AOT need —
  `Value`, `BopError`, memory tracking, builtins, methods, operator
  primitives — lives in `bop-lang`'s public API (exposed through the
  `bop::value`, `bop::error`, `bop::memory`, `bop::ops`, `bop::builtins`,
  `bop::methods` modules). VM / AOT crates depend on `bop-lang` directly.
- **Yes** to a **Bytecode VM** as its own crate from day one
  (`bop-vm`), dependency-light, WASM-capable.
- **Yes** to **AOT Rust** transpilation (`bop-compile`): Bop source →
  Rust source → native binary.
- **No** to JIT: out of scope.

Package name vs import name: `bop-lang` is the Cargo package; `bop` is
the lib name used in `use` paths. Keep both — the `bop` import name is
shorter and already shipped. Plan text uses "`bop-lang`" when referring
to the package / crate and "`bop::`" when referring to imports.

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

- **Separate crate** (`bop-vm`) that depends on `bop-lang` for both
  the AST and the runtime surface (`Value`, ops, builtins, memory
  tracking). Keeps the tree-walker path unaffected and avoids a
  growing `#[cfg]` forest inside `bop-lang`.
- **Minimise dependencies.** The VM is pure Rust — no LLVM, no Cranelift,
  no external runtime. At most, small well-vetted crates, and only if
  strictly needed.
- **`no_std` compatible.** Same story as the interpreter: works under
  `alloc` only.
- **WASM support is nice-to-have, not essential.** If a genuine tradeoff
  appears between WASM compatibility and VM performance / clarity, prefer
  performance. WASM should come mostly for free by keeping dependencies
  minimal and avoiding host-specific code in the VM itself.
- **Same resource-limit semantics** as the tree-walker, reinterpreted
  in VM terms:
  - **Step counting**: per bytecode instruction dispatched, with a
    per-op cost table (most ops = 1; calls / long-running ops can
    charge more). A check at every loop backedge and at call entry
    guarantees infinite loops halt. `BopLimits::max_steps` stays the
    user-facing knob; the VM scales it internally if a source-level
    step maps to several bytecode ops, so `standard()` / `demo()`
    remain meaningful without retuning.
  - **Memory tracking**: all `Value` allocations (strings, arrays,
    dicts) route through `bop::memory`'s accounting hook, shared with
    the tree-walker. The VM must not create `Value::Str` / `Value::Arr`
    directly — it goes through `Value::new_*` constructors so the
    memory ceiling is enforced uniformly.
  - **Host calls**: `BopHost::on_tick` fires at the same cadence as in
    the tree-walker (at least once per loop backedge and call entry),
    so timeouts and cancellation work identically.

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
  `bop-lang` for language-level builtins and `bop-sys` for host calls
  (when the program uses them).
- The toolchain assumption (`cargo`/`rustc` installed) is acceptable
  because this path is for users who want a native binary; they're
  already in Rust territory.

Shape:

- Lives in a new crate, `bop-compile`, invoked by `bop-cli` (e.g.
  `bop build foo.bop`).
- Emits **human-readable Rust**, not obfuscated codegen — easier to
  debug, easier to audit, easier to hand-tune if someone wants to.
- Generated code depends on `bop-lang` for `Value`, operators, and
  built-in methods (so we don't reimplement `range`, `str`, `split`,
  etc. twice), and on `bop-sys` for host-backed builtins when the
  program uses them.
- Resource limits become optional at this layer, off by default — the
  hot path should be clean. Behind a `--sandbox` flag, the transpiler
  emits step-count checks at loop backedges / function entry and
  `bop::memory`'s allocation hooks enforce the memory ceiling. Without
  `--sandbox`, the output is straight-line Rust with no accounting
  overhead.

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
bop-lang     core: lexer, parser, AST, tree-walking evaluator, BopHost trait,
             Value, operator primitives (bop::ops), builtins, methods,
             memory accounting. Self-contained — no internal crate deps.
bop-vm       bytecode compiler + stack VM  (depends on bop-lang for AST,
             Value, ops, memory tracking; dep-light, no_std-capable)
bop-sys      standard host: I/O, time, env, stdin  (implements BopHost; std-only)
bop-compile  AOT: Bop source → Rust source  (emits code that depends on
             bop-lang and optionally bop-sys)
bop-cli      user-facing CLI: run, repl, build
```

The VM and AOT-Rust output share `bop-lang`'s runtime surface:
`bop::value::Value`, `bop::ops` (operator primitives as pure
functions), `bop::memory` (allocation tracking), `bop::builtins`, and
`bop::methods`. Without this single source of truth, each engine would
reinvent coercion rules and drift.

`bop-vm` is a separate crate from day one, not a feature inside
`bop-lang`. Rationale: `bop-lang` stays minimal and `no_std`-clean;
embedders that never want the VM don't pay compile cost for it; the VM
can iterate without touching `bop-lang`'s public surface. Feature-flag
splits are easy to add later and painful to remove after publication.

## Phasing

Rough order of work (each step is independently shippable):

1. **Split `bop-sys` out** of `bop-cli` / `bop-lang`. Lowest risk;
   unblocks later work. Pure refactor from a user's point of view.
   *Status: done.*

1b. **Expose the runtime surface as public API in `bop-lang`.** Promote
    the operator primitives (previously inlined in `evaluator::binary_op`)
    into a `bop::ops` module (`add`, `sub`, `mul`, `div`, `rem`, `eq`,
    `not_eq`, `lt`, `gt`, `lt_eq`, `gt_eq`, `neg`, `not`, `index_get`,
    `index_set`) as pure functions over `Value`. Keep `Value`,
    `BopError`, `memory`, `builtins`, `methods` where they are —
    `bop-lang` stays self-contained. The VM and AOT crates will
    depend on `bop-lang` directly for this surface. `BopError::runtime`
    is the canonical error constructor.
    *Status: done.*

2. **Bytecode VM** as a new `bop-vm` crate. Must not change tree-walker
   behavior. Ships in three sub-steps, each independently mergeable:

   - **2a. Compiler.** AST (from `bop-lang`) → bytecode. Stable
     instruction set, stack-based, documented in the crate. No
     execution yet — round-trip tests (compile → disassemble) only.
     *Status: done. Crate: [`bop-vm`](../bop-vm). Instruction set
     and pool layout documented in `bop-vm/src/lib.rs`. 25
     round-trip tests in `bop-vm/tests/compile_roundtrip.rs` pin the
     emitted shape for every language feature (literals, variables,
     operators, short-circuit, if/else chains, while/for/repeat with
     break/continue, methods with mutating back-assign, functions,
     nested functions, string interpolation, dicts).*
   - **2b. VM + limits.** Dispatch loop, step counting per-op with a
     cost table, loop-backedge / call-entry checks, `on_tick` hooks,
     memory tracking routed through `bop::memory`. At the end of 2b
     the VM passes the full `bop-lang` test suite on its own.
     *Status: done. Implementation: [`bop-vm/src/vm.rs`](../bop-vm/src/vm.rs).
     Stack-based dispatch with per-frame scopes, `Rc<Chunk>` for
     function sharing, and iteration/repeat slots kept inline on the
     value stack. `bop::memory` tracks allocations automatically via
     `Value`'s `Clone` / `Drop`, so no VM-specific bookkeeping is
     needed for `max_memory`. `max_steps` is scaled internally
     (`STEP_SCALE = 8`) so source-level budgets survive the 1-op-per-
     instruction expansion. `bop::builtins` and `bop::methods` are
     promoted to public modules so the VM can share the tree-walker's
     builtin / method implementations. Tested in
     [`bop-vm/tests/semantics.rs`](../bop-vm/tests/semantics.rs)
     (117 tests mirroring `bop-lang`'s suite, including safety /
     resource-limit cases).*
   - **2c. Differential harness.** Every test in the suite runs
     against both engines; outputs and error messages must match.
     Fuzzing layered on top (random programs from a constrained
     grammar, compare outputs). Promoted from "nice to have" — this
     is how we keep the engines from drifting. Required before
     shipping the VM.

3. **AOT-Rust transpiler** (`bop-compile`). Depends on `bop-lang` for
   the AST and runtime surface. Start with a subset (numbers, strings,
   arrays, dicts, functions, control flow, common builtins). Extend
   the differential harness from 2c to run tests through the
   transpiled path too; grow the subset until it reaches parity.

JIT is not on the roadmap. If that changes, open a new design doc.

## Non-goals

- Breaking the tree-walker API. The current public surface (`run`,
  `BopHost`, `BopLimits`, `Value`, `BopError`) stays stable.
- Stable on-disk bytecode format. Bytecode is internal.
- Multi-language AOT targets. Rust output only for v1. C / C++ / WASM
  could come later but are not on the near-term plan.
- `no_std` support for `bop-sys`. `bop-sys` is std-only by design —
  it exists to provide OS-backed I/O, time, and env. `no_std`
  embedders depend on `bop-lang` (and later `bop-vm`) directly and
  supply their own `BopHost`.
- JIT. See above.

## Open questions

- What's the smallest useful subset of the language for AOT v1? Probably:
  numbers, strings, arrays, dicts, functions, control flow, the common
  builtins. Closures can come later if the surface allows them cleanly.
- Exact VM per-op cost table. Straw-man: most ops = 1, `call` = 1 +
  arg-count, allocation ops charged on the value they build. Needs
  calibration against the tree-walker so `BopLimits::standard()` keeps
  roughly the same script wall-clock ceiling.
- How does `bop::memory` expose accounting to external engines
  without leaking internals? Today it uses thread-local / static
  counters via free functions (`bop_memory_init`, `bop_alloc`,
  `bop_dealloc`). Revisit if we want per-engine isolation: likely a
  small `MemoryTracker` type that both the tree-walker and VM hold a
  reference to.
