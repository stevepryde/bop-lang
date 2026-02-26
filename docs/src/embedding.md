# Embedding Bop

Bop is designed to be embedded in Rust applications. You can add custom functions, capture output, and control execution through the `BopHost` trait.

## Quick start

Add `bop-lang` to your `Cargo.toml`:

```toml
[dependencies]
bop = { package = "bop-lang", version = "0.1" }
```

Run a program with the default host:

```rust
use bop::{run, StdHost, BopLimits};

fn main() {
    let source = r#"
        let name = "world"
        print("Hello, {name}!")
    "#;

    let mut host = StdHost;
    let limits = BopLimits::standard();

    if let Err(e) = run(source, &mut host, &limits) {
        eprintln!("Error: {e}");
    }
}
```

## The BopHost trait

The `BopHost` trait is the integration point between your application and the Bop interpreter:

```rust
pub trait BopHost {
    /// Called for unknown function names. Return `None` if not handled.
    fn call(
        &mut self,
        name: &str,
        args: &[Value],
        line: u32,
    ) -> Option<Result<Value, BopError>>;

    /// Called by `print()`. Default: writes to stdout.
    fn on_print(&mut self, message: &str) {
        println!("{}", message);
    }

    /// Hint text appended to "function not found" errors.
    fn function_hint(&self) -> &str {
        ""
    }

    /// Called each tick (statement, loop iteration). Return Err to halt.
    fn on_tick(&mut self) -> Result<(), BopError> {
        Ok(())
    }
}
```

### `call` — Custom functions

This is the primary extension point. When Bop encounters a function call that isn't a built-in or user-defined function, it calls `host.call()`. Return `None` if you don't handle the function name — Bop will report a "function not found" error. Return `Some(Ok(value))` on success, or `Some(Err(error))` to raise a runtime error.

```rust
use bop::{BopHost, BopError, Value};

struct MyHost;

impl BopHost for MyHost {
    fn call(
        &mut self,
        name: &str,
        args: &[Value],
        line: u32,
    ) -> Option<Result<Value, BopError>> {
        match name {
            "square" => {
                if args.len() != 1 {
                    return Some(Err(BopError::runtime(
                        "square() takes 1 argument", line
                    )));
                }
                match &args[0] {
                    Value::Number(n) => Some(Ok(Value::Number(n * n))),
                    _ => Some(Err(BopError::runtime(
                        "square() requires a number", line
                    ))),
                }
            }
            _ => None, // not handled
        }
    }
}
```

Bop scripts can then call `square()` as if it were built-in:

```bop
let result = square(5)
print(str(result))    // 25
```

### `on_print` — Capturing output

Override `on_print` to redirect `print()` output to a buffer, log, UI widget, or anywhere else:

```rust
struct BufferedHost {
    output: Vec<String>,
}

impl BopHost for BufferedHost {
    fn call(&mut self, _: &str, _: &[Value], _: u32)
        -> Option<Result<Value, BopError>>
    {
        None
    }

    fn on_print(&mut self, message: &str) {
        self.output.push(message.to_string());
    }
}
```

### `on_tick` — Execution control

`on_tick` is called on every interpreter step (each statement, loop iteration, etc.). Use it for:

- **Timeouts** — check elapsed time and halt if exceeded
- **Cancellation** — check a flag set by another thread
- **Progress tracking** — count steps or update a progress bar

```rust
use std::time::{Duration, Instant};

struct TimedHost {
    start: Instant,
    timeout: Duration,
}

impl BopHost for TimedHost {
    fn call(&mut self, _: &str, _: &[Value], _: u32)
        -> Option<Result<Value, BopError>>
    {
        None
    }

    fn on_tick(&mut self) -> Result<(), BopError> {
        if self.start.elapsed() > self.timeout {
            Err(BopError::runtime("execution timed out", 0))
        } else {
            Ok(())
        }
    }
}
```

### `function_hint` — Better error messages

Return a hint string that gets appended to "function not found" errors. This helps guide users toward the functions your host provides:

```rust
fn function_hint(&self) -> &str {
    "Available functions: square(), cube(), sqrt()"
}
```

## Resource limits

`BopLimits` controls how much work a script can do:

```rust
pub struct BopLimits {
    pub max_steps: u64,      // loop iterations, statements, etc.
    pub max_memory: usize,   // bytes for strings + arrays
}
```

Two presets are provided:

| Preset | `max_steps` | `max_memory` |
|--------|-------------|--------------|
| `BopLimits::standard()` | 10,000 | 10 MB |
| `BopLimits::demo()` | 1,000 | 1 MB |

Or create your own:

```rust
let limits = BopLimits {
    max_steps: 50_000,
    max_memory: 32 * 1024 * 1024, // 32 MB
};
```

When a limit is exceeded, `run()` returns a `BopError` — the script is halted cleanly without panicking.

## Putting it all together

Here's a complete example of a host that provides domain-specific functions, captures output, and enforces a timeout:

```rust
use bop::{run, BopHost, BopError, BopLimits, Value};
use std::time::{Duration, Instant};

struct AppHost {
    output: Vec<String>,
    start: Instant,
    data: Vec<f64>,
}

impl BopHost for AppHost {
    fn call(
        &mut self,
        name: &str,
        args: &[Value],
        line: u32,
    ) -> Option<Result<Value, BopError>> {
        match name {
            "add_data" => {
                if let Some(Value::Number(n)) = args.first() {
                    self.data.push(*n);
                    Some(Ok(Value::None))
                } else {
                    Some(Err(BopError::runtime(
                        "add_data() requires a number", line
                    )))
                }
            }
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
            Err(BopError::runtime("timed out", 0))
        } else {
            Ok(())
        }
    }

    fn function_hint(&self) -> &str {
        "Available: add_data(n), average()"
    }
}

fn main() {
    let source = r#"
        for n in [10, 20, 30, 40, 50] {
            add_data(n)
        }
        print("Average: " + str(average()))
    "#;

    let mut host = AppHost {
        output: vec![],
        start: Instant::now(),
        data: vec![],
    };

    match run(source, &mut host, &BopLimits::standard()) {
        Ok(()) => {
            for line in &host.output {
                println!("{line}");
            }
        }
        Err(e) => eprintln!("Script error: {e}"),
    }
}
```
