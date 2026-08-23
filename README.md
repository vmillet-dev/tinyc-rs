# TinyC

**A compiler for a small typed language, written from scratch in Rust — source
text in, x86-64 assembly out.**

No LLVM, no parser generator, no dependency but `clap`. Every stage is hand
written and can be inspected on its own: the lexer, the recursive-descent
parser, the type checker, the lowering to a control flow graph of three-address
code, the linear-scan register allocator, and the backend that emits NASM.

The language it accepts is small on purpose, but it is a real one — classes with
inheritance and virtual dispatch, enums with exhaustive matching, fixed arrays
and growable lists, Unicode-aware strings, and arithmetic that stops the program
rather than answering wrongly.

## Features

**The language**

* `int` (64-bit signed), `bool`, `char` (one Unicode character) and `string` — a
  run of characters, so `len` counts characters, `s[i]` is one, and UTF-8 exists
  only at the edges.
* Fixed arrays whose length is part of the type (`int[3]`), and growable lists
  whose length is a fact about the data (`int[]`).
* Classes with fields, methods, single inheritance and virtual dispatch — plus
  composition: an object holds whole arrays and whole objects *inside* itself.
* Enums, and a `match` that must cover every variant, usable as an expression or
  as a statement.
* `if` / `else if` / `else`, `while`, `for`, `break`, `continue`, and
  short-circuiting `&&`, `||` and `!`.
* Value semantics throughout: no references, no globals, no null. Assignment
  copies; nothing aliases anything else.
* No implicit conversions at all — `string(n)`, `int(c)` and `char(n)` are
  written out, so a message with a number in it says where the number became
  text.
* Three built-ins for input: `read_line()`, `eof()` and `is_int(s)`.

**Safety**

* **Arithmetic never answers wrongly.** Overflow, division by zero and
  `i64::MIN / -1` stop the program with a message instead of wrapping.
* **Every index is checked.** One the compiler can work out is checked before
  the program is built; only one it cannot see costs anything at run time.
* **Diagnostics point at a line and a column**, with a window of the source and
  a caret, sorted into source order, one message per mistake, and columns
  counted in characters so accents do not shift them.

**The compiler**

* Constant folding, dead-function elimination, compare-and-branch fusion and
  frameless leaf functions — each small enough to read in one sitting, and each
  visible in the output of some `--emit`.
* Linear-scan register allocation over live ranges computed by a backward
  dataflow pass on the control flow graph, run once per function.
* Target-independent up to and including register allocation: a backend
  describes its own register file and emits its own text.
* Tested end to end — the integration suite assembles, links and **runs** the
  compiled programs, comparing what they print.

## Quick start

```bash
cargo run -- examples/hello.tc -o out/hello.asm
```

`tinyc` emits NASM assembly and stops there. To get an executable on Windows:

```powershell
.\scripts\build.ps1 examples\hello.tc
```

That needs `nasm` (`winget install nasm`) and a Visual Studio installation with
the "Desktop development with C++" workload, for `link.exe` and the CRT import
libraries — `print` compiles into a call to the C runtime's `printf`, which is
why the CRT is linked in.

Every stage can be printed on its own:

```bash
cargo run -- examples/hello.tc --emit ir
```

| Flag | Meaning |
|------|---------|
| `-o, --output <FILE>` | where to write the assembly (default: input path with `.asm`) |
| `--emit tokens\|ast\|ir\|asm` | stop after a stage and print its result |
| `--target <NAME>` | target to generate code for (default `x86_64-windows`) |
| `--dump-regalloc` | print live intervals and register assignments |

## How it works

Each arrow is a module, and each stage can be inspected with `--emit`:

| Stage | Module | Produces |
|-------|--------|----------|
| lexing | [`src/lexer.rs`](src/lexer.rs), [`src/token.rs`](src/token.rs) | tokens with source spans |
| parsing | [`src/parser.rs`](src/parser.rs), [`src/ast.rs`](src/ast.rs) | an AST |
| type checking | [`src/sema.rs`](src/sema.rs) | the type of every expression |
| lowering | [`src/ir.rs`](src/ir.rs) | a control flow graph of three-address code |
| register allocation | [`src/codegen/regalloc.rs`](src/codegen/regalloc.rs) | a machine register or stack slot per value |
| emission | [`src/codegen/x64_win.rs`](src/codegen/x64_win.rs) | NASM assembly |

## Examples

`examples/` holds one program per feature, each written to be read:

| Programs | What they show |
|----------|----------------|
| [`hello.tc`](examples/hello.tc), [`arith.tc`](examples/arith.tc), [`bool.tc`](examples/bool.tc), [`reassign.tc`](examples/reassign.tc) | variables, arithmetic, printing |
| [`control_flow.tc`](examples/control_flow.tc), [`functions.tc`](examples/functions.tc) | branches, loops, calls, recursion |
| [`classes.tc`](examples/classes.tc), [`composition.tc`](examples/composition.tc), [`enums.tc`](examples/enums.tc) | objects, dispatch, matching |
| [`arrays.tc`](examples/arrays.tc), [`lists.tc`](examples/lists.tc), [`strings.tc`](examples/strings.tc) | values that do not fit in a register |
| [`interactive.tc`](examples/interactive.tc) | reading input |
| [`spill.tc`](examples/spill.tc) | more live values than registers |
| [`errors/`](examples/errors) | one program per kind of diagnostic |

## Tests

```bash
cargo test
```

Unit tests live beside each stage; five integration suites sit on top, covering
error positions, the CLI, the pipeline as a whole, every backend against the
same contract, and — in [`tests/execution.rs`](tests/execution.rs) — the
compiled programs actually running.

## Showcase

Everything the language has, in one program.

```c
// A weather log, in one program: every feature TinyC has.

// An enum is a type the compiler can count, so a `match` on one is checked.
enum Level { Info, Warn, Error }

// A class is data with behaviour. `self` is the object, borrowed from the caller.
class Reading {
  string place;
  int degrees;

  fn label(self) -> string {
    return self.place + " " + string(self.degrees);   // nothing converts on its own
  }
  fn level(self) -> Level {
    return Level::Info;
  }
}

// A subclass may stand for its base. An override keeps its base's vtable slot,
// so a call through a `Reading` lands here.
class Frost : Reading {
  int wind;
  fn level(self) -> Level {
    if (self.degrees < -10 || self.wind > 60) {   // `||` short-circuits
      return Level::Error;
    }
    return Level::Warn;
  }
}

// Composition: what a class holds, it holds *inside* — a whole array and a
// whole object, never an address. `peak` reserves the room the biggest class in
// the hierarchy needs, so it keeps answering as whatever was put in it.
class Station {
  string name;
  int[3] samples;
  Reading peak;

  fn total(self) -> int {
    int sum = 0;
    for (int i = 0; i < len(self.samples); i = i + 1) {
      sum = sum + self.samples[i];
    }
    return sum;
  }
}

// A match is an expression, and it must cover every variant.
fn tag(Level l) -> string {
  return match (l) {
    Level::Info => "ok",
    Level::Warn => "watch",
    Level::Error => "alert",
  };
}

// Signatures are collected before any body is checked, so `fib` sees itself.
fn fib(int n) -> int {
  if (n < 2) {
    return n;
  }
  return fib(n - 1) + fib(n - 2);
}

// A string is a run of *characters*, not bytes: `len` counts characters and
// `text[i]` is one. A char list grows in place, then becomes a string once.
fn shout(string text) -> string {
  char[] out = [];
  for (int i = 0; i < len(text); i = i + 1) {
    char c = text[i];
    if (c >= 'a' && c <= 'z') {
      c = char(int(c) - 32);
    }
    push(out, c);
  }
  return string(out);
}

// A list's length is a fact about the data rather than the program, so it grows
// and may be returned. `int[3]` and `int[]` are deliberately different types.
fn readings() -> Reading[] {
  Reading[] rs = [];
  push(rs, Reading { place: "Nice", degrees: 21 });
  push(rs, Frost { place: "Oslo", degrees: -12, wind: 40 });
  push(rs, Frost { place: "Bergen", degrees: -2, wind: 70 });
  return rs;
}

// No return type means no value: this can only be called as a statement.
fn banner(string title) {
  print("== " + shout(title) + " ==");
}

fn main() {
  banner("weather log");

  // Dispatch decided by the object, over a list nobody counted in advance.
  Reading[] rs = readings();
  int alerts = 0;
  for (int i = 0; i < len(rs); i = i + 1) {
    Level l = rs[i].level();
    print(rs[i].label() + " -> " + tag(l));
    if (l == Level::Error) {
      alerts = alerts + 1;
    }
  }
  print("alerts: " + string(alerts));

  // An object literal, a field written through, and a method on a field.
  Station s = Station {
    name: "Alpha",
    samples: [3, 1, 4],
    peak: Frost { place: "Oslo", degrees: -12, wind: 40 }
  };
  print(s.name + ": " + string(s.total()));   // Alpha: 8
  s.samples[0] = 30;
  print(s.name + ": " + string(s.total()));   // Alpha: 35
  print(tag(s.peak.level()));                 // alert — the vtable travelled with the copy

  // if / else if / else, and a `!` that inverts a comparison rather than a value.
  if (alerts > 2) {
    print("storm");
  } else if (!(alerts == 0)) {
    print("watch");
  } else {
    print("clear");
  }

  // while, continue and break; `%` takes its sign from the dividend.
  int n = 0;
  while (n < 10) {
    n = n + 1;
    if (n % 2 == 0) {
      continue;
    }
    if (n > 5) {
      break;
    }
    print(n);            // 1, 3, 5
  }

  // Recursion, and arithmetic that stops the program rather than wrapping.
  print(fib(10));        // 55
  print(6 * 7 % 10);     // 2

  // A match written as a statement runs its arms for effect.
  match (rs[2].level()) {
    Level::Info => { print("quiet"); }
    Level::Warn => { print("cold"); }
    Level::Error => { print("freezing"); }
  }

  // Input, when there is any: `eof()` looks without consuming, `read_line()`
  // takes one line, `is_int` says whether it spells a number.
  int extra = 0;
  while (!eof()) {
    string line = read_line();
    if (is_int(line)) {
      extra = extra + int(line);
    }
  }
  print("piped in: " + string(extra));
}
```

Compiled, linked and run with `12`, `7` and `x` on standard input:

```text
== WEATHER LOG ==
Nice 21 -> ok
Oslo -12 -> alert
Bergen -2 -> alert
alerts: 2
Alpha: 8
Alpha: 35
alert
watch
1
3
5
55
2
freezing
piped in: 19
```

## Documentation

[**docs/architecture.md**](docs/architecture.md) is the long form: the whole
grammar, what every feature costs underneath, how register allocation and the
guarded arithmetic work, and what it takes to add a target.
