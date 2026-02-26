use std::io::{self, BufRead, Write};

use bop::{BopLimits, BopHost, BopError, Value, StdHost};

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
        let mut host = StdHost;
        if let Err(e) = bop::run(&source, &mut host, &BopLimits::standard()) {
            eprintln!("{}", e);
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

    let mut host = ReplHost;

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
            Err(e) => eprintln!("{}", e),
        }

        print!("> ");
        stdout.flush().unwrap();
    }
}

struct ReplHost;

impl BopHost for ReplHost {
    fn call(&mut self, _name: &str, _args: &[Value], _line: u32) -> Option<Result<Value, BopError>> {
        None
    }
}
