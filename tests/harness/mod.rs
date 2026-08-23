//! Everything an execution test needs, and the one place that knows how *this*
//! machine turns assembly into a running process.
//!
//! An execution test is the only kind that can catch a miscompilation: reading
//! assembly cannot tell a `setl` from a `setg`, and only running the program
//! answers what it prints. That makes these tests worth having on every target
//! the compiler grows — so nothing about a table of cases may mention one.
//!
//! The split is:
//!
//! * a table of cases says *what a program should print*. Pure TinyC, no
//!   platform anywhere. This is almost all of the code, and none of it changes
//!   when a target is added.
//! * a [`Toolchain`] says *how to get from assembly text to an executable*, and
//!   which [`Target`] to ask the compiler for. One implementation per platform,
//!   each in its own file.
//! * [`host_toolchain`] picks one. It is the **only** `cfg` in the whole
//!   harness, and the only thing to touch when a platform is added.
//!
//! So adding the Linux backend means giving `elf.rs` the target it should ask
//! for — the case tables come along for free.
//!
//! When the host has no toolchain, or the compiler has no backend for it, every
//! test here says what was missing and passes, so `cargo test` still works on a
//! machine that cannot link. Run them deliberately with:
//!
//! ```text
//! cargo test --test execution -- --nocapture
//! ```
//!
//! and read the "skipped" lines.
#![allow(dead_code)] // Not every test crate that includes this uses all of it.

use std::path::{Path, PathBuf};
use std::process::Command;

use tinyc::codegen::Target;

#[cfg(unix)]
mod elf;
#[cfg(windows)]
mod msvc;

/// Turning assembly text into something the machine will run.
///
/// Deliberately one step rather than "assemble" then "link": how many programs
/// that takes, and what they are called, is exactly the part that differs.
pub trait Toolchain: Send + Sync {
    /// What to call this toolchain when reporting what was used.
    fn name(&self) -> String;

    /// The target to ask `tinyc` for. Answering a [`Target`] rather than a
    /// string is what makes an unknown one impossible to write.
    fn target(&self) -> Target;

    /// Assemble and link `asm` into `exe`, or say why it could not be done.
    ///
    /// `scratch` is a directory to put intermediate files in; a toolchain that
    /// needs an object file on the way puts it there.
    fn build(&self, scratch: &Path, asm: &Path, exe: &Path) -> Result<(), String>;
}

/// The toolchain for the machine these tests are running on, or the reason
/// there is none.
///
/// The only `cfg` in the harness. Both failure modes are ordinary answers, not
/// panics: a machine may have no assembler, and the compiler may have no
/// backend for a machine that does.
fn host_toolchain() -> Result<Box<dyn Toolchain>, String> {
    #[cfg(windows)]
    return msvc::find();
    #[cfg(unix)]
    return elf::find();
    #[cfg(not(any(windows, unix)))]
    return Err(format!("no toolchain is known for {}", std::env::consts::OS));
}

/// A compiled program that has been run.
pub struct Run {
    pub status: std::process::ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

/// A toolchain plus a directory to work in: what a test needs to go from TinyC
/// source to what the program printed.
pub struct Harness {
    toolchain: Box<dyn Toolchain>,
    scratch: PathBuf,
}

impl Harness {
    /// Answers `None` — after saying why — when this machine cannot build.
    ///
    /// Looking a toolchain up costs a second or two and every test wants the
    /// same answer, so it is found once for the whole run.
    pub fn find() -> Option<&'static Harness> {
        static HARNESS: std::sync::OnceLock<Option<Harness>> = std::sync::OnceLock::new();
        HARNESS
            .get_or_init(|| match host_toolchain() {
                Ok(toolchain) => {
                    let scratch = std::env::temp_dir().join("tinyc-execution-tests");
                    std::fs::create_dir_all(&scratch).expect("a directory to build in");
                    println!("building with {}", toolchain.name());
                    Some(Harness { toolchain, scratch })
                }
                Err(why) => {
                    println!("skipped: {why}");
                    None
                }
            })
            .as_ref()
    }

    pub fn target(&self) -> Target {
        self.toolchain.target()
    }

    pub fn scratch(&self) -> &Path {
        &self.scratch
    }

    /// tinyc -> assembler -> linker -> run, answering what the program printed.
    ///
    /// `name` is what the intermediate files are called, so a case that fails
    /// leaves its source and its assembly behind under [`Harness::scratch`] to
    /// be looked at by hand.
    pub fn build_and_run(&self, name: &str, source: &str, input: &[u8]) -> Run {
        let tc = self.scratch.join(format!("{name}.tc"));
        let asm = self.scratch.join(format!("{name}.asm"));
        // The host is the only machine that can run what it builds, so its own
        // suffix is the right one.
        let exe = self.scratch.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        std::fs::write(&tc, source).expect("the scratch directory should be writable");

        let target = self.toolchain.target();
        let compiled = tinyc::with_compiler_stack(|| tinyc::compile(source, target))
            .unwrap_or_else(|errors| panic!("{name} failed to compile: {errors:?}"));
        std::fs::write(&asm, &compiled.asm).expect("the assembly should be writable");

        if let Err(problem) = self.toolchain.build(&self.scratch, &asm, &exe) {
            panic!("{name} did not build:\n{problem}");
        }
        run(&exe, input)
    }

    /// Compile each body as the whole of `main`, run it, and check what it
    /// printed.
    pub fn each_prints(&self, stem: &str, cases: &[(&str, &str)]) {
        self.each_prints_after(stem, "", cases);
    }

    /// The same, with `prelude` — the functions and classes the bodies need —
    /// written above `main`.
    pub fn each_prints_after(&self, stem: &str, prelude: &str, cases: &[(&str, &str)]) {
        for (index, (body, expected)) in cases.iter().enumerate() {
            let name = format!("{stem}{index}");
            let source = format!("{prelude}fn main() {{\n{body}\n}}\n");

            let run = self.build_and_run(&name, &source, b"");
            assert!(
                run.status.success(),
                "case {index} exited with {}\n{}",
                run.status,
                run.stderr
            );
            // Normalised on both sides, so a case may span several lines: what
            // separates them is the platform's business, not the program's.
            assert_eq!(normalise(&run.stdout), normalise(expected), "case {index}:\n{body}");
        }
    }

    /// Run each body on the input written beside it.
    ///
    /// The one thing that cannot be checked by looking at a program alone: what
    /// it does depends on what it is given.
    pub fn each_prints_given(&self, stem: &str, cases: &[(&[u8], &str, &str)]) {
        for (index, (input, body, expected)) in cases.iter().enumerate() {
            let name = format!("{stem}{index}");
            let source = format!("fn main() {{\n{body}\n}}\n");

            let run = self.build_and_run(&name, &source, input);
            assert!(
                run.status.success(),
                "case {index} exited with {}\n{}",
                run.status,
                run.stderr
            );
            assert_eq!(normalise(&run.stdout), normalise(expected), "case {index}:\n{body}");
        }
    }

    /// Each whole program here must *stop*, and say so: these are the
    /// operations with no right answer to hand back, and the check is that none
    /// of them hands one back anyway.
    pub fn each_stops_with(&self, stem: &str, cases: &[(&str, &str)]) {
        for (index, (program, expected)) in cases.iter().enumerate() {
            let name = format!("{stem}{index}");
            let run = self.build_and_run(&name, program, b"");

            assert!(!run.status.success(), "case {index} was expected to fail: {}", run.stdout);
            assert!(
                run.stderr.contains(expected),
                "case {index} should have mentioned `{expected}`, said: {}",
                run.stderr
            );
        }
    }
}

/// Run a built executable on `input`, and collect what it said.
///
/// A program that reads is given exactly these bytes and then the end of the
/// input, which is what makes `eof()` testable at all: run with a terminal
/// attached it would wait for someone to type.
fn run(exe: &Path, input: &[u8]) -> Run {
    let mut child = Command::new(exe)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("{} should run: {e}", exe.display()));
    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().expect("stdin was piped");
        // A program is free to stop before reading all of its input — asking
        // for one line of many, or aborting on the first. The pipe closing
        // under the writer is that, not a failure of the test.
        let _ = stdin.write_all(input);
    }
    let run = child.wait_with_output().expect("the program should finish");
    Run {
        status: run.status,
        stdout: String::from_utf8_lossy(&run.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&run.stderr).into_owned(),
    }
}

/// Run one step of a build, answering the reason it failed rather than panicking.
///
/// Shared by the toolchain implementations, which all shell out.
pub fn step(what: &str, command: &mut Command) -> Result<(), String> {
    let output = command.output().map_err(|e| format!("{what} could not be run: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "{what} failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

/// Where the checked-in TinyC programs live.
pub fn examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("examples")
}

/// The programs that must keep printing exactly what
/// `examples/expected/<name>.txt` says, and the source of truth for what a
/// working TinyC program looks like.
///
/// `examples/hello.tc` is deliberately absent from the *compile* lists that
/// live beside this one: it is a scratch file for trying things out by hand.
/// Here it is included, because what it prints is checked against a file that
/// is edited with it.
pub const EXAMPLES: [&str; 13] = [
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
    "lists.tc",
    "interactive.tc",
];

/// Trailing whitespace and line endings are the shell's business, not the
/// compiler's.
pub fn normalise(text: &str) -> String {
    text.replace("\r\n", "\n").trim_end().to_string() + "\n"
}
