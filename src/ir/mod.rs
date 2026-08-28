//! Stage 4: AST -> intermediate representation.
//!
//! The IR is three-address code over an unbounded supply of *virtual*
//! registers, arranged as a **control flow graph**: a list of basic blocks,
//! each a straight run of instructions ending in a terminator that names its
//! successors.
//!
//! ## One function, one world
//!
//! A [`Function`] owns its blocks *and* its virtual registers, so [`BlockId`]
//! and [`VReg`] are indices **into that function**, never across the program.
//! That is what lets [`crate::codegen::regalloc`] run once per function and
//! give each one its own frame.
//!
//! String literals are the exception: they are interned for the whole
//! [`Program`], because they end up in a single `.data` section.
//!
//! ## Why a virtual register has no type
//!
//! It holds a machine word, and what the word *means* is the business of the
//! instruction reading it: an `int` is two's complement, a `char` is a code
//! point, a `bool` is 0 or 1, a `float` is the bits of an IEEE-754 double, and
//! a string, list or object is an address. Only the instructions that do
//! arithmetic on a word have to be told which — see [`Num`].

mod block;
mod coalesce;
mod liveness;
pub mod ssa;
mod dump;
mod fold;
mod func;
mod instr;
mod lower;
mod value;

#[cfg(test)]
mod tests;

pub use block::{Block, BlockKind, Target, Terminator, prune_unreachable};
pub use fold::{fold_bin, fold_cmp};
pub(crate) use fold::{fold_logic, negate_const, zero_to_subtract_from};
pub use func::{Function, Program};
pub use liveness::{Liveness, VRegSet, liveness};
pub use instr::{DivGuards, Instr, Runtime};
pub use lower::lower;
pub use value::{
    BlockId, CHAR_BYTES, FuncId, MAX_FRAME_BYTES, Num, STR_HEADER, StrId, TextId, VReg, Value,
};
