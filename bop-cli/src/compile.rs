//! `bop compile FILE` — AOT-transpile a script and build a
//! native binary.
//!
//! Pipeline:
//!
//! 1. Read source, set up a module resolver that layers
//!    filesystem-relative imports on top of `bop-std`'s bundled
//!    stdlib (same surface `bop run` sees via `bop-sys`).
//! 2. `bop_compile::transpile(source, opts)` → Rust source
//!    string.
//! 3. If `--emit-rs`: write that string to `-o OUT` (or
//!    `FILE.rs` beside the input) and stop.
//! 4. Otherwise: drop the Rust source into a scratch cargo
//!    project under the OS temp dir, declare `bop-lang` /
//!    `bop-sys` / `bop-std` as deps at the current `bop`
//!    version, run `cargo build --release`, and copy the
//!    produced binary to `-o OUT` (or the input file's stem).
//!
//! Errors:
//!
//! - Missing `cargo` on PATH → print a pointer to rustup + the
//!   `--emit-rs` escape hatch.
//! - Transpiler / cargo failures → surface stderr verbatim and
//!   return non-zero.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use bop_compile::{ModuleResolver, Options, transpile};

pub fn compile_file(
    input: &str,
    output: Option<&str>,
    emit_rs: bool,
    keep: bool,
) -> ExitCode {
    let input_path = PathBuf::from(input);
    let source = match std::fs::read_to_string(&input_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error reading `{input}`: {e}");
            return ExitCode::from(1);
        }
    };

    // Build the resolver the transpiler feeds every `import`
    // through. Mirrors `bop-sys::StdHost::resolve_module`: look
    // in `bop-std` first for canonical stdlib names, then fall
    // back to a filesystem search rooted at the input's parent
    // directory so `import ./helpers` keeps working.
    let input_dir = input_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let resolver = make_resolver(input_dir);

    let opts = Options {
        emit_main: true,
        use_bop_sys: true,
        sandbox: false,
        module_name: None,
        module_resolver: Some(resolver),
    };

    let rust_src = match transpile(&source, &opts) {
        Ok(s) => s,
        Err(e) => {
            eprint!("{}", e.render(&source));
            return ExitCode::from(1);
        }
    };

    if emit_rs {
        let out_path = match output {
            Some(p) => PathBuf::from(p),
            None => default_rs_path(&input_path),
        };
        if let Err(e) = std::fs::write(&out_path, &rust_src) {
            eprintln!("error writing `{}`: {e}", out_path.display());
            return ExitCode::from(1);
        }
        eprintln!("wrote {}", out_path.display());
        return ExitCode::SUCCESS;
    }

    // Cargo is the build driver — it's what ships with rustup,
    // and it handles the `bop-lang` / `bop-sys` deps that the
    // transpiled code needs. Check for its presence before
    // setting up scratch work so the failure mode is clean.
    if !cargo_available() {
        eprintln!(
            "error: couldn't find `cargo` on your PATH.\n\
             `bop compile` needs a Rust toolchain to produce a native binary.\n\
             Install it from https://rustup.rs, or re-run with `--emit-rs`\n\
             to get the transpiled Rust source without building it."
        );
        return ExitCode::from(1);
    }

    let output_path = match output {
        Some(p) => PathBuf::from(p),
        None => default_binary_path(&input_path),
    };

    let scratch = match build_native(&rust_src, &input_path, &output_path) {
        Ok(s) => s,
        Err(code) => return code,
    };

    if keep {
        eprintln!("scratch project kept at {}", scratch.display());
    } else {
        let _ = std::fs::remove_dir_all(&scratch);
    }

    eprintln!("built {}", output_path.display());
    ExitCode::SUCCESS
}

fn make_resolver(root: PathBuf) -> ModuleResolver {
    use std::cell::RefCell;
    use std::rc::Rc;

    Rc::new(RefCell::new(move |name: &str| {
        // `bop-std` canonical stdlib modules first.
        if let Some(src) = bop_std::resolve(name) {
            return Some(Ok(src.to_string()));
        }
        // Filesystem fallback — look for `<name>.bop` relative
        // to the input directory. Dots in the module name are
        // treated as path separators, matching `bop-sys`'s
        // behaviour (so `import foo.bar` → `root/foo/bar.bop`).
        let mut path = root.clone();
        for segment in name.split('.') {
            path.push(segment);
        }
        path.set_extension("bop");
        match std::fs::read_to_string(&path) {
            Ok(src) => Some(Ok(src)),
            Err(_) => None,
        }
    }))
}

/// Create a scratch cargo project, drop the transpiled Rust in,
/// `cargo build --release`, and copy the binary out. Returns the
/// scratch dir so the caller can preserve it for `--keep`.
fn build_native(
    rust_src: &str,
    input_path: &Path,
    output_path: &Path,
) -> Result<PathBuf, ExitCode> {
    let stem = input_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("script");
    let scratch = scratch_dir(stem);
    if let Err(e) = std::fs::create_dir_all(scratch.join("src")) {
        eprintln!(
            "error creating scratch dir `{}`: {e}",
            scratch.display()
        );
        return Err(ExitCode::from(1));
    }

    let manifest = manifest_for_output(stem);
    if let Err(e) = std::fs::write(scratch.join("Cargo.toml"), manifest) {
        eprintln!("error writing scratch Cargo.toml: {e}");
        return Err(ExitCode::from(1));
    }
    if let Err(e) = std::fs::write(scratch.join("src/main.rs"), rust_src) {
        eprintln!("error writing scratch main.rs: {e}");
        return Err(ExitCode::from(1));
    }

    // `cargo build --release` — stdout goes through, stderr
    // prints cargo's own diagnostics if the build fails. We
    // don't try to suppress or reformat them; they're
    // generally the right thing for a user to see.
    let status = Command::new("cargo")
        .arg("build")
        .arg("--release")
        .current_dir(&scratch)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();
    let status = match status {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error invoking cargo: {e}");
            return Err(ExitCode::from(1));
        }
    };
    if !status.success() {
        eprintln!("cargo build failed — generated Rust source is under {}", scratch.display());
        return Err(ExitCode::from(1));
    }

    // Copy the produced binary to the user-facing output.
    let mut built = scratch.join("target/release").join(stem);
    if cfg!(windows) {
        built.set_extension("exe");
    }
    if let Err(e) = std::fs::copy(&built, output_path) {
        eprintln!(
            "error copying built binary `{}` → `{}`: {e}",
            built.display(),
            output_path.display()
        );
        return Err(ExitCode::from(1));
    }
    // Make sure the output is executable on Unix. `copy`
    // preserves the source's permissions, which is usually fine
    // since cargo already set the executable bit — but be
    // defensive.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(output_path) {
            let mut perms = meta.permissions();
            perms.set_mode(perms.mode() | 0o111);
            let _ = std::fs::set_permissions(output_path, perms);
        }
    }

    Ok(scratch)
}

fn cargo_available() -> bool {
    Command::new("cargo")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn default_rs_path(input: &Path) -> PathBuf {
    let mut p = input.to_path_buf();
    p.set_extension("rs");
    p
}

fn default_binary_path(input: &Path) -> PathBuf {
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("script");
    let mut p = PathBuf::from(stem);
    if cfg!(windows) {
        p.set_extension("exe");
    }
    p
}

fn scratch_dir(stem: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("bop-compile-{stem}-{}", std::process::id()));
    p
}

/// Manifest for the scratch cargo crate we feed the transpiled
/// Rust into. Declares `bop-lang` / `bop-sys` / `bop-std` at the
/// current `bop` CLI's version — the CLI and libraries ship
/// together, so by construction the deps are always in lockstep.
///
/// The generated crate carries `[workspace]` on its own so
/// cargo doesn't try to adopt it as a member of some surrounding
/// project the user happens to be sitting inside when they run
/// `bop compile`.
///
/// # Local development
///
/// Setting `BOP_DEV_WORKSPACE=/path/to/bop-repo` points the
/// generated manifest at path-based deps pointing into that
/// workspace, so `bop compile` works against the uncommitted
/// library code. Published builds leave the env var unset and
/// resolve against crates.io.
fn manifest_for_output(stem: &str) -> String {
    let version = env!("CARGO_PKG_VERSION");
    let deps = match std::env::var("BOP_DEV_WORKSPACE") {
        Ok(root) if !root.is_empty() => format!(
            r#"bop = {{ path = "{root}/bop", package = "bop-lang" }}
bop-sys = {{ path = "{root}/bop-sys" }}
bop-std = {{ path = "{root}/bop-std" }}"#,
        ),
        _ => format!(
            r#"bop = {{ version = "{version}", package = "bop-lang" }}
bop-sys = "{version}"
bop-std = "{version}""#,
        ),
    };
    format!(
        r#"[package]
name = "{stem}"
version = "0.0.0"
edition = "2024"
publish = false

[[bin]]
name = "{stem}"
path = "src/main.rs"

[dependencies]
{deps}

[workspace]

[profile.release]
# Small + fast enough: matches what a hand-written Rust user
# would reach for when building a CLI. LTO trims the AOT-emitted
# dispatch noise without pushing build time into the stratosphere.
opt-level = 3
lto = "thin"
"#
    )
}
