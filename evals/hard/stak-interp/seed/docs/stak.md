# Stak

Stak is a small stack language. A Stak program is one text file. An
implementation is run as `sh stak.sh FILE`: it reads FILE, checks it, and
then runs it from the first token to the last.

This document is the whole contract. Where an example and a rule seem to
disagree, the rule wins; they are meant not to.

## 1. Values and the stack

Every value is an integer. Programs keep all values, including results,
within ±2^53, so an implementation never has to handle anything larger.

There is one data stack, empty at the start. "Push" puts a value on top;
"pop" takes the top value off. Below, a stack effect `( a b -- c )` means
the word pops `b` (the top) and then `a`, and pushes `c`.

Numbers are printed in plain decimal, with no leading zeros and with `-`
only before a negative number: `-0` prints as `0`.

## 2. Source text

### 2.1 Lines and whitespace

Lines are separated by LF and numbered from 1. The last line may lack an
LF. Whitespace is space, tab and LF. Every other byte is part of a token.

### 2.2 Tokens

A token is a maximal run of non-whitespace bytes, read left to right, with
two exceptions:

- **Comments.** A token that is exactly `#` starts a comment: it and the
  rest of its line are ignored. A `#` inside a longer token is an ordinary
  byte, so `#x` and `.#` are words like any other (and, unless defined,
  unknown ones).
- **Strings.** A token that is exactly `."` starts a string. The string's
  text begins after the one space or tab that follows `."` and ends just
  before the next `"` on the same line. That `"` is not part of any token,
  and reading continues right after it. The text may contain any byte
  except LF and `"`, including spaces and `#`, and may be empty. If no `"`
  follows on the same line, or `."` ends its line, the string is
  unterminated (a syntax error, section 7).

A token that matches `-?[0-9]+` (an optional `-` and one or more digits,
nothing else) is a number. `007` is the number 7 and `-0` is 0; `+5`, `1.5`
and `-` are not numbers.

Every other token is a word. Words are case-sensitive: `dup` is built in,
`DUP` and `Dup` are not.

## 3. Built-in words

When a number is executed it is pushed. A string prints its text when it is
executed, with nothing added. The built-in words:

| word | effect | meaning |
|---|---|---|
| `+` | `( a b -- a+b )` | |
| `-` | `( a b -- a-b )` | |
| `*` | `( a b -- a*b )` | |
| `/` | `( a b -- q )` | `a` divided by `b`, truncated toward zero |
| `mod` | `( a b -- r )` | `a - b*q` with `q` as for `/`, so `r` has the sign of `a` (or is 0) |
| `dup` | `( a -- a a )` | |
| `drop` | `( a -- )` | |
| `swap` | `( a b -- b a )` | |
| `over` | `( a b -- a b a )` | |
| `rot` | `( a b c -- b c a )` | the third item moves to the top |
| `=` | `( a b -- f )` | 1 if `a` equals `b`, else 0 |
| `<` | `( a b -- f )` | 1 if `a` is less than `b`, else 0 |
| `>` | `( a b -- f )` | 1 if `a` is greater than `b`, else 0 |
| `not` | `( a -- f )` | 1 if `a` is 0, else 0 |
| `.` | `( a -- )` | print `a` followed by one space |
| `cr` | `( -- )` | print one LF |
| `emit` | `( a -- )` | print the single byte whose code is `a`; `a` must be 32 to 126 |

So `-7 2 /` gives -3, `-7 2 mod` gives -1, `7 -2 /` gives -3 and `7 -2 mod`
gives 1. Dividing by 0 with `/` or `mod` is a runtime error, and so is
`emit` of a value outside 32 to 126.

The words `if`, `else`, `then`, `begin`, `while`, `repeat`, `until`, `:`
and `;` are control words (sections 4 and 5). Together with the table above
they are the built-ins. Every built-in that pops (including `if`, `while`
and `until`, which pop one value each) needs that many values on the stack;
with fewer, it is a stack underflow, a runtime error, and the stack is not
changed.

## 4. Control flow

Control structures may appear at the top level and inside definitions, and
may nest to any depth.

- `if A then`: `if` pops a value. If it is not 0, `A` runs. Then execution
  continues after `then`.
- `if A else B then`: `if` pops a value. If it is not 0, `A` runs, else `B`
  runs. Then execution continues after `then`.
- `begin A while B repeat`: `A` runs, then `while` pops a value. If it is 0,
  execution continues after `repeat`. Otherwise `B` runs and execution goes
  back to `begin`.
- `begin A until`: `A` runs, then `until` pops a value. If it is 0,
  execution goes back to `begin`; otherwise it continues after `until`.

`A` and `B` are any sequence of tokens in which every structure is
complete, including empty ones.

## 5. Definitions

`: NAME BODY ;` defines the word NAME. NAME is the token after `:`; it must
be a word that is not a built-in (so not a number, and not a string). BODY
is any sequence of tokens in which every control structure is complete.

A definition may appear only at the top level: not inside another
definition and not inside a top-level control structure.

When execution reaches a definition, it does not run BODY; it makes NAME
refer to this definition from that point on, and skips to after the `;`.
Executing a defined word runs its body and then continues after the call.
A word may be defined more than once; each definition is separate.

### 5.1 Name resolution

Every word in a program that is not a built-in is bound to a definition
when the program is read, from the text before it, in these four ways:

1. **A call at the top level** binds to the most recent definition of that
   name that appears before the call in the file. In
   `: hi 1 . ; hi : hi 2 . ; hi` the first `hi` prints `1 ` and the second
   prints `2 `.
2. **A call inside a body to a word defined earlier** binds to the
   definition that was most recent when the body was read, and keeps that
   binding even if the word is defined again later. In
   `: a 1 . ; : b a ; : a 2 . ; b a` the call `b` prints `1 ` (its `a` is
   the first one), and the last `a` prints `2 `. The whole program prints
   `1 2 `.
3. **A call inside a body to the word being defined** binds to that same
   definition, so a word can call itself. This holds even when an older
   definition of the name exists:
   `: f 9 . ; : f dup if dup . 1 - f then ; 2 f` prints `2 1 `.
4. **A call to a word with no definition before it** in the file is
   unknown, even if the word is defined later in the file. It is not an
   error when the program is read: it is a runtime error only if and when
   it is executed. In `: a b ; : b 5 . ; 7 . a` the program prints `7 `
   and then stops with `error: unknown word b at line 1`. In
   `: g 0 if nope then 3 . ; g` the unknown `nope` is never executed, and
   the program prints `3 ` and succeeds.

## 6. Running and exit status

The program runs from its first token. When it runs past its last token it
ends successfully with exit status 0; values left on the stack are ignored.

Everything the program prints goes to standard output, as soon as it is
executed. Error messages go to standard error, as exactly one line, and
nothing else is printed there.

If FILE does not exist or cannot be read, print nothing on standard output
and exit with status 2.

## 7. Syntax errors

Before anything runs, the whole file is read and checked. If it has a
syntax error, nothing is printed on standard output at all, even for code
before the error; standard error gets `error: syntax at line N` and the exit
status is 2.

The checker reads tokens in file order and stops at the first offending
token; N is that token's line. These are the offending tokens:

- `:` inside a definition or inside a top-level control structure.
- The token after `:` when it is a number, a built-in or a string (for a
  string, its line is the line of its `."`).
- `;` when no definition is open, or when a control structure inside the
  open definition is not complete.
- `else` when the innermost open structure is not an `if`, or is an `if`
  that already has its `else`.
- `then` when the innermost open structure is not an `if`.
- `while` when the innermost open structure is not a `begin`, or is a
  `begin` that already has its `while`.
- `repeat` when the innermost open structure is not a `begin` that has a
  `while`.
- `until` when the innermost open structure is not a `begin`, or is a
  `begin` that has a `while`.
- `."` of an unterminated string.

Here "open structure" counts `if`, `begin` and the definition itself, so
in `: x then ;` the `then` is offending. If the file ends while something
is still open (`:` without its `;`, `if` without `then`, `begin` without
its `until` or `repeat`, or `:` as the last token), N is the line of the
most recently opened one that is still open.

Because the checker stops at the first offending token in file order, in

```text
1 . then
." oops
```

the error is `error: syntax at line 1`, not line 2.

## 8. Runtime errors

A runtime error stops the program at once. Everything printed before it
stays printed. Standard error gets one of these lines, and the exit status
is 3:

- `error: stack underflow at line N`
- `error: unknown word W at line N` (W is the word exactly as written)
- `error: division by zero at line N`
- `error: bad emit at line N`

N is the line of the token being executed when the error happens. For a
word inside a body, that is the line where the token is written in the
body, not the line of the call. So in

```text
: ratio
  / ;
1 . 6 0 ratio
```

the program prints `1 ` and then fails with
`error: division by zero at line 2`.

## 9. Speed

A loop such as `200000 begin dup while 1 - repeat` (200,000 iterations)
must finish in well under 10 seconds.

## 10. Example

```text
# Count down, then print a factorial.
: countdown begin dup . 1 - dup not until drop ;
: fact dup 1 > if dup 1 - fact * then ;
5 countdown ." liftoff" cr
." 6! =" 32 emit 6 fact . ." (factorial)" cr
```

prints these two lines, each ending with LF:

```text
5 4 3 2 1 liftoff
6! = 720 (factorial)
```

More programs and their exact output are in `examples/`.
