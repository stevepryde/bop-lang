# Grammar

An informal grammar for the Bop language, plus the complete list of reserved words.

## Reserved words

```
let const fn return
if else while for in repeat break continue
use as struct enum match try
true false none
```

These can't be used as variable or function names. There are no word-spelled logical operators — use `&&`, `||`, `!` rather than `and`, `or`, `not`.

## Built-in functions

Always in scope; can't be shadowed by user fns. See [Built-in Functions](builtins.md) for details.

| Function | Returns | Description |
|----------|---------|-------------|
| `print(args...)` | none | Host-captured output |
| `range(n)` / `range(s, e)` / `range(s, e, step)` | array | Integer range |
| `rand(n)` | int | Pseudo-random `0..n` |
| `try_call(f)` | Result | Run `f`, return `Ok(v)` or `Err(RuntimeError)` |
| `panic(message)` | never returns | Raise a non-fatal runtime error carrying `message` |

All math and conversion operations are [methods on values](methods.md): `x.type()`, `x.to_str()`, `x.to_int()`, `x.to_float()`, `x.abs()`, `a.min(b)`, `x.sqrt()`, `x.len()`, etc.

## Grammar

Informal EBNF. Whitespace is insignificant *except* for newlines, which auto-insert semicolons (see below).

```
program     = statement*
statement   = letDecl | constDecl | assign | ifStmt | whileStmt | repeatStmt
            | forStmt | fnDecl | returnStmt | breakStmt | continueStmt
            | useStmt | structDecl | enumDecl | methodDecl
            | exprStmt

letDecl     = "let" IDENT "=" expr
constDecl   = "const" IDENT "=" expr
assign      = target ("=" | "+=" | "-=" | "*=" | "/=" | "%=") expr
target      = IDENT | postfix "[" expr "]" | postfix "." IDENT

ifStmt      = "if" expr block ("else" "if" expr block)* ("else" block)?
whileStmt   = "while" expr block
repeatStmt  = "repeat" expr block
forStmt     = "for" IDENT "in" expr block
fnDecl      = "fn" IDENT "(" params? ")" block
returnStmt  = "return" expr?
breakStmt   = "break"
continueStmt = "continue"

useStmt     = "use" path
            | "use" path "." "{" IDENT ("," IDENT)* "}"
            | "use" path "as" IDENT
            | "use" path "." "{" IDENT ("," IDENT)* "}" "as" IDENT
path        = IDENT ("." IDENT)*

structDecl  = "struct" IDENT "{" fields? "}"
fields      = IDENT ("," IDENT)*
enumDecl    = "enum" IDENT "{" variants? "}"
variants    = variant ("," variant)*
variant     = IDENT                                             // unit
            | IDENT "(" IDENT ("," IDENT)* ")"                  // tuple
            | IDENT "{" IDENT ("," IDENT)* "}"                  // struct
methodDecl  = "fn" IDENT "." IDENT "(" params ")" block

exprStmt    = expr
block       = "{" statement* "}"

expr        = or
or          = and ("||" and)*
and         = equality ("&&" equality)*
equality    = comparison (("==" | "!=") comparison)*
comparison  = addition (("<" | ">" | "<=" | ">=") addition)*
addition    = multiply (("+" | "-") multiply)*
multiply    = unary (("*" | "/" | "%") unary)*
unary       = ("!" | "-" | "try") unary | postfix
postfix     = primary (call | index | field | method | structLit | variantCtor)*
call        = "(" args? ")"
index       = "[" expr "]"
field       = "." IDENT
method      = "." IDENT "(" args? ")"
structLit   = "{" (IDENT ":" expr ("," IDENT ":" expr)*)? "}"   // only at expr position
variantCtor = "::" IDENT payload?
payload     = "(" expr ("," expr)* ")"                          // tuple variant
            | "{" IDENT ":" expr ("," IDENT ":" expr)* "}"      // struct variant

primary     = INT | NUMBER | STRING | "true" | "false" | "none"
            | IDENT | "(" expr ")" | arrayLit | dictLit
            | ifExpr | matchExpr | fnExpr

arrayLit    = "[" (expr ("," expr)*)? "]"
dictLit     = "{" (STRING ":" expr ("," STRING ":" expr)*)? "}"
ifExpr      = "if" expr "{" expr "}" "else" "{" expr "}"
matchExpr   = "match" expr "{" arm ("," arm)* ","? "}"
arm         = pattern ("if" expr)? "=>" expr
fnExpr      = "fn" "(" params? ")" block

pattern     = orPattern
orPattern   = singlePattern ("|" singlePattern)*
singlePattern = "_" | IDENT | literal | variantPattern | structPattern | arrayPattern
variantPattern = (IDENT ".")? IDENT "::" IDENT
              | (IDENT ".")? IDENT "::" IDENT "(" pattern ("," pattern)* ")"
              | (IDENT ".")? IDENT "::" IDENT "{" IDENT ":" pattern ("," IDENT ":" pattern)* "}"
structPattern  = (IDENT ".")? IDENT "{" IDENT ":" pattern ("," IDENT ":" pattern)* "}"
arrayPattern   = "[" patternList? arrayRest? "]"
patternList    = pattern ("," pattern)*
arrayRest      = ".." | ".." IDENT

params      = IDENT ("," IDENT)*
args        = expr ("," expr)*
```

Note: `methodDecl`, enum variant `IDENT`s, and `struct` names must start with an uppercase letter. `IDENT` bound by `let`, `fn`, parameters, `for`, etc. must start with lowercase or `_`. `const` names must be all-caps. Mis-shaped declarations parse-error with a "did you mean?" suggestion — see [Variables](../basics/variables.md#name-shapes-are-checked).

## Automatic semicolons

Bop automatically inserts a semicolon at the end of a line if the last token is one of:

- An identifier or literal (`int`, `number`, `string`)
- `true`, `false`, `none`
- `break`, `continue`, `return`
- `)`, `]`, `}`

This means the opening `{` of a block must be on the same line as its keyword:

```bop
// Correct
if x > 3 {
  print("yes")
}

// Wrong — semicolon inserted after "3"
if x > 3
{
  print("yes")
}
```

You can also separate statements on the same line with an explicit `;`:

```bop
let x = 1; let y = 2
```

## Comments

`//` starts a line comment — everything to the end of line is ignored:

```bop
// Whole-line comment
let x = 5   // Inline trailing comment
```

There's no block-comment syntax.

## String interpolation

Inside double-quoted strings, `{identifier}` inserts the value of a variable. Only plain variable names are allowed — no expressions, operators, or function calls:

```bop
let name = "Alice"
print("Hello, {name}!")     // works
// print("Hello, {1 + 2}!")  // error — expressions not allowed
```

Use `\{` and `\}` for literal braces in strings. Other supported escapes: `\"`, `\\`, `\n`, `\t`, `\r`.
