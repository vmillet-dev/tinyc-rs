# TinyC

A small compiler, written in Rust 2024 with no dependencies except `clap`, that
turns a tiny typed language into x86-64 assembly.

```c
fn add(int a, int b) -> int {
  return a + b;
}

fn main() {
  string s = "Hello World";
  print(add(10, 20));
  print(s);
}
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
fn double(%n) -> int:
entry0:
  0  %n = param 0
  1  %t1 = mul %n, 2
  2  return %t1

fn main():
entry0:
  0  %t0 = call double(21)
  1  print int %t0
  2  return
```

Each function owns its blocks and its virtual registers, so both are numbered
from zero in every one of them. Only string literals are shared, because they
all end up in a single `.data` section.

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
program := fn_decl*
fn_decl := "fn" IDENT "(" params? ")" ("->" type)? block
params  := param ("," param)*
param   := type IDENT
type    := "int" | "string" | "bool"
stmt    := decl | assign | print | if | while | for | return | call ";"
decl    := type IDENT "=" expr ";"
assign  := IDENT "=" expr ";"
print   := "print" "(" expr ")" ";"
if      := "if" "(" expr ")" block ("else" (block | if))?
while   := "while" "(" expr ")" block
for     := "for" "(" (decl | assign) expr ";" assign ")" block
return  := "return" expr? ";"
block   := "{" stmt* "}"
expr    := sum (("==" | "!=" | "<" | "<=" | ">" | ">=") sum)*
sum     := term (("+" | "-") term)*
term    := unary (("*" | "/") unary)*
unary   := "-" unary | primary
primary := INT | STRING | BOOL | IDENT | call | "(" expr ")"
call    := IDENT "(" (expr ("," expr)*)? ")"
```

`int` is a 64-bit signed integer, `string` is a pointer to static bytes, `bool`
is `true` or `false`. Arithmetic is `int`-only, `//` starts a comment, and a
variable keeps the type it was declared with — assigning a `string` to an `int`
is an error. There are deliberately no arrays yet.

### Functions

A program is a list of functions and nothing else; execution starts at `main`,
which takes no parameters and returns nothing
([`examples/functions.tc`](examples/functions.tc)):

```c
fn add(int a, int b) -> int {
  return a + b;
}

fn banner(string title) {     // no `->`: this one returns nothing
  print(title);
}

fn fib(int n) -> int {
  if (n < 2) { return n; }
  else { return fib(n - 1) + fib(n - 2); }
}
```

* **Signatures are collected before any body is checked.** That is what makes
  `fib` visible inside `fib`, and lets `main` call a function declared below it.
  A single pass could only ever look backwards.
* **A function returning nothing has no return type at all**, rather than a
  `void` one. In the AST that is `ret: Option<Ty>`, so `Ty` keeps meaning "a type
  a value can have" and no `match` anywhere gains an impossible arm.
* **A non-`void` function must return on every path.** The check is deliberately
  simple — a loop is never assumed to run, so `while (true) { return 1; }` is
  rejected. It can only ever reject a program that would in fact have been fine.
* **A call returning nothing is a statement, never a value.** `greet("hi");` is
  the only expression statement in the language, and it exists precisely so that
  a `void` function is callable at all.
* **At most four parameters**, because that is how many the Microsoft x64 ABI
  passes in registers. A fifth would need stack arguments.

### What functions changed underneath

Two things stopped being singular.

* **The IR grew a level.** `ir::Program` used to be one list of basic blocks;
  it is now a list of `Function`s, each owning its own blocks *and* its own
  virtual registers. `BlockId` and `VReg` became indices into a function rather
  than into the program, which is what lets the allocator run once per function
  and give each one its own frame.
* **A parameter needs a definition point.** Arguments arrive in registers, and
  `Instr::Param` at the top of the entry block is what records that. Without it
  liveness would start a parameter's interval at its *first use*, and the
  register the argument arrived in could be handed to something else in the
  meantime.

```
$ cargo run -- examples/functions.tc --emit ir | head
str0 = "--------"

fn add(%a, %b) -> int:
entry0:
  0  %a = param 0
  1  %b = param 1
  2  %t2 = add %a, %b
  3  return %t2
```

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
 --> examples/errors/type_mismatch.tc:4:13
  |
4 |   print(x + s);
  |             ^ expected int, found string
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
the back edge. It runs **once per function**, so each one gets its own registers,
its own spill slots and its own frame.

```bash
cargo run -- examples/hello.tc --dump-regalloc
```

```
fn main():
vreg     live range   across call  location
%x       [ 0,  3]        no         rbx
%y       [ 1,  3]        no         rsi
%s       [ 2,  5]       yes         rdi
%t3      [ 3,  4]        no         rsi
%isReady [ 6,  7]        no         rdi
%i       [ 8, 14]       yes         rdi
%t6      [10, 11]        no         rsi

0 spill slot(s), callee-saved used: rbx, rsi, rdi
```

Three things worth noticing:

* `%t3` reuses `rsi`, the register `%y` occupied — a temporary hands its register
  to the next one as soon as it dies, so long expressions do not need long rows
  of registers. `examples/arith.tc` computes twenty-one temporaries in two.
* `%s` is still needed after the first `printf`, so it must survive that call.
  `%t3` dies before it and can share `rsi` with `%y`.
* When registers run out the longest-lived value is spilled to a stack slot, and
  slots are recycled just like registers. `examples/spill.tc` forces this.

### Why every register here is callee-saved

`r8` and `r9` used to be allocatable. They are also argument registers three and
four, and once calls exist that is a contradiction: setting up `f(x, y, z)`
writes `r8`, which may be exactly where `z` is still waiting to be read. Solving
that in general is the *parallel move* problem — the moves have to be ordered,
and cycles broken with a temporary.

Withdrawing `r8` and `r9` from the pool sidesteps it. No source of an argument
move can be an argument register any more, so the moves can be emitted in any
order. The cost is a `push`/`pop` pair per register used, which is a good trade
at this size — and it is the kind of decision a register allocator exists to
make explicit.

## Targeting another platform

Everything up to and including register allocation is target-independent. A new
target needs:

1. a module implementing `Backend` (`name`, `register_file`, `emit`);
2. a variant in `codegen::Target` and an entry in `codegen::TARGETS`.

The x64 Windows backend keeps the ABI-critical registers (`rcx`, `rdx`, `r8`,
`r9` for arguments, `rax` for `idiv` and return values, `r10`/`r11` as scratch)
out of the allocator's hands, so the allocator only ever reasons about
"caller-saved" and "callee-saved" pools — never about x86.

## Tests

```bash
cargo test
```

Unit tests live beside each stage; `tests/error_positions.rs` asserts the exact
line and column reported for every program in `examples/errors/`.
