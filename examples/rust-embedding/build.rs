use std::{env, fs, path::PathBuf};

use bop_compile::{Options, transpile};

fn main() {
    let source_path = "src/plugin.bop";
    println!("cargo:rerun-if-changed={source_path}");

    let source = fs::read_to_string(source_path).expect("read Bop plugin source");
    let generated = transpile(
        &source,
        &Options {
            emit_main: false,
            use_bop_sys: false,
            sandbox: true,
            module_name: Some("plugin".to_owned()),
            module_resolver: None,
        },
    )
    .expect("transpile Bop plugin");
    let generated =
        format!("#[allow(clippy::all, clippy::pedantic, clippy::nursery)]\n{generated}",);

    let output = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("plugin.rs");
    fs::write(output, generated).expect("write generated Rust");
}
