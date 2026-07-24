//! Process-level regressions for `bop compile`.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_PROJECT_ID: AtomicU64 = AtomicU64::new(0);

fn temp_project() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let project_id = TEMP_PROJECT_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "bop-compile-e2e-{}-{nonce}-{project_id}",
        std::process::id(),
    ));
    std::fs::create_dir_all(&path).expect("create temporary project");
    path
}

#[test]
fn compile_finds_target_triple_artifact_with_external_target_dir() {
    let project = temp_project();
    let fake_bin = project.join("fake-bin");
    let external_target = project.join("ambient-target");
    let build_target = "test-target-triple";
    let cargo_log = project.join("cargo-target-dir.txt");
    let input = project.join("entry.bop");
    let output_path = project.join("compiled-entry");
    std::fs::create_dir_all(&fake_bin).unwrap();
    std::fs::create_dir_all(&external_target).unwrap();
    std::fs::write(&input, "print(\"compiled\")").unwrap();

    // The fake keeps this regression quick while exercising the complete CLI
    // process boundary: cargo discovery, argument construction, deterministic
    // artifact lookup, copy-out, and scratch cleanup.
    let fake_cargo = fake_bin.join("cargo");
    std::fs::write(
        &fake_cargo,
        r#"#!/bin/sh
set -eu
if [ "${1-}" = "--version" ]; then
    echo "cargo 1.88.0"
    exit 0
fi

target_dir=""
while [ "$#" -gt 0 ]; do
    if [ "$1" = "--target-dir" ]; then
        shift
        target_dir="$1"
    fi
    shift
done

test -n "$target_dir"
artifact_dir="$target_dir/release"
if [ -n "${CARGO_BUILD_TARGET-}" ]; then
    artifact_dir="$target_dir/$CARGO_BUILD_TARGET/release"
fi
printf '%s\n%s' "$target_dir" "$artifact_dir" > "$FAKE_CARGO_LOG"
mkdir -p "$artifact_dir"
printf '#!/bin/sh\nexit 0\n' > "$artifact_dir/bop_entry"
chmod +x "$artifact_dir/bop_entry"
"#,
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&fake_cargo).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&fake_cargo, permissions).unwrap();

    let path = std::env::join_paths(std::iter::once(fake_bin.clone()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_bop"))
        .arg("compile")
        .arg(&input)
        .arg("-o")
        .arg(&output_path)
        .env("PATH", path)
        .env("CARGO_TARGET_DIR", &external_target)
        .env("CARGO_BUILD_TARGET", build_target)
        .env("FAKE_CARGO_LOG", &cargo_log)
        .output()
        .expect("run bop compile");
    let stderr = String::from_utf8_lossy(&result.stderr);

    assert_eq!(result.status.code(), Some(0), "stderr:\n{stderr}");
    assert!(output_path.is_file(), "requested executable was not copied");

    let cargo_log = std::fs::read_to_string(&cargo_log).unwrap();
    let mut cargo_paths = cargo_log.lines().map(PathBuf::from);
    let explicit_target = cargo_paths
        .next()
        .expect("logged explicit target directory");
    let artifact_dir = cargo_paths.next().expect("logged Cargo artifact directory");
    assert!(
        cargo_paths.next().is_none(),
        "fake Cargo emitted an unexpected log shape"
    );
    assert_ne!(explicit_target, external_target);
    assert_eq!(
        explicit_target.file_name().and_then(|name| name.to_str()),
        Some("target")
    );
    assert_eq!(
        artifact_dir,
        explicit_target.join(build_target).join("release"),
        "process regression must exercise target/<triple>/release"
    );
    assert!(
        !explicit_target.exists(),
        "scratch directory should be cleaned after copy-out"
    );
    assert!(
        std::fs::read_dir(&external_target)
            .unwrap()
            .next()
            .is_none(),
        "ambient target directory should not receive build artifacts"
    );

    std::fs::remove_dir_all(project).expect("remove temporary project");
}
