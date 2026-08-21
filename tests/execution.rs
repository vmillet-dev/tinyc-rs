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
const EXAMPLES: [&str; 7] = [
    "hello.tc",
    "arith.tc",
    "spill.tc",
    "reassign.tc",
    "bool.tc",
    "control_flow.tc",
    "functions.tc",
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

    let cases: [(&str, &str); 10] = [
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
    ];

    for (index, (body, expected)) in cases.iter().enumerate() {
        let source = tools.scratch.join(format!("corner{index}.tc"));
        std::fs::write(&source, format!("fn main() {{\n{body}\n}}\n")).unwrap();

        let run = tools.build_and_run(&source, &format!("corner{index}"));
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

    let cases: [(&str, &str); 11] = [
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
    ];

    for (index, (body, expected)) in cases.iter().enumerate() {
        let source = tools.scratch.join(format!("logic{index}.tc"));
        std::fs::write(&source, format!("fn main() {{\n{body}\n}}\n")).unwrap();

        let run = tools.build_and_run(&source, &format!("logic{index}"));
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
        //  - legacy_stdio_definitions.lib : printf as a real symbol rather than
        //                                   the inline function the UCRT headers
        //                                   normally provide
        let linked = Command::new(&self.link)
            .env("LIB", &self.lib)
            .args(["/nologo", "/subsystem:console", "/entry:mainCRTStartup"])
            .arg(format!("/out:{}", exe.display()))
            .arg(&obj)
            .args(["msvcrt.lib", "legacy_stdio_definitions.lib"])
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
