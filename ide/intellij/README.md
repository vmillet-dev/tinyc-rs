# TinyC for IntelliJ IDEs

Editing support for TinyC in RustRover, IntelliJ IDEA, CLion — any IntelliJ-based
IDE. It is built against the one installed on this machine and runs in that
version or a later one; building it against an older IDE is a line in
`gradle.properties`.

## What it does

* **Syntax highlighting**, from a lexer that mirrors the shape of `src/lexer.rs`
  and takes its *vocabulary* from the compiler itself — see [The vocabulary is
  generated, not repeated](#the-vocabulary-is-generated-not-repeated) — plus the
  colouring a lexer cannot do: a class, a variant, a field, a
  parameter and a call all lex as one identifier, and telling them apart takes
  the tree. `%d` inside a `print`'s format string is highlighted as the
  specifier it is.
* **Completion** drawn from the file itself — the fields and methods of a value
  after `.` (inherited ones included), an enum's variants after `::`, the fields
  an object literal still needs, the locals, parameters and pattern bindings in
  scope, the top-level functions, the built-ins, and the keywords that fit where
  the caret is. Only `fn`, `class` and `enum` are offered at the top level,
  because those are the only things that may be written there.
* **The compiler's own diagnostics, in the editor.** See below.
* **Running a program from the IDE**: a run configuration that drives `tinyc`,
  `nasm` and the linker itself — no script, no repository — a green arrow in the
  gutter beside `fn main`, and a `file:line:column` in any console turned into a
  link.
* Structure view, brace matching, `//` commenting, and a colour scheme page
  under *Settings | Editor | Color Scheme | TinyC*.

## The one rule this plugin follows

**`tinyc` is the only thing that decides whether a program is correct.**

The plugin's own parser never reports a mistake. It exists to know where the
declarations are — which is what completion, the structure view and brace
matching need, and which stays true of a file being typed into halfway through a
word. Everything else is asked of the compiler: the text in the editor is handed
to `tinyc --emit ir`, and what it refuses is marked where it says it is.

That is what makes the editor agree with the terminal *by construction*. A
second opinion, written to keep an editor busy, would only ever be a worse copy
of the type checker — and would start refusing perfectly good code the day the
language grew. It also means the editor knows the things only the whole compiler
knows: an index that is out of range, an overflow the optimiser can see, a `%d`
handed a string, a class that contains itself.

### The vocabulary is generated, not repeated

Colouring cannot run a process per keystroke, so the plugin has to know the
words and the symbols for itself. It does not *repeat* them: `TinyCTokens` is
**generated** at build time from `grammar/vocabulary.txt`, which the compiler
writes out of `vocabulary::SPELLED` — the very table its lexer reads to decide
whether a word is a keyword, and which takes each spelling from
`TokenKind::text` rather than repeating it.

```text
src/vocabulary.rs ─cargo run --bin export-vocabulary─> grammar/vocabulary.txt
                                                     │
                                       generateVocabulary
                                                     ↓
                                      build/generated/…/TinyCTokens.kt
```

So adding a keyword to TinyC is adding a row in Rust, and the editor colours it,
completes it and refuses it as a variable name without anything here being
edited. What arrives that way is the words, the punctuation and the roles that
tell `if` from `int` from `print`; the built-ins with their real signatures; and
what a `%` in a format string may be followed by.

The generator knows none of those by name. It makes a `TokenSet` and a word list
**per role it finds in the file**, so a role added to the compiler shows up here
as `<ROLE>_TOKENS`, joins `ALL_KEYWORDS` if it is a word, and gets an entry in
`ROLES` — with nothing edited in `build.gradle.kts`. Only which *colour* a role
is painted in stays a decision of this side, in `TinyCSyntaxHighlighter`.

The file is checked in, so building the plugin needs neither Rust nor cargo —
and a test in the compiler compares it against what this run of `tinyc` would
write, so forgetting to regenerate is a failing `cargo test` rather than a word
that quietly stops being highlighted.

What is *not* generated is the shape of the productions. The plugin's parser is
deliberately tolerant — it only finds declarations, and anything it does not
recognise falls through without complaint — so it survives the language growing
a construct in a way a generated parser would not. The rules for what a comment,
a literal and a name look like are hand-written in `TinyCLexer` for the same
reason: they are the part that has not changed since the language began.

### One thing is still repeated

The **link line**. The plugin drives `tinyc`, `nasm` and the linker itself
rather than calling `scripts/build.*`, which is what lets it build a `.tc` file
that has no repository anywhere above it. `BuildMatchesTheScriptsTest` reads the
flags and the libraries back out of both scripts and compares them with the
constants in `TinyCBuild`, so a change to how a program is linked cannot reach
one and not the other.

## Independent of the project

Nothing in the plugin needs the TinyC repository to be the open project, or to
exist. It needs three tools, each of which it **looks for** and each of which
can be **said** in *Settings | Tools | TinyC*:

| Tool | Found by |
| --- | --- |
| `tinyc` | a `target/release` or `target/debug` binary of a repository above the file, then `PATH`, then `cargo run` in that repository |
| `nasm` | `PATH`, then the places `winget install nasm` leaves it |
| linker | Windows: `link.exe` from the Visual Studio `vswhere` names — Linux: the first of `cc`, `gcc`, `clang` |
| `vcvars64.bat` | `vswhere` (Windows only) |

The Windows linker needs `LIB` set, which `vcvars64.bat` does. Rather than
launching every build through `cmd /c call vcvars && link`, the environment that
script produces is captured once and handed to `link.exe` directly — the same
bargain, made once per IDE session instead of once per build. And because `link`
is also the name of a coreutils program that Git for Windows puts on `PATH`, a
candidate is only accepted when `cl.exe` sits beside it.

The button **What would be used?** on the settings page answers with the three
paths it would pick, with the fields as they are on screen — so a path can be
tried before it is applied.

## Building it

The build uses an IDE **already installed on this machine**, so it downloads no
platform at all. It looks where installers and Toolbox put them — a directory is
an installation when it holds `product-info.json` — and prefers IntelliJ IDEA
when a machine has several, since that is the platform this targets.

Nothing to configure, then, unless your IDE is somewhere unusual:

```properties
tinycIdePath=D:/somewhere/else/RustRover
```

A path that turns out not to be an installation is reported and then ignored, so
a stale one does not silently become a download of something else.

Two values are special:

| `tinycIdePath` | |
| --- | --- |
| empty | look for one; download only if nothing is found |
| `none` | do not look — download `platformVersion`, which is what CI does |

`none` exists so that the download path can be run on a machine that has an IDE
on it. It is the path a CI runner takes, and one nobody can exercise is one that
rots: it had been broken since IDEA Community stopped being published as a
distribution of its own in 2025.3, and nothing said so until a build was
attempted without an IDE around.

The plugin declares support for the platform it was built against and everything
after it, and is compiled for the Java that platform runs on (25, for 2026.2).
Building against an older IDE means lowering `jvmTarget` in `build.gradle.kts`
to match — that is the only thing the version affects.

```bash
./gradlew buildPlugin        # -> build/distributions/tinyc-intellij-0.1.0.zip
./gradlew test               # the lexer, the parser, completion, the diagnostics
./gradlew runIde             # a sandbox IDE with the plugin loaded
```

## Installing it

*Settings | Plugins | ⚙ | Install Plugin from Disk…* and choose the zip from
`build/distributions`. Restart when asked. `.tc` files are picked up by
extension; on a machine where the three tools are in the usual places, there is
nothing else to configure.

## Running a TinyC program from the IDE

Right-click a `.tc` file (or click the arrow in the gutter beside `fn main`) and
choose **Run**. The configuration does the three steps itself:

```text
tinyc  source.tc  -> source.asm
nasm   source.asm -> source.obj   (-f win64, or elf64 elsewhere)
link   source.obj -> source.exe   (against the C runtime, for printf)
```

and then starts the program, with its own console — so a program that calls
`read_line()` can be typed into. The build log is handed to that same console
first, so a run reads as one story; when a step fails, the console opens anyway
and holds the diagnostic, with its `file:line:column` as a link.

Five fields: the source file, the working directory (empty: the repository above
the file, else the directory it is in), the output directory for the `.asm`, the
object and the executable (`out`, relative to the working directory), extra
compiler arguments (`--no-optimise`, `--target x86_64-linux`, …) and *Build only,
do not run*.

## Settings

*Settings | Tools | TinyC* — every one of these is a path the plugin would
otherwise find on its own; see the table above for how.

| Setting | Default |
| --- | --- |
| `tinyc` | a built binary, then `PATH`, then `cargo run` |
| `nasm` | `PATH`, then where `winget install nasm` puts it |
| linker | `link.exe` from Visual Studio, or `cc`/`gcc`/`clang` |
| `vcvars64.bat` | found through `vswhere` (Windows) |
| Report the compiler's diagnostics while typing | on |
| Run the optimiser for those diagnostics | off — a pass may not change what a program means, so it would only cost time |

Building the compiler once (`cargo build --release`) is worth it: the editor asks
it for an opinion on nearly every keystroke, and a release binary answers in
milliseconds where `cargo run` first has to decide whether anything changed.

## What it does not do yet

* **Go to definition, find usages, rename.** The tree has the declarations in
  it, so this is mostly a matter of putting references on the identifiers.
* **Formatting.** There is no `tinyc fmt` to be the authority, and inventing one
  in the plugin would break the rule above.
* **A parameter-info popup** while typing a call.

An LSP server built from the compiler's own crates would collapse most of the
remaining work — sema already knows every type and every span. It would also
replace this plugin's completion rather than extend it, which is why it was not
the starting point: this way the editor is useful today and the compiler grew no
new dependency.
