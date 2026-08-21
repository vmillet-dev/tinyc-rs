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
symbol, since the UCRT headers normally supply it as an inline function. A
program containing a division that has to be guarded also reaches for `_write`
and `exit`, both from the same library — see [Runtime failures](#runtime-failures).

## The language

```
program := fn_decl*
fn_decl := "fn" IDENT "(" params? ")" ("->" type)? block
params  := param ("," param)*
param   := type IDENT
type    := "int" | "string" | "bool"
stmt    := decl | assign | print | if | while | for | return | break | continue | call ";"
decl    := type IDENT "=" expr ";"
assign  := IDENT "=" expr ";"
print   := "print" "(" expr ")" ";"
if      := "if" "(" expr ")" block ("else" (block | if))?
while   := "while" "(" expr ")" block
for     := "for" "(" (decl | assign) expr ";" assign ")" block
return  := "return" expr? ";"
break   := "break" ";"
cont    := "continue" ";"
block   := "{" stmt* "}"
expr    := and ("||" and)*
and     := cmp ("&&" cmp)*
cmp     := sum (("==" | "!=" | "<" | "<=" | ">" | ">=") sum)*
sum     := term (("+" | "-") term)*
term    := unary (("*" | "/") unary)*
unary   := "-" unary | primary
primary := INT | STRING | BOOL | IDENT | call | "(" expr ")"
call    := IDENT "(" (expr ("," expr)*)? ")"
```

`int` is a 64-bit signed integer, `string` is a pointer to static bytes, `bool`
is `true` or `false`. Arithmetic is `int`-only, `&&` and `||` are `bool`-only,
`//` starts a comment, and a variable keeps the type it was declared with —
assigning a `string` to an `int` is an error. There are deliberately no arrays
yet, and no `!`: the only way that character can appear is in `!=`.

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
  passes in registers. A fifth would need stack arguments. The number is the
  target's to state, not the language's: it comes from
  `RegisterFile::max_args`, and `sema::check` enforces whatever the backend
  reports rather than a constant of its own.

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

`break` leaves the innermost enclosing loop and `continue` starts its next
iteration; outside a loop, both are errors. Neither is a syntactic question, so
the parser accepts them anywhere and `sema` — the first stage that counts how
deeply a statement is nested — is what rejects them.

```c
for (int c = 1; c <= 10; c = c + 1) {
  if (c == 2) { continue; }   // the step still runs on the way past
  if (c == 4) { break; }
  print(c);                   // 1, 3
}
```

That parenthesis about the step is the whole difficulty. Lowering a `continue`
in a `for` cannot simply jump back to the header, because the step lives at the
end of the body and skipping it would leave the counter alone forever. So the
step gets a block of its own — a *latch* — that both the body and every
`continue` jump to. It is created only when a `continue` needs it, which is what
keeps a plain `for` lowering to exactly the `while` it desugars into.

### Short-circuiting `&&` and `||`

`&&` and `||` evaluate their right operand only when the left one has not
already settled the answer, which is what makes a guard like this work:

```c
int zero = 0;
print(zero == 0 || total / zero > 1);   // true, without ever dividing
```

They are **not** `BinOp`s in the AST, and there is no `and` or `or` instruction
in the IR. There could not usefully be one: an instruction reads both its
operands, and not reading one is the entire point. Short circuiting *is* control
flow, so `a && b` lowers to the same diamond an `if` produces, with both arms
writing the destination the join then reads:

```
  0  %t2 = cmp > %x, 1
  1  branch %t2 ? rhs1 : short2
rhs1:
  2  %ok = call f(%x)
  3  jump join3
short2:
  4  %ok = const 0
  5  jump join3
join3:
  6  print bool %ok
```

Writing one register from two blocks is only expressible because the IR is not
in SSA form — the same property that let `if (c) { n = 1; } else { n = 2; }`
work without phi nodes.

Two details earn their keep:

* **A known left operand drops the right one outright.** `false && f()` folds to
  `false` and never emits the call, which is not an optimisation but the
  semantics. `true && e` folds the other way, to just `e`.
* **The arm the branch continues into is laid out first**, so the backend
  reaches it by falling through: the right operand for `&&`, the short circuit
  for `||`. Both spellings then cost one conditional jump and one unconditional
  one, instead of `||` paying for a jump to the block that was already next.

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

Three rules keep a list of them readable:

* **Source order.** The type checker walks a statement's value before its name,
  so it finds mistakes out of order; the diagnostics are sorted before they are
  printed.
* **One mistake, one message.** `y = y + 1` mentions an undeclared `y` twice and
  is reported once — the first time a name is missed in a function is the only
  time it is worth saying so.
* **A window, not the whole line.** A generated file can hold a line of any
  length; only the hundred characters around the caret are echoed, with `...`
  for what was cut.

### The nesting limit

Recursive descent turns nesting in the source into frames on the call stack, and
so do the type checker, the lowering, and dropping the tree afterwards. Left
alone, `((((...))))` is not a parse error but a stack overflow — a crash instead
of a diagnostic. Two constants keep that from happening, and have to be read
together:

* `parser::MAX_NESTING` caps how deeply expressions and blocks may nest, three
  orders of magnitude beyond anything written by hand;
* `STACK_SIZE` is the stack the pipeline runs on, because a debug build spends
  several kilobytes per level and a thread gets a megabyte by default.

Anything running the pipeline should go through `tinyc::with_compiler_stack`,
which is what the CLI does.

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

## What gets optimised

Four passes, each small enough to read in one sitting, and each visible in the
output of some `--emit`.

**Constant folding, during lowering.** `print(1 + 2 * 3)` reaches the backend as
`print int 7`: no `add`, no `mul`, and no register to hold the answer. A division
the machine would refuse is deliberately *not* folded — `checked_div` answers
`None` for both `x / 0` and `i64::MIN / -1`, so those stay instructions and the
program fails where it was written rather than inside the compiler.

**Dead functions, during lowering.** `ir::prune_unreachable_functions` walks the
call graph from `main` exactly as `prune_unreachable` walks the control flow
graph from block 0, and renumbers the `FuncId`s the survivors call each other
by. A helper nobody calls costs a label, a prologue and an epilogue otherwise.

**Compare-and-branch fusion, in the backend.** x86 compares by setting flags and
`jcc` reads them straight back, so a comparison whose only reader is the branch
right after it never has to become a 0 or a 1:

```
  3  %t2 = cmp <= %i, 5          .loop1:
  4  branch %t2 ? body2 : done3      cmp  rsi, 5
                                     jg   .done3
```

Seven instructions became two, in the hottest place a program has. This lives in
the backend rather than in the IR because flags are an x86 concept — with the
one cost that the allocator has already found `%t2` a register by then, and
keeps it reserved. Moving the decision earlier would mean teaching the
target-independent half of the compiler about flags.

**Leaf frames.** A function that calls nothing — not another function, and not
the runtime's abort — has no call to align the stack for and no callee to leave
shadow space for, so it reserves room for its spills and nothing else. Most
leaves spill nothing and get no frame at all.

## Runtime failures

`idiv` does not answer `x / 0`, and it does not answer `i64::MIN / -1` either:
the quotient does not fit, so the CPU faults exactly as it does on a zero
divisor. Left alone that is a silent `0xC0000094` with nothing printed.

Each division is guarded, and each guard a literal operand already answers is
left out — `n / 7` carries no check at all, `n / (0 - 1)` checks only for
overflow. When a check does fire, control jumps to an out-of-line stub that
writes a message to standard error and leaves with a non-zero status:

```
$ ./divide_by_zero.exe
runtime error: division by zero
$ echo $?
1
```

The stub is reached by `jmp`, not `call`, so it runs on the frame of whoever
jumped to it — which is why a function containing a guarded division is never
treated as a leaf.

## Symbol names

Every TinyC function is emitted as `tc$name`, and the compiler's own helpers as
`tc$rt$name`. A `$` is a valid character in a NASM identifier and one TinyC's
lexer will never produce, so the two namespaces cannot meet.

That matters more than it looks. `print` compiles into a call to the C runtime's
`printf`; without the prefix, `fn printf()` defines the very symbol that call
reaches, and the program compiles, links, runs, and quietly does nothing — while
`fn str0()` collides with a string literal's label and stops NASM outright.

`main` is the exception, and has to be: it is the name the C runtime startup
calls. Nothing the backend generates is called `main`, so it is safe alone.

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

Unit tests live beside each stage, and two integration suites sit on top:

* `tests/error_positions.rs` asserts the exact line and column reported for
  every program in `examples/errors/`, and checks the *shape* of the emitted
  assembly — that every path out of a function undoes its prologue, and that no
  generated symbol is one a TinyC function could also claim.
* `tests/execution.rs` **runs the compiled programs.** Every other test in the
  repository inspects text, and text cannot tell a `setl` from a `setg` or
  notice a register clobbered between two instructions that each look right on
  their own. Each example is assembled, linked and run, and its output compared
  against `examples/expected/`; a second test does the same for the corners of
  code generation that are easiest to get subtly wrong — a destination that
  aliases its own operand, an immediate too wide for an instruction, enough live
  values to force spills.

The execution suite needs `nasm` and the Microsoft linker. When it cannot find
them it says so and passes, so `cargo test` still works without a toolchain:

```bash
cargo test --test execution -- --nocapture
```

`link.exe` only knows where the C runtime is if `vcvars64.bat` has told it, so
the suite runs that once, keeps the `LIB` it sets, and calls the linker directly
from then on.
