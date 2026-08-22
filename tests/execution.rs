//! Compile each example the whole way and check what it *prints*.
//!
//! Every other test in this repository inspects text: tokens, a tree, an IR
//! dump, a line of assembly. None of them can tell a `setl` from a `setg`, or
//! notice a register clobbered between two instructions that both look right on
//! their own. Running the program is the only check that can, so this is where a
//! miscompilation gets caught.
//!
//! It needs `nasm` and the Microsoft linker. When either is missing the tests
//! report what they could not find and pass, so `cargo test` still works on a
//! machine without a toolchain — run them deliberately with:
//!
//! ```text
//! cargo test --test execution -- --nocapture
//! ```
//!
//! and read the "skipped" lines.
//!
//! The only target is x86_64-windows, so the whole file is Windows-only.
#![cfg(windows)]

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use tinyc::codegen::Target;

/// Programs that must keep printing exactly what `examples/expected/<name>.txt`
/// says, and the source of truth for what a working TinyC program looks like.
const EXAMPLES: [&str; 11] = [
    "hello.tc",
    "arith.tc",
    "spill.tc",
    "reassign.tc",
    "bool.tc",
    "control_flow.tc",
    "functions.tc",
    "enums.tc",
    "arrays.tc",
    "classes.tc",
    "strings.tc",
];

#[test]
fn every_example_prints_what_it_promises() {
    let Some(tools) = Tools::find() else { return };

    for file in EXAMPLES {
        let source = examples().join(file);
        let expected = examples().join("expected").join(file.replace(".tc", ".txt"));
        let expected = std::fs::read_to_string(&expected)
            .unwrap_or_else(|e| panic!("{}: {e}", expected.display()));

        let run = tools.build_and_run(&source, file);
        assert!(run.status.success(), "{file} exited with {}\n{}", run.status, run.stderr);
        assert_eq!(normalise(&run.stdout), normalise(&expected), "{file} printed the wrong thing");
    }
}

/// The cases the backend is most likely to get subtly wrong, each written so
/// that a wrong answer is a wrong *number* rather than a crash.
#[test]
fn the_awkward_corners_of_code_generation_still_compute() {
    let Some(tools) = Tools::find() else { return };

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

    run_each(&tools, "corner", &cases);
}

/// What a string does, checked by running it rather than by reading assembly.
///
/// Every case here answers a *number* or a `true`/`false` wherever it can, so
/// that a mangled character shows up as a wrong count rather than as output
/// that merely looks odd.
#[test]
fn strings_hold_characters_rather_than_bytes() {
    let Some(tools) = Tools::find() else { return };

    let cases: [(&str, &str); 16] = [
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
        // Enough joining to send the arena back to `malloc` for another chunk.
        (r#"string s = "";
            for (int i = 0; i < 5000; i = i + 1) {
              s = s + "0123456789";
            }
            print(len(s));"#, "50000"),
    ];

    run_each(&tools, "string", &cases);
}

/// Compile each body as the whole of `main`, run it, and check what it printed.
fn run_each(tools: &Tools, stem: &str, cases: &[(&str, &str)]) {
    for (index, (body, expected)) in cases.iter().enumerate() {
        let name = format!("{stem}{index}");
        let source = tools.scratch.join(format!("{name}.tc"));
        std::fs::write(&source, format!("fn main() {{\n{body}\n}}\n")).unwrap();

        let run = tools.build_and_run(&source, &name);
        assert!(run.status.success(), "case {index} exited with {}\n{}", run.status, run.stderr);
        assert_eq!(run.stdout.trim(), *expected, "case {index}:\n{body}");
    }
}

/// A division the hardware cannot perform must say so and stop, rather than
/// dying on a hardware exception with nothing printed.
#[test]
fn a_division_that_cannot_be_performed_reports_and_exits() {
    let Some(tools) = Tools::find() else { return };

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

    for (index, (program, expected)) in cases.iter().enumerate() {
        let source = tools.scratch.join(format!("abort{index}.tc"));
        std::fs::write(&source, program).unwrap();

        let run = tools.build_and_run(&source, &format!("abort{index}"));
        assert!(!run.status.success(), "case {index} was expected to fail: {}", run.stdout);
        assert!(
            run.stderr.contains(expected),
            "case {index} should have mentioned `{expected}`, said: {}",
            run.stderr
        );
    }
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
    let Some(tools) = Tools::find() else { return };

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

    for (index, (body, expected)) in cases.iter().enumerate() {
        let source = tools.scratch.join(format!("logic{index}.tc"));
        std::fs::write(&source, format!("fn main() {{\n{body}\n}}\n")).unwrap();

        let run = tools.build_and_run(&source, &format!("logic{index}"));
        assert!(run.status.success(), "case {index} exited with {}\n{}", run.status, run.stderr);
        assert_eq!(run.stdout.trim(), *expected, "case {index}:\n{body}");
    }
}

/// Objects: the layout, the dispatch, and the upcast that makes them useful.
///
/// Getting a field offset or a vtable slot wrong produces a plausible number
/// rather than a crash, so running the thing is the only check that catches it.
#[test]
fn objects_dispatch_on_what_they_are() {
    let Some(tools) = Tools::find() else { return };

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

    for (index, (body, expected)) in cases.iter().enumerate() {
        let source = tools.scratch.join(format!("object{index}.tc"));
        std::fs::write(&source, format!("{prelude}fn main() {{\n{body}\n}}\n")).unwrap();

        let run = tools.build_and_run(&source, &format!("object{index}"));
        assert!(run.status.success(), "case {index} exited with {}\n{}", run.status, run.stderr);
        assert_eq!(run.stdout.trim(), *expected, "case {index}:\n{body}");
    }
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
    let Some(tools) = Tools::find() else { return };

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

    for (index, (body, expected)) in cases.iter().enumerate() {
        let source = tools.scratch.join(format!("poly{index}.tc"));
        std::fs::write(&source, format!("{prelude}fn main() {{\n{body}\n}}\n")).unwrap();

        let run = tools.build_and_run(&source, &format!("poly{index}"));
        assert!(run.status.success(), "case {index} exited with {}\n{}", run.status, run.stderr);
        assert_eq!(run.stdout.trim(), *expected, "case {index}:\n{body}");
    }
}

/// Arrays really read and write the memory they claim to.
///
/// The only feature whose values live outside registers, so this is the one
/// place a wrong offset or a clobbered base register shows up — as a wrong
/// number rather than as anything the assembly looks guilty about.
#[test]
fn arrays_address_the_elements_they_promise() {
    let Some(tools) = Tools::find() else { return };

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

    for (index, (body, expected)) in cases.iter().enumerate() {
        let source = tools.scratch.join(format!("array{index}.tc"));
        std::fs::write(&source, format!("{prelude}fn main() {{\n{body}\n}}\n")).unwrap();

        let run = tools.build_and_run(&source, &format!("array{index}"));
        assert!(run.status.success(), "case {index} exited with {}\n{}", run.status, run.stderr);
        assert_eq!(run.stdout.trim(), *expected, "case {index}:\n{body}");
    }
}

/// An index the compiler could not check must be checked where it lands.
#[test]
fn an_index_out_of_bounds_reports_and_exits() {
    let Some(tools) = Tools::find() else { return };

    let cases = [
        // Past the end, and negative — one unsigned comparison catches both.
        "fn at() -> int {\n  return 3;\n}\n\
         fn main() {\n  int[3] xs = [1, 2, 3];\n  print(xs[at()]);\n}",
        "fn at() -> int {\n  return 0 - 1;\n}\n\
         fn main() {\n  int[3] xs = [1, 2, 3];\n  print(xs[at()]);\n}",
        // Writing is guarded exactly as reading is.
        "fn at() -> int {\n  return 9;\n}\n\
         fn main() {\n  int[3] xs = [1, 2, 3];\n  xs[at()] = 1;\n}",
        // And through a parameter, where the length comes from the type.
        "fn set(int[2] xs, int i) {\n  xs[i] = 1;\n}\n\
         fn main() {\n  int[2] xs = [1, 2];\n  set(xs, 5);\n}",
    ];

    for (index, program) in cases.iter().enumerate() {
        let source = tools.scratch.join(format!("bounds{index}.tc"));
        std::fs::write(&source, program).unwrap();

        let run = tools.build_and_run(&source, &format!("bounds{index}"));
        assert!(!run.status.success(), "case {index} was expected to fail: {}", run.stdout);
        assert!(
            run.stderr.contains("out of bounds"),
            "case {index} should have said so, said: {}",
            run.stderr
        );
    }
}

/// A `match` used as a value, where getting the control flow wrong shows up as
/// a wrong answer rather than a crash.
#[test]
fn a_match_expression_yields_the_arm_that_ran() {
    let Some(tools) = Tools::find() else { return };

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

    for (index, (body, expected)) in cases.iter().enumerate() {
        let source = tools.scratch.join(format!("matchval{index}.tc"));
        std::fs::write(&source, format!("{prelude}fn main() {{\n{body}\n}}\n")).unwrap();

        let run = tools.build_and_run(&source, &format!("matchval{index}"));
        assert!(run.status.success(), "case {index} exited with {}\n{}", run.status, run.stderr);
        assert_eq!(run.stdout.trim(), *expected, "case {index}:\n{body}");
    }
}

/// A TinyC function may be called anything, including the name of the runtime
/// routine `print` itself compiles into.
#[test]
fn a_program_may_name_a_function_after_the_runtime() {
    let Some(tools) = Tools::find() else { return };

    let source = tools.scratch.join("shadow_runtime.tc");
    std::fs::write(
        &source,
        "fn printf() -> int {\n  return 41;\n}\n\
         fn str0() -> int {\n  return 1;\n}\n\
         fn main() {\n  print(\"hi\");\n  print(printf() + str0());\n}\n",
    )
    .unwrap();

    let run = tools.build_and_run(&source, "shadow_runtime");
    assert!(run.status.success(), "exited with {}\n{}", run.status, run.stderr);
    assert_eq!(normalise(&run.stdout), "hi\n42\n");
}

// -- the toolchain ---------------------------------------------------------

fn examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples")
}

/// Trailing whitespace and line endings are the shell's business, not the
/// compiler's.
fn normalise(text: &str) -> String {
    text.replace("\r\n", "\n").trim_end().to_string() + "\n"
}

struct Run {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

/// The external programs `tinyc` stops short of, and a directory to work in.
struct Tools {
    nasm: PathBuf,
    link: PathBuf,
    /// The library search path `vcvars64.bat` sets up. Captured once, because
    /// running that batch file per link is slower than everything else here put
    /// together.
    lib: String,
    scratch: PathBuf,
}

impl Tools {
    /// Answers `None` — after saying why — when the machine cannot link.
    ///
    /// Looking the toolchain up costs a second or two, and every test in this
    /// file wants the same answer, so it is found once for the whole run.
    fn find() -> Option<&'static Tools> {
        static TOOLS: std::sync::OnceLock<Option<Tools>> = std::sync::OnceLock::new();
        TOOLS.get_or_init(Tools::look_for).as_ref()
    }

    fn look_for() -> Option<Tools> {
        let Some(nasm) = find_nasm() else {
            println!("skipped: nasm not found (winget install nasm)");
            return None;
        };
        let Some((link, lib)) = find_linker() else {
            println!("skipped: no Visual Studio C++ toolchain found");
            return None;
        };

        let scratch = std::env::temp_dir().join("tinyc-execution-tests");
        std::fs::create_dir_all(&scratch).expect("a directory to build in");
        Some(Tools { nasm, link, lib, scratch })
    }

    /// tinyc -> nasm -> link -> run, answering what the program printed.
    fn build_and_run(&self, source: &Path, name: &str) -> Run {
        let asm = self.scratch.join(format!("{name}.asm"));
        let obj = self.scratch.join(format!("{name}.obj"));
        let exe = self.scratch.join(format!("{name}.exe"));

        let text = std::fs::read_to_string(source)
            .unwrap_or_else(|e| panic!("{}: {e}", source.display()));
        let compiled = tinyc::with_compiler_stack(|| tinyc::compile(&text, Target::X86_64Windows))
            .unwrap_or_else(|errors| panic!("{name} failed to compile: {errors:?}"));
        std::fs::write(&asm, &compiled.asm).unwrap();

        let assembled = Command::new(&self.nasm)
            .args(["-f", "win64", "-o"])
            .arg(&obj)
            .arg(&asm)
            .output()
            .expect("nasm should run");
        assert!(
            assembled.status.success(),
            "{name} did not assemble:\n{}",
            String::from_utf8_lossy(&assembled.stderr)
        );

        // `link` finds the C runtime through `LIB`, which is the one thing a
        // developer command prompt would have set up for it.
        //
        //  - msvcrt.lib                   : the C runtime, where printf lives
        //  - kernel32.lib                 : SetConsoleOutputCP, so a console reads
        //                                   what is printed to it as UTF-8
        //  - legacy_stdio_definitions.lib : printf as a real symbol rather than
        //                                   the inline function the UCRT headers
        //                                   normally provide
        let linked = Command::new(&self.link)
            .env("LIB", &self.lib)
            .args(["/nologo", "/subsystem:console", "/entry:mainCRTStartup"])
            .arg(format!("/out:{}", exe.display()))
            .arg(&obj)
            .args(["msvcrt.lib", "kernel32.lib", "legacy_stdio_definitions.lib"])
            .output()
            .expect("link should run");
        assert!(
            linked.status.success(),
            "{name} did not link:\n{}{}",
            String::from_utf8_lossy(&linked.stdout),
            String::from_utf8_lossy(&linked.stderr)
        );

        let run = Command::new(&exe).output().expect("the compiled program should run");
        Run {
            status: run.status,
            stdout: String::from_utf8_lossy(&run.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&run.stderr).into_owned(),
        }
    }
}

/// `winget install nasm` does not put it on `PATH`, so look where it lands.
fn find_nasm() -> Option<PathBuf> {
    let mut candidates = vec![PathBuf::from("nasm.exe")];
    for variable in ["LOCALAPPDATA", "ProgramFiles", "ProgramFiles(x86)"] {
        if let Ok(root) = std::env::var(variable) {
            candidates.push(Path::new(&root).join("bin").join("NASM").join("nasm.exe"));
            candidates.push(Path::new(&root).join("NASM").join("nasm.exe"));
        }
    }
    candidates.into_iter().find(|path| {
        path.exists()
            || Command::new(path).arg("-v").output().is_ok_and(|out| out.status.success())
    })
}

/// `link.exe` and the `LIB` path it needs, the way a developer command prompt
/// would provide them.
///
/// `vcvars64.bat` is the only thing that knows where the C runtime's import
/// libraries are, and it only ever announces it by setting environment
/// variables — so it is run once, in a shell that then prints its environment
/// back. See the same dance in `scripts/build.ps1`.
fn find_linker() -> Option<(PathBuf, String)> {
    let program_files = std::env::var("ProgramFiles(x86)").ok()?;
    let vswhere = Path::new(&program_files)
        .join("Microsoft Visual Studio")
        .join("Installer")
        .join("vswhere.exe");
    if !vswhere.exists() {
        return None;
    }

    let found = Command::new(&vswhere)
        .args(["-latest", "-products", "*", "-property", "installationPath"])
        .output()
        .ok()?;
    let root = String::from_utf8_lossy(&found.stdout).trim().to_string();
    if root.is_empty() {
        return None;
    }

    let vcvars = Path::new(&root).join("VC").join("Auxiliary").join("Build").join("vcvars64.bat");
    if !vcvars.exists() {
        return None;
    }

    // `raw_arg`: `cmd.exe` does not parse its command line the way Rust quotes
    // arguments for it, and the quotes around the path have to reach it as
    // written.
    let environment = Command::new("cmd")
        .raw_arg(format!("/c call \"{}\" >nul 2>&1 && set", vcvars.display()))
        .output()
        .ok()?;
    let environment = String::from_utf8_lossy(&environment.stdout);

    let value = |wanted: &str| {
        environment.lines().find_map(|line| {
            let (name, value) = line.split_once('=')?;
            name.eq_ignore_ascii_case(wanted).then(|| value.to_string())
        })
    };

    let lib = value("LIB")?;
    let link = value("PATH")?
        .split(';')
        .map(|directory| Path::new(directory).join("link.exe"))
        .find(|candidate| candidate.exists())?;
    Some((link, lib))
}
