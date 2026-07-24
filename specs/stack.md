# Stack

## Committed choices

- **Language:** Rust 2024 edition for all workspace crates.
- **MSRV:** Rust 1.88, as declared by the workspace manifest. Verify the pinned
  release dependency graph with
  `cargo +1.88.0 check --workspace --all-targets --locked`.
- **Core dependency policy:** `bop-lang` remains zero third-party Rust
  dependencies in its standard configuration; `alloc`/`core` support the
  portable core and the existing `libm` feature supports `no_std` math.
  Rust `std` is an explicit default feature and takes precedence when Cargo
  unifies it with `no_std`; genuine no_std builds use
  `--no-default-features --features no_std`.
- **Workspace:** Cargo workspace containing `bop`, `bop-vm`, `bop-compile`,
  `bop-sys`, and `bop-cli`; manifests and `Cargo.lock` own exact versions.
- **Testing:** Cargo unit/integration tests plus VM differential and AOT
  three-way suites. `cargo clippy --workspace --all-targets` is the code-health
  target, and the explicit Rust 1.88 command above is the release MSRV gate.
- **AOT runtime mode:** `bop-compile` supports opt-in sandbox emission, while
  `bop-cli compile` currently emits unsandboxed binaries.
- **Website and documentation:** Zola templates, Tailwind CSS v4, and Markdown
  content live under `docs/`; generated `docs/public/` output is derived rather
  than normative and is published through Cloudflare Pages.

## Constraints

- Preserve the existing Rust-first architecture and shared runtime instead of
  reimplementing semantics per engine.
- Do not add OS access or general-purpose dependencies to `bop-lang`.
- New language behaviour must include focused regression coverage and, when it
  can diverge by engine, differential coverage.
- Performance work must retain sandbox checks and parity before speed claims
  are accepted.
