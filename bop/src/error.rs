//! Error type for the Bop interpreter.

#[cfg(not(feature = "std"))]
use alloc::string::String;

#[derive(Debug, Clone)]
pub struct BopError {
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub message: String,
    pub friendly_hint: Option<String>,
    /// Fatal errors can't be caught by `try_call`; they always
    /// unwind to the engine boundary. This is the load-bearing
    /// property that makes `BopLimits` a real sandbox — a
    /// script can't wrap an infinite loop in `try_call` and
    /// loop forever by swallowing the step-limit error.
    ///
    /// Non-fatal errors (the default) describe ordinary runtime
    /// problems — type mismatches, missing fields, index out of
    /// bounds, "function not found". Those can be caught.
    ///
    /// Currently set only on resource-limit errors
    /// (`Your code took too many steps`, `Memory limit
    /// exceeded`). Any new fatal case must explicitly construct
    /// `BopError::fatal` rather than `BopError::runtime`.
    pub is_fatal: bool,
    /// True only for the sentinel error the walker uses to
    /// unwind a `try`-driven early-return out of an enclosing
    /// fn. When set, the `message` / `line` / `friendly_hint`
    /// fields are unused — the return value lives on the
    /// evaluator's `pending_try_return` slot. `call_bop_fn`
    /// traps errors with this flag and converts them into a
    /// normal `Signal::Return`.
    ///
    /// Always `false` outside that narrow window. Users and
    /// host code should never construct a `BopError` with this
    /// flag set; use [`BopError::runtime`] / [`BopError::fatal`]
    /// for real errors.
    ///
    /// Replaces the older `"__bop_try_return_signal__"` message
    /// sentinel — a field lookup is cheaper than a string
    /// compare, and a flag can never collide with a user
    /// message that happens to spell the same bytes.
    pub is_try_return: bool,
}

impl BopError {
    /// Create a runtime error at the given source line.
    pub fn runtime(message: impl Into<String>, line: u32) -> Self {
        Self {
            line: Some(line),
            column: None,
            message: message.into(),
            friendly_hint: None,
            is_fatal: false,
            is_try_return: false,
        }
    }

    /// Create a **fatal** runtime error at the given source line.
    /// Used for resource-limit violations (`too many steps`,
    /// `Memory limit exceeded`) — see [`BopError::is_fatal`]
    /// for why those must never be swallowed by `try_call`.
    pub fn fatal(message: impl Into<String>, line: u32) -> Self {
        Self {
            line: Some(line),
            column: None,
            message: message.into(),
            friendly_hint: None,
            is_fatal: true,
            is_try_return: false,
        }
    }

    /// Build the sentinel error the walker uses to unwind a
    /// `try`-driven early-return. Private to the crate because
    /// no one outside the walker's fn-call boundary should be
    /// constructing one of these — they'd leak a "phantom"
    /// error to user code. The return value itself travels on
    /// the evaluator's `pending_try_return` slot (see
    /// `Evaluator::eval_try`).
    pub(crate) fn try_return_signal(line: u32) -> Self {
        Self {
            line: Some(line),
            column: None,
            message: String::new(),
            friendly_hint: None,
            is_fatal: false,
            is_try_return: true,
        }
    }
}

impl core::fmt::Display for BopError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if let Some(line) = self.line {
            write!(f, "[line {}] {}", line, self.message)
        } else {
            write!(f, "{}", self.message)
        }
    }
}

impl BopError {
    /// Render the error with an inline source snippet and a
    /// `^` carat under the offending position. Needs the full
    /// program source that produced the error — pass the same
    /// string you handed to `bop::run` / `bop::parse`.
    ///
    /// Falls back gracefully:
    /// - No line set → just the message.
    /// - Line set but out of range (e.g. source was truncated)
    ///   → message + "[line N]" without the snippet.
    /// - No column set → message + snippet, no carat.
    /// - Column set → message + snippet + carat.
    ///
    /// Appends the `friendly_hint` as a `hint:` line when
    /// present. Used by `bop-cli` to render program failures;
    /// embedders can call it from their own error path.
    pub fn render(&self, source: &str) -> String {
        let mut out = String::new();
        match self.line {
            Some(line) if line > 0 => {
                out.push_str(&format!("error: {}\n", self.message));
                let line_str = format!("  --> line {}", line);
                if let Some(col) = self.column {
                    out.push_str(&format!("{}:{}\n", line_str, col));
                } else {
                    out.push_str(&format!("{}\n", line_str));
                }
                if let Some(src_line) = source.lines().nth((line - 1) as usize) {
                    let gutter_width = digits_of(line);
                    let gutter_pad = " ".repeat(gutter_width);
                    out.push_str(&format!("{} |\n", gutter_pad));
                    out.push_str(&format!("{} | {}\n", line, src_line));
                    out.push_str(&format!("{} | ", gutter_pad));
                    if let Some(col) = self.column {
                        // `column` is 1-indexed; characters up to
                        // `col - 1` get a padding space each.
                        let col_idx = col.saturating_sub(1) as usize;
                        let mut pad = String::new();
                        for (i, ch) in src_line.chars().enumerate() {
                            if i >= col_idx {
                                break;
                            }
                            // Preserve tab alignment so the carat
                            // lands under the right column even
                            // in tab-indented source.
                            pad.push(if ch == '\t' { '\t' } else { ' ' });
                        }
                        out.push_str(&pad);
                        out.push_str("^\n");
                    } else {
                        out.push('\n');
                    }
                }
            }
            _ => {
                out.push_str(&format!("error: {}\n", self.message));
            }
        }
        if let Some(hint) = &self.friendly_hint {
            out.push_str(&format!("hint: {}\n", hint));
        }
        out
    }
}

/// Count decimal digits in a positive integer — used for
/// gutter width in `render`.
fn digits_of(mut n: u32) -> usize {
    let mut d = 0usize;
    if n == 0 {
        return 1;
    }
    while n > 0 {
        d += 1;
        n /= 10;
    }
    d
}

/// Non-fatal diagnostic surfaced by static checks that run
/// after parsing (currently: match-exhaustiveness analysis in
/// [`crate::check`]). Shape mirrors `BopError` so the same
/// source-snippet rendering works; the only divergence is the
/// leading header, which says `warning:` instead of `error:`.
///
/// Warnings never halt execution — they're informational. The
/// CLI prints them and then runs the program anyway. Embedders
/// that want to treat them as errors can call
/// [`BopWarning::into_error`].
#[derive(Debug, Clone)]
pub struct BopWarning {
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub message: String,
    pub friendly_hint: Option<String>,
}

impl BopWarning {
    /// Convenience constructor that matches `BopError::runtime`'s
    /// shape so check passes can build warnings at a single
    /// source line.
    pub fn at(message: impl Into<String>, line: u32) -> Self {
        Self {
            line: Some(line),
            column: None,
            message: message.into(),
            friendly_hint: None,
            }
    }

    /// Attach a "hint:" line to the rendered output. Chained
    /// from the constructor so call sites stay tidy.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.friendly_hint = Some(hint.into());
        self
    }

    /// Promote the warning to a fatal [`BopError`] with the same
    /// fields. Useful for `-Werror`-style embedders.
    pub fn into_error(self) -> BopError {
        BopError {
            line: self.line,
            column: self.column,
            message: self.message,
            friendly_hint: self.friendly_hint,
            is_fatal: false,
            is_try_return: false,
        }
    }

    /// Render the warning with a source snippet. Mirrors
    /// [`BopError::render`] but leads with `warning:` rather
    /// than `error:`.
    pub fn render(&self, source: &str) -> String {
        let err = BopError {
            line: self.line,
            column: self.column,
            message: self.message.clone(),
            friendly_hint: self.friendly_hint.clone(),
            is_fatal: false,
            is_try_return: false,
        };
        // Swap the leading `error:` for `warning:` so the
        // output is visually distinct. The rest of the carat /
        // snippet logic is identical to `BopError::render`.
        err.render(source).replacen("error:", "warning:", 1)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for BopError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_error_sets_message_and_line() {
        let err = BopError::runtime("boom", 7);

        assert_eq!(err.message, "boom");
        assert_eq!(err.line, Some(7));
        assert_eq!(err.column, None);
        assert_eq!(err.friendly_hint, None);
        assert!(!err.is_fatal);
    }

    #[test]
    fn fatal_error_marks_is_fatal_true() {
        let err = BopError::fatal("step limit", 0);
        assert!(err.is_fatal);
        assert!(!BopError::runtime("nope", 0).is_fatal);
    }

    #[test]
    fn render_without_source_falls_back_to_message_only() {
        let err = BopError::runtime("boom", 0);
        let rendered = err.render("");
        assert!(rendered.contains("error: boom"));
    }

    #[test]
    fn render_with_line_shows_snippet() {
        let src = "let x = 1\nlet y = 2\nlet z = 3";
        let err = BopError {
            line: Some(2),
            column: None,
            message: "something broke".into(),
            friendly_hint: None,
            is_fatal: false,

            is_try_return: false,
        };
        let rendered = err.render(src);
        assert!(rendered.contains("error: something broke"));
        assert!(rendered.contains("--> line 2"));
        assert!(rendered.contains("let y = 2"));
    }

    #[test]
    fn render_with_line_and_column_places_carat() {
        let src = "let x = 1\nlet abc = foo()\nlet z = 3";
        let err = BopError {
            line: Some(2),
            column: Some(11),
            message: "undefined".into(),
            friendly_hint: Some("did you mean `bar`?".into()),
            is_fatal: false,

            is_try_return: false,
        };
        let rendered = err.render(src);
        assert!(rendered.contains("--> line 2:11"));
        assert!(rendered.contains("let abc = foo()"));
        // Carat at column 11 → 10 spaces of padding before `^`.
        assert!(
            rendered.contains(&format!("{}^", " ".repeat(10))),
            "rendered:\n{}",
            rendered
        );
        assert!(rendered.contains("hint: did you mean `bar`?"));
    }

    #[test]
    fn render_handles_out_of_range_line_gracefully() {
        let src = "let x = 1";
        let err = BopError {
            line: Some(99),
            column: Some(3),
            message: "off the end".into(),
            friendly_hint: None,
            is_fatal: false,

            is_try_return: false,
        };
        // Shouldn't panic; just produces the header without a
        // snippet.
        let rendered = err.render(src);
        assert!(rendered.contains("--> line 99:3"));
        assert!(rendered.contains("error: off the end"));
    }

    #[test]
    fn render_preserves_tab_alignment_in_carat() {
        // Source has a leading tab. Carat padding should use a
        // tab too so it lines up under the offending char.
        let src = "\tlet x = bad_call()";
        let err = BopError {
            line: Some(1),
            column: Some(10),
            message: "undefined".into(),
            friendly_hint: None,
            is_fatal: false,

            is_try_return: false,
        };
        let rendered = err.render(src);
        // The carat line has one tab (from column 1's tab in
        // source) plus 8 spaces for columns 2–9, then `^`.
        assert!(rendered.contains("\t        ^"), "rendered:\n{}", rendered);
    }
}
