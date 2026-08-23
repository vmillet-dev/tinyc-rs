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

    /// Produce assembly text for an allocated program: one [`Allocation`] per
    /// function, in the same order as [`Program::functions`].
    fn emit(&self, program: &Program, allocations: &[Allocation]) -> String;
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
///
/// Allocation is per function — each one gets its own registers, spill slots
/// and stack frame, and nothing about one function's pressure affects another.
pub fn compile(program: &Program, backend: &dyn Backend) -> (Vec<Allocation>, String) {
    let allocations: Vec<Allocation> = program
        .functions
        .iter()
        .map(|function| regalloc::allocate(function, backend.register_file()))
        .collect();
    let asm = backend.emit(program, &allocations);
    (allocations, asm)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lower a whole program to IR, which is what this stage takes.
    fn ir_of(src: &str) -> Program {
        let tokens = crate::lexer::lex(src).expect("the source should lex");
        let ast = crate::parser::parse(&tokens).expect("the source should parse");
        let backend = backend_for(Target::X86_64Windows);
        let types = crate::sema::check(&ast, backend.register_file().max_args)
            .expect("the source should check");
        crate::ir::lower(&ast, &types)
    }

    #[test]
    fn every_target_answers_to_the_name_it_is_listed_under() {
        // `--target` matches on these strings, and `Target::names` is what the
        // CLI prints when it does not recognise one. A name in the list that
        // does not resolve would be advice that does not work.
        for name in Target::names() {
            assert!(Target::from_name(name).is_some(), "`{name}` is listed but unknown");
        }
        assert_eq!(Target::names().len(), TARGETS.len());
        assert_eq!(Target::from_name("x86_64-windows"), Some(Target::X86_64Windows));
    }

    #[test]
    fn an_unknown_target_is_none_rather_than_a_default() {
        // A default would silently compile for something other than what was
        // asked for.
        assert_eq!(Target::from_name(""), None);
        assert_eq!(Target::from_name("x86_64-linux"), None);
        assert_eq!(Target::from_name("X86_64-WINDOWS"), None, "the match is exact");
    }

    #[test]
    fn a_backend_describes_a_register_file_the_allocator_can_work_from() {
        let backend = backend_for(Target::X86_64Windows);
        let registers = backend.register_file();

        assert!(!backend.name().is_empty());
        assert!(registers.max_args > 0, "a target has to pass at least one argument");
        assert!(
            !registers.caller_saved.is_empty() || !registers.callee_saved.is_empty(),
            "the allocator needs something to hand out"
        );
        // Every allocatable register has to be nameable, and belong to exactly
        // one pool: one in both would be handed out twice.
        for reg in registers.caller_saved.iter().chain(&registers.callee_saved) {
            assert!(!registers.name(*reg).is_empty(), "{reg:?} has no name");
        }
        for reg in &registers.caller_saved {
            assert!(!registers.callee_saved.contains(reg), "{reg:?} is in both pools");
        }
    }

    #[test]
    fn allocation_is_per_function_and_in_the_programs_own_order() {
        // The backend pairs `allocations[i]` with `functions[i]` by position
        // and nothing else, so the two lists have to agree.
        let ir = ir_of(
            "fn one() -> int {\n  return 1;\n}\n\
             fn two(int a) -> int {\n  return a + one();\n}\n\
             fn main() {\n  print(two(2));\n}\n",
        );
        let backend = backend_for(Target::X86_64Windows);
        let (allocations, asm) = compile(&ir, backend.as_ref());

        assert_eq!(allocations.len(), ir.functions.len());
        for (function, allocation) in ir.functions.iter().zip(&allocations) {
            assert!(
                regalloc::verify(allocation, backend.register_file()).is_ok(),
                "{} was allocated inconsistently",
                function.signature(&ir.table)
            );
        }
        assert!(asm.contains(backend.name()), "the header names the target: {asm}");
    }

    #[test]
    fn nothing_about_one_functions_pressure_reaches_another() {
        // Each function gets its own registers, spill slots and frame, which is
        // what lets them be allocated in any order at all.
        let alone = ir_of("fn main() {\n  int a = 1;\n  print(a);\n}\n");
        let crowded = ir_of(
            "fn heavy() -> int {\n  \
               int a = 1; int b = 2; int c = 3; int d = 4; int e = 5;\n  \
               int f = 6; int g = 7; int h = 8; int i = 9; int j = 10;\n  \
               return a - b - c - d - e - f - g - h - i - j;\n}\n\
             fn main() {\n  int a = 1;\n  print(a);\n}\n",
        );
        let backend = backend_for(Target::X86_64Windows);

        let (alone, _) = compile(&alone, backend.as_ref());
        let (crowded, _) = compile(&crowded, backend.as_ref());
        let main_alone = alone.last().expect("main");
        let main_crowded = crowded.last().expect("main");

        assert_eq!(main_alone.spill_slots, main_crowded.spill_slots);
        assert_eq!(main_alone.used_callee_saved, main_crowded.used_callee_saved);
    }
}
