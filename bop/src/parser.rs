#[cfg(not(feature = "std"))]
use alloc::{boxed::Box, format, string::{String, ToString}, vec::Vec};

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
    /// Bare field read: `obj.field` (no parens after the field
    /// name). Distinct from `MethodCall`, which always has `(…)`.
    FieldAccess {
        object: Box<Expr>,
        field: String,
    },
    /// Struct literal: `Point { x: 1, y: 2 }`. Only parsed in
    /// contexts where struct literals are allowed — control-flow
    /// conditions and `for-in` iterables disallow them so that
    /// `if foo { body }` stays unambiguous.
    StructConstruct {
        type_name: String,
        fields: Vec<(String, Expr)>,
    },
    /// Enum variant construction: `Shape::Circle(5)`,
    /// `Shape::Rectangle { w: 4, h: 3 }`, `Shape::Empty`. The
    /// payload shape is determined at parse time from the syntax
    /// at the construction site.
    EnumConstruct {
        type_name: String,
        variant: String,
        payload: VariantPayload,
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
    /// Anonymous function expression: `fn(params) { body }`.
    /// Captures the referenced free variables from the enclosing
    /// scope when evaluated; see the evaluator for capture rules.
    Lambda {
        params: Vec<String>,
        body: Vec<Stmt>,
    },
    /// `match scrutinee { pat => body, ... }` — checks each arm
    /// top-to-bottom, evaluates the first matching arm's body,
    /// and returns its value. Raises a runtime error if no arm
    /// matches (exhaustiveness isn't checked statically in v1).
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
    },

    /// `try <expr>` — inspect a Result-shaped enum variant.
    /// If `<expr>` evaluates to an `Ok(value)`-shaped variant,
    /// unwrap to `value`. If it evaluates to an `Err(...)`-shaped
    /// variant, short-circuit the enclosing function's return
    /// with that value (same mechanism as a `return` statement).
    /// At top-level scope (no enclosing fn) or on a non-Result
    /// scrutinee, `try` raises a runtime error.
    ///
    /// Desugars roughly to the match:
    /// ```text
    /// match <expr> {
    ///     Ok(v) => v,
    ///     Err(_) => return <expr>,
    /// }
    /// ```
    /// but is its own AST node so each engine can compile it
    /// directly without paying the pattern-construction cost.
    Try(Box<Expr>),
}

#[derive(Debug, Clone)]
pub struct MatchArm {
    pub pattern: Pattern,
    /// Optional guard expression — evaluated after the pattern
    /// matches. A `false` guard skips to the next arm.
    pub guard: Option<Expr>,
    pub body: Expr,
    pub line: u32,
}

/// A pattern appears in `match` arms (phase 4 introduces this;
/// future phases may reuse it in `let` destructuring and fn
/// params). Structurally mirrors the runtime `Value` enum so each
/// variant's matcher reads as "does this value fit here?".
#[derive(Debug, Clone)]
pub enum Pattern {
    /// Matches a specific value verbatim: `1`, `"foo"`, `true`,
    /// `false`, `none`.
    Literal(LiteralPattern),
    /// `_` — matches anything, binds nothing.
    Wildcard,
    /// Bare identifier — matches anything, binds the value to
    /// this name for the arm's body.
    Binding(String),
    /// `Type::Variant` / `Type::Variant(...)` / `Type::Variant { ... }`.
    EnumVariant {
        type_name: String,
        variant: String,
        payload: VariantPatternPayload,
    },
    /// `Type { field: pat, field, .. }` destructures a struct.
    Struct {
        type_name: String,
        fields: Vec<(String, Pattern)>,
        rest: bool,
    },
    /// `[a, b, ..rest]` destructures an array.
    Array {
        elements: Vec<Pattern>,
        rest: Option<ArrayRest>,
    },
    /// `p1 | p2 | p3` — match if any alternative matches. Every
    /// alternative must introduce the same set of bindings.
    Or(Vec<Pattern>),
}

#[derive(Debug, Clone)]
pub enum LiteralPattern {
    Number(f64),
    Str(String),
    Bool(bool),
    None,
}

#[derive(Debug, Clone)]
pub enum VariantPatternPayload {
    Unit,
    Tuple(Vec<Pattern>),
    Struct {
        fields: Vec<(String, Pattern)>,
        rest: bool,
    },
}

/// What `..rest` does at the tail of an array pattern.
#[derive(Debug, Clone)]
pub enum ArrayRest {
    /// `..` — matches any remaining elements, binds nothing.
    Ignored,
    /// `..name` — captures remaining elements as an array.
    Named(String),
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
    /// `fn Type.method(self, ...) { body }` — declares a method
    /// on a user-defined struct or enum. At call time
    /// (`obj.method(...)`) the receiver is passed as the first
    /// parameter (conventionally named `self`), followed by the
    /// rest.
    MethodDecl {
        type_name: String,
        method_name: String,
        params: Vec<String>,
        body: Vec<Stmt>,
    },
    Return {
        value: Option<Expr>,
    },
    Break,
    Continue,
    /// `import foo.bar.baz` — resolves the module named by the
    /// dot-joined path through `BopHost::resolve_module`, evaluates
    /// its top-level statements in a fresh scope, and injects the
    /// module's `let` / `fn` bindings into the importer's scope.
    Import {
        path: String,
    },
    /// `struct Point { x, y }` — registers a user-defined struct
    /// type with the listed field names. Field values get their
    /// types from the construction site (`Point { x: 1, y: 2 }`).
    StructDecl {
        name: String,
        fields: Vec<String>,
    },
    /// `enum Shape { Circle(radius), Rectangle { w, h }, Empty }`
    /// — registers a user-defined sum type with named variants.
    EnumDecl {
        name: String,
        variants: Vec<VariantDecl>,
    },
    ExprStmt(Expr),
}

/// One variant of an `enum` declaration.
#[derive(Debug, Clone)]
pub struct VariantDecl {
    pub name: String,
    pub kind: VariantKind,
}

/// What shape a variant's payload takes.
#[derive(Debug, Clone)]
pub enum VariantKind {
    /// No payload — `Empty`.
    Unit,
    /// Positional payload — `Circle(radius)`.
    Tuple(Vec<String>),
    /// Named payload — `Rectangle { width, height }`.
    Struct(Vec<String>),
}

/// Runtime-side payload at a `T::Variant(...)` construction site.
#[derive(Debug, Clone)]
pub enum VariantPayload {
    Unit,
    Tuple(Vec<Expr>),
    Struct(Vec<(String, Expr)>),
}

#[derive(Debug, Clone)]
pub enum AssignTarget {
    Variable(String),
    Index { object: Expr, index: Expr },
    /// Assignment to a struct field: `obj.field = v`. Like
    /// `Index`, only a bare `Ident` for `object` is currently
    /// assignable — the runtime clones out, mutates, and writes
    /// back through the variable.
    Field { object: Expr, field: String },
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
    /// When false, `Ident { ... }` at expression position is
    /// *not* parsed as a struct literal — the `{` is left for the
    /// enclosing control-flow construct (e.g. `if foo { body }`,
    /// `for x in arr { body }`). Flipped off while parsing `if`
    /// / `while` conditions and `for-in` iterables.
    allow_struct_literal: bool,
}

impl Parser {
    fn new(tokens: Vec<SpannedToken>) -> Self {
        Self {
            tokens,
            pos: 0,
            depth: 0,
            allow_struct_literal: true,
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

    fn peek_at(&self, offset: usize) -> &Token {
        self.tokens
            .get(self.pos + offset)
            .map(|t| &t.token)
            .unwrap_or(&Token::Eof)
    }

    /// Run `f` with `allow_struct_literal = false`, restoring the
    /// prior value on exit. Used for `if` / `while` conditions and
    /// `for-in` iterables so `if foo { body }` doesn't mis-parse
    /// as `if (struct-literal-foo) { … }`.
    fn without_struct_literal<F, R>(&mut self, f: F) -> R
    where
        F: FnOnce(&mut Self) -> R,
    {
        let saved = self.allow_struct_literal;
        self.allow_struct_literal = false;
        let result = f(self);
        self.allow_struct_literal = saved;
        result
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
            is_fatal: false,
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
            // `fn name(...)` is a declaration; `fn(...)` is a
            // lambda expression at statement position — delegate
            // to the expression parser so it becomes an `ExprStmt`.
            Token::Fn if matches!(self.peek_at(1), Token::Ident(_)) => self.parse_fn_decl(),
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
            Token::Import => self.parse_import(),
            Token::Struct => self.parse_struct_decl(),
            Token::Enum => self.parse_enum_decl(),
            _ => self.parse_expr_or_assign(),
        }
    }

    fn parse_struct_decl(&mut self) -> Result<Stmt, BopError> {
        let line = self.peek_line();
        self.advance(); // consume `struct`
        let (name, _) = self.expect_ident()?;
        self.expect(&Token::LBrace)?;
        let mut fields = Vec::new();
        if !matches!(self.peek(), Token::RBrace) {
            let (f, _) = self.expect_ident()?;
            fields.push(f);
            while matches!(self.peek(), Token::Comma) {
                self.advance();
                if matches!(self.peek(), Token::RBrace) {
                    break; // trailing comma
                }
                let (f, _) = self.expect_ident()?;
                fields.push(f);
            }
        }
        self.expect(&Token::RBrace)?;
        Ok(Stmt {
            kind: StmtKind::StructDecl { name, fields },
            line,
        })
    }

    fn parse_enum_decl(&mut self) -> Result<Stmt, BopError> {
        let line = self.peek_line();
        self.advance(); // consume `enum`
        let (name, _) = self.expect_ident()?;
        self.expect(&Token::LBrace)?;
        let mut variants: Vec<VariantDecl> = Vec::new();
        if !matches!(self.peek(), Token::RBrace) {
            variants.push(self.parse_variant_decl()?);
            while matches!(self.peek(), Token::Comma) {
                self.advance();
                if matches!(self.peek(), Token::RBrace) {
                    break; // trailing comma
                }
                variants.push(self.parse_variant_decl()?);
            }
        }
        self.expect(&Token::RBrace)?;
        Ok(Stmt {
            kind: StmtKind::EnumDecl { name, variants },
            line,
        })
    }

    fn parse_variant_decl(&mut self) -> Result<VariantDecl, BopError> {
        let (name, _) = self.expect_ident()?;
        let kind = match self.peek() {
            Token::LParen => {
                self.advance();
                let mut fields: Vec<String> = Vec::new();
                if !matches!(self.peek(), Token::RParen) {
                    let (f, _) = self.expect_ident()?;
                    fields.push(f);
                    while matches!(self.peek(), Token::Comma) {
                        self.advance();
                        if matches!(self.peek(), Token::RParen) {
                            break;
                        }
                        let (f, _) = self.expect_ident()?;
                        fields.push(f);
                    }
                }
                self.expect(&Token::RParen)?;
                VariantKind::Tuple(fields)
            }
            Token::LBrace => {
                self.advance();
                let mut fields: Vec<String> = Vec::new();
                if !matches!(self.peek(), Token::RBrace) {
                    let (f, _) = self.expect_ident()?;
                    fields.push(f);
                    while matches!(self.peek(), Token::Comma) {
                        self.advance();
                        if matches!(self.peek(), Token::RBrace) {
                            break;
                        }
                        let (f, _) = self.expect_ident()?;
                        fields.push(f);
                    }
                }
                self.expect(&Token::RBrace)?;
                VariantKind::Struct(fields)
            }
            _ => VariantKind::Unit,
        };
        Ok(VariantDecl { name, kind })
    }

    fn parse_import(&mut self) -> Result<Stmt, BopError> {
        let line = self.peek_line();
        self.advance(); // consume `import`
        let (first, _) = self.expect_ident()?;
        let mut path = first;
        while matches!(self.peek(), Token::Dot) {
            self.advance();
            let (seg, _) = self.expect_ident()?;
            path.push('.');
            path.push_str(&seg);
        }
        Ok(Stmt {
            kind: StmtKind::Import { path },
            line,
        })
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
        let condition = self.without_struct_literal(|p| p.parse_expr())?;
        let body = self.parse_block()?;

        let mut else_ifs = Vec::new();
        let mut else_body = None;

        while matches!(self.peek(), Token::Else) {
            self.advance(); // consume 'else'
            if matches!(self.peek(), Token::If) {
                self.advance(); // consume 'if'
                let cond = self.without_struct_literal(|p| p.parse_expr())?;
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
        let condition = self.without_struct_literal(|p| p.parse_expr())?;
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
        let iterable = self.without_struct_literal(|p| p.parse_expr())?;
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
        let count = self.without_struct_literal(|p| p.parse_expr())?;
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

        // Method declaration: `fn Type.method(...)`. The leading
        // ident is the receiver's type; the post-dot ident is the
        // method's name. Everything else matches a regular fn
        // decl.
        if matches!(self.peek(), Token::Dot) {
            self.advance();
            let (method_name, _) = self.expect_ident()?;
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
            return Ok(Stmt {
                kind: StmtKind::MethodDecl {
                    type_name: name,
                    method_name,
                    params,
                    body,
                },
                line,
            });
        }

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
            Token::Try => {
                // `try <expr>` binds tighter than binary ops but
                // looser than postfix (calls, methods, indexing),
                // mirroring Rust's `?`. Recursing into
                // `parse_unary` lets `try try foo()` parse as
                // `try (try foo())` without a special case.
                self.advance();
                let expr = self.parse_unary()?;
                Ok(Expr {
                    kind: ExprKind::Try(Box::new(expr)),
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
                    let (name, _) = self.expect_ident()?;
                    if matches!(self.peek(), Token::LParen) {
                        // Method call: `.name(args)`.
                        self.advance();
                        let args = self.parse_args()?;
                        self.expect(&Token::RParen)?;
                        expr = Expr {
                            kind: ExprKind::MethodCall {
                                object: Box::new(expr),
                                method: name,
                                args,
                            },
                            line,
                        };
                    } else {
                        // Bare field read: `.name`.
                        expr = Expr {
                            kind: ExprKind::FieldAccess {
                                object: Box::new(expr),
                                field: name,
                            },
                            line,
                        };
                    }
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
                // Enum variant construction: `Type::Variant…`.
                // Always parse (the `::` is unambiguous); the
                // payload shape is determined by what follows
                // the variant name.
                if matches!(self.peek(), Token::ColonColon) {
                    return self.parse_enum_variant_tail(name, line);
                }
                // Struct literal: `Name { field: value, ... }`.
                // Parsed only when struct literals are allowed in
                // the current context (see
                // `without_struct_literal`). This keeps `if foo {
                // body }` / `for x in arr { body }` parseable.
                if self.allow_struct_literal && matches!(self.peek(), Token::LBrace) {
                    return self.parse_struct_literal(name, line);
                }
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
            Token::Fn => self.parse_lambda(),
            Token::Match => self.parse_match_expr(),
            _ => Err(self.error(
                line,
                format!("I didn't expect `{}` here", fmt_token(self.peek())),
            )),
        }
    }

    fn parse_enum_variant_tail(
        &mut self,
        type_name: String,
        line: u32,
    ) -> Result<Expr, BopError> {
        self.advance(); // consume `::`
        let (variant, _) = self.expect_ident()?;
        let payload = match self.peek() {
            Token::LParen => {
                self.enter()?;
                self.advance();
                let mut args: Vec<Expr> = Vec::new();
                if !matches!(self.peek(), Token::RParen) {
                    args.push(self.parse_expr()?);
                    while matches!(self.peek(), Token::Comma) {
                        self.advance();
                        if matches!(self.peek(), Token::RParen) {
                            break;
                        }
                        args.push(self.parse_expr()?);
                    }
                }
                self.expect(&Token::RParen)?;
                self.leave();
                VariantPayload::Tuple(args)
            }
            Token::LBrace if self.allow_struct_literal => {
                self.enter()?;
                self.advance();
                let mut fields: Vec<(String, Expr)> = Vec::new();
                if !matches!(self.peek(), Token::RBrace) {
                    let (fname, _) = self.expect_ident()?;
                    self.expect(&Token::Colon)?;
                    let fvalue = self.parse_expr()?;
                    fields.push((fname, fvalue));
                    while matches!(self.peek(), Token::Comma) {
                        self.advance();
                        if matches!(self.peek(), Token::RBrace) {
                            break;
                        }
                        let (fname, _) = self.expect_ident()?;
                        self.expect(&Token::Colon)?;
                        let fvalue = self.parse_expr()?;
                        fields.push((fname, fvalue));
                    }
                }
                self.expect(&Token::RBrace)?;
                self.leave();
                VariantPayload::Struct(fields)
            }
            _ => VariantPayload::Unit,
        };
        Ok(Expr {
            kind: ExprKind::EnumConstruct {
                type_name,
                variant,
                payload,
            },
            line,
        })
    }

    fn parse_struct_literal(&mut self, type_name: String, line: u32) -> Result<Expr, BopError> {
        self.enter()?;
        self.expect(&Token::LBrace)?;
        let mut fields: Vec<(String, Expr)> = Vec::new();
        if !matches!(self.peek(), Token::RBrace) {
            let (fname, _) = self.expect_ident()?;
            self.expect(&Token::Colon)?;
            let fvalue = self.parse_expr()?;
            fields.push((fname, fvalue));
            while matches!(self.peek(), Token::Comma) {
                self.advance();
                if matches!(self.peek(), Token::RBrace) {
                    break; // trailing comma
                }
                let (fname, _) = self.expect_ident()?;
                self.expect(&Token::Colon)?;
                let fvalue = self.parse_expr()?;
                fields.push((fname, fvalue));
            }
        }
        self.expect(&Token::RBrace)?;
        self.leave();
        Ok(Expr {
            kind: ExprKind::StructConstruct { type_name, fields },
            line,
        })
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

    fn parse_match_expr(&mut self) -> Result<Expr, BopError> {
        let line = self.peek_line();
        self.advance(); // consume 'match'
        // Scrutinee reads without struct-literal parsing — same
        // rule as `if` / `while` / `for`, so `match foo { ... }`
        // stays parseable.
        let scrutinee = self.without_struct_literal(|p| p.parse_expr())?;
        self.expect(&Token::LBrace)?;
        let mut arms: Vec<MatchArm> = Vec::new();
        self.skip_semicolons();
        while !matches!(self.peek(), Token::RBrace | Token::Eof) {
            arms.push(self.parse_match_arm()?);
            // Arms separate by `,` (common) or `;` (auto-semi from
            // newline). Accept and continue; also accept trailing
            // separators before the closing brace.
            while matches!(self.peek(), Token::Comma | Token::Semicolon) {
                self.advance();
            }
        }
        self.expect(&Token::RBrace)?;
        Ok(Expr {
            kind: ExprKind::Match {
                scrutinee: Box::new(scrutinee),
                arms,
            },
            line,
        })
    }

    fn parse_match_arm(&mut self) -> Result<MatchArm, BopError> {
        let line = self.peek_line();
        let pattern = self.parse_pattern()?;
        let guard = if matches!(self.peek(), Token::If) {
            self.advance();
            Some(self.without_struct_literal(|p| p.parse_expr())?)
        } else {
            None
        };
        self.expect(&Token::FatArrow)?;
        let body = self.parse_expr()?;
        Ok(MatchArm {
            pattern,
            guard,
            body,
            line,
        })
    }

    fn parse_pattern(&mut self) -> Result<Pattern, BopError> {
        let first = self.parse_pattern_single()?;
        // Or-pattern: `p1 | p2 | p3`. Keep the left-associative
        // tree flat in a single `Or` variant for ergonomic
        // matching later.
        if matches!(self.peek(), Token::Pipe) {
            let mut alts = vec![first];
            while matches!(self.peek(), Token::Pipe) {
                self.advance();
                alts.push(self.parse_pattern_single()?);
            }
            Ok(Pattern::Or(alts))
        } else {
            Ok(first)
        }
    }

    fn parse_pattern_single(&mut self) -> Result<Pattern, BopError> {
        self.enter()?;
        let line = self.peek_line();
        let result = match self.peek().clone() {
            Token::Number(n) => {
                self.advance();
                Ok(Pattern::Literal(LiteralPattern::Number(n)))
            }
            Token::Str(s) => {
                self.advance();
                Ok(Pattern::Literal(LiteralPattern::Str(s)))
            }
            Token::True => {
                self.advance();
                Ok(Pattern::Literal(LiteralPattern::Bool(true)))
            }
            Token::False => {
                self.advance();
                Ok(Pattern::Literal(LiteralPattern::Bool(false)))
            }
            Token::None => {
                self.advance();
                Ok(Pattern::Literal(LiteralPattern::None))
            }
            Token::Minus => {
                // Negative number literal: `-1`, `-3.14`.
                self.advance();
                match self.peek().clone() {
                    Token::Number(n) => {
                        self.advance();
                        Ok(Pattern::Literal(LiteralPattern::Number(-n)))
                    }
                    other => Err(self.error(
                        line,
                        format!(
                            "Expected a number after `-` in pattern, got `{}`",
                            fmt_token(&other)
                        ),
                    )),
                }
            }
            Token::LBracket => self.parse_pattern_array(),
            Token::Ident(name) if name == "_" => {
                self.advance();
                Ok(Pattern::Wildcard)
            }
            Token::Ident(name) => {
                self.advance();
                // `Type::Variant[...]` path pattern.
                if matches!(self.peek(), Token::ColonColon) {
                    self.parse_pattern_variant_tail(name)
                } else if matches!(self.peek(), Token::LBrace) {
                    // `Type { ... }` struct pattern — only when
                    // the `LBrace` is syntactically plausible as
                    // a struct pattern. Inside a match arm pattern
                    // it always is.
                    self.parse_pattern_struct(name)
                } else {
                    // Bare identifier = binding. `_` is handled
                    // above as wildcard.
                    Ok(Pattern::Binding(name))
                }
            }
            other => Err(self.error(
                line,
                format!("Expected a pattern, got `{}`", fmt_token(&other)),
            )),
        };
        self.leave();
        result
    }

    fn parse_pattern_array(&mut self) -> Result<Pattern, BopError> {
        self.advance(); // consume `[`
        let mut elements: Vec<Pattern> = Vec::new();
        let mut rest: Option<ArrayRest> = None;
        if !matches!(self.peek(), Token::RBracket) {
            loop {
                if matches!(self.peek(), Token::DotDot) {
                    self.advance();
                    // Optional name binding after `..`.
                    let captured = match self.peek().clone() {
                        Token::Ident(n) if n != "_" => {
                            self.advance();
                            ArrayRest::Named(n)
                        }
                        _ => ArrayRest::Ignored,
                    };
                    rest = Some(captured);
                    // `..` must be the last element in the array
                    // pattern; the parser enforces this by
                    // stopping here.
                    break;
                }
                elements.push(self.parse_pattern()?);
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                    if matches!(self.peek(), Token::RBracket) {
                        break; // trailing comma
                    }
                } else {
                    break;
                }
            }
        }
        self.expect(&Token::RBracket)?;
        Ok(Pattern::Array { elements, rest })
    }

    fn parse_pattern_variant_tail(
        &mut self,
        type_name: String,
    ) -> Result<Pattern, BopError> {
        self.advance(); // consume `::`
        let (variant, _) = self.expect_ident()?;
        let payload = match self.peek() {
            Token::LParen => {
                self.advance();
                let mut items: Vec<Pattern> = Vec::new();
                if !matches!(self.peek(), Token::RParen) {
                    items.push(self.parse_pattern()?);
                    while matches!(self.peek(), Token::Comma) {
                        self.advance();
                        if matches!(self.peek(), Token::RParen) {
                            break;
                        }
                        items.push(self.parse_pattern()?);
                    }
                }
                self.expect(&Token::RParen)?;
                VariantPatternPayload::Tuple(items)
            }
            Token::LBrace => {
                let (fields, rest) = self.parse_pattern_field_list()?;
                VariantPatternPayload::Struct { fields, rest }
            }
            _ => VariantPatternPayload::Unit,
        };
        Ok(Pattern::EnumVariant {
            type_name,
            variant,
            payload,
        })
    }

    fn parse_pattern_struct(&mut self, type_name: String) -> Result<Pattern, BopError> {
        let (fields, rest) = self.parse_pattern_field_list()?;
        Ok(Pattern::Struct {
            type_name,
            fields,
            rest,
        })
    }

    fn parse_pattern_field_list(
        &mut self,
    ) -> Result<(Vec<(String, Pattern)>, bool), BopError> {
        self.expect(&Token::LBrace)?;
        let mut fields: Vec<(String, Pattern)> = Vec::new();
        let mut rest = false;
        if !matches!(self.peek(), Token::RBrace) {
            loop {
                if matches!(self.peek(), Token::DotDot) {
                    self.advance();
                    rest = true;
                    break;
                }
                let (fname, _) = self.expect_ident()?;
                // Shorthand `{ field }` binds the field value to
                // a local named `field`. Full form `{ field: pat }`
                // lets the user nest or wildcard.
                let sub = if matches!(self.peek(), Token::Colon) {
                    self.advance();
                    self.parse_pattern()?
                } else {
                    Pattern::Binding(fname.clone())
                };
                fields.push((fname, sub));
                if matches!(self.peek(), Token::Comma) {
                    self.advance();
                    if matches!(self.peek(), Token::RBrace) {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        self.expect(&Token::RBrace)?;
        Ok((fields, rest))
    }

    fn parse_lambda(&mut self) -> Result<Expr, BopError> {
        let line = self.peek_line();
        self.advance(); // consume 'fn'
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
        Ok(Expr {
            kind: ExprKind::Lambda { params, body },
            line,
        })
    }

    fn parse_if_expr(&mut self) -> Result<Expr, BopError> {
        let line = self.peek_line();
        self.advance(); // consume 'if'
        let condition = self.without_struct_literal(|p| p.parse_expr())?;
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
        ExprKind::FieldAccess { object, field } => Ok(AssignTarget::Field {
            object: *object,
            field,
        }),
        _ => Err(BopError {
            line: Some(line),
            column: None,
            message:
                "You can only assign to a variable, an index (`arr[0]`), or a struct field (`point.x`)"
                    .to_string(),
            friendly_hint: None,
            is_fatal: false,
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
        Token::Import => "import",
        Token::Struct => "struct",
        Token::Enum => "enum",
        Token::Match => "match",
        Token::Try => "try",
        Token::ColonColon => "::",
        Token::DotDot => "..",
        Token::FatArrow => "=>",
        Token::Pipe => "|",
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
