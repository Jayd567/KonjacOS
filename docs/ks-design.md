# ks (KonjacShell) design

ks is the shell that will replace KonjacOS's current one (`shell.rs`,
which splits a line at the first space and looks the word up in a
table). Commands pass **structured values** to each other instead of
text, a failed step **stops the pipeline**, and changes to files are
**previewed and grouped** so a pipeline can't half-finish. ks can also
control the desktop. This document fixes the language, its values, how
pipelines and errors behave, and the order it gets built in. Status:
**stage 1 implemented**, plus most of stage 3 and the first layer of
stage 4 (see [What building stage 1 changed](#what-building-stage-1-changed)).

```
konjac> ls | filter size > 50MB | sort-by modified | delete
delete 3 files (210.4 MB)? [y/N]
```

## Goals

1. **Values, not text.** `ls` returns a table of files with a size, a
   date and a type. The next command reads `size` and gets a size, not
   the characters "52428800".
2. **One language.** Whatever works at the prompt works in a script,
   and the reverse. Scripts get stricter *checking*, not different
   syntax.
3. **Fail loudly and early.** A failing step stops the pipeline before
   the next step sees anything. The message names the step, the item
   and the reason, and points at the place in the line.
4. **Safe by default.** Commands that change files show what they'll do
   first, and a pipeline's changes land on KonjacFS together or not at
   all.
5. **Never take the kernel down.** ks runs inside the kernel, where a
   panic stops the whole machine. Any input, however wrong, must end in
   an error message.
6. **Testable without booting.** The language core is a separate crate
   that `cargo test` runs on the host in seconds.

### Not in version 1

Regular expressions, background jobs (`&`), redirecting a Linux
program's output into a pipeline (see [Programs](#programs-that-print-text)),
plugins, and moving ks out of the kernel into a user program. Each can
be added later without changing the language.

## Values

Every value has one of these types:

| Type | Example | Notes |
|---|---|---|
| `nothing` | `null` | What a command that returns nothing returns. |
| `bool` | `true`, `false` | |
| `int` | `42`, `-7`, `0x1F` | 64-bit. Overflow is an error, not a wrap. |
| `float` | `3.5`, `1e-3` | 64-bit. |
| `string` | `"hello"`, `'C:\raw'`, `hello` | Bare words are strings (below). |
| `size` | `50MB`, `4KiB`, `512B` | Bytes, as a 64-bit count. |
| `duration` | `250ms`, `2s`, `5min`, `3h`, `2day` | Nanoseconds, 64-bit. |
| `date` | `2026-10-06`, `2026-10-06T14:30` | Nanoseconds since 1970, like KonjacFS times. |
| `list` | `[1 2 3]`, `[a, b]` | Commas are optional. |
| `record` | `{name: "a.txt", size: 3KB}` | Fields keep their order. |
| `closure` | `{\|f\| $f.size > 1MB}` | A block with parameters. |
| `error` | | Only travels as a pipeline failure (below). |

A **table** is not a separate type: it's a list of records. The display
and the column-based commands (`select`, `sort-by`, `filter`) treat
any list of records as a table.

There's no separate path type either. Paths are strings, and the
commands that take paths resolve them against the current folder.

### Sizes

`B`, `KB`, `MB`, `GB` and `TB` are powers of 1000; `KiB`, `MiB`, `GiB`
and `TiB` are powers of 1024. Units are case-insensitive (`50mb` is
`50MB`), and a size may be fractional (`1.5GB`); it's rounded to whole
bytes.

Sizes **display** in powers of 1000 with one decimal: `4.2 MB`, `830 B`.
The Files app will switch to the same units, so the two never disagree
about a file's size.

### Arithmetic

Arithmetic keeps units where it makes sense and is an error otherwise:

| Expression | Result |
|---|---|
| `size + size`, `size - size` | `size` |
| `size * int`, `size / int` | `size` |
| `size / size` | `float` |
| `date - date` | `duration` |
| `date + duration` | `date` |
| `int + float` | `float` |
| `string + string` | `string` |
| `list ++ list` | `list` |
| `size + int`, `date + date`, `"a" * 2` | error |

Comparisons need both sides to be the same type (an `int` compares with
a `float`). `size > 50` is an error that says "50 has no unit; did you
mean 50MB?". In a script the checker catches it before the script runs.

## Syntax

### Commands and pipelines

```
ls docs | filter type == file | sort-by size --reverse | first 5
```

A line is a **pipeline**: commands separated by `|`, each taking the
previous one's output as its **input**. Statements are separated by
newlines or `;`. `#` starts a comment.

A command's arguments are:
- **positional**: `first 5`;
- **flags**: `--reverse`, or the short form `-r`;
- **flags with a value**: `--depth 2`.

The command's signature (below) decides which is which, so the parser
knows `sort-by size --reverse` is one argument and one flag.

### Words, strings, variables

- A **bare word** is a string: `cd docs`, `open notes.txt`, `echo hi`.
  That keeps typing at the prompt as easy as it is now.
- `"double quotes"` take escapes (`\n`, `\t`, `\"`, `\\`) and
  interpolation: `"hello $name, you have ($files | length) files"`.
- `'single quotes'` are taken literally.
- `$name` is a variable, and `$name.field` or `$list.0` reaches inside
  it. `$in` is the current input (in a `def`, a closure or `each`).
- A word that looks like a number, a size, a duration or a date is that
  type: `5`, `50MB`, `2s`, `2026-10-06`. Quote it to keep it a string.

### Expressions

`( )` holds an expression or a whole pipeline:

```
let total = (ls | get size | math sum)
echo (1 + 2 * 3)
```

The operators, from loosest to tightest binding:
- `or`;
- `and`;
- `not`;
- `==`, `!=`, `<`, `<=`, `>`, `>=`, and `=~`, a glob match:
  `name =~ "*.txt"`;
- `in`: `ext in [txt md]`;
- `+`, `-`, `++`;
- `*`, `/`, `mod`.

### Conditions on rows

`filter`, and anything else that tests each row, takes a **row
condition**. In it, a bare name on the left of an operator is a column
of the current row:

```
ls | filter size > 50MB and name =~ "*.wad"
```

means `{|row| $row.size > 50MB and $row.name =~ "*.wad"}`. The closure
form also works, for anything more complicated.

### Variables, functions, control flow

```
let limit = 50MB              # can't be reassigned
mut count = 0                 # can
count = $count + 1

def big [folder: string, --over: size = 10MB] {
    ls $folder | filter size > $over
}

if $count > 3 { echo many } else if $count > 0 { echo some } else { echo none }
for f in (ls) { echo $f.name }
while $count > 0 { count = $count - 1 }
```

- `break`, `continue` and `return` work as usual.
- A block is a scope: a `let` inside `{ }` is gone after it.
- Parameter types are checked when the function is called.
- `try { ... } catch {|e| ... }` catches a pipeline failure, so a
  script can handle it.

## Commands

### Signatures

Every built-in command, and every `def`, has a **signature**:

```rust
pub struct Signature {
    pub name: &'static str,
    pub summary: &'static str,
    pub params: &'static [Param],   // positional: name, type, optional?
    pub flags: &'static [Flag],     // long, short, value type (or none)
    pub input: Type,                // e.g. Table, Any, Nothing
    pub output: Type,
    pub destructive: bool,          // changes files: previewed, grouped
    pub apex: bool,                 // asks for the apex password first
}
```

This one table is the single source of truth for:
- **parsing**: whether `-r` takes a value;
- **help**: `help sort-by` prints it;
- **checking**: an unknown flag or a wrong type is an error before
  anything runs;
- **completion and red underlines** (stage 6).

### The first set

| Group | Commands |
|---|---|
| Files | `ls`, `cd`, `pwd`, `open`, `save`, `mkdir`, `delete` (alias `rm`), `move` (`mv`), `copy` (`cp`), `stat` |
| Tables | `filter`, `sort-by`, `select`, `reject`, `get`, `first`, `last`, `skip`, `length`, `reverse`, `uniq`, `each`, `group-by`, `enumerate` |
| Text | `lines`, `split`, `str contains`, `str upcase`, `str downcase`, `str trim`, `str replace`, `str length`, `from json`, `to json`, `into int`, `into size`, `into string` |
| Maths | `math sum`, `math avg`, `math min`, `math max` |
| System | `ps`, `kill`, `uptime`, `mem`, `date now`, `disks`, `help`, `clear`, `echo` |

What some of them return:

- **`ls [folder]`**: a table with `name`, `type` (`file` or `dir`),
  `size` and `modified`. `ls -l` adds `created` and `mode`. This needs
  `vfs::DirEntry` to carry the times: KonjacFS already stores them in
  every inode, and FAT16 stores a modification date in every directory
  entry.
- **`ps`**: `id`, `name`, `state`, `ticks` and `current`.
- **`mem`**: a record of the physical memory and the heap, used and
  free.
- **`disks`**: a table of the mounted volumes: `mount`, `format`,
  `size`, `free`.
- **`open file`**: the file's text as a string, or its bytes when it
  isn't text. `open data.json` parses it straight away; `open --raw`
  skips that.

`delete`, `move` and `copy` take paths as arguments, or a table of files
(anything with a `name` column) as input, as in the example at the top.

### The old commands

Everything in today's command table (`diskbench`, `kfstest`, `verify`,
`doom`, `run`, `reboot`, `halt`, `apex`, `alloc`, `readat`, `write`,
`cdemo`, `cio`, `about`) keeps working from day one. Each one becomes a
**text command**: it gets its arguments as one string, prints to the
screen as now, and returns nothing. Each can get a proper signature
later, one at a time. `ls`, `ps`, `cat`, `rm` and `meminfo` are
replaced by the structured versions above; `cat` stays as an alias for
`open --raw`.

### Programs that print text

Linux programs, `.exe`s and Java write straight to the terminal. In
version 1 that stays true:
- `run hello.exe`, or just `./hello.exe`, runs the program and returns
  nothing.
- A non-zero exit code is a pipeline failure, like any other error.

Capturing a program's output into the pipeline, as text that `lines`
or `from json` can turn into values, needs the program's `stdout` to
be redirected into a buffer. That's a change to the Linux layer, and it
comes after version 1.

## Pipelines and errors

### Each step finishes before the next starts

In version 1, each step of a pipeline runs to the end and produces its
whole output before the next step starts. That's simple, and it gives
two guarantees for free:

1. **A destructive command sees its whole input**, so it can show
   "delete 3 files (210.4 MB)?" with the right numbers, and nothing is
   touched if any earlier step fails.
2. **An error stops everything after it.** Nothing downstream ever sees
   part of a result.

The cost is memory: a step holds its whole output. That's fine for
folder listings and process tables. Streaming, where items flow one at
a time for large inputs, can be added later behind the same interface,
and destructive commands will still collect their whole input first.

### Errors

An error is a value: what went wrong, where in the line, and on which
item, if there was one:

```
konjac> ls | filter size > 5 | first 3
error: can't compare a size with an int
  | ls | filter size > 5 | first 3
  |             ~~~~   ^ 5 has no unit; did you mean 5MB?
```

```
konjac> ls /fat | each { open $in.name } | length
error: each: open: not a file (item 2 of 7: "SUBDIR")
  | ls /fat | each { open $in.name } | length
  |                  ^^^^
```

- A failure stops the pipeline, and the rest of the line is skipped.
  In a script, the script stops too, unless the failure is inside a
  `try`.
- A failing step never passes a half-finished value on.
- The message points at the place in the line with ASCII `^` and `~`
  (the console font is ASCII).

Running out of heap still halts the kernel, as it does today. ks limits
what it controls:
- how deep expressions and calls can nest (so the kernel stack can't
  overflow);
- how long a string or list can grow from `*` or loops.

**Ctrl+C** stops a running pipeline: the evaluator checks for it in
every loop.

## Safety

Commands whose signature says `destructive` (`delete`, `move`, `save`
over an existing file, and later anything else that changes the disk)
get three layers, built in this order:

1. **Preview.** Before acting, the command says what it's about to do
   and waits for `y`: "delete 3 files (210.4 MB)? [y/N]". `--yes`
   skips the question, for scripts; `--dry-run` lists what it would do
   and stops. One file named on the command line (`delete notes.txt`)
   doesn't ask.
2. **All or nothing.** KonjacFS commits each operation atomically today.
   A new `kfs::begin()` / `kfs::finish()` pair makes everything in
   between one commit, and ks wraps each destructive command in it. If
   the power goes out in the middle of `delete`, either every file is
   gone or none is. On FAT16 (at `/fat`) each file is still deleted one
   by one, and the preview says so.
3. **Undo.** Once KonjacFS has snapshots (milestone 5 of its design),
   ks takes one before each destructive command and keeps the last few.
   `undo` puts the disk back as it was.

The apex password is still asked for where today's commands ask for it,
once per pipeline.

## The terminal

Today's Terminal is a grid of 88 by 26 single-colour characters, and
the input line is a fixed 120-byte buffer with only backspace. ks needs
more:

- **Colour and styles.** Each cell of the console grid gets a
  foreground colour, a background colour and an underline flag. Text
  sets them with ANSI escape codes (`ESC[31m` and friends), because
  Linux programs already print those. The colours come from the
  desktop theme, so they look right on glass.
- **A line editor** that the shell owns:
  - left, right, Home, End, Ctrl+left and Ctrl+right;
  - Delete, and typing in the middle of the line;
  - lines longer than the screen wrap;
  - Up and Down step through history, kept in `/.ks_history`;
  - Ctrl+R searches it.

  The keyboard driver today hands the shell bytes, not keys, so it needs
  to report the arrow keys and Home and End too.
- **Highlighting as you type**: commands, strings, numbers and
  variables in different colours, and an unknown command in red. The
  lexer re-runs on every keystroke; a line is short, so that's cheap.

## Desktop control

The shell and the desktop are in the same kernel, so ks talks to the
desktop over a message channel: ks sends a request, and the desktop task
does it on its next frame.

| Command | Does |
|---|---|
| `desktop` | A record of the settings: `glass`, `accent`, `wallpaper`, `clock24`, ... |
| `desktop set glass frosted` | Changes one setting, as the Settings app would, and saves it. |
| `windows` | A table of open windows: `id`, `app`, `title`, `x`, `y`, `width`, `height`, `focused`, `minimized`. |
| `windows \| filter app == Files \| close` | Closes them (also `minimize`, `focus`, `move-to`). |
| `start notes.txt` | Opens a file in its app, as double-clicking it in Files does. |
| `start files` | Opens an app. |

`open` reads a file into the shell, as everywhere else in ks; `start`
hands it to the desktop.

## Completion and inline errors

When the line editor exists, it gets help from the same signatures:

- **Tab** opens a menu under the cursor. Arrow keys move through it,
  Enter or Tab picks, and Esc closes it. It offers:
  - commands and `def`s, at the start of a step;
  - flags, from the signature, after `-`;
  - paths, where the signature says a path goes;
  - variables, after `$`;
  - column names, after `filter`, `sort-by` and `select`, when the
    previous step's output columns are known.
- **A suggestion from history** appears greyed out after the cursor, as
  in fish; right arrow accepts it. History is weighted by how often and
  how recently each line was used.
- **Red underlines before Enter**: the checker runs on the line as you
  type and underlines an unknown command, an unknown flag or a type
  mismatch, with the reason shown below the line.

KonjacOS runs on one core, so this doesn't use a background thread.
What keeps it instant is that each piece is small:
- lexing and checking a line takes microseconds;
- folder listings come from KonjacFS's cache, and ks keeps the last few
  itself.

Anything slower (a folder on FAT16, say) is done between keystrokes, a
piece at a time, and the menu fills in when it's ready.

## Scripts

A script is a `.ks` file, run with `source script.ks` or just
`script.ks`. Before a script runs, the **checker** reads all of it and
refuses to start if it finds any of these, listing every one with its
line:
- an undefined variable or command;
- a flag the command doesn't have, or a missing argument;
- assigning to a `let`;
- a type mismatch it can see without running (`size > 5`, a
  `string` passed where a `def` wants an `int`).

At the prompt the same checker runs, but only on the line being
entered. A script can take arguments through a `def main [...]`, whose
signature also checks what it's given.

## Architecture

```
ks/                     the language: no kernel code, so `cargo test` runs it on the host
  src/lexer.rs          text -> tokens, each with its position in the line
  src/parser.rs         tokens -> syntax tree, with positions, using the signatures
  src/value.rs          the value types, arithmetic, comparisons
  src/check.rs          the checker (scripts, red underlines)
  src/eval.rs           runs the tree
  src/display.rs        values -> text: tables, records, sizes, dates
  src/builtins/         commands that only need values: filter, sort-by, math ...
kernel/src/ks/          the kernel side
  host.rs               files (vfs), tasks, memory, desktop, the apex prompt
  commands.rs           ls, ps, disks, delete ... and the old text commands
  editor.rs             the line editor, history, highlighting, completion menu
```

- **The `ks` crate is `no_std` and uses only `alloc`.** It reaches the
  outside world through a `Host` trait (list a folder, read a file,
  delete, list tasks, print, ask yes or no). The kernel implements the
  trait for real, and the tests implement it with an in-memory disk.
- **Never panic.** No `unwrap`, no unchecked indexing and no
  overflowing arithmetic on anything that comes from the user.
  Recursion is limited (expressions 64 deep, calls 128 deep). Tests
  feed the parser random bytes to check it always returns.
- `shell.rs` shrinks to a loop: read a line through the editor, hand it
  to ks, show the result.

## Stages

Each stage leaves KonjacOS with a working shell.

1. **Core.** Values, the lexer and parser, pipelines, the evaluator,
   error messages, table display, and the first set of commands. The old
   commands run as text commands. The `ks` crate has host tests.
   *Done when* `ls | filter size > 1MB | sort-by modified` shows a
   table, and every old command still works.
2. **Terminal.** Colours and underline in the console, the line editor,
   arrow keys from the keyboard driver, history, highlighting.
3. **Scripts.** `let`/`mut`, `def`, `if`/`for`/`while`, `try`, `.ks`
   files, and the checker.
4. **Safety.** Previews, `--yes` and `--dry-run`, then all-or-nothing
   commits through `kfs::begin()`/`finish()`, then `undo` once KonjacFS
   has snapshots.
5. **Desktop.** The message channel, `desktop`, `windows` and `start`.
6. **Completion.** The Tab menu, history suggestions, and red
   underlines as you type.

Stages 1 and 2 already give the example at the top, apart from the
preview, with a coloured table on screen.

## Decisions

These are settled:

| Question | Answer |
|---|---|
| One language or two modes? | One language; scripts get the checker. |
| `filter` or `where`? | `filter`. |
| What does `MB` mean? | 1000-based; `MiB` is 1024-based. |
| Name | `ks`, with scripts in `.ks` files. |
| Where does it run? | In the kernel, with the language in its own crate. |
| Streaming? | Not in version 1: each step finishes first. |

## What building stage 1 changed

Things that turned out differently from the plan above, or that the
plan didn't cover.

**Done earlier than planned.**
- `let`, `mut`, `def`, `if`, `for`, `while`, `break`, `continue`,
  `return` and `source` (stage 3) came with the evaluator, since they
  cost little once it existed. What's left of stage 3 is `try`/`catch`,
  `def main` for scripts, and the checker.
- The preview, `--yes` and `--dry-run` (stage 4's first layer) are in
  `delete`. `delete` also asks when several paths, or any folder, are
  named on the command line, not only when they're piped in. All-or-
  nothing commits still need `kfs::begin()`/`finish()`.
- `kill` takes task ids, or rows piped in from `ps`.

**Rules the syntax needed.**
- Operators need spaces around them: `size > 5MB`, not `size>5MB`.
  A word runs up to a space, so `*.txt` and `../docs` stay one word.
- `,` always separates (in lists and records), so text that is just a
  comma needs quotes: `split ","`.
- Inside a block, a bare word at the start of a step is a command, as at
  the prompt. `if $x { a }` runs a command called `a`; write `{ 'a' }`.
- `let x = word` stores the text when `word` isn't a command.
- In a row condition, a bare word right of a comparison is text
  (`type == file`), and everywhere else it's a column.
- A `def` sees only its parameters, not the prompt's variables. A
  closure copies the variables it uses when it's made.
- A `def` has to come before its first use in a script.

**Smaller decisions.**
- `echo a b` gives the text `a b`, not a list.
- `ls` lists folders first, then names ignoring case; names are paths
  from where you are (`ls docs` gives `docs/a.md`).
- `cd` alone goes to `/`.
- `=~` ignores case.
- The stable toolchain's `alloc` is built for unwinding, and some of it
  (`format!`) refers to `_Unwind_Resume`. The kernel now defines that
  symbol (it can never be called under `panic = "abort"`), so ks and the
  rest of the kernel can use `format!`.

**Issues found, for later.**
- `run` doesn't wait for the program it starts, so the program's output
  lands after the next prompt. ks can't tell whether a program failed
  until the loader waits for it and hands back the exit code.
- The input line is still the old 120-character buffer with only
  backspace; stage 2 replaces it.
- The console grid holds ASCII only, so other characters in output show
  as `?`.
