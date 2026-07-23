//! Process-level module diagnostic regressions for run and compile.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
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
        "bop-module-diagnostics-{}-{nonce}-{project_id}",
        std::process::id(),
    ));
    std::fs::create_dir_all(&path).expect("create temporary project");
    path
}

fn run_bop(project: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bop"))
        .args(args)
        .current_dir(project)
        .output()
        .expect("run bop binary")
}

fn assert_module_diagnostic(output: &Output, expected_module: &str) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(1),
        "command should fail cleanly:\n{stderr}"
    );
    assert!(
        stderr.contains(&format!("in module `{expected_module}` at line 2")),
        "missing module identity:\n{stderr}"
    );
    assert!(
        stderr.contains("2 | let broken ="),
        "missing module snippet:\n{stderr}"
    );
    assert!(
        !stderr.contains("1 | use outer"),
        "root source was rendered for a module error:\n{stderr}"
    );
}

#[test]
fn run_vm_walker_and_compile_render_transitive_module_parse_source() {
    let project = temp_project();
    std::fs::write(project.join("root.bop"), "use outer").unwrap();
    std::fs::write(project.join("outer.bop"), "use inner\nlet outer = 1").unwrap();
    std::fs::write(project.join("inner.bop"), "let okay = 1\nlet broken =").unwrap();

    assert_module_diagnostic(&run_bop(&project, &["run", "root.bop"]), "inner");
    assert_module_diagnostic(&run_bop(&project, &["run", "root.bop", "--novm"]), "inner");
    assert_module_diagnostic(
        &run_bop(
            &project,
            &["compile", "--emit-rs", "root.bop", "-o", "root.rs"],
        ),
        "inner",
    );

    std::fs::remove_dir_all(project).expect("remove temporary project");
}

#[test]
fn root_parse_error_still_renders_root_source() {
    let project = temp_project();
    std::fs::write(project.join("root.bop"), "let root_broken =").unwrap();

    let output = run_bop(&project, &["run", "root.bop"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "stderr:\n{stderr}");
    assert!(stderr.contains("--> line 1"), "stderr:\n{stderr}");
    assert!(
        stderr.contains("1 | let root_broken ="),
        "stderr:\n{stderr}"
    );
    assert!(!stderr.contains("in module"), "stderr:\n{stderr}");

    std::fs::remove_dir_all(project).expect("remove temporary project");
}

#[test]
fn warning_pass_does_not_eagerly_reject_an_unexecuted_broken_module() {
    let project = temp_project();
    std::fs::write(
        project.join("root.bop"),
        "fn unused() { use broken }\nprint(\"ok\")",
    )
    .unwrap();
    std::fs::write(project.join("broken.bop"), "let broken =").unwrap();

    for args in [vec!["run", "root.bop"], vec!["run", "root.bop", "--novm"]] {
        let output = run_bop(&project, &args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(0), "stderr:\n{stderr}");
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "ok");
    }

    std::fs::remove_dir_all(project).expect("remove temporary project");
}
