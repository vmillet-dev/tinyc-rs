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

/// Bytes of header in front of a string's characters, holding the count.
pub const STR_HEADER: u32 = 8;

/// Bytes of frame one function's locals may take.
///
/// The stack is the one resource a program is handed rather than asking for,
/// and it is the smaller of the two machines that decides how much: a Windows
/// thread gets a megabyte by default, a Linux one eight. A single function that
/// wants a quarter of the smaller is not a program that will run and be
/// unlucky — it is one that cannot recurse at all, and the compiler can see
/// that before it emits a `sub rsp` no stack could satisfy.
///
/// This is [`crate::sema::MAX_OBJECT_BYTES`] one level up. That one bounds a
/// single object; this one bounds what a function declares, which containment
/// and repetition can push far past any single object's size.
pub const MAX_FRAME_BYTES: u32 = 256 * 1024;

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
/// The price of the alternative is what makes this worth it. Giving a float its
/// own kind of virtual register would mean a second register class in
/// [`crate::codegen::regalloc`], a second set of spill slots, a second argument
/// convention and a prologue that saves both — and it would buy nothing here,
/// because a double is eight bytes and so is everything else. Copying, storing,
/// spilling, passing, returning and holding a float in an array are all the
/// same code as for an `int`, and stay that way.
///
/// What it costs instead is a `movq` at each end of every float instruction:
/// the operands come out of general registers into the two vector registers the
/// backend keeps as scratch, and the answer goes back. That is the trade, and
/// it is written down here because nothing else in the compiler should have to
/// rediscover it.
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

