//! `bop` / `bop repl` — interactive REPL.
//!
//! Backed by rustyline for arrow-key history, emacs-style
//! editing, persisted history, multi-line input via the
//! `Validator` hook, and tab completion driven by the
//! `Completer` hook.
//!
//! **Multi-line input** works by handing the raw buffer to the
//! parser — if the parse error message looks like "unexpected
//! end of input" (unclosed brace, incomplete match, etc.), we
//! tell rustyline the input is incomplete and it prompts again
//! with `... `. Any other parse error is surfaced right away so
//! the user can see the problem without retyping the whole
//! block.
//!
//! **Tab completion** offers the intersection of:
//! - Bop keywords (`let`, `fn`, `if`, `use`, …)
//! - `bop::suggest::CORE_CALLABLE_BUILTINS` (language builtins)
//! - identifier-shaped tokens the user has already typed in the
//!   session (a rough proxy for "names in scope")
//!
//! **History** lives at `$HOME/.bop_history`. Save-on-exit is
//! best-effort; failure doesn't abort the session.
//!
//! Engine choice: walker by default — REPL workloads are
//! tiny, and the walker's startup cost is negligible. The VM's
//! hot-loop speedup doesn't pay for its compile step at this
//! scale. `--repl` users that want the VM can pass `--vm`
//! (not currently surfaced; a line of code away when we need it).

use std::process::ExitCode;

use bop::BopLimits;
use bop_sys::StdHost;
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{Context, Editor, Helper};

/// Bop language keywords — offered as completion candidates
/// when the user hits tab on a bare prefix.
const KEYWORDS: &[&str] = &[
    "let", "const", "fn", "if", "else", "while", "repeat", "for", "in",
    "return", "break", "continue", "match", "struct", "enum", "use",
    "as", "true", "false", "none", "try", "and", "or", "not",
];

/// Combined `Helper` impl for rustyline. Holds the set of
/// names the REPL has observed so far (parameters, `let`
/// bindings, fn names, imported identifiers…) so tab can
/// complete against them. The set grows monotonically — names
/// that went out of scope aren't pruned, matching the
/// "loose hint" semantics completions usually have.
struct BopHelper {
    names: std::cell::RefCell<std::collections::BTreeSet<String>>,
}

impl BopHelper {
    fn new() -> Self {
        Self {
            names: std::cell::RefCell::new(std::collections::BTreeSet::new()),
        }
    }

    /// Harvest identifier-shaped words from a source line and
    /// add them to the completion set. Called after each
    /// successfully parsed input so the next tab suggestion
    /// knows about fresh bindings. Deliberately dumb — a real
    /// name resolver would need persistent Evaluator state,
    /// which today's REPL doesn't have.
    fn absorb_names(&self, source: &str) {
        let mut out = self.names.borrow_mut();
        let mut chars = source.char_indices().peekable();
        while let Some((start, ch)) = chars.next() {
            if !ch.is_alphabetic() && ch != '_' {
                continue;
            }
            let mut end = start + ch.len_utf8();
            while let Some(&(i, c)) = chars.peek() {
                if c.is_alphanumeric() || c == '_' {
                    end = i + c.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            let word = &source[start..end];
            if word.len() >= 2 {
                out.insert(word.to_string());
            }
        }
    }
}

impl Helper for BopHelper {}
impl Highlighter for BopHelper {}
impl Hinter for BopHelper {
    type Hint = String;
}

impl Completer for BopHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        // Walk backwards from the cursor to find the start of
        // the current identifier. Non-ident chars (spaces,
        // punctuation) terminate the scan.
        let prefix_start = line[..pos]
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
            .last()
            .map(|(i, _)| i)
            .unwrap_or(pos);
        let prefix = &line[prefix_start..pos];

        if prefix.is_empty() {
            return Ok((pos, Vec::new()));
        }

        let names = self.names.borrow();
        let mut candidates: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        for &kw in KEYWORDS {
            if kw.starts_with(prefix) {
                candidates.insert(kw.to_string());
            }
        }
        for &b in bop::suggest::CORE_CALLABLE_BUILTINS {
            if b.starts_with(prefix) {
                candidates.insert(b.to_string());
            }
        }
        for name in names.iter() {
            if name.starts_with(prefix) && name.as_str() != prefix {
                candidates.insert(name.clone());
            }
        }
        let pairs = candidates
            .into_iter()
            .map(|s| Pair {
                display: s.clone(),
                replacement: s,
            })
            .collect();
        Ok((prefix_start, pairs))
    }
}

/// Classify `input` as either "ready to execute" or "needs
/// more input". Extracted from the `Validator` impl so tests
/// can exercise the heuristic without going through
/// rustyline's private `ValidationContext` constructor.
///
/// Returns `true` for incomplete (tell rustyline to prompt for
/// continuation), `false` for ready.
fn is_incomplete_input(input: &str) -> bool {
    if input.trim().is_empty() {
        return false;
    }
    match bop::parse(input) {
        Ok(_) => false,
        Err(e) => {
            // Heuristic: parse errors caused by running off
            // the end of the buffer mean the user is still
            // typing — tell rustyline to keep the buffer and
            // prompt for continuation. Any other parse error
            // surfaces immediately so typos don't get buried
            // in a stale multi-line buffer.
            //
            // Bop's parser spells the "hit EOF" case as
            // `end of code` (both the "Expected X but found
            // end of code" and "I didn't expect end of code
            // here" shapes). We match case-insensitively so
            // the heuristic survives small message tweaks.
            let msg = e.message.to_lowercase();
            msg.contains("end of code")
                || msg.contains("end of input")
                || msg.contains("unexpected eof")
        }
    }
}

impl Validator for BopHelper {
    fn validate(
        &self,
        ctx: &mut ValidationContext,
    ) -> rustyline::Result<ValidationResult> {
        if is_incomplete_input(ctx.input()) {
            Ok(ValidationResult::Incomplete)
        } else {
            Ok(ValidationResult::Valid(None))
        }
    }

    fn validate_while_typing(&self) -> bool {
        false
    }
}

/// Best-effort history-file path. Returns `None` if we can't
/// figure out a home dir (rare on current platforms); the REPL
/// then runs with no persisted history.
fn history_path() -> Option<std::path::PathBuf> {
    // `dirs` would be cleaner but we avoid pulling an extra
    // crate just for this. `HOME` (unix) / `USERPROFILE`
    // (windows) covers the common cases.
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|h| {
            let mut p = std::path::PathBuf::from(h);
            p.push(".bop_history");
            p
        })
}

pub fn run() -> ExitCode {
    // When stdin isn't a terminal (piped input, heredoc,
    // `bop repl <<EOF ... EOF`), rustyline's multi-line
    // Validator doesn't fire between stdin lines. Fall back
    // to reading the whole buffer as a single program so the
    // user's multi-line input survives the pipe. This matches
    // what `python3` / `node` do for the same shape of input.
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        return run_non_tty();
    }

    let helper = BopHelper::new();
    let mut rl = match Editor::<BopHelper, rustyline::history::FileHistory>::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: couldn't initialise line editor: {}", e);
            return ExitCode::from(1);
        }
    };
    rl.set_helper(Some(helper));

    let hist = history_path();
    if let Some(ref p) = hist {
        // Missing history file on first run is expected —
        // ignore the error.
        let _ = rl.load_history(p);
    }

    let mut host = StdHost::new();

    loop {
        let readline = rl.readline("> ");
        match readline {
            Ok(line) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                // Record the exact source the user entered so
                // up-arrow recalls the whole multi-line block,
                // not just a trailing fragment.
                let _ = rl.add_history_entry(line.as_str());
                if let Some(h) = rl.helper() {
                    h.absorb_names(&line);
                }
                if let Err(e) = bop::run(&line, &mut host, &BopLimits::standard()) {
                    eprint!("{}", e.render(&line));
                }
            }
            Err(ReadlineError::Interrupted) => {
                // Ctrl-C in the middle of a prompt: clear and
                // keep looping. (rustyline handles clearing
                // the partial buffer for us.)
                continue;
            }
            Err(ReadlineError::Eof) => {
                // Ctrl-D on an empty prompt: graceful exit.
                break;
            }
            Err(e) => {
                eprintln!("readline error: {}", e);
                break;
            }
        }
    }

    if let Some(ref p) = hist {
        // Best effort — a failing save shouldn't fail the
        // exit code.
        let _ = rl.save_history(p);
    }

    ExitCode::SUCCESS
}

/// Non-interactive path: slurp all of stdin and execute it as
/// a single program. Keeps `bop repl <<EOF ... EOF` working
/// the way users expect, and makes `echo "fn f() { 1 }\nf()"
/// | bop repl` a one-shot runner. No history, no completion,
/// no Validator — the whole thing is one parse + run.
fn run_non_tty() -> ExitCode {
    use std::io::Read;
    let mut buf = String::new();
    if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
        eprintln!("error reading stdin: {}", e);
        return ExitCode::from(1);
    }
    if buf.trim().is_empty() {
        return ExitCode::SUCCESS;
    }
    let mut host = StdHost::new();
    match bop::run(&buf, &mut host, &BopLimits::standard()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprint!("{}", e.render(&buf));
            ExitCode::from(1)
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_statement_is_valid() {
        assert!(!is_incomplete_input("let x = 1"));
    }

    #[test]
    fn unclosed_fn_body_is_incomplete() {
        assert!(is_incomplete_input(
            "fn double(x) {\n    return x + x"
        ));
    }

    #[test]
    fn unclosed_if_body_is_incomplete() {
        assert!(is_incomplete_input("if true {"));
    }

    #[test]
    fn trailing_plus_is_incomplete() {
        assert!(is_incomplete_input("let x = 1 +"));
    }

    #[test]
    fn real_parse_error_is_not_incomplete() {
        // A typo-style parse error (garbage token, not a
        // hanging buffer) should *not* leave the buffer in
        // limbo — surface it to the main loop to render.
        assert!(!is_incomplete_input("let 1x = 2"));
    }

    #[test]
    fn empty_input_is_not_incomplete() {
        assert!(!is_incomplete_input(""));
        assert!(!is_incomplete_input("   \n\t "));
    }

    #[test]
    fn completer_prefix_matches_keyword() {
        let h = BopHelper::new();
        let line = "le";
        let (_start, cands) = h
            .complete(line, 2, &rustyline::Context::new(
                &rustyline::history::FileHistory::new(),
            ))
            .unwrap();
        let disp: Vec<String> = cands.iter().map(|p| p.display.clone()).collect();
        assert!(disp.contains(&"let".to_string()));
    }

    #[test]
    fn completer_prefix_matches_builtin_and_user_name() {
        let h = BopHelper::new();
        h.absorb_names("let my_thing = 1");
        let line = "m";
        let (_start, cands) = h
            .complete(line, 1, &rustyline::Context::new(
                &rustyline::history::FileHistory::new(),
            ))
            .unwrap();
        let disp: Vec<String> = cands.iter().map(|p| p.display.clone()).collect();
        assert!(
            disp.contains(&"match".to_string()),
            "expected `match` keyword in: {:?}",
            disp
        );
        assert!(
            disp.contains(&"max".to_string())
                || disp.contains(&"min".to_string()),
            "expected `max` / `min` builtin in: {:?}",
            disp
        );
        assert!(
            disp.contains(&"my_thing".to_string()),
            "expected user-observed name `my_thing` in: {:?}",
            disp
        );
    }
}
