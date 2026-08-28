# TinyC

**A compiler for a small typed language, written from scratch in Rust — source
text in, x86-64 assembly out.**

No LLVM, no parser generator, no dependency but `clap`. Every stage is hand
written and can be printed on its own: the lexer, the recursive-descent parser,
the type checker, the lowering to a control flow graph, the SSA passes, the
register allocator and the NASM backend.

```c
enum Level { Info, Warn, Error }

class Reading {
  string place;
  int degrees;

  fn level(self) -> Level {
    if (self.degrees < -10) { return Level::Error; }
    return Level::Info;
  }
}

fn main() {
  Reading r = Reading { place: "Oslo", degrees: -12 };
  println("%s -> %s", r.place, match (r.level()) {
    Level::Info => "ok",
    Level::Warn => "watch",
    Level::Error => "alert",
  });
}
```

[`examples/showcase.tc`](examples/showcase.tc) is every feature in one program.

## Quick start

```bash
cargo run -- examples/hello.tc -o out/hello.asm
```

`tinyc` emits NASM assembly and stops there. To get an executable:

```powershell
.\scripts\build.ps1 examples\hello.tc
```

```bash
./scripts/build.sh examples/hello.tc
```

Both need `nasm` (`winget install nasm`, or `apt install nasm`). Windows also
needs Visual Studio with the "Desktop development with C++" workload, for
`link.exe`; Linux needs a C compiler (`apt install build-essential`), used only
as the linker.

| Flag | Meaning |
|------|---------|
| `-o, --output <FILE>` | where to write the assembly (default: input path with `.asm`) |
| `--emit tokens\|ast\|ssa\|ir\|asm` | stop after a stage and print its result |
| `--target <NAME>` | `x86_64-windows` or `x86_64-linux` (default: this machine's) |
| `--dump-regalloc` | print live intervals and register assignments |
| `--no-optimise` | hand the backend the IR exactly as lowering produced it |

Either target can be built from either machine; only *running* the result needs
the matching one.

## The language

* `int`, `float`, `bool`, `char` and `string`. A string is a run of Unicode
  characters, so `len` counts characters and `s[i]` is one.
* **Nothing converts on its own.** `string(n)`, `int(f)`, `char(n)` and the rest
  are written out, so an average says where it stopped being whole.
* Fixed arrays (`int[3]`), whose length is part of the type, and growable lists
  (`int[]`), whose length is a fact about the data.
* Classes with fields, methods, single inheritance and virtual dispatch. An
  object holds whole arrays and whole objects *inside* itself, and a list beside
  itself — which is what gives TinyC **trees** with no reference type and no
  `null` ([`tree.tc`](examples/tree.tc)).
* Enums whose variants may **carry values**, taken apart by the pattern that
  matched them ([`payloads.tc`](examples/payloads.tc)).
* `match` as an expression or a statement, checked for completeness.
* `if` / `while` / `for` / `break` / `continue`, and short-circuiting `&&`,
  `||`, `!`.
* **Assignment copies; a parameter borrows.** Naming an array, object or list
  duplicates it, so writing through one name is never visible through another.
* `println("Grade: %c for %s", g, name)` — the `%`s are checked while the
  program is compiled, and nothing at run time ever reads one.
* `read_line()`, `eof()` and `is_int(s)` for input.

### Nothing answers wrongly

* **Arithmetic stops rather than wrap.** Overflow, division by zero and
  `i64::MIN / -1` end the program with a message. A `float` has an answer for
  all three, so it stops for none of them.
* **Every index is checked** — the ones the compiler can work out, before the
  program is built.
* **Running out of stack is a message, not a `SIGSEGV`.**
* **Diagnostics carry a line, a column and a caret**, in source order, one per
  mistake. Every stage reports every mistake it can still find its footing
  after, so four things wrong in a file is one recompile.

## How it works

| Stage | Module | Produces |
|-------|--------|----------|
| lexing | [`lexer.rs`](src/lexer.rs), [`token.rs`](src/token.rs) | tokens with source spans |
| parsing | [`parser.rs`](src/parser.rs), [`ast.rs`](src/ast.rs) | an AST |
| type checking | [`sema/`](src/sema) | the type of every expression |
| lowering | [`ir/lower/`](src/ir/lower) | a control flow graph of three-address code |
| SSA construction | [`ir/ssa/`](src/ir/ssa) | one definition per virtual register |
| optimisation | [`opt/`](src/opt) | the same graph, with less in it |
| SSA destruction | [`ir/ssa/`](src/ir/ssa) | a graph the allocator can read |
| register allocation | [`codegen/regalloc.rs`](src/codegen/regalloc.rs) | a machine register or stack slot per value |
| emission | [`codegen/x64/`](src/codegen/x64/) | NASM assembly, for either platform |

The middle of the compiler is in **SSA form**: every virtual register has one
definition, and where two of them meet the block grows a parameter.
`--emit ssa` shows it. Four passes are written against that — sparse conditional
constant propagation, copy propagation, global value numbering and dead code
elimination — and `--no-optimise` shows what they did as a diff:

```bash
cargo run -- examples/arith.tc --emit ir --no-optimise
cargo run -- examples/arith.tc --emit ir
```

```text
int a = 6;  int b = 7;  int c = 2;  println(a + b * c);

  %a = const 6
  %b = const 7            becomes
  %c = const 2            ------->          println int 20
  %t3 = mul %b, %c
  %t4 = add %a, %t3
  println int %t4
```

The rule they live by is one line: *a pass may change how long a program takes,
never where it stops*. In a language that halts rather than answer wrongly that
has teeth — an operation whose answer does not fit is never folded, and one that
can fail is never dead however little anybody wanted its result.

Two targets, Windows and Linux, share one x86-64 code generator. A backend
describes its register file, emits its own text, and reports how big a word is
and how many arguments arrive in registers — which is what the front end lays
everything out from.

## Examples

`examples/` holds one program per feature, each written to be read:

| Programs | What they show |
|----------|----------------|
| [`hello.tc`](examples/hello.tc), [`arith.tc`](examples/arith.tc), [`bool.tc`](examples/bool.tc), [`reassign.tc`](examples/reassign.tc) | variables, arithmetic, printing |
| [`float.tc`](examples/float.tc) | IEEE-754 arithmetic, and the four places it is not an `int` |
| [`format.tc`](examples/format.tc) | `print` and `println`, and every specifier a format takes |
| [`control_flow.tc`](examples/control_flow.tc), [`functions.tc`](examples/functions.tc) | branches, loops, calls, recursion |
| [`classes.tc`](examples/classes.tc), [`composition.tc`](examples/composition.tc), [`enums.tc`](examples/enums.tc) | objects, dispatch, matching |
| [`payloads.tc`](examples/payloads.tc) | variants that carry values, and the patterns that take them apart |
| [`arrays.tc`](examples/arrays.tc), [`lists.tc`](examples/lists.tc), [`strings.tc`](examples/strings.tc) | values that do not fit in a register |
| [`tree.tc`](examples/tree.tc) | a class that reaches itself, through a list field |
| [`interactive.tc`](examples/interactive.tc) | reading input |
| [`spill.tc`](examples/spill.tc) | more live values than registers |
| [`showcase.tc`](examples/showcase.tc) | all of it, in one program |
| [`errors/`](examples/errors) | one program per kind of diagnostic |

## Tests

```bash
cargo test
```

Unit tests live beside each stage; five integration suites sit on top, covering
error positions, the CLI, the pipeline as a whole, every backend against the
same contract, and — in [`tests/execution.rs`](tests/execution.rs) — the
compiled programs actually running. Every example above is built, run and
compared against the output recorded beside it. CI does it on Windows and Linux.

## Editing TinyC

[`ide/intellij/`](ide/intellij) is a plugin for RustRover and other
IntelliJ-based IDEs: highlighting, completion, a run configuration that drives
`tinyc`, `nasm` and the linker, and **the compiler's own diagnostics in the
editor** — `tinyc` is run on what is being typed, and nothing in the plugin has
a second opinion about whether a program is correct.

```bash
cd ide/intellij && ./gradlew buildPlugin
```

then *Settings | Plugins | ⚙ | Install Plugin from Disk…*. See
[its README](ide/intellij/README.md) for the details.

## Documentation

[**docs/architecture.md**](docs/architecture.md) is the long form: the whole
grammar, what every feature costs underneath, how SSA, register allocation and
the guarded arithmetic work, and what it takes to add a target.
