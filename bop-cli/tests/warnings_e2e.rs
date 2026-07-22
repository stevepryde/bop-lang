//! Warning parity through both real `bop run` execution engines.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn run_source(source: &str, no_vm: bool) -> (String, i32) {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "bop-warning-parity-{}-{unique}.bop",
        std::process::id()
    ));
    std::fs::write(&path, source).expect("write temporary Bop source");

    let mut command = Command::new(env!("CARGO_BIN_EXE_bop"));
    command.arg("run").arg(&path);
    if no_vm {
        command.arg("--novm");
    }
    let output = command.output().expect("run bop binary");
    std::fs::remove_file(&path).expect("remove temporary Bop source");
    (
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.code().unwrap_or(-1),
    )
}

#[test]
fn walker_and_vm_render_the_same_source_ordered_warning() {
    let source = r#"if true {
    enum Choice { Left, Missing }
    let _ = match Choice::Left { Choice::Left => 1 }
} else {
    enum Choice { Right }
}"#;
    let (vm_stderr, vm_code) = run_source(source, false);
    let (walker_stderr, walker_code) = run_source(source, true);

    assert_eq!(vm_code, 0, "VM stderr: {vm_stderr}");
    assert_eq!(walker_code, 0, "walker stderr: {walker_stderr}");
    assert_eq!(vm_stderr, walker_stderr);
    assert!(vm_stderr.contains("`Choice::Missing`"));
}
