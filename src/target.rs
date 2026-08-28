//! What the front end has to be told about the machine it is building for.
//!
//! Everything in here is decided **before** a register is chosen, which is what
//! separates it from [`crate::codegen::RegisterFile`]: how big a word is
//! decides what `sema` lays a class out as and what offset the lowering emits,
//! long before anything knows which register anything lands in.
//!
//! It exists because those facts used to be the literal `8` written in nine
//! places — in `size_of`, in the vtable pointer's width, in an enum's tag, in a
//! string's header, in an array's element scale. Every one of them was the same
//! fact about x86-64, and none of them said so. A 32-bit target is now a
//! different [`Layout`] rather than a search for the eights that meant *this*
//! eight.
//!
//! ## What is not here
//!
//! Two constants look like they belong and do not.
//!
//! * [`crate::ir::CHAR_BYTES`] is four because a Unicode scalar value runs to
//!   `0x10FFFF`. That is true of every machine there is.
//! * [`crate::sema::MAX_OBJECT_BYTES`] and [`crate::sema::MAX_ARRAY_LEN`] are
//!   limits the *language* sets, so that a mistake is a diagnostic rather than
//!   a program that cannot run. A bigger machine does not make them bigger.
//!
//! ## What a second machine would still have to answer
//!
//! Naming the sizes is most of the way to a second architecture and not all of
//! it. Three things in the IR are decisions rather than descriptions, and a
//! backend for something other than x86-64 would meet them:
//!
//! * **A float travels in a general register.** [`crate::ir::Num`] says so, and
//!   the register allocator has one class of register because of it. On a
//!   machine with its own floating-point registers that is a `fmv` at each end
//!   of every float instruction — correct, and a cost. Making it free means a
//!   second register class in [`crate::codegen::regalloc`], which is the one
//!   change here that is not a matter of writing a new backend module.
//! * **Arithmetic that does not fit stops the program.** The x86 backend reads
//!   a flag the instruction already set; a machine without one has to work the
//!   overflow out. That is a backend's problem, and the IR says only *that* it
//!   can fail — see [`crate::ir::Instr::can_fail`].
//! * **An argument arrives in a register.** [`crate::ir::Instr::Param`] names
//!   the argument by index and [`Machine::max_args`] is how many there are, so
//!   a target with fewer registers to pass in reports fewer. A target that
//!   passed arguments on the stack would need that instruction to say where,
//!   which is the one piece of ABI the IR does not currently carry.

/// The sizes the front end settles, and every stage after it reads back out of
/// [`crate::ast::TypeTable`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// Bytes a value that fits in a register takes, and therefore the width of
    /// everything the IR calls a word: a field of scalar type, an array
    /// element, a pointer, an enum's tag, a string's length.
    pub word: u32,
    /// Bytes of frame one function's locals may take.
    ///
    /// The stack is the one resource a program is handed rather than asking
    /// for, and it is the smaller of the two machines that decides how much: a
    /// Windows thread gets a megabyte by default, a Linux one eight. A function
    /// wanting a quarter of the smaller is not one that will run and be
    /// unlucky, it is one that cannot recurse at all — and the compiler can see
    /// that before it emits a `sub rsp` no stack could satisfy.
    pub max_frame: u32,
}

impl Layout {
    /// Every 64-bit target the compiler has: eight-byte words, and a frame
    /// bounded by the smaller of the two stacks a thread is given.
    pub const LP64: Layout = Layout { word: 8, max_frame: 256 * 1024 };

    /// Bytes of header in front of a string's characters, holding the count.
    pub fn str_header(self) -> u32 {
        self.word
    }

    /// Bytes of header in front of a list's elements, likewise.
    pub fn list_header(self) -> u32 {
        self.word
    }

    /// Bytes of method table pointer at the front of an object with methods.
    pub fn vptr(self) -> u32 {
        self.word
    }

    /// Bytes of tag in front of an enum variant's payload.
    pub fn tag(self) -> u32 {
        self.word
    }
}

/// Everything about a target the front end and the lowering need.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Machine {
    pub layout: Layout,
    /// How many arguments this target passes in registers, and therefore the
    /// most parameters a function may declare.
    ///
    /// It lives here rather than in [`crate::sema`] because it is an ABI fact,
    /// not a language one: the type checker enforces the number the target
    /// reports instead of hard-coding one backend's answer.
    pub max_args: usize,
}

#[cfg(test)]
impl Machine {
    /// What every test that is not about a target builds for.
    pub const TEST: Machine = Machine { layout: Layout::LP64, max_args: 4 };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Ty;
    use crate::{lexer, parser, sema};

    /// A machine that does not exist, to prove the sizes really do come from
    /// here. If any of them were still a literal eight somewhere, this would
    /// lay out the same class as [`Layout::LP64`] does.
    const ILP32: Machine =
        Machine { layout: Layout { word: 4, max_frame: 64 * 1024 }, max_args: 4 };

    fn class_of(machine: Machine, src: &str) -> (u32, Vec<u32>) {
        let ast = parser::parse(&lexer::lex(src).unwrap()).unwrap();
        let types = sema::check(&ast, machine).unwrap();
        let class = types.table().class(crate::ast::ClassId(0));
        (class.storage, class.fields.iter().map(|f| f.offset).collect())
    }

    const POINT: &str = "class Point {\n  int x;\n  int y;\n}\nfn main() {\n  Point p = Point { x: 1, y: 2 };\n  println(p.x);\n}\n";

    #[test]
    fn a_class_is_laid_out_for_the_machine_it_is_being_built_for() {
        // A vtable pointer, then two fields, all one word each.
        assert_eq!(class_of(Machine::TEST, POINT), (24, vec![8, 16]));
        assert_eq!(class_of(ILP32, POINT), (12, vec![4, 8]));
    }

    #[test]
    fn every_size_the_table_answers_follows_the_word() {
        for machine in [Machine::TEST, ILP32] {
            let ast = parser::parse(&lexer::lex(POINT).unwrap()).unwrap();
            let types = sema::check(&ast, machine).unwrap();
            let word = machine.layout.word;
            for ty in [Ty::Int, Ty::Float, Ty::Bool, Ty::Char, Ty::Str] {
                assert_eq!(types.table().size_of(ty), word, "{ty:?} on {machine:?}");
            }
        }
    }

    #[test]
    fn the_headers_a_value_carries_are_all_one_word() {
        // A string's length, a list's, an enum's tag and an object's method
        // table pointer are one fact written four ways. They agreeing is what
        // makes a second machine one change rather than four.
        for layout in [Layout::LP64, ILP32.layout] {
            for header in [layout.str_header(), layout.list_header(), layout.vptr(), layout.tag()] {
                assert_eq!(header, layout.word);
            }
        }
    }
}
