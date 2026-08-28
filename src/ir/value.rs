//! Names: virtual registers, operands, and the indices that address a program.

use crate::ast::Ty;

/// A virtual register: a value name, not yet a machine register. Scoped to one
/// [`Function`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VReg(pub u32);

/// Bytes one character of a string occupies.
///
/// Four, because a character is a Unicode scalar value and they run to
/// `0x10FFFF`. Storing them at a fixed width is what makes `s[i]` an address
/// computation rather than a walk from the start — the trade UTF-8 makes the
/// other way round, and the reason UTF-8 stays at the edges of the language.
pub const CHAR_BYTES: u32 = 4;

/// Index into [`Program::strings`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StrId(pub u32);

/// Index into [`Program::texts`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextId(pub u32);

/// Index into [`Function::blocks`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId(pub u32);

/// Index into [`Program::functions`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FuncId(pub u32);

/// An instruction operand: either an immediate or a virtual register.
///
/// The immediate is an `i64` whatever it stands for. A `float` travels as the
/// *bits* of its double — see [`Num`] — so this is a machine word rather than a
/// number, and only the instruction reading it knows which.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value {
    Const(i64),
    Reg(VReg),
}

/// Which arithmetic an instruction reads its operands as.
///
/// **This is the whole of how `float` is carried.** Every value in the IR is a
/// machine word: an `int` is its two's complement and a `float` is the bits of
/// its IEEE-754 double. Nothing about the word says which, so the two
/// instructions that *interpret* one say so themselves.
///
/// One class of virtual register is what that buys: copying, storing,
/// spilling, passing, returning and holding a float in an array are the same
/// code as for an `int`. It costs a move at each end of every float
/// instruction, into the floating-point registers the backend keeps as scratch
/// and back.
///
/// **This is the one portability decision in the IR.** Everything else here
/// says what a machine must do and leaves how to it; this has decided, on every
/// machine's behalf, that floats live in general registers. Making that free on
/// a machine with its own is a second register class in
/// [`crate::codegen::regalloc`] rather than a new backend — see
/// [`crate::target`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Num {
    Int,
    Float,
}

impl Num {
    /// How a value of this type is read. Everything that is not a `float` is a
    /// whole number of some width: a character's code point, an enum's tag, a
    /// bool's 0 or 1, an address.
    pub fn of(ty: Ty) -> Num {
        match ty {
            Ty::Float => Num::Float,
            _ => Num::Int,
        }
    }

    /// What the dump calls it, which is nothing at all for the ordinary case.
    pub(super) fn suffix(self) -> &'static str {
        match self {
            Num::Int => "",
            Num::Float => ".f",
        }
    }
}

