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
| lowering | [`src/ir.rs`](src/ir.rs) | three-address code over virtual registers |
| register allocation | [`src/codegen/regalloc.rs`](src/codegen/regalloc.rs) | a machine register or stack slot per value |
| emission | [`src/codegen/x64_win.rs`](src/codegen/x64_win.rs) | MASM assembly |

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

`tinyc` emits assembly; `scripts/build.ps1` takes it the rest of the way using
the Microsoft assembler and linker from a Visual Studio installation
(`ml64` + `link`, with the "Desktop development with C++" workload installed):

```powershell
.\scripts\build.ps1 examples\hello.tc
```

By hand, from a *x64 Native Tools Command Prompt*:

```
ml64 /c /Fo out\hello.obj out\hello.asm
link /subsystem:console /entry:mainCRTStartup /out:out\hello.exe out\hello.obj msvcrt.lib legacy_stdio_definitions.lib
```

`print` is compiled into a call to the C runtime's `printf`, which is why the
CRT is linked in. `legacy_stdio_definitions.lib` provides `printf` as a real
symbol, since the UCRT headers normally supply it as an inline function.

## The language

```
program := stmt*
stmt    := decl | assign | print
decl    := ("int" | "string") IDENT "=" expr ";"
assign  := IDENT "=" expr ";"
print   := "print" "(" expr ")" ";"
expr    := term (("+" | "-") term)*
term    := unary (("*" | "/") unary)*
unary   := "-" unary | primary
primary := INT | STRING | IDENT | "(" expr ")"
```

`int` is a 64-bit signed integer, `string` is a pointer to static bytes.
Arithmetic is `int`-only, `//` starts a comment, and a variable keeps the type
it was declared with — assigning a `string` to an `int` is an error. There are
deliberately no functions, loops or conditionals yet.

### Reassignment

A variable can be given a new value ([`examples/reassign.tc`](examples/reassign.tc)):

```c
int n = 1;
n = n + 41;
print(n);      // 42
```

In the IR this becomes *renaming* rather than mutation — each assignment
introduces a fresh virtual register and the variable starts pointing at it:

```
  0  %n = const 1
  1  %n.1 = add %n, 41
  2  print int %n.1
```

Every virtual register therefore still has exactly one definition, which is what
keeps live intervals exact and lets the allocator stay as simple as it is. With
no control flow in the language, that is all the SSA construction the IR needs.
It also means `%n` and `%n.1` are independent values, so the allocator is free
to put the new one wherever the old one just died.

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
location for every value.

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
