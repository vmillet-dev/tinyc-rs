//! The command line itself: what `tinyc` writes, what it prints, and what it
//! says when it will not.
//!
//! `src/main.rs` is the one module no other test reaches. Everything else goes
//! through `tinyc::compile`, which skips the argument parsing, the output path,
//! the exit code and the rendered diagnostic — that is, everything a person
//! actually interacts with.
//!
//! Nothing here needs an assembler: `tinyc` stops at assembly text, so the
//! whole file runs on any machine, for any target the compiler lists.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tinyc::codegen::Target;

/// The binary Cargo just built. Not a path guessed from `target/`, which would
/// be wrong under `--release` and wrong again for a cross build.
const TINYC: &str = env!("CARGO_BIN_EXE_tinyc");

/// A directory of this test's own, so tests that write files cannot collide
/// while the runner has them going at once.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tinyc-cli-tests").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a directory to work in");
    dir
}

/// Write a source file into `dir` and answer its path.
fn source_in(dir: &Path, name: &str, text: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, text).expect("the scratch directory should be writable");
    path
}

struct Ran {
    output: Output,
    stdout: String,
    stderr: String,
}

impl Ran {
    fn succeeded(&self) -> bool {
        self.output.status.success()
    }
}

fn tinyc<S: AsRef<std::ffi::OsStr>>(args: &[S]) -> Ran {
    let output = Command::new(TINYC).args(args).output().expect("tinyc should run");
    Ran {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        output,
    }
}

const HELLO: &str = "fn main() {\n  println(\"hi\");\n}\n";

// -- what it writes --------------------------------------------------------

/// With no `-o`, the assembly lands beside the source with its suffix changed.
#[test]
fn assembly_goes_beside_the_source_by_default() {
    let dir = scratch("default_output");
    let source = source_in(&dir, "hello.tc", HELLO);

    let ran = tinyc(&[source.as_os_str()]);
    assert!(ran.succeeded(), "{}{}", ran.stdout, ran.stderr);

    let written = dir.join("hello.asm");
    assert!(written.exists(), "nothing was written to {}", written.display());
    let asm = std::fs::read_to_string(&written).unwrap();
    assert!(asm.contains("global main"), "{asm}");
    // It says where it put it, which is the only way to find out without looking.
    assert!(ran.stdout.contains("hello.asm"), "{}", ran.stdout);
}

#[test]
fn an_explicit_output_path_is_used_as_given() {
    let dir = scratch("explicit_output");
    let source = source_in(&dir, "hello.tc", HELLO);
    let out = dir.join("somewhere-else.s");

    let ran = tinyc(&[source.as_os_str(), "-o".as_ref(), out.as_os_str()]);
    assert!(ran.succeeded(), "{}{}", ran.stdout, ran.stderr);
    assert!(out.exists(), "nothing was written to {}", out.display());
    assert!(!dir.join("hello.asm").exists(), "it wrote the default path as well");
}

/// A directory that does not exist yet is created rather than reported.
///
/// `-o out/hello.asm` is the first thing anyone types, and `out/` is in
/// `.gitignore` — so on a fresh clone it is never there.
#[test]
fn the_output_directory_is_created_when_it_is_missing() {
    let dir = scratch("nested_output");
    let source = source_in(&dir, "hello.tc", HELLO);
    let out = dir.join("build").join("nested").join("hello.asm");

    let ran = tinyc(&[source.as_os_str(), "-o".as_ref(), out.as_os_str()]);
    assert!(ran.succeeded(), "{}{}", ran.stdout, ran.stderr);
    assert!(out.exists(), "{} was not created", out.display());
}

/// An output path with no directory part at all must not be mistaken for one
/// that needs a directory created.
#[test]
fn a_bare_output_name_is_written_where_it_was_asked_for() {
    let dir = scratch("bare_output");
    // Named relative to `dir` below, so the path `tinyc` sees has no directory
    // part at all — which is the case being checked.
    source_in(&dir, "hello.tc", HELLO);

    let output = Command::new(TINYC)
        .current_dir(&dir)
        .args(["hello.tc", "-o", "out.asm"])
        .output()
        .expect("tinyc should run");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(dir.join("out.asm").exists());
}

// -- stopping after a stage ------------------------------------------------

/// `--emit` prints a stage and writes nothing: asking to see the tokens is not
/// asking for a build.
#[test]
fn each_emit_prints_its_stage_and_writes_no_assembly() {
    let dir = scratch("emit");
    let source = source_in(&dir, "hello.tc", "fn main() {\n  println(1 + 2);\n}\n");

    // Each stage, with something *only* that stage's output could contain — so
    // that printing the wrong one is caught rather than passing on a substring
    // they happen to share.
    let cases = [
        // A token kind, which nothing downstream prints.
        ("tokens", "KwFn"),
        // The tree still holds both operands; by the IR they are folded to 3.
        ("ast", "int 1"),
        // A basic-block label, which only three-address code has.
        ("ir", "entry0"),
    ];
    for (stage, marker) in cases {
        let ran = tinyc(&[source.as_os_str(), "--emit".as_ref(), stage.as_ref()]);
        assert!(ran.succeeded(), "--emit {stage}: {}{}", ran.stdout, ran.stderr);
        assert!(!ran.stdout.is_empty(), "--emit {stage} printed nothing");
        assert!(
            ran.stdout.contains(marker),
            "--emit {stage} should have mentioned `{marker}`, printed:\n{}",
            ran.stdout
        );
        assert!(
            !dir.join("hello.asm").exists(),
            "--emit {stage} wrote assembly, which is not what it was asked for"
        );
    }

    // ... and the default does build.
    let ran = tinyc(&[source.as_os_str()]);
    assert!(ran.succeeded(), "{}{}", ran.stderr, ran.stdout);
    assert!(dir.join("hello.asm").exists());
}

/// `--emit tokens` on a program that does not type-check still prints the
/// tokens: the stage it was asked for succeeded, and the later one never ran.
#[test]
fn an_early_emit_does_not_wait_for_a_later_stage_to_fail() {
    let dir = scratch("emit_early");
    // Lexes and parses; `main` may not take a parameter, so `sema` refuses it.
    let source = source_in(&dir, "bad.tc", "fn main(int a) {\n  println(a);\n}\n");

    let ran = tinyc(&[source.as_os_str(), "--emit".as_ref(), "tokens".as_ref()]);
    assert!(ran.succeeded(), "the tokens are there to print: {}", ran.stderr);
    assert!(ran.stdout.contains("KwFn"), "{}", ran.stdout);

    // The stage that does depend on the type checker cannot say the same.
    let ran = tinyc(&[source.as_os_str(), "--emit".as_ref(), "ir".as_ref()]);
    assert!(!ran.succeeded(), "there is no IR for a program that does not check");
    assert!(!ran.stderr.is_empty(), "it should say why");
}

#[test]
fn dump_regalloc_names_registers_and_still_writes_the_assembly() {
    let dir = scratch("regalloc");
    // `x` comes back from a call, so the optimiser cannot fold it away and
    // there is really a value to allocate a register to. Written as a literal
    // there would be nothing left in the dump to name.
    let source = source_in(
        &dir,
        "hello.tc",
        "fn one() -> int {\n  return 1;\n}\nfn main() {\n  int x = one();\n  println(x + 1);\n}\n",
    );

    let ran = tinyc(&[source.as_os_str(), "--dump-regalloc".as_ref()]);
    assert!(ran.succeeded(), "{}{}", ran.stdout, ran.stderr);
    assert!(ran.stdout.contains("fn main()"), "the dump names the function: {}", ran.stdout);
    assert!(ran.stdout.contains("%x"), "the dump names the value: {}", ran.stdout);
    assert!(dir.join("hello.asm").exists(), "the build still happened");
}

/// `--no-optimise` is what makes the passes readable: the same program, twice,
/// and the difference is exactly what they did.
#[test]
fn no_optimise_hands_the_backend_what_lowering_produced() {
    let dir = scratch("no_optimise");
    let source = source_in(
        &dir,
        "arith.tc",
        "fn main() {\n  int a = 6;\n  int b = 7;\n  println(a + b);\n}\n",
    );

    let emit_ir: [&std::ffi::OsStr; 2] = ["--emit".as_ref(), "ir".as_ref()];
    let raw = tinyc(&[source.as_os_str(), emit_ir[0], emit_ir[1], "--no-optimise".as_ref()]);
    assert!(raw.succeeded(), "{}{}", raw.stdout, raw.stderr);
    assert!(raw.stdout.contains("add %a, %b"), "the addition should survive:\n{}", raw.stdout);

    let optimised = tinyc(&[source.as_os_str(), emit_ir[0], emit_ir[1]]);
    assert!(optimised.succeeded(), "{}{}", optimised.stdout, optimised.stderr);
    assert!(optimised.stdout.contains("print int 13"), "{}", optimised.stdout);
    assert!(!optimised.stdout.contains("add"), "{}", optimised.stdout);

    // And it is a compiler flag, not an inspection one: it changes the build.
    let built = tinyc(&[source.as_os_str(), "--no-optimise".as_ref()]);
    assert!(built.succeeded(), "{}{}", built.stdout, built.stderr);
    let asm = std::fs::read_to_string(dir.join("arith.asm")).expect("the assembly");
    assert!(asm.contains("add "), "the unoptimised build still adds:\n{asm}");
}

// -- targets ---------------------------------------------------------------

/// Every target the compiler lists can be asked for by name on the command
/// line, and says which one it built for.
///
/// The CLI's half of the claim `tests/targets.rs` makes about the library: a
/// backend that exists but cannot be selected is not a target anyone can use.
#[test]
fn every_listed_target_can_be_asked_for() {
    let dir = scratch("targets");
    let source = source_in(&dir, "hello.tc", HELLO);

    for name in Target::names() {
        let out = dir.join(format!("{name}.asm"));
        let ran =
            tinyc(&[source.as_os_str(), "--target".as_ref(), name.as_ref(), "-o".as_ref(), out.as_os_str()]);
        assert!(ran.succeeded(), "--target {name}: {}{}", ran.stdout, ran.stderr);
        assert!(out.exists(), "--target {name} wrote nothing");
        assert!(ran.stdout.contains(name), "it should say what it built for: {}", ran.stdout);
    }
}

/// An unknown target is refused, and the refusal lists the ones that would have
/// worked.
///
/// A default would be the worst possible answer: silently compiling for
/// something other than what was asked for.
#[test]
fn an_unknown_target_is_refused_with_the_known_ones_listed() {
    let dir = scratch("unknown_target");
    let source = source_in(&dir, "hello.tc", HELLO);

    let ran = tinyc(&[source.as_os_str(), "--target".as_ref(), "vax-bsd".as_ref()]);
    assert!(!ran.succeeded(), "an unknown target must not build");
    assert!(ran.stderr.contains("vax-bsd"), "it should name what was asked for: {}", ran.stderr);
    for name in Target::names() {
        assert!(ran.stderr.contains(name), "`{name}` should be offered: {}", ran.stderr);
    }
    assert!(!dir.join("hello.asm").exists(), "it wrote assembly for a target it does not have");
}

// -- refusing ---------------------------------------------------------------

#[test]
fn a_missing_input_file_is_reported_by_name() {
    let dir = scratch("missing_input");
    let missing = dir.join("not-here.tc");

    let ran = tinyc(&[missing.as_os_str()]);
    assert!(!ran.succeeded());
    assert!(ran.stderr.contains("not-here.tc"), "it should say which file: {}", ran.stderr);
}

/// A program that does not compile exits non-zero, says so on *stderr*, and
/// leaves no half-written assembly behind.
#[test]
fn a_program_that_does_not_compile_reports_and_writes_nothing() {
    let dir = scratch("refused");
    let source = source_in(&dir, "bad.tc", "fn main() {\n  println(nope);\n}\n");

    let ran = tinyc(&[source.as_os_str()]);
    assert!(!ran.succeeded(), "it should not have succeeded");
    assert!(ran.stdout.is_empty(), "a refusal belongs on stderr: {}", ran.stdout);
    assert!(ran.stderr.contains("error:"), "{}", ran.stderr);
    assert!(ran.stderr.contains("nope"), "the message names the mistake: {}", ran.stderr);
    assert!(!dir.join("bad.asm").exists(), "assembly was written for a program that was refused");
}

/// The rendered diagnostic points into the file *as it was named on the command
/// line*, with a line, a column and a caret.
///
/// This is the whole reason spans carry offsets rather than text: nothing but
/// the CLI knows what the file was called.
#[test]
fn a_diagnostic_names_the_file_and_points_into_it() {
    let dir = scratch("rendered");
    let source = source_in(&dir, "typo.tc", "fn main() {\n  int x = 1;\n  println(y);\n}\n");

    let ran = tinyc(&[source.as_os_str()]);
    assert!(!ran.succeeded());
    assert!(ran.stderr.contains("typo.tc:3:11"), "line and column: {}", ran.stderr);
    assert!(ran.stderr.contains("println(y);"), "the source line is echoed: {}", ran.stderr);
    assert!(ran.stderr.contains('^'), "a caret marks the spot: {}", ran.stderr);
}

/// A source file whose mistakes are found by different stages is still one
/// refusal with one exit code.
#[test]
fn each_stage_reports_through_the_same_path() {
    let dir = scratch("stages");
    let cases = [
        ("lex.tc", "fn main() {\n  println(@);\n}\n"),
        ("parse.tc", "fn main() {\n  println(1\n}\n"),
        ("check.tc", "fn main() {\n  int x = \"text\";\n}\n"),
    ];
    for (name, text) in cases {
        let source = source_in(&dir, name, text);
        let ran = tinyc(&[source.as_os_str()]);
        assert!(!ran.succeeded(), "{name} should have been refused");
        assert!(ran.stderr.contains("error:"), "{name}: {}", ran.stderr);
        assert!(ran.stderr.contains(name), "{name}: the file is named: {}", ran.stderr);
    }
}

/// A program nested past the limit is a diagnostic from the real binary, not a
/// crash.
///
/// `tinyc::STACK_SIZE` and `parser::MAX_NESTING` are one bargain, and
/// `tests/pipeline.rs` checks it on a thread it builds itself. Only here is it
/// checked on the stack the CLI actually gives it — which is the one that
/// matters, because a stack overflow kills the process without a word.
#[test]
fn deep_nesting_is_a_diagnostic_from_the_binary_too() {
    let dir = scratch("deep");
    let depth = 20_000;
    let text = format!("fn main() {{ println(1{}); }}\n", " + 1".repeat(depth));
    let source = source_in(&dir, "deep.tc", &text);

    let ran = tinyc(&[source.as_os_str()]);
    assert!(!ran.succeeded(), "a chain {depth} deep should be refused");
    assert!(
        ran.stderr.contains("nests too deeply"),
        "it must say why rather than die: {}",
        ran.stderr
    );
}

/// A program right at the edge of what is allowed still compiles, on the stack
/// the CLI gives it and not a larger one.
#[test]
fn nesting_just_under_the_limit_still_builds_from_the_binary() {
    let dir = scratch("deep_ok");
    let depth = tinyc::parser::MAX_NESTING as usize - 8;
    let text = format!("fn main() {{ println(1{}); }}\n", " + 1".repeat(depth));
    let source = source_in(&dir, "deep_ok.tc", &text);

    let ran = tinyc(&[source.as_os_str()]);
    assert!(ran.succeeded(), "{depth} deep should compile: {}", ran.stderr);
    assert!(dir.join("deep_ok.asm").exists());
}

// -- the usual courtesies ---------------------------------------------------

#[test]
fn help_and_version_answer_without_a_source_file() {
    let help = tinyc(&["--help"]);
    assert!(help.succeeded(), "{}", help.stderr);
    assert!(help.stdout.contains("--target"), "{}", help.stdout);
    assert!(help.stdout.contains("--emit"), "{}", help.stdout);

    let version = tinyc(&["--version"]);
    assert!(version.succeeded(), "{}", version.stderr);
    assert!(version.stdout.contains(env!("CARGO_PKG_VERSION")), "{}", version.stdout);
}

#[test]
fn no_arguments_at_all_is_an_error_that_explains_itself() {
    let ran = tinyc::<&str>(&[]);
    assert!(!ran.succeeded(), "an input file is required");
    assert!(ran.stderr.contains("Usage") || ran.stderr.contains("usage"), "{}", ran.stderr);
}
