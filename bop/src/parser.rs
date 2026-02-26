use crate::error::BopError;
use crate::lexer::{SpannedToken, StringPart, Token};

// ─── AST ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub line: u32,
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    Number(f64),
    Str(String),
    StringInterp(Vec<StringPart>),
    Bool(bool),
    None,
    Ident(String),
    BinaryOp {
        left: Box<Expr>,
        op: BinOp,
        right: Box<Expr>,
    },
    UnaryOp {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
    },
    MethodCall {
        object: Box<Expr>,
        method: String,
        args: Vec<Expr>,
    },
    Index {
        object: Box<Expr>,
        index: Box<Expr>,
    },
    Array(Vec<Expr>),
    Dict(Vec<(String, Expr)>),
    IfExpr {
        condition: Box<Expr>,
        then_expr: Box<Expr>,
        else_expr: Box<Expr>,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    NotEq,
    Lt,
    Gt,
    LtEq,
    GtEq,
    And,
    Or,
}

#[derive(Debug, Clone, Copy)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Debug, Clone)]
pub struct Stmt {
    pub kind: StmtKind,
    pub line: u32,
}

#[derive(Debug, Clone)]
pub enum StmtKind {
    Let {
        name: String,
        value: Expr,
    },
    Assign {
        target: AssignTarget,
        op: AssignOp,
        value: Expr,
    },
    If {
        condition: Expr,
        body: Vec<Stmt>,
        else_ifs: Vec<(Expr, Vec<Stmt>)>,
        else_body: Option<Vec<Stmt>>,
    },
    While {
        condition: Expr,
        body: Vec<Stmt>,
    },
    Repeat {
        count: Expr,
        body: Vec<Stmt>,
    },
    ForIn {
        var: String,
        iterable: Expr,
        body: Vec<Stmt>,
    },
    FnDecl {
        name: String,
        params: Vec<String>,
        body: Vec<Stmt>,
    },
    Return {
        value: Option<Expr>,
    },
    Break,
    Continue,
    ExprStmt(Expr),
}

#[derive(Debug, Clone)]
pub enum AssignTarget {
    Variable(String),
    Index { object: Expr, index: Expr },
}

#[derive(Debug, Clone, Copy)]
pub enum AssignOp {
    Eq,
    AddEq,
    SubEq,
    MulEq,
    DivEq,
    ModEq,
}

// ─── Parser ────────────────────────────────────────────────────────────────

const MAX_PARSE_DEPTH: usize = 128;

pub fn parse(tokens: Vec<SpannedToken>) -> Result<Vec<Stmt>, BopError> {
    let mut parser = Parser::new(tokens);
    parser.parse_program()
}

struct Parser {
    tokens: Vec<SpannedToken>,
    pos: usize,
    depth: usize,
}

impl Parser {
    fn new(tokens: Vec<SpannedToken>) -> Self {
        Self {
            tokens,
            pos: 0,
            depth: 0,
        }
    }

    fn enter(&mut self) -> Result<(), BopError> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            Err(self.error(self.peek_line(), "Code is nested too deeply"))
        } else {
            Ok(())
        }
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    fn peek(&self) -> &Token {
        self.tokens
            .get(self.pos)
            .map(|t| &t.token)
            .unwrap_or(&Token::Eof)
    }

    fn peek_line(&self) -> u32 {
        self.tokens.get(self.pos).map(|t| t.line).unwrap_or(0)
    }

    fn advance(&mut self) -> &Token {
        let tok = self
            .tokens
            .get(self.pos)
            .map(|t| &t.token)
            .unwrap_or(&Token::Eof);
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    fn is_at_end(&self) -> bool {
        matches!(self.peek(), Token::Eof)
    }

    fn expect(&mut self, expected: &Token) -> Result<u32, BopError> {
        let line = self.peek_line();
        if self.peek() == expected {
            self.advance();
            Ok(line)
        } else {
            Err(self.error(
                line,
                format!(
                    "Expected `{}` but found `{}`",
                    fmt_token(expected),
                    fmt_token(self.peek())
                ),
            ))
        }
    }

    fn expect_ident(&mut self) -> Result<(String, u32), BopError> {
        let line = self.peek_line();
        if let Token::Ident(name) = self.peek().clone() {
            self.advance();
            Ok((name, line))
        } else {
            Err(self.error(
                line,
                format!("Expected a name but found `{}`", fmt_token(self.peek())),
            ))
        }
    }

    fn skip_semicolons(&mut self) {
        while matches!(self.peek(), Token::Semicolon) {
            self.advance();
        }
    }

    fn error(&self, line: u32, message: impl Into<String>) -> BopError {
        BopError {
            line: Some(line),
            column: None,
            message: message.into(),
            friendly_hint: None,
        }
    }

    // ─── Program & Blocks ──────────────────────────────────────────────

    fn parse_program(&mut self) -> Result<Vec<Stmt>, BopError> {
        let mut stmts = Vec::new();
        self.skip_semicolons();
        while !self.is_at_end() {
            stmts.push(self.parse_statement()?);
            self.skip_semicolons();
        }
        Ok(stmts)
    }

    fn parse_block(&mut self) -> Result<Vec<Stmt>, BopError> {
        self.enter()?;
        self.expect(&Token::LBrace)?;
        let mut stmts = Vec::new();
        self.skip_semicolons();
        while !matches!(self.peek(), Token::RBrace | Token::Eof) {
            stmts.push(self.parse_statement()?);
            self.skip_semicolons();
        }
        self.expect(&Token::RBrace)?;
        self.leave();
        Ok(stmts)
    }

    // ─── Statements ────────────────────────────────────────────────────

    fn parse_statement(&mut self) -> Result<Stmt, BopError> {
        let line = self.peek_line();
        match self.peek() {
            Token::Let => self.parse_let(),
            Token::If => self.parse_if_stmt(),
            Token::While => self.parse_while(),
            Token::For => self.parse_for(),
            Token::Repeat => self.parse_repeat(),
            Token::Fn => self.parse_fn_decl(),
            Token::Return => self.parse_return(),
            Token::Break => {
                self.advance();
                Ok(Stmt {
                    kind: StmtKind::Break,
                    line,
                })
            }
            Token::Continue => {
                self.advance();
                Ok(Stmt {
                    kind: StmtKind::Continue,
                    line,
                })
            }
            _ => self.parse_expr_or_assign(),
        }
    }

    fn parse_let(&mut self) -> Result<Stmt, BopError> {
        let line = self.peek_line();
        self.advance(); // consume 'let'
        let (name, _) = self.expect_ident()?;
        self.expect(&Token::Eq)?;
        let value = self.parse_expr()?;
        Ok(Stmt {
            kind: StmtKind::Let { name, value },
            line,
        })
    }

    fn parse_if_stmt(&mut self) -> Result<Stmt, BopError> {
        let line = self.peek_line();
        self.advance(); // consume 'if'
        let condition = self.parse_expr()?;
        let body = self.parse_block()?;

        let mut else_ifs = Vec::new();
        let mut else_body = None;

        while matches!(self.peek(), Token::Else) {
            self.advance(); // consume 'else'
            if matches!(self.peek(), Token::If) {
                self.advance(); // consume 'if'
                let cond = self.parse_expr()?;
                let block = self.parse_block()?;
                else_ifs.push((cond, block));
            } else {
                else_body = Some(self.parse_block()?);
                break;
            }
        }

        Ok(Stmt {
            kind: StmtKind::If {
                condition,
                body,
                else_ifs,
                else_body,
            },
            line,
        })
    }

    fn parse_while(&mut self) -> Result<Stmt, BopError> {
        let line = self.peek_line();
        self.advance(); // consume 'while'
        let condition = self.parse_expr()?;
        let body = self.parse_block()?;
        Ok(Stmt {
            kind: StmtKind::While { condition, body },
            line,
        })
    }

    fn parse_for(&mut self) -> Result<Stmt, BopError> {
        let line = self.peek_line();
        self.advance(); // consume 'for'
        let (var, _) = self.expect_ident()?;
        self.expect(&Token::In)?;
        let iterable = self.parse_expr()?;
        let body = self.parse_block()?;
        Ok(Stmt {
            kind: StmtKind::ForIn {
                var,
                iterable,
                body,
            },
            line,
        })
    }

    fn parse_repeat(&mut self) -> Result<Stmt, BopError> {
        let line = self.peek_line();
        self.advance(); // consume 'repeat'
        let count = self.parse_expr()?;
        let body = self.parse_block()?;
        Ok(Stmt {
            kind: StmtKind::Repeat { count, body },
            line,
        })
    }

    fn parse_fn_decl(&mut self) -> Result<Stmt, BopError> {
        let line = self.peek_line();
        self.advance(); // consume 'fn'
        let (name, _) = self.expect_ident()?;
        self.expect(&Token::LParen)?;

        let mut params = Vec::new();
        if !matches!(self.peek(), Token::RParen) {
            let (p, _) = self.expect_ident()?;
            params.push(p);
            while matches!(self.peek(), Token::Comma) {
                self.advance();
                let (p, _) = self.expect_ident()?;
                params.push(p);
            }
        }
        self.expect(&Token::RParen)?;
        let body = self.parse_block()?;
        Ok(Stmt {
            kind: StmtKind::FnDecl { name, params, body },
            line,
        })
    }

    fn parse_return(&mut self) -> Result<Stmt, BopError> {
        let line = self.peek_line();
        self.advance(); // consume 'return'
        let value = if matches!(self.peek(), Token::Semicolon | Token::RBrace | Token::Eof) {
            None
        } else {
            Some(self.parse_expr()?)
        };
        Ok(Stmt {
            kind: StmtKind::Return { value },
            line,
        })
    }

    fn parse_expr_or_assign(&mut self) -> Result<Stmt, BopError> {
        let line = self.peek_line();
        let expr = self.parse_expr()?;

        let op = match self.peek() {
            Token::Eq => Some(AssignOp::Eq),
            Token::PlusEq => Some(AssignOp::AddEq),
            Token::MinusEq => Some(AssignOp::SubEq),
            Token::StarEq => Some(AssignOp::MulEq),
            Token::SlashEq => Some(AssignOp::DivEq),
            Token::PercentEq => Some(AssignOp::ModEq),
            _ => None,
        };

        if let Some(op) = op {
            self.advance(); // consume assignment operator
            let target = expr_to_assign_target(expr, line)?;
            let value = self.parse_expr()?;
            Ok(Stmt {
                kind: StmtKind::Assign { target, op, value },
                line,
            })
        } else {
            Ok(Stmt {
                kind: StmtKind::ExprStmt(expr),
                line,
            })
        }
    }

    // ─── Expressions ───────────────────────────────────────────────────

    fn parse_expr(&mut self) -> Result<Expr, BopError> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<Expr, BopError> {
        let mut left = self.parse_and()?;
        while matches!(self.peek(), Token::PipePipe) {
            let line = self.peek_line();
            self.advance();
            let right = self.parse_and()?;
            left = Expr {
                kind: ExprKind::BinaryOp {
                    left: Box::new(left),
                    op: BinOp::Or,
                    right: Box::new(right),
                },
                line,
            };
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, BopError> {
        let mut left = self.parse_equality()?;
        while matches!(self.peek(), Token::AmpAmp) {
            let line = self.peek_line();
            self.advance();
            let right = self.parse_equality()?;
            left = Expr {
                kind: ExprKind::BinaryOp {
                    left: Box::new(left),
                    op: BinOp::And,
                    right: Box::new(right),
                },
                line,
            };
        }
        Ok(left)
    }

    fn parse_equality(&mut self) -> Result<Expr, BopError> {
        let mut left = self.parse_comparison()?;
        while matches!(self.peek(), Token::EqEq | Token::BangEq) {
            let line = self.peek_line();
            let op = if matches!(self.peek(), Token::EqEq) {
                BinOp::Eq
            } else {
                BinOp::NotEq
            };
            self.advance();
            let right = self.parse_comparison()?;
            left = Expr {
                kind: ExprKind::BinaryOp {
                    left: Box::new(left),
                    op,
                    right: Box::new(right),
                },
                line,
            };
        }
        Ok(left)
    }

    fn parse_comparison(&mut self) -> Result<Expr, BopError> {
        let mut left = self.parse_addition()?;
        while matches!(
            self.peek(),
            Token::Lt | Token::Gt | Token::LtEq | Token::GtEq
        ) {
            let line = self.peek_line();
            let op = match self.peek() {
                Token::Lt => BinOp::Lt,
                Token::Gt => BinOp::Gt,
                Token::LtEq => BinOp::LtEq,
                _ => BinOp::GtEq,
            };
            self.advance();
            let right = self.parse_addition()?;
            left = Expr {
                kind: ExprKind::BinaryOp {
                    left: Box::new(left),
                    op,
                    right: Box::new(right),
                },
                line,
            };
        }
        Ok(left)
    }

    fn parse_addition(&mut self) -> Result<Expr, BopError> {
        let mut left = self.parse_multiply()?;
        while matches!(self.peek(), Token::Plus | Token::Minus) {
            let line = self.peek_line();
            let op = if matches!(self.peek(), Token::Plus) {
                BinOp::Add
            } else {
                BinOp::Sub
            };
            self.advance();
            let right = self.parse_multiply()?;
            left = Expr {
                kind: ExprKind::BinaryOp {
                    left: Box::new(left),
                    op,
                    right: Box::new(right),
                },
                line,
            };
        }
        Ok(left)
    }

    fn parse_multiply(&mut self) -> Result<Expr, BopError> {
        let mut left = self.parse_unary()?;
        while matches!(self.peek(), Token::Star | Token::Slash | Token::Percent) {
            let line = self.peek_line();
            let op = match self.peek() {
                Token::Star => BinOp::Mul,
                Token::Slash => BinOp::Div,
                _ => BinOp::Mod,
            };
            self.advance();
            let right = self.parse_unary()?;
            left = Expr {
                kind: ExprKind::BinaryOp {
                    left: Box::new(left),
                    op,
                    right: Box::new(right),
                },
                line,
            };
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr, BopError> {
        self.enter()?;
        let line = self.peek_line();
        let result = match self.peek() {
            Token::Bang => {
                self.advance();
                let expr = self.parse_unary()?;
                Ok(Expr {
                    kind: ExprKind::UnaryOp {
                        op: UnaryOp::Not,
                        expr: Box::new(expr),
                    },
                    line,
                })
            }
            Token::Minus => {
                self.advance();
                let expr = self.parse_unary()?;
                Ok(Expr {
                    kind: ExprKind::UnaryOp {
                        op: UnaryOp::Neg,
                        expr: Box::new(expr),
                    },
                    line,
                })
            }
            _ => self.parse_postfix(),
        };
        self.leave();
        result
    }

    fn parse_postfix(&mut self) -> Result<Expr, BopError> {
        let mut expr = self.parse_primary()?;

        loop {
            match self.peek() {
                Token::LParen => {
                    let line = self.peek_line();
                    self.advance();
                    let args = self.parse_args()?;
                    self.expect(&Token::RParen)?;
                    expr = Expr {
                        kind: ExprKind::Call {
                            callee: Box::new(expr),
                            args,
                        },
                        line,
                    };
                }
                Token::LBracket => {
                    let line = self.peek_line();
                    self.advance();
                    let index = self.parse_expr()?;
                    self.expect(&Token::RBracket)?;
                    expr = Expr {
                        kind: ExprKind::Index {
                            object: Box::new(expr),
                            index: Box::new(index),
                        },
                        line,
                    };
                }
                Token::Dot => {
                    let line = self.peek_line();
                    self.advance();
                    let (method, _) = self.expect_ident()?;
                    self.expect(&Token::LParen)?;
                    let args = self.parse_args()?;
                    self.expect(&Token::RParen)?;
                    expr = Expr {
                        kind: ExprKind::MethodCall {
                            object: Box::new(expr),
                            method,
                            args,
                        },
                        line,
                    };
                }
                _ => break,
            }
        }

        Ok(expr)
    }

    fn parse_args(&mut self) -> Result<Vec<Expr>, BopError> {
        let mut args = Vec::new();
        if !matches!(self.peek(), Token::RParen) {
            args.push(self.parse_expr()?);
            while matches!(self.peek(), Token::Comma) {
                self.advance();
                args.push(self.parse_expr()?);
            }
        }
        Ok(args)
    }

    fn parse_primary(&mut self) -> Result<Expr, BopError> {
        let line = self.peek_line();

        match self.peek().clone() {
            Token::Number(n) => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Number(n),
                    line,
                })
            }
            Token::Str(s) => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Str(s),
                    line,
                })
            }
            Token::StringInterp(parts) => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::StringInterp(parts),
                    line,
                })
            }
            Token::True => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Bool(true),
                    line,
                })
            }
            Token::False => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Bool(false),
                    line,
                })
            }
            Token::None => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::None,
                    line,
                })
            }
            Token::Ident(name) => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Ident(name),
                    line,
                })
            }
            Token::LParen => {
                self.enter()?;
                self.advance();
                let expr = self.parse_expr()?;
                self.expect(&Token::RParen)?;
                self.leave();
                Ok(expr)
            }
            Token::LBracket => self.parse_array_literal(),
            Token::LBrace => self.parse_dict_literal(),
            Token::If => self.parse_if_expr(),
            _ => Err(self.error(
                line,
                format!("I didn't expect `{}` here", fmt_token(self.peek())),
            )),
        }
    }

    fn parse_array_literal(&mut self) -> Result<Expr, BopError> {
        let line = self.peek_line();
        self.advance(); // consume [
        let mut elements = Vec::new();
        if !matches!(self.peek(), Token::RBracket) {
            elements.push(self.parse_expr()?);
            while matches!(self.peek(), Token::Comma) {
                self.advance();
                if matches!(self.peek(), Token::RBracket) {
                    break; // trailing comma
                }
                elements.push(self.parse_expr()?);
            }
        }
        self.expect(&Token::RBracket)?;
        Ok(Expr {
            kind: ExprKind::Array(elements),
            line,
        })
    }

    fn parse_dict_literal(&mut self) -> Result<Expr, BopError> {
        let line = self.peek_line();
        self.advance(); // consume {
        let mut entries = Vec::new();
        if !matches!(self.peek(), Token::RBrace) {
            let key = self.expect_string_key()?;
            self.expect(&Token::Colon)?;
            let value = self.parse_expr()?;
            entries.push((key, value));
            while matches!(self.peek(), Token::Comma) {
                self.advance();
                if matches!(self.peek(), Token::RBrace) {
                    break; // trailing comma
                }
                let key = self.expect_string_key()?;
                self.expect(&Token::Colon)?;
                let value = self.parse_expr()?;
                entries.push((key, value));
            }
        }
        self.expect(&Token::RBrace)?;
        Ok(Expr {
            kind: ExprKind::Dict(entries),
            line,
        })
    }

    fn expect_string_key(&mut self) -> Result<String, BopError> {
        let line = self.peek_line();
        match self.peek().clone() {
            Token::Str(s) => {
                self.advance();
                Ok(s)
            }
            _ => Err(self.error(line, "Dict keys must be strings (in quotes)")),
        }
    }

    fn parse_if_expr(&mut self) -> Result<Expr, BopError> {
        let line = self.peek_line();
        self.advance(); // consume 'if'
        let condition = self.parse_expr()?;
        self.expect(&Token::LBrace)?;
        let then_expr = self.parse_expr()?;
        self.expect(&Token::RBrace)?;
        self.expect(&Token::Else)?;
        self.expect(&Token::LBrace)?;
        let else_expr = self.parse_expr()?;
        self.expect(&Token::RBrace)?;
        Ok(Expr {
            kind: ExprKind::IfExpr {
                condition: Box::new(condition),
                then_expr: Box::new(then_expr),
                else_expr: Box::new(else_expr),
            },
            line,
        })
    }
}

// ─── Instruction counting ───────────────────────────────────────────────────

/// Count instructions in a list of statements (AST-based, format-independent).
///
/// Every `Stmt` counts as 1 instruction. Compound statements (if/while/repeat/for)
/// recurse into their body. `FnDecl` counts as 1 but does NOT recurse into the
/// function body — this rewards defining reusable functions.
pub fn count_instructions(stmts: &[Stmt]) -> u32 {
    let mut count = 0u32;
    for stmt in stmts {
        count += 1; // the statement itself
        match &stmt.kind {
            StmtKind::If {
                body,
                else_ifs,
                else_body,
                ..
            } => {
                count += count_instructions(body);
                for (_, branch_body) in else_ifs {
                    count += count_instructions(branch_body);
                }
                if let Some(eb) = else_body {
                    count += count_instructions(eb);
                }
            }
            StmtKind::While { body, .. }
            | StmtKind::Repeat { body, .. }
            | StmtKind::ForIn { body, .. } => {
                count += count_instructions(body);
            }
            StmtKind::FnDecl { .. } => {
                // Don't recurse into function body — reward reuse
            }
            _ => {}
        }
    }
    count
}

// ─── Helpers ───────────────────────────────────────────────────────────────

fn expr_to_assign_target(expr: Expr, line: u32) -> Result<AssignTarget, BopError> {
    match expr.kind {
        ExprKind::Ident(name) => Ok(AssignTarget::Variable(name)),
        ExprKind::Index { object, index } => Ok(AssignTarget::Index {
            object: *object,
            index: *index,
        }),
        _ => Err(BopError {
            line: Some(line),
            column: None,
            message: "You can only assign to a variable or an index (like `arr[0]`)".to_string(),
            friendly_hint: None,
        }),
    }
}

pub fn fmt_token(token: &Token) -> &'static str {
    match token {
        Token::Number(_) => "a number",
        Token::Str(_) | Token::StringInterp(_) => "a string",
        Token::True => "true",
        Token::False => "false",
        Token::None => "none",
        Token::Ident(_) => "a name",
        Token::Let => "let",
        Token::Fn => "fn",
        Token::Return => "return",
        Token::If => "if",
        Token::Else => "else",
        Token::While => "while",
        Token::For => "for",
        Token::In => "in",
        Token::Repeat => "repeat",
        Token::Break => "break",
        Token::Continue => "continue",
        Token::Plus => "+",
        Token::Minus => "-",
        Token::Star => "*",
        Token::Slash => "/",
        Token::Percent => "%",
        Token::EqEq => "==",
        Token::BangEq => "!=",
        Token::Lt => "<",
        Token::Gt => ">",
        Token::LtEq => "<=",
        Token::GtEq => ">=",
        Token::AmpAmp => "&&",
        Token::PipePipe => "||",
        Token::Bang => "!",
        Token::Eq => "=",
        Token::PlusEq => "+=",
        Token::MinusEq => "-=",
        Token::StarEq => "*=",
        Token::SlashEq => "/=",
        Token::PercentEq => "%=",
        Token::LParen => "(",
        Token::RParen => ")",
        Token::LBracket => "[",
        Token::RBracket => "]",
        Token::LBrace => "{",
        Token::RBrace => "}",
        Token::Comma => ",",
        Token::Colon => ":",
        Token::Dot => ".",
        Token::Semicolon => ";",
        Token::Newline => "newline",
        Token::Eof => "end of code",
    }
}
