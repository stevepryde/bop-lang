# Embedding Bop

Bop is designed to be embedded in Rust applications. Three ways to run a program are available; every one uses the same `BopHost` trait as the integration seam for print output, custom functions, module resolution, timeouts, etc.

## Quick start

Add the crates you need to `Cargo.toml`:

```toml
[dependencies]
bop = { package = "bop-lang", version = "0.3" }
bop-sys = "0.3"          # optional — OS-backed standard host
bop-vm = "0.3"           # optional — bytecode VM (faster per-fn cost)
```

Run a program with the standard host:

```rust
use bop::BopLimits;
use bop_sys::StandardHost;

fn main() {
    let source = r#"
        let name = "world"
        print("Hello, {name}!")
    "#;

    let mut host = StandardHost::new();

    if let Err(e) = bop::run(source, &mut host, &BopLimits::standard()) {
        eprintln!("{}", e.render(source));
    }
}
```

`StandardHost` (aliased as `StdHost`) lives in `bop-sys` and is the OS-backed reference host. It supports `print()` output plus these host functions:

| Function | Description |
|----------|-------------|
| `readline()` / `readline(prompt)` | Read one line from stdin; `none` at EOF |
| `read_file(path)` | Read a UTF-8 file into a string |
| `write_file(path, contents)` | Replace a file with string contents |
| `append_file(path, contents)` | Append string contents to a file |
| `file_exists(path)` | `true` / `false` |
| `env(name)` | Environment variable, or `none` if missing |
| `unix_time()` | Seconds since the epoch |
| `unix_time_ms()` | Milliseconds since the epoch |

Call `StandardHost::new().with_module_root(<path>)` to enable filesystem module resolution: `use foo.bar.baz` maps to `<root>/foo/bar/baz.bop` with path-traversal guards.

## Three engines, same host

Bop ships three execution engines — all of them share the `BopHost` trait, `BopError`, `BopLimits`, and `Value` types, so the same program and host work with any of them. Pick whichever fits your workload:

| Engine | Entry point | When it's best |
|--------|-------------|----------------|
| Tree-walker | `bop::run(src, host, limits)` | Lowest start-up cost. Best for one-off scripts, REPL, small inputs, no_std / wasm. |
| Bytecode VM | `bop_vm::run(src, host, limits)` | 2–3× faster than the walker on hot loops. Same program, no compilation to disk. |
| AOT transpiler | `bop_compile::transpile(src, opts)` → `cargo build` | Bop → Rust source, compiled to a native binary. Maximum throughput, at the cost of a `cargo build` step. |

All three obey the same `BopLimits` and surface errors as `BopError` — the engine choice is an implementation detail from the host's perspective.

## The `BopHost` trait

The `BopHost` trait is the integration point between your application and Bop:

```rust
pub trait BopHost {
    /// Called for unknown function names. `None` = not handled.
    fn call(
        &mut self,
        name: &str,
        args: &[Value],
        line: u32,
    ) -> Option<Result<Value, BopError>>;

    /// Called by `print()`. Default: drops the message.
    fn on_print(&mut self, message: &str) { let _ = message; }

    /// Appended to "function not found" errors as a
    /// friendly hint (e.g. "Available functions: ...").
    fn function_hint(&self) -> &str { "" }

    /// Called at each interpreter tick (statement, loop
    /// iteration, fn entry). Return `Err` to halt.
    fn on_tick(&mut self) -> Result<(), BopError> { Ok(()) }

    /// Resolve a `use` target to Bop source.
    /// `None` = "not mine" (the runtime raises "module not
    /// found"); `Some(Err(e))` = resolver failed (propagated).
    fn resolve_module(&mut self, name: &str)
        -> Option<Result<String, BopError>>
    { let _ = name; None }
}
```

### `call` — Custom functions

The primary extension point. When Bop encounters a function call that isn't a built-in or user-defined fn, it asks the host. Return `None` if you don't handle the name — Bop then surfaces "function not found" with the optional `function_hint`. Return `Some(Ok(v))` on success or `Some(Err(e))` to raise a runtime error.

```rust
use bop::{BopError, BopHost, Value};

struct MyHost;

impl BopHost for MyHost {
    fn call(
        &mut self,
        name: &str,
        args: &[Value],
        line: u32,
    ) -> Option<Result<Value, BopError>> {
        match name {
            "square" => match args {
                [Value::Int(n)]    => Some(Ok(Value::Int(n * n))),
                [Value::Number(n)] => Some(Ok(Value::Number(n * n))),
                _ => Some(Err(BopError::runtime(
                    "square(n) expects one number", line
                ))),
            },
            _ => None,
        }
    }

    fn function_hint(&self) -> &str {
        "Custom functions: square(n)"
    }
}
```

Bop scripts now call `square(5)` as if it were built-in.

### `on_print` — Capturing output

Override `on_print` to redirect `print()` output to a buffer, log, UI widget — anywhere that isn't stdout:

```rust
struct Buffered { output: Vec<String> }

impl BopHost for Buffered {
    fn call(&mut self, _: &str, _: &[Value], _: u32)
        -> Option<Result<Value, BopError>> { None }
    fn on_print(&mut self, msg: &str) { self.output.push(msg.into()); }
}
```

### `resolve_module` — Custom `use` resolution

Supply module source for `use path.to.module` statements. Return:
- `Some(Ok(source))` — the module's source text (Bop parses and executes it).
- `Some(Err(err))` — resolver error; propagated to the user.
- `None` — "not my module"; Bop raises "module `foo` not found".

```rust
impl BopHost for MyHost {
    fn resolve_module(&mut self, name: &str)
        -> Option<Result<String, BopError>>
    {
        match name {
            "greetings" => Some(Ok(r#"
                fn hello(who) { return "hi " + who }
            "#.into())),
            _ => None,
        }
    }
    // ... call, etc.
}
```

`bop::host::resolve_from_map` and `bop::host::StringModuleHost` (below) are ready-made helpers that cover the common "in-memory module table" pattern.

### `on_tick` — Execution control

Called on every tick — fn entry, loop iteration, most statements. Use it for:

- **Timeouts** — check elapsed time and halt.
- **Cancellation** — read a `&AtomicBool` set by another thread.
- **Progress tracking** — increment a counter or refresh a progress bar.

```rust
use std::time::{Duration, Instant};

struct Timed { start: Instant, budget: Duration }

impl BopHost for Timed {
    fn call(&mut self, _: &str, _: &[Value], _: u32)
        -> Option<Result<Value, BopError>> { None }
    fn on_tick(&mut self) -> Result<(), BopError> {
        if self.start.elapsed() > self.budget {
            Err(BopError::runtime("execution timed out", 0))
        } else {
            Ok(())
        }
    }
}
```

`on_tick` errors count as runtime errors — they can be caught by `try_call` inside Bop. Use `BopError::fatal` instead if you need the halt to be uncatchable:

```rust
fn on_tick(&mut self) -> Result<(), BopError> {
    if cancel_flag.load(Ordering::Relaxed) {
        Err(BopError::fatal("cancelled", 0))   // `try_call` won't swallow this
    } else {
        Ok(())
    }
}
```

## Resource limits

`BopLimits` controls how much work a program can do before it's killed with a fatal error:

```rust
pub struct BopLimits {
    pub max_steps: u64,      // tick budget
    pub max_memory: usize,   // bytes for strings + arrays + structs
}
```

Two presets:

| Preset | `max_steps` | `max_memory` |
|--------|-------------|--------------|
| `BopLimits::standard()` | 10,000 | 10 MB |
| `BopLimits::demo()` | 1,000 | 1 MB |

Custom:

```rust
let limits = BopLimits {
    max_steps: 50_000,
    max_memory: 32 * 1024 * 1024,
};
```

Limit violations are **fatal** — `try_call` in user code can't swallow them.

## Ready-made host helpers

`bop::host` bundles the two most common host shapes so embedders don't have to hand-roll them.

### `bop::host::resolve_from_map(entries)`

Build a `resolve_module`-compatible closure from any iterable of `(name, source)` pairs. Drop it inside your own `BopHost` impl:

```rust
use bop::host::resolve_from_map;

struct MyHost { resolve: Box<dyn Fn(&str) -> Option<Result<String, BopError>>> }

impl MyHost {
    fn new() -> Self {
        let resolve = resolve_from_map([
            ("greetings", "fn hello() { return \"hi\" }"),
            ("math_ext",  "fn sq(n) { return n * n }"),
        ]);
        Self { resolve: Box::new(resolve) }
    }
}

impl BopHost for MyHost {
    // ...
    fn resolve_module(&mut self, name: &str)
        -> Option<Result<String, BopError>>
    { (self.resolve)(name) }
}
```

### `bop::host::StringModuleHost`

A minimal full `BopHost` implementation — captures prints to an in-memory vec and resolves modules from a string map. Useful for tests and playgrounds:

```rust
use bop::host::StringModuleHost;
use bop::BopLimits;

let mut host = StringModuleHost::new([
    ("greetings", "fn hello(who) { return \"hi \" + who }"),
]);

bop::run(
    r#"use greetings
print(hello("Bop"))"#,
    &mut host,
    &BopLimits::standard(),
).unwrap();

assert_eq!(host.output(), "hi Bop");
```

## Stateful REPL sessions

`bop::ReplSession` carries `let` bindings, `fn` declarations, user types, methods, module aliases, and the import cache across `eval` calls. Use it when you want "one Bop interpreter, many user inputs" — interactive REPLs, notebook cells, per-request scripting.

```rust
use bop::{BopLimits, ReplSession};
use bop_sys::StandardHost;

let mut session = ReplSession::new();
let mut host = StandardHost::new();

session.eval("let x = 5", &mut host, &BopLimits::standard()).unwrap();
session.eval("let y = x + 3", &mut host, &BopLimits::standard()).unwrap();

// `eval` returns `Ok(Some(v))` when the last statement is a bare
// expression; `Ok(None)` for `let` / `fn` / `use` / etc.
let r = session.eval("y * 2", &mut host, &BopLimits::standard()).unwrap();
assert!(matches!(r, Some(bop::Value::Int(16))));

// Introspection.
assert!(session.get("x").is_some());
assert_eq!(session.binding_names(), vec!["x".to_string(), "y".to_string()]);
```

Each `eval` still respects the `BopLimits` you pass — useful if you want to allow a higher step budget per cell than for a batch-run program. The built-in `bop` CLI's `repl` subcommand is the canonical consumer: it adds rustyline, multi-line input, `:help` / `:vars` / `:reset` / `:quit` meta-commands, tab completion, and a persistent history file. See [REPL](repl.md) for the user-facing view.

## Error rendering

`BopError::render(source)` produces a terminal-friendly error with a source snippet and a `^` carat under the offending column (when the error carries column info). Parse errors always have columns; runtime errors do when the failing expression was parsed from source.

```rust
match bop::run(src, &mut host, &BopLimits::standard()) {
    Ok(()) => {}
    Err(e) => eprintln!("{}", e.render(src)),
}
```

Typical output:

```
error: Variable `undefined` not found
  --> line 2:7
  |
2 | print(undefined)
  |       ^
hint: Did you forget to create it with `let`?
```

## Putting it all together

A complete host that provides domain-specific functions, captures output, resolves modules from memory, and enforces a timeout:

```rust
use bop::{BopError, BopHost, BopLimits, Value};
use bop::host::resolve_from_map;
use std::time::{Duration, Instant};

struct AppHost {
    output: Vec<String>,
    start: Instant,
    data: Vec<f64>,
    resolve: Box<dyn Fn(&str) -> Option<Result<String, BopError>>>,
}

impl AppHost {
    fn new() -> Self {
        let resolve = resolve_from_map([
            ("stats_helpers", r#"
                fn median(xs) {
                    let sorted = xs
                    sorted.sort()
                    let mid = int(len(sorted) / 2)
                    return sorted[mid]
                }
            "#),
        ]);
        Self {
            output: vec![],
            start: Instant::now(),
            data: vec![],
            resolve: Box::new(resolve),
        }
    }
}

impl BopHost for AppHost {
    fn call(
        &mut self,
        name: &str,
        args: &[Value],
        line: u32,
    ) -> Option<Result<Value, BopError>> {
        match name {
            "add_data" => match args {
                [Value::Int(n)]    => { self.data.push(*n as f64); Some(Ok(Value::None)) }
                [Value::Number(n)] => { self.data.push(*n);       Some(Ok(Value::None)) }
                _ => Some(Err(BopError::runtime("add_data(n) expects a number", line))),
            },
            "average" => {
                if self.data.is_empty() {
                    Some(Ok(Value::Number(0.0)))
                } else {
                    let sum: f64 = self.data.iter().sum();
                    Some(Ok(Value::Number(sum / self.data.len() as f64)))
                }
            }
            _ => None,
        }
    }

    fn on_print(&mut self, message: &str) {
        self.output.push(message.to_string());
    }

    fn on_tick(&mut self) -> Result<(), BopError> {
        if self.start.elapsed() > Duration::from_secs(5) {
            Err(BopError::fatal("timed out", 0))
        } else {
            Ok(())
        }
    }

    fn function_hint(&self) -> &str {
        "Custom host: add_data(n), average()"
    }

    fn resolve_module(&mut self, name: &str)
        -> Option<Result<String, BopError>>
    { (self.resolve)(name) }
}

fn main() {
    let source = r#"
        use stats_helpers
        for n in [10, 20, 30, 40, 50] {
            add_data(n)
        }
        let avg = average()
        let mid = median([10, 20, 30, 40, 50])
        print("Average: " + str(avg) + ", median: " + str(mid))
    "#;

    let mut host = AppHost::new();
    match bop::run(source, &mut host, &BopLimits::standard()) {
        Ok(()) => {
            for line in &host.output { println!("{}", line); }
        }
        Err(e) => eprintln!("{}", e.render(source)),
    }
}
```
