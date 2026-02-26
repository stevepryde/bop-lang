# Grammar

An informal grammar for the Bop language, plus the complete list of reserved words.

## Reserved words

```
let fn return if else while for in repeat break continue
true false none
```

These cannot be used as variable or function names.

## Built-in functions

Built-in functions available everywhere:

| Function | Returns | Description |
|----------|---------|-------------|
| `range(n)` | array | `[0, 1, ..., n-1]` |
| `range(start, end)` | array | `[start, start+1, ..., end-1]` (auto-detects direction) |
| `range(start, end, step)` | array | Sequence with custom step |
| `str(x)` | string | Convert any value to string |
| `int(x)` | number | Truncate to integer, or parse string |
| `type(x)` | string | Type name: `"number"`, `"string"`, `"bool"`, `"none"`, `"array"`, `"dict"` |
| `abs(x)` | number | Absolute value |
| `min(a, b)` | number | Smaller of two numbers |
| `max(a, b)` | number | Larger of two numbers |
| `rand(n)` | number | Random integer 0 to n-1 |
| `len(x)` | number | Length of string, array, or dict |
| `print(args...)` | none | Print values separated by spaces |
| `inspect(x)` | string | Debug representation (strings quoted) |

## Grammar

```
program     = statement*
statement   = letDecl | assignment | ifStmt | whileStmt | repeatStmt
            | forStmt | fnDecl | returnStmt | breakStmt | continueStmt
            | exprStmt

letDecl     = "let" IDENT "=" expr
assignment  = target ("=" | "+=" | "-=" | "*=" | "/=" | "%=") expr
target      = IDENT | expr "[" expr "]"

ifStmt      = "if" expr "{" statement* "}"
              ("else" "if" expr "{" statement* "}")*
              ("else" "{" statement* "}")?
whileStmt   = "while" expr "{" statement* "}"
repeatStmt  = "repeat" expr "{" statement* "}"
forStmt     = "for" IDENT "in" expr "{" statement* "}"
fnDecl      = "fn" IDENT "(" params? ")" "{" statement* "}"
returnStmt  = "return" expr?
breakStmt   = "break"
continueStmt = "continue"

exprStmt    = expr

expr        = or
or          = and ("||" and)*
and         = equality ("&&" equality)*
equality    = comparison (("==" | "!=") comparison)*
comparison  = addition (("<" | ">" | "<=" | ">=") addition)*
addition    = multiply (("+" | "-") multiply)*
multiply    = unary (("*" | "/" | "%") unary)*
unary       = ("!" | "-") unary | postfix
postfix     = primary (call | index | method)*
call        = "(" args? ")"
index       = "[" expr "]"
method      = "." IDENT "(" args? ")"
primary     = NUMBER | STRING | "true" | "false" | "none"
            | IDENT | "(" expr ")" | arrayLit | dictLit
            | ifExpr

arrayLit    = "[" (expr ("," expr)*)? "]"
dictLit     = "{" (STRING ":" expr ("," STRING ":" expr)*)? "}"
ifExpr      = "if" expr "{" expr "}" "else" "{" expr "}"

params      = IDENT ("," IDENT)*
args        = expr ("," expr)*
```

## Automatic semicolons

Bop automatically inserts a semicolon at the end of a line if the last token is one of:

- An identifier or literal
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

## String interpolation

Inside double-quoted strings, `{identifier}` inserts the value of a variable. Only plain variable names are allowed — no expressions, operators, or function calls:

```bop
let name = "Alice"
print("Hello, {name}!")     // works
// print("Hello, {1 + 2}!")  // error — expressions not allowed
```

Use `\{` and `\}` for literal braces in strings.
