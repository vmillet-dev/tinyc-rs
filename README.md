# TinyC

A small compiler, written in Rust 2024 with no dependencies except `clap`, that
turns a tiny typed language into x86-64 assembly.

```c
int x = 10;
int y = 20;
string s = "Hello World";
print(x + y);
print(s);
```

```
30
Hello World
```

## The pipeline

Each arrow is a module, and each stage can be inspected on its own with
`--emit`:

| Stage | Module | Produces |
|-------|--------|----------|
| lexing | [`src/lexer.rs`](src/lexer.rs), [`src/token.rs`](src/token.rs) | tokens with source spans |
| parsing | [`src/parser.rs`](src/parser.rs), [`src/ast.rs`](src/ast.rs) | an AST |
| type checking | [`src/sema.rs`](src/sema.rs) | the type of every expression |
| lowering | [`src/ir.rs`](src/ir.rs) | a control flow graph of three-address code |
| register allocation | [`src/codegen/regalloc.rs`](src/codegen/regalloc.rs) | a machine register or stack slot per value |
| emission | [`src/codegen/x64_win.rs`](src/codegen/x64_win.rs) | NASM assembly |

```bash
cargo run -- examples/hello.tc --emit tokens
```

```bash
cargo run -- examples/hello.tc --emit ir
```

```
str0 = "Hello World"

  0  %x = const 10
  1  %y = const 20
  2  %s = straddr str0
  3  %t3 = add %x, %y
  4  print int %t3
  5  print string %s
```

## Compiling to assembly

```bash
cargo run -- examples/hello.tc -o out/hello.asm
```

Options:

| Flag | Meaning |
|------|---------|
| `-o, --output <FILE>` | where to write the assembly (default: input path with `.asm`) |
| `--emit tokens\|ast\|ir\|asm` | stop after a stage and print its result |
| `--target <NAME>` | target to generate code for (default `x86_64-windows`) |
| `--dump-regalloc` | print live intervals and register assignments |

## Building an executable

The emitted assembly is **NASM syntax**. `tinyc` stops there; `scripts/build.ps1`
takes it the rest of the way with `nasm` and the Microsoft linker:

```powershell
.\scripts\build.ps1 examples\hello.tc
```

You need `nasm` (`winget install nasm` — the script finds it even when winget
does not add it to `PATH`) and a Visual Studio installation with the "Desktop
development with C++" workload, for `link.exe` and the CRT import libraries.

By hand, with `link` on `PATH` from a *x64 Native Tools Command Prompt*:

```
nasm -f win64 -o out\hello.obj out\hello.asm
link /subsystem:console /entry:mainCRTStartup /out:out\hello.exe out\hello.obj msvcrt.lib legacy_stdio_definitions.lib
```

`-f win64` produces a COFF object, which is exactly what `link.exe` expects — so
NASM and the Microsoft linker work together without anything in between.

`print` is compiled into a call to the C runtime's `printf`, which is why the
CRT is linked in. `legacy_stdio_definitions.lib` provides `printf` as a real
symbol, since the UCRT headers normally supply it as an inline function.

## The language

```
program := stmt*
stmt    := decl | assign | print | if | while | for
decl    := ("int" | "string" | "bool") IDENT "=" expr ";"
assign  := IDENT "=" expr ";"
print   := "print" "(" expr ")" ";"
if      := "if" "(" expr ")" block ("else" (block | if))?
while   := "while" "(" expr ")" block
for     := "for" "(" (decl | assign) expr ";" assign ")" block
block   := "{" stmt* "}"
expr    := sum (("==" | "!=" | "<" | "<=" | ">" | ">=") sum)*
sum     := term (("+" | "-") term)*
term    := unary (("*" | "/") unary)*
unary   := "-" unary | primary
primary := INT | STRING | BOOL | IDENT | "(" expr ")"
```

`int` is a 64-bit signed integer, `string` is a pointer to static bytes, `bool`
is `true` or `false`. Arithmetic is `int`-only, `//` starts a comment, and a
variable keeps the type it was declared with — assigning a `string` to an `int`
is an error. There are deliberately no functions or arrays yet.

### Control flow

Conditions must be `bool`, which is what a comparison produces — `if (n)` on an
integer is a type error, not an implicit truth test
([`examples/control_flow.tc`](examples/control_flow.tc)):

```c
if (n < 5) { print("small"); } else if (n < 10) { print("medium"); }

int i = 3;
while (i > 0) { print(i); i = i - 1; }

for (int j = 1; j <= 5; j = j + 1) { total = total + j; }
```

A block is a scope: declarations inside one disappear at its closing brace, an
inner block may shadow an outer name, and a `for` variable does not outlive its
loop. `for` is desugared during lowering — `for (init; cond; step) body`
produces exactly the same IR as `init; while (cond) { body; step; }`.

### What control flow changed underneath

Before conditionals, the IR was one flat list of instructions and each
assignment could introduce a *new* virtual register (`%n`, `%n.1`, ...), so
every register had exactly one definition. Two things broke at once:

* **Variables need one home.** After `if (c) { n = 1; } else { n = 2; }` there is
  no single register that holds `n` unless both branches write the same one.
  Resolving that in SSA needs phi nodes; instead a variable now keeps one
  register for its whole life and may be written many times.
* **Live ranges need real analysis.** A single forward pass cannot see that a
  value assigned at the bottom of a loop is read again at the top. The allocator
  now computes live-in/live-out sets per block by iterating to a fixpoint.

So the IR is a control flow graph: basic blocks, each ending in a terminator
naming its successors.

```
$ cargo run -- examples/control_flow.tc --emit ir
entry0:
  0  %total = const 0
  1  %i = const 1
  2  jump loop1
loop1:
  3  %t2 = cmp <= %i, 5
  4  branch %t2 ? body2 : done3
body2:
  5  %total = add %total, %i
  6  %i = add %i, 1
  7  jump loop1
done3:
  8  print int %total
  9  return
```

## Errors

Every diagnostic points at a line and a column:

```
$ cargo run -- examples/errors/type_mismatch.tc
error: cannot apply `+` to `int` and `string`
 --> examples/errors/type_mismatch.tc:3:11
  |
3 | print(x + s);
  |           ^ expected int, found string
```

Columns are counted in characters rather than bytes, so non-ASCII text earlier
on a line does not shift them. `examples/errors/` holds one program per kind of
error.

## Register allocation

The allocator is a linear scan over the IR's virtual registers, and it is
target-independent: the backend hands it a `RegisterFile` describing which
machine registers exist and which of them survive a call, and gets back a
location for every value. Live ranges come from a backward dataflow analysis
over the control flow graph, so a value carried around a loop stays live across
the back edge.

```bash
cargo run -- examples/hello.tc --dump-regalloc
```

```
vreg  live range   across call  location
%x    [ 0,  3]        no          r8
%y    [ 1,  3]        no          r9
%s    [ 2,  5]       yes         rbx
%t3   [ 3,  4]        no          r9

0 spill slot(s), callee-saved used: rbx
```

Three things worth noticing:

* `%t3` reuses `r9`, the register `%y` occupied — a temporary hands its register
  to the next one as soon as it dies, so long expressions do not need long rows
  of registers. `examples/arith.tc` computes twenty-one temporaries in two.
* `%s` is still needed after the first `printf`, so it goes in `rbx`, a
  callee-saved register, and the prologue pushes it. Values that die before the
  call stay in the cheap caller-saved registers.
* When registers run out the longest-lived value is spilled to a stack slot, and
  slots are recycled just like registers. `examples/spill.tc` forces this.

## Targeting another platform

Everything up to and including register allocation is target-independent. A new
target needs:

1. a module implementing `Backend` (`name`, `register_file`, `emit`);
2. a variant in `codegen::Target` and an entry in `codegen::TARGETS`.

The x64 Windows backend keeps the ABI-critical registers (`rcx`, `rdx` for
arguments, `rax`/`rdx` for `idiv`, `r10`/`r11` as scratch) out of the
allocator's hands, so the allocator only ever reasons about "caller-saved" and
"callee-saved" pools.

## Tests

```bash
cargo test
```

Unit tests live beside each stage; `tests/error_positions.rs` asserts the exact
line and column reported for every program in `examples/errors/`.
