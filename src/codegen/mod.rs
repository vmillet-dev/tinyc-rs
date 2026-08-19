//! Stage 5: IR -> machine code.
//!
//! The stage is split in two so that only the last part is target-specific:
//!
//! * [`regalloc`] assigns machine registers, driven by the [`RegisterFile`] the
//!   backend describes. It knows nothing about x86.
//! * A [`Backend`] turns the IR plus that assignment into assembly text.
//!
//! Adding a target means writing one new module implementing [`Backend`] and
//! adding a variant to [`Target`]; nothing else in the compiler changes.

pub mod regalloc;
pub mod x64_win;

use crate::ir::Program;
pub use regalloc::{Allocation, Location, PhysReg, RegisterFile};

pub trait Backend {
    /// Target triple-ish name, used in the assembly header.
    fn name(&self) -> &'static str;

    /// The machine registers the allocator may hand out.
    fn register_file(&self) -> &RegisterFile;

    /// Produce assembly text for an allocated program.
    fn emit(&self, program: &Program, allocation: &Allocation) -> String;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    X86_64Windows,
}

/// Every target the compiler can emit, as accepted by `--target`.
pub const TARGETS: &[(&str, Target)] = &[("x86_64-windows", Target::X86_64Windows)];

impl Target {
    pub fn from_name(name: &str) -> Option<Target> {
        TARGETS.iter().find(|(n, _)| *n == name).map(|(_, t)| *t)
    }

    pub fn names() -> Vec<&'static str> {
        TARGETS.iter().map(|(name, _)| *name).collect()
    }
}

pub fn backend_for(target: Target) -> Box<dyn Backend> {
    match target {
        Target::X86_64Windows => Box::new(x64_win::X64Windows::new()),
    }
}

/// Run the whole code generation stage: allocate registers, then emit.
pub fn compile(program: &Program, target: Target) -> (Box<dyn Backend>, Allocation, String) {
    let backend = backend_for(target);
    let allocation = regalloc::allocate(program, backend.register_file());
    let asm = backend.emit(program, &allocation);
    (backend, allocation, asm)
}
