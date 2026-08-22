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
program containing guarded arithmetic also reaches for `_write` and `exit`, both
from the same library — see
[Arithmetic never answers wrongly](#arithmetic-never-answers-wrongly).

## The language

```
program := (enum_decl | fn_decl)*
enum    := "enum" IDENT "{" IDENT ("," IDENT)* "}"
fn_decl := "fn" IDENT "(" params? ")" ("->" type)? block
params  := param ("," param)*
param   := type IDENT
type    := "int" | "string" | "bool" | IDENT
stmt    := decl | assign | print | if | while | for | match | return
         | break | continue | call ";"
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
term    := unary (("*" | "/" | "%") unary)*
unary   := ("-" | "!") unary | primary
primary := INT | STRING | BOOL | IDENT | variant | call | match
         | "(" expr ")"
variant := IDENT "::" IDENT
call    := IDENT "(" (expr ("," expr)*)? ")"
match   := "match" "(" expr ")" "{" arm* "}"
arm     := IDENT "::" IDENT "=>" (expr ","? | block ","?)
```

`int` is a 64-bit signed integer, `string` is a pointer to static bytes, `bool`
is `true` or `false`, and an `enum` declares a type of its own with a fixed set
of values. Arithmetic is `int`-only, `&&`, `||` and `!` are `bool`-only, `//`
starts a comment, and a variable keeps the type it was declared with — assigning
a `string` to an `int` is an error. There are deliberately no arrays yet.

There is no implicit truth test either, so `!n` on an `int` is a type error
rather than a comparison against zero. `%` takes its sign from the dividend, as
in C: `-7 % 2` is `-1`. Arithmetic that has no answer stops the program rather
than wrapping — see
[Arithmetic never answers wrongly](#arithmetic-never-answers-wrongly).

### Functions

A program is a list of enums and functions, with no top-level statements;
execution starts at `main`,
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
  one of the language's two expression statements — a `match` written for its
  effect is the other — and it exists precisely so that a `void` function is
  callable at all.
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

### Enums and exhaustive matching

An `enum` declares a type with a fixed set of values, and a `match` must handle
every one of them ([`examples/enums.tc`](examples/enums.tc)):

```c
enum Colour { Red, Green, Blue }

fn temperature(Colour c) -> string {
  return match (c) {
    Colour::Red => "warm",
    Colour::Green => "cool",
    Colour::Blue => "cold",
  };
}
```

Forget one and the program does not build:

```
error: this match does not cover every variant of `Colour`
 --> colour.tc:4:3
  |
4 |   match (c) {
  |   ^^^^^ `Green` and `Blue` are not handled
  = note: every variant needs an arm; TinyC has no catch-all pattern, so that
          adding a variant cannot be quietly ignored
```

**There is deliberately no `_` pattern.** A catch-all is exactly what would
absorb a new variant in silence, and turning "I added a case and forgot
somewhere" from a bug into a compile error is the entire point of the check.

The decisions behind the rest of it:

* **A variant carries no payload, so a value *is* its variant's index.** It
  moves and compares like an int, and the backend needs nothing new for it —
  only a table of names, and only for an enum something actually prints.
* **A variant is always written qualified**, `Colour::Red`. So two enums may
  both have a `Red`, no enum can shadow a variable, and telling a variant from a
  variable or a call needs one token of lookahead and no name table.
* **Enums answer `==` and `!=` but nothing about order.** The declaration puts
  the variants in a sequence, but the program never said that sequence meant
  anything, so `Colour::Red < Colour::Blue` is a type error rather than an
  invented answer.
* **An exhaustive match counts as returning.** A match whose every arm is a
  block ending in `return` needs no trailing `return` after it — there is no
  path that reaches the end of the body. This is the check paying for itself.

#### A match is an expression

The form above produces a value, so it fits anywhere one does — after `return`,
in an initialiser, as an operand. Two shapes of arm, and the token after `=>`
picks between them with no lookahead:

```c
Colour::Red   => "warm",                              // a value
Colour::Green => { print("thinking"); return "cool"; } // statements
```

**A block arm never produces a value.** `return` keeps its single meaning of
leaving the *function*, rather than gaining a second one inside a match — giving
one keyword two meanings by context is exactly the guessing this language
refuses. So where a match is used as a value, a block arm has to be one control
never falls out of: it must `return`, `break` or `continue`. Anything else is
rejected, because there would be a path with no value to hand back.

```
error: this arm produces no value
6 |     Colour::Green => { print("thinking"); }
  |             ^^^^^ but the match it belongs to is used as one
  = note: an arm is either an expression, or a block that returns, breaks or continues
```

The other three checks that fall out of this: every value arm must agree on a
type (the first one sets it, and the diagnostic points back at it); a match used
as a statement may not have value arms, since TinyC discards no values; and a
match whose every arm leaves produces nothing, so it cannot be used as a value
at all.

A `match` written for its effect is a statement in the one way a call is —
`Stmt::Match` wraps the same node that `ExprKind::Match` is. Those two are now
the only expression statements in the language, and each exists for the same
reason: without them, a `void` function and an effects-only match would be
unwritable.

Two consequences reach further down than they look. The parser cannot produce a
type any more: `Colour c = ...` and `int c = ...` are the same shape, and only
the stage holding the table of declared types knows whether `Colour` is one. So
syntax carries a `TypeRef` — a name and a span — and resolution happens once, in
`sema`. And a declaration may now begin with a plain identifier, so what tells
`Colour c` from `c = 1` and `c(1)` is the *second* token.

Lowering a match is a chain of equality tests against the tag, with one saving
that only exhaustiveness makes safe: **the last arm is not tested at all.** There
is nowhere else for the value to be, so three variants cost two comparisons —
each one fused into its branch, so none of them ever becomes a 0 or a 1.

```
  0  %t1 = cmp == %c, 0
  1  branch %t1 ? arm1 : case2
arm1: ...
case2:
  5  %t3 = cmp == %c, 1
  6  branch %t3 ? arm3 : arm4      ; `arm4` is Blue, reached by elimination
```

When the match is a value, every value arm writes the **same** register before
jumping to the join — the trick `&&` already plays, and the one a non-SSA IR is
what allows. A diverging block arm writes nothing and never arrives:

```
arm1:  %t1 = straddr str0    ; jump join5
arm3:  print string %t4
       return %t5            ; leaves the function, never reaches join5
arm4:  %t1 = straddr str3    ; jump join5
join5: return %t1
```

Printing a value is the one place the names survive into the binary. It is the
same lookup a `bool` already did — that picks between `"true"` and `"false"` —
with a table in place of the choice:

```nasm
enum0_v0: db 82, 101, 100, 0          ; "Red"
enum0_names: dq enum0_v0, enum0_v1, enum0_v2
...
lea  r11, [enum0_names]
mov  rdx, [r11+r10*8]
```

The index is safe by construction rather than by checking: a tag can only have
come from a variant of that very enum, because there is no cast, no arithmetic
on enums, and no other way to make one.

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

`!` takes the opposite route. There is no `not` instruction in the IR and none
is wanted, because `!x` **is** `x == 0` — a comparison, which already folds and
already fuses into the branch that reads it. When the operand is itself a
comparison the lowering goes one better and inverts it in place, so `!(a < b)`
becomes `a >= b`:

```
if (a < b)  { ... }      cmp  rbx, rsi      if (!(a < b)) { ... }      cmp  rbx, rsi
                         jge  .join2                                   jl   .join2
```

Negation costs nothing at all there — the same single `cmp`, with the jump the
other way round. The general case, `!ok` on a value that is not a comparison,
costs one `cmp reg, 0` that the branch absorbs.

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

## Arithmetic never answers wrongly

**No operation on an `int` ever produces a value that is not the answer.** When
one cannot, the program stops and says so; it never hands on a wrong number as
if it were right.

Three things can go wrong, and all three are caught:

| | |
|---|---|
| `x / 0`, `x % 0` | `idiv` faults |
| `i64::MIN / -1`, `i64::MIN % -1` | the quotient does not fit, so `idiv` faults again — even for `%`, whose answer is 0 on paper |
| `a + b`, `a - b`, `a * b` past the range | the result does not fit |

The first two the CPU refuses outright; left alone that is a silent
`0xC0000094` with nothing printed. The third it performs happily, wrapping
around to a number of the wrong sign — which is worse, because nothing marks it.

The rule is checked in whichever of two places can see it:

**At compile time, when the operands are known.** `sema` evaluates constant
arithmetic through the same `BinOp::apply` the folder uses, so the two can never
disagree, and rejects what has no answer:

```
error: this multiplication overflows an `int`
 --> big.tc:2:9
  |
2 |   print(2 * 2 * 4611686018427387904);
  |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^ `18446744073709551616` does not fit in an `int`
  = note: `int` values must fit in -9223372036854775808..=9223372036854775807
```

It reaches through nesting — the left operand above is itself an expression —
but never looks a variable up, because a variable's value is not that stage's
business.

**At runtime otherwise.** `add`, `sub` and `imul` each get a `jo`, and each
division the guards a literal operand cannot already answer: `n / 7` carries no
division check at all, `n / (0 - 1)` checks only for overflow. A check that
fires jumps to an out-of-line routine that writes to standard error and exits
non-zero:

```
$ ./overflow.exe
runtime error: arithmetic overflows an int
$ echo $?
1
```

The cost is one never-taken conditional branch per operation, which a modern
predictor makes free, and one instruction of code.

That routine is reached by `jmp`, not `call`, so `rsp` on arrival is whatever
the failing function happened to be using. Rather than oblige every function
that can fail to keep a frame a call could be made from — nearly all of them,
now that any addition can fail — the routine builds one out of thin air:

```nasm
tc$rt$abort:
    and  rsp, -16      ; force the alignment a call needs
    sub  rsp, 32       ; shadow space
    mov  rcx, 2
    call _write
```

It can afford to destroy `rsp` because it never returns. So a leaf full of
guarded arithmetic is still a leaf, and reserves nothing.

### Why not wrap

Wrapping is what C, Go and Java do, and `int` really is a 64-bit machine word,
so it would have been defensible. It is not what TinyC does, because it is the
one place the language would quietly hand back something other than what the
program asked for — and TinyC refuses that everywhere else: no implicit
conversion, no truth test on an integer, no variable without an initialiser. C's
syntax does not oblige anyone to C's semantics.

There is deliberately no escape hatch. If wrapping is ever wanted on purpose it
should be spelled out at the operator, not switched on for a whole program.

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
