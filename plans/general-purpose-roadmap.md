# Bop → General-Purpose Language Roadmap

Status: design / roadmap. `plans/execution-modes.md` covers how Bop
programs are *executed*; this document covers how Bop becomes rich
enough to *write non-trivial programs in*. The execution-modes
roadmap is essentially complete (three engines, shared differential
harness); this one is the next multi-phase effort.

## Goal

Take Bop from "well-engineered embedded scripting language" to
"general-purpose language you could actually build an app in" —
without breaking the thing that makes it worth embedding.

By the end of this roadmap a user should be able to write:

- A multi-file CLI tool with subcommands, error handling, and a
  stdlib it can lean on (`math`, `iter`, `string`, `json`, `test`).
- A small data-processing script that reads files, transforms
  records, and reports results, using closures and structs to
  model data cleanly.
- A simple web scraper or config parser where the structure of the
  program — imports, modules, user-defined types — looks and feels
  like something written in Python or Lua, not like a shell script.

## The non-negotiable invariant

> **`bop-lang` (the `bop` crate) stays zero-dep and embeddable.**

Every phase below is gated by that constraint. Anything that needs
filesystem access, network, a package registry, OS time, FFI, or a
Rust dependency beyond `core`/`alloc` goes into a separate crate
(`bop-sys`, `bop-std`, `bop-pkg`, …) or behind a `BopHost` hook
that the embedder supplies.

If a proposed feature genuinely cannot be implemented without
violating that invariant, the feature gets deferred or redesigned.
This is a hard line, not a soft preference.

### How the constraint expresses itself

- Core never reads files, opens sockets, or touches the clock.
- Core never pulls in a Rust crate outside the workspace (no
  `serde`, no `regex`, no `indexmap`). `alloc` only; `std` gated
  behind the existing feature flag.
- Language features that need I/O (imports, stdlib with OS access)
  are implemented as `BopHost` trait methods, with OS-backed
  default impls living in `bop-sys`.
- Standard library content that can be written in Bop itself
  (iterator helpers, math, assertions) lives in `bop-std` as
  bundled source strings, loaded through the module system —
  not compiled into core.

Practical test when adding a phase: *could a `no_std` embedder
pull just `bop` and a tiny custom host and still use this
feature?* If not, the feature needs to sit outside core.

## Target crate landscape

Current:

```
bop         core language: lexer, parser, AST, tree-walker, Value,
            ops, builtins, methods, memory tracking, BopHost trait.
            Zero deps, no_std-capable.
bop-vm      bytecode compiler + stack VM (depends on bop)
bop-sys     standard host: stdio, fs, env, time (std-only)
bop-compile AOT Bop → Rust transpiler (depends on bop)
bop-cli     CLI driver (run, repl; build coming with bop-compile CLI)
```

Additions this roadmap calls for:

```
bop-std     standard library written in Bop source; bundled as
            string assets, loaded through the module system.
            Zero OS dependencies — the embedder chooses whether
            to load it via BopHost::resolve_module.
bop-pkg     (far future) package manager CLI + registry client.
            Separate plan doc when it lands.
```

## Phases

Phases are listed in dependency order. Each is independently
shippable, differential-tested across walker / VM / AOT, and
documented in `plans/execution-modes.md`-style "status: done" blocks
once it lands.

A useful mental split:

- **Phases 1–6** constitute the "MVP general-purpose" set. After
  phase 6 lands, Bop is a small-but-real general-purpose language.
- **Phases 7–9** are quality-of-life and ecosystem work on top.

### Phase 1 — Closures and first-class functions

**Why first.** Everything downstream assumes functions are values.
Iterators, callbacks, the stdlib's `map` / `filter` / `reduce`,
event handlers in embedders, higher-order patterns in user code —
none of it works cleanly without closures. Closures are also the
single biggest reason the current language feels toy-like.

**Scope.**

- New `Value::Fn` variant carrying `params`, compiled body, and a
  captured-environment snapshot.
- Function declarations become sugar: `fn foo(x) { ... }` lowers
  to `let foo = <fn-value>`. Named-only calls (`foo(x)`) keep
  working; value-calls (`let g = foo; g(x)`) start working too.
- Lambda syntax. Prefer `fn(x) { ... }` for consistency with
  declared fns (no new token types). Reconsider `|x| x * 2` later
  if the noise bothers anyone.
- Lexical capture: a function captures the variables it references
  from the enclosing scope at the moment it's constructed, by
  value (clone). Mutations to captured vars inside the closure
  don't propagate back out — same call-by-value model Bop already
  uses for function parameters.
- Call dispatch is extended: the `Call { callee, args }` path now
  accepts any expression that evaluates to `Value::Fn`, not just a
  named identifier. Host and builtin dispatch still resolve by
  name as today.

**Where it lives.** `bop-lang` core. `bop-vm` needs a new opcode
(`MakeClosure(fn_idx)`) plus a call-indirect dispatch. `bop-compile`
emits a Rust closure (`Box<dyn Fn(...) -> Result<Value, BopError>>`
or a generated fn per lambda).

**Non-goals.** No currying, no partial application, no rest
parameters. `self` and method binding stay tied to method-call
syntax.

**Risks.** The VM's `Rc<Chunk>` model interacts with capturing —
a closure holds its body and an env snapshot; cloning the Value
must be cheap (envs use `Rc` internally).

### Phase 2 — Modules and imports

**Why second.** Multi-file programs are table stakes. Closures
without modules would still leave users writing 2000-line single
files.

**Scope.**

- Extension to `BopHost`:
  ```rust
  fn resolve_module(&mut self, name: &str) -> Option<Result<String, BopError>>;
  ```
  Default impl returns `None`. Embedders supply their own — file
  system, embedded string assets, network fetch, whatever fits.
- Core parser: `import foo` / `import foo.bar` / `from foo import x`.
  Names are identifier-dotted strings.
- Module caching: repeated imports of the same name resolve once.
- Modules are Bop source files; loading one parses it, evaluates
  its top-level statements in a fresh module scope, and exposes
  its public bindings under the module name.
- What "public" means: start simple — every top-level `let` and
  `fn` in a module is public. Add a `pub` / `priv` distinction
  later if we need it (probably we will).
- `bop-sys` adds a filesystem resolver that maps `foo.bar` →
  `./foo/bar.bop` relative to a configurable root.

**Where it lives.** Parser + evaluator changes go in `bop-lang`.
The `BopHost` trait (also in `bop-lang`) gains the new method —
core stays zero-dep because it doesn't *implement* resolution, it
just declares the hook. `bop-sys` gets the filesystem impl.

**Non-goals.** No circular import magic (error out); no version
selection (that's the package manager); no re-exports (can add
later).

**Risks.** Module scoping intersects with closures — a module-
level `fn` captures module-level state. Design must be clear that
module scope is its own lexical environment.

### Phase 3 — User-defined types (structs)

**Why third.** Dicts-with-convention simulate structs badly. Real
data modeling wants named fields, a type identity, and methods
that know their receiver's type at a glance. This also unlocks
cleaner stdlib designs later — `Result`, `Option`, iterators,
sets, queues.

**Scope.**

- Syntax:
  ```
  struct Point { x, y }

  fn Point.distance(self, other) {
      let dx = self.x - other.x
      let dy = self.y - other.y
      return math.sqrt(dx * dx + dy * dy)
  }
  ```
- Construction: `Point { x: 1, y: 2 }`. Positional `Point(1, 2)`
  can come later.
- Field access via `.`: distinct from dict indexing. `foo.x`
  always resolves by name at parse time; `foo["x"]` works for
  dicts.
- New `Value::Struct { type_name, fields }`.
- Method resolution: `foo.bar(args)` looks up `bar` on the
  struct's type first, then falls back to the built-in method
  dispatch (arrays / strings / dicts).

**Where it lives.** Core. Three engines follow.

**Non-goals.** No inheritance. No traits or interfaces. No
generics. Methods are just regular functions keyed by `TypeName`.
Keep it dead simple.

**Risks.** Pattern matching (phase 7) will want struct
destructuring; make sure the `Value::Struct` shape works for it
without retrofitting.

### Phase 4 — Exception handling

**Why fourth.** With modules and structs, programs get large
enough that "error aborts the whole thing" becomes painful.
Exceptions let libraries signal recoverable failures without the
caller having to pre-check everything.

**Scope.**

- `try { ... } catch err { ... }` statement form. `err` binds the
  thrown value (a `Value`) in the catch block's scope.
- `throw <expr>` raises the value. The value is conventionally a
  dict or a struct, but Bop doesn't enforce a shape.
- Optional `finally { ... }` clause.
- Integration with existing `BopError`: runtime errors produced by
  ops / builtins / methods become catchable — they materialise as
  a struct (say, `Error { message, line }`) when a `try` block is
  active. Uncaught errors still abort, same as today.

**Where it lives.** Core. Three engines follow.

**Non-goals.** No typed exception hierarchies. No resumable
exceptions. No stack traces on the exception value (yet —
revisit when/if debugging gets attention in phase 9).

**Risks.** VM integration — unwinding to the nearest `try` frame
means tracking frame depth at the catch site. AOT emits Rust
`Result` chains; maps onto `?` reasonably cleanly.

### Phase 5 — Integer type

**Why fifth.** `f64`-only arithmetic bites real use cases: bit
twiddling, array indices past 2^53, any domain where
`3.0000000001` surprises you. Independent of the above phases, so
it can move in parallel, but cheaper to do after the semantic
surface stabilises so we only refit `ops` / `methods` / `builtins`
once.

**Scope.**

- New `Value::Int(i64)` variant alongside existing
  `Value::Number(f64)`.
- Numeric literals: `42` → `Int`, `42.0` → `Number`.
- Ops:
  - `Int op Int` → `Int` (overflow → wrapping? panic? error? —
    pick one and document; lean toward `BopError`).
  - `Int op Number` and `Number op Int` → `Number` (Int widens).
  - Integer division: new `//` operator, or have `/` split
    (`/` always returns Number, `//` returns Int). Pick the
    latter — matches Python.
- Builtins: `int()` keeps truncating semantics; new `float()`
  coerces up. `type()` returns `"int"` or `"number"` accordingly.
- String parsing: `int("42")` returns Int; `float("3.14")` returns
  Number.

**Where it lives.** Core. Breaks tree-walker tests that assert
`type(42) == "number"`; fine, those get updated as part of the
phase.

**Non-goals.** No `i32` / `u64` / unsigned variants. No bigints.
No arbitrary precision. One integer type.

**Risks.** Every single `f64` pattern match in `ops.rs`,
`methods.rs`, `builtins.rs` needs an `Int` arm. Tedious; no
architectural risk.

### Phase 6 — Standard library (`bop-std`)

**Why sixth.** Everything above enables it, and it's the thing
that turns "Bop can do it" into "Bop ships with it". Phases 1–5
produce the language; phase 6 produces the library.

**Scope.**

- New crate `bop-std`. Contents are Bop source files bundled as
  `&'static str` constants (or loaded via `include_str!`).
- The crate exposes one function: roughly
  ```rust
  pub fn resolve(name: &str) -> Option<&'static str>;
  ```
  which maps module names (`"std.iter"`, `"std.math"`, …) back
  to the bundled source.
- Embedders opt in by chaining `bop-std::resolve` into their
  `BopHost::resolve_module` impl. `bop-sys`'s default host adds
  this chain so `bop-cli` users get stdlib imports for free.
- Proposed modules for v1:
  - `std.math` — `sqrt`, `sin`, `cos`, `abs`, `pi`, `e`, `floor`,
    `ceil`, `round`, `pow`. (Some delegate to built-ins that
    wrap `f64::*` — those built-ins live in core.)
  - `std.iter` — `map`, `filter`, `reduce`, `take`, `drop`,
    `zip`, `enumerate`, `sum`, `product`, `any`, `all`, `find`.
    Operates on arrays; returns arrays (no lazy iterator
    protocol in v1).
  - `std.string` — `pad_left`, `pad_right`, `repeat`, `chars`,
    helpers that don't fit the method-on-string pattern.
  - `std.collections` — `Set`, `Queue`, `Stack` as struct types
    wrapping arrays/dicts.
  - `std.test` — `assert`, `assert_eq`, `assert_near`, and a
    tiny `test("name") { ... }` runner. Enables Bop programs to
    self-test.
  - `std.json` — `parse(str)` and `stringify(value)`. Needs
    thought once structs land.

**Where it lives.** `bop-std` crate. Zero Rust deps beyond
`bop-lang` (and only for the resolver signature — no runtime
dep). Core stays untouched.

**Non-goals.** No I/O in `bop-std` (goes through bop-sys hooks).
No date/time (bop-sys or a new bop-time crate). No regex (probably
its own crate — regex engines are non-trivial).

**Risks.** None to core. The main risk is bikeshedding naming and
APIs; enforce "write idiomatic Bop first, Rust library influence
second".

### — Checkpoint: "MVP general purpose" reached —

After phase 6 Bop has: closures, modules, structs, exceptions, an
integer type, and a standard library. A competent developer can
write a non-trivial program in it. The core crate is still
zero-dep embeddable. The remaining phases are quality-of-life.

### Phase 7 — Pattern matching

**Scope.**

- `match expr { pattern => expr, ... }` — expression form.
- Patterns: literals, wildcards (`_`), variable bindings, array
  (`[a, b, ..rest]`), struct (`Point { x, y }`, `Point { x, .. }`),
  or patterns (`1 | 2 | 3`), and guards (`x if x > 0 => ...`).
- Compiled to nested `if`/`else` chains in the walker; a small
  decision tree in the VM and AOT.

**Where it lives.** Core.

**Non-goals.** No range patterns for v1 (`1..10`). No custom
matcher protocol. No exhaustiveness checking (nice to have; not
mandatory for a dynamic language).

### Phase 8 — Package manager (`bop-pkg`)

Its own plan doc when the time comes. Rough shape:

- `bop-pkg` CLI: `bop install foo`, `bop publish`.
- `bop.toml` manifest at the root of a project: name, version,
  dependencies.
- Start with a single git-based registry, no central index.
- Install puts sources in `./.bop/modules/`; `bop-sys`'s fs
  resolver checks there first, then the project root.

None of this touches core. It's tooling plus convention.

### Phase 9 — Polish

Ongoing, not a discrete phase:

- Documentation — tutorial, reference, `bop-std` API docs, a
  landing page. Without docs, none of the above matters to
  anyone who isn't already in the codebase.
- Better parse / runtime errors (line spans, source pointers).
- REPL: multi-line input, history, tab completion.
- Language server (far future; may never happen).
- Debugging: tracebacks on caught exceptions, a `debug()` hook
  for embedders.
- Performance pass once the feature set is stable: VM dispatch
  tuning, Value cloning reduction, inline caches.

## Deliberately out of scope

Keeping this list is as important as the roadmap itself.

- **Static typing.** Bop is dynamic. If static typing becomes
  interesting, it's an entirely separate language; don't bolt it
  on.
- **Macros / metaprogramming.** Adds complexity; the niche Bop
  fills doesn't need it.
- **JIT.** Already rejected in `plans/execution-modes.md`. AOT
  covers "I want native speed".
- **Concurrency / async / threading.** Out of scope at the
  language level. Embedders already drive the host; if they want
  concurrency, they run multiple engines on multiple threads.
- **FFI to arbitrary C.** The `BopHost` trait *is* the FFI. If
  you want to call C, expose it through a host method.
- **GC beyond reference counting.** The current Value model
  (Clone/Drop with tracked allocations) is fine for the
  workloads Bop targets.
- **Breaking the embeddable invariant for convenience.** Ever.

## Open questions

- **Closure representation in the VM.** `Value::Fn` needs to
  carry the captured env. `Rc<Vec<(String, Value)>>` works but
  clones the whole env on every `Fn` value construction. Consider
  a flat slot vec indexed at compile time for speed.
- **Integer vs Number default for comparison.** Should
  `1 == 1.0` be `true` or `false`? Current Bop says `1 == "1"`
  is `false`; consistency argues `Int(1) == Number(1.0)` is also
  `false`. Users might expect `true`. Decide before shipping
  phase 5.
- **Exception payload shape.** Struct? Dict? Both? Probably a
  built-in `Error` struct with `message` + `line` + optional
  `cause` field, constructed by the runtime when internal errors
  become catchable.
- **Module resolution timing.** Parse-time (all imports resolved
  before any code runs) or run-time (imports resolved when
  executed)? Python does the latter, which enables lazy /
  conditional imports but makes error diagnostics worse. Lean
  toward parse-time for clarity; revisit if it bites.
- **Should `bop-std` modules be in `bop-sys`'s default resolver
  chain, or opt-in?** Default-on is friendlier; opt-in is purer.
  Lean default-on in `bop-cli`, opt-in for direct embedders.

## Dependency graph between phases

```
Phase 1 (closures) ─┬─> Phase 6 (stdlib) ──> Phase 9 (polish)
                    ├─> Phase 2 (modules) ─┤
                    └─> Phase 4 (exceptions)
                                           │
Phase 3 (structs) ──────────────────────────┼─> Phase 7 (match)
                                           │
Phase 5 (integer type) ────────────────────┘

Phase 8 (package manager) depends on phase 2 (modules) and
the existence of a stdlib (phase 6), but is otherwise orthogonal.
```

Phases 1–5 can be mostly tackled in order. Phase 5 (integer type)
can slot in parallel to any of 2, 3, or 4 since it's orthogonal.
Phase 6 wants everything below it green before it starts.

## How each phase gets shipped

Follow the pattern the execution-modes roadmap established:

1. Design note in this document (update the phase's scope block
   with concrete decisions).
2. Land the feature in the tree-walker first, with walker-only
   tests in `bop/src/lib.rs`.
3. Extend the VM and the AOT transpiler.
4. Add the 2c differential test cases so walker + VM agree.
5. Add 3-way corpus entries where feasible.
6. Update `plans/general-purpose-roadmap.md` with a "Status: done"
   block under the phase (same pattern as `plans/execution-modes.md`).
7. Single commit to `main`, following the repo's direct-push
   style.

Safety / resource-limit semantics carry over unchanged — every new
feature must respect `BopLimits` and go through the existing
tick / memory hooks in all three engines.
