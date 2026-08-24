//! Building on a Unix: `nasm -f elf64`, then the system C compiler as linker.
//!
//! The counterpart of `msvc.rs`. This file was written before the backend it
//! builds for existed, so that the port would be one job rather than two — and
//! it worked: when `codegen` grew its `x86_64-linux` target the whole execution
//! suite started running against it, and the only thing that needed saying here
//! was this paragraph.
//!
//! When the machine has no assembler [`find`] says so and the tests pass, the
//! same way they do on a Windows box without Visual Studio.

use std::path::{Path, PathBuf};
use std::process::Command;

use tinyc::codegen::Target;

use super::{Toolchain, step};

/// The target this toolchain builds for, by the name `--target` knows it under.
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

        // `-no-pie`: a position-independent executable reaches every symbol
        // through the GOT or the PLT, and assembly that names them outright
        // does not. Most distributions link PIE by default, so this is the one
        // flag a TinyC program cannot be built without — `scripts/build.sh`
        // passes it too.
        //
        // `-lpthread`: `pthread_getattr_np` is how the prologue's stack check
        // finds out where the stack ends. Since glibc 2.34 it is in libc proper
        // and this asks for nothing; on anything older it is where the symbol
        // lives.
        step(
            "cc",
            Command::new(&self.cc)
                .arg("-no-pie")
                .arg(&obj)
                .arg("-o")
                .arg(exe)
                .arg("-lpthread"),
        )
    }
}

/// The first thing by that name on `PATH`, or `None`.
///
/// Unix has no `where.exe`, and shelling out to `which` is one more program
/// that may not be there — walking `PATH` is both shorter and surer.
pub(super) fn on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(program)).find(|candidate| candidate.is_file())
}
