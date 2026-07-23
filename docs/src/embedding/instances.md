# Stateful instances

`BopInstance` loads a program once and lets the host call its public entry
points repeatedly. It is the plugin-style counterpart to the one-shot
`bop::run` and `bop_vm::run` functions.

The instance retains the state produced while loading and by later calls:

- root and imported-module bindings;
- functions and returned callbacks;
- type and method declarations;
- module aliases and the import cache;
- the random-number generator state.

The tree-walker and VM expose the same API. Sandboxed AOT output generates an
equivalent API with the source already compiled into it.

## Declaring host entry points

Mark a direct root function with `pub` to include it in the instance ABI:

```bop
let count = 0

pub fn increment(by) {
  count += by
  return count
}

pub fn make_reader() {
  return fn() { return count }
}

fn private_helper() {
  return count
}
```

`pub fn` is only valid at the direct program root. It does not make a function
globally visible to ordinary Bop code, and `pub` declarations inside imported
modules are not root instance entries. It only opts the final executed root
declaration into the host-callable ABI.

Loading executes top-level code before the entry list is collected. Therefore:

- a declaration after a top-level `return` is not an entry;
- redeclaring a public name replaces its earlier ABI position and arity;
- a later private `fn` with the same name removes it from the ABI;
- `entry_points()` reports the final surviving entries in declaration order.

Ordinary Bop calls continue to use normal lexical name lookup. Host
`BopInstance::call` uses the dedicated public-entry table, so assigning another
value to an ordinary name cannot redirect the host ABI.

## Tree-walker instance

```rust
use bop::{BopHost, BopInstance, BopLimits, Value};

# struct Host;
# impl BopHost for Host {
#     fn call(&mut self, _: &str, _: &[Value], _: u32)
#         -> Option<Result<Value, bop::BopError>> { None }
# }
# fn main() -> Result<(), bop::BopError> {
let source = r#"
    let count = 0
    pub fn increment(by) {
        count += by
        return count
    }
    pub fn make_reader() {
        return fn() { return count }
    }
"#;

let mut host = Host;
let limits = BopLimits::standard();
let mut instance = BopInstance::load(source, &mut host, &limits)?;

for entry in instance.entry_points() {
    println!("{}/{}", entry.name(), entry.arity());
}

let first = instance.call("increment", &[Value::Int(2)], &mut host)?;
assert_eq!(first.inspect(), "2");

let reader = instance.call("make_reader", &[], &mut host)?;
instance.call("increment", &[Value::Int(3)], &mut host)?;
let current = instance.call_value(&reader, &[], &mut host)?;
assert_eq!(current.inspect(), "5");
# Ok(())
# }
```

`call` validates the public name and arity. `call_value` accepts a function
value created by that exact instance, including a callback returned by another
call.

## Bytecode VM instance

The VM is a drop-in replacement at this API boundary:

```rust
use bop::{BopHost, BopLimits, Value};
use bop_vm::BopInstance;

# struct Host;
# impl BopHost for Host {
#     fn call(&mut self, _: &str, _: &[Value], _: u32)
#         -> Option<Result<Value, bop::BopError>> { None }
# }
# fn main() -> Result<(), bop::BopError> {
let mut host = Host;
let mut instance = BopInstance::load(
    "let total = 0\npub fn add(n) { total += n; return total }",
    &mut host,
    &BopLimits::standard(),
)?;

assert_eq!(
    instance.call("add", &[Value::Int(4)], &mut host)?.inspect(),
    "4",
);
assert_eq!(
    instance.call("add", &[Value::Int(5)], &mut host)?.inspect(),
    "9",
);
# Ok(())
# }
```

Use `compile` plus `execute` when you want to reuse bytecode but intentionally
start with fresh program state on every execution. Use `BopInstance` when the
state itself must persist.

## Sandboxed AOT instances

The AOT transpiler emits a persistent `BopInstance` only when
`Options::sandbox` is enabled. Generate library-shaped Rust and compile it into
the host application:

```rust
use bop_compile::{Options, transpile};

let generated = transpile(
    "let count = 0\npub fn next() { count += 1; return count }",
    &Options {
        emit_main: false,
        use_bop_sys: false,
        sandbox: true,
        ..Options::default()
    },
)?;
```

The generated module provides:

```rust,ignore
let mut instance = BopInstance::load(&mut host, &limits)?;
let entries = instance.entry_points();
let value = instance.call("next", &[], &mut host)?;
let value = instance.call_value(&callback, &[], &mut host)?;
```

Because the Bop source is already compiled into the generated Rust,
`BopInstance::load` takes only `host` and `limits`, not a source string.
Unsandboxed output remains a one-shot `run` API and does not emit the
persistent instance surface.

Generated code also contains hygienically named convenience wrappers for
potential direct-root public declarations. They delegate to `call`, so the
runtime entry table remains authoritative when top-level control flow skips or
replaces a declaration.

## Hosts and re-entry

An instance borrows a `BopHost` only for `load` or one call; it never stores the
host. Later operations may use a different compatible host. This also keeps
host-owned allocations outside the instance's memory account.

The same instance cannot be re-entered while one of its operations is active.
For example, a host function called by instance A must not recursively call A.
It may call a different instance B, and B keeps independent state, limits, and
memory accounting.

Function values have instance affinity. Pass a callback back only to the
instance that created it; `call_value`, public entries, and callback-taking
builtins reject functions from another walker, VM, or generated AOT instance.

## Limits and failed calls

`load` and every later operation enforce the limits captured at load time:

- the step counter and fixed call-depth guard start fresh for each operation;
- tracked memory belongs to the instance and remains accounted across calls;
- returned values continue to charge the originating instance while they keep
  instance-owned storage alive.

A step or call-depth failure unwinds transient call frames and leaves the
instance callable again. Calls are not transactions: mutations completed
before an ordinary or fatal error remain visible to later calls.

Memory exhaustion is different because the retained state may itself still be
over budget. The instance continues returning a fatal memory error until
enough charged values are released. If an over-budget value was stored in a
persistent global, the instance can remain unusable.

## Instances versus REPL sessions

Use [`ReplSession`](../embedding.md#stateful-repl-sessions) when each interaction
introduces more source, as in a REPL or notebook. Use `BopInstance` when the
program is loaded once and exposes a deliberate host ABI through `pub fn`.
