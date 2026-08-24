//! What every target emits is *assembly*, and the assembler is the judge.
//!
//! `tests/targets.rs` checks that each backend emits something, and
//! `tests/execution.rs` checks that what the host's backend emits does the
//! right thing when it runs. Between them is a gap exactly the shape of a
//! cross-compiler: the assembly for the target this machine is *not* is never
//! looked at by anything that could object to it. A misspelled mnemonic, an
//! addressing mode x86 does not have, a local label nobody defined — all of it
//! sails through both suites and is only found by whoever first tries to build
//! on the other machine.
//!
//! NASM assembles for either format from either machine, so it can close that
//! gap here. Every example, for every target, through the assembler — which
//! answers the syntax question completely, and says nothing at all about
//! whether the program is *correct*. That is still `execution.rs`'s job, and it
//! still only runs for the host.
//!
//! Without NASM the tests say so and pass, exactly like the execution suite —
//! and CI sets [`harness::REQUIRE`], where a missing assembler is a failure.

mod harness;

use std::path::{Path, PathBuf};
use std::process::Command;

use tinyc::codegen::Target;

/// The object format NASM should be asked for, per target.
///
/// Listed rather than derived from the name, so that a new target has to say
/// which format it assembles to instead of silently matching a prefix.
fn object_format(target: &str) -> &'static str {
    match target {
        "x86_64-windows" => "win64",
        "x86_64-linux" => "elf64",
        other => panic!(
            "`{other}` is a target but this file does not know what object format it wants. \
             Add it to `object_format`."
        ),
    }
}

/// The assembler, or the reason there is none. Found once for the whole run.
fn assembler() -> Option<&'static PathBuf> {
    static NASM: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    NASM.get_or_init(|| match harness::nasm() {
        Some(path) => {
            println!("assembling with {}", path.display());
            Some(path)
        }
        None => {
            assert!(
                std::env::var_os(harness::REQUIRE).is_none(),
                "{} is set, so a missing assembler is a failure",
                harness::REQUIRE
            );
            println!("skipped: nasm not found");
            None
        }
    })
    .as_ref()
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join("tinyc-assembles");
    std::fs::create_dir_all(&dir).expect("a directory to assemble in");
    dir
}

/// Every example, for every target, is assembly the assembler accepts.
#[test]
fn every_target_emits_assembly_the_assembler_accepts() {
    let Some(nasm) = assembler() else { return };
    let scratch = scratch();
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");

    let mut checked = 0;
    for name in Target::names() {
        let target = Target::from_name(name).expect("a listed target resolves");
        let format = object_format(name);

        for file in harness::EXAMPLES {
            let source = std::fs::read_to_string(examples.join(file))
                .unwrap_or_else(|e| panic!("{file}: {e}"));
            let compiled = tinyc::with_compiler_stack(|| tinyc::compile(&source, target))
                .unwrap_or_else(|errors| panic!("{file} failed for {name}: {errors:?}"));

            let stem = format!("{name}-{}", file.trim_end_matches(".tc"));
            let asm = scratch.join(format!("{stem}.asm"));
            let obj = scratch.join(format!("{stem}.o"));
            std::fs::write(&asm, &compiled.asm).expect("the assembly should be writable");

            let ran = Command::new(nasm)
                .args(["-f", format, "-o"])
                .arg(&obj)
                .arg(&asm)
                .output()
                .unwrap_or_else(|e| panic!("nasm should run: {e}"));

            assert!(
                ran.status.success(),
                "{name}/{file} is not assembly nasm accepts (-f {format}):\n{}{}\n\
                 the listing is at {}",
                String::from_utf8_lossy(&ran.stdout),
                String::from_utf8_lossy(&ran.stderr),
                asm.display()
            );
            // NASM warns rather than fails on a good deal — a truncated
            // immediate, a size it had to guess — and every one of those is a
            // wrong instruction waiting to happen.
            assert!(
                ran.stderr.is_empty(),
                "{name}/{file} assembled with warnings:\n{}",
                String::from_utf8_lossy(&ran.stderr)
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "no example was assembled for any target");
}
