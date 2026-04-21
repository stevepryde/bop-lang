//! `bop` / `bop repl` — interactive REPL.
//!
//! Single-line input per `>` prompt; the walker is the engine
//! here because startup + per-line overhead trumps the VM's
//! hot-loop speed advantage for one-liners. (Flipping this to
//! the VM would be a line or two of change; we'll revisit once
//! REPL usage patterns are clearer.)

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

use bop::BopLimits;
use bop_sys::StdHost;

pub fn run() -> ExitCode {
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    let mut host = StdHost::new();

    print!("> ");
    let _ = stdout.flush();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };

        if line.trim().is_empty() {
            print!("> ");
            let _ = stdout.flush();
            continue;
        }

        if let Err(e) = bop::run(&line, &mut host, &BopLimits::standard()) {
            eprint!("{}", e.render(&line));
        }

        print!("> ");
        let _ = stdout.flush();
    }

    ExitCode::SUCCESS
}
