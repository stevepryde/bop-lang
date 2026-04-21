//! `bop run FILE` — execute a script.

use std::process::ExitCode;

use bop::BopLimits;
use bop_sys::StdHost;

/// Read `path`, run the match-exhaustiveness / warning pass, and
/// execute. Defaults to the bytecode VM; `no_vm` forces the
/// tree-walker.
///
/// Errors (file I/O, parse, runtime) render with
/// `BopError::render`/`BopWarning::render` so the terminal output
/// has source snippets + carats under the offending column when
/// the error carries one.
pub fn run_file(path: &str, no_vm: bool) -> ExitCode {
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error reading `{path}`: {e}");
            return ExitCode::from(1);
        }
    };

    // Static checks (match exhaustiveness) surface before any
    // program output. Parse errors fail fast; warnings are
    // informational.
    match bop::parse_with_warnings(&source) {
        Ok((_stmts, warnings)) => {
            for w in &warnings {
                eprint!("{}", w.render(&source));
            }
        }
        Err(e) => {
            eprint!("{}", e.render(&source));
            return ExitCode::from(1);
        }
    }

    let mut host = StdHost::new();
    let result = if no_vm {
        bop::run(&source, &mut host, &BopLimits::standard())
    } else {
        bop_vm::run(&source, &mut host, &BopLimits::standard())
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprint!("{}", e.render(&source));
            ExitCode::from(1)
        }
    }
}
