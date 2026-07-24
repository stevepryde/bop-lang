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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use bop_compile::{ModuleResolver, Options, transpile};

const SCRATCH_CREATE_ATTEMPTS: usize = 128;
static NEXT_SCRATCH_DIR: AtomicUsize = AtomicUsize::new(0);

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

    // Build the resolver the transpiler feeds every `use`
    // through. Mirrors `bop-sys::StdHost::resolve_module`: look
    // in `bop-std` first for canonical stdlib names, then fall
    // back to a filesystem search rooted at the input's parent
    // directory so `use ./helpers` keeps working.
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
        if let Some(src) = bop::stdlib::resolve(name) {
            return Some(Ok(src.to_string()));
        }
        // Filesystem fallback shares runtime resolution's path,
        // validation, and NotFound-vs-I/O-error contract.
        bop_sys::resolve_module_from_root(&root, name)
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
    // Cargo package/bin names are identifiers, not display
    // names. Keep them independent from the user's filename so
    // spaces, quotes, Unicode, and other path-safe characters
    // cannot produce an invalid or malformed manifest.
    let target_name = cargo_target_name(stem);
    let scratch = match create_scratch_dir(&target_name) {
        Ok(path) => path,
        Err(e) => {
            eprintln!("error creating scratch dir: {e}");
            return Err(ExitCode::from(1));
        }
    };
    if let Err(e) = std::fs::create_dir(scratch.join("src")) {
        eprintln!(
            "error creating scratch source dir `{}`: {e}",
            scratch.display()
        );
        return Err(ExitCode::from(1));
    }

    let manifest = manifest_for_output(&target_name);
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
    let mut built = scratch.join("target/release").join(&target_name);
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

/// Atomically claim a fresh, private scratch project root.
///
/// The path must not be accepted when it already exists: Cargo automatically
/// discovers files such as `build.rs`, so reusing a directory an attacker can
/// populate would execute their code during `cargo build`. Candidate names are
/// deliberately hard to collide with, but the security boundary is the atomic
/// non-recursive `create`, not the secrecy of the name.
fn create_scratch_dir(stem: &str) -> std::io::Result<PathBuf> {
    let parent = std::env::temp_dir();
    let started_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = NEXT_SCRATCH_DIR.fetch_add(1, Ordering::Relaxed) as u128;
    let pid = std::process::id();

    let candidates = (0..SCRATCH_CREATE_ATTEMPTS).map(|attempt| {
        let nonce = started_at
            .wrapping_add(sequence << 64)
            .wrapping_add(attempt as u128);
        parent.join(format!("bop-compile-{stem}-{pid}-{nonce:032x}"))
    });
    claim_first_available_scratch_dir(candidates)
}

fn claim_first_available_scratch_dir(
    candidates: impl IntoIterator<Item = PathBuf>,
) -> std::io::Result<PathBuf> {
    for candidate in candidates {
        match create_private_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(std::io::Error::new(
                    error.kind(),
                    format!("`{}`: {error}", candidate.display()),
                ));
            }
        }
    }

    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "couldn't claim a new scratch directory after {SCRATCH_CREATE_ATTEMPTS} attempts"
        ),
    ))
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder.create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::DirBuilder::new().create(path)
    }
}

/// Derive a guaranteed-safe internal Cargo package and binary
/// name from the user-visible filename. The `bop_` prefix keeps
/// leading digits and Rust keywords valid; limiting the body to
/// ASCII identifier characters also prevents TOML quoting and
/// platform-specific filename surprises.
fn cargo_target_name(stem: &str) -> String {
    let mut body = String::with_capacity(stem.len().min(48));
    let mut previous_was_separator = false;

    for ch in stem.chars() {
        if body.len() == 48 {
            break;
        }
        if ch.is_ascii_alphanumeric() {
            body.push(ch.to_ascii_lowercase());
            previous_was_separator = false;
        } else if !body.is_empty() && !previous_was_separator {
            body.push('_');
            previous_was_separator = true;
        }
    }

    while body.ends_with('_') {
        body.pop();
    }
    if body.is_empty() {
        body.push_str("script");
    }

    format!("bop_{body}")
}

/// Manifest for the scratch cargo crate we feed the transpiled
/// Rust into. Declares `bop-lang` + `bop-sys` at the current
/// `bop` CLI's version — the CLI and libraries ship together,
/// so by construction the deps are always in lockstep. The
/// bundled stdlib comes for free via the `bop-std` feature
/// (default on) on both deps — no separate crate line needed.
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
bop-sys = {{ path = "{root}/bop-sys" }}"#,
        ),
        _ => format!(
            r#"bop = {{ version = "{version}", package = "bop-lang" }}
bop-sys = "{version}""#,
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

#[cfg(test)]
mod tests {
    use super::*;

    static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

    fn resolver_test_root(label: &str) -> PathBuf {
        let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "bop_cli_resolver_{}_{}_{}",
            std::process::id(),
            label,
            sequence
        ));
        match std::fs::remove_dir_all(&root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("stale test resolver root should be removable: {error}"),
        }
        std::fs::create_dir_all(&root).expect("test resolver root should be created");
        root
    }

    fn resolve_once(
        resolver: &ModuleResolver,
        name: &str,
    ) -> Option<Result<String, bop::BopError>> {
        (resolver.borrow_mut())(name)
    }

    #[test]
    fn cargo_target_name_sanitizes_user_filename_stems() {
        assert_eq!(cargo_target_name("my prog"), "bop_my_prog");
        assert_eq!(cargo_target_name("quoted\" name"), "bop_quoted_name");
        assert_eq!(cargo_target_name("123 🚀"), "bop_123");
        assert_eq!(cargo_target_name("\" 🚀 \""), "bop_script");
        assert_eq!(cargo_target_name("type"), "bop_type");
    }

    #[test]
    fn manifest_uses_only_the_safe_internal_target_name() {
        let unsafe_stem = "my \"program\"";
        let target_name = cargo_target_name(unsafe_stem);
        let manifest = manifest_for_output(&target_name);

        assert!(manifest.contains("name = \"bop_my_program\""));
        assert!(!manifest.contains(unsafe_stem));
    }

    #[test]
    fn default_output_path_preserves_the_user_visible_stem() {
        assert_eq!(
            default_binary_path(Path::new("my program.bop")),
            PathBuf::from(if cfg!(windows) {
                "my program.exe"
            } else {
                "my program"
            })
        );
    }

    #[test]
    fn scratch_creation_never_reuses_an_attacker_controlled_directory() {
        let root = resolver_test_root("scratch_collision");
        let injected = root.join("attacker-claimed");
        let fresh = root.join("bop-claimed");
        std::fs::create_dir(&injected).unwrap();
        std::fs::write(
            injected.join("build.rs"),
            "compile_error!(\"attacker build script executed\");",
        )
        .unwrap();

        let scratch =
            claim_first_available_scratch_dir([injected.clone(), fresh.clone()]).unwrap();

        assert_eq!(scratch, fresh);
        assert!(!scratch.join("build.rs").exists());
        assert_eq!(
            std::fs::read_to_string(injected.join("build.rs")).unwrap(),
            "compile_error!(\"attacker build script executed\");"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn scratch_creation_uses_private_unix_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root = resolver_test_root("scratch_permissions");
        let scratch =
            claim_first_available_scratch_dir([root.join("bop-claimed")]).unwrap();
        let mode = std::fs::metadata(&scratch).unwrap().permissions().mode();

        assert_eq!(mode & 0o077, 0, "scratch mode was {mode:o}");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compile_resolver_reads_filesystem_modules() {
        let root = resolver_test_root("success");
        let module_dir = root.join("math");
        std::fs::create_dir_all(&module_dir).unwrap();
        std::fs::write(module_dir.join("util.bop"), "let answer = 42").unwrap();

        let resolver = make_resolver(root.clone());
        let source = resolve_once(&resolver, "math.util")
            .expect("module should be handled")
            .expect("module should be readable");
        assert_eq!(source, "let answer = 42");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compile_resolver_returns_none_only_for_not_found() {
        let root = resolver_test_root("missing");
        let resolver = make_resolver(root.clone());

        assert!(resolve_once(&resolver, "does_not_exist").is_none());

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compile_resolver_surfaces_non_not_found_read_errors() {
        let root = resolver_test_root("read_error");
        // The resolver expects a file here. A directory is deterministic and
        // unreadable as text without relying on permissions (which root can
        // bypass in CI containers).
        std::fs::create_dir(root.join("broken.bop")).unwrap();
        let resolver = make_resolver(root.clone());

        let error = resolve_once(&resolver, "broken")
            .expect("non-NotFound failures must be handled")
            .expect_err("directory read must surface as an I/O error");
        assert!(
            error.message.contains("couldn't read module `broken`"),
            "unexpected error: {}",
            error.message
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn compile_resolver_prefers_bundled_stdlib_over_filesystem() {
        let root = resolver_test_root("stdlib_precedence");
        let std_dir = root.join("std");
        std::fs::create_dir_all(&std_dir).unwrap();
        std::fs::write(std_dir.join("math.bop"), "let shadow = true").unwrap();
        let resolver = make_resolver(root.clone());

        let source = resolve_once(&resolver, "std.math")
            .expect("stdlib module should be handled")
            .expect("stdlib module should resolve");
        assert_eq!(source, bop::stdlib::resolve("std.math").unwrap());
        assert!(!source.contains("let shadow = true"));

        let _ = std::fs::remove_dir_all(root);
    }
}
