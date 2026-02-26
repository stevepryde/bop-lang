use crate::error::BopError;

const RESERVED_KEYWORDS: &[&str] = &[
    // Core language
    "let", "fn", "return", "if", "else", "while", "for", "in", "repeat", "break", "continue",
    // Literals
    "true", "false", "none",
    // Future
    "on", "event", "entity", "spawn", "state", "match", "loop", "class", "self", "import", "from",
    "as",
    // Error prevention
    "try", "catch", "throw", "async", "await", "yield", "const", "var", "pub", "use", "mod",
    "enum", "struct", "type",
    // Confusion prevention
    "null",
];

pub fn check(code: &str) -> Option<BopError> {
    for &keyword in RESERVED_KEYWORDS {
        let let_pattern = format!("let {} ", keyword);
        if code.contains(&let_pattern) {
            let line = code
                .lines()
                .enumerate()
                .find(|(_, line)| line.contains(&let_pattern))
                .map(|(i, _)| i as u32 + 1);

            return Some(BopError {
                line,
                column: None,
                message: format!("`{}` is a reserved word in Bop", keyword),
                friendly_hint: Some(format!(
                    "You can't use `{}` as a variable name — try something like `my_{}` instead!",
                    keyword, keyword
                )),
            });
        }

        let fn_pattern = format!("fn {}(", keyword);
        let fn_pattern_space = format!("fn {} (", keyword);
        if code.contains(&fn_pattern) || code.contains(&fn_pattern_space) {
            let line = code
                .lines()
                .enumerate()
                .find(|(_, line)| {
                    line.contains(&fn_pattern) || line.contains(&fn_pattern_space)
                })
                .map(|(i, _)| i as u32 + 1);

            return Some(BopError {
                line,
                column: None,
                message: format!("`{}` is a reserved word in Bop", keyword),
                friendly_hint: Some(format!(
                    "You can't name a function `{}` — try something like `do_{}` instead!",
                    keyword, keyword
                )),
            });
        }
    }

    None
}
