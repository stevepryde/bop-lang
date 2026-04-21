//! The `bop` command-line interface.
//!
//! Subcommands:
//!
//! | command                  | what it does                                |
//! |--------------------------|---------------------------------------------|
//! | (no args)                | opens the REPL                              |
//! | `repl`                   | opens the REPL (explicit)                   |
//! | `run FILE`               | executes `FILE` with the bytecode VM        |
//! | `run --novm FILE`        | executes `FILE` with the tree-walker        |
//! | `compile FILE`           | AOT-transpiles and builds a native binary   |
//! | `compile --emit-rs FILE` | emits the transpiled Rust source only       |
//!
//! The default `run` path goes through the bytecode VM — it's
//! 2–3× faster than the walker on realistic workloads and the
//! semantics are identical. `--novm` is kept as an escape hatch
//! for debugging or when binary size matters.

use std::process::ExitCode;

mod args;
mod compile;
mod repl;
mod run;

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let cmd = match args::parse(&argv) {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            eprintln!();
            args::print_usage();
            return ExitCode::from(2);
        }
    };
    match cmd {
        args::Command::Repl => repl::run(),
        args::Command::Run { file, no_vm } => run::run_file(&file, no_vm),
        args::Command::Compile {
            file,
            output,
            emit_rs,
            keep,
        } => compile::compile_file(&file, output.as_deref(), emit_rs, keep),
        args::Command::Help => {
            args::print_usage();
            ExitCode::SUCCESS
        }
        args::Command::Version => {
            println!("bop {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
    }
}
