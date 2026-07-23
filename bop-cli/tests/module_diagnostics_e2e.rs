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

/// Run root.bop through both engines, assert both fail with the
/// expected exit code, and assert their stdout/stderr are
/// byte-for-byte identical. Returns the shared stderr.
fn run_both_engines_identically(project: &Path, expected_exit: i32) -> String {
    let vm = run_bop(project, &["run", "root.bop"]);
    let walker = run_bop(project, &["run", "root.bop", "--novm"]);
    let vm_stderr = String::from_utf8_lossy(&vm.stderr).into_owned();
    let walker_stderr = String::from_utf8_lossy(&walker.stderr).into_owned();
    assert_eq!(
        vm.status.code(),
        Some(expected_exit),
        "vm stderr:\n{vm_stderr}"
    );
    assert_eq!(
        walker.status.code(),
        Some(expected_exit),
        "walker stderr:\n{walker_stderr}"
    );
    assert_eq!(
        vm_stderr, walker_stderr,
        "walker and VM stderr must stay identical"
    );
    assert_eq!(
        String::from_utf8_lossy(&vm.stdout),
        String::from_utf8_lossy(&walker.stdout),
        "walker and VM stdout must stay identical"
    );
    vm_stderr
}

#[test]
fn module_runtime_error_renders_module_identity_not_root_snippet() {
    // The module's failing line collides with an unrelated root
    // line: before the fix both engines quoted the root file's
    // line 3 under the module's error.
    let project = temp_project();
    std::fs::write(
        project.join("root.bop"),
        "use rterr3\nlet unrelated = \"root line 2\"\nlet also_unrelated = \"root line 3\"\nprint(boom(1))",
    )
    .unwrap();
    std::fs::write(
        project.join("rterr3.bop"),
        "fn boom(x) {\n    let denom = 0\n    return x / denom\n}",
    )
    .unwrap();

    let stderr = run_both_engines_identically(&project, 1);
    assert!(
        stderr.contains("in module `rterr3` at line 3"),
        "missing module identity:\n{stderr}"
    );
    assert!(
        !stderr.contains("also_unrelated"),
        "root source was rendered for a module runtime error:\n{stderr}"
    );

    std::fs::remove_dir_all(project).expect("remove temporary project");
}

#[test]
fn module_runtime_error_beyond_root_length_keeps_module_identity() {
    // Before the fix the snippet was silently omitted and the
    // error looked like a root-file error at an impossible line.
    let project = temp_project();
    std::fs::write(project.join("root.bop"), "use longmod\nprint(late_boom())").unwrap();
    std::fs::write(
        project.join("longmod.bop"),
        "let a = 1\nlet b = 2\nlet c = 3\nlet d = 4\nfn late_boom() {\n    let items = [1, 2]\n    return items[10]\n}",
    )
    .unwrap();

    let stderr = run_both_engines_identically(&project, 1);
    assert!(
        stderr.contains("in module `longmod` at line 7"),
        "missing module identity:\n{stderr}"
    );

    std::fs::remove_dir_all(project).expect("remove temporary project");
}

#[test]
fn nested_module_runtime_error_attributes_deepest_module() {
    let project = temp_project();
    std::fs::write(project.join("root.bop"), "use outer\nprint(outer_call())").unwrap();
    std::fs::write(
        project.join("outer.bop"),
        "use inner\nfn outer_call() {\n    return inner_boom()\n}",
    )
    .unwrap();
    std::fs::write(
        project.join("inner.bop"),
        "fn inner_boom() {\n    let zero = 0\n    return 7 / zero\n}",
    )
    .unwrap();

    let stderr = run_both_engines_identically(&project, 1);
    assert!(
        stderr.contains("in module `inner` at line 3"),
        "deepest module must win:\n{stderr}"
    );
    assert!(!stderr.contains("`outer`"), "stderr:\n{stderr}");

    std::fs::remove_dir_all(project).expect("remove temporary project");
}

#[test]
fn aliased_and_selective_module_calls_attribute_runtime_errors() {
    let project = temp_project();
    std::fs::write(
        project.join("rterr3.bop"),
        "fn boom(x) {\n    let denom = 0\n    return x / denom\n}",
    )
    .unwrap();
    for root in [
        "use rterr3 as m\nprint(m.boom(5))",
        "use rterr3.{boom}\nprint(boom(5))",
    ] {
        std::fs::write(project.join("root.bop"), root).unwrap();
        let stderr = run_both_engines_identically(&project, 1);
        assert!(
            stderr.contains("in module `rterr3` at line 3"),
            "missing module identity for root `{root}`:\n{stderr}"
        );
    }

    std::fs::remove_dir_all(project).expect("remove temporary project");
}

#[test]
fn module_method_runtime_error_attributes_declaring_module() {
    let project = temp_project();
    std::fs::write(
        project.join("root.bop"),
        "use shapes\nlet c = Circle { radius: 0 }\nprint(c.inverse_area())",
    )
    .unwrap();
    std::fs::write(
        project.join("shapes.bop"),
        "struct Circle { radius }\n\nfn Circle.inverse_area(self) {\n    return 1 / self.radius\n}",
    )
    .unwrap();

    let stderr = run_both_engines_identically(&project, 1);
    assert!(
        stderr.contains("in module `shapes` at line 4"),
        "missing module identity:\n{stderr}"
    );

    std::fs::remove_dir_all(project).expect("remove temporary project");
}

#[test]
fn module_top_level_runtime_error_keeps_module_snippet() {
    // Runtime errors raised while a module's top-level code runs
    // (including inside its own fns) still render the module
    // source snippet — the load boundary owns the text and
    // backfills it into the runtime boundary's context.
    let project = temp_project();
    std::fs::write(project.join("root.bop"), "use toperr\nprint(\"unreached\")").unwrap();
    std::fs::write(
        project.join("toperr.bop"),
        "let fine = 1\nlet zero = 0\nlet kaput = 3 / zero",
    )
    .unwrap();

    let stderr = run_both_engines_identically(&project, 1);
    assert!(
        stderr.contains("in module `toperr` at line 3"),
        "missing module identity:\n{stderr}"
    );
    assert!(
        stderr.contains("3 | let kaput = 3 / zero"),
        "missing module snippet:\n{stderr}"
    );

    std::fs::remove_dir_all(project).expect("remove temporary project");
}

#[test]
fn root_callback_error_inside_module_fn_renders_root_source() {
    // A root-declared callback invoked by a module fn errors on a
    // root line: the module boundary must not claim the error.
    let project = temp_project();
    std::fs::write(
        project.join("root.bop"),
        "use callmod\nfn root_cb() {\n    let zero = 0\n    return 9 / zero\n}\nprint(invoke(root_cb))",
    )
    .unwrap();
    std::fs::write(
        project.join("callmod.bop"),
        "fn invoke(callback) {\n    return callback()\n}",
    )
    .unwrap();

    let stderr = run_both_engines_identically(&project, 1);
    assert!(!stderr.contains("in module"), "stderr:\n{stderr}");
    assert!(stderr.contains("--> line 4"), "stderr:\n{stderr}");
    assert!(
        stderr.contains("4 |     return 9 / zero"),
        "root snippet must render:\n{stderr}"
    );

    std::fs::remove_dir_all(project).expect("remove temporary project");
}

#[test]
fn try_call_still_catches_module_runtime_errors() {
    let project = temp_project();
    std::fs::write(
        project.join("root.bop"),
        "use rterr3\nlet r = try_call(fn() { return boom(1) })\nprint(r.is_err())\nprint(\"continues\")",
    )
    .unwrap();
    std::fs::write(
        project.join("rterr3.bop"),
        "fn boom(x) {\n    let denom = 0\n    return x / denom\n}",
    )
    .unwrap();

    for args in [vec!["run", "root.bop"], vec!["run", "root.bop", "--novm"]] {
        let output = run_bop(&project, &args);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(0), "stderr:\n{stderr}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout.trim(), "true\ncontinues", "stdout:\n{stdout}");
    }

    std::fs::remove_dir_all(project).expect("remove temporary project");
}

#[test]
fn root_runtime_error_still_renders_root_source() {
    let project = temp_project();
    std::fs::write(
        project.join("root.bop"),
        "let a = 1\nlet zero = 0\nprint(a / zero)",
    )
    .unwrap();

    let stderr = run_both_engines_identically(&project, 1);
    assert!(!stderr.contains("in module"), "stderr:\n{stderr}");
    assert!(stderr.contains("--> line 3"), "stderr:\n{stderr}");
    assert!(
        stderr.contains("3 | print(a / zero)"),
        "root snippet must render:\n{stderr}"
    );

    std::fs::remove_dir_all(project).expect("remove temporary project");
}
