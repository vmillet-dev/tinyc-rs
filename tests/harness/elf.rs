//! Building on a Unix: `nasm -f elf64`, then the system C compiler as linker.
//!
//! The counterpart of `msvc.rs`, and the half of a Linux port that does not
//! need a compiler backend to exist. It is written now so that the port is one
//! job rather than two: when `codegen` grows an `x86_64-linux` target, the
//! whole execution suite starts running against it with no further edits here.
//!
//! Until then [`find`] answers the same way it would on a machine with no
//! assembler — the tests say what is missing and pass.
//!
//! **Not exercised yet**, for exactly that reason: there is no backend for it
//! to build. Expect the first port to adjust the flags below, `-no-pie` above
//! all, which is what hand-written NASM that names absolute addresses needs.

use std::path::{Path, PathBuf};
use std::process::Command;

use tinyc::codegen::Target;

use super::{Toolchain, step};

/// The target this toolchain builds for, by the name `--target` would know it
/// under. Nothing resolves it today; adding the variant to `codegen::TARGETS`
/// is what turns this file on.
const TARGET: &str = "x86_64-linux";

pub struct Elf {
    target: Target,
    nasm: PathBuf,
    /// The C compiler, used only as a linker: it is what knows where the C
    /// runtime is and which startup object calls `main` — the job `vcvars64`
    /// and `/entry:mainCRTStartup` do on Windows.
    cc: PathBuf,
}

/// Look for what it takes to build, or say what was missing.
pub fn find() -> Result<Box<dyn Toolchain>, String> {
    // The compiler having no backend for this machine is an ordinary answer,
    // not a failure: it is what a half-finished port looks like.
    let target = Target::from_name(TARGET)
        .ok_or_else(|| format!("this compiler has no `{TARGET}` backend"))?;
    let nasm = on_path("nasm").ok_or("nasm not found (apt install nasm)")?;
    let cc = on_path("cc").or_else(|| on_path("gcc")).or_else(|| on_path("clang"));
    let cc = cc.ok_or("no C compiler found to link with (apt install build-essential)")?;
    Ok(Box::new(Elf { target, nasm, cc }))
}

impl Toolchain for Elf {
    fn name(&self) -> String {
        format!("{TARGET} (nasm at {}, cc at {})", self.nasm.display(), self.cc.display())
    }

    fn target(&self) -> Target {
        self.target
    }

    fn build(&self, scratch: &Path, asm: &Path, exe: &Path) -> Result<(), String> {
        let obj = scratch.join(asm.with_extension("o").file_name().expect("a file name"));

        step("nasm", Command::new(&self.nasm).args(["-f", "elf64", "-o"]).arg(&obj).arg(asm))?;

        // `-no-pie`: a position-independent executable needs every address to
        // be reached relative to `rip`, which hand-written assembly naming
        // symbols outright does not do.
        step(
            "cc",
            Command::new(&self.cc).arg("-no-pie").arg(&obj).arg("-o").arg(exe),
        )
    }
}

/// The first thing by that name on `PATH`, or `None`.
///
/// Unix has no `where.exe`, and shelling out to `which` is one more program
/// that may not be there — walking `PATH` is both shorter and surer.
fn on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(program)).find(|candidate| candidate.is_file())
}
