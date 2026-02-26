# Welcome to Bop

Bop is a small, dynamically-typed programming language designed for teaching and embedding. It has a simple, modern syntax with no semicolons, no boilerplate, and no surprises — just the core concepts that matter. It runs in a sandbox with bounded resources and no access to the filesystem or network.

Bop isn't Python, JavaScript, or any existing language, but it deliberately borrows familiar syntax so that skills transfer directly to real-world languages. Variables, loops, functions, arrays, dictionaries — real programming concepts, zero setup. The library has no dependencies and also supports no-std and wasm.

## Quick example

```bop
// Sum the numbers from 1 to 10
let total = 0
for i in range(1, 11) {
  total += i
}
print("Sum: " + str(total))
```

## What makes Bop different?

- **Built for learning** — no semicolons, no boilerplate, simple syntax that teaches real programming concepts without getting in the way.
- **Looks like real code** — curly braces, functions, operators. Very similar to modern languages so that skills are directly transferable.
- **Friendly errors** — never cryptic, always helpful. (`"I don't know what 'pritn' is — did you mean 'print'?"`)
- **Sandboxed by design** — no imports, no file I/O, no network access. All resource usage is bounded.
- **Embeddable** — zero dependencies, wasm support, and the `BopHost` trait for adding custom functions and controlling execution.

## Where to start

If you're new to Bop, start with the **Basics** section and work your way through. If you're looking for a specific function or operator, jump straight to the **Reference** section. If you want to embed Bop in your own Rust project, see the **Embedding** chapter.
