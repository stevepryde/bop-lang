# Syntax

Bop's syntax is deliberately simple. If you've seen JavaScript or Python, most of it will look familiar.

## Blocks

Code blocks use curly braces `{ }` and only appear after control flow keywords (`if`, `else`, `while`, `for`, `repeat`, `fn`):

```bop
if count > 3 {
  print("That's a lot!")
}
```

> **Important:** The opening `{` must be on the same line as its keyword. Bop automatically inserts semicolons at the end of lines, so putting `{` on the next line would cause a parse error.

```bop
// Good
if count > 3 {
  print("Nice!")
}

// Bad — will cause an error
if count > 3
{
  print("Nice!")
}
```

## Statements

Statements end with a newline. Bop automatically inserts semicolons after lines ending in an identifier, literal, `true`, `false`, `none`, `break`, `continue`, `return`, `)`, `]`, or `}`.

You can put multiple statements on one line with an explicit semicolon:

```bop
let x = 1; let y = 2
```

## Comments

Line comments start with `//`. Everything after `//` on that line is ignored:

```bop
// This is a comment
let x = 5   // So is this
```

## Identifiers

Variable and function names start with a letter or underscore and can contain letters, digits, and underscores:

```bop
let my_var = 5
let _count = 0
let item3 = "hello"
```

## Whitespace

Spaces and tabs are insignificant — use whatever indentation style you like. Only newlines matter (they end statements).
