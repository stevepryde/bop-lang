use std::io::{self, BufRead, Write};

use bop::BopLimits;
use bop_sys::StdHost;

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() > 1 {
        // File execution mode
        let path = &args[1];
        let source = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Error reading {}: {}", path, e);
                std::process::exit(1);
            }
        };
        // Run the static-check pass first so warnings surface
        // before any program output. Parse errors fail fast;
        // warnings are informational and don't block
        // execution. On a successful check we continue into
        // `bop::run` which does its own parse again —
        // acceptable overhead for now.
        match bop::parse_with_warnings(&source) {
            Ok((_stmts, warnings)) => {
                for w in &warnings {
                    eprint!("{}", w.render(&source));
                }
            }
            Err(e) => {
                eprint!("{}", e.render(&source));
                std::process::exit(1);
            }
        }
        let mut host = StdHost::new();
        if let Err(e) = bop::run(&source, &mut host, &BopLimits::standard()) {
            // Render with the source so parse errors show the
            // offending line + a carat under the column. Runtime
            // errors (no column) still get the snippet.
            eprint!("{}", e.render(&source));
            std::process::exit(1);
        }
    } else {
        // REPL mode
        repl();
    }
}

fn repl() {
    let stdin = io::stdin();
    let mut stdout = io::stdout();

    let mut host = StdHost::new();

    print!("> ");
    stdout.flush().unwrap();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };

        if line.trim().is_empty() {
            print!("> ");
            stdout.flush().unwrap();
            continue;
        }

        match bop::run(&line, &mut host, &BopLimits::standard()) {
            Ok(()) => {}
            Err(e) => eprint!("{}", e.render(&line)),
        }

        print!("> ");
        stdout.flush().unwrap();
    }
}
