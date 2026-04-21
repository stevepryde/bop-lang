//! Argument parsing for the `bop` CLI.
//!
//! Hand-rolled rather than via `clap` because the surface is
//! tiny (three subcommands, a handful of flags) and the `bop`
//! binary is one of the things we publish — keeping the
//! dependency count down matters for install times and supply-
//! chain surface.

pub enum Command {
    /// `bop` or `bop repl` — launch the interactive REPL.
    Repl,
    /// `bop run FILE [--novm]` — execute a script.
    Run { file: String, no_vm: bool },
    /// `bop compile FILE [-o OUT] [--emit-rs] [--keep]` —
    /// transpile to Rust and (by default) build a native binary.
    Compile {
        file: String,
        output: Option<String>,
        emit_rs: bool,
        keep: bool,
    },
    Help,
    Version,
}

pub fn parse(argv: &[String]) -> Result<Command, String> {
    // argv[0] is the binary name — skip it.
    let args: &[String] = if argv.is_empty() { &[] } else { &argv[1..] };

    match args.first().map(String::as_str) {
        None => Ok(Command::Repl),
        Some("repl") => {
            forbid_extras("repl", &args[1..])?;
            Ok(Command::Repl)
        }
        Some("run") => parse_run(&args[1..]),
        Some("compile") => parse_compile(&args[1..]),
        Some("--help") | Some("-h") | Some("help") => Ok(Command::Help),
        Some("--version") | Some("-V") => Ok(Command::Version),
        // Legacy convenience: `bop FILE.bop` still works as an
        // alias for `bop run FILE.bop`. Keeps scripts and
        // shebangs that predate the subcommand split from
        // breaking on upgrade.
        Some(first) if !first.starts_with('-') => Ok(Command::Run {
            file: first.to_string(),
            no_vm: false,
        }),
        Some(unknown) => Err(format!("unknown argument: {unknown}")),
    }
}

fn parse_run(rest: &[String]) -> Result<Command, String> {
    let mut no_vm = false;
    let mut file: Option<String> = None;
    for arg in rest {
        match arg.as_str() {
            "--novm" => no_vm = true,
            other if other.starts_with('-') => {
                return Err(format!("`run`: unknown flag `{other}`"));
            }
            other => {
                if file.is_some() {
                    return Err(format!(
                        "`run`: only one script file accepted (got `{}` after `{}`)",
                        other,
                        file.as_ref().unwrap()
                    ));
                }
                file = Some(other.to_string());
            }
        }
    }
    let file = file.ok_or_else(|| "`run`: missing script file".to_string())?;
    Ok(Command::Run { file, no_vm })
}

fn parse_compile(rest: &[String]) -> Result<Command, String> {
    let mut output: Option<String> = None;
    let mut emit_rs = false;
    let mut keep = false;
    let mut file: Option<String> = None;
    let mut i = 0;
    while i < rest.len() {
        let arg = &rest[i];
        match arg.as_str() {
            "--emit-rs" => emit_rs = true,
            "--keep" => keep = true,
            "-o" | "--output" => {
                i += 1;
                match rest.get(i) {
                    Some(v) => output = Some(v.clone()),
                    None => return Err(format!("`compile`: `{arg}` needs a path argument")),
                }
            }
            other if other.starts_with('-') => {
                return Err(format!("`compile`: unknown flag `{other}`"));
            }
            other => {
                if file.is_some() {
                    return Err(format!(
                        "`compile`: only one script file accepted (got `{}` after `{}`)",
                        other,
                        file.as_ref().unwrap()
                    ));
                }
                file = Some(other.to_string());
            }
        }
        i += 1;
    }
    let file = file.ok_or_else(|| "`compile`: missing script file".to_string())?;
    Ok(Command::Compile {
        file,
        output,
        emit_rs,
        keep,
    })
}

fn forbid_extras(name: &str, rest: &[String]) -> Result<(), String> {
    if rest.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "`{name}`: unexpected extra argument `{}`",
            rest[0]
        ))
    }
}

pub fn print_usage() {
    eprintln!(
        "\
bop — a small, dynamically-typed, embeddable language.

USAGE:
    bop                         Open the REPL
    bop run FILE [--novm]       Execute a .bop script
                                --novm runs the walker instead of the VM
    bop compile FILE [OPTIONS]  Transpile + build a native binary
    bop repl                    Open the REPL (explicit)
    bop --version               Print version
    bop --help                  This message

COMPILE OPTIONS:
    -o, --output PATH   Output path (default: script name with no extension,
                        or .rs path when --emit-rs is set)
    --emit-rs           Emit transpiled Rust source only; don't invoke cargo
    --keep              Keep the scratch cargo project after building
                        (useful for inspecting the generated code)

Examples:
    bop                         # interactive REPL
    bop hello.bop               # quick run (alias for `bop run hello.bop`)
    bop run hello.bop           # same, explicit
    bop run hello.bop --novm    # use the walker
    bop compile hello.bop       # produces ./hello (native binary)
    bop compile hello.bop -o h  # produces ./h
    bop compile --emit-rs hello.bop -o hello.rs
"
    );
}
