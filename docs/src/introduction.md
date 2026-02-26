# Welcome to Bop

Bop is a small, dynamically-typed programming language designed for embedding. It looks like real code — curly braces, functions, operators — but runs in a sandbox with bounded resources and no access to the filesystem or network.

Bop isn't Python, JavaScript, or any existing language, but it borrows familiar syntax so that the skills transfer. Variables, loops, functions, arrays, dictionaries — real programming concepts, zero setup.

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

- **Looks like real code** — curly braces, functions, operators. Skills transfer to real languages.
- **Friendly errors** — never cryptic, always helpful. (`"I don't know what 'pritn' is — did you mean 'print'?"`)
- **Sandboxed by design** — no imports, no file I/O, no network access. All resource usage is bounded.
- **Embeddable** — add custom functions and control execution through the `BopHost` trait.

## Where to start

If you're new to Bop, start with the **Basics** section and work your way through. If you're looking for a specific function or operator, jump straight to the **Reference** section. If you want to embed Bop in your own Rust project, see the **Embedding** chapter.
