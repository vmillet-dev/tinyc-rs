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
program := (enum_decl | class_decl | fn_decl)*
enum    := "enum" IDENT "{" IDENT ("," IDENT)* "}"
class   := "class" IDENT (":" IDENT)? "{" (field | fn_decl)* "}"
field   := type IDENT ";"
fn_decl := "fn" IDENT "(" params? ")" ("->" type)? block
params  := param ("," param)*
param   := type IDENT | "self"
type    := ("int" | "string" | "char" | "bool" | IDENT) ("[" INT? "]")?
stmt    := decl | assign | print | push | if | while | for | match | return
         | break | continue | call ";"
decl    := type IDENT "=" expr ";"
assign  := place "=" expr ";"
place   := IDENT (("[" expr "]") | ("." IDENT))*
print   := "print" "(" expr ")" ";"
push    := "push" "(" place "," expr ")" ";"
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
primary := atom postfix*
atom    := INT | STRING | CHAR | BOOL | IDENT | variant | call | match | array
         | object | len | convert | "(" expr ")"
postfix := "[" expr "]" | "." IDENT | "." IDENT "(" args? ")"
variant := IDENT "::" IDENT
array   := "[" (expr ("," expr)*)? "]"
object  := IDENT "{" (IDENT ":" expr ("," IDENT ":" expr)*)? "}"
len     := "len" "(" expr ")"
convert := ("int" | "char" | "string" | "bool") "(" expr ")"
call    := IDENT "(" (expr ("," expr)*)? ")"
match   := "match" "(" expr ")" "{" arm* "}"
arm     := IDENT "::" IDENT "=>" (expr ","? | block ","?)
```

`int` is a 64-bit signed integer, `string` is a run of characters, `char` is one
Unicode character, `bool` is `true` or `false`, an `enum` declares a type of its
own with a fixed set of values, a `class` declares one with fields and methods,
`int[3]` is a fixed-length array of them and `int[]` a list whose length the
program decides. Arithmetic is `int`-only, `+` also
joins two strings, `&&`, `||` and `!` are `bool`-only, `//` starts a comment, and
a variable keeps the type it was declared with — assigning a `string` to an
`int` is an error.

Three functions come with the language rather than being declared —
`read_line() -> string`, `eof() -> bool` and `is_int(string) -> bool`, see
[Reading input](#reading-input) — and a program cannot take their names.

There is no implicit truth test either, so `!n` on an `int` is a type error
rather than a comparison against zero. `%` takes its sign from the dividend, as
in C: `-7 % 2` is `-1`. Arithmetic that has no answer stops the program rather
than wrapping — see
[Arithmetic never answers wrongly](#arithmetic-never-answers-wrongly).

### Functions

A program is a list of enums, classes and functions, with no top-level
statements; execution starts at `main`,
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

### Classes

A `class` bundles fields with the methods that work on them, and a subclass may
stand for its base ([`examples/classes.tc`](examples/classes.tc)):

```c
class Shape {
  fn area(self) -> int { return 0; }
}

class Circle : Shape {
  int r;
  fn area(self) -> int { return 3 * self.r * self.r; }
}

fn report(Shape s) {
  print(s.area());     // 75 when handed a Circle
}
```

**The layout is the whole design.** An object is a vtable pointer at offset 0,
then the base's fields, then the subclass's own:

```
Circle:  [ vtable ][ (Shape has none) ][ r ]
           0         8                   8
```

A subclass's fields are its base's plus more, at the same offsets — so a
`Circle` **is** a `Shape` at the same address, and the upcast in `report(c)`
costs not one instruction. The other direction is a type error: no `Shape` is
known to be a `Circle`.

A field access is one `lea` with nothing to check: the same arithmetic an array
index does, minus the bounds check — a field's place was settled at compile time
and cannot be out of range. The offset stopped being the field's *position* the
moment a field could be an array or another object; it is the sum of the sizes
in front of it. See [Composition](#composition).

#### Dispatch decided by the object, and by the compiler when it can

`s.area()` reads the object's table and calls through it:

```nasm
mov  r10, rbx        ; the receiver
mov  r10, [r10]      ; its vtable
mov  rcx, rbx        ; ... which is also argument zero
call [r10+0]
```

A subclass's table is its base's with the overridden slots replaced, so slot 0
means `area` for every `Shape` there will ever be. Nothing installs it at
startup: the entries are known at compile time.

But **a class nothing derives from is called directly.** TinyC compiles the
whole program at once, so "nothing derives from `Point`" is a fact rather than a
hope, and the indirection would be deciding a question with one answer. A
separately compiled language cannot do this: someone might extend the class in
another translation unit.

#### Value semantics, and the size a hierarchy reserves

Every object of a hierarchy is given room for the **biggest class in it** —
`storage` in `ClassInfo`, alongside its own `size`, and the same number for
every class in the hierarchy. Whole-program compilation is again what makes it
knowable: nothing can derive from a class after the fact.

That one decision is what lets a polymorphic value be a local, a return and an
element:

```c
Shape held = c;        // room for the biggest Shape; the Circle is copied in
c.r = 100;
print(held.area());    // 75, not 30000 — a copy, not an alias
```

The copy carries the vtable pointer, so the value keeps answering as a `Circle`.
**There is no slicing**, unlike C++'s value semantics — and no reference type,
no `null` and no lifetimes either, because nothing is shared in the first place.

Giving every class in a hierarchy the *same* number matters as much as the
number itself: a smaller one for `Circle` would mean copying `storage(Shape)`
bytes out of a `Circle`-sized object could read past the end of it.

An array of them scales by that width rather than by eight, which is the one
case where an element's address is not a single `lea` — x86 scales by 1, 2, 4
or 8 and nothing else, so an `imul` goes in front.

#### Returning one

An aggregate does not come back in a register. **The caller reserves the room
and passes its address in ahead of the written arguments**; the callee copies
into what the caller already owns, and returns nothing at all:

```
fn make() -> Shape:
entry0:
  0  %out = param 0            ; the caller's room
  ...
  4  copy 24 bytes to %out, from %t2
  5  return                    ; nothing is handed outward
```

So returning is as safe as passing, and for the same reason: no address ever
travels outward, so none can dangle. The cost is one of the four argument
registers, which `sema` accounts for — a function returning an aggregate takes
at most three parameters, and says so when it takes four.

#### Composition

A field may be an array or another object, and what a class holds it holds
**inside itself** ([`examples/composition.tc`](examples/composition.tc)):

```c
class Point {
  int x;
  int y;
}

class Segment {
  Point a;        // a whole `Point`, not the address of one
  Point b;
}
```

```
Segment:  [ vtable ][ a: [ vtable ][ x ][ y ] ][ b: [ vtable ][ x ][ y ] ]
            0         8                          32
```

Everything else follows from that one change:

* **A copy carries the whole tree.** `Segment t = s;` copies the bytes of both
  points, so writing through `s.a` afterwards is invisible through `t`. Nothing
  new had to be said for that — an object was always copied outright, and now
  there is simply more of it.
* **`s.a` is an address, not something read out of the object.** A `Point` does
  not fit in a register, so reaching into one is arithmetic on the outer
  object's address and nothing is loaded until a scalar is. It is the rule
  `xs[i]` already followed when the elements were objects.
* **A field may be any class in a hierarchy**, and it reserves the room the
  biggest of them needs — the same `storage` a local of that class would. So a
  `Holder { Shape held; }` may hold a `Circle`, and the vtable pointer travels
  with the copy.

**What an object contains is a tree, never a graph.** There is no reference type
to close a ring with, so a class that would contain itself — directly, through
another class, or through an array — is refused rather than represented:

```
error: `Node` cannot contain a `Node`
  |
6 |   Node next;
  |   ^^^^ a field lives inside the object, so its room is part of this one's
  = note: TinyC has no reference type, so what an object holds it holds outright
```

A linked list is simply not a shape this language describes; `int[]` is what
holds a quantity that grows.

**The order the classes are laid out in** is what makes any of this answerable,
and two rules decide it. A class's fields follow its base's, so a base is
measured first. And a field holding an object reserves that object's room, so
whatever it names is measured first too — which means the compiler walks the
containments before it measures anything, in hierarchies rather than in classes,
because every class in a hierarchy reserves the same amount. That last point is
why `class A { B b; }` with `class B : A {}` is the same error as
`class A { A a; }`.

**Containment multiplies, and the frame does not.** An object lives in the
frame, so a class holding a thousand rows of a thousand `int`s would be eight
megabytes of stack. It is a diagnostic rather than a crash in a program that
compiled:

```
error: `Grid` is too big
  |
8 | class Grid {
  |       ^^^^ 8396808 bytes, and at most 65536 are supported
  = note: an object lives in the frame, and containment multiplies; `int[]` is
    what holds a quantity the frame cannot
```

#### What objects still may not do

* **An object is complete or it does not exist.** Every field is named in the
  literal, inherited ones included; there is no default and no partial object,
  which is what removes the question `null` would have answered.
* **No printing and no comparing.** `print` takes one value, and comparing
  addresses would quietly answer a different question.
* **A field may not be a list.** Everything else nests — see
  [Composition](#composition) — because it lives inside the object and is
  copied along with it. A list's elements live in the arena, so a field would
  hold only their address and a copy would share them rather than copy them.
* **A class may not take an enum's name, or another class's.** A type name is
  resolved to one type and there is nowhere for a second to go, so the loser
  would be a declaration no program could ever name. The compiler says so
  instead, pointing at whichever of the two was written second.
* **Arrays stay invariant.** A `Circle[2]` is not a `Shape[2]`, because writing
  a `Rect` through the second would put one in the first. What *is* allowed is
  building a `Shape[2]` directly: an array literal takes the nearest common
  ancestor of its elements rather than letting the first one decide.

### Arrays

The **length is part of the type**, so `int[3]` and `int[4]` are different types
and every length is known at compile time
([`examples/arrays.tc`](examples/arrays.tc)):

```c
fn total(int[5] xs) -> int {
  int sum = 0;
  for (int i = 0; i < len(xs); i = i + 1) {
    sum = sum + xs[i];
  }
  return sum;
}
```

That is what pays for the safety. An index the compiler can work out is settled
before the program is built, and costs nothing at run time:

```
error: index `3` is out of bounds
3 |   print(xs[3]);
  |            ^ this array holds 3, so the last index is 2
  = note: an index the compiler can see is checked here rather than guarded at run time
```

Only an index it cannot see becomes a check in the emitted code — and a single
one, because the comparison is *unsigned*: a negative index read as unsigned is
enormous, so it fails the same test that catches one past the end.

```nasm
cmp  rdi, 3
jae  tc$rt$bounds
lea  r12, [rbx+rdi*8]
```

`len(xs)` is a fact about the type, so it folds to a literal: the loop above
compares against `5`, and nothing computes it.

**An array is passed by address and returned by copy.** A parameter borrows the
caller's own array — there is no pointer type, no globals and no nesting, so an
address a callee receives has nowhere to be kept, and cannot outlive what it
points at. Returning one is the mirror image: the caller reserves the room and
passes its address in, so the callee fills what already belongs to it. Nothing
ever travels outward, which is the whole safety story in one sentence. See
[Returning one](#returning-one), which arrays and objects share.

The rest of the rules fall out of the same reasoning: an array literal's length
is its element count and the declaration must agree with it; every element must
have the same type; arrays answer no comparison and no arithmetic; and `print`
takes one value, so it refuses an array.

### What arrays changed underneath

**The IR learned about memory.** Until now every value lived in a virtual
register or in a spill slot the backend owned, and nothing in `ir.rs` named a
*place*. Arrays add four instructions that do:

```
  0  %xs = frame 0          ; the address of room this function owns
  1  %t1 = elem %xs[0] of 3 ; base + index * 8, with the length for the check
  2  store %t1, 10
  8  %t5 = load %t4
```

Two of them cost nothing they look like they should. `elem` is a `lea`, because
`base + index * 8` is an x86 addressing mode rather than arithmetic — which is
also why it picks up none of the overflow guards `Bin` carries. And `frame` is a
`lea` off `rsp`: the room is reserved once in the prologue, above the spill
slots, so nothing between an array and `rsp` moves for the life of the call.

**A type can no longer name itself.** `Ty` stays `Copy` and compares as an
integer, so `Ty::Array` holds an *index* exactly as `Ty::Enum` does. Array types
are interned as the program writes them, so that two `int[3]`s written apart get
the same id — without which `Ty`'s equality would say two identical types
differ.

**A declaration may now begin with a length.** `Colour[3] cs = ...` and
`cs[3] = 1` agree for three tokens and part at the fourth, so `starts_declaration`
looks that far: a type's length is a literal, and what follows the `]` is a name
in a declaration and an `=` in an assignment.

### Strings and characters

A `string` is a run of **characters**, not of bytes
([`examples/strings.tc`](examples/strings.tc)):

```c
string a = "héllo";
print(len(a));                 // 5 — five characters, six bytes to write
print(a[1]);                   // é
print(a + " wörld");           // héllo wörld
print(a == "héllo");           // true — the contents, not the address
print("count = " + string(5)); // count = 5
```

`len` counts characters, `s[i]` is one character, and a letter written with an
accent counts once however many bytes it took to type. UTF-8 lives only at the
edges: the lexer decodes the source, `print` encodes what it writes, and nothing
in between ever sees it. What TinyC counts is Unicode *scalar values*, not
grapheme clusters — `"é"` typed at a keyboard is one, the same letter written as
`e` plus a combining accent is two. Normalising would take Unicode tables the
compiler does not carry, so it says so rather than promising more.

A `char` is one of those characters, and it is a type of its own rather than a
small integer. It compares, including for order, because the order of code
points is a fact about the encoding — `'0' <= c && c <= '9'` is the question it
exists to answer. Two *strings* are not ordered: where `é` sorts is a question
about a language, not about characters, so `<` on strings is refused rather than
quietly answered wrongly.

There is no arithmetic on a character and no implicit widening. The way across
is written out, and these four conversions are the whole list:

| Written | Gives | Fails when |
|---|---|---|
| `int(c)` | a character's code point | never |
| `int(s)` | the number a string spells | the text is not a number an `int` can hold — ask `is_int(s)` first |
| `char(n)` | the character with that code point | `n` names none — checked at compile time when it is a constant, at run time otherwise |
| `string(c)` | a string of that one character | never |
| `string(n)` | a number written out in decimal | never |
| `string(cs)` | the characters of a `char[]`, sealed into a string | never |

A string is **read-only**: `s[i] = c` is a compile error, and `+` produces a
third string rather than changing either operand. That is what makes sharing one
free — two variables may well hold the same characters in memory, and since
neither can change them, the alias cannot be observed. "Assignment copies, never
aliases" therefore stays true everywhere it can be seen, while assigning a
string moves one pointer.

### What strings changed underneath

**TinyC gained a heap, and it never frees.** Until now every value lived in a
register or in a frame, and the rule was that *no address ever travels outward*
— a callee borrows the caller's array, a returned object fills room the caller
reserved. That rule is what made dangling impossible without lifetimes.

A string cannot obey it. `a + b` needs characters nobody wrote at compile time,
and `fn greet(string) -> string` cannot fill room its caller reserved, because
the caller has no way to know how much to reserve. So a built string is cut from
an **arena**: a bump pointer through chunks asked of `malloc`, where nothing is
ever given back.

That is the amendment, and it is deliberate. An arena address *does* travel
outward — but memory that is never freed cannot dangle, so the question the old
rule existed to answer does not arise. No lifetimes, no reference counts, no
collector. What it costs is memory a long-running program stops using and never
reclaims; TinyC programs run and finish, so the trade is a good one, and it is
the whole reason a string can be a value here at all.

**The sharp edge that buys: building a string in a loop.**

```c
string s = "";
for (int i = 0; i < 4000; i = i + 1) {
  s = s + "0123456789";   // 313 MB peak, for a 160 KB answer
}
```

Each turn allocates the whole of the new string and abandons the whole of the
old one, so both the copying and the *memory* grow with the square of the loop
count. Java, C# and Python have the same quadratic copying on `s += x`; what is
different here is that their collectors reclaim the intermediates and this arena
does not, so the trap that costs time elsewhere costs memory here.

It cannot be fixed inside `+`, and the reason is worth knowing: the length lives
*with* the characters. `t = s` copies one pointer, so `t` and `s` share the
count at `[p-8]`, and appending in place would bump a count `t` can see —
breaking exactly the immutability that makes sharing safe in the first place.
Fixing it needs either a two-word string value, which would stop a string
fitting in a register, or knowing that nothing else points at `s`, which needs
ownership. Both undo more than they buy.

What fixes it is a [list](#lists) of characters: `push` doubles the capacity, so
n characters cost O(n) work and O(n) garbage instead of O(n²) of each. Building
400 000 characters both ways, measured:

| | peak memory | time |
|---|---|---|
| `s = s + x` in a loop | 2.3 GB, then `runtime error: out of memory` | did not finish |
| `push` onto a `char[]`, then `string(cs)` | 11.9 MB | 76 ms |

Accumulating characters and reading an unknown quantity of input are the same
problem, and one answer serves both.

**A string is one pointer, with its length in front of it.**

```
[ character count : 8 bytes ][ characters, 4 bytes each ]
                             ^ this is the value
```

Four bytes per character, so `s[i]` is an address computation and not a walk
from the start — the trade UTF-8 makes the other way round. The count sits
*behind* the address, which is the only place in the compiler that reads
backwards from a pointer it was given, and it buys three things at once: a
string still travels in a register, still fits in an array slot, and still knows
its own length. `len(s)` is one `mov` from `[p-8]`.

**A literal is laid out identically.** `.data` holds the same count and the same
four-byte characters as the arena produces, so no instruction anywhere asks
which kind of string it is holding. There is one representation, not two.

**Only the loops became calls.** `len` is a load, `s[i]` is a bounds check and a
`lea`, and a character read out of a string is a 32-bit `mov` that widens as it
lands — the one value in the language narrower than a machine word. What needs
to walk the characters becomes a call to `tc$rt$…`: joining, comparing, encoding
for output, and writing a number out. The backend emits each only when the
program reaches it, so a program that touches no string never links `malloc`.

**Every index into a string is checked.** An array's length is part of its type,
so a constant index is settled at compile time and costs nothing. A string's
length is not knowable until it exists, so even `s[1]` is guarded — the same
single unsigned comparison, now against a loaded length rather than a literal.

**Printing goes through the encoder.** A string's characters are encoded into a
UTF-8 buffer — itself cut from the arena, and kept between calls so a loop of
prints allocates once — and handed to the same `printf` everything else uses, so
nothing interleaves. The entry point of a program that writes text also calls
`SetConsoleOutputCP(65001)` first: without it a Windows console renders `é` as
mojibake, and the language's promise about characters would stop at the terminal.

### Lists

An array's length is part of its type. A **list**'s is not
([`examples/lists.tc`](examples/lists.tc)):

```c
int[] xs = [];
for (int i = 1; i <= 3; i = i + 1) {
  push(xs, i * i);
}
print(len(xs));    // 3
print(xs[2]);      // 9
```

`int[3]` and `int[]` are different types, and neither replaces the other: use
the array when the count is a fact about the *program*, the list when it is a
fact about the *data*. What you give up is the compile-time index check — a
list's length is not knowable until it exists, so every index costs the same
single unsigned comparison a computed one costs on an array. What you get is the
only thing an array cannot do: hold a quantity nobody knew in advance, and be
returned from the function that built it.

**`push` takes a place, not a value**, and that is the whole design in one
detail. Growing a list may *move* it — the elements are copied into a larger
block — so whoever names the list has to be told where it went. A variable can
be told; an expression cannot. For the same reason `push` onto a **parameter**
is a compile error:

```
error: cannot push onto `xs`, which is a parameter
  |
2 |   push(xs, 1);
  |        ^^ a parameter is the caller's list, and growing it may move it
```

Left alone that would be the worst kind of bug. The length lives *with* the
elements, so a push that happened to fit would be visible to the caller and a
push that had to move would silently not be. Writing an *element* through a
parameter is fine, and visible, exactly as it is for an array — nothing moves.

**Assigning a list copies its elements.** A list is the one thing in the
language that can be written to *and* travels as an address, so without the copy
two names for one list would be observable the moment either was written
through. Every other type either cannot be written to (a string) or is copied
outright (an array, an object). This is what keeps "assignment copies, never
aliases" true without exception.

**A list may hold objects**, and it holds them the way an array does: whole,
inside the list itself ([`examples/lists.tc`](examples/lists.tc)).

```c
class Reading {
  string place;
  int degrees;
}

Reading[] readings = [];
push(readings, Reading { place: "Oslo", degrees: -3 });
readings[0].degrees = 40;      // an element *is* the object
```

That is what pairs the two halves of the language: a class says what a record
is, and a list holds as many of them as the data turns out to have. It needed
no rule of its own, because the rules a list already had cover it — growing
copies whole elements rather than handles, so there is nothing for two lists to
share, and assigning still copies. An element of a list of a *base* class is
the hierarchy's size, exactly as an array's is, so a `Shape[]` may hold a
`Circle` and the call through it still reaches `Circle`'s `area`.

The one thing a list cannot hold is another list, and it cannot be *written*
either — a type carries at most one pair of brackets. The mirror of that rule
is that a field of a class cannot be a list: an object is copied outright, so
the copy would share the elements rather than copy them. Everything that is not
a list does nest in an object — see [Composition](#composition) — precisely
because it lives *inside* the object rather than in the arena.

### What lists changed underneath

**A list is laid out like a string, on purpose.**

```
[ capacity : 8 ][ length : 8 ][ elements, as wide as one is ]
                              ^ this is the value
```

The length sits at `[p-8]` for both, so `len` is one instruction that never asks
which of the two it was handed. `Instr::Count` is that instruction, and it is
the only place in the compiler that reads *behind* an address it was given —
which is exactly what lets a list stay one pointer, travel in a register, and
still know how long it is.

**Both counts are in elements, and how wide one is comes in as an argument.**
That is the whole of what holding objects took: the routines walk the elements
rather than reading one, so they cannot work the width out for themselves, and
the compiler — which knows it from the type — tells them. Every width is a
multiple of eight, so every copy in them moves words rather than bytes.

**Four routines, and all of them are loops.** `list_new` cuts a block from the
arena, `list_room` makes room for one more and doubles when full, `list_clone`
is what an assignment costs. The fourth is the *second* push: `list_push` puts a
register in the room `list_room` made, and `list_push_big` copies into it from
an address, because an element too big for a register cannot arrive in one. The
compiler picks between them from the element's type rather than the runtime
guessing from a width — an object of one word is exactly as wide as an `int`,
and only the type says which it is. Everything else is inline: indexing is a
bounds check and a `lea`, `len` is a load. The rule is the same one the strings
follow — a call only where there is a loop.

**The arena is what makes `push(xs, xs[0])` mean what it says.** The push may
move the list, and the element it is copying *from* then lives in the block that
was left behind — which is still there, because nothing here is ever reclaimed.
The same property that removed lifetimes from the language removes a special
case from the runtime.

**Growing abandons the old block**, like everything else on this arena. Doubling
is what keeps that honest: n pushes copy 2n elements in total and leave n behind,
so both stay linear.

**A literal now takes its shape from the type it is given to.** `[1, 2, 3]` is an
`int[3]` on its own and an `int[]` where a list is wanted, and nothing in the
literal could say which — so `sema` checks a value against the type it is going
to at the four places one can be handed over: a declaration, an assignment, a
return, and an argument. It is the only expression in the language whose type
depends on its context, and `[]` — a list with nothing in it — is the case that
makes it necessary rather than merely convenient.

**A name now knows whether it is a parameter.** The scope holds a `Binding`
rather than a bare type, for one question asked in one place: `push` onto a
parameter is refused. That is the smallest amount of ownership tracking that
makes the arena safe to grow things in.

### Reading input

Three functions the compiler provides, and the first programs that do not know
what they will be given ([`examples/interactive.tc`](examples/interactive.tc)):

```c
while (!eof()) {
  string line = read_line();
  print(line + " (" + string(len(line)) + " characters)");
}
```

`read_line()` answers one line without its ending — `\n` or `\r\n`, either way —
and `eof()` says whether anything is left. **`eof()` consumes nothing**, which is
what makes the loop above read every line and ask for none that is not there.

**Asking for a line when there is none stops the program**, rather than
answering `""`. That is the whole reason `eof()` exists: an empty line and the
end of the input would otherwise be the same value, and nothing afterwards could
tell them apart. It is the same bargain as an index out of range — there is a
way to ask first, so failing to is a mistake worth naming.

Everything arriving is UTF-8; everything TinyC holds is characters. Bytes that
spell no character stop the program too:

```
runtime error: the input is not valid UTF-8
```

`int(s)` is the way back from text to a number, and it guesses nothing: an
optional `-`, then digits, and nothing else — no leading spaces, no trailing
units, and no settling for zero when the text is not a number at all. A number
too large for an `int` is refused for the same reason arithmetic that overflows
is.

**`is_int(s)` is how you ask first**, and it is there for exactly the reason
`eof()` is. Stopping the program is the right answer to a mistake *in the
program* — an index out of range, an overflow. Text that spells no number is
not that: it is the data, and a program that reads what it was given has to be
able to handle it.

```c
string line = read_line();
if (is_int(line)) {
  print(int(line) * 2);
} else {
  print("that is not a number");
}
```

Nothing a program could write itself would do. A loop over the characters gets
most of the way, and then cannot answer the last question: whether nineteen
digits still *fit* in an `int` is decided by an overflow, and performing the
overflow is what stops the program. So the language answers it — and the two
are **one routine asked two ways** (`tc$rt$parse_int`, which `int(s)` calls and
aborts on and `is_int(s)` calls and reports), so they cannot drift apart about
what a number is.

`char(n)` needs no equivalent, and the difference is worth naming: a program
*can* ask that one for itself, with the comparisons `int` already has. `int(s)`
was the only conversion whose question could not be asked in the language.

### What input changed underneath

**The first functions the compiler provides.** `len` and `push` are *constructs*
because no signature could describe them: one takes several unrelated types, the
other takes a place rather than a value. `read_line`, `eof` and `is_int` have
signatures a TinyC program could have written itself, so they are not syntax at
all — they are names already in the signature table when the first line is
checked, reached through the ordinary call machinery, and differing from a
declared function only in having no body to compile. `is_int` is the first that
takes something, and it needed no new machinery for that: its argument is
checked by whatever checks every other call's. A program that tries to declare
one of their names collides with something already there:

```
error: `eof` is built in and cannot be redefined
```

The whole cost is one row per built-in and one arm in lowering, where the call
becomes a `tc$rt$…` routine instead of a `FuncId`.

**The compiler buffers the input itself**, with `_read` and 4 KB of `.bss`,
rather than going through a `FILE*`. That is what makes `eof()` answerable
without pushing a character back: "has the input run out" becomes a question
about that buffer, and it costs nothing at all while the buffer is not empty.

**`read_line` is built out of the two features before it.** Characters
accumulate in a list, which grows by doubling, and `string(cs)` seals them —
so a line of any length costs one pass and no quadratic anything. It is the
clearest argument for having built the lists first.

**And the console is told twice.** `SetConsoleOutputCP(65001)` was already there
for printing; `SetConsoleCP(65001)` is its counterpart, so that what is *typed*
arrives as UTF-8 too. Without it a console hands over its own code page, and the
decoder would rightly refuse it.

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
 --> examples/errors/type_mismatch.tc:4:9
  |
4 |   print(x + s);
  |         ^ expected string, found int
  = note: `+` joins two strings; `string(n)` makes one out of a number, and `string(c)` out of a character
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

What is counted is the depth of the **tree**, not the depth the parser happens
to recurse to, and the two part company wherever a loop builds a left-leaning
chain. `1 + 1 + 1` reads as flat and is a tree three deep; so is a chain of
`else if`, each of which nests inside the previous `else`. Both were a stack
overflow rather than a diagnostic while only the parser's own recursion was
counted, because every later pass still walks the tree the loop built.

Width is not depth and is not limited: an array literal's elements are siblings,
so a thousand of them cost one level, not a thousand.

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

An index out of range is the fourth thing, and follows the same pattern for the
same reason: reaching past the end is something the machine would do without
complaint, so the compiler settles what it can and guards the rest. It is
described under [Arrays](#arrays), and applies to
[strings](#strings-and-characters) too.

Six more joined the family with strings, lists and input, and none of them is a
special case: `char(n)` for an `n` that names no character — settled at compile
time when it is a constant — an arena that cannot get memory from the operating
system, `int(s)` on text that is not a number, a line asked for when there is
none, input that is not valid UTF-8, and input the operating system refuses to
hand over. **Ten ways to stop, one routine to report them, and not one of them
answers wrongly instead.**

Two of those ten are about the *data* rather than about the program, and each
has a question that avoids it: `eof()` before `read_line()`, and `is_int(s)`
before `int(s)`. Stopping is the right answer to a mistake in the program; it
is not an answer to input a program was always going to be given.

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

**Nothing in the test suite needs to change either.** `tests/targets.rs` walks
`Target::names()`, so step 2 alone puts the new backend under every contract
every other target already keeps; `tests/cli.rs` will ask for it by name; and
`tests/execution.rs` will assemble, link and *run* its output as soon as
`tests/harness/elf.rs` reports a target that resolves. Adding a target is one
job, not two — see [The execution harness](#the-execution-harness).

## Tests

```bash
cargo test
```

Unit tests live beside each stage — including the ones about the *text* a
backend emits, which belong to that backend and no other: a prologue, a symbol
prefix and a mnemonic are facts about `x64_win`, so they are checked in
`x64_win`. Five integration suites sit on top:

* `tests/error_positions.rs` asserts the exact line and column reported for
  every program in `examples/errors/`. Only about where the caret lands.
* `tests/targets.rs` holds every backend to the same contract, walking
  `Target::names()` rather than naming one: every example compiles, every
  allocation verifies, every register file holds together, the parameter limit
  is the target's own, the front end builds the same tree for all of them, and
  every error example is refused by all of them. Nothing in it names a target,
  so a new backend arrives already covered.
* `tests/cli.rs` drives the built binary: where the assembly is written, what
  `--emit` prints and does not write, what an unknown target says, and that a
  program refused at any stage exits non-zero with a rendered diagnostic on
  stderr. It needs no assembler.
* `tests/pipeline.rs` covers what belongs to the pipeline as a whole rather than
  to any one stage: that every shape of nesting is refused *in words* past the
  limit and survives every stage below it, and that `--emit` really stops the
  run where it says it does.
* `tests/execution.rs` **runs the compiled programs.** Every other test in the
  repository inspects text, and text cannot tell a `setl` from a `setg` or
  notice a register clobbered between two instructions that each look right on
  their own. Each example is assembled, linked and run, and its output compared
  against `examples/expected/`; further tests do the same for the corners of
  code generation that are easiest to get subtly wrong — a destination that
  aliases its own operand, an immediate too wide for an instruction, enough live
  values to force spills — and for what a string does, where each case answers a
  *number* wherever it can, so that a mangled character shows up as a wrong
  count rather than as output that merely looks odd.

### The execution harness

`tests/execution.rs` names no platform. It is a set of tables — a small TinyC
program and what it must print — and everything that differs per machine lives
behind one trait in `tests/harness/`:

```text
tests/harness/mod.rs   the tables' runners, and `host_toolchain()`
tests/harness/msvc.rs  nasm -f win64, then link.exe   (#[cfg(windows)])
tests/harness/elf.rs   nasm -f elf64, then cc         (#[cfg(unix)])
```

`Toolchain` is one method — assembly text in, an executable out — plus the
`Target` to ask the compiler for. `host_toolchain()` picks one, and is the only
`cfg` in the whole harness. So adding the Linux backend means giving `elf.rs`
a target that resolves; every case table comes along unchanged.

Both failure modes are ordinary answers rather than panics: a machine may have
no assembler, and the compiler may have no backend for a machine that does. In
either case the suite says what was missing and passes, so `cargo test` still
works without a toolchain:

```bash
cargo test --test execution -- --nocapture
```

`link.exe` only knows where the C runtime is if `vcvars64.bat` has told it, so
the suite runs that once, keeps the `LIB` it sets, and calls the linker directly
from then on.
