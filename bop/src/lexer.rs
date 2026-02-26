use crate::error::BopError;

#[derive(Debug, Clone, PartialEq)]
pub enum StringPart {
    Literal(String),
    Variable(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    // Literals
    Number(f64),
    Str(String),
    StringInterp(Vec<StringPart>),
    True,
    False,
    None,

    // Identifiers & Keywords
    Ident(String),
    Let,
    Fn,
    Return,
    If,
    Else,
    While,
    For,
    In,
    Repeat,
    Break,
    Continue,

    // Operators
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    EqEq,
    BangEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    AmpAmp,
    PipePipe,
    Bang,
    Eq,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,

    // Delimiters
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Colon,
    Dot,
    Semicolon,

    // Internal (removed after auto-semicolons)
    Newline,

    Eof,
}

#[derive(Debug, Clone)]
pub struct SpannedToken {
    pub token: Token,
    pub line: u32,
}

pub fn lex(source: &str) -> Result<Vec<SpannedToken>, BopError> {
    let mut lexer = Lexer::new(source);
    let raw = lexer.lex_all()?;
    Ok(insert_semicolons(raw))
}

fn triggers_semicolon(token: &Token) -> bool {
    matches!(
        token,
        Token::Ident(_)
            | Token::Number(_)
            | Token::Str(_)
            | Token::StringInterp(_)
            | Token::True
            | Token::False
            | Token::None
            | Token::Break
            | Token::Continue
            | Token::Return
            | Token::RParen
            | Token::RBracket
            | Token::RBrace
    )
}

fn insert_semicolons(raw: Vec<SpannedToken>) -> Vec<SpannedToken> {
    let mut result: Vec<SpannedToken> = Vec::new();
    for token in raw {
        if token.token == Token::Newline {
            if let Some(last) = result.last() {
                if triggers_semicolon(&last.token) {
                    result.push(SpannedToken {
                        token: Token::Semicolon,
                        line: token.line,
                    });
                }
            }
        } else {
            result.push(token);
        }
    }
    result
}

struct Lexer {
    chars: Vec<char>,
    pos: usize,
    line: u32,
}

impl Lexer {
    fn new(source: &str) -> Self {
        Self {
            chars: source.chars().collect(),
            pos: 0,
            line: 1,
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_next(&self) -> Option<char> {
        self.chars.get(self.pos + 1).copied()
    }

    fn advance(&mut self) -> Option<char> {
        let ch = self.chars.get(self.pos).copied()?;
        self.pos += 1;
        Some(ch)
    }

    fn error(&self, message: impl Into<String>) -> BopError {
        BopError {
            line: Some(self.line),
            column: None,
            message: message.into(),
            friendly_hint: None,
        }
    }

    fn error_with_hint(
        &self,
        message: impl Into<String>,
        hint: impl Into<String>,
    ) -> BopError {
        BopError {
            line: Some(self.line),
            column: None,
            message: message.into(),
            friendly_hint: Some(hint.into()),
        }
    }

    fn lex_all(&mut self) -> Result<Vec<SpannedToken>, BopError> {
        let mut tokens = Vec::new();

        loop {
            // Skip whitespace (not newlines)
            while let Some(ch) = self.peek() {
                if ch == ' ' || ch == '\t' || ch == '\r' {
                    self.advance();
                } else {
                    break;
                }
            }

            let Some(ch) = self.peek() else {
                tokens.push(SpannedToken {
                    token: Token::Eof,
                    line: self.line,
                });
                break;
            };

            let line = self.line;

            match ch {
                '\n' => {
                    self.advance();
                    self.line += 1;
                    tokens.push(SpannedToken {
                        token: Token::Newline,
                        line,
                    });
                }

                '/' if self.peek_next() == Some('/') => {
                    // Line comment — skip to end of line
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.advance();
                    }
                }

                '"' => {
                    tokens.push(SpannedToken {
                        token: self.lex_string()?,
                        line,
                    });
                }

                '0'..='9' => {
                    tokens.push(SpannedToken {
                        token: self.lex_number()?,
                        line,
                    });
                }

                'a'..='z' | 'A'..='Z' | '_' => {
                    tokens.push(SpannedToken {
                        token: self.lex_ident_or_keyword(),
                        line,
                    });
                }

                '+' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::PlusEq,
                            line,
                        });
                    } else {
                        tokens.push(SpannedToken {
                            token: Token::Plus,
                            line,
                        });
                    }
                }
                '-' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::MinusEq,
                            line,
                        });
                    } else {
                        tokens.push(SpannedToken {
                            token: Token::Minus,
                            line,
                        });
                    }
                }
                '*' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::StarEq,
                            line,
                        });
                    } else {
                        tokens.push(SpannedToken {
                            token: Token::Star,
                            line,
                        });
                    }
                }
                '/' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::SlashEq,
                            line,
                        });
                    } else {
                        tokens.push(SpannedToken {
                            token: Token::Slash,
                            line,
                        });
                    }
                }
                '%' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::PercentEq,
                            line,
                        });
                    } else {
                        tokens.push(SpannedToken {
                            token: Token::Percent,
                            line,
                        });
                    }
                }

                '=' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::EqEq,
                            line,
                        });
                    } else {
                        tokens.push(SpannedToken {
                            token: Token::Eq,
                            line,
                        });
                    }
                }
                '!' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::BangEq,
                            line,
                        });
                    } else {
                        tokens.push(SpannedToken {
                            token: Token::Bang,
                            line,
                        });
                    }
                }
                '<' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::LtEq,
                            line,
                        });
                    } else {
                        tokens.push(SpannedToken {
                            token: Token::Lt,
                            line,
                        });
                    }
                }
                '>' => {
                    self.advance();
                    if self.peek() == Some('=') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::GtEq,
                            line,
                        });
                    } else {
                        tokens.push(SpannedToken {
                            token: Token::Gt,
                            line,
                        });
                    }
                }

                '&' => {
                    self.advance();
                    if self.peek() == Some('&') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::AmpAmp,
                            line,
                        });
                    } else {
                        return Err(
                            self.error_with_hint("Unexpected `&`", "Did you mean `&&` (and)?")
                        );
                    }
                }
                '|' => {
                    self.advance();
                    if self.peek() == Some('|') {
                        self.advance();
                        tokens.push(SpannedToken {
                            token: Token::PipePipe,
                            line,
                        });
                    } else {
                        return Err(
                            self.error_with_hint("Unexpected `|`", "Did you mean `||` (or)?")
                        );
                    }
                }

                '(' => {
                    self.advance();
                    tokens.push(SpannedToken {
                        token: Token::LParen,
                        line,
                    });
                }
                ')' => {
                    self.advance();
                    tokens.push(SpannedToken {
                        token: Token::RParen,
                        line,
                    });
                }
                '[' => {
                    self.advance();
                    tokens.push(SpannedToken {
                        token: Token::LBracket,
                        line,
                    });
                }
                ']' => {
                    self.advance();
                    tokens.push(SpannedToken {
                        token: Token::RBracket,
                        line,
                    });
                }
                '{' => {
                    self.advance();
                    tokens.push(SpannedToken {
                        token: Token::LBrace,
                        line,
                    });
                }
                '}' => {
                    self.advance();
                    tokens.push(SpannedToken {
                        token: Token::RBrace,
                        line,
                    });
                }
                ',' => {
                    self.advance();
                    tokens.push(SpannedToken {
                        token: Token::Comma,
                        line,
                    });
                }
                ':' => {
                    self.advance();
                    tokens.push(SpannedToken {
                        token: Token::Colon,
                        line,
                    });
                }
                '.' => {
                    self.advance();
                    tokens.push(SpannedToken {
                        token: Token::Dot,
                        line,
                    });
                }
                ';' => {
                    self.advance();
                    tokens.push(SpannedToken {
                        token: Token::Semicolon,
                        line,
                    });
                }

                _ => {
                    return Err(self.error(format!("I don't understand the character `{}`", ch)));
                }
            }
        }

        Ok(tokens)
    }

    fn lex_number(&mut self) -> Result<Token, BopError> {
        let mut s = String::new();
        while let Some(ch) = self.peek() {
            if ch.is_ascii_digit() {
                s.push(ch);
                self.advance();
            } else {
                break;
            }
        }
        if self.peek() == Some('.') && self.peek_next().is_some_and(|c| c.is_ascii_digit()) {
            s.push('.');
            self.advance();
            while let Some(ch) = self.peek() {
                if ch.is_ascii_digit() {
                    s.push(ch);
                    self.advance();
                } else {
                    break;
                }
            }
        }
        let n: f64 = s
            .parse()
            .map_err(|_| self.error(format!("Invalid number: {}", s)))?;
        Ok(Token::Number(n))
    }

    fn lex_ident_or_keyword(&mut self) -> Token {
        let mut s = String::new();
        while let Some(ch) = self.peek() {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                s.push(ch);
                self.advance();
            } else {
                break;
            }
        }
        match s.as_str() {
            "let" => Token::Let,
            "fn" => Token::Fn,
            "return" => Token::Return,
            "if" => Token::If,
            "else" => Token::Else,
            "while" => Token::While,
            "for" => Token::For,
            "in" => Token::In,
            "repeat" => Token::Repeat,
            "break" => Token::Break,
            "continue" => Token::Continue,
            "true" => Token::True,
            "false" => Token::False,
            "none" => Token::None,
            _ => Token::Ident(s),
        }
    }

    fn lex_string(&mut self) -> Result<Token, BopError> {
        self.advance(); // consume opening "
        let mut parts: Vec<StringPart> = Vec::new();
        let mut current = String::new();

        loop {
            match self.peek() {
                None | Some('\n') => {
                    return Err(self.error_with_hint(
                        "This string is missing its closing `\"`",
                        "Every string needs to start and end with quotes.",
                    ));
                }
                Some('"') => {
                    self.advance();
                    break;
                }
                Some('\\') => {
                    self.advance();
                    match self.peek() {
                        Some('"') => {
                            current.push('"');
                            self.advance();
                        }
                        Some('\\') => {
                            current.push('\\');
                            self.advance();
                        }
                        Some('n') => {
                            current.push('\n');
                            self.advance();
                        }
                        Some('t') => {
                            current.push('\t');
                            self.advance();
                        }
                        Some('{') => {
                            current.push('{');
                            self.advance();
                        }
                        Some('}') => {
                            current.push('}');
                            self.advance();
                        }
                        Some(c) => {
                            return Err(self.error(format!("Unknown escape sequence `\\{}`", c)));
                        }
                        None => {
                            return Err(self.error("Unexpected end of string after `\\`"));
                        }
                    }
                }
                Some('{')
                    if self
                        .peek_next()
                        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_') =>
                {
                    self.advance(); // consume {
                    // Read variable name
                    let mut var = String::new();
                    while let Some(ch) = self.peek() {
                        if ch.is_ascii_alphanumeric() || ch == '_' {
                            var.push(ch);
                            self.advance();
                        } else {
                            break;
                        }
                    }
                    if self.peek() != Some('}') {
                        return Err(self.error_with_hint(
                            format!("Missing `}}` after `{{{}`", var),
                            "String interpolation needs a closing `}`, like: \"{name}\"",
                        ));
                    }
                    self.advance(); // consume }
                    if !current.is_empty() {
                        parts.push(StringPart::Literal(std::mem::take(&mut current)));
                    }
                    parts.push(StringPart::Variable(var));
                }
                Some(ch) => {
                    current.push(ch);
                    self.advance();
                }
            }
        }

        if parts.is_empty() {
            // Plain string, no interpolation
            Ok(Token::Str(current))
        } else {
            if !current.is_empty() {
                parts.push(StringPart::Literal(current));
            }
            Ok(Token::StringInterp(parts))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lex and strip Eof, returning just token variants
    fn toks(code: &str) -> Vec<Token> {
        lex(code)
            .unwrap()
            .into_iter()
            .map(|t| t.token)
            .filter(|t| !matches!(t, Token::Eof))
            .collect()
    }

    fn lex_err(code: &str) -> String {
        lex(code).unwrap_err().message
    }

    // ─── Numbers ───────────────────────────────────────────────────

    #[test]
    fn integer() {
        assert_eq!(toks("42"), vec![Token::Number(42.0)]);
    }

    #[test]
    fn float() {
        assert_eq!(toks("3.14"), vec![Token::Number(3.14)]);
    }

    #[test]
    fn leading_zero_float() {
        assert_eq!(toks("0.5"), vec![Token::Number(0.5)]);
    }

    // ─── Strings ───────────────────────────────────────────────────

    #[test]
    fn plain_string() {
        assert_eq!(toks(r#""hello""#), vec![Token::Str("hello".into())]);
    }

    #[test]
    fn escape_sequences() {
        assert_eq!(
            toks(r#""a\nb\t\\\"c""#),
            vec![Token::Str("a\nb\t\\\"c".into())]
        );
    }

    #[test]
    fn string_interpolation() {
        assert_eq!(
            toks(r#""hi {name}!""#),
            vec![Token::StringInterp(vec![
                StringPart::Literal("hi ".into()),
                StringPart::Variable("name".into()),
                StringPart::Literal("!".into()),
            ])]
        );
    }

    #[test]
    fn string_interpolation_multiple_vars() {
        assert_eq!(
            toks(r#""{x},{y}""#),
            vec![Token::StringInterp(vec![
                StringPart::Variable("x".into()),
                StringPart::Literal(",".into()),
                StringPart::Variable("y".into()),
            ])]
        );
    }

    #[test]
    fn unterminated_string() {
        assert!(lex_err(r#""hello"#).contains("missing its closing"));
    }

    #[test]
    fn unknown_escape() {
        assert!(lex_err(r#""hello\q""#).contains("Unknown escape"));
    }

    // ─── Keywords vs Identifiers ───────────────────────────────────

    #[test]
    fn keywords() {
        assert_eq!(
            toks("let fn return if else while for in repeat break continue true false none"),
            vec![
                Token::Let,
                Token::Fn,
                Token::Return,
                Token::If,
                Token::Else,
                Token::While,
                Token::For,
                Token::In,
                Token::Repeat,
                Token::Break,
                Token::Continue,
                Token::True,
                Token::False,
                Token::None,
            ]
        );
    }

    #[test]
    fn identifiers() {
        assert_eq!(
            toks("foo bar_baz _x abc123"),
            vec![
                Token::Ident("foo".into()),
                Token::Ident("bar_baz".into()),
                Token::Ident("_x".into()),
                Token::Ident("abc123".into()),
            ]
        );
    }

    // ─── Operators ─────────────────────────────────────────────────

    #[test]
    fn single_char_ops() {
        assert_eq!(
            toks("+ - * / % = ! < > ( ) [ ] { } , : . ;"),
            vec![
                Token::Plus,
                Token::Minus,
                Token::Star,
                Token::Slash,
                Token::Percent,
                Token::Eq,
                Token::Bang,
                Token::Lt,
                Token::Gt,
                Token::LParen,
                Token::RParen,
                Token::LBracket,
                Token::RBracket,
                Token::LBrace,
                Token::RBrace,
                Token::Comma,
                Token::Colon,
                Token::Dot,
                Token::Semicolon,
            ]
        );
    }

    #[test]
    fn double_char_ops() {
        assert_eq!(
            toks("== != <= >= && || += -= *= /= %="),
            vec![
                Token::EqEq,
                Token::BangEq,
                Token::LtEq,
                Token::GtEq,
                Token::AmpAmp,
                Token::PipePipe,
                Token::PlusEq,
                Token::MinusEq,
                Token::StarEq,
                Token::SlashEq,
                Token::PercentEq,
            ]
        );
    }

    #[test]
    fn lone_ampersand_error() {
        assert!(lex_err("&x").contains("Unexpected `&`"));
    }

    #[test]
    fn lone_pipe_error() {
        assert!(lex_err("|x").contains("Unexpected `|`"));
    }

    // ─── Comments ──────────────────────────────────────────────────

    #[test]
    fn line_comment_skipped() {
        assert_eq!(
            toks("1 // comment\n2"),
            vec![Token::Number(1.0), Token::Semicolon, Token::Number(2.0),]
        );
    }

    #[test]
    fn comment_at_end() {
        assert_eq!(toks("x // done"), vec![Token::Ident("x".into())]);
    }

    // ─── Auto-semicolons ──────────────────────────────────────────

    #[test]
    fn auto_semi_after_ident() {
        assert_eq!(
            toks("x\ny"),
            vec![
                Token::Ident("x".into()),
                Token::Semicolon,
                Token::Ident("y".into()),
            ]
        );
    }

    #[test]
    fn auto_semi_after_number() {
        assert_eq!(
            toks("42\n10"),
            vec![Token::Number(42.0), Token::Semicolon, Token::Number(10.0),]
        );
    }

    #[test]
    fn auto_semi_after_rparen() {
        assert_eq!(
            toks("f()\ng()"),
            vec![
                Token::Ident("f".into()),
                Token::LParen,
                Token::RParen,
                Token::Semicolon,
                Token::Ident("g".into()),
                Token::LParen,
                Token::RParen,
            ]
        );
    }

    #[test]
    fn auto_semi_after_rbrace() {
        assert_eq!(
            toks("{\n}\nx"),
            vec![
                Token::LBrace,
                Token::RBrace,
                Token::Semicolon,
                Token::Ident("x".into()),
            ]
        );
    }

    #[test]
    fn no_semi_after_open_delim() {
        assert_eq!(toks("{\nx"), vec![Token::LBrace, Token::Ident("x".into()),]);
    }

    #[test]
    fn no_semi_after_operator() {
        assert_eq!(
            toks("x +\ny"),
            vec![
                Token::Ident("x".into()),
                Token::Plus,
                Token::Ident("y".into()),
            ]
        );
    }

    #[test]
    fn auto_semi_after_break_continue_return() {
        assert_eq!(
            toks("break\ncontinue\nreturn"),
            vec![
                Token::Break,
                Token::Semicolon,
                Token::Continue,
                Token::Semicolon,
                Token::Return,
            ]
        );
    }

    #[test]
    fn auto_semi_after_true_false_none() {
        assert_eq!(
            toks("true\nfalse\nnone"),
            vec![
                Token::True,
                Token::Semicolon,
                Token::False,
                Token::Semicolon,
                Token::None,
            ]
        );
    }

    // ─── Line tracking ─────────────────────────────────────────────

    #[test]
    fn line_numbers() {
        let tokens = lex("x\ny\nz").unwrap();
        let lines: Vec<u32> = tokens.iter().map(|t| t.line).collect();
        // x(L1), ;(L1), y(L2), ;(L2), z(L3), Eof(L3)
        assert_eq!(lines, vec![1, 1, 2, 2, 3, 3]);
    }

    // ─── Unknown character ─────────────────────────────────────────

    #[test]
    fn unknown_char() {
        assert!(lex_err("@").contains("don't understand"));
    }
}
