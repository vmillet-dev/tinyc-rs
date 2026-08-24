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
        ("int y = 3;\nint x = 10;\nx = y - x;\nprintln(x);", "-7"),
        ("int y = 3;\nint x = 100;\nx = x / y;\nprintln(x);", "33"),
        ("int y = 3;\nint x = 10;\nx = x - y;\nprintln(x);", "7"),
        // Commutative operators swap instead, which must not change the answer.
        ("int y = 3;\nint x = 10;\nx = y + x;\nprintln(x);", "13"),
        // An immediate wider than the 32 bits an ALU operand allows.
        ("int a = 1;\nprintln(a + 4611686018427387904);", "4611686018427387905"),
        // Signed division rounds towards zero, and the sign has to survive
        // `cqo`.
        ("int a = 0 - 100;\nint b = 7;\nprintln(a / b);", "-14"),
        // Enough live values at once to force spills, all read back afterwards.
        (
            "int a = 1; int b = 2; int c = 3; int d = 4; int e = 5;\n\
             int f = 6; int g = 7; int h = 8; int i = 9; int j = 10;\n\
             println(a - b - c - d - e - f - g - h - i - j);",
            "-53",
        ),
        // A comparison used as a value rather than as a branch.
        ("int n = 5;\nbool big = n > 3;\nprintln(big);", "true"),
        // The guards around a division must not change a division that works.
        ("int a = 7;\nint b = 0 - 1;\nprintln(a / b);", "-7"),
        // A loop whose counter is compared against a literal, which is the
        // shape the branch fusion rewrites.
        (
            "int total = 0;\nfor (int k = 1; k <= 10; k = k + 1) {\n  total = total + k;\n}\n\
             println(total);",
            "55",
        ),
        // `%` reads `rdx` where `/` reads `rax`; swapping them silently would
        // still produce a plausible-looking number.
        ("int a = 17;\nint b = 5;\nprintln(a % b);", "2"),
        ("int a = 17;\nint b = 5;\nprintln(a / b);", "3"),
        // The remainder takes its sign from the dividend, which is what `cqo`
        // sign-extending into `rdx:rax` is for.
        ("int a = 0 - 17;\nint b = 5;\nprintln(a % b);", "-2"),
        ("int a = 17;\nint b = 0 - 5;\nprintln(a % b);", "2"),
        // The destination shares a register with an operand, as for division.
        ("int a = 17;\nint b = 5;\na = a % b;\nprintln(a);", "2"),
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
        (r#"println(len("héllo"));"#, "5"),
        (r#"println(len("日本語"));"#, "3"),
        (r#"println(len(""));"#, "0"),
        // Joining, including the two empty cases the copy loops have to get
        // right by doing nothing at all.
        (r#"println("a" + "b" == "ab");"#, "true"),
        (r#"println("" + "x" == "x");"#, "true"),
        (r#"println("x" + "" == "x");"#, "true"),
        (r#"println(len("é" + "é"));"#, "2"),
        // Equality is about contents. Two literals are interned, so a program
        // that compared addresses would get the first of these right and the
        // second wrong.
        (r#"println("ab" == "ab");"#, "true"),
        (r#"string s = "a" + "b";
            println(s == "ab");"#, "true"),
        (r#"println("ab" == "abc");"#, "false"),
        (r#"println("abc" == "abd");"#, "false"),
        // Indexing lands on the character it promises, not on a byte of one.
        (r#"println(int("héllo"[1]));"#, "233"),
        (r#"println(int("日本語"[2]));"#, "35486"),
        // A round trip through both conversions, for a character that needs
        // three bytes of UTF-8 and would lose them if anything counted bytes.
        (r#"println(string(char(int('語'))) == "語");"#, "true"),
        // A number written out, including the one with no positive twin.
        (r#"println(string(0 - 9223372036854775807 - 1) == "-9223372036854775808");"#, "true"),
        // Enough joining to send the arena back to `malloc` for more chunks.
        // Deliberately modest: `s = s + x` abandons every intermediate, so the
        // memory this costs grows with the *square* of the loop count.
        (r#"string s = "";
            for (int i = 0; i < 300; i = i + 1) {
              s = s + "0123456789";
            }
            println(len(s));"#, "3000"),
        // Doubling, which asks for one block larger than a whole chunk — the
        // path where the arena gives up on its usual size and asks for what was
        // wanted instead. It also joins a string to *itself*, so the two source
        // pointers the copy loops walk are the same one.
        (r#"string s = "0123456789abcdef";
            for (int i = 0; i < 11; i = i + 1) {
              s = s + s;
            }
            println(len(s));"#, "32768"),
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
            println(len(xs));"#, "0"),
        (r#"int[] xs = [3, 1, 4];
            println(len(xs) * 100 + xs[2]);"#, "304"),
        // Past the first block, so the elements are copied at least twice.
        (r#"int[] xs = [];
            for (int i = 0; i < 100; i = i + 1) { push(xs, i); }
            println(len(xs) * 1000 + xs[99]);"#, "100099"),
        // Every element survives the moves, not just the last one.
        (r#"int[] xs = [];
            for (int i = 0; i < 100; i = i + 1) { push(xs, i * i); }
            int total = 0;
            for (int i = 0; i < len(xs); i = i + 1) { total = total + xs[i]; }
            println(total);"#, "328350"),
        // Assignment copies: growing one must not be visible through the other,
        // and neither must writing an element.
        (r#"int[] a = [1, 2];
            int[] b = a;
            push(b, 3);
            println(len(a) * 10 + len(b));"#, "23"),
        (r#"int[] a = [1, 2];
            int[] b = a;
            b[0] = 9;
            println(a[0] * 10 + b[0]);"#, "19"),
        // ... even when the copy is made from a list that has already moved.
        (r#"int[] a = [];
            for (int i = 0; i < 50; i = i + 1) { push(a, i); }
            int[] b = a;
            push(b, 999);
            println(len(a) * 1000 + b[50]);"#, "50999"),
        // A parameter borrows: writing an element is visible to the caller.
        (r#"int[] a = [1, 2];
            double_each(a);
            println(a[0] + a[1]);"#, "6"),
        // A returned list outlives the call that built it.
        (r#"int[] a = squares(30);
            println(len(a) * 10000 + a[29]);"#, "300900"),
        // Lists of the other things that fit in a register.
        (r#"string[] w = [];
            push(w, "a");
            push(w, "bé");
            println(len(w) * 10 + len(w[1]));"#, "22"),
        (r#"char[] cs = ['a', 'b'];
            push(cs, 'c');
            println(string(cs) == "abc");"#, "true"),
        // The reason lists come before `read_line`: building a string this way
        // is linear, where `s = s + x` in a loop is quadratic in both time and
        // memory. 20000 characters would cost gigabytes the other way.
        (r#"char[] cs = [];
            for (int i = 0; i < 20000; i = i + 1) { push(cs, '0'); }
            string s = string(cs);
            println(len(s));"#, "20000"),
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

/// A list holds its elements *whole*, which is what lets one hold objects.
///
/// Everything here would still compile if a list held addresses instead, and
/// nearly everything would still print the right number — until the list moved.
/// So the cases that matter are the ones that grow past a block: an element
/// read after the move, and an element pushed *from* the block being left.
#[test]
fn lists_hold_objects_whole() {
    let Some(harness) = Harness::find() else { return };

    let prelude = "class Point {\n  int x;\n  int y;\n}\n\
                   class Pair {\n  Point a;\n  Point b;\n}\n\
                   class Named {\n  string name;\n  int n;\n}\n\
                   class Shape {\n  fn area(self) -> int { return 0; }\n}\n\
                   class Circle : Shape {\n  int r;\n  \
                   fn area(self) -> int { return 3 * self.r * self.r; }\n}\n\
                   class Rect : Shape {\n  int w;\n  int h;\n  \
                   fn area(self) -> int { return self.w * self.h; }\n}\n\
                   fn built(int n) -> Point[] {\n  Point[] ps = [];\n  \
                   for (int i = 0; i < n; i = i + 1) {\n    \
                   push(ps, Point { x: i, y: i * 2 });\n  }\n  return ps;\n}\n\
                   fn total(Shape[] ss) -> int {\n  int sum = 0;\n  \
                   for (int i = 0; i < len(ss); i = i + 1) {\n    \
                   sum = sum + ss[i].area();\n  }\n  return sum;\n}\n\
                   fn bump(Point[] ps) {\n  ps[0].x = ps[0].x + 1;\n}\n";

    let cases: [(&str, &str); 10] = [
        // An element that is itself composed: the list moves whole objects,
        // whatever they are made of, and copying the list copies all of it.
        (
            "Pair[] ps = [];\n\
             for (int i = 0; i < 30; i = i + 1) {\n  \
             push(ps, Pair { a: Point { x: i, y: 0 }, b: Point { x: i * 10, y: 1 } });\n}\n\
             Pair[] kept = ps;\nps[0].a.x = 999;\nprintln(ps[29].b.x + kept[0].a.x);",
            "290",
        ),
        // Built by pushing, handed back, and read at both ends: an element is
        // two fields wide, so an address that still counted registers would
        // land in the middle of one.
        ("Point[] ps = built(10);\nprintln(ps[9].x + ps[9].y);", "27"),
        ("Point[] ps = built(1);\nprintln(len(ps) + ps[0].y);", "1"),
        ("Point[] ps = [];\nprintln(len(ps));", "0"),
        // A hundred of them is several moves, and every element has to arrive.
        (
            "Point[] ps = built(100);\nint sum = 0;\n\
             for (int i = 0; i < len(ps); i = i + 1) {\n  sum = sum + ps[i].x;\n}\nprintln(sum);",
            "4950",
        ),
        // Writing through an element writes into the list, and a parameter is
        // still the caller's own.
        ("Point[] ps = built(3);\nps[1].x = 50;\nprintln(ps[1].x + ps[2].x);", "52"),
        ("Point[] ps = built(3);\nbump(ps);\nprintln(ps[0].x);", "1"),
        // Assignment copies the elements, so writing through one is invisible
        // through the other.
        ("Point[] a = built(3);\nPoint[] b = a;\na[0].x = 9;\nprintln(b[0].x + len(b));", "3"),
        // Pushing one of the list's own elements: the block it is read from is
        // the one being left behind, and the arena never takes it back.
        (
            "Point[] rs = [Point { x: 7, y: 8 }];\n\
             for (int i = 0; i < 6; i = i + 1) {\n  push(rs, rs[0]);\n}\n\
             println(rs[6].x + rs[6].y + len(rs));",
            "22",
        ),
        // A list of a base class: every slot is the hierarchy's size, and the
        // vtable pointer travels with the copy into it.
        (
            "Shape[] ss = [Circle { r: 1 }, Rect { w: 2, h: 3 }];\n\
             push(ss, Circle { r: 2 });\nprintln(total(ss));",
            "21",
        ),
    ];

    harness.each_prints_after("objlist", prelude, &cases);

    // A string inside an element is one pointer among the object's bytes, and
    // it has to survive every move the list makes.
    let cases: [(&str, &str); 1] = [(
        "Named[] ns = [];\nfor (int i = 0; i < 20; i = i + 1) {\n  \
         push(ns, Named { name: \"n\" + string(i), n: i });\n}\n\
         println(ns[19].name + \" \" + string(ns[19].n));",
        "n19 19",
    )];
    harness.each_prints_after("objliststr", prelude, &cases);
}

/// Reading input, which is the one thing that cannot be checked by looking at
/// the program alone: what it does depends on what it is given.
#[test]
fn reading_the_input_sees_characters_and_knows_when_to_stop() {
    let Some(harness) = Harness::find() else { return };

    // `(input, body, expected)`. The inputs are written as raw bytes on
    // purpose: what arrives is UTF-8, and turning it into characters is the
    // very thing under test.
    let cases: [(&[u8], &str, &str); 15] = [
        // Nothing at all: the end of the input is where the program starts.
        (b"", "println(eof());", "true"),
        (b"x\n", "println(eof());", "false"),
        // Asking does not consume, so asking twice answers twice.
        (b"x\n", "println(eof());\nprintln(eof());\nprintln(read_line());", "false\nfalse\nx"),
        // A line is what is between the endings, and both endings work.
        (b"one\ntwo\n", "println(read_line());\nprintln(read_line());", "one\ntwo"),
        (b"one\r\ntwo\r\n", "println(len(read_line()));", "3"),
        // An empty line is a line, and is not the end.
        (b"\nx\n", "println(len(read_line()));\nprintln(read_line());", "0\nx"),
        // A last line with no ending is still a line.
        (b"tail", "println(read_line());\nprintln(eof());", "tail\ntrue"),
        // Characters, not bytes: five characters in six bytes.
        ("h\u{e9}llo\n".as_bytes(), "println(len(read_line()));", "5"),
        ("\u{65e5}\u{672c}\u{8a9e}\n".as_bytes(), "println(len(read_line()));", "3"),
        // A line longer than the buffer, so the refill happens mid-line.
        (
            &[b'a'; 5000],
            "string line = read_line();\nprintln(len(line));",
            "5000",
        ),
        // Counting an unknown quantity, which is what all of this was for.
        (
            b"3\n1\n4\n1\n5\n",
            "int total = 0;\n\
             int[] seen = [];\n\
             while (!eof()) {\n  push(seen, int(read_line()));\n}\n\
             for (int i = 0; i < len(seen); i = i + 1) { total = total + seen[i]; }\n\
             println(len(seen) * 100 + total);",
            "514",
        ),
        // Text into a number and out again, unchanged — including the one with
        // no positive twin, which a parser that negates at the end would lose.
        (
            b"-9223372036854775808\n",
            "println(string(int(read_line())) == \"-9223372036854775808\");",
            "true",
        ),
        // A byte order mark is how several Windows editors spell "this file is
        // UTF-8". It is not a character of the text, and a program that counted
        // it would read `42` as a three-character word rather than as a number.
        (b"\xEF\xBB\xBF42\n", "println(int(read_line()) + 1);", "43"),
        // Only the first bytes can carry one: the same three bytes later in the
        // input are the character they spell, and stay.
        (
            b"\xEF\xBB\xBFa\n\xEF\xBB\xBFb\n",
            "println(read_line());\nprintln(len(read_line()));",
            "a\n2",
        ),
        // A mark and nothing else is a file with no lines in it, not a file
        // whose first line failed to arrive.
        (b"\xEF\xBB\xBF", "println(eof());", "true"),
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
        "fn main() {\n  println(read_line());\n  println(read_line());\n}\n",
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
        harness.build_and_run("bad_utf8", "fn main() {\n  println(read_line());\n}\n", b"\xff\xfe\n");
    assert!(!run.status.success(), "it should not have finished");
    assert!(run.stderr.contains("not valid UTF-8"), "{}", run.stderr);
}

/// A division the hardware cannot perform must say so and stop, rather than
/// dying on a hardware exception with nothing printed.
#[test]
fn a_division_that_cannot_be_performed_reports_and_exits() {
    let Some(harness) = Harness::find() else { return };

    let cases = [
        ("fn zero() -> int {\n  return 0;\n}\nfn main() {\n  println(1 / zero());\n}", "by zero"),
        (
            "fn neg() -> int {\n  return 0 - 1;\n}\n\
             fn main() {\n  int m = 0 - 9223372036854775807 - 1;\n  println(m / neg());\n}",
            "overflows",
        ),
        // `%` is the same `idiv`, so it faults in both the same ways — the
        // second one even though `MIN % -1` is 0 on paper.
        ("fn zero() -> int {\n  return 0;\n}\nfn main() {\n  println(1 % zero());\n}", "by zero"),
        (
            "fn neg() -> int {\n  return 0 - 1;\n}\n\
             fn main() {\n  int m = 0 - 9223372036854775807 - 1;\n  println(m % neg());\n}",
            "overflows",
        ),
        // Overflow the compiler cannot see, because the value comes back from a
        // call. Each of the three guarded operators, plus unary minus, which is
        // a subtraction and inherits the guard.
        (
            "fn big() -> int {\n  return 9223372036854775807;\n}\n\
             fn main() {\n  println(big() + 1);\n}",
            "arithmetic overflows",
        ),
        (
            "fn small() -> int {\n  return 0 - 9223372036854775807 - 1;\n}\n\
             fn main() {\n  println(small() - 1);\n}",
            "arithmetic overflows",
        ),
        (
            "fn big() -> int {\n  return 9223372036854775807;\n}\n\
             fn main() {\n  println(big() * 2);\n}",
            "arithmetic overflows",
        ),
        (
            "fn small() -> int {\n  return 0 - 9223372036854775807 - 1;\n}\n\
             fn main() {\n  println(-small());\n}",
            "arithmetic overflows",
        ),
        // The case the abort routine's own frame exists for: `bump` makes no
        // call, so it is a leaf and reserves nothing. Whatever `rsp` it hands
        // over, the report has to be able to call `_write` from it.
        (
            "fn bump(int n) -> int {\n  return n + 9223372036854775807;\n}\n\
             fn main() {\n  println(bump(2));\n}",
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
        ("int z = 0;\nprintln(z != 0 && 10 / z > 1);", "false"),
        ("int z = 0;\nprintln(z == 0 || 10 / z > 1);", "true"),
        // ... but it must run when the left one settles nothing.
        ("int z = 2;\nprintln(z != 0 && 10 / z > 1);", "true"),
        ("int z = 2;\nprintln(z == 0 || 10 / z > 4);", "true"),
        // `&&` binds tighter than `||`, so this is `true || (false && false)`.
        ("println(true || false && false);", "true"),
        // A chain, evaluated left to right and stopping at the first `false`.
        ("int z = 0;\nprintln(1 < 2 && z == 0 && 2 < 1 && 10 / z > 0);", "false"),
        // A `continue` in a `for` still runs the step, or this never ends.
        (
            "int total = 0;\nfor (int i = 1; i <= 10; i = i + 1) {\n  if (i == 5) {\n    \
             continue;\n  }\n  total = total + i;\n}\nprintln(total);",
            "50",
        ),
        // The same in a `while`, where the increment is inside the body and a
        // `continue` therefore has to come *after* it.
        (
            "int i = 0;\nint total = 0;\nwhile (i < 10) {\n  i = i + 1;\n  if (i == 5) {\n    \
             continue;\n  }\n  total = total + i;\n}\nprintln(total);",
            "50",
        ),
        // `break` leaves the innermost loop only: the outer one runs to the end.
        (
            "int hits = 0;\nfor (int a = 1; a <= 3; a = a + 1) {\n  \
             for (int b = 1; b <= 3; b = b + 1) {\n    if (b == 2) {\n      break;\n    }\n    \
             hits = hits + 1;\n  }\n}\nprintln(hits);",
            "3",
        ),
        // A short circuit *as* a loop condition, so its join is what the back
        // edge returns through.
        (
            "int i = 0;\nwhile (i < 100 && i * i < 30) {\n  i = i + 1;\n}\nprintln(i);",
            "6",
        ),
        // The two new shapes stacked: a `for` whose step is itself a short
        // circuit, reached through the step block a `continue` asked for. The
        // back edge has to leave the block the step *ended* in, which is the
        // join of the short circuit rather than the step block itself.
        (
            "bool ok = true;\nint i = 0;\nfor (i = 0; i < 4; ok = ok && i < 2) {\n  \
             if (i == 1) {\n    i = i + 1;\n    continue;\n  }\n  i = i + 1;\n}\nprintln(ok);",
            "false",
        ),
        // `!(a < b)` is compiled as `a >= b`, so an off-by-one in the inversion
        // table would show up right at the boundary where the two differ.
        ("int a = 5;\nint b = 5;\nprintln(!(a < b));", "true"),
        ("int a = 5;\nint b = 5;\nprintln(!(a <= b));", "false"),
        // Negating something that is not a comparison goes through `== 0`.
        ("bool ok = 1 > 0;\nprintln(!ok);", "false"),
        ("bool ok = 1 > 0;\nprintln(!!ok);", "true"),
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
        ("Circle c = Circle { r: 5 };\nprintln(report(c));", "75"),
        // A second subclass through the same parameter, so the dispatch really
        // depends on the object and not on where it was written.
        ("Rect r = Rect { w: 4, h: 6 };\nprintln(report(r));", "24"),
        // A method the subclass does *not* override still reaches the base's.
        ("Circle c = Circle { r: 1 };\nprintln(label(c));", "shape"),
        ("Rect r = Rect { w: 1, h: 1 };\nprintln(label(r));", "rect"),
        // Fields, read and written through the object.
        ("Rect r = Rect { w: 4, h: 6 };\nr.w = 10;\nprintln(r.area());", "60"),
        // A field declared by the base and one by the subclass do not overlap.
        ("Rect r = Rect { h: 7, w: 3 };\nprintln(r.w * 100 + r.h);", "307"),
        // Two objects at once, so their frame regions must stay apart.
        (
            "Circle a = Circle { r: 2 };\nRect b = Rect { w: 5, h: 5 };\n\
             a.r = 3;\nprintln(a.area() + b.area());",
            "52",
        ),
        // A direct call on a sealed class: `Rect` has no subclasses, so this is
        // the devirtualised path rather than the vtable one.
        ("Rect r = Rect { w: 3, h: 3 };\nprintln(r.area());", "9"),
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
        ("Shape s = Circle { r: 2 };\nprintln(s.area());", "12"),
        ("Shape s = Rect { w: 3, h: 5 };\nprintln(s.area());", "15"),
        // A copy, not an alias: changing the source must not change the copy.
        (
            "Circle c = Circle { r: 3 };\nShape t = c;\nc.r = 100;\nprintln(t.area());",
            "27",
        ),
        // The same for assignment rather than declaration.
        (
            "Shape s = Circle { r: 1 };\nCircle c = Circle { r: 4 };\ns = c;\nc.r = 100;\n\
             println(s.area());",
            "48",
        ),
        // A returned object, both branches, through the caller's own room.
        ("println(pick(0).area());", "75"),
        ("println(pick(1).area());", "24"),
        // A returned object outliving the call that made it, which is the whole
        // point of copying into the caller's room.
        ("Shape s = pick(0);\nprintln(s.area());", "75"),
        // A heterogeneous collection: three objects of different sizes, each in
        // a slot the size of the biggest.
        (
            "Shape[3] all = [Circle { r: 1 }, Rect { w: 2, h: 3 }, Circle { r: 2 }];\n\
             int total = 0;\n\
             for (int i = 0; i < len(all); i = i + 1) {\n  total = total + all[i].area();\n}\n\
             println(total);",
            "21",
        ),
    ];

    harness.each_prints_after("poly", prelude, &cases);
}

/// What an object contains, it contains *inside* itself.
///
/// The one feature where a field's offset stops being its position: an
/// aggregate field takes its whole size, so everything after it moves along by
/// that much and a copy carries all of it. A wrong offset here reads a
/// neighbouring field, and a missing copy shows up as one object changing when
/// another is written to — both of which are wrong *numbers* rather than
/// crashes.
#[test]
fn objects_hold_what_they_contain_inside_them() {
    let Some(harness) = Harness::find() else { return };

    let prelude = "class Point {\n  int x;\n  int y;\n  \
                   fn sum(self) -> int { return self.x + self.y; }\n}\n\
                   class Segment {\n  Point a;\n  Point b;\n}\n\
                   class Row {\n  int[3] cells;\n}\n\
                   class Grid {\n  Row[2] rows;\n}\n\
                   class Shape {\n  fn area(self) -> int { return 0; }\n}\n\
                   class Circle : Shape {\n  int r;\n  \
                   fn area(self) -> int { return 3 * self.r * self.r; }\n}\n\
                   class Holder {\n  Shape held;\n  int tag;\n}\n\
                   fn seg() -> Segment {\n  \
                   return Segment { a: Point { x: 1, y: 2 }, b: Point { x: 10, y: 20 } };\n}\n\
                   fn widen(Segment s) {\n  s.b.x = s.b.x + 1;\n}\n\
                   fn moved(Segment s, int by) -> Segment {\n  \
                   return Segment { a: s.a, b: Point { x: s.b.x + by, y: s.b.y } };\n}\n";

    let cases: [(&str, &str); 13] = [
        // Two levels down, read and written. `b` starts past the whole of `a`,
        // so a field offset that still counted registers would land in `a.y`.
        ("Segment s = seg();\nprintln(s.b.x);", "10"),
        ("Segment s = seg();\ns.b.x = 30;\nprintln(s.b.x);", "30"),
        ("Segment s = seg();\nprintln(s.a.sum() + s.b.sum());", "33"),
        // Copying the outer object copies the inner ones with it.
        ("Segment s = seg();\nSegment t = s;\ns.a.x = 100;\nprintln(t.a.x);", "1"),
        // ... and copying an inner one out is a copy too, not a second name.
        ("Segment s = seg();\nPoint p = s.b;\ns.b.x = 99;\nprintln(p.x);", "10"),
        // A whole object written into a field.
        ("Segment s = seg();\ns.b = Point { x: 7, y: 8 };\nprintln(s.b.sum());", "15"),
        // A parameter still borrows the caller's object, however deep the write
        // goes, and a returned one still fills room the caller reserved.
        ("Segment s = seg();\nwiden(s);\nprintln(s.b.x);", "11"),
        ("Segment s = seg();\nprintln(moved(s, 5).b.x);", "15"),
        // An array field: its elements are the object's own bytes.
        (
            "Row r = Row { cells: [1, 2, 3] };\nr.cells[1] = 5;\n\
             println(r.cells[0] + r.cells[1] + r.cells[2]);",
            "9",
        ),
        // An array of objects inside an object, indexed twice over.
        (
            "Grid g = Grid { rows: [Row { cells: [1, 2, 3] }, Row { cells: [4, 5, 6] }] };\n\
             g.rows[1].cells[2] = 60;\nprintln(g.rows[0].cells[0] + g.rows[1].cells[2]);",
            "61",
        ),
        // Writing through an element that is an object: the address of the
        // element is the object, with nothing to read out of it first.
        (
            "Point[2] ps = [Point { x: 1, y: 1 }, Point { x: 2, y: 2 }];\n\
             ps[1].x = 7;\nprintln(ps[1].x + ps[0].x);",
            "8",
        ),
        // A field may be any class in a hierarchy: it reserves the biggest, and
        // the vtable pointer travels with the copy. The field beside it, which
        // starts past all of that room, must be untouched.
        (
            "Holder h = Holder { held: Circle { r: 2 }, tag: 5 };\nprintln(h.held.area() + h.tag);",
            "17",
        ),
        ("Holder h = Holder { held: Circle { r: 2 }, tag: 5 };\nHolder g = h;\nh.tag = 9;\nprintln(g.tag);", "5"),
    ];

    harness.each_prints_after("nested", prelude, &cases);
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
        ("int[4] xs = [7, 8, 9, 10];\nprintln(xs[0]);", "7"),
        ("int[4] xs = [7, 8, 9, 10];\nprintln(xs[3]);", "10"),
        // The same through an index the compiler cannot fold.
        ("int[4] xs = [7, 8, 9, 10];\nint i = 3;\nprintln(xs[i]);", "10"),
        // Writing, then reading back through a different index expression.
        ("int[4] xs = [0, 0, 0, 0];\nxs[2] = 42;\nint i = 2;\nprintln(xs[i]);", "42"),
        // A callee writing through the caller's array.
        (
            "int[4] xs = [0, 0, 0, 0];\nfill(xs);\nprintln(xs[3]);",
            "30",
        ),
        // Two arrays at once, so their frame regions must not overlap.
        (
            "int[2] a = [1, 2];\nint[2] b = [3, 4];\na[0] = 99;\nprintln(b[0] + a[0]);",
            "102",
        ),
        // An array live across a call: the base register has to survive it.
        (
            "int[2] a = [5, 6];\nfill([0, 0, 0, 0]);\nprintln(a[1]);",
            "6",
        ),
        // Enough other values to force spills, with the array still readable.
        (
            "int[2] xs = [1, 2];\nint a = 1; int b = 2; int c = 3; int d = 4; int e = 5;\n\
             int f = 6; int g = 7; int h = 8; int i = 9; int j = 10;\n\
             println(xs[1] + a + b + c + d + e + f + g + h + i + j);",
            "57",
        ),
        // Strings and bools live in arrays too.
        ("string[2] w = [\"no\", \"yes\"];\nprintln(w[1]);", "yes"),
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
            format!("{}fn main() {{\n  int[3] xs = [1, 2, 3];\n  println(xs[at()]);\n}}", at("3")),
            bounds,
        ),
        (
            format!(
                "{}fn main() {{\n  int[3] xs = [1, 2, 3];\n  println(xs[at()]);\n}}",
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
            format!("{}fn main() {{\n  string s = \"abc\";\n  println(s[at()]);\n}}", at("3")),
            bounds,
        ),
        (
            format!("{}fn main() {{\n  string s = \"abc\";\n  println(s[at()]);\n}}", at("0 - 1")),
            bounds,
        ),
        // A character counts as one however many bytes it took, so the guard
        // has to be about characters too.
        (
            format!("{}fn main() {{\n  string s = \"éé\";\n  println(s[at()]);\n}}", at("2")),
            bounds,
        ),
        // A list, including the empty one, where every index is out of range.
        (
            format!("{}fn main() {{\n  int[] xs = [1, 2];\n  println(xs[at()]);\n}}", at("2")),
            bounds,
        ),
        (
            format!("{}fn main() {{\n  int[] xs = [];\n  println(xs[at()]);\n}}", at("0")),
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
        (format!("{}fn main() {{\n  println(char(n()));\n}}", number("1114112")), bad_char),
        (format!("{}fn main() {{\n  println(char(n()));\n}}", number("0 - 1")), bad_char),
        (format!("{}fn main() {{\n  println(char(n()));\n}}", number("55296")), bad_char),
        (format!("{}fn main() {{\n  println(char(n()));\n}}", number("57343")), bad_char),
        // `int(s)`: nothing at all, a sign with no digits, a sign in the wrong
        // place, and text that is not digits.
        (format!("{}fn main() {{\n  println(int(t()));\n}}", text("")), not_a_number),
        (format!("{}fn main() {{\n  println(int(t()));\n}}", text("-")), not_a_number),
        (format!("{}fn main() {{\n  println(int(t()));\n}}", text("+12")), not_a_number),
        (format!("{}fn main() {{\n  println(int(t()));\n}}", text("12 ")), not_a_number),
        (format!("{}fn main() {{\n  println(int(t()));\n}}", text("12a")), not_a_number),
        // ... and a number no `int` can hold, which is the same refusal: an
        // answer that had to be truncated would be a wrong one.
        (
            format!("{}fn main() {{\n  println(int(t()));\n}}", text("9223372036854775808")),
            not_a_number,
        ),
        (
            format!("{}fn main() {{\n  println(int(t()));\n}}", text("-9223372036854775809")),
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
        ("println(int(char(n(0))));", "0"),
        ("println(int(char(n(55295))));", "55295"),
        ("println(int(char(n(57344))));", "57344"),
        ("println(int(char(n(1114111))));", "1114111"),
        // The widest numbers an `int` holds, in and back out again. The most
        // negative one has no positive twin, which is what a parser that
        // negated at the end would lose.
        ("println(int(t(\"9223372036854775807\")));", "9223372036854775807"),
        ("println(int(t(\"-9223372036854775808\")));", "-9223372036854775808"),
        ("println(int(t(\"0\")));", "0"),
        ("println(int(t(\"-0\")));", "0"),
    ];

    // The values go through calls so that nothing is a constant `sema` could
    // settle: what is under test is the check in the emitted code.
    let prelude = "fn n(int v) -> int {\n  return v;\n}\n\
                   fn t(string v) -> string {\n  return v;\n}\n";
    harness.each_prints_after("boundary", prelude, &cases);
}

/// `is_int(s)` and `int(s)` have to agree, and this is where that is checked.
///
/// They are one routine asked two ways, so a disagreement would mean the
/// wrapper rather than the parse — but the property is the whole point of the
/// built-in, so it is asserted rather than assumed: every case here converts
/// exactly when it was told it could, and the program must survive it.
#[test]
fn asking_whether_text_is_a_number_agrees_with_converting_it() {
    let Some(harness) = Harness::find() else { return };

    // Through a call, so nothing is a constant `sema` could settle: what is
    // under test is the routine in the emitted code.
    let prelude = "fn t(string v) -> string {\n  return v;\n}\n\
                   fn show(string s) {\n  if (is_int(s)) {\n    println(int(s));\n  } else {\n    \
                   println(\"no\");\n  }\n}\n";

    let cases: [(&str, &str); 3] = [
        (
            "show(t(\"42\"));\nshow(t(\"-42\"));\nshow(t(\"007\"));\nshow(t(\"\"));\n\
             show(t(\"-\"));\nshow(t(\"abc\"));\nshow(t(\"12a\"));\nshow(t(\" 7\"));\n\
             show(t(\"4 \"));\nshow(t(\"+7\"));",
            "42\n-42\n7\nno\nno\nno\nno\nno\nno\nno",
        ),
        // The edge of the range is the one place the two could come apart,
        // because there it is an overflow that decides rather than a character.
        (
            "show(t(\"9223372036854775807\"));\nshow(t(\"9223372036854775808\"));\n\
             show(t(\"-9223372036854775808\"));\nshow(t(\"-9223372036854775809\"));",
            "9223372036854775807\nno\n-9223372036854775808\nno",
        ),
        // The answer is an ordinary `bool` and prints as one.
        ("println(is_int(t(\"1\")));\nprintln(is_int(t(\"x\")));", "true\nfalse"),
    ];

    harness.each_prints_after("isint", prelude, &cases);
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
        ("int n = match (Colour::Red) { Colour::Red => 1, Colour::Green => 2, Colour::Blue => 3, };\nprintln(n);", "1"),
        ("int n = match (Colour::Green) { Colour::Red => 1, Colour::Green => 2, Colour::Blue => 3, };\nprintln(n);", "2"),
        // The last arm is the one no test guards, so it is the one a wrong
        // chain would reach by accident.
        ("int n = match (Colour::Blue) { Colour::Red => 1, Colour::Green => 2, Colour::Blue => 3, };\nprintln(n);", "3"),
        // A block arm that leaves the function: the join is never reached, so
        // the answer is the arm's own `return` and not one of the values.
        ("println(pick(Colour::Green));", "42"),
        // A block arm that leaves a *loop*. If it fell through into the join
        // instead, `n` would be overwritten with whatever the join held.
        (
            "int n = 7;\nwhile (true) {\n  n = match (Colour::Green) {\n    Colour::Red => 1,\n    \
             Colour::Green => { break; }\n    Colour::Blue => 3,\n  };\n}\nprintln(n);",
            "7",
        ),
        // An arm's value may itself be computed, and lands in the same register.
        (
            "int k = 10;\nint n = match (Colour::Blue) {\n  Colour::Red => k + 1,\n  \
             Colour::Green => k * 2,\n  Colour::Blue => k - 4,\n};\nprintln(n);",
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
         fn main() {\n  println(\"hi\");\n  println(printf() + str0());\n}\n",
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
        ("println(0);", "0"),
        ("println(7);", "7"),
        // The widest numbers there are, which is where a renderer that worked
        // digit by digit would run out.
        ("println(9223372036854775807);", "9223372036854775807"),
        ("println(0 - 9223372036854775807 - 1);", "-9223372036854775808"),
        ("println(true);", "true"),
        ("println(false);", "false"),
        ("println(\"text\");", "text"),
        // A character is printed as itself, not as its number.
        ("println('a');", "a"),
        ("println('\u{e9}');", "\u{e9}"),
        // A variant prints its own name, and the last one is the one a table
        // read one entry short would miss.
        ("println(Colour::Red);", "Red"),
        ("println(Colour::Blue);", "Blue"),
        // A value computed rather than written, so nothing was folded away.
        ("int n = 20;\nprintln(n * 5 + 1);", "101"),
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
        ("println(max() - 1 + 1);", "9223372036854775807"),
        ("println(min() + 1 - 1);", "-9223372036854775808"),
        ("println(max() + min());", "-1"),
        ("println(max() - max());", "0"),
        // Multiplication that just fits, and its negative twin.
        ("println(v(4611686018427387903) * 2 + 1);", "9223372036854775807"),
        ("println(v(0 - 4611686018427387904) * 2);", "-9223372036854775808"),
        // Division at the ends. `min() / -1` is the one that overflows and is
        // checked elsewhere; these are the ones next to it.
        ("println(min() / v(1));", "-9223372036854775808"),
        ("println(max() / v(0 - 1));", "-9223372036854775807"),
        ("println(min() / v(2));", "-4611686018427387904"),
        // A remainder that is zero, including for the value with no positive
        // twin — where the division it is paired with would overflow.
        ("println(min() % v(1));", "0"),
        ("println(max() % max());", "0"),
        // Negating the largest positive number is fine; negating the most
        // negative one is not, and is checked elsewhere.
        ("println(-max());", "-9223372036854775807"),
        // Zero absorbs, and must not be mistaken for an overflow.
        ("println(min() * v(0));", "0"),
        // Both ends in one expression, reaching zero through -1 rather than
        // through `0 - min()`, which has no answer at all.
        ("println(min() + max() + 1);", "0"),
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
        ("println(v(5) < v(5));", "false"),
        ("println(v(4) < v(5));", "true"),
        ("println(v(5) <= v(5));", "true"),
        ("println(v(6) <= v(5));", "false"),
        ("println(v(5) > v(5));", "false"),
        ("println(v(6) > v(5));", "true"),
        ("println(v(5) >= v(5));", "true"),
        ("println(v(4) >= v(5));", "false"),
        ("println(v(5) == v(5));", "true"),
        ("println(v(5) != v(5));", "false"),
        // A negative against a positive: unsigned, this answers the opposite.
        ("println(v(0 - 1) < v(1));", "true"),
        ("println(v(0 - 1) > v(1));", "false"),
        // Two negatives, where the larger magnitude is the smaller number.
        ("println(v(0 - 5) < v(0 - 4));", "true"),
        ("println(v(0 - 5) >= v(0 - 4));", "false"),
        // The two ends of the range against each other.
        ("println(v(0 - 9223372036854775807 - 1) < v(9223372036854775807));", "true"),
        ("println(v(9223372036854775807) <= v(0 - 9223372036854775807 - 1));", "false"),
        // Characters and enums answer equality, and only equality.
        ("println('a' == 'a');\nprintln('a' != 'b');", "true\ntrue"),
        ("println(\"ab\" == \"ab\");\nprintln(\"ab\" != \"ba\");", "true\ntrue"),
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
        ("println(four(1, 2, 3, 4));", "1234"),
        // Each argument the result of a call, so every one of them is live
        // across a later call and none may sit where a call destroys it.
        ("println(four(one(0), one(1), one(2), one(3)));", "1234"),
        // A call inside a call's own argument list, at the last position.
        ("println(four(1, 2, 3, one(3)));", "1234"),
        // Returning an aggregate spends an argument register on the hidden
        // address of the caller's room, leaving three — so this is the full
        // house for a function that returns one.
        ("Point p = build(1, 2, 3);\nprintln(p.x * 10 + p.y);", "1023"),
        // The returned object outlives the call that built it, and a second
        // call must not land on top of the first one's room.
        (
            "Point a = build(1, 2, 3);\nPoint b = build(4, 5, 6);\n\
             println(a.x * 10000 + b.x);",
            "1020405",
        ),
        // Recursion deep enough that the frames have to be right hundreds of
        // times over, not just once.
        ("println(depth(500));", "500"),
        ("println(total(1000));", "500500"),
        // A call in a loop condition, evaluated afresh each time round.
        (
            "int n = 0;\nwhile (one(n) < 10) {\n  n = n + 1;\n}\nprintln(n);",
            "9",
        ),
        // Values that must survive a call made between their definition and
        // their use — more of them than there are registers to hold.
        (
            "int a = 1; int b = 2; int c = 3; int d = 4; int e = 5;\n\
             int f = 6; int g = 7; int h = 8;\n\
             println(one(0) + a + b + c + d + e + f + g + h);",
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
        ("nothing();\nprintln(1);", "1"),
        ("nothing_twice();\nprintln(2);", "2"),
        // A `while` whose condition is false from the start.
        ("int n = 0;\nwhile (n > 0) {\n  n = n + 1;\n}\nprintln(n);", "0"),
        // A `for` whose range is empty, including one that never could run.
        ("int n = 0;\nfor (int i = 0; i < 0; i = i + 1) {\n  n = n + 1;\n}\nprintln(n);", "0"),
        ("int n = 0;\nfor (int i = 5; i < 3; i = i + 1) {\n  n = n + 1;\n}\nprintln(n);", "0"),
        // An `if` with no `else`, taken and not taken.
        ("int n = 1;\nif (n > 5) {\n  n = 99;\n}\nprintln(n);", "1"),
        ("int n = 1;\nif (n < 5) {\n  n = 99;\n}\nprintln(n);", "99"),
        // An empty block in the middle of a function, which must not disturb
        // what is around it.
        ("int n = 1;\nif (true) {\n}\nn = n + 1;\nprintln(n);", "2"),
        // A loop that runs exactly once, which is the boundary either side of
        // the two above.
        ("int n = 0;\nfor (int i = 0; i < 1; i = i + 1) {\n  n = n + 1;\n}\nprintln(n);", "1"),
        // A `break` on the first pass, so the body runs once and the loop ends
        // without the condition ever being false.
        ("int n = 0;\nwhile (true) {\n  n = n + 1;\n  break;\n}\nprintln(n);", "1"),
    ];

    harness.each_prints_after("empty", prelude, &cases);
}

/// Format strings, run rather than read.
///
/// The compiler splits a format at compile time and emits one write per piece,
/// so what these check is that the pieces come out in the right order, with the
/// right values in them, and nothing between them.
#[test]
fn a_format_writes_its_pieces_in_order() {
    let Some(harness) = Harness::find() else { return };

    let cases: [(&str, &str); 12] = [
        ("int x = 42;\nprintln(\"Number is: %d\", x);", "Number is: 42"),
        (
            "char g = 'A';\nstring s = \"Toto\";\nprintln(\"Grade: %c for student %s\", g, s);",
            "Grade: A for student Toto",
        ),
        ("println(\"%b and %b\", true, false);", "true and false"),
        // `%%` is one percent sign, and the text around it is not cut in two.
        ("println(\"100%% sure\");", "100% sure"),
        // A specifier at either end, with nothing beside it.
        ("println(\"%d\", 7);", "7"),
        ("int n = 1;\nprintln(\"a%db\", n);", "a1b"),
        // Several of them in a row, with no text between: the pieces must not
        // be merged or reordered.
        ("println(\"%d%d%d\", 1, 2, 3);", "123"),
        // The values are ordinary expressions, evaluated left to right.
        ("int n = 2;\nprintln(\"%d then %d\", n * 10, n + 1);", "20 then 3"),
        // An escape in the format is resolved by the lexer, so the format sees
        // one character and the specifier after it still lines up.
        ("println(\"a\\tb %d\", 5);", "a\tb 5"),
        // A character that takes more than one byte in UTF-8, written as text
        // rather than as a value: the bytes are in the file already.
        ("println(\"café %d\", 1);", "café 1"),
        // And as a value, which goes through the encoder instead.
        ("char c = 'é';\nprintln(\"%c\", c);", "é"),
        // A format is not required: one value on its own is the older form and
        // still works.
        ("println(1 + 1);", "2"),
    ];

    harness.each_prints("format", &cases);
}

/// Enums are written by name under `%e`, which is the only rendering they have.
#[test]
fn a_variant_is_written_by_name() {
    let Some(harness) = Harness::find() else { return };

    let cases: [(&str, &str); 2] = [
        ("Colour c = Colour::Green;\nprintln(\"it is %e\", c);", "it is Green"),
        (
            "println(\"%e then %e\", Colour::Red, Colour::Green);",
            "Red then Green",
        ),
    ];

    harness.each_prints_after("variant", "enum Colour { Red, Green }\n", &cases);
}

/// The whole point of the two spellings: `print` leaves the line open.
///
/// Checked against the *exact* bytes, because trailing whitespace is what every
/// other test in this file normalises away — and here it is the answer.
#[test]
fn print_leaves_the_line_open_and_println_ends_it() {
    let Some(harness) = Harness::find() else { return };

    let cases: [(&str, &str); 6] = [
        ("print(\"a\");\nprint(\"b\");", "ab"),
        ("println(\"a\");\nprintln(\"b\");", "a\nb\n"),
        ("print(1);\nprint(2);", "12"),
        // A line built a piece at a time, which is what `print` is for.
        (
            "print(\"ranks:\");\nfor (int i = 1; i <= 3; i = i + 1) {\n  print(\" %d\", i);\n}\nprintln();",
            "ranks: 1 2 3\n",
        ),
        // `println` with nothing to write is a blank line.
        ("print(\"a\");\nprintln();\nprintln();", "a\n\n"),
        // `print` with nothing to write does nothing at all.
        ("print();\nprint(\"a\");", "a"),
    ];

    for (index, (body, expected)) in cases.iter().enumerate() {
        let source = format!("fn main() {{\n{body}\n}}\n");
        let run = harness.build_and_run(&format!("openline{index}"), &source, b"");
        assert!(run.status.success(), "case {index} exited with {}\n{}", run.status, run.stderr);
        assert_eq!(
            run.stdout.replace("\r\n", "\n"),
            *expected,
            "case {index}, byte for byte:\n{body}"
        );
    }
}
