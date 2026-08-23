//! Compile each example the whole way and check what it *prints*.
//!
//! Every other test in this repository inspects text: tokens, a tree, an IR
//! dump, a line of assembly. None of them can tell a `setl` from a `setg`, or
//! notice a register clobbered between two instructions that both look right on
//! their own. Running the program is the only check that can, so this is where a
//! miscompilation gets caught.
//!
//! Nothing in this file names a platform. Every test is a table of small TinyC
//! programs and what each must print, run through the [`harness`], which owns
//! the one part that differs per machine — see `tests/harness/mod.rs`. A new
//! backend inherits all of it unchanged.
//!
//! It needs an assembler and a linker. When either is missing the tests report
//! what they could not find and pass, so `cargo test` still works on a machine
//! without a toolchain — run them deliberately with:
//!
//! ```text
//! cargo test --test execution -- --nocapture
//! ```
//!
//! and read the "skipped" lines.

mod harness;

use harness::{EXAMPLES, Harness, examples, normalise};


#[test]
fn every_example_prints_what_it_promises() {
    let Some(harness) = Harness::find() else { return };

    for file in EXAMPLES {
        let source = std::fs::read_to_string(examples().join(file))
            .unwrap_or_else(|e| panic!("{file}: {e}"));
        let expected = examples().join("expected").join(file.replace(".tc", ".txt"));
        let expected = std::fs::read_to_string(&expected)
            .unwrap_or_else(|e| panic!("{}: {e}", expected.display()));

        // An example that reads is fed the `.in` beside its expected output;
        // one that does not gets nothing, and so sees the end of the input at
        // once — which is itself the state `eof()` has to get right.
        let input = examples().join("expected").join(file.replace(".tc", ".in"));
        let input = std::fs::read(&input).unwrap_or_default();

        let run = harness.build_and_run(file, &source, &input);
        assert!(run.status.success(), "{file} exited with {}\n{}", run.status, run.stderr);
        assert_eq!(normalise(&run.stdout), normalise(&expected), "{file} printed the wrong thing");
    }
}

/// The cases the backend is most likely to get subtly wrong, each written so
/// that a wrong answer is a wrong *number* rather than a crash.
#[test]
fn the_awkward_corners_of_code_generation_still_compute() {
    let Some(harness) = Harness::find() else { return };

    let cases: [(&str, &str); 15] = [
        // The destination shares a register with the right operand of an
        // operator that does not commute, so the operand has to be read before
        // the destination is written.
        ("int y = 3;\nint x = 10;\nx = y - x;\nprint(x);", "-7"),
        ("int y = 3;\nint x = 100;\nx = x / y;\nprint(x);", "33"),
        ("int y = 3;\nint x = 10;\nx = x - y;\nprint(x);", "7"),
        // Commutative operators swap instead, which must not change the answer.
        ("int y = 3;\nint x = 10;\nx = y + x;\nprint(x);", "13"),
        // An immediate wider than the 32 bits an ALU operand allows.
        ("int a = 1;\nprint(a + 4611686018427387904);", "4611686018427387905"),
        // Signed division rounds towards zero, and the sign has to survive
        // `cqo`.
        ("int a = 0 - 100;\nint b = 7;\nprint(a / b);", "-14"),
        // Enough live values at once to force spills, all read back afterwards.
        (
            "int a = 1; int b = 2; int c = 3; int d = 4; int e = 5;\n\
             int f = 6; int g = 7; int h = 8; int i = 9; int j = 10;\n\
             print(a - b - c - d - e - f - g - h - i - j);",
            "-53",
        ),
        // A comparison used as a value rather than as a branch.
        ("int n = 5;\nbool big = n > 3;\nprint(big);", "true"),
        // The guards around a division must not change a division that works.
        ("int a = 7;\nint b = 0 - 1;\nprint(a / b);", "-7"),
        // A loop whose counter is compared against a literal, which is the
        // shape the branch fusion rewrites.
        (
            "int total = 0;\nfor (int k = 1; k <= 10; k = k + 1) {\n  total = total + k;\n}\n\
             print(total);",
            "55",
        ),
        // `%` reads `rdx` where `/` reads `rax`; swapping them silently would
        // still produce a plausible-looking number.
        ("int a = 17;\nint b = 5;\nprint(a % b);", "2"),
        ("int a = 17;\nint b = 5;\nprint(a / b);", "3"),
        // The remainder takes its sign from the dividend, which is what `cqo`
        // sign-extending into `rdx:rax` is for.
        ("int a = 0 - 17;\nint b = 5;\nprint(a % b);", "-2"),
        ("int a = 17;\nint b = 0 - 5;\nprint(a % b);", "2"),
        // The destination shares a register with an operand, as for division.
        ("int a = 17;\nint b = 5;\na = a % b;\nprint(a);", "2"),
    ];

    harness.each_prints("corner", &cases);
}

/// What a string does, checked by running it rather than by reading assembly.
///
/// Every case here answers a *number* or a `true`/`false` wherever it can, so
/// that a mangled character shows up as a wrong count rather than as output
/// that merely looks odd.
#[test]
fn strings_hold_characters_rather_than_bytes() {
    let Some(harness) = Harness::find() else { return };

    let cases: [(&str, &str); 17] = [
        // A length is a count of characters, whatever they cost to write.
        (r#"print(len("héllo"));"#, "5"),
        (r#"print(len("日本語"));"#, "3"),
        (r#"print(len(""));"#, "0"),
        // Joining, including the two empty cases the copy loops have to get
        // right by doing nothing at all.
        (r#"print("a" + "b" == "ab");"#, "true"),
        (r#"print("" + "x" == "x");"#, "true"),
        (r#"print("x" + "" == "x");"#, "true"),
        (r#"print(len("é" + "é"));"#, "2"),
        // Equality is about contents. Two literals are interned, so a program
        // that compared addresses would get the first of these right and the
        // second wrong.
        (r#"print("ab" == "ab");"#, "true"),
        (r#"string s = "a" + "b";
            print(s == "ab");"#, "true"),
        (r#"print("ab" == "abc");"#, "false"),
        (r#"print("abc" == "abd");"#, "false"),
        // Indexing lands on the character it promises, not on a byte of one.
        (r#"print(int("héllo"[1]));"#, "233"),
        (r#"print(int("日本語"[2]));"#, "35486"),
        // A round trip through both conversions, for a character that needs
        // three bytes of UTF-8 and would lose them if anything counted bytes.
        (r#"print(string(char(int('語'))) == "語");"#, "true"),
        // A number written out, including the one with no positive twin.
        (r#"print(string(0 - 9223372036854775807 - 1) == "-9223372036854775808");"#, "true"),
        // Enough joining to send the arena back to `malloc` for more chunks.
        // Deliberately modest: `s = s + x` abandons every intermediate, so the
        // memory this costs grows with the *square* of the loop count.
        (r#"string s = "";
            for (int i = 0; i < 300; i = i + 1) {
              s = s + "0123456789";
            }
            print(len(s));"#, "3000"),
        // Doubling, which asks for one block larger than a whole chunk — the
        // path where the arena gives up on its usual size and asks for what was
        // wanted instead. It also joins a string to *itself*, so the two source
        // pointers the copy loops walk are the same one.
        (r#"string s = "0123456789abcdef";
            for (int i = 0; i < 11; i = i + 1) {
              s = s + s;
            }
            print(len(s));"#, "32768"),
    ];

    harness.each_prints("string", &cases);
}

/// What a list does, checked by running it.
///
/// The cases that matter most are the ones where growing *moves* the elements:
/// a list that never outgrows its first block would hide every mistake in the
/// copy, so several of these deliberately push past it.
#[test]
fn lists_grow_and_stay_their_owners() {
    let Some(harness) = Harness::find() else { return };

    let cases: [(&str, &str); 12] = [
        (r#"int[] xs = [];
            print(len(xs));"#, "0"),
        (r#"int[] xs = [3, 1, 4];
            print(len(xs) * 100 + xs[2]);"#, "304"),
        // Past the first block, so the elements are copied at least twice.
        (r#"int[] xs = [];
            for (int i = 0; i < 100; i = i + 1) { push(xs, i); }
            print(len(xs) * 1000 + xs[99]);"#, "100099"),
        // Every element survives the moves, not just the last one.
        (r#"int[] xs = [];
            for (int i = 0; i < 100; i = i + 1) { push(xs, i * i); }
            int total = 0;
            for (int i = 0; i < len(xs); i = i + 1) { total = total + xs[i]; }
            print(total);"#, "328350"),
        // Assignment copies: growing one must not be visible through the other,
        // and neither must writing an element.
        (r#"int[] a = [1, 2];
            int[] b = a;
            push(b, 3);
            print(len(a) * 10 + len(b));"#, "23"),
        (r#"int[] a = [1, 2];
            int[] b = a;
            b[0] = 9;
            print(a[0] * 10 + b[0]);"#, "19"),
        // ... even when the copy is made from a list that has already moved.
        (r#"int[] a = [];
            for (int i = 0; i < 50; i = i + 1) { push(a, i); }
            int[] b = a;
            push(b, 999);
            print(len(a) * 1000 + b[50]);"#, "50999"),
        // A parameter borrows: writing an element is visible to the caller.
        (r#"int[] a = [1, 2];
            double_each(a);
            print(a[0] + a[1]);"#, "6"),
        // A returned list outlives the call that built it.
        (r#"int[] a = squares(30);
            print(len(a) * 10000 + a[29]);"#, "300900"),
        // Lists of the other things that fit in a register.
        (r#"string[] w = [];
            push(w, "a");
            push(w, "bé");
            print(len(w) * 10 + len(w[1]));"#, "22"),
        (r#"char[] cs = ['a', 'b'];
            push(cs, 'c');
            print(string(cs) == "abc");"#, "true"),
        // The reason lists come before `read_line`: building a string this way
        // is linear, where `s = s + x` in a loop is quadratic in both time and
        // memory. 20000 characters would cost gigabytes the other way.
        (r#"char[] cs = [];
            for (int i = 0; i < 20000; i = i + 1) { push(cs, '0'); }
            string s = string(cs);
            print(len(s));"#, "20000"),
    ];

    // Two of the cases call helpers, so they are compiled with them in scope.
    let prelude = "fn double_each(int[] xs) {\n\
                   \x20 for (int i = 0; i < len(xs); i = i + 1) { xs[i] = xs[i] * 2; }\n\
                   }\n\
                   fn squares(int n) -> int[] {\n\
                   \x20 int[] out = [];\n\
                   \x20 for (int i = 1; i <= n; i = i + 1) { push(out, i * i); }\n\
                   \x20 return out;\n\
                   }\n";
    harness.each_prints_after("list", prelude, &cases);
}

/// Reading input, which is the one thing that cannot be checked by looking at
/// the program alone: what it does depends on what it is given.
#[test]
fn reading_the_input_sees_characters_and_knows_when_to_stop() {
    let Some(harness) = Harness::find() else { return };

    // `(input, body, expected)`. The inputs are written as raw bytes on
    // purpose: what arrives is UTF-8, and turning it into characters is the
    // very thing under test.
    let cases: [(&[u8], &str, &str); 12] = [
        // Nothing at all: the end of the input is where the program starts.
        (b"", "print(eof());", "true"),
        (b"x\n", "print(eof());", "false"),
        // Asking does not consume, so asking twice answers twice.
        (b"x\n", "print(eof());\nprint(eof());\nprint(read_line());", "false\nfalse\nx"),
        // A line is what is between the endings, and both endings work.
        (b"one\ntwo\n", "print(read_line());\nprint(read_line());", "one\ntwo"),
        (b"one\r\ntwo\r\n", "print(len(read_line()));", "3"),
        // An empty line is a line, and is not the end.
        (b"\nx\n", "print(len(read_line()));\nprint(read_line());", "0\nx"),
        // A last line with no ending is still a line.
        (b"tail", "print(read_line());\nprint(eof());", "tail\ntrue"),
        // Characters, not bytes: five characters in six bytes.
        ("h\u{e9}llo\n".as_bytes(), "print(len(read_line()));", "5"),
        ("\u{65e5}\u{672c}\u{8a9e}\n".as_bytes(), "print(len(read_line()));", "3"),
        // A line longer than the buffer, so the refill happens mid-line.
        (
            &[b'a'; 5000],
            "string line = read_line();\nprint(len(line));",
            "5000",
        ),
        // Counting an unknown quantity, which is what all of this was for.
        (
            b"3\n1\n4\n1\n5\n",
            "int total = 0;\n\
             int[] seen = [];\n\
             while (!eof()) {\n  push(seen, int(read_line()));\n}\n\
             for (int i = 0; i < len(seen); i = i + 1) { total = total + seen[i]; }\n\
             print(len(seen) * 100 + total);",
            "514",
        ),
        // Text into a number and out again, unchanged — including the one with
        // no positive twin, which a parser that negates at the end would lose.
        (
            b"-9223372036854775808\n",
            "print(string(int(read_line())) == \"-9223372036854775808\");",
            "true",
        ),
    ];

    harness.each_prints_given("input", &cases);
}

/// Asking for a line that is not there stops the program, rather than answering
/// with something that could be mistaken for an empty line.
#[test]
fn reading_past_the_end_reports_and_exits() {
    let Some(harness) = Harness::find() else { return };

    let run = harness.build_and_run(
        "past_the_end",
        "fn main() {\n  print(read_line());\n  print(read_line());\n}\n",
        b"only\n",
    );
    assert!(!run.status.success(), "it should not have finished");
    assert!(run.stdout.contains("only"), "the line it did have: {}", run.stdout);
    assert!(run.stderr.contains("no more input"), "{}", run.stderr);
}

/// Bytes that spell no character stop the program too, rather than becoming
/// some replacement nobody asked for.
#[test]
fn input_that_is_not_utf8_reports_and_exits() {
    let Some(harness) = Harness::find() else { return };

    let run =
        harness.build_and_run("bad_utf8", "fn main() {\n  print(read_line());\n}\n", b"\xff\xfe\n");
    assert!(!run.status.success(), "it should not have finished");
    assert!(run.stderr.contains("not valid UTF-8"), "{}", run.stderr);
}

/// A division the hardware cannot perform must say so and stop, rather than
/// dying on a hardware exception with nothing printed.
#[test]
fn a_division_that_cannot_be_performed_reports_and_exits() {
    let Some(harness) = Harness::find() else { return };

    let cases = [
        ("fn zero() -> int {\n  return 0;\n}\nfn main() {\n  print(1 / zero());\n}", "by zero"),
        (
            "fn neg() -> int {\n  return 0 - 1;\n}\n\
             fn main() {\n  int m = 0 - 9223372036854775807 - 1;\n  print(m / neg());\n}",
            "overflows",
        ),
        // `%` is the same `idiv`, so it faults in both the same ways — the
        // second one even though `MIN % -1` is 0 on paper.
        ("fn zero() -> int {\n  return 0;\n}\nfn main() {\n  print(1 % zero());\n}", "by zero"),
        (
            "fn neg() -> int {\n  return 0 - 1;\n}\n\
             fn main() {\n  int m = 0 - 9223372036854775807 - 1;\n  print(m % neg());\n}",
            "overflows",
        ),
        // Overflow the compiler cannot see, because the value comes back from a
        // call. Each of the three guarded operators, plus unary minus, which is
        // a subtraction and inherits the guard.
        (
            "fn big() -> int {\n  return 9223372036854775807;\n}\n\
             fn main() {\n  print(big() + 1);\n}",
            "arithmetic overflows",
        ),
        (
            "fn small() -> int {\n  return 0 - 9223372036854775807 - 1;\n}\n\
             fn main() {\n  print(small() - 1);\n}",
            "arithmetic overflows",
        ),
        (
            "fn big() -> int {\n  return 9223372036854775807;\n}\n\
             fn main() {\n  print(big() * 2);\n}",
            "arithmetic overflows",
        ),
        (
            "fn small() -> int {\n  return 0 - 9223372036854775807 - 1;\n}\n\
             fn main() {\n  print(-small());\n}",
            "arithmetic overflows",
        ),
        // The case the abort routine's own frame exists for: `bump` makes no
        // call, so it is a leaf and reserves nothing. Whatever `rsp` it hands
        // over, the report has to be able to call `_write` from it.
        (
            "fn bump(int n) -> int {\n  return n + 9223372036854775807;\n}\n\
             fn main() {\n  print(bump(2));\n}",
            "arithmetic overflows",
        ),
    ];

    harness.each_stops_with("abort", &cases);
}

/// Short circuiting and loop jumps are the two features whose whole point is
/// what a program *does not* do, which no amount of reading assembly proves.
///
/// Each case is written so that getting it wrong is loud: an operand that
/// should never have been evaluated divides by zero and aborts, a `continue`
/// that skips a `for`'s step loops forever, and a `break` that leaves the wrong
/// loop prints the wrong number.
#[test]
fn short_circuits_and_loop_jumps_do_what_they_promise() {
    let Some(harness) = Harness::find() else { return };

    let cases: [(&str, &str); 15] = [
        // The right operand must not run: `10 / z` would abort the process.
        ("int z = 0;\nprint(z != 0 && 10 / z > 1);", "false"),
        ("int z = 0;\nprint(z == 0 || 10 / z > 1);", "true"),
        // ... but it must run when the left one settles nothing.
        ("int z = 2;\nprint(z != 0 && 10 / z > 1);", "true"),
        ("int z = 2;\nprint(z == 0 || 10 / z > 4);", "true"),
        // `&&` binds tighter than `||`, so this is `true || (false && false)`.
        ("print(true || false && false);", "true"),
        // A chain, evaluated left to right and stopping at the first `false`.
        ("int z = 0;\nprint(1 < 2 && z == 0 && 2 < 1 && 10 / z > 0);", "false"),
        // A `continue` in a `for` still runs the step, or this never ends.
        (
            "int total = 0;\nfor (int i = 1; i <= 10; i = i + 1) {\n  if (i == 5) {\n    \
             continue;\n  }\n  total = total + i;\n}\nprint(total);",
            "50",
        ),
        // The same in a `while`, where the increment is inside the body and a
        // `continue` therefore has to come *after* it.
        (
            "int i = 0;\nint total = 0;\nwhile (i < 10) {\n  i = i + 1;\n  if (i == 5) {\n    \
             continue;\n  }\n  total = total + i;\n}\nprint(total);",
            "50",
        ),
        // `break` leaves the innermost loop only: the outer one runs to the end.
        (
            "int hits = 0;\nfor (int a = 1; a <= 3; a = a + 1) {\n  \
             for (int b = 1; b <= 3; b = b + 1) {\n    if (b == 2) {\n      break;\n    }\n    \
             hits = hits + 1;\n  }\n}\nprint(hits);",
            "3",
        ),
        // A short circuit *as* a loop condition, so its join is what the back
        // edge returns through.
        (
            "int i = 0;\nwhile (i < 100 && i * i < 30) {\n  i = i + 1;\n}\nprint(i);",
            "6",
        ),
        // The two new shapes stacked: a `for` whose step is itself a short
        // circuit, reached through the step block a `continue` asked for. The
        // back edge has to leave the block the step *ended* in, which is the
        // join of the short circuit rather than the step block itself.
        (
            "bool ok = true;\nint i = 0;\nfor (i = 0; i < 4; ok = ok && i < 2) {\n  \
             if (i == 1) {\n    i = i + 1;\n    continue;\n  }\n  i = i + 1;\n}\nprint(ok);",
            "false",
        ),
        // `!(a < b)` is compiled as `a >= b`, so an off-by-one in the inversion
        // table would show up right at the boundary where the two differ.
        ("int a = 5;\nint b = 5;\nprint(!(a < b));", "true"),
        ("int a = 5;\nint b = 5;\nprint(!(a <= b));", "false"),
        // Negating something that is not a comparison goes through `== 0`.
        ("bool ok = 1 > 0;\nprint(!ok);", "false"),
        ("bool ok = 1 > 0;\nprint(!!ok);", "true"),
    ];

    harness.each_prints("logic", &cases);
}

/// Objects: the layout, the dispatch, and the upcast that makes them useful.
///
/// Getting a field offset or a vtable slot wrong produces a plausible number
/// rather than a crash, so running the thing is the only check that catches it.
#[test]
fn objects_dispatch_on_what_they_are() {
    let Some(harness) = Harness::find() else { return };

    let prelude = "class Shape {\n  fn area(self) -> int { return 0; }\n  \
                   fn name(self) -> string { return \"shape\"; }\n}\n\
                   class Circle : Shape {\n  int r;\n  \
                   fn area(self) -> int { return 3 * self.r * self.r; }\n}\n\
                   class Rect : Shape {\n  int w;\n  int h;\n  \
                   fn area(self) -> int { return self.w * self.h; }\n  \
                   fn name(self) -> string { return \"rect\"; }\n}\n\
                   fn report(Shape s) -> int {\n  return s.area();\n}\n\
                   fn label(Shape s) -> string {\n  return s.name();\n}\n";
    let cases: [(&str, &str); 8] = [
        // The request's own example.
        ("Circle c = Circle { r: 5 };\nprint(report(c));", "75"),
        // A second subclass through the same parameter, so the dispatch really
        // depends on the object and not on where it was written.
        ("Rect r = Rect { w: 4, h: 6 };\nprint(report(r));", "24"),
        // A method the subclass does *not* override still reaches the base's.
        ("Circle c = Circle { r: 1 };\nprint(label(c));", "shape"),
        ("Rect r = Rect { w: 1, h: 1 };\nprint(label(r));", "rect"),
        // Fields, read and written through the object.
        ("Rect r = Rect { w: 4, h: 6 };\nr.w = 10;\nprint(r.area());", "60"),
        // A field declared by the base and one by the subclass do not overlap.
        ("Rect r = Rect { h: 7, w: 3 };\nprint(r.w * 100 + r.h);", "307"),
        // Two objects at once, so their frame regions must stay apart.
        (
            "Circle a = Circle { r: 2 };\nRect b = Rect { w: 5, h: 5 };\n\
             a.r = 3;\nprint(a.area() + b.area());",
            "52",
        ),
        // A direct call on a sealed class: `Rect` has no subclasses, so this is
        // the devirtualised path rather than the vtable one.
        ("Rect r = Rect { w: 3, h: 3 };\nprint(r.area());", "9"),
    ];

    harness.each_prints_after("object", prelude, &cases);
}

/// The three things a polymorphic value could not be until aggregates gained
/// value semantics: a local, a return, and an element.
///
/// Each rests on the same fact — every object of a hierarchy is given the
/// hierarchy's room — so a copy carries the vtable pointer with it and nothing
/// is sliced. Getting that wrong shows up as the *base* class's answer, which
/// is a plausible number rather than a crash.
#[test]
fn a_polymorphic_value_keeps_what_it_is_when_it_is_copied() {
    let Some(harness) = Harness::find() else { return };

    let prelude = "class Shape {\n  fn area(self) -> int { return 0; }\n}\n\
                   class Circle : Shape {\n  int r;\n  \
                   fn area(self) -> int { return 3 * self.r * self.r; }\n}\n\
                   class Rect : Shape {\n  int w;\n  int h;\n  \
                   fn area(self) -> int { return self.w * self.h; }\n}\n\
                   fn pick(int n) -> Shape {\n  if (n == 0) {\n    return Circle { r: 5 };\n  }\n  \
                   return Rect { w: 4, h: 6 };\n}\n";
    let cases: [(&str, &str); 8] = [
        // A local of the base class, holding a subclass. A slice would answer 0.
        ("Shape s = Circle { r: 2 };\nprint(s.area());", "12"),
        ("Shape s = Rect { w: 3, h: 5 };\nprint(s.area());", "15"),
        // A copy, not an alias: changing the source must not change the copy.
        (
            "Circle c = Circle { r: 3 };\nShape t = c;\nc.r = 100;\nprint(t.area());",
            "27",
        ),
        // The same for assignment rather than declaration.
        (
            "Shape s = Circle { r: 1 };\nCircle c = Circle { r: 4 };\ns = c;\nc.r = 100;\n\
             print(s.area());",
            "48",
        ),
        // A returned object, both branches, through the caller's own room.
        ("print(pick(0).area());", "75"),
        ("print(pick(1).area());", "24"),
        // A returned object outliving the call that made it, which is the whole
        // point of copying into the caller's room.
        ("Shape s = pick(0);\nprint(s.area());", "75"),
        // A heterogeneous collection: three objects of different sizes, each in
        // a slot the size of the biggest.
        (
            "Shape[3] all = [Circle { r: 1 }, Rect { w: 2, h: 3 }, Circle { r: 2 }];\n\
             int total = 0;\n\
             for (int i = 0; i < len(all); i = i + 1) {\n  total = total + all[i].area();\n}\n\
             print(total);",
            "21",
        ),
    ];

    harness.each_prints_after("poly", prelude, &cases);
}

/// Arrays really read and write the memory they claim to.
///
/// The only feature whose values live outside registers, so this is the one
/// place a wrong offset or a clobbered base register shows up — as a wrong
/// number rather than as anything the assembly looks guilty about.
#[test]
fn arrays_address_the_elements_they_promise() {
    let Some(harness) = Harness::find() else { return };

    let prelude = "fn fill(int[4] xs) {\n  for (int i = 0; i < len(xs); i = i + 1) {\n    \
                   xs[i] = i * 10;\n  }\n}\n";
    let cases: [(&str, &str); 9] = [
        // Every element in turn, so an off-by-one in the offset is caught.
        ("int[4] xs = [7, 8, 9, 10];\nprint(xs[0]);", "7"),
        ("int[4] xs = [7, 8, 9, 10];\nprint(xs[3]);", "10"),
        // The same through an index the compiler cannot fold.
        ("int[4] xs = [7, 8, 9, 10];\nint i = 3;\nprint(xs[i]);", "10"),
        // Writing, then reading back through a different index expression.
        ("int[4] xs = [0, 0, 0, 0];\nxs[2] = 42;\nint i = 2;\nprint(xs[i]);", "42"),
        // A callee writing through the caller's array.
        (
            "int[4] xs = [0, 0, 0, 0];\nfill(xs);\nprint(xs[3]);",
            "30",
        ),
        // Two arrays at once, so their frame regions must not overlap.
        (
            "int[2] a = [1, 2];\nint[2] b = [3, 4];\na[0] = 99;\nprint(b[0] + a[0]);",
            "102",
        ),
        // An array live across a call: the base register has to survive it.
        (
            "int[2] a = [5, 6];\nfill([0, 0, 0, 0]);\nprint(a[1]);",
            "6",
        ),
        // Enough other values to force spills, with the array still readable.
        (
            "int[2] xs = [1, 2];\nint a = 1; int b = 2; int c = 3; int d = 4; int e = 5;\n\
             int f = 6; int g = 7; int h = 8; int i = 9; int j = 10;\n\
             print(xs[1] + a + b + c + d + e + f + g + h + i + j);",
            "57",
        ),
        // Strings and bools live in arrays too.
        ("string[2] w = [\"no\", \"yes\"];\nprint(w[1]);", "yes"),
    ];

    harness.each_prints_after("array", prelude, &cases);
}

/// An index the compiler could not check must be checked where it lands.
///
/// All three indexable types are here. An array is the only one whose length is
/// in its *type*, so it is the only one a constant index is settled for at
/// compile time; a string's and a list's are never known until the program
/// runs, which makes this the only check they have at all.
#[test]
fn an_index_out_of_bounds_reports_and_exits() {
    let Some(harness) = Harness::find() else { return };

    let bounds = "index out of bounds";
    let at = |n: &str| format!("fn at() -> int {{\n  return {n};\n}}\n");
    let cases = [
        // Past the end, and negative — one unsigned comparison catches both.
        (
            format!("{}fn main() {{\n  int[3] xs = [1, 2, 3];\n  print(xs[at()]);\n}}", at("3")),
            bounds,
        ),
        (
            format!(
                "{}fn main() {{\n  int[3] xs = [1, 2, 3];\n  print(xs[at()]);\n}}",
                at("0 - 1")
            ),
            bounds,
        ),
        // Writing is guarded exactly as reading is.
        (
            format!("{}fn main() {{\n  int[3] xs = [1, 2, 3];\n  xs[at()] = 1;\n}}", at("9")),
            bounds,
        ),
        // And through a parameter, where the length comes from the type.
        (
            "fn set(int[2] xs, int i) {\n  xs[i] = 1;\n}\n\
             fn main() {\n  int[2] xs = [1, 2];\n  set(xs, 5);\n}"
                .to_string(),
            bounds,
        ),
        // A string, whose length is a load rather than a fact about its type.
        (
            format!("{}fn main() {{\n  string s = \"abc\";\n  print(s[at()]);\n}}", at("3")),
            bounds,
        ),
        (
            format!("{}fn main() {{\n  string s = \"abc\";\n  print(s[at()]);\n}}", at("0 - 1")),
            bounds,
        ),
        // A character counts as one however many bytes it took, so the guard
        // has to be about characters too.
        (
            format!("{}fn main() {{\n  string s = \"éé\";\n  print(s[at()]);\n}}", at("2")),
            bounds,
        ),
        // A list, including the empty one, where every index is out of range.
        (
            format!("{}fn main() {{\n  int[] xs = [1, 2];\n  print(xs[at()]);\n}}", at("2")),
            bounds,
        ),
        (
            format!("{}fn main() {{\n  int[] xs = [];\n  print(xs[at()]);\n}}", at("0")),
            bounds,
        ),
        (
            format!("{}fn main() {{\n  int[] xs = [1];\n  xs[at()] = 5;\n}}", at("1")),
            bounds,
        ),
    ];

    let cases: Vec<(&str, &str)> = cases.iter().map(|(p, e)| (p.as_str(), *e)).collect();
    harness.each_stops_with("bounds", &cases);
}

/// The two conversions that can refuse: neither invents an answer for input
/// that names none.
///
/// Both are checked where they land, because both take a value only the running
/// program knows — a constant is settled by `sema` long before this.
#[test]
fn a_conversion_that_has_no_answer_reports_and_exits() {
    let Some(harness) = Harness::find() else { return };

    let number = |n: &str| format!("fn n() -> int {{\n  return {n};\n}}\n");
    let text = |s: &str| format!("fn t() -> string {{\n  return \"{s}\";\n}}\n");
    let bad_char = "this number is not a character";
    let not_a_number = "this text is not a number";
    let cases = [
        // `char(n)`: past the last scalar value, below the first, and inside
        // the block reserved for UTF-16 surrogates in the middle.
        (format!("{}fn main() {{\n  print(char(n()));\n}}", number("1114112")), bad_char),
        (format!("{}fn main() {{\n  print(char(n()));\n}}", number("0 - 1")), bad_char),
        (format!("{}fn main() {{\n  print(char(n()));\n}}", number("55296")), bad_char),
        (format!("{}fn main() {{\n  print(char(n()));\n}}", number("57343")), bad_char),
        // `int(s)`: nothing at all, a sign with no digits, a sign in the wrong
        // place, and text that is not digits.
        (format!("{}fn main() {{\n  print(int(t()));\n}}", text("")), not_a_number),
        (format!("{}fn main() {{\n  print(int(t()));\n}}", text("-")), not_a_number),
        (format!("{}fn main() {{\n  print(int(t()));\n}}", text("+12")), not_a_number),
        (format!("{}fn main() {{\n  print(int(t()));\n}}", text("12 ")), not_a_number),
        (format!("{}fn main() {{\n  print(int(t()));\n}}", text("12a")), not_a_number),
        // ... and a number no `int` can hold, which is the same refusal: an
        // answer that had to be truncated would be a wrong one.
        (
            format!("{}fn main() {{\n  print(int(t()));\n}}", text("9223372036854775808")),
            not_a_number,
        ),
        (
            format!("{}fn main() {{\n  print(int(t()));\n}}", text("-9223372036854775809")),
            not_a_number,
        ),
    ];

    let cases: Vec<(&str, &str)> = cases.iter().map(|(p, e)| (p.as_str(), *e)).collect();
    harness.each_stops_with("convert", &cases);
}

/// The boundary values these two conversions *do* answer, next door to the ones
/// above — a guard one out either way would show up here rather than there.
#[test]
fn a_conversion_answers_everything_right_up_to_the_boundary() {
    let Some(harness) = Harness::find() else { return };

    let cases: [(&str, &str); 8] = [
        // The first and last characters there are, and the two either side of
        // the surrogate block.
        ("print(int(char(n(0))));", "0"),
        ("print(int(char(n(55295))));", "55295"),
        ("print(int(char(n(57344))));", "57344"),
        ("print(int(char(n(1114111))));", "1114111"),
        // The widest numbers an `int` holds, in and back out again. The most
        // negative one has no positive twin, which is what a parser that
        // negated at the end would lose.
        ("print(int(t(\"9223372036854775807\")));", "9223372036854775807"),
        ("print(int(t(\"-9223372036854775808\")));", "-9223372036854775808"),
        ("print(int(t(\"0\")));", "0"),
        ("print(int(t(\"-0\")));", "0"),
    ];

    // The values go through calls so that nothing is a constant `sema` could
    // settle: what is under test is the check in the emitted code.
    let prelude = "fn n(int v) -> int {\n  return v;\n}\n\
                   fn t(string v) -> string {\n  return v;\n}\n";
    harness.each_prints_after("boundary", prelude, &cases);
}

/// A `match` used as a value, where getting the control flow wrong shows up as
/// a wrong answer rather than a crash.
#[test]
fn a_match_expression_yields_the_arm_that_ran() {
    let Some(harness) = Harness::find() else { return };

    // A helper whose arms mix the two shapes, so the diverging one is exercised
    // where it belongs: leaving the function rather than reaching the join.
    let prelude = "enum Colour { Red, Green, Blue }\n\
                   fn pick(Colour c) -> int {\n  return match (c) {\n    Colour::Red => 1,\n    \
                   Colour::Green => { return 42; }\n    Colour::Blue => 3,\n  };\n}\n";
    let cases: [(&str, &str); 6] = [
        // Each arm in turn, so a chain that fell through would be caught.
        ("int n = match (Colour::Red) { Colour::Red => 1, Colour::Green => 2, Colour::Blue => 3, };\nprint(n);", "1"),
        ("int n = match (Colour::Green) { Colour::Red => 1, Colour::Green => 2, Colour::Blue => 3, };\nprint(n);", "2"),
        // The last arm is the one no test guards, so it is the one a wrong
        // chain would reach by accident.
        ("int n = match (Colour::Blue) { Colour::Red => 1, Colour::Green => 2, Colour::Blue => 3, };\nprint(n);", "3"),
        // A block arm that leaves the function: the join is never reached, so
        // the answer is the arm's own `return` and not one of the values.
        ("print(pick(Colour::Green));", "42"),
        // A block arm that leaves a *loop*. If it fell through into the join
        // instead, `n` would be overwritten with whatever the join held.
        (
            "int n = 7;\nwhile (true) {\n  n = match (Colour::Green) {\n    Colour::Red => 1,\n    \
             Colour::Green => { break; }\n    Colour::Blue => 3,\n  };\n}\nprint(n);",
            "7",
        ),
        // An arm's value may itself be computed, and lands in the same register.
        (
            "int k = 10;\nint n = match (Colour::Blue) {\n  Colour::Red => k + 1,\n  \
             Colour::Green => k * 2,\n  Colour::Blue => k - 4,\n};\nprint(n);",
            "6",
        ),
    ];

    harness.each_prints_after("matchval", prelude, &cases);
}

/// A TinyC function may be called anything, including the name of the runtime
/// routine `print` itself compiles into.
#[test]
fn a_program_may_name_a_function_after_the_runtime() {
    let Some(harness) = Harness::find() else { return };

    let run = harness.build_and_run(
        "shadow_runtime",
        "fn printf() -> int {\n  return 41;\n}\n\
         fn str0() -> int {\n  return 1;\n}\n\
         fn main() {\n  print(\"hi\");\n  print(printf() + str0());\n}\n",
        b"",
    );
    assert!(run.status.success(), "exited with {}\n{}", run.status, run.stderr);
    assert_eq!(normalise(&run.stdout), "hi\n42\n");
}


/// Every type `print` accepts renders as itself.
///
/// `print` is the only way a TinyC program says anything, so a type it renders
/// wrongly makes every other test in this file report about the wrong thing.
/// One case per printable type, and one for each of the two `bool` texts.
#[test]
fn every_type_prints_as_itself() {
    let Some(harness) = Harness::find() else { return };

    let prelude = "enum Colour { Red, Green, Blue }\n";
    let cases: [(&str, &str); 12] = [
        ("print(0);", "0"),
        ("print(7);", "7"),
        // The widest numbers there are, which is where a renderer that worked
        // digit by digit would run out.
        ("print(9223372036854775807);", "9223372036854775807"),
        ("print(0 - 9223372036854775807 - 1);", "-9223372036854775808"),
        ("print(true);", "true"),
        ("print(false);", "false"),
        ("print(\"text\");", "text"),
        // A character is printed as itself, not as its number.
        ("print('a');", "a"),
        ("print('\u{e9}');", "\u{e9}"),
        // A variant prints its own name, and the last one is the one a table
        // read one entry short would miss.
        ("print(Colour::Red);", "Red"),
        ("print(Colour::Blue);", "Blue"),
        // A value computed rather than written, so nothing was folded away.
        ("int n = 20;\nprint(n * 5 + 1);", "101"),
    ];

    harness.each_prints_after("print", prelude, &cases);
}

/// Arithmetic that lands exactly on the edge of the range, and must *not* be
/// refused.
///
/// TinyC stops a program rather than answering wrongly, which makes the guard
/// itself worth testing from both sides: `tests/execution.rs` already has the
/// operations that overflow, and these are their neighbours that do not. A
/// guard one out either way would show up here rather than there.
#[test]
fn arithmetic_at_the_edge_of_the_range_still_answers() {
    let Some(harness) = Harness::find() else { return };

    // The values come back from calls so nothing is a constant `sema` could
    // settle: what is under test is the check in the emitted code.
    let prelude = "fn v(int n) -> int {\n  return n;\n}\n\
                   fn max() -> int {\n  return 9223372036854775807;\n}\n\
                   fn min() -> int {\n  return 0 - 9223372036854775807 - 1;\n}\n";
    let cases: [(&str, &str); 14] = [
        // Reaching each end exactly.
        ("print(max() - 1 + 1);", "9223372036854775807"),
        ("print(min() + 1 - 1);", "-9223372036854775808"),
        ("print(max() + min());", "-1"),
        ("print(max() - max());", "0"),
        // Multiplication that just fits, and its negative twin.
        ("print(v(4611686018427387903) * 2 + 1);", "9223372036854775807"),
        ("print(v(0 - 4611686018427387904) * 2);", "-9223372036854775808"),
        // Division at the ends. `min() / -1` is the one that overflows and is
        // checked elsewhere; these are the ones next to it.
        ("print(min() / v(1));", "-9223372036854775808"),
        ("print(max() / v(0 - 1));", "-9223372036854775807"),
        ("print(min() / v(2));", "-4611686018427387904"),
        // A remainder that is zero, including for the value with no positive
        // twin — where the division it is paired with would overflow.
        ("print(min() % v(1));", "0"),
        ("print(max() % max());", "0"),
        // Negating the largest positive number is fine; negating the most
        // negative one is not, and is checked elsewhere.
        ("print(-max());", "-9223372036854775807"),
        // Zero absorbs, and must not be mistaken for an overflow.
        ("print(min() * v(0));", "0"),
        // Both ends in one expression, reaching zero through -1 rather than
        // through `0 - min()`, which has no answer at all.
        ("print(min() + max() + 1);", "0"),
    ];

    harness.each_prints_after("edge", prelude, &cases);
}

/// Every comparison, checked on both sides of the boundary it turns on.
///
/// Each pair of operators differs at exactly one input — `<` and `<=` only at
/// equality — so an off-by-one in the condition codes is invisible anywhere
/// else. All four combinations of sign are here too, because a comparison done
/// unsigned would get the positive cases right and the negative ones wrong.
#[test]
fn every_comparison_turns_at_the_right_place() {
    let Some(harness) = Harness::find() else { return };

    let prelude = "fn v(int n) -> int {\n  return n;\n}\n";
    let cases: [(&str, &str); 18] = [
        ("print(v(5) < v(5));", "false"),
        ("print(v(4) < v(5));", "true"),
        ("print(v(5) <= v(5));", "true"),
        ("print(v(6) <= v(5));", "false"),
        ("print(v(5) > v(5));", "false"),
        ("print(v(6) > v(5));", "true"),
        ("print(v(5) >= v(5));", "true"),
        ("print(v(4) >= v(5));", "false"),
        ("print(v(5) == v(5));", "true"),
        ("print(v(5) != v(5));", "false"),
        // A negative against a positive: unsigned, this answers the opposite.
        ("print(v(0 - 1) < v(1));", "true"),
        ("print(v(0 - 1) > v(1));", "false"),
        // Two negatives, where the larger magnitude is the smaller number.
        ("print(v(0 - 5) < v(0 - 4));", "true"),
        ("print(v(0 - 5) >= v(0 - 4));", "false"),
        // The two ends of the range against each other.
        ("print(v(0 - 9223372036854775807 - 1) < v(9223372036854775807));", "true"),
        ("print(v(9223372036854775807) <= v(0 - 9223372036854775807 - 1));", "false"),
        // Characters and enums answer equality, and only equality.
        ("print('a' == 'a');\nprint('a' != 'b');", "true\ntrue"),
        ("print(\"ab\" == \"ab\");\nprint(\"ab\" != \"ba\");", "true\ntrue"),
    ];

    harness.each_prints_after("compare", prelude, &cases);
}

/// Calls at the limits of what the calling convention can carry.
///
/// Arguments travel in registers, and there are only so many — so the shapes
/// worth running are the ones that use all of them, use them again before the
/// first call has finished, or spend one of them on something invisible.
#[test]
fn calls_pass_their_arguments_even_at_the_limits() {
    let Some(harness) = Harness::find() else { return };

    let prelude = "class Point {\n  int x;\n  int y;\n}\n\
                   fn four(int a, int b, int c, int d) -> int {\n  \
                     return a * 1000 + b * 100 + c * 10 + d;\n}\n\
                   fn one(int a) -> int {\n  return a + 1;\n}\n\
                   fn build(int a, int b, int c) -> Point {\n  \
                     return Point { x: a * 100 + b, y: c };\n}\n\
                   fn depth(int n) -> int {\n  if (n == 0) {\n    return 0;\n  }\n  \
                     return 1 + depth(n - 1);\n}\n\
                   fn total(int n) -> int {\n  if (n == 0) {\n    return 0;\n  }\n  \
                     return n + total(n - 1);\n}\n";
    let cases: [(&str, &str); 9] = [
        // Every argument register at once, in order — a pair swapped would
        // still answer a number, just the wrong one.
        ("print(four(1, 2, 3, 4));", "1234"),
        // Each argument the result of a call, so every one of them is live
        // across a later call and none may sit where a call destroys it.
        ("print(four(one(0), one(1), one(2), one(3)));", "1234"),
        // A call inside a call's own argument list, at the last position.
        ("print(four(1, 2, 3, one(3)));", "1234"),
        // Returning an aggregate spends an argument register on the hidden
        // address of the caller's room, leaving three — so this is the full
        // house for a function that returns one.
        ("Point p = build(1, 2, 3);\nprint(p.x * 10 + p.y);", "1023"),
        // The returned object outlives the call that built it, and a second
        // call must not land on top of the first one's room.
        (
            "Point a = build(1, 2, 3);\nPoint b = build(4, 5, 6);\n\
             print(a.x * 10000 + b.x);",
            "1020405",
        ),
        // Recursion deep enough that the frames have to be right hundreds of
        // times over, not just once.
        ("print(depth(500));", "500"),
        ("print(total(1000));", "500500"),
        // A call in a loop condition, evaluated afresh each time round.
        (
            "int n = 0;\nwhile (one(n) < 10) {\n  n = n + 1;\n}\nprint(n);",
            "9",
        ),
        // Values that must survive a call made between their definition and
        // their use — more of them than there are registers to hold.
        (
            "int a = 1; int b = 2; int c = 3; int d = 4; int e = 5;\n\
             int f = 6; int g = 7; int h = 8;\n\
             print(one(0) + a + b + c + d + e + f + g + h);",
            "37",
        ),
    ];

    harness.each_prints_after("call", prelude, &cases);
}

/// Code that never runs still has to be code.
///
/// A loop whose condition starts false, an `if` with nothing on the other side,
/// a function with an empty body: each emits a block that is jumped over. If
/// the jump goes to the wrong place, or a frame is set up and not released, the
/// failure is not a wrong answer — it is a crash, or a program that never ends.
#[test]
fn a_body_that_never_runs_leaves_everything_as_it_was() {
    let Some(harness) = Harness::find() else { return };

    let prelude = "fn nothing() {\n}\n\
                   fn nothing_twice() {\n  if (true) {\n  }\n  while (false) {\n  }\n}\n";
    let cases: [(&str, &str); 10] = [
        ("nothing();\nprint(1);", "1"),
        ("nothing_twice();\nprint(2);", "2"),
        // A `while` whose condition is false from the start.
        ("int n = 0;\nwhile (n > 0) {\n  n = n + 1;\n}\nprint(n);", "0"),
        // A `for` whose range is empty, including one that never could run.
        ("int n = 0;\nfor (int i = 0; i < 0; i = i + 1) {\n  n = n + 1;\n}\nprint(n);", "0"),
        ("int n = 0;\nfor (int i = 5; i < 3; i = i + 1) {\n  n = n + 1;\n}\nprint(n);", "0"),
        // An `if` with no `else`, taken and not taken.
        ("int n = 1;\nif (n > 5) {\n  n = 99;\n}\nprint(n);", "1"),
        ("int n = 1;\nif (n < 5) {\n  n = 99;\n}\nprint(n);", "99"),
        // An empty block in the middle of a function, which must not disturb
        // what is around it.
        ("int n = 1;\nif (true) {\n}\nn = n + 1;\nprint(n);", "2"),
        // A loop that runs exactly once, which is the boundary either side of
        // the two above.
        ("int n = 0;\nfor (int i = 0; i < 1; i = i + 1) {\n  n = n + 1;\n}\nprint(n);", "1"),
        // A `break` on the first pass, so the body runs once and the loop ends
        // without the condition ever being false.
        ("int n = 0;\nwhile (true) {\n  n = n + 1;\n  break;\n}\nprint(n);", "1"),
    ];

    harness.each_prints_after("empty", prelude, &cases);
}
