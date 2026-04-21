#[cfg(feature = "no_std")]
use alloc::{boxed::Box, format, string::{String, ToString}, vec, vec::Vec};

use crate::error::BopError;
use crate::lexer::{SpannedToken, StringPart, Token};
use crate::naming;

// ─── Naming helpers ─────────────────────────────────────────────────────
//
// Enforce the shape rules defined in `bop::naming` at every
// identifier-introducing site. Each `ensure_*_name` returns a
// parse error whose `message` says what the site expects and
// whose `friendly_hint` offers a concrete rename — cheap to
// generate, and makes errors read like the compiler wants to
// help rather than just complain.

fn ident_shape_error(site: &str, expected: &str, actual: &str, line: u32) -> BopError {
    let actual_label = naming::kind_label(naming::classify(actual));
    let message = format!(
        "{} `{}` looks like a {}, but a {} name is required here",
        site, actual, actual_label, expected
    );
    let mut err = BopError::runtime(message, line);
    err.friendly_hint = Some(naming::hint_for(expected, actual));
    err
}

/// Require a lowercase-first (or leading-underscore) identifier
/// at a `let` / `fn` / param / field / method / alias /
/// match-binding / `for-in` / `use` alias site.
fn ensure_value_name(name: &str, site: &str, line: u32) -> Result<(), BopError> {
    if naming::is_value_name(name) {
        Ok(())
    } else {
        Err(ident_shape_error(site, "value", name, line))
    }
}

/// Require an uppercase-first identifier at a `struct` / `enum` /
/// variant site. Both PascalCase and ALL_CAPS are accepted —
/// `struct Entity {}`, `enum Dir { N, E, S, W }`, and
/// `struct HTTP {}` all pass.
fn ensure_type_name(name: &str, site: &str, line: u32) -> Result<(), BopError> {
    if naming::is_type_name(name) {
        Ok(())
    } else {
        Err(ident_shape_error(site, "type", name, line))
    }
}

/// Require an all-uppercase identifier at a `const` site.
fn ensure_constant_name(name: &str, site: &str, line: u32) -> Result<(), BopError> {
    if naming::is_constant_name(name) {
        Ok(())
    } else {
        Err(ident_shape_error(site, "constant", name, line))
    }
}

// ─── AST ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub line: u32,
    /// 1-indexed column where this expression starts in the
    /// source. `None` on synthetic nodes that don't correspond
    /// to a specific source position. Niche-packed into 4 bytes
    /// so the column field costs nothing beyond a plain `u32`
    /// — runtime error construction reads it out to render a
    /// carat under the offending character.
    pub column: Option<core::num::NonZeroU32>,
}

impl Expr {
    /// Build an `Expr` from its kind and a 1-indexed source
    /// line, leaving `column` unset. Convenience constructor
    /// for call sites that don't have a token column handy
    /// (synthetic / desugared nodes, for instance).
    pub fn line(kind: ExprKind, line: u32) -> Self {
        Self { kind, line, column: None }
    }

    /// Build an `Expr` with a full source position. `column`
    /// is typically `NonZeroU32::new(tok.column)`.
    pub fn at(kind: ExprKind, line: u32, column: Option<core::num::NonZeroU32>) -> Self {
        Self { kind, line, column }
    }
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    /// Integer literal (phase 6). Produced by integer tokens
    /// like `42`; distinct from `Number` so each engine can
    /// emit a `Value::Int(i64)` directly.
    Int(i64),
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
        /// `Some("m")` for `m.Entity { ... }` — a namespaced
        /// struct literal through a module alias. `None` for
        /// the unqualified `Entity { ... }` form. The walker /
        /// VM / AOT resolve the (namespace, type_name) pair via
        /// the current scope's type-alias table before
        /// constructing the `Value::Struct`.
        namespace: Option<String>,
        type_name: String,
        fields: Vec<(String, Expr)>,
    },
    /// Enum variant construction: `Shape::Circle(5)`,
    /// `Shape::Rectangle { w: 4, h: 3 }`, `Shape::Empty`. The
    /// payload shape is determined at parse time from the syntax
    /// at the construction site.
    EnumConstruct {
        /// `Some("r")` for `r.Result::Ok(v)` — a namespaced
        /// variant constructor through a module alias. `None`
        /// for unqualified `Result::Ok(v)`.
        namespace: Option<String>,
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
    /// `Type::Variant` / `Type::Variant(...)` / `Type::Variant { ... }`,
    /// optionally namespaced via `ns.Type::Variant(...)`.
    EnumVariant {
        namespace: Option<String>,
        type_name: String,
        variant: String,
        payload: VariantPatternPayload,
    },
    /// `Type { field: pat, field, .. }` destructures a struct,
    /// optionally namespaced via `ns.Type { ... }`.
    Struct {
        namespace: Option<String>,
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
    /// Integer literal pattern (e.g. `match x { 1 => ... }`).
    /// Added in phase 6 alongside `Value::Int`.
    Int(i64),
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
    /// 1-indexed column where this statement starts. See
    /// [`Expr::column`] — same niche-packed shape, same
    /// purpose (carat rendering on runtime errors).
    pub column: Option<core::num::NonZeroU32>,
}

impl Stmt {
    /// Build a `Stmt` from its kind and a 1-indexed source
    /// line, leaving `column` unset. See
    /// [`Expr::line`].
    pub fn line(kind: StmtKind, line: u32) -> Self {
        Self { kind, line, column: None }
    }

    /// Build a `Stmt` with a full source position.
    pub fn at(kind: StmtKind, line: u32, column: Option<core::num::NonZeroU32>) -> Self {
        Self { kind, line, column }
    }
}

#[derive(Debug, Clone)]
pub enum StmtKind {
    /// `let NAME = expr` (value binding, mutable) and `const NAME
    /// = expr` (constant binding, immutable). The `is_const` flag
    /// flips enforcement at use/assign sites: reassigning a
    /// constant is a compile-time error (the parser refuses any
    /// assignment whose LHS is an all-uppercase identifier — see
    /// [`parse_assign`]).
    Let {
        name: String,
        value: Expr,
        is_const: bool,
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
    /// `use foo.bar.baz` — resolves the module named by the
    /// dot-joined path through `BopHost::resolve_module`,
    /// evaluates its top-level statements in a fresh scope, and
    /// injects its exports into the importer's scope. The shape
    /// of the injection depends on the optional `items` / `alias`:
    ///
    /// - `use foo`                  — glob: all non-private
    ///   top-level names land unqualified.
    /// - `use foo.{a, b}`           — selective: only `a` and
    ///   `b` land unqualified. `_`-prefixed names can be
    ///   explicitly listed.
    /// - `use foo as m`             — aliased: all exports
    ///   (including `_`-prefixed) hang off a new `m` namespace
    ///   value. Access via `m.a`, `m.Entity`, etc.
    /// - `use foo.{a, b} as m`      — selective + aliased:
    ///   `m` namespace contains only `a` and `b`.
    ///
    /// Glob imports skip `_`-prefixed top-level names (privacy
    /// convention). Aliased and selective imports pass them
    /// through when the user asks for them explicitly.
    Use {
        path: String,
        /// `Some` iff the caller used the selective `.{a, b}`
        /// form. The listed names are injected (whatever shape
        /// they have); anything not listed is skipped.
        items: Option<Vec<String>>,
        /// `Some("m")` iff the caller used the `as m` form.
        /// Exports are bound inside a `Value::Module` under this
        /// name rather than in the caller's scope directly.
        alias: Option<String>,
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
#[derive(Debug, Clone, PartialEq)]
pub struct VariantDecl {
    pub name: String,
    pub kind: VariantKind,
}

/// What shape a variant's payload takes.
#[derive(Debug, Clone, PartialEq)]
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

    /// 1-indexed column of the current token's first character.
    /// Used for parse-error reporting so the carat under the
    /// offending line points at the right place.
    fn peek_column(&self) -> u32 {
        self.tokens.get(self.pos).map(|t| t.column).unwrap_or(0)
    }

    /// Grab both the line and the niche-packed column of the
    /// current token in one shot. Shorthand used at the head
    /// of parse fns that build an `Expr` / `Stmt` — rather than
    /// calling `peek_line()` then having to re-fetch column
    /// later, capture the source position once.
    fn peek_pos(&self) -> (u32, Option<core::num::NonZeroU32>) {
        let tok = self.tokens.get(self.pos);
        let line = tok.map(|t| t.line).unwrap_or(0);
        let column = tok
            .map(|t| t.column)
            .and_then(core::num::NonZeroU32::new);
        (line, column)
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
        let (line, _column) = self.peek_pos();
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
        let (line, _column) = self.peek_pos();
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
        // Use the current token's column; if we've already
        // advanced past the token that triggered the complaint,
        // column 0 (unknown) is the honest answer rather than
        // silently misreporting.
        let column = if self.tokens.get(self.pos).map(|t| t.line) == Some(line) {
            Some(self.peek_column())
        } else {
            None
        };
        BopError {
            line: Some(line),
            column,
            message: message.into(),
            friendly_hint: None,
            is_fatal: false,
            is_try_return: false,
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
        let (line, column) = self.peek_pos();
        match self.peek() {
            Token::Let => self.parse_let(),
            Token::Const => self.parse_const(),
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
                    column,
                })
            }
            Token::Continue => {
                self.advance();
                Ok(Stmt {
                    kind: StmtKind::Continue,
                    line,
                    column,
                })
            }
            Token::Use => self.parse_use(),
            Token::Struct => self.parse_struct_decl(),
            Token::Enum => self.parse_enum_decl(),
            _ => self.parse_expr_or_assign(),
        }
    }

    fn parse_struct_decl(&mut self) -> Result<Stmt, BopError> {
        let (line, column) = self.peek_pos();
        self.advance(); // consume `struct`
        let (name, name_line) = self.expect_ident()?;
        ensure_type_name(&name, "`struct` declaration", name_line)?;
        self.expect(&Token::LBrace)?;
        let mut fields = Vec::new();
        if !matches!(self.peek(), Token::RBrace) {
            let (f, f_line) = self.expect_ident()?;
            ensure_value_name(&f, "struct field", f_line)?;
            fields.push(f);
            while matches!(self.peek(), Token::Comma) {
                self.advance();
                if matches!(self.peek(), Token::RBrace) {
                    break; // trailing comma
                }
                let (f, f_line) = self.expect_ident()?;
                ensure_value_name(&f, "struct field", f_line)?;
                fields.push(f);
            }
        }
        self.expect(&Token::RBrace)?;
        Ok(Stmt {
            kind: StmtKind::StructDecl { name, fields },
            line,
                    column,
        })
    }

    fn parse_enum_decl(&mut self) -> Result<Stmt, BopError> {
        let (line, column) = self.peek_pos();
        self.advance(); // consume `enum`
        let (name, name_line) = self.expect_ident()?;
        ensure_type_name(&name, "`enum` declaration", name_line)?;
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
                    column,
        })
    }

    fn parse_variant_decl(&mut self) -> Result<VariantDecl, BopError> {
        let (name, name_line) = self.expect_ident()?;
        ensure_type_name(&name, "enum variant", name_line)?;
        let kind = match self.peek() {
            Token::LParen => {
                self.advance();
                let mut fields: Vec<String> = Vec::new();
                if !matches!(self.peek(), Token::RParen) {
                    let (f, f_line) = self.expect_ident()?;
                    ensure_value_name(&f, "variant payload field", f_line)?;
                    fields.push(f);
                    while matches!(self.peek(), Token::Comma) {
                        self.advance();
                        if matches!(self.peek(), Token::RParen) {
                            break;
                        }
                        let (f, f_line) = self.expect_ident()?;
                        ensure_value_name(&f, "variant payload field", f_line)?;
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
                    let (f, f_line) = self.expect_ident()?;
                    ensure_value_name(&f, "variant payload field", f_line)?;
                    fields.push(f);
                    while matches!(self.peek(), Token::Comma) {
                        self.advance();
                        if matches!(self.peek(), Token::RBrace) {
                            break;
                        }
                        let (f, f_line) = self.expect_ident()?;
                        ensure_value_name(&f, "variant payload field", f_line)?;
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

    fn parse_use(&mut self) -> Result<Stmt, BopError> {
        let (line, column) = self.peek_pos();
        self.advance(); // consume `use`
        let (first, first_line) = self.expect_ident()?;
        ensure_value_name(&first, "module path segment", first_line)?;
        let mut path = first;

        // Consume dotted path segments. Breaks early if we see a
        // `.{` — the selective-import opener.
        loop {
            if !matches!(self.peek(), Token::Dot) {
                break;
            }
            // Peek ahead one more token: `.{` opens selective
            // imports; `.ident` continues the path.
            if matches!(self.peek_at(1), Token::LBrace) {
                break;
            }
            self.advance(); // consume '.'
            let (seg, seg_line) = self.expect_ident()?;
            ensure_value_name(&seg, "module path segment", seg_line)?;
            path.push('.');
            path.push_str(&seg);
        }

        // Selective import: `use foo.bar.{a, b, c}`.
        let items = if matches!(self.peek(), Token::Dot) {
            self.advance(); // '.'
            self.expect(&Token::LBrace)?;
            let mut list: Vec<String> = Vec::new();
            if !matches!(self.peek(), Token::RBrace) {
                let (name, _) = self.expect_ident()?;
                list.push(name);
                while matches!(self.peek(), Token::Comma) {
                    self.advance();
                    if matches!(self.peek(), Token::RBrace) {
                        break; // trailing comma
                    }
                    let (name, _) = self.expect_ident()?;
                    list.push(name);
                }
            }
            self.expect(&Token::RBrace)?;
            Some(list)
        } else {
            None
        };

        // Optional `as alias`.
        let alias = if matches!(self.peek(), Token::As) {
            self.advance();
            let (name, name_line) = self.expect_ident()?;
            ensure_value_name(&name, "`use` alias", name_line)?;
            Some(name)
        } else {
            None
        };

        Ok(Stmt {
            kind: StmtKind::Use { path, items, alias },
            line,
                    column,
        })
    }

    fn parse_let(&mut self) -> Result<Stmt, BopError> {
        let (line, column) = self.peek_pos();
        self.advance(); // consume 'let'
        let (name, _) = self.expect_ident()?;
        ensure_value_name(&name, "`let` binding", line)?;
        self.expect(&Token::Eq)?;
        let value = self.parse_expr()?;
        Ok(Stmt {
            kind: StmtKind::Let { name, value, is_const: false },
            line,
                    column,
        })
    }

    /// `const NAME = expr` — immutable binding, SCREAMING_SNAKE_CASE
    /// name enforced. Shares the `StmtKind::Let` variant with a
    /// `is_const: true` flag — the runtime treats constants as
    /// let bindings that were parsed in a way that makes
    /// reassignment impossible (the parser rejects any `=` whose
    /// LHS is an all-uppercase identifier).
    fn parse_const(&mut self) -> Result<Stmt, BopError> {
        let (line, column) = self.peek_pos();
        self.advance(); // consume 'const'
        let (name, _) = self.expect_ident()?;
        ensure_constant_name(&name, "`const` declaration", line)?;
        self.expect(&Token::Eq)?;
        let value = self.parse_expr()?;
        Ok(Stmt {
            kind: StmtKind::Let { name, value, is_const: true },
            line,
                    column,
        })
    }

    fn parse_if_stmt(&mut self) -> Result<Stmt, BopError> {
        let (line, column) = self.peek_pos();
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
                    column,
        })
    }

    fn parse_while(&mut self) -> Result<Stmt, BopError> {
        let (line, column) = self.peek_pos();
        self.advance(); // consume 'while'
        let condition = self.without_struct_literal(|p| p.parse_expr())?;
        let body = self.parse_block()?;
        Ok(Stmt {
            kind: StmtKind::While { condition, body },
            line,
                    column,
        })
    }

    fn parse_for(&mut self) -> Result<Stmt, BopError> {
        let (line, column) = self.peek_pos();
        self.advance(); // consume 'for'
        let (var, var_line) = self.expect_ident()?;
        ensure_value_name(&var, "`for` loop variable", var_line)?;
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
                    column,
        })
    }

    fn parse_repeat(&mut self) -> Result<Stmt, BopError> {
        let (line, column) = self.peek_pos();
        self.advance(); // consume 'repeat'
        let count = self.without_struct_literal(|p| p.parse_expr())?;
        let body = self.parse_block()?;
        Ok(Stmt {
            kind: StmtKind::Repeat { count, body },
            line,
                    column,
        })
    }

    fn parse_fn_decl(&mut self) -> Result<Stmt, BopError> {
        let (line, column) = self.peek_pos();
        self.advance(); // consume 'fn'
        let (name, name_line) = self.expect_ident()?;

        // Method declaration: `fn Type.method(...)`. The leading
        // ident is the receiver's type; the post-dot ident is the
        // method's name. Everything else matches a regular fn
        // decl.
        if matches!(self.peek(), Token::Dot) {
            ensure_type_name(&name, "method receiver", name_line)?;
            self.advance();
            let (method_name, method_line) = self.expect_ident()?;
            ensure_value_name(&method_name, "method name", method_line)?;
            self.expect(&Token::LParen)?;
            let mut params = Vec::new();
            if !matches!(self.peek(), Token::RParen) {
                let (p, p_line) = self.expect_ident()?;
                ensure_value_name(&p, "method parameter", p_line)?;
                params.push(p);
                while matches!(self.peek(), Token::Comma) {
                    self.advance();
                    let (p, p_line) = self.expect_ident()?;
                    ensure_value_name(&p, "method parameter", p_line)?;
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
                    column,
            });
        }

        ensure_value_name(&name, "`fn` declaration", name_line)?;
        self.expect(&Token::LParen)?;

        let mut params = Vec::new();
        if !matches!(self.peek(), Token::RParen) {
            let (p, p_line) = self.expect_ident()?;
            ensure_value_name(&p, "function parameter", p_line)?;
            params.push(p);
            while matches!(self.peek(), Token::Comma) {
                self.advance();
                let (p, p_line) = self.expect_ident()?;
                ensure_value_name(&p, "function parameter", p_line)?;
                params.push(p);
            }
        }
        self.expect(&Token::RParen)?;
        let body = self.parse_block()?;
        Ok(Stmt {
            kind: StmtKind::FnDecl { name, params, body },
            line,
                    column,
        })
    }

    fn parse_return(&mut self) -> Result<Stmt, BopError> {
        let (line, column) = self.peek_pos();
        self.advance(); // consume 'return'
        let value = if matches!(self.peek(), Token::Semicolon | Token::RBrace | Token::Eof) {
            None
        } else {
            Some(self.parse_expr()?)
        };
        Ok(Stmt {
            kind: StmtKind::Return { value },
            line,
                    column,
        })
    }

    fn parse_expr_or_assign(&mut self) -> Result<Stmt, BopError> {
        let (line, column) = self.peek_pos();
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
                    column,
            })
        } else {
            Ok(Stmt {
                kind: StmtKind::ExprStmt(expr),
                line,
                    column,
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
            let (line, column) = self.peek_pos();
            self.advance();
            let right = self.parse_and()?;
            left = Expr {
                kind: ExprKind::BinaryOp {
                    left: Box::new(left),
                    op: BinOp::Or,
                    right: Box::new(right),
                },
                line,
                    column,
            };
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<Expr, BopError> {
        let mut left = self.parse_equality()?;
        while matches!(self.peek(), Token::AmpAmp) {
            let (line, column) = self.peek_pos();
            self.advance();
            let right = self.parse_equality()?;
            left = Expr {
                kind: ExprKind::BinaryOp {
                    left: Box::new(left),
                    op: BinOp::And,
                    right: Box::new(right),
                },
                line,
                    column,
            };
        }
        Ok(left)
    }

    fn parse_equality(&mut self) -> Result<Expr, BopError> {
        let mut left = self.parse_comparison()?;
        while matches!(self.peek(), Token::EqEq | Token::BangEq) {
            let (line, column) = self.peek_pos();
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
                    column,
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
            let (line, column) = self.peek_pos();
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
                    column,
            };
        }
        Ok(left)
    }

    fn parse_addition(&mut self) -> Result<Expr, BopError> {
        let mut left = self.parse_multiply()?;
        while matches!(self.peek(), Token::Plus | Token::Minus) {
            let (line, column) = self.peek_pos();
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
                    column,
            };
        }
        Ok(left)
    }

    fn parse_multiply(&mut self) -> Result<Expr, BopError> {
        let mut left = self.parse_unary()?;
        while matches!(
            self.peek(),
            Token::Star | Token::Slash | Token::Percent
        ) {
            let (line, column) = self.peek_pos();
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
                    column,
            };
        }
        Ok(left)
    }

    fn parse_unary(&mut self) -> Result<Expr, BopError> {
        self.enter()?;
        let (line, column) = self.peek_pos();
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
                    column,
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
                    column,
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
                    column,
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
                    let (line, column) = self.peek_pos();
                    self.advance();
                    let args = self.parse_args()?;
                    self.expect(&Token::RParen)?;
                    expr = Expr {
                        kind: ExprKind::Call {
                            callee: Box::new(expr),
                            args,
                        },
                        line,
                    column,
                    };
                }
                Token::LBracket => {
                    let (line, column) = self.peek_pos();
                    self.advance();
                    let index = self.parse_expr()?;
                    self.expect(&Token::RBracket)?;
                    expr = Expr {
                        kind: ExprKind::Index {
                            object: Box::new(expr),
                            index: Box::new(index),
                        },
                        line,
                    column,
                    };
                }
                Token::Dot => {
                    let (line, column) = self.peek_pos();
                    self.advance();
                    let (name, _) = self.expect_ident()?;

                    // `a.B::V(...)` / `a.B { ... }` — namespaced
                    // type access through a module alias. We only
                    // take this path when the receiver is a bare
                    // `Ident` (the alias) and the field is a
                    // type-shape name. Anything else (method
                    // call, plain field read) falls through.
                    if let ExprKind::Ident(ns) = &expr.kind {
                        if naming::is_type_name(&name) {
                            match self.peek() {
                                Token::ColonColon => {
                                    let ns_owned = ns.clone();
                                    expr = self.parse_enum_variant_tail(
                                        name,
                                        Some(ns_owned),
                                        line,
                                        expr.column,
                                    )?;
                                    continue;
                                }
                                Token::LBrace if self.allow_struct_literal => {
                                    let ns_owned = ns.clone();
                                    expr = self.parse_struct_literal(
                                        name,
                                        Some(ns_owned),
                                        line,
                                        expr.column,
                                    )?;
                                    continue;
                                }
                                _ => {}
                            }
                        }
                    }

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
                    column,
                        };
                    } else {
                        // Bare field read: `.name`.
                        expr = Expr {
                            kind: ExprKind::FieldAccess {
                                object: Box::new(expr),
                                field: name,
                            },
                            line,
                    column,
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
        let (line, column) = self.peek_pos();

        match self.peek().clone() {
            Token::Int(n) => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Int(n),
                    line,
                    column,
                })
            }
            Token::Number(n) => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Number(n),
                    line,
                    column,
                })
            }
            Token::Str(s) => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Str(s),
                    line,
                    column,
                })
            }
            Token::StringInterp(parts) => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::StringInterp(parts),
                    line,
                    column,
                })
            }
            Token::True => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Bool(true),
                    line,
                    column,
                })
            }
            Token::False => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::Bool(false),
                    line,
                    column,
                })
            }
            Token::None => {
                self.advance();
                Ok(Expr {
                    kind: ExprKind::None,
                    line,
                    column,
                })
            }
            Token::Ident(name) => {
                self.advance();
                // Sugar: bare `Ok(args)` / `Err(args)` desugars
                // to the built-in `Result::Ok(args)` /
                // `Result::Err(args)`. Bop's case rules already
                // reserve uppercase idents for type / variant
                // names, so `Ok` and `Err` can't collide with a
                // user fn or variable. Users who want the `Ok` /
                // `Err` variants of a *different* enum have to
                // name it explicitly (`MyEnum::Ok(...)`).
                if (name == "Ok" || name == "Err")
                    && matches!(self.peek(), Token::LParen)
                {
                    return self.parse_result_shorthand(name, line, column);
                }
                // Enum variant construction: `Type::Variant…`.
                // Always parse (the `::` is unambiguous); the
                // payload shape is determined by what follows
                // the variant name.
                if matches!(self.peek(), Token::ColonColon) {
                    return self.parse_enum_variant_tail(name, None, line, column);
                }
                // Struct literal: `Name { field: value, ... }`.
                // Parsed only when struct literals are allowed in
                // the current context (see
                // `without_struct_literal`). This keeps `if foo {
                // body }` / `for x in arr { body }` parseable.
                if self.allow_struct_literal && matches!(self.peek(), Token::LBrace) {
                    return self.parse_struct_literal(name, None, line, column);
                }
                Ok(Expr {
                    kind: ExprKind::Ident(name),
                    line,
                    column,
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
        namespace: Option<String>,
        line: u32,
        column: Option<core::num::NonZeroU32>,
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
                namespace,
                type_name,
                variant,
                payload,
            },
            line,
                    column,
        })
    }

    /// `Ok(args)` / `Err(args)` — parser-level sugar for the
    /// built-in `Result::Ok(args)` / `Result::Err(args)` so user
    /// code can skip the `Result::` prefix for the two variants
    /// used overwhelmingly often. The caller already advanced
    /// past the identifier and verified the lookahead is
    /// `LParen`; `variant` must be `"Ok"` or `"Err"`.
    fn parse_result_shorthand(
        &mut self,
        variant: String,
        line: u32,
        column: Option<core::num::NonZeroU32>,
    ) -> Result<Expr, BopError> {
        debug_assert!(variant == "Ok" || variant == "Err");
        self.enter()?;
        self.expect(&Token::LParen)?;
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
        Ok(Expr {
            kind: ExprKind::EnumConstruct {
                namespace: None,
                type_name: String::from("Result"),
                variant,
                payload: VariantPayload::Tuple(args),
            },
            line,
            column,
        })
    }

    fn parse_struct_literal(
        &mut self,
        type_name: String,
        namespace: Option<String>,
        line: u32,
        column: Option<core::num::NonZeroU32>,
    ) -> Result<Expr, BopError> {
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
            kind: ExprKind::StructConstruct {
                namespace,
                type_name,
                fields,
            },
            line,
                    column,
        })
    }

    fn parse_array_literal(&mut self) -> Result<Expr, BopError> {
        let (line, column) = self.peek_pos();
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
                    column,
        })
    }

    fn parse_dict_literal(&mut self) -> Result<Expr, BopError> {
        let (line, column) = self.peek_pos();
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
                    column,
        })
    }

    fn expect_string_key(&mut self) -> Result<String, BopError> {
        let (line, _column) = self.peek_pos();
        match self.peek().clone() {
            Token::Str(s) => {
                self.advance();
                Ok(s)
            }
            _ => Err(self.error(line, "Dict keys must be strings (in quotes)")),
        }
    }

    fn parse_match_expr(&mut self) -> Result<Expr, BopError> {
        let (line, column) = self.peek_pos();
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
                    column,
        })
    }

    fn parse_match_arm(&mut self) -> Result<MatchArm, BopError> {
        let (line, _column) = self.peek_pos();
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
        let (line, _column) = self.peek_pos();
        let result = match self.peek().clone() {
            Token::Int(n) => {
                self.advance();
                Ok(Pattern::Literal(LiteralPattern::Int(n)))
            }
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
                    Token::Int(n) => {
                        self.advance();
                        // `i64::MIN` has no positive counterpart
                        // — negating it overflows. Raise a clear
                        // parse error rather than silently
                        // wrapping.
                        match n.checked_neg() {
                            Some(neg) => Ok(Pattern::Literal(LiteralPattern::Int(neg))),
                            None => Err(self.error(
                                line,
                                format!(
                                    "Integer literal `-{}` is out of range for i64",
                                    n
                                ),
                            )),
                        }
                    }
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
                // Sugar: `Ok(p)` / `Err(p)` as patterns — mirror
                // the expression-side shortcut. Reduces the
                // `match r { Result::Ok(v) => ..., Result::Err(e)
                // => ... }` boilerplate to plain `Ok(v)` / `Err(e)`.
                if (name == "Ok" || name == "Err")
                    && matches!(self.peek(), Token::LParen)
                {
                    return self.parse_result_shorthand_pattern(name);
                }
                // `Type::Variant[...]` path pattern.
                if matches!(self.peek(), Token::ColonColon) {
                    self.parse_pattern_variant_tail(name, None)
                } else if matches!(self.peek(), Token::LBrace) {
                    // `Type { ... }` struct pattern — only when
                    // the `LBrace` is syntactically plausible as
                    // a struct pattern. Inside a match arm pattern
                    // it always is.
                    self.parse_pattern_struct(name, None)
                } else if matches!(self.peek(), Token::Dot)
                    && naming::is_value_name(&name)
                {
                    // `ns.Type...` — namespaced variant / struct
                    // pattern through a module alias. Only fires
                    // when the first segment is value-shaped
                    // (an alias), to keep bare `Type.field` from
                    // being misread as a pattern.
                    self.advance(); // consume '.'
                    let (type_name, type_line) = self.expect_ident()?;
                    if !naming::is_type_name(&type_name) {
                        return Err(self.error(
                            type_line,
                            format!(
                                "Expected a type name after `{}.` in pattern, got `{}`",
                                name, type_name
                            ),
                        ));
                    }
                    if matches!(self.peek(), Token::ColonColon) {
                        self.parse_pattern_variant_tail(type_name, Some(name))
                    } else if matches!(self.peek(), Token::LBrace) {
                        self.parse_pattern_struct(type_name, Some(name))
                    } else {
                        Err(self.error(
                            type_line,
                            format!(
                                "Expected `::Variant(...)` or `{{...}}` after `{}.{}` in pattern",
                                name, type_name
                            ),
                        ))
                    }
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
        namespace: Option<String>,
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
            namespace,
            type_name,
            variant,
            payload,
        })
    }

    /// Pattern-side mirror of [`Self::parse_result_shorthand`]:
    /// `Ok(p)` / `Err(p)` in a pattern desugar to the built-in
    /// `Result::Ok(p)` / `Result::Err(p)`. Caller has already
    /// advanced past the ident and verified `LParen` follows;
    /// `variant` must be `"Ok"` or `"Err"`.
    fn parse_result_shorthand_pattern(
        &mut self,
        variant: String,
    ) -> Result<Pattern, BopError> {
        debug_assert!(variant == "Ok" || variant == "Err");
        self.expect(&Token::LParen)?;
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
        Ok(Pattern::EnumVariant {
            namespace: None,
            type_name: String::from("Result"),
            variant,
            payload: VariantPatternPayload::Tuple(items),
        })
    }

    fn parse_pattern_struct(
        &mut self,
        type_name: String,
        namespace: Option<String>,
    ) -> Result<Pattern, BopError> {
        let (fields, rest) = self.parse_pattern_field_list()?;
        Ok(Pattern::Struct {
            namespace,
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
        let (line, column) = self.peek_pos();
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
                    column,
        })
    }

    fn parse_if_expr(&mut self) -> Result<Expr, BopError> {
        let (line, column) = self.peek_pos();
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
                    column,
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
        ExprKind::Ident(name) => {
            // All-caps LHS → reassigning a constant. Refused at
            // parse time without needing scope tracking: the
            // parser already forbids `let` / `fn` bindings with
            // an all-caps shape, so any all-caps identifier in
            // the source must have come from a `const` declaration
            // (or is undeclared, in which case we give the user
            // the right kind of diagnostic anyway).
            if naming::is_constant_name(&name) {
                let mut err = BopError::runtime(
                    format!("can't reassign `{}` — it's a constant", name),
                    line,
                );
                err.friendly_hint = Some(
                    "constants are immutable. Use `let` if you want a mutable binding."
                        .to_string(),
                );
                return Err(err);
            }
            Ok(AssignTarget::Variable(name))
        }
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
            is_try_return: false,
        }),
    }
}

pub fn fmt_token(token: &Token) -> &'static str {
    match token {
        Token::Int(_) => "an integer",
        Token::Number(_) => "a number",
        Token::Str(_) | Token::StringInterp(_) => "a string",
        Token::True => "true",
        Token::False => "false",
        Token::None => "none",
        Token::Ident(_) => "a name",
        Token::Let => "let",
        Token::Const => "const",
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
        Token::Use => "use",
        Token::As => "as",
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
