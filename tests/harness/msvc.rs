//! Building on Windows: `nasm -f win64`, then the Microsoft linker.
//!
//! Everything Windows-specific in the test suite is in this file. It is the
//! counterpart of `scripts/build.ps1`, which does the same thing by hand.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use tinyc::codegen::Target;

use super::{Toolchain, step};

/// The target this toolchain builds for, by the name `--target` knows it under.
const TARGET: &str = "x86_64-windows";

pub struct Msvc {
    target: Target,
    nasm: PathBuf,
    link: PathBuf,
    /// The library search path `vcvars64.bat` sets up. Captured once, because
    /// running that batch file per link is slower than everything else here put
    /// together.
    lib: String,
}

/// Look for what it takes to build, or say what was missing.
pub fn find() -> Result<Box<dyn Toolchain>, String> {
    // The compiler having no backend for this machine is an ordinary answer,
    // not a failure: it is what a half-finished port looks like.
    let target = Target::from_name(TARGET)
        .ok_or_else(|| format!("this compiler has no `{TARGET}` backend"))?;
    let nasm = find_nasm().ok_or("nasm not found (winget install nasm)")?;
    let (link, lib) = find_linker().ok_or("no Visual Studio C++ toolchain found")?;
    Ok(Box::new(Msvc { target, nasm, link, lib }))
}

impl Toolchain for Msvc {
    fn name(&self) -> String {
        format!("{TARGET} (nasm at {}, link at {})", self.nasm.display(), self.link.display())
    }

    fn target(&self) -> Target {
        self.target
    }

    fn build(&self, scratch: &Path, asm: &Path, exe: &Path) -> Result<(), String> {
        let obj = scratch.join(asm.with_extension("obj").file_name().expect("a file name"));

        // NASM's win64 output is a COFF object, exactly what link.exe wants.
        step("nasm", Command::new(&self.nasm).args(["-f", "win64", "-o"]).arg(&obj).arg(asm))?;

        // `link` finds the C runtime through `LIB`, which is the one thing a
        // developer command prompt would have set up for it.
        //
        //  - msvcrt.lib                   : the C runtime, where printf lives
        //  - kernel32.lib                 : SetConsoleOutputCP, so a console reads
        //                                   what is printed to it as UTF-8
        //  - legacy_stdio_definitions.lib : printf as a real symbol rather than
        //                                   the inline function the UCRT headers
        //                                   normally provide
        step(
            "link",
            Command::new(&self.link)
                .env("LIB", &self.lib)
                .args(["/nologo", "/subsystem:console", "/entry:mainCRTStartup"])
                .arg(format!("/out:{}", exe.display()))
                .arg(&obj)
                .args(["msvcrt.lib", "kernel32.lib", "legacy_stdio_definitions.lib"]),
        )
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
