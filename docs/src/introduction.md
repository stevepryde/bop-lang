# Welcome to Bop

Bop is a small, dynamically-typed programming language designed for teaching and embedding. It has a simple, modern syntax with no semicolons, no boilerplate, and no surprises — just the core concepts that matter. It runs in a sandbox with bounded resources and no filesystem or network access unless the embedding host opts in.

Bop isn't Python, JavaScript, or any existing language, but it deliberately borrows familiar syntax so that skills transfer directly to real-world languages. Variables, loops, functions, arrays, dictionaries, structs, enums, pattern matching, modules — real programming concepts, zero setup. The core library has no dependencies and supports no-std and wasm.

## Quick example

```bop
// Sum the numbers from 1 to 10
let total = 0
for i in range(1, 11) {
  total += i
}
print("Sum: {total}")
```

## What makes Bop different?

- **Built for learning** — no semicolons, no boilerplate, simple syntax that teaches real programming concepts without getting in the way.
- **Looks like real code** — curly braces, functions, structs, enums, pattern matching, modules. Skills transfer directly to real-world languages.
- **Friendly errors** — never cryptic, always helpful. Error messages include a source snippet with a carat under the offending column and a `hint:` line when the parser / runtime can guess what you meant (`"I don't know what 'pritn' is — did you mean 'print'?"`).
- **Sandboxed by default** — the core library never touches the filesystem, network, or the clock. Anything stateful or side-effecting goes through the `BopHost` trait; the embedder chooses what to expose.
- **Embeddable** — zero dependencies, wasm support, three engines (tree-walker, bytecode VM, AOT Rust transpiler) you can pick from per workload, and a `ReplSession` API for tools that want to drive Bop as a scripting layer.

## Where to start

If you're new to Bop, start with the **Basics** section and work your way through. If you're looking for a specific function or operator, jump straight to the **Reference** section. If you want to embed Bop in your own Rust project, see the **Embedding** chapter.
