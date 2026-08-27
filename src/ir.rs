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
//! give each one its own frame: nothing a function does can be observed by
//! another except through a call.
//!
//! String literals are the exception. They are interned once for the whole
//! [`Program`], because they end up in a single `.data` section.
//!
//! ## Why variables are not in SSA form
//!
//! Before control flow existed, each assignment could simply introduce a new
//! virtual register (`%n`, `%n.1`, ...) and every register had exactly one
//! definition. That breaks as soon as two paths can reach the same point:
//!
//! ```text
//! if (c) { n = 1; } else { n = 2; }
//! print(n);                          // which register is `n`?
//! ```
//!
//! Answering that in SSA needs phi nodes. Instead a variable keeps **one**
//! virtual register for its whole life and may be written many times, so both
//! branches assign the same register and the join needs nothing. The cost is
//! that live ranges can no longer be read off in one forward pass — see
//! [`crate::codegen::regalloc`], which computes them with a dataflow analysis.
//!
//! Temporaries are still written exactly once each.
//!
//! ## Why a virtual register has no type
//!
//! It holds a machine word, and what the word *means* is the business of the
//! instruction reading it: an `int` is two's complement, a `char` is a code
//! point, a `bool` is 0 or 1, a `float` is the bits of an IEEE-754 double, and
//! a string, list or object is an address. Only the instructions that do
//! arithmetic on a word have to be told which — see [`Num`], where that trade
//! and what it saves are written down.

use std::collections::{HashMap, HashSet};

use crate::ast::{
    FieldInit,
    ArmBody, BinOp, Block as AstBlock, Builtin, ClassId, CmpOp, EnumId, Expr, ExprKind, FnDecl,
    LogicOp,
    MatchArm, Pattern, Place, Prim, PrintPart, Program as Ast, Stmt, Ty, TypeTable,
    fits_in_an_int, is_scalar_value,
};
use crate::diag::{Diagnostic, Result};
use crate::sema::Types;

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    fn suffix(self) -> &'static str {
        match self {
            Num::Int => "",
            Num::Float => ".f",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Instr {
    /// `dst = val`
    Const { dst: VReg, val: i64 },
    /// `dst = &strings[id]`
    StrAddr { dst: VReg, id: StrId },
    /// `dst = src`
    Copy { dst: VReg, src: Value },
    /// `dst = lhs op rhs`, with `num` saying how the operands are read.
    Bin { num: Num, op: BinOp, dst: VReg, lhs: Value, rhs: Value },
    /// `dst = (lhs op rhs)`, producing 0 or 1.
    Cmp { num: Num, op: CmpOp, dst: VReg, lhs: Value, rhs: Value },
    /// `dst = src`, read as one kind of number and written as the other.
    ///
    /// The only instruction that changes a word's *meaning* rather than its
    /// value, and the reason `float(n)` and `int(f)` are written out: nothing
    /// converts on its own, so nowhere else does a word change what it is.
    ///
    /// `to` is the direction, because there are only two. One of them can fail
    /// — a float too large, or no number at all, has no `int` — which is the
    /// same asymmetry `char(n)` has.
    Cast { dst: VReg, to: Num, src: Value },
    /// `dst = &frame[offset]`: the address of a run of bytes this function owns.
    ///
    /// The first instruction in TinyC that is about *memory*. Everything else
    /// names a value; this names a place, and it exists because an array is the
    /// one thing too big to live in a register.
    ///
    /// The frame region is reserved once, in the prologue, so this costs a
    /// single `lea` and never allocates.
    Frame { dst: VReg, offset: u32 },
    /// `dst = base + offset`: the address of a field.
    ///
    /// Told apart from [`Instr::Elem`] because it is a different question. A
    /// field's place was settled by `sema` and cannot be out of range, so there
    /// is nothing to check — and the offset is in bytes, not elements.
    Field { dst: VReg, base: Value, offset: u32 },
    /// `dst = base + index * scale`, the address of one element.
    ///
    /// `len` travels with it so the backend can bounds check, and *only* so it
    /// can: an index and a length both known at compile time were settled by
    /// `sema` and need none, which is the same bargain `DivGuards` strikes for
    /// division. An array's length is always known; a string's never is, so
    /// indexing one always costs the check.
    ///
    /// `scale` is eight for everything that fits in a register, four for the
    /// characters of a string, and the object's room for an array or a list
    /// of them — which is the one case where the address is not a single
    /// `lea`, because x86 scales by 1, 2, 4 or 8 and nothing else.
    Elem { dst: VReg, base: Value, index: Value, len: Value, scale: u32 },
    /// `dst = len(of)`: the count that a string or a list carries in the eight
    /// bytes in front of its elements.
    ///
    /// The one place the compiler reads *behind* an address it was given. That
    /// is the whole reason the count lives there: the value stays one pointer,
    /// so it still travels in a register and still fits in an array slot, and
    /// yet knows its own length. It is also what makes strings and lists one
    /// question here — `len` never asks which of the two it was handed.
    Count { dst: VReg, of: Value },
    /// `dst = *addr`, reading four bytes and widening them.
    ///
    /// Only a string's characters are narrower than a machine word. Everywhere
    /// else — a variable, a field, an array element — a character occupies the
    /// eight bytes everything else does, which is what keeps every other offset
    /// in the compiler a multiple of eight.
    LoadChar { dst: VReg, addr: Value },
    /// `*dst = *src` for `bytes` bytes: what copying an aggregate *is*.
    ///
    /// An object or an array does not fit in a register, so assigning one is a
    /// run of moves rather than one. The count is always a multiple of eight
    /// and known at compile time, so it unrolls.
    CopyBytes { dst: Value, src: Value, bytes: u32 },
    /// The bytes of an aggregate have just been copied — now give the copy its
    /// own elements.
    ///
    /// `count` objects of `stride` bytes each, starting at `at`. What has to be
    /// done to one is decided by **the object**, not by the type of the hole it
    /// sits in: the routine hangs off its vtable, so a `Reading`-shaped field
    /// that turned out to hold a `Frost` fixes up a `Frost`.
    ///
    /// This is the whole price of a list field, and it is only ever emitted
    /// where one is reachable — [`TypeTable::holds_a_list`] is the question
    /// asked. Everything else a copy touches lives *inside* the bytes that were
    /// copied; a list's elements do not, so without this the copy and the
    /// original would name one run of them, and writing through either would be
    /// visible through the other.
    Fixup { at: Value, count: Value, stride: u32 },
    /// `dst = *addr`
    Load { dst: VReg, addr: Value },
    /// `*addr = value`
    Store { addr: Value, value: Value },
    /// `dst = arg(index)`: the incoming parameter that arrived in the ABI's
    /// register for `index`.
    ///
    /// These are the first instructions of the entry block, and they exist so
    /// that a parameter has a *definition point* at the top of the function.
    /// Without one, liveness would start a parameter's interval at its first
    /// use and happily hand its register to something else in the meantime.
    Param { dst: VReg, index: u32 },
    /// `dst = &vtable(class)`: the address of a class's method table.
    VTable { dst: VReg, class: ClassId },
    /// `dst = &value(enum::variant)`: the one value a variant that carries
    /// nothing has, written down once in `.data`.
    ///
    /// Only for an enum some *other* variant of which carries something, and so
    /// which is a pointer rather than a tag. Where every variant carries
    /// nothing the value is the tag itself and this never appears.
    VariantAddr { dst: VReg, id: EnumId, tag: u32 },
    /// `dst = callee(args)`, with `dst` absent when the result is discarded.
    Call { dst: Option<VReg>, callee: FuncId, args: Vec<Value> },
    /// `dst = receiver.vtable[slot](args)` — a call whose target is decided by
    /// the object rather than by the program text.
    ///
    /// The receiver is named on its own as well as appearing in `args`, because
    /// where it sits among them moves: a call that returns an aggregate leads
    /// with the address it fills. Its two roles are separate, so they are said
    /// separately.
    ///
    /// Lowering emits this only when the receiver's static class really has
    /// subclasses; when it has none there is nothing to decide, and an ordinary
    /// [`Instr::Call`] goes out instead.
    CallVirtual { dst: Option<VReg>, slot: u32, receiver: Value, args: Vec<Value> },
    /// `dst = callee(args)`, where the callee is one of the compiler's own
    /// routines rather than anything the program declared.
    ///
    /// It travels as a [`Runtime`] rather than a name so that the choice of
    /// which routines exist stays here, in the target-independent half, and
    /// only their *spelling* belongs to a backend.
    RtCall { dst: Option<VReg>, callee: Runtime, args: Vec<Value> },
    /// Write one value out, rendered according to its type.
    ///
    /// `newline` is set when this write is the last thing a `println` does, so
    /// that ending the line costs nothing: the format the backend reaches for
    /// already ends in one. Without it a `println(n)` was two calls, the second
    /// of them handing `printf` a format *and* an argument in order to write a
    /// single character.
    ///
    /// It is the value that carries the flag rather than a piece of text after
    /// it, because only the last *part* of a `println` can end the line, and
    /// when that part is text the newline simply joins it — the same reason
    /// `println("done")` has always been one call and not two.
    Print { ty: Ty, val: Value, newline: bool },
    /// Write out a run of text the program settled at compile time: the words
    /// around the specifiers in a format string, and the line ending a
    /// `println` adds.
    ///
    /// Separate from [`Instr::Print`] of a `Ty::Str` because the two are not
    /// the same job. A string is a run of characters somewhere in memory whose
    /// UTF-8 has to be built before anything can be written; this is bytes the
    /// backend already has, so it costs one call and no memory at all.
    PrintText { id: TextId },
}

/// The compiler's own routines, called like functions but declared by nobody.
///
/// Each is here because it is a *loop*, and a loop is the one thing lowering
/// cannot inline without emitting blocks for it. Everything a string does in a
/// straight line — its length, one of its characters — is an instruction
/// instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Runtime {
    /// `(a, b) -> string`: a new string holding a's characters then b's.
    Concat,
    /// `(a, b) -> string`: the same answer as [`Runtime::Concat`], made in
    /// place where it can be.
    ///
    /// Emitted only for `s = s + e`, and only where lowering has established
    /// that **nothing else can be holding `s`** — see [`owned_strings`]. That
    /// is the whole of the difference: a string's length lives with its
    /// characters, so growing one where it stands would bump a count every
    /// other name for it can see. Where there is no other name, there is
    /// nothing to see it.
    ///
    /// It still only grows in place when `s` is the *last* thing the arena
    /// handed out, which is what a bump pointer can give back; otherwise it
    /// allocates exactly as `Concat` does. So the answer never differs — only
    /// how much memory a loop of them leaves behind, which goes from the square
    /// of the loop count to a constant factor of the answer.
    Append,
    /// `(a, b) -> bool`: same length, same characters. Comparing the two
    /// addresses would answer a question nobody asked.
    StrEq,
    /// `(n) -> char`: `n` itself, having refused one that names no character.
    CheckChar,
    /// `(c) -> string`: a string holding that one character.
    CharToStr,
    /// `(n) -> string`: the number written out in decimal.
    IntToStr,
    /// `(n, bytes) -> list`: room for `n` elements of `bytes` each, with the
    /// length already set — what a list literal fills in.
    ///
    /// The width is an argument because an element is not always a register:
    /// a list holds its elements where it is, so one holding objects holds
    /// whole objects.
    ListNew,
    /// `(list, value) -> list`: one more element on the end, **answering where
    /// the list now is**. Growing copies the elements into a larger block, so
    /// the address may change and the caller has to be told.
    ListPush,
    /// `(list, from, bytes) -> list`: the same push for an element too big for
    /// a register, which therefore arrives as an *address* and is copied in.
    ///
    /// Both go through one routine that makes the room; only what is written
    /// into it differs — a store there, a copy here.
    ListPushBig,
    /// `(s) -> int`: the number a string spells, refusing anything else.
    StrToInt,
    /// `(s) -> bool`: whether [`Runtime::StrToInt`] would answer rather than
    /// stop the program.
    ///
    /// The two are one routine asked two ways — see the backend — so they
    /// cannot come to disagree about what a number is.
    IsInt,
    /// `() -> string`: one line of input, without its line ending.
    ReadLine,
    /// `() -> bool`: whether the input has run out, asked without consuming
    /// anything.
    Eof,
    /// `(chars) -> string`: the characters of a `char[]`, sealed into a string.
    ///
    /// The way to build a string a character at a time without paying for a
    /// whole new string on each one.
    CharsToStr,
    /// `(list, bytes) -> list`: a new list holding the same elements. What
    /// makes assigning a list a copy rather than a second name for one.
    ListClone,
    /// `(bytes) -> address`: room in the arena, uninitialised.
    ///
    /// The one routine lowering calls for its own sake rather than to stand
    /// in for an operator. A variant that carries something needs room for a
    /// tag and its payload, and what goes in it is stores the caller emits —
    /// so there is nothing here worth a routine of its own.
    Alloc,
}

impl Runtime {
    /// What this routine is called, without the prefix a backend adds.
    pub fn name(self) -> &'static str {
        match self {
            Runtime::Concat => "concat",
            Runtime::Append => "append",
            Runtime::StrEq => "str_eq",
            Runtime::CheckChar => "check_char",
            Runtime::CharToStr => "char_str",
            Runtime::IntToStr => "int_str",
            Runtime::ListNew => "list_new",
            Runtime::ListPush => "list_push",
            Runtime::ListPushBig => "list_push_big",
            Runtime::ListClone => "list_clone",
            Runtime::Alloc => "alloc",
            Runtime::CharsToStr => "chars_str",
            Runtime::StrToInt => "str_int",
            Runtime::IsInt => "is_int",
            Runtime::ReadLine => "read_line",
            Runtime::Eof => "eof",
        }
    }

    /// The routine a built-in function is, which is the only thing that tells
    /// them apart once lowering is done.
    pub fn of(builtin: Builtin) -> Runtime {
        match builtin {
            Builtin::ReadLine => Runtime::ReadLine,
            Builtin::Eof => Runtime::Eof,
            Builtin::IsInt => Runtime::IsInt,
        }
    }
}

/// How a basic block ends. Every block has exactly one.
#[derive(Clone, Debug)]
pub enum Terminator {
    Jump(BlockId),
    /// Continue at `then_blk` when `cond` is non-zero, `else_blk` otherwise.
    Branch { cond: Value, then_blk: BlockId, else_blk: BlockId },
    /// Leave the function, with a value for a function that returns one.
    Return(Option<Value>),
}

impl Terminator {
    pub fn successors(&self) -> Vec<BlockId> {
        match self {
            Terminator::Jump(target) => vec![*target],
            Terminator::Branch { then_blk, else_blk, .. } => vec![*then_blk, *else_blk],
            Terminator::Return(_) => Vec::new(),
        }
    }

    /// Show `visit` every virtual register the terminator reads.
    ///
    /// A callback rather than an iterator or a `Vec`: liveness asks this of
    /// every terminator on every round of its fixpoint, and there is nothing to
    /// allocate for at most one register.
    pub fn uses(&self, mut visit: impl FnMut(VReg)) {
        if let Terminator::Branch { cond: Value::Reg(reg), .. }
        | Terminator::Return(Some(Value::Reg(reg))) = self
        {
            visit(*reg);
        }
    }
}

/// What a block is *for*, which is the half of its name that survives
/// renumbering.
///
/// A label is derived from this and the block's index rather than stored as
/// text, so pruning renumbers a block by assigning a number instead of by
/// editing a string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    /// Where the function starts. Always block 0.
    Entry,
    Then,
    Else,
    /// Where the arms of an `if` meet again.
    Join,
    /// A loop header: it re-tests the condition on every iteration.
    Loop,
    Body,
    /// A `for`'s step, on the occasions it needs a block of its own: a
    /// `continue` has to jump somewhere that still runs it.
    Step,
    /// Where a loop leaves.
    Done,
    /// One `match` arm's body.
    Arm,
    /// Where a `match` tests the next variant, having ruled out the ones before.
    Case,
    /// The right operand of `&&` or `||`, reached only when the left one did
    /// not already settle the answer.
    Rhs,
    /// Where a short-circuited `&&` or `||` lands, carrying the answer its left
    /// operand gave on its own.
    Short,
    /// Opened after a `return`, `break` or `continue` for whatever follows it,
    /// and reached by nothing.
    Unreachable,
}

impl BlockKind {
    fn prefix(self) -> &'static str {
        match self {
            BlockKind::Entry => "entry",
            BlockKind::Then => "then",
            BlockKind::Else => "else",
            BlockKind::Join => "join",
            BlockKind::Loop => "loop",
            BlockKind::Body => "body",
            BlockKind::Step => "step",
            BlockKind::Done => "done",
            BlockKind::Arm => "arm",
            BlockKind::Case => "case",
            BlockKind::Rhs => "rhs",
            BlockKind::Short => "short",
            BlockKind::Unreachable => "unreachable",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Block {
    pub kind: BlockKind,
    /// Position in [`Function::blocks`], repeated here so a block can name
    /// itself.
    pub index: u32,
    pub instrs: Vec<Instr>,
    pub term: Terminator,
}

impl Block {
    /// Assembly label and dump name, e.g. `then0` or `loop2`.
    pub fn label(&self) -> String {
        format!("{}{}", self.kind.prefix(), self.index)
    }
}

impl Instr {
    /// Show `visit` every virtual register this instruction reads.
    ///
    /// See [`Terminator::uses`] for why this hands registers to a callback
    /// rather than collecting them.
    pub fn uses(&self, mut visit: impl FnMut(VReg)) {
        let mut reg = |value: &Value| {
            if let Value::Reg(r) = value {
                visit(*r);
            }
        };
        match self {
            Instr::Const { .. }
            | Instr::StrAddr { .. }
            | Instr::Param { .. }
            | Instr::Frame { .. }
            | Instr::PrintText { .. }
            | Instr::VariantAddr { .. }
            | Instr::VTable { .. } => {}
            Instr::Copy { src, .. }
            | Instr::Cast { src, .. }
            | Instr::Load { addr: src, .. }
            | Instr::LoadChar { addr: src, .. }
            | Instr::Count { of: src, .. } => reg(src),
            Instr::Bin { lhs, rhs, .. } | Instr::Cmp { lhs, rhs, .. } => {
                reg(lhs);
                reg(rhs);
            }
            Instr::Elem { base, index, len, .. } => {
                reg(base);
                reg(index);
                reg(len);
            }
            Instr::Field { base, .. } => reg(base),
            Instr::Store { addr, value } => {
                reg(addr);
                reg(value);
            }
            Instr::CopyBytes { dst, src, .. } => {
                reg(dst);
                reg(src);
            }
            Instr::Fixup { at, count, .. } => {
                reg(at);
                reg(count);
            }
            Instr::Print { val, .. } => reg(val),
            Instr::Call { args, .. } | Instr::RtCall { args, .. } => args.iter().for_each(reg),
            Instr::CallVirtual { receiver, args, .. } => {
                reg(receiver);
                args.iter().for_each(reg);
            }
        }
    }

    /// The virtual register written by this instruction, if any.
    pub fn def(&self) -> Option<VReg> {
        match self {
            Instr::Const { dst, .. }
            | Instr::StrAddr { dst, .. }
            | Instr::Copy { dst, .. }
            | Instr::Cast { dst, .. }
            | Instr::Bin { dst, .. }
            | Instr::Param { dst, .. }
            | Instr::Frame { dst, .. }
            | Instr::VTable { dst, .. }
            | Instr::VariantAddr { dst, .. }
            | Instr::Elem { dst, .. }
            | Instr::Field { dst, .. }
            | Instr::Load { dst, .. }
            | Instr::LoadChar { dst, .. }
            | Instr::Count { dst, .. }
            | Instr::Cmp { dst, .. } => Some(*dst),
            Instr::Call { dst, .. } | Instr::CallVirtual { dst, .. } | Instr::RtCall { dst, .. } => {
                *dst
            }
            Instr::Print { .. }
            | Instr::PrintText { .. }
            | Instr::Store { .. }
            | Instr::CopyBytes { .. }
            | Instr::Fixup { .. } => None,
        }
    }

    /// Whether this instruction can stop the program rather than answer.
    ///
    /// A fact about the *language*, not about a machine: it is the list of
    /// places TinyC promised to refuse rather than answer wrongly. Two readers
    /// need it and would otherwise each have their own opinion — the backend,
    /// which emits a guard and therefore has to declare the abort routine, and
    /// [`crate::opt`], which may not delete a computation whose fault is the
    /// whole reason the program stops where it does.
    pub fn can_fail(&self) -> bool {
        match self {
            // A constant index into a length known here was settled by `sema`;
            // anything else — including every index into a string, whose length
            // is never known here — is checked where it lands.
            Instr::Elem { index, len, .. } => {
                !matches!((index, len), (Value::Const(_), Value::Const(_)))
            }
            // Float arithmetic answers whatever it is given: too large is an
            // infinity and zero into zero is a NaN, both of which are values.
            // There is nothing to refuse, so there is nothing to guard and
            // nothing keeping a float computation nobody reads alive.
            Instr::Bin { num: Num::Float, .. } => false,
            Instr::Bin { op, lhs, rhs, .. } if op.divides() => DivGuards::of(lhs, rhs).any(),
            // `add`, `sub` and `imul` are all guarded; a folded result never
            // reaches an instruction in the first place.
            Instr::Bin { .. } => true,
            // The other direction cannot: every `int` has a `float` nearest it,
            // while a float too large, or no number at all, has no `int`.
            Instr::Cast { to: Num::Int, .. } => true,
            Instr::Cast { .. } => false,
            // Every runtime routine can fail: the two that allocate run out of
            // memory, and the one that checks a character refuses.
            Instr::RtCall { .. } => true,
            _ => false,
        }
    }

    /// Show `visit` every operand, so that it may be replaced.
    ///
    /// The counterpart of [`Self::uses`], and the whole of what a rewrite needs
    /// to substitute one value for another. Listing the operands twice is worth
    /// it: an instruction added to the enum without a row here would silently
    /// stop being optimised rather than fail to compile — which is why the
    /// exhaustive `match` has no wildcard arm.
    pub fn values_mut(&mut self, mut visit: impl FnMut(&mut Value)) {
        match self {
            Instr::Const { .. }
            | Instr::StrAddr { .. }
            | Instr::Param { .. }
            | Instr::Frame { .. }
            | Instr::PrintText { .. }
            | Instr::VariantAddr { .. }
            | Instr::VTable { .. } => {}
            Instr::Copy { src: value, .. }
            | Instr::Cast { src: value, .. }
            | Instr::Load { addr: value, .. }
            | Instr::LoadChar { addr: value, .. }
            | Instr::Count { of: value, .. }
            | Instr::Field { base: value, .. }
            | Instr::Print { val: value, .. } => visit(value),
            Instr::Bin { lhs, rhs, .. } | Instr::Cmp { lhs, rhs, .. } => {
                visit(lhs);
                visit(rhs);
            }
            Instr::Elem { base, index, len, .. } => {
                visit(base);
                visit(index);
                visit(len);
            }
            Instr::Store { addr, value } => {
                visit(addr);
                visit(value);
            }
            Instr::CopyBytes { dst, src, .. } => {
                visit(dst);
                visit(src);
            }
            Instr::Fixup { at, count, .. } => {
                visit(at);
                visit(count);
            }
            Instr::Call { args, .. } | Instr::RtCall { args, .. } => args.iter_mut().for_each(visit),
            Instr::CallVirtual { receiver, args, .. } => {
                visit(receiver);
                args.iter_mut().for_each(visit);
            }
        }
    }

    /// Whether this instruction performs a call, and therefore destroys the
    /// caller-saved registers.
    pub fn is_call(&self) -> bool {
        matches!(
            self,
            Instr::Print { .. }
                | Instr::PrintText { .. }
                | Instr::Call { .. }
                | Instr::CallVirtual { .. }
                | Instr::RtCall { .. }
                | Instr::Fixup { .. }
        )
    }
}

/// Which of division's two faults a division still has to rule out.
///
/// Division traps on a zero divisor, and also on `i64::MIN / -1`, whose
/// quotient does not fit in the width it would have to come back in. Every
/// check an operand already known here answers is one the emitted code does not
/// carry — which is why this is asked of [`Value`]s rather than of registers.
///
/// It lives beside the IR rather than in a backend because it is a fact about
/// TinyC's arithmetic that happens to be true of x86 as well: a machine that
/// divided without trapping would still have to be told to stop, because the
/// language says so. The backend reads both fields to choose which guards to
/// emit; [`Instr::can_fail`] reads only whether either is left.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DivGuards {
    pub zero: bool,
    pub overflow: bool,
}

impl DivGuards {
    pub fn of(lhs: &Value, rhs: &Value) -> DivGuards {
        let zero = !matches!(rhs, Value::Const(c) if *c != 0);
        let overflow = match (lhs, rhs) {
            // Only `i64::MIN` can overflow, and only when divided by -1.
            (Value::Const(dividend), _) if *dividend != i64::MIN => false,
            (_, Value::Const(divisor)) => *divisor == -1,
            _ => true,
        };
        DivGuards { zero, overflow }
    }

    pub fn any(self) -> bool {
        self.zero || self.overflow
    }
}

/// Whether the room being written into is brand new, or already has a name.
///
/// It decides one thing, and it is the difference between two and four
/// instructions per element: whether an aggregate **literal** may be built where
/// it is going, rather than somewhere else and then copied over.
///
/// * [`Room::Fresh`] — a field of an object being constructed, an element of an
///   array literal, the room a declaration just reserved, the room a caller
///   passed for a return. Nothing can name it yet, so the expression filling it
///   cannot read it, and filling it piece by piece is not observable.
/// * [`Room::Named`] — the target of an assignment. Here it very much can:
///
///   ```text
///   int[2] a = [1, 2];
///   a = [a[1], a[0]];      // a swap
///   ```
///
///   Filling `a` element by element would write `a[1]` into `a[0]` and then
///   read it straight back out, and the swap would answer `[2, 2]`. So an
///   assignment builds the literal elsewhere and copies it, which is what makes
///   the whole value change at once — the same reason assignment copies rather
///   than aliasing in the first place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Room {
    Fresh,
    Named,
}

/// One function's control flow graph, virtual registers and signature.
pub struct Function {
    pub name: String,
    /// The register each parameter was lowered into, in declaration order.
    pub params: Vec<VReg>,
    /// `None` for a function that returns nothing.
    pub ret: Option<Ty>,
    /// Bytes of frame this function's arrays need, reserved once in the
    /// prologue and handed out by [`Instr::Frame`].
    pub frame_bytes: u32,
    /// Basic blocks in the order they will be emitted; block 0 is the entry.
    pub blocks: Vec<Block>,
    /// Human-readable name per virtual register, used by IR and allocator dumps.
    pub vreg_names: Vec<String>,
}

impl Function {
    pub fn vreg_count(&self) -> usize {
        self.vreg_names.len()
    }

    pub fn name_of(&self, reg: VReg) -> &str {
        &self.vreg_names[reg.0 as usize]
    }

    pub fn block(&self, id: BlockId) -> &Block {
        &self.blocks[id.0 as usize]
    }

    fn value_name(&self, value: &Value) -> String {
        self.value_name_as(Num::Int, value)
    }

    /// The same, for an operand whose instruction says how to read it.
    ///
    /// A float constant is the bits of a double, and `4620974692658839552` in
    /// the dump beside a `printf` says nothing a reader could check. Written
    /// back out as `8.5` it does — and the `f` on the end says the number is
    /// what the word means rather than what it holds.
    fn value_name_as(&self, num: Num, value: &Value) -> String {
        match (num, value) {
            (Num::Float, Value::Const(c)) => format!("{}f", f64::from_bits(*c as u64)),
            (_, Value::Const(c)) => c.to_string(),
            (_, Value::Reg(r)) => format!("%{}", self.name_of(*r)),
        }
    }

    /// The signature line used by the IR dump and by assembly comments.
    pub fn signature(&self, table: &TypeTable) -> String {
        let params: Vec<String> =
            self.params.iter().map(|&r| format!("%{}", self.name_of(r))).collect();
        let ret = match self.ret {
            Some(ty) => format!(" -> {}", ty.name(table)),
            None => String::new(),
        };
        format!("fn {}({}){}", self.name, params.join(", "), ret)
    }
}

pub struct Program {
    pub functions: Vec<Function>,
    /// Interned string literals, each as the characters it holds.
    pub strings: Vec<Vec<char>>,
    /// Interned runs of literal text, each already the UTF-8 it will be written
    /// as. See [`Instr::PrintText`].
    pub texts: Vec<String>,
    /// One method table per class, in `ClassId` order.
    pub vtables: Vec<Vec<FuncId>>,
    /// Every type the program has, carried through so that a type can still be named
    /// and a value of one can still be printed.
    ///
    /// An enum's *values* need nothing here: a variant is its index, so it is
    /// an integer everywhere the backend is concerned. Only the names survive.
    pub table: TypeTable,
}

impl Program {
    pub fn function(&self, id: FuncId) -> &Function {
        &self.functions[id.0 as usize]
    }

    /// Render the IR for `--emit ir`.
    pub fn dump(&self) -> String {
        let mut out = String::new();
        for (i, s) in self.strings.iter().enumerate() {
            out.push_str(&format!("str{i} = {:?}
", s.iter().collect::<String>()));
        }
        if !self.strings.is_empty() {
            out.push('\n');
        }

        for function in &self.functions {
            out.push_str(&format!("{}:\n", function.signature(&self.table)));

            // Numbering restarts per function, matching the indices the
            // allocator works with.
            let mut index = 0;
            for block in &function.blocks {
                out.push_str(&format!("{}:\n", block.label()));
                for instr in &block.instrs {
                    let text = self.instr_text(function, instr);
                    out.push_str(&format!("{index:>3}  {text}\n"));
                    index += 1;
                }

                let text = match &block.term {
                    Terminator::Jump(target) => {
                        format!("jump {}", function.block(*target).label())
                    }
                    Terminator::Branch { cond, then_blk, else_blk } => format!(
                        "branch {} ? {} : {}",
                        function.value_name(cond),
                        function.block(*then_blk).label(),
                        function.block(*else_blk).label()
                    ),
                    Terminator::Return(None) => "return".to_string(),
                    Terminator::Return(Some(value)) => {
                        format!("return {}", function.value_name(value))
                    }
                };
                out.push_str(&format!("{index:>3}  {text}\n"));
                index += 1;
            }
            out.push('\n');
        }
        out
    }

    /// One instruction, in the form `--emit ir` prints it. The backend echoes
    /// these as comments, so a reader can line the assembly up against the IR
    /// dump instruction by instruction.
    pub fn instr_text(&self, function: &Function, instr: &Instr) -> String {
        let value = |v: &Value| function.value_name(v);
        match instr {
            Instr::Const { dst, val } => format!("%{} = const {val}", function.name_of(*dst)),
            Instr::StrAddr { dst, id } => {
                format!("%{} = straddr str{}", function.name_of(*dst), id.0)
            }
            Instr::Copy { dst, src } => {
                format!("%{} = copy {}", function.name_of(*dst), value(src))
            }
            Instr::Bin { num, op, dst, lhs, rhs } => format!(
                "%{} = {}{} {}, {}",
                function.name_of(*dst),
                op_name(*op),
                num.suffix(),
                function.value_name_as(*num, lhs),
                function.value_name_as(*num, rhs)
            ),
            Instr::Cmp { num, op, dst, lhs, rhs } => format!(
                "%{} = cmp{} {} {}, {}",
                function.name_of(*dst),
                num.suffix(),
                op.symbol(),
                function.value_name_as(*num, lhs),
                function.value_name_as(*num, rhs)
            ),
            // Named as the conversion the program wrote — `int(f)`, `float(n)`
            // — rather than with a word of the dump's own, and the operand is
            // read the *other* way round from what the instruction produces.
            Instr::Cast { dst, to, src } => format!(
                "%{} = {} {}",
                function.name_of(*dst),
                match to {
                    Num::Int => Prim::Int,
                    Num::Float => Prim::Float,
                }
                .name(),
                match to {
                    Num::Int => function.value_name_as(Num::Float, src),
                    Num::Float => function.value_name_as(Num::Int, src),
                }
            ),
            Instr::Param { dst, index } => {
                format!("%{} = param {index}", function.name_of(*dst))
            }
            Instr::Frame { dst, offset } => {
                format!("%{} = frame {offset}", function.name_of(*dst))
            }
            Instr::Field { dst, base, offset } => {
                format!("%{} = field {} + {offset}", function.name_of(*dst), value(base))
            }
            Instr::Elem { dst, base, index, len, scale } => format!(
                "%{} = elem {}[{}] of {} by {scale}",
                function.name_of(*dst),
                value(base),
                value(index),
                value(len)
            ),
            Instr::Count { dst, of } => {
                format!("%{} = count {}", function.name_of(*dst), value(of))
            }
            Instr::LoadChar { dst, addr } => {
                format!("%{} = loadchar {}", function.name_of(*dst), value(addr))
            }
            Instr::RtCall { dst, callee, args } => {
                let args: Vec<String> = args.iter().map(value).collect();
                let call = format!("rt.{}({})", callee.name(), args.join(", "));
                match dst {
                    Some(dst) => format!("%{} = {call}", function.name_of(*dst)),
                    None => call,
                }
            }
            Instr::Fixup { at, count, stride } => {
                format!("fixup {} of {} bytes at {}", value(count), stride, value(at))
            }
            Instr::CopyBytes { dst, src, bytes } => {
                format!("copy {} bytes to {}, from {}", bytes, value(dst), value(src))
            }
            Instr::Load { dst, addr } => {
                format!("%{} = load {}", function.name_of(*dst), value(addr))
            }
            Instr::Store { addr, value: stored } => {
                format!("store {}, {}", value(addr), value(stored))
            }
            Instr::Call { dst, callee, args } => {
                let args: Vec<String> = args.iter().map(value).collect();
                let call = format!("call {}({})", self.function(*callee).name, args.join(", "));
                match dst {
                    Some(dst) => format!("%{} = {call}", function.name_of(*dst)),
                    None => call,
                }
            }
            Instr::VariantAddr { dst, id, tag } => format!(
                "%{} = value {}::{}",
                function.name_of(*dst),
                self.table.enum_info(*id).name,
                self.table.enum_info(*id).variants[*tag as usize].name
            ),
            Instr::VTable { dst, class } => format!(
                "%{} = vtable {}",
                function.name_of(*dst),
                self.table.class(*class).name
            ),
            Instr::CallVirtual { dst, slot, receiver, args } => {
                let args: Vec<String> = args.iter().map(value).collect();
                let call =
                    format!("callv {}[{slot}]({})", value(receiver), args.join(", "));
                match dst {
                    Some(dst) => format!("%{} = {call}", function.name_of(*dst)),
                    None => call,
                }
            }
            Instr::Print { ty, val, newline } => format!(
                "print{} {} {}",
                match newline {
                    true => "ln",
                    false => "",
                },
                ty.name(&self.table),
                function.value_name_as(Num::of(*ty), val)
            ),
            Instr::PrintText { id } => {
                format!("print text{} {:?}", id.0, self.texts[id.0 as usize])
            }
        }
    }
}

fn op_name(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "add",
        BinOp::Sub => "sub",
        BinOp::Mul => "mul",
        BinOp::Div => "div",
        BinOp::Rem => "rem",
    }
}

/// Lower a type-checked AST to IR. Assumes [`crate::sema::check`] succeeded.
///
/// The one thing that can still fail here is the size of a frame, and it can
/// only fail here: how much stack a function wants is not a fact about its
/// types, it is the sum of every aggregate this stage hands room to. The number
/// that goes into `sub rsp` is the number checked, so the two cannot drift.
pub fn lower(ast: &Ast, types: &Types) -> Result<Program> {
    // Function ids follow declaration order, so a call can be lowered to an
    // index without caring whether the callee has been lowered yet — which is
    // exactly what recursion and forward calls need.
    //
    // The first declaration of a name wins, matching the table `sema` built:
    // a duplicate is an error, and the two stages must at least agree on which
    // one they were talking about.
    // A method's name lives in its class rather than in the program, so two
    // classes may both have an `area`. Qualifying it here is what keeps the one
    // flat table of callables — and, further on, keeps their symbols apart.
    let names: Vec<String> = qualified_names(ast);
    let func_ids: Vec<FuncId> = (0..ast.functions.len() as u32).map(FuncId).collect();

    let mut ids: HashMap<String, FuncId> = HashMap::new();
    for (index, name) in names.iter().enumerate() {
        ids.entry(name.clone()).or_insert(FuncId(index as u32));
    }

    let mut strings = Strings::default();
    let mut functions = Vec::new();
    let mut errors = Vec::new();
    for (index, decl) in ast.functions.iter().enumerate() {
        let lowering = Lowering {
            blocks: Vec::new(),
            vreg_names: Vec::new(),
            current: BlockId(0),
            scopes: vec![HashMap::new()],
            loops: Vec::new(),
            frame_bytes: 0,
            frame_peak: 0,
            out_pointer: None,
            name_counts: HashMap::new(),
            types,
            table: types.table(),
            func_ids: &func_ids,
            strings: &mut strings,
            ids: &ids,
            owned: owned_strings(decl, types),
        };
        let mut lowered =
            lowering.run(decl, types.ret_of(index), types.params_of(index));
        lowered.name = names[index].clone();
        if lowered.frame_bytes > MAX_FRAME_BYTES {
            errors.push(too_much_stack(&lowered, decl));
        }
        functions.push(lowered);
    }

    // One vtable per class, holding the implementation each slot resolved to.
    let vtables: Vec<Vec<FuncId>> = types
        .table()
        .classes
        .iter()
        .map(|class| class.methods.iter().map(|m| func_ids[m.function]).collect())
        .collect();

    // Before the pruning, deliberately: a frame nothing reaches is still one
    // the program asked for, and `sema` reports a mistake in an uncalled
    // function too. What is emitted must not decide what is diagnosed, or a
    // program would start failing to compile the moment something called it.
    if !errors.is_empty() {
        errors.sort_by_key(|d| d.span.offset);
        return Err(errors);
    }

    let (functions, vtables) =
        prune_unreachable_functions(functions, vtables, ids.get(crate::sema::ENTRY_POINT));
    Ok(Program {
        functions,
        strings: strings.chars,
        texts: strings.texts,
        table: types.table().clone(),
        vtables,
    })
}

/// A function whose locals no stack would hold.
fn too_much_stack(lowered: &Function, decl: &FnDecl) -> Diagnostic {
    let bytes = lowered.frame_bytes;
    let size = match bytes == u32::MAX {
        true => "more than four gigabytes".to_string(),
        false => format!("{bytes} bytes"),
    };
    Diagnostic::new(format!("`{}` needs too much stack", decl.name), decl.name_span)
        .with_label(format!("{size} of locals, and at most {MAX_FRAME_BYTES} are supported"))
        .with_note(
            "every value too big for a register lives in the frame, and the frame is reserved \
             for the whole call; `int[]` is what holds a quantity the stack cannot",
            None,
        )
}

/// The name each of the program's functions is known by once methods and free
/// functions share one list: `Circle$area` for a method, the plain name
/// otherwise.
fn qualified_names(ast: &Ast) -> Vec<String> {
    let mut names: Vec<String> = ast.functions.iter().map(|f| f.name.clone()).collect();
    for class in &ast.classes {
        for &at in &class.methods {
            names[at] = format!("{}${}", class.name, ast.functions[at].name);
        }
    }
    names
}

/// The literals every function shares, plus the index that keeps interning them
/// a lookup rather than a scan.
#[derive(Default)]
struct Strings {
    chars: Vec<Vec<char>>,
    ids: HashMap<Vec<char>, StrId>,
    texts: Vec<String>,
    text_ids: HashMap<String, TextId>,
}

/// Drop the functions nothing can call, and renumber the survivors.
///
/// The same walk as [`prune_unreachable`], one level up: the call graph instead
/// of the control flow graph, rooted at the entry point rather than at block 0.
/// A helper nobody calls costs a label, a prologue and an epilogue otherwise.
fn prune_unreachable_functions(
    functions: Vec<Function>,
    vtables: Vec<Vec<FuncId>>,
    entry: Option<&FuncId>,
) -> (Vec<Function>, Vec<Vec<FuncId>>) {
    let Some(&entry) = entry else {
        // No entry point: `sema` has already rejected the program, and there is
        // no root to walk from.
        return (functions, vtables);
    };

    let mut reachable = vec![false; functions.len()];
    let mut stack = vec![entry];
    while let Some(id) = stack.pop() {
        let index = id.0 as usize;
        if std::mem::replace(&mut reachable[index], true) {
            continue;
        }
        for block in &functions[index].blocks {
            for instr in &block.instrs {
                match instr {
                    Instr::Call { callee, .. } => stack.push(*callee),
                    // Making an object is what makes its methods callable, and
                    // the only thing that does: a virtual call names a slot,
                    // and the objects that could answer it are exactly the ones
                    // some `New` built.
                    Instr::VTable { class, .. } => {
                        stack.extend(&vtables[class.0 as usize]);
                    }
                    _ => {}
                }
            }
        }
    }

    // Old index -> new index, for the calls and the tables that name them.
    let mut renumber = vec![FuncId(0); functions.len()];
    let mut next = 0;
    for (index, keep) in reachable.iter().enumerate() {
        if *keep {
            renumber[index] = FuncId(next);
            next += 1;
        }
    }

    let vtables = vtables
        .into_iter()
        .map(|slots| slots.into_iter().map(|at| renumber[at.0 as usize]).collect())
        .collect();

    let functions = functions
        .into_iter()
        .zip(&reachable)
        .filter(|(_, keep)| **keep)
        .map(|(mut function, _)| {
            for block in &mut function.blocks {
                for instr in &mut block.instrs {
                    if let Instr::Call { callee, .. } = instr {
                        *callee = renumber[callee.0 as usize];
                    }
                }
            }
            function
        })
        .collect();
    (functions, vtables)
}

/// Drop the blocks nothing can reach, and renumber the survivors.
///
/// Lowering a `return` opens a fresh block for whatever follows it, which is
/// usually nothing at all. Without this pass every function ending in a
/// `return` would carry a stray block, and the backend would dutifully emit a
/// second, unreachable epilogue for it.
pub(crate) fn prune_unreachable(blocks: Vec<Block>) -> Vec<Block> {
    let mut reachable = vec![false; blocks.len()];
    let mut stack = vec![BlockId(0)];
    while let Some(id) = stack.pop() {
        let index = id.0 as usize;
        // `replace` answers what the flag was *before* it was set, which is the
        // "have I already been here?" a graph walk needs.
        if std::mem::replace(&mut reachable[index], true) {
            continue;
        }
        stack.extend(blocks[index].term.successors());
    }

    // Old index -> new index, for the terminators that name them.
    let mut renumber = vec![BlockId(0); blocks.len()];
    let mut next = 0;
    for (index, keep) in reachable.iter().enumerate() {
        if *keep {
            renumber[index] = BlockId(next);
            next += 1;
        }
    }

    blocks
        .into_iter()
        .zip(&reachable)
        .filter(|(_, keep)| **keep)
        .enumerate()
        .map(|(index, (mut block, _))| {
            // A label is derived from the index, so renumbering is a single
            // assignment: `else3` becomes `else2` when a block ahead of it went
            // away, and the kind still says where the block came from.
            block.index = index as u32;

            block.term = match block.term {
                Terminator::Jump(target) => Terminator::Jump(renumber[target.0 as usize]),
                Terminator::Branch { cond, then_blk, else_blk } => Terminator::Branch {
                    cond,
                    then_blk: renumber[then_blk.0 as usize],
                    else_blk: renumber[else_blk.0 as usize],
                },
                term @ Terminator::Return(_) => term,
            };
            block
        })
        .collect()
}

/// A block while it is still being filled in.
///
/// The terminator is an `Option` on purpose: "not decided yet" is a state the
/// lowering really is in, and giving it a `Terminator` value instead would mean
/// a block whose terminator was never patched still assembles — into a plausible
/// but wrong `ret`. [`Lowering::run`] turns the `None` that should be impossible
/// into a panic rather than a miscompilation.
struct PendingBlock {
    kind: BlockKind,
    instrs: Vec<Instr>,
    term: Option<Terminator>,
}

/// One open loop, and the blocks whose exit it still owes an answer.
///
/// A `break` knows where it is going only once the loop's `done` block exists,
/// and a `continue` only once it is settled whether the loop needs a step block
/// — both of which happen *after* the body has been lowered. So each jump
/// finishes its own block later: it records which block it left and the loop
/// patches the terminator in on the way out, the same way [`Lowering::if_stmt`]
/// patches the arms of a diamond.
#[derive(Default)]
struct LoopFrame {
    /// Blocks ending in a `break`, waiting for the loop's exit.
    breaks: Vec<BlockId>,
    /// Blocks ending in a `continue`, waiting for the loop's back edge.
    continues: Vec<BlockId>,
}

struct Lowering<'a> {
    blocks: Vec<PendingBlock>,
    vreg_names: Vec<String>,
    /// The block instructions are currently appended to.
    current: BlockId,
    /// Variable name -> virtual register, one map per open scope.
    scopes: Vec<HashMap<String, (VReg, Ty)>>,
    /// The loops enclosing the statement being lowered, innermost last.
    loops: Vec<LoopFrame>,
    /// Where the next aggregate goes, which is how much of the frame is in use
    /// *here*. It goes back down when a block ends — see [`Self::block_stmts`].
    frame_bytes: u32,
    /// The most that was ever in use at once, and so what the prologue reserves.
    ///
    /// Two counters rather than one because room can now be given back: what a
    /// block took is available again to the block after it, and only the
    /// high-water mark is a fact about the function.
    frame_peak: u32,
    /// Where a `return` copies to, for a function whose answer does not fit in
    /// a register. `None` for every other function.
    out_pointer: Option<VReg>,
    /// How many registers have borne each name, so a shadowing declaration in
    /// another scope gets a distinct dump name (`i`, then `i.1`).
    name_counts: HashMap<String, u32>,
    types: &'a Types,
    /// Every type the program has, for turning a variant name into its tag and
    /// an array type into a size.
    table: &'a TypeTable,
    /// Which lowered function each of the program's functions became, so a
    /// method's implementation can be named from its class's table.
    func_ids: &'a [FuncId],
    /// Shared with every other function: the strings all land in one section.
    strings: &'a mut Strings,
    ids: &'a HashMap<String, FuncId>,
    /// The string variables of this function nothing else can be holding, and
    /// so the ones `s = s + e` may grow where they stand. See
    /// [`owned_strings`].
    owned: HashSet<String>,
}

impl Lowering<'_> {
    fn run(mut self, decl: &FnDecl, ret: Option<Ty>, param_types: &[Ty]) -> Function {
        self.new_block(BlockKind::Entry);

        // An aggregate does not come back in a register, so the caller reserves
        // the room and hands its address in ahead of everything else. The
        // callee fills what the caller already owns, which is why returning one
        // hands nothing outward and nothing can dangle.
        let returns_aggregate = ret.is_some_and(|ty| !ty.fits_in_a_register());
        if returns_aggregate {
            let dst = self.fresh("out");
            self.emit(Instr::Param { dst, index: 0 });
            self.out_pointer = Some(dst);
        }
        let first = u32::from(returns_aggregate);

        // Parameters next, so each one is defined at the top of the function.
        //
        // An aggregate parameter is no different here: what arrived in the
        // register is its address, and the address is what the register keeps.
        let mut params = Vec::new();
        for (index, (param, ty)) in decl.params.iter().zip(param_types).enumerate() {
            let dst = self.declare(&param.name, *ty);
            self.emit(Instr::Param { dst, index: index as u32 + first });
            params.push(dst);
        }

        for stmt in &decl.body.stmts {
            self.stmt(stmt);
        }
        // Falling off the end returns nothing. For a function with a return
        // type sema has already proved this is unreachable.
        self.terminate(Terminator::Return(None));

        // Every block that stopped being the current one was finished by the
        // construct that moved away from it, and the last one was just finished
        // above. A `None` here would mean a path forgot to.
        let blocks = self
            .blocks
            .into_iter()
            .enumerate()
            .map(|(index, block)| Block {
                kind: block.kind,
                index: index as u32,
                instrs: block.instrs,
                term: block.term.unwrap_or_else(|| {
                    panic!("block {index} of `{}` was left unterminated", decl.name)
                }),
            })
            .collect();

        Function {
            name: decl.name.clone(),
            params,
            ret,
            // What the prologue reserves is the most that was ever in use at
            // once, not what is in use at the end — which is nothing, since
            // every scope has closed by now.
            frame_bytes: self.frame_peak,
            blocks: prune_unreachable(blocks),
            vreg_names: self.vreg_names,
        }
    }

    // -- blocks ------------------------------------------------------------

    /// Append a new block, make it current, and return its id. It has no
    /// terminator until [`Self::terminate`] gives it one.
    fn new_block(&mut self, kind: BlockKind) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        self.blocks.push(PendingBlock { kind, instrs: Vec::new(), term: None });
        self.current = id;
        id
    }

    fn emit(&mut self, instr: Instr) {
        self.blocks[self.current.0 as usize].instrs.push(instr);
    }

    /// Finish the current block.
    fn terminate(&mut self, term: Terminator) {
        self.finish(self.current, term);
    }

    /// Finish a block that is no longer the current one.
    fn finish(&mut self, block: BlockId, term: Terminator) {
        self.blocks[block.0 as usize].term = Some(term);
    }

    fn switch_to(&mut self, block: BlockId) {
        self.current = block;
    }

    // -- names -------------------------------------------------------------

    fn fresh(&mut self, name: &str) -> VReg {
        let reg = VReg(self.vreg_names.len() as u32);
        let count = self.name_counts.entry(name.to_string()).or_insert(0);
        let label = if *count == 0 { name.to_string() } else { format!("{name}.{count}") };
        *count += 1;
        self.vreg_names.push(label);
        reg
    }

    /// Temporaries are named after their own index, so `%t7` is always virtual
    /// register 7.
    fn fresh_temp(&mut self) -> VReg {
        let reg = VReg(self.vreg_names.len() as u32);
        self.vreg_names.push(format!("t{}", reg.0));
        reg
    }

    /// Give a name a register, and remember the type it holds.
    ///
    /// The type is carried because an array's *length* is part of it, and the
    /// length is what a bounds check and an allocation both need. Nothing else
    /// asks.
    fn declare(&mut self, name: &str, ty: Ty) -> VReg {
        let reg = self.fresh(name);
        self.scopes
            .last_mut()
            .expect("a scope is always open")
            .insert(name.to_string(), (reg, ty));
        reg
    }

    fn lookup(&self, name: &str) -> VReg {
        self.binding(name).0
    }

    fn lookup_type(&self, name: &str) -> Ty {
        self.binding(name).1
    }

    fn binding(&self, name: &str) -> (VReg, Ty) {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
            .expect("sema rejects undeclared variables")
    }

    // -- statements --------------------------------------------------------

    /// Lower a block's statements in a scope of their own.
    ///
    /// The frame is given back with the scope, so two blocks that cannot be
    /// running at the same time share their room:
    ///
    /// ```text
    /// if (c) { int[1000] a = ...; } else { int[1000] b = ...; }
    /// ```
    ///
    /// takes eight kilobytes rather than sixteen. That is sound for the same
    /// reason nothing in this language dangles: **no address ever travels
    /// outward**, and inside a function that means no frame address is ever
    /// stored into memory or kept in a variable declared outside — assignment
    /// copies. So when a block's names go out of scope, so does every way of
    /// reaching what they named.
    ///
    /// A block inside a loop is lowered once and re-entered at run time, so its
    /// room is the same room on every iteration, which is what a local in a loop
    /// body has always been.
    fn block_stmts(&mut self, block: &AstBlock) {
        let outer = self.frame_bytes;
        self.scopes.push(HashMap::new());
        for stmt in &block.stmts {
            self.stmt(stmt);
        }
        self.scopes.pop();
        self.frame_bytes = outer;
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Decl { id, name, init, .. } => {
                // The declared type, not the initialiser's: `Shape s = c;`
                // makes a `Shape`, and what it holds may be any of them.
                let ty = self.types.of(*id);
                let dst = self.declare(name, ty);
                match ty.fits_in_a_register() {
                    true => self.keep_into(dst, init),
                    // An aggregate variable owns its room. An expression that
                    // reserved some already moves in; anything else names room
                    // that belongs to something else, and is copied out of it.
                    false if builds_its_own(init) => self.expr_into(dst, init),
                    false => {
                        self.allocate_for(dst, ty);
                        self.write_through(Value::Reg(dst), init, Room::Fresh);
                    }
                }
            }
            Stmt::Assign { target, value } => match target {
                Place::Var { name, .. } => {
                    let (dst, ty) = self.binding(name);
                    // `s = s + a + b`, where nothing else can be holding `s`.
                    // The same answer, added to where `s` already is whenever
                    // the arena can still give that room back — which is what
                    // turns building a string in a loop from quadratic in
                    // *memory* into linear. See `owned_strings`.
                    let chain = match ty.fits_in_a_register() {
                        true => self.append_chain(name, value),
                        false => None,
                    };
                    match chain {
                        Some(pieces) => {
                            for piece in pieces {
                                // Whether this piece was built by this very
                                // statement, and so is nobody else's to lose.
                                let own = self.builds_a_temporary(piece);
                                let rhs = self.expr(piece);
                                self.emit(Instr::RtCall {
                                    dst: Some(dst),
                                    callee: Runtime::Append,
                                    args: vec![
                                        Value::Reg(dst),
                                        rhs,
                                        Value::Const(i64::from(own)),
                                    ],
                                });
                            }
                        }
                        // The variable keeps its register; the assignment
                        // overwrites it. An aggregate variable keeps its
                        // *room*, so the value is copied into it rather than
                        // the address swapped — anything else would make
                        // assignment aliasing.
                        None => match ty.fits_in_a_register() {
                            true => self.keep_into(dst, value),
                            false => self.write_through(Value::Reg(dst), value, Room::Named),
                        },
                    }
                }
                // Everything else names memory rather than a register, so the
                // write goes through an address.
                target => {
                    let addr = self.place_address(target);
                    self.write_through(Value::Reg(addr), value, Room::Named);
                }
            },
            // The routine answers where the list *now* is, and that answer has
            // to land back where the list is named — which is the whole reason
            // `push` takes a place rather than a value.
            Stmt::Push { target, value, .. } => {
                let (elem, bytes) = self.element_of(self.place_type(target));
                let value = self.expr(value);
                // An element too big for a register arrives as its address, and
                // what the routine does with it is a copy. The list may move
                // out from under that copy, and the block it moved *from* is
                // still there to read — the arena never gives anything back,
                // which is what makes `push(xs, xs[0])` mean what it says.
                let (callee, mut rest) = match elem.fits_in_a_register() {
                    true => (Runtime::ListPush, vec![value]),
                    // What goes in is a copy, and a copy owns nothing yet —
                    // so the routine is told whether to give it its own.
                    false => (
                        Runtime::ListPushBig,
                        vec![
                            value,
                            Value::Const(i64::from(bytes)),
                            Value::Const(i64::from(self.table.holds_a_list(elem))),
                        ],
                    ),
                };
                match target {
                    Place::Var { name, .. } => {
                        let (dst, _) = self.binding(name);
                        let mut args = vec![Value::Reg(dst)];
                        args.append(&mut rest);
                        self.emit(Instr::RtCall { dst: Some(dst), callee, args });
                    }
                    target => {
                        let addr = self.place_address(target);
                        let held = self.fresh_temp();
                        self.emit(Instr::Load { dst: held, addr: Value::Reg(addr) });
                        let grown = self.fresh_temp();
                        let mut args = vec![Value::Reg(held)];
                        args.append(&mut rest);
                        self.emit(Instr::RtCall { dst: Some(grown), callee, args });
                        self.emit(Instr::Store {
                            addr: Value::Reg(addr),
                            value: Value::Reg(grown),
                        });
                    }
                }
            }
            Stmt::Print { newline, parts, .. } => self.print_stmt(*newline, parts),
            Stmt::If { cond, then_block, else_block } => self.if_stmt(cond, then_block, else_block),
            Stmt::While { cond, body } => self.while_stmt(cond, body),
            // `for (init; cond; step) body` is exactly `init; while (cond) { body; step; }`
            // with the initialiser's variable scoped to the loop.
            Stmt::For { init, cond, step, body } => {
                let outer = self.frame_bytes;
                self.scopes.push(HashMap::new());
                self.stmt(init);
                self.loop_with_step(cond, body, Some(step));
                self.scopes.pop();
                // The initialiser's variable is scoped to the loop, so its room
                // goes back with it.
                self.frame_bytes = outer;
            }
            // An aggregate answer is copied into the room the caller reserved,
            // and the function then leaves with nothing — there is no address
            // to hand back, which is exactly why none can dangle.
            Stmt::Return { value: Some(expr), .. } if self.out_pointer.is_some() => {
                let out = self.out_pointer.expect("just matched");
                self.write_through(Value::Reg(out), expr, Room::Fresh);
                self.terminate(Terminator::Return(None));
                self.new_block(BlockKind::Unreachable);
            }
            Stmt::Return { value, .. } => {
                // Handed *outward*, so a list somebody else owns is copied
                // here rather than at the call site — which is what lets the
                // caller treat every returned list as its own. Nothing else
                // needs a register of its own: `return 0` stays an immediate.
                let value = value.as_ref().map(|expr| match self.types.of(expr.id) {
                    Ty::List(_) => {
                        let dst = self.fresh_temp();
                        self.keep_into(dst, expr);
                        Value::Reg(dst)
                    }
                    _ => self.expr(expr),
                });
                self.terminate(Terminator::Return(value));
                // Anything written after a `return` still needs somewhere to
                // go. This block has no predecessor, so it is dead code the
                // backend simply never reaches.
                self.new_block(BlockKind::Unreachable);
            }
            Stmt::Match(expr) => self.match_lowering(None, expr),
            Stmt::Break { .. } => self.loop_jump(|frame| &mut frame.breaks),
            Stmt::Continue { .. } => self.loop_jump(|frame| &mut frame.continues),
            Stmt::Call(call) => match &call.kind {
                ExprKind::MethodCall { .. } => self.method_call(None, call),
                // `read_line();` is a line skipped: the call happens, and what
                // it answered is thrown away like any other call statement's.
                ExprKind::Call { name, args, .. } if Builtin::from_name(name).is_some() => {
                    let args: Vec<Value> = args.iter().map(|arg| self.expr(arg)).collect();
                    let callee = Runtime::of(Builtin::from_name(name).expect("just matched"));
                    self.emit(Instr::RtCall { dst: None, callee, args });
                }
                _ => {
                    let (callee, args) = self.call_parts(call);
                    self.emit(Instr::Call { dst: None, callee, args });
                }
            },
        }
    }

    /// Lower a `match` into a chain of equality tests, optionally leaving what
    /// its arms produced in `dst`.
    ///
    /// A variant is its tag, so this is the same shape as `if / else if`, with
    /// one saving that only exhaustiveness makes safe: **the last arm is not
    /// tested at all.** There is nowhere else for a value to be, so the last
    /// test's failure is already the answer — and no arm has to be written for
    /// "none of the above", because there is no such case.
    ///
    /// When `dst` is given, every value arm writes it before jumping to the
    /// join — the same trick `&&` plays, and the same one a non-SSA IR is what
    /// allows. A block arm writes nothing: `sema` has established that control
    /// never reaches its end.
    ///
    /// A jump table would beat the chain on a large enum. It would need an
    /// indirect terminator, which nothing else in this IR wants yet.
    fn match_lowering(&mut self, dst: Option<VReg>, expr: &Expr) {
        let ExprKind::Match { scrutinee, arms, .. } = &expr.kind else {
            unreachable!("the caller matched a match");
        };
        let value = self.expr(scrutinee);
        let scrutinee_ty = self.types.of(scrutinee.id);
        // A boxed enum is a pointer, and every test below is about the tag it
        // points at — so that is read once here rather than once per arm. The
        // pointer itself is still what an arm's bindings are read out of.
        let tested = self.tag_of(scrutinee_ty, value);
        let before = self.current;

        // Where each arm's decision begins, so a failing test knows where to
        // send control; and the tests themselves, which cannot be finished
        // until the arm *after* them exists.
        let mut entries = Vec::new();
        let mut tests = Vec::new();
        let mut exits = Vec::new();

        for (index, arm) in arms.iter().enumerate() {
            if index + 1 == arms.len() {
                // The last arm is never tested: either it is the `_` that takes
                // whatever is left, or the domain was countable and the arms
                // before it took everything else. Control simply runs in.
                entries.push(self.new_block(BlockKind::Arm));
            } else {
                // The first test belongs to the block the scrutinee was
                // computed in; each later one gets a block of its own for the
                // previous test to fail into.
                if index > 0 {
                    self.new_block(BlockKind::Case);
                }
                entries.push(self.current);
                let cond = self.arm_test(scrutinee, tested, arm);
                let test = self.current;
                let arm_block = self.new_block(BlockKind::Arm);
                tests.push((test, cond, arm_block));
            }
            // What the pattern named is in scope for exactly this arm, so the
            // arm gets a scope of its own — which is what lets two arms use one
            // name for quite different things.
            self.scopes.push(HashMap::new());
            if let Ty::Enum(id) = scrutinee_ty {
                self.bind_arm_payload(id, value, arm);
            }
            match &arm.body {
                // A value arm leaves its answer where the join will read it.
                // Where the match is a statement `dst` is absent, and `sema`
                // has already rejected an arm that produced one.
                ArmBody::Value(value) => match dst {
                    Some(dst) => self.expr_into(dst, value),
                    None => unreachable!("sema rejects a value arm in statement position"),
                },
                ArmBody::Block(block) => self.block_stmts(block),
            }
            self.scopes.pop();
            exits.push(self.current);
        }

        let join = self.new_block(BlockKind::Join);

        // A single-variant enum has nothing to test, so control simply runs
        // into the one arm.
        if tests.is_empty() {
            self.finish(before, Terminator::Jump(entries[0]));
        }
        for (index, (test, cond, arm)) in tests.into_iter().enumerate() {
            self.finish(
                test,
                Terminator::Branch { cond, then_blk: arm, else_blk: entries[index + 1] },
            );
        }
        for exit in exits {
            self.finish(exit, Terminator::Jump(join));
        }
        self.switch_to(join);
    }

    /// The tag a `Color::Red` expression stands for, which `sema` has already
    /// established exists.
    /// Bytes in front of a boxed enum's payload, holding its tag.
    ///
    /// The same eight a string and a list spend on their length, and it sits in
    /// the same place: at the front, where the value points.
    const TAG_BYTES: u32 = 8;

    /// Build `Enum::Variant(...)` into `dst`.
    ///
    /// An enum whose variants all carry nothing *is* its tag, and costs exactly
    /// what an integer literal does — which is what every TinyC enum was until
    /// payloads existed, and what most still are.
    ///
    /// One that carries something is a **pointer** to its tag and payload in
    /// the arena, laid out like every other run of values here: the thing that
    /// tells the value apart in front, the values after it. It can be a pointer
    /// rather than something in the frame because an enum is read-only — there
    /// is no syntax that writes into a payload — so two names for one of them
    /// cannot be told apart. That is the same bargain a string strikes, and it
    /// is why an enum still fits in a register however much it carries.
    fn variant_into(&mut self, dst: VReg, expr: &Expr, args: &[Expr]) {
        let ExprKind::Variant { variant, .. } = &expr.kind else {
            unreachable!("the caller matched a variant");
        };
        let Ty::Enum(id) = self.types.of(expr.id) else {
            unreachable!("sema gives a variant its enum's type");
        };
        let info = self.table.enum_info(id);
        let tag = info.tag(variant).expect("sema rejects an unknown variant");
        if !info.carries_data() {
            return self.emit(Instr::Const { dst, val: tag });
        }

        // A variant of a boxed enum that carries nothing is the same value
        // every time it is written, so it is written down once, in `.data`.
        // Nothing else would be gained by allocating a fresh eight bytes to
        // hold a number the compiler already knows.
        if args.is_empty() {
            return self.emit(Instr::VariantAddr { dst, id, tag: tag as u32 });
        }

        let slots = info.slots() as u32;
        let bytes = Self::TAG_BYTES + slots * 8;
        self.emit(Instr::RtCall {
            dst: Some(dst),
            callee: Runtime::Alloc,
            args: vec![Value::Const(i64::from(bytes))],
        });
        self.emit(Instr::Store { addr: Value::Reg(dst), value: Value::Const(tag) });
        for (index, arg) in args.iter().enumerate() {
            let at = self.fresh_temp();
            self.emit(Instr::Field {
                dst: at,
                base: Value::Reg(dst),
                offset: Self::TAG_BYTES + index as u32 * 8,
            });
            // `Room::Fresh`, and through the same path a field takes: what goes
            // into a variant is the variant's from then on, so a list is copied
            // in rather than shared. There is no way to reach it again except
            // by matching, which copies back out.
            self.write_through(Value::Reg(at), arg, Room::Fresh);
        }
    }

    /// Whether a value of this type is a pointer to a tag rather than the tag.
    fn is_boxed_enum(&self, ty: Ty) -> bool {
        matches!(ty, Ty::Enum(id) if self.table.enum_info(id).carries_data())
    }

    /// The tag of an enum value, whichever of the two shapes it has.
    ///
    /// For an enum that carries nothing anywhere the value *is* the tag and
    /// this is the identity — which is what keeps every such program emitting
    /// exactly the instructions it emitted before payloads existed.
    fn tag_of(&mut self, ty: Ty, value: Value) -> Value {
        if !self.is_boxed_enum(ty) {
            return value;
        }
        let dst = self.fresh_temp();
        self.emit(Instr::Load { dst, addr: value });
        Value::Reg(dst)
    }

    /// Name what the matched variant carries, at the top of its arm.
    ///
    /// A list comes out as a copy, exactly as it went in. That is what makes an
    /// enum's payload the enum's: there is no way to reach the elements it
    /// holds except through a pattern, and a pattern hands back something of
    /// the arm's own.
    fn bind_arm_payload(&mut self, id: EnumId, value: Value, arm: &MatchArm) {
        let Pattern::Variant { variant, bindings, .. } = &arm.pattern else { return };
        if bindings.is_empty() {
            return;
        }
        let info = self.table.enum_info(id);
        let payload = info.variant(variant).map(|v| v.payload.clone()).unwrap_or_default();
        for (index, (name, _)) in bindings.iter().enumerate() {
            let Some(&ty) = payload.get(index) else { break };
            let dst = self.declare(name, ty);
            let at = self.fresh_temp();
            self.emit(Instr::Field {
                dst: at,
                base: value,
                offset: Self::TAG_BYTES + index as u32 * 8,
            });
            self.emit(Instr::Load { dst, addr: Value::Reg(at) });
            if let Ty::List(list) = ty {
                let elem = self.table.element(list);
                let bytes = self.table.size_of(elem);
                let deep = i64::from(self.table.holds_a_list(elem));
                self.emit(Instr::RtCall {
                    dst: Some(dst),
                    callee: Runtime::ListClone,
                    args: vec![
                        Value::Reg(dst),
                        Value::Const(i64::from(bytes)),
                        Value::Const(deep),
                    ],
                });
            }
        }
    }

    /// The tag of a variant that carries nothing, for the enums that are still
    /// a bare tag.
    fn variant_tag(&self, expr: &Expr) -> i64 {
        let ExprKind::Variant { variant, .. } = &expr.kind else {
            unreachable!("the caller matched a variant");
        };
        let Ty::Enum(id) = self.types.of(expr.id) else {
            unreachable!("sema gives a variant its enum's type");
        };
        self.table.enum_info(id).tag(variant).expect("sema rejects an unknown variant")
    }

    /// Whether the scrutinee is what this arm's pattern selects, as a `bool`.
    ///
    /// One comparison for everything a register holds — a variant's tag, a
    /// number, a character, a `bool` — because in every one of those cases the
    /// pattern is a value settled while the program was compiled. A string is
    /// the exception, and the same exception `==` already is: comparing the
    /// addresses would answer a different question, so it costs a call.
    ///
    /// Every pattern here was checked by [`crate::sema`], which is what makes
    /// the lookups `expect`s rather than diagnostics.
    fn arm_test(&mut self, scrutinee: &Expr, value: Value, arm: &MatchArm) -> Value {
        if let Pattern::Str(chars) = &arm.pattern {
            let id = self.intern(chars);
            let literal = self.fresh_temp();
            self.emit(Instr::StrAddr { dst: literal, id });
            let dst = self.fresh_temp();
            self.emit(Instr::RtCall {
                dst: Some(dst),
                callee: Runtime::StrEq,
                args: vec![value, Value::Reg(literal)],
            });
            return Value::Reg(dst);
        }
        let wanted = match &arm.pattern {
            Pattern::Variant { variant, .. } => {
                let Ty::Enum(id) = self.types.of(scrutinee.id) else {
                    unreachable!("sema rejects a variant pattern on anything but an enum");
                };
                self.table.enum_info(id).tag(variant).expect("sema rejects an unknown variant")
            }
            Pattern::Int(v) => *v,
            Pattern::Char(c) => i64::from(u32::from(*c)),
            Pattern::Bool(v) => i64::from(*v),
            Pattern::Str(_) => unreachable!("handled above"),
            // Matching is equality on a machine word, and equality on a float
            // is not that: `-0.0` and `0.0` are the same number written two
            // ways, and a NaN is equal to nothing at all. So `sema` refuses to
            // match on one rather than have this quietly mean something else.
            Pattern::Float(_) => unreachable!("sema rejects matching on a float"),
            // A catch-all is the last arm, and the last arm is the one control
            // simply runs into — so nothing ever asks it a question.
            Pattern::Wildcard => unreachable!("sema puts `_` last, where nothing is tested"),
        };
        let dst = self.fresh_temp();
        self.emit(Instr::Cmp {
            num: Num::Int,
            op: CmpOp::Eq,
            dst,
            lhs: value,
            rhs: Value::Const(wanted),
        });
        Value::Reg(dst)
    }

    /// Lower a `break` or a `continue`: hand the block it ends to the innermost
    /// loop, which will terminate it once it knows where the jump goes.
    ///
    /// `which` picks the list to join, and is the only difference between the
    /// two statements at this stage.
    fn loop_jump(&mut self, which: impl FnOnce(&mut LoopFrame) -> &mut Vec<BlockId>) {
        let leaving = self.current;
        let frame = self.loops.last_mut().expect("sema rejects a loop jump outside a loop");
        which(frame).push(leaving);
        // As after a `return`: whatever was written next still has to be
        // lowered somewhere, and nothing reaches it.
        self.new_block(BlockKind::Unreachable);
    }

    fn if_stmt(&mut self, cond: &Expr, then_block: &AstBlock, else_block: &Option<AstBlock>) {
        let cond = self.expr(cond);
        // The branch belongs to whichever block the condition was computed in.
        let entry = self.current;

        let then_id = self.new_block(BlockKind::Then);
        self.block_stmts(then_block);
        let then_exit = self.current;

        // `None` all the way through: with no `else`, there is no block to name
        // and no exit to send to the join — the branch goes straight there.
        let alternative = else_block.as_ref().map(|block| {
            let id = self.new_block(BlockKind::Else);
            self.block_stmts(block);
            (id, self.current)
        });

        let join = self.new_block(BlockKind::Join);

        self.finish(
            entry,
            Terminator::Branch {
                cond,
                then_blk: then_id,
                else_blk: alternative.map_or(join, |(id, _)| id),
            },
        );

        self.finish(then_exit, Terminator::Jump(join));
        if let Some((_, exit)) = alternative {
            self.finish(exit, Terminator::Jump(join));
        }

        self.switch_to(join);
    }

    fn while_stmt(&mut self, cond: &Expr, body: &AstBlock) {
        self.loop_with_step(cond, body, None);
    }

    /// The shape shared by `while` and `for`: a header that re-tests the
    /// condition on every iteration, a body, and an optional step run at the
    /// end of the body.
    fn loop_with_step(&mut self, cond: &Expr, body: &AstBlock, step: Option<&Stmt>) {
        let before = self.current;

        // The condition must be re-evaluated each time round, so it gets a
        // block of its own that the body jumps back to.
        let header = self.new_block(BlockKind::Loop);
        let cond = self.expr(cond);
        let header_exit = self.current;

        let body_id = self.new_block(BlockKind::Body);
        self.loops.push(LoopFrame::default());
        self.block_stmts(body);
        let frame = self.loops.pop().expect("the frame pushed just above");

        // Where the back edge starts, and where a `continue` goes.
        //
        // A `for` has to run its step at the end of *every* iteration, the ones
        // a `continue` cuts short included — so when one exists the step needs a
        // block of its own to jump to. When none does the step simply ends the
        // body, and the `for` lowers to exactly the `while` it desugars into.
        let latch = match step {
            Some(step) if !frame.continues.is_empty() => {
                let body_exit = self.current;
                let latch = self.new_block(BlockKind::Step);
                self.stmt(step);
                self.finish(body_exit, Terminator::Jump(latch));
                latch
            }
            Some(step) => {
                self.stmt(step);
                header
            }
            None => header,
        };
        // The step may itself open blocks — `i = i + 1` does not, but
        // `ok = ok && f()` would — so where it ended is not where it began.
        let latch_exit = self.current;

        let after = self.new_block(BlockKind::Done);

        self.finish(before, Terminator::Jump(header));
        self.finish(header_exit, Terminator::Branch { cond, then_blk: body_id, else_blk: after });
        // The back edge: this is what makes liveness need a fixpoint.
        self.finish(latch_exit, Terminator::Jump(header));

        for block in frame.continues {
            self.finish(block, Terminator::Jump(latch));
        }
        for block in frame.breaks {
            self.finish(block, Terminator::Jump(after));
        }

        self.switch_to(after);
    }

    // -- arrays ------------------------------------------------------------

    /// The address of `array[index]`, in a fresh register.
    ///
    /// One `Elem` and nothing else: the multiply-and-add that turns an index
    /// into an offset is an addressing mode on x86, not arithmetic, so it is
    /// not lowered as arithmetic and never picks up the overflow guard that
    /// `Bin` carries.
    fn element_address(&mut self, array: &Expr, index: &Expr) -> VReg {
        let ty = self.types.of(array.id);
        let base = self.expr(array);
        let (len, scale) = self.shape_of(ty, base);
        let index = self.expr(index);
        let dst = self.fresh_temp();
        self.emit(Instr::Elem { dst, base, index, len, scale });
        dst
    }

    /// How long the thing in `base` is and how wide each of its elements is.
    ///
    /// For an array both are facts about the *type*, known here with nothing
    /// computed. For a string the width still is, but the length is a load —
    /// which is the whole difference between the two, and the reason a constant
    /// index into a string is checked at run time like any other.
    fn shape_of(&mut self, ty: Ty, base: Value) -> (Value, u32) {
        match ty {
            Ty::Array(id) => {
                let info = self.table.array(id);
                (Value::Const(i64::from(info.len)), self.table.size_of(info.elem))
            }
            Ty::Str => (self.length_of(base), CHAR_BYTES),
            // A list holds its elements where it is, so one of objects scales
            // by the whole object — the same arithmetic an array of them does,
            // with a length that has to be read rather than known.
            Ty::List(_) => {
                let (_, bytes) = self.element_of(ty);
                (self.length_of(base), bytes)
            }
            _ => unreachable!("sema rejects indexing anything without elements"),
        }
    }

    /// What one element of this list type is, and how many bytes it takes.
    ///
    /// The second half is what the routines have to be told: they walk the
    /// elements rather than reading one, so they cannot work it out.
    fn element_of(&self, list: Ty) -> (Ty, u32) {
        let Ty::List(id) = list else {
            unreachable!("sema rejects a list operation on anything but a list");
        };
        let elem = self.table.element(id);
        (elem, self.table.size_of(elem))
    }

    /// The count in front of a string's characters or a list's elements.
    fn length_of(&mut self, of: Value) -> Value {
        let dst = self.fresh_temp();
        self.emit(Instr::Count { dst, of });
        Value::Reg(dst)
    }

    /// Lower a value that is about to be **kept** — put in a variable, or
    /// handed back from a function.
    ///
    /// A list is one pointer, so lowering one straight into a variable would
    /// give two names to one run of elements. Every other type in the language
    /// either cannot be written to, so the sharing could not be observed, or is
    /// too big to fit in a register and is copied by the code above. A list is
    /// neither, so this is where "assignment copies, never aliases" is paid
    /// for.
    ///
    /// Something that built its own is nobody else's already, and moves in as
    /// it is — which is why a function that returns a list costs no copy at the
    /// call site: it cloned at its own `return`, if it had anything to clone.
    fn keep_into(&mut self, dst: VReg, expr: &Expr) {
        self.expr_into(dst, expr);
        let ty = self.types.of(expr.id);
        if matches!(ty, Ty::List(_)) && !builds_its_own(expr) {
            let (elem, bytes) = self.element_of(ty);
            self.emit(Instr::RtCall {
                dst: Some(dst),
                callee: Runtime::ListClone,
                args: vec![
                    Value::Reg(dst),
                    Value::Const(i64::from(bytes)),
                    // The elements are copies too. If one of them holds a list
                    // of its own, copying its bytes shared that list, and the
                    // clone has to go one level further in.
                    Value::Const(i64::from(self.table.holds_a_list(elem))),
                ],
            });
        }
    }

    /// `s = s + a + b + …` taken apart into the pieces to add, in order, for a
    /// string `s` that [`owned_strings`] proved nothing else can be holding.
    ///
    /// The chain matters as much as the single step. `+` leans left, so
    /// `s = s + string(i) + ","` is `s = ((s + string(i)) + ",")` — its
    /// outermost operand is not the variable, and matching only `s = s + e`
    /// would leave the commonest way of building a line quadratic.
    ///
    /// The shape has to start at the variable. `s = "a" + s` is not it —
    /// prepending cannot grow a block where it stands, whatever is known about
    /// it — and neither is `s = t + e`, which is somebody else's string.
    ///
    /// **No piece may mention `s` itself.** Appending them one at a time makes
    /// the intermediate values visible where the single expression would have
    /// read the variable once, at the start; `s = s + f(s)` would hand `f` a
    /// string the original never would.
    fn append_chain<'e>(&self, name: &str, value: &'e Expr) -> Option<Vec<&'e Expr>> {
        if !self.owned.contains(name) {
            return None;
        }
        let mut pieces = Vec::new();
        let mut at = value;
        while let ExprKind::Bin { op: BinOp::Add, lhs, rhs } = &at.kind {
            if self.types.of(lhs.id) != Ty::Str {
                return None;
            }
            pieces.push(&**rhs);
            at = lhs;
        }
        let ExprKind::Var(left) = &at.kind else { return None };
        if left != name || pieces.is_empty() {
            return None;
        }
        if pieces.iter().any(|piece| mentions(piece, name)) {
            return None;
        }
        pieces.reverse();
        Some(pieces)
    }

    /// Whether this expression allocates the string it produces *here*, so that
    /// what it produced is this statement's own and nobody else's.
    ///
    /// The narrow question the arena needs in order to hand a block back: a
    /// temporary built and consumed inside one statement is the one thing the
    /// bump pointer can safely retract. A literal is deliberately not one — it
    /// lives in `.data` and there is nothing to retract — and neither is a
    /// variable, a call, an element or a field, all of which hand on a string
    /// that already had a name.
    fn builds_a_temporary(&self, expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::Bin { op: BinOp::Add, lhs, .. } => self.types.of(lhs.id) == Ty::Str,
            ExprKind::Convert { to: Prim::Str, .. } => true,
            ExprKind::Call { name, args, .. } => {
                name == Builtin::ReadLine.name() && args.is_empty()
            }
            _ => false,
        }
    }

    /// Whether this is one of the operators a string gives a second meaning to,
    /// and so one that becomes a call rather than an instruction.
    fn is_string_op(&self, expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::Bin { op: BinOp::Add, lhs, .. } | ExprKind::Cmp { lhs, .. } => {
                self.types.of(lhs.id) == Ty::Str
            }
            _ => false,
        }
    }

    /// Lower `a + b` or `a == b` on two strings into `dst`.
    fn string_op_into(&mut self, dst: VReg, expr: &Expr) {
        let (op, lhs, rhs) = match &expr.kind {
            ExprKind::Bin { lhs, rhs, .. } => (None, lhs, rhs),
            ExprKind::Cmp { op, lhs, rhs } => (Some(*op), lhs, rhs),
            _ => unreachable!("the caller checked this is a string operator"),
        };
        let args = vec![self.expr(lhs), self.expr(rhs)];

        let Some(op) = op else {
            self.emit(Instr::RtCall { dst: Some(dst), callee: Runtime::Concat, args });
            return;
        };

        // The routine answers whether the two are the same, and `!=` is that
        // question read the other way round — so one routine serves both, and
        // the negation costs the comparison against zero that `!` already is.
        if op == CmpOp::Eq {
            self.emit(Instr::RtCall { dst: Some(dst), callee: Runtime::StrEq, args });
            return;
        }
        let same = self.fresh_temp();
        self.emit(Instr::RtCall { dst: Some(same), callee: Runtime::StrEq, args });
        self.emit(Instr::Cmp {
            num: Num::Int,
            op: CmpOp::Eq,
            dst,
            lhs: Value::Reg(same),
            rhs: Value::Const(0),
        });
    }

    /// Lower `int(c)` or `char(n)` into `dst`.
    ///
    /// One direction is free and the other is not, and the asymmetry is the
    /// design: every character has a code point, but not every number names a
    /// character. So only that direction can fail — and it fails where it was
    /// written, rather than handing on a value nothing else in the language
    /// could have produced.
    fn convert_into(&mut self, dst: VReg, to: Prim, value: &Expr) {
        let from = self.types.of(value.id);
        let src = self.expr(value);
        // A constant `sema` has already accepted needs no check at run time,
        // which is the same bargain a constant index strikes.
        let settled = matches!(src, Value::Const(c) if is_scalar_value(c));

        // Between `int` and `float` the word itself changes, so this is the one
        // conversion that is neither a routine nor a move. A constant is
        // settled here for the same reason a constant character is: what is
        // left to check at run time is only ever a value the running program
        // alone knows.
        let cast = match (from, to) {
            (Ty::Int, Prim::Float) => Some(Num::Float),
            (Ty::Float, Prim::Int) => Some(Num::Int),
            _ => None,
        };
        if let Some(to) = cast {
            match (to, src) {
                (Num::Float, Value::Const(c)) => {
                    self.emit(Instr::Const { dst, val: (c as f64).to_bits() as i64 })
                }
                (Num::Int, Value::Const(c)) if fits_in_an_int(f64::from_bits(c as u64)) => {
                    self.emit(Instr::Const { dst, val: f64::from_bits(c as u64) as i64 })
                }
                _ => self.emit(Instr::Cast { dst, to, src }),
            }
            return;
        }

        let callee = match (from, to) {
            (Ty::Int, Prim::Char) if !settled => Runtime::CheckChar,
            (Ty::Str, Prim::Int) => Runtime::StrToInt,
            (Ty::Char, Prim::Str) => Runtime::CharToStr,
            (Ty::Int, Prim::Str) => Runtime::IntToStr,
            (Ty::List(_), Prim::Str) => Runtime::CharsToStr,
            // A code point *is* the character's representation, so reading one
            // as the other moves nothing at all.
            _ => return self.emit(Instr::Copy { dst, src }),
        };
        self.emit(Instr::RtCall { dst: Some(dst), callee, args: vec![src] });
    }

    /// Room in the frame for a value of `ty`, with its address in `dst`.
    fn allocate_for(&mut self, dst: VReg, ty: Ty) {
        let bytes = self.table.size_of(ty);
        self.allocate(dst, bytes);
    }

    /// Put `value` where `addr` points, whichever kind of value it is.
    ///
    /// A scalar is one store. An aggregate does not fit in a register, so what
    /// the expression produced is an *address* and the value is copied out of
    /// it — which is what makes assignment value semantics rather than
    /// aliasing, and what carries an object's vtable pointer along with it.
    fn write_through(&mut self, addr: Value, value: &Expr, room: Room) {
        let ty = self.types.of(value.id);
        if ty.fits_in_a_register() {
            // A list fits, and is the one thing that fits and can still be
            // written to — so storing what the expression produced would give
            // this room a second name for somebody else's elements. The same
            // reason `keep_into` exists, one indirection further along, and
            // what makes a list *field* copy like every other field.
            if matches!(ty, Ty::List(_)) {
                let held = self.fresh_temp();
                self.keep_into(held, value);
                self.emit(Instr::Store { addr, value: Value::Reg(held) });
                return;
            }
            let value = self.expr(value);
            self.emit(Instr::Store { addr, value });
            return;
        }
        // A literal has no room of its own until something gives it some, so
        // where the room here is new it may as well be this room — see [`Room`]
        // for why "new" is the condition and not merely "aggregate".
        if matches!(room, Room::Fresh) {
            match &value.kind {
                ExprKind::New { fields, .. } => {
                    let Ty::Class(id) = ty else {
                        unreachable!("sema gives an object literal its class's type");
                    };
                    return self.fill_object(addr, id, fields);
                }
                // A *list* literal is not one of these: its elements live in the
                // arena, so what it produces is an address rather than room, and
                // it never reaches here — a list fits in a register.
                ExprKind::Array { elements, .. } => return self.fill_array(addr, ty, elements),
                _ => {}
            }
        }
        let src = self.expr(value);
        let bytes = self.table.size_of(ty);
        self.emit(Instr::CopyBytes { dst: addr, src, bytes });
        self.fixup_after_copy(addr, ty);
    }

    /// Give a fresh copy its own elements, where what was copied may hold a
    /// list.
    ///
    /// Emitted only where [`TypeTable::holds_a_list`] says it can be needed, so
    /// a program whose classes hold nothing but numbers and objects carries
    /// none of this — not the instruction, not the routines, not the word in
    /// front of its vtables.
    fn fixup_after_copy(&mut self, at: Value, ty: Ty) {
        if !self.table.holds_a_list(ty) {
            return;
        }
        // An array was copied whole, so every element of it is a fresh copy.
        // Anything else is one value.
        let (count, stride) = match ty {
            Ty::Array(id) => {
                let info = self.table.array(id);
                (i64::from(info.len), self.table.size_of(info.elem))
            }
            _ => (1, 0),
        };
        self.emit(Instr::Fixup { at, count: Value::Const(count), stride });
    }

    /// Put a class's vtable pointer and every field where `at` points.
    ///
    /// The vtable pointer goes in first, at offset 0. It is what makes the
    /// object *this* class rather than merely its shape, and it is what travels
    /// with a copy — so the object is a complete one of its class from the
    /// first instruction.
    fn fill_object(&mut self, at: Value, id: ClassId, fields: &[FieldInit]) {
        let info = self.table.class(id).clone();
        let vptr = self.fresh_temp();
        self.emit(Instr::VTable { dst: vptr, class: id });
        self.emit(Instr::Store { addr: at, value: Value::Reg(vptr) });

        for init in fields {
            let offset =
                info.field(&init.name).expect("sema rejects an unknown field").offset;
            let addr = self.fresh_temp();
            self.emit(Instr::Field { dst: addr, base: at, offset });
            // A field of an object being built is room nothing can name, so
            // whatever fills it may be built there directly.
            self.write_through(Value::Reg(addr), &init.value, Room::Fresh);
        }
    }

    /// Put every element of an array or list literal where `at` points.
    ///
    /// The room is filled in written order. An element's value may mention the
    /// array being built only in ways `sema` has already ruled out, so the order
    /// is not observable.
    fn fill_array(&mut self, at: Value, ty: Ty, elements: &[Expr]) {
        let (len, scale) = self.shape_of(ty, at);
        for (index, element) in elements.iter().enumerate() {
            let addr = self.fresh_temp();
            self.emit(Instr::Elem {
                dst: addr,
                base: at,
                index: Value::Const(index as i64),
                len,
                scale,
            });
            self.write_through(Value::Reg(addr), element, Room::Fresh);
        }
    }

    /// The address of `object.field`, which is the object's plus a fixed offset.
    ///
    /// The same `Elem` an array index uses, with the offset in place of the
    /// index — a field's place in an object is settled once, by `sema`, so
    /// there is nothing to check and nothing to compute.
    fn field_address(&mut self, object: Value, class: ClassId, name: &str) -> VReg {
        let offset = self
            .table
            .class(class)
            .field(name)
            .expect("sema rejects an unknown field")
            .offset;
        let dst = self.fresh_temp();
        self.emit(Instr::Field { dst, base: object, offset });
        dst
    }

    /// Lower `receiver.method(args)`.
    ///
    /// The receiver is the first argument, and also where the vtable comes
    /// from. Which of the two calls goes out is settled here: a class nothing
    /// derives from has one possible implementation, so the indirection would
    /// decide a question with one answer — whole-program compilation is what
    /// makes that knowable.
    fn method_call(&mut self, dst: Option<VReg>, expr: &Expr) {
        let ExprKind::MethodCall { receiver, name, args, .. } = &expr.kind else {
            unreachable!("the caller matched a method call");
        };
        let Ty::Class(id) = self.types.of(receiver.id) else {
            unreachable!("sema rejects a method call on anything but an object");
        };

        let method = self.table.class(id).method(name).expect("sema rejects an unknown method");
        let (slot, function) = (method.slot as u32, method.function);
        let sealed = self.table.is_sealed(id);

        // The receiver is the first written argument, so it is evaluated before
        // the rest.
        let object = self.expr(receiver);
        let mut values = vec![object];
        values.extend(args.iter().map(|arg| self.expr(arg)));

        // An aggregate answer goes in room the caller reserves, whose address
        // leads the arguments — ahead of the receiver, since the receiver is
        // an ordinary argument and this is not.
        let ty = self.types.of(expr.id);
        let dst = match (dst, ty.fits_in_a_register()) {
            (Some(dst), false) => {
                self.allocate_for(dst, ty);
                values.insert(0, Value::Reg(dst));
                None
            }
            (dst, _) => dst,
        };

        if sealed {
            self.emit(Instr::Call { dst, callee: self.func_ids[function], args: values });
        } else {
            self.emit(Instr::CallVirtual { dst, slot, receiver: object, args: values });
        }
    }

    /// The type a place has, read off the chain of names that leads to it.
    fn place_type(&self, place: &Place) -> Ty {
        match place {
            Place::Var { name, .. } => self.lookup_type(name),
            Place::Element { base, .. } => match self.place_type(base) {
                Ty::Array(id) => self.table.array(id).elem,
                // Reached once a list can hold objects: `xs[i].f` asks what
                // `xs[i]` is before it can ask where its field is.
                Ty::List(id) => self.table.element(id),
                _ => unreachable!("sema rejects indexing anything but an array or a list"),
            },
            Place::Field { base, name, .. } => match self.place_type(base) {
                Ty::Class(id) => {
                    self.table.class(id).field(name).expect("sema rejects an unknown field").ty
                }
                _ => unreachable!("sema rejects a field on anything but an object"),
            },
        }
    }

    /// The address a place names, for a write.
    ///
    /// Only a variable has no address — it is a register — and the caller has
    /// already dealt with that case.
    fn place_address(&mut self, place: &Place) -> VReg {
        match place {
            Place::Var { .. } => unreachable!("a variable is a register, not an address"),
            Place::Element { base, index, .. } => {
                let ty = self.place_type(base);
                let object = self.place_value(base);
                let (len, scale) = self.shape_of(ty, object);
                let index = self.expr(index);
                let dst = self.fresh_temp();
                self.emit(Instr::Elem { dst, base: object, index, len, scale });
                dst
            }
            Place::Field { base, name, .. } => {
                let Ty::Class(id) = self.place_type(base) else {
                    unreachable!("sema rejects a field on anything but an object");
                };
                let object = self.place_value(base);
                self.field_address(object, id, name)
            }
        }
    }

    /// What a place *holds*, which for anything but a variable means reading it.
    ///
    /// An aggregate is the exception, and it is the same exception an element
    /// and a field make when they are read as expressions: what a place of that
    /// type holds does not fit in a register, so its *address* is the value.
    /// Reading eight bytes out of it would produce an object's vtable pointer
    /// rather than the object, which is the address of nothing at all.
    fn place_value(&mut self, place: &Place) -> Value {
        match place {
            Place::Var { name, .. } => Value::Reg(self.lookup(name)),
            other => {
                let addr = self.place_address(other);
                if !self.place_type(other).fits_in_a_register() {
                    return Value::Reg(addr);
                }
                let dst = self.fresh_temp();
                self.emit(Instr::Load { dst, addr: Value::Reg(addr) });
                Value::Reg(dst)
            }
        }
    }

    /// Reserve `bytes` of frame and put their address in `dst`.
    ///
    /// Saturating, like the object layout it is the sum of: a function with
    /// four gigabytes of locals is answered by [`too_much_stack`], not by a
    /// total that wrapped and looked reasonable.
    fn allocate(&mut self, dst: VReg, bytes: u32) {
        let offset = self.frame_bytes;
        self.frame_bytes = self.frame_bytes.saturating_add(bytes);
        self.frame_peak = self.frame_peak.max(self.frame_bytes);
        self.emit(Instr::Frame { dst, offset });
    }

    // -- expressions -------------------------------------------------------

    /// The callee and evaluated arguments of a call expression.
    fn call_parts(&mut self, call: &Expr) -> (FuncId, Vec<Value>) {
        let ExprKind::Call { name, args, .. } = &call.kind else {
            unreachable!("sema guarantees this is a call");
        };
        // Arguments are evaluated left to right, before the call itself; a
        // nested call therefore finishes first and leaves its result in a
        // temporary that the outer call reads as an operand.
        let args: Vec<Value> = args.iter().map(|arg| self.expr(arg)).collect();
        let callee = self.ids[name.as_str()];
        (callee, args)
    }

    /// Lower an expression whose result must land in `dst`.
    fn expr_into(&mut self, dst: VReg, expr: &Expr) {
        match &expr.kind {
            ExprKind::Int(v) => self.emit(Instr::Const { dst, val: *v }),
            ExprKind::Float(v) => self.emit(Instr::Const { dst, val: v.to_bits() as i64 }),
            ExprKind::Bool(v) => self.emit(Instr::Const { dst, val: i64::from(*v) }),
            ExprKind::Variant { args, .. } => self.variant_into(dst, expr, args),
            // The array's room is reserved first, then filled: an element's
            // value may itself mention the array being built only in ways sema
            // has already ruled out, so the order is not observable.
            // The object's room is reserved at its hierarchy's size, then its
            // vtable pointer goes in at offset 0 and its fields after — so it
            // is a complete object of its class from the first instruction.
            ExprKind::New { fields, .. } => {
                let Ty::Class(id) = self.types.of(expr.id) else {
                    unreachable!("sema gives an object literal its class's type");
                };
                self.allocate(dst, self.table.class(id).storage);
                self.fill_object(Value::Reg(dst), id, fields);
            }
            ExprKind::Field { object, name, .. } => {
                let Ty::Class(id) = self.types.of(object.id) else {
                    unreachable!("sema rejects a field on anything but an object");
                };
                let object = self.expr(object);
                let addr = Value::Reg(self.field_address(object, id, name));
                // A field that is itself an aggregate *is* its address, exactly
                // as an element of one is: it lives inside the object, so there
                // is nothing to read out and nothing to copy until somebody
                // says where to.
                match self.types.of(expr.id).fits_in_a_register() {
                    true => self.emit(Instr::Load { dst, addr }),
                    false => self.emit(Instr::Copy { dst, src: addr }),
                }
            }
            ExprKind::MethodCall { .. } => self.method_call(Some(dst), expr),
            // A list literal reserves nothing in the frame: its elements live
            // in the arena, because how many there will be by the end is not a
            // question this function can answer.
            ExprKind::Array { elements, .. } if matches!(self.types.of(expr.id), Ty::List(_)) => {
                let (_, bytes) = self.element_of(self.types.of(expr.id));
                let len = Value::Const(elements.len() as i64);
                self.emit(Instr::RtCall {
                    dst: Some(dst),
                    callee: Runtime::ListNew,
                    args: vec![len, Value::Const(i64::from(bytes))],
                });
                for (index, element) in elements.iter().enumerate() {
                    let addr = self.fresh_temp();
                    self.emit(Instr::Elem {
                        dst: addr,
                        base: Value::Reg(dst),
                        index: Value::Const(index as i64),
                        len,
                        scale: bytes,
                    });
                    self.write_through(Value::Reg(addr), element, Room::Fresh);
                }
            }
            ExprKind::Array { elements, .. } => {
                let ty = self.types.of(expr.id);
                self.allocate_for(dst, ty);
                self.fill_array(Value::Reg(dst), ty, elements);
            }
            ExprKind::Index { array, index, .. } => {
                let of = self.types.of(array.id);
                let addr = self.element_address(array, index);
                let addr = Value::Reg(addr);
                // An aggregate element *is* its address; only a value that
                // fits in a register has to be read out — and a string's
                // characters are the one thing narrower than a register.
                match (of, self.types.of(expr.id).fits_in_a_register()) {
                    (Ty::Str, _) => self.emit(Instr::LoadChar { dst, addr }),
                    (_, true) => self.emit(Instr::Load { dst, addr }),
                    (_, false) => self.emit(Instr::Copy { dst, src: addr }),
                }
            }
            ExprKind::Len { .. } => {
                let src = self.expr(expr);
                self.emit(Instr::Copy { dst, src });
            }
            ExprKind::Char(c) => self.emit(Instr::Const { dst, val: i64::from(*c as u32) }),
            ExprKind::Convert { to, value, .. } => self.convert_into(dst, *to, value),
            ExprKind::Str(bytes) => {
                let id = self.intern(bytes);
                self.emit(Instr::StrAddr { dst, id });
            }
            // `+` on two strings, and `==` on two strings, are the only
            // operators that are a *loop* rather than an instruction, so they
            // are the only ones that leave through a call.
            ExprKind::Bin { .. } | ExprKind::Cmp { .. } if self.is_string_op(expr) => {
                self.string_op_into(dst, expr)
            }
            ExprKind::Bin { op, lhs, rhs } => {
                let num = self.num_of(expr);
                let lhs = self.expr(lhs);
                let rhs = self.expr(rhs);
                match fold_bin(num, *op, lhs, rhs) {
                    Some(val) => self.emit(Instr::Const { dst, val }),
                    None => self.emit(Instr::Bin { num, op: *op, dst, lhs, rhs }),
                }
            }
            // A comparison answers a bool whatever it compared, so what says
            // how to read the operands is an *operand's* type and not this
            // expression's.
            ExprKind::Cmp { op, lhs, rhs } => {
                let num = self.num_of(lhs);
                let lhs = self.expr(lhs);
                let rhs = self.expr(rhs);
                match fold_cmp(num, *op, lhs, rhs) {
                    Some(val) => self.emit(Instr::Const { dst, val }),
                    None => self.emit(Instr::Cmp { num, op: *op, dst, lhs, rhs }),
                }
            }
            ExprKind::Neg(operand) => {
                let num = self.num_of(expr);
                let val = self.expr(operand);
                match val {
                    Value::Const(c) => self.emit(Instr::Const { dst, val: negate_const(num, c) }),
                    val => self.emit(Instr::Bin {
                        num,
                        op: BinOp::Sub,
                        dst,
                        lhs: zero_to_subtract_from(num),
                        rhs: val,
                    }),
                }
            }
            ExprKind::Not(operand) => {
                let (num, op, lhs, rhs) = self.negated(operand);
                match fold_cmp(num, op, lhs, rhs) {
                    Some(val) => self.emit(Instr::Const { dst, val }),
                    None => self.emit(Instr::Cmp { num, op, dst, lhs, rhs }),
                }
            }
            ExprKind::Logic { op, lhs, rhs } => self.logic_into(dst, *op, lhs, rhs),
            ExprKind::Match { .. } => self.match_lowering(Some(dst), expr),
            // A built-in is a call with no body to compile, so it goes out as
            // the routine it is. Nothing else about the call site differs.
            ExprKind::Call { name, args, .. } if Builtin::from_name(name).is_some() => {
                let args: Vec<Value> = args.iter().map(|arg| self.expr(arg)).collect();
                let callee = Runtime::of(Builtin::from_name(name).expect("just matched"));
                self.emit(Instr::RtCall { dst: Some(dst), callee, args });
            }
            ExprKind::Call { .. } => {
                let (callee, mut args) = self.call_parts(expr);
                let ty = self.types.of(expr.id);
                if ty.fits_in_a_register() {
                    self.emit(Instr::Call { dst: Some(dst), callee, args });
                    return;
                }
                // The room is the caller's, and its address goes in ahead of
                // the written arguments. `dst` ends up naming it, so what the
                // caller gets back is room it already owned.
                self.allocate_for(dst, ty);
                args.insert(0, Value::Reg(dst));
                self.emit(Instr::Call { dst: None, callee, args });
            }
            ExprKind::Var(_) => {
                let src = self.expr(expr);
                self.emit(Instr::Copy { dst, src });
            }
        }
    }

    /// Lower `lhs && rhs` or `lhs || rhs` into `dst`.
    fn logic_into(&mut self, dst: VReg, op: LogicOp, lhs: &Expr, rhs: &Expr) {
        let cond = self.expr(lhs);
        if matches!(cond, Value::Const(_)) {
            // A known left operand settles the expression at compile time.
            // Dropping the right one is not just an optimisation here: with
            // `false && f()`, *not* calling `f` is the semantics.
            match fold_logic(op, cond) {
                Some(val) => self.emit(Instr::Const { dst, val }),
                // It decided nothing: `true && e` is simply `e`.
                None => self.expr_into(dst, rhs),
            }
            return;
        }
        self.logic_branch(dst, op, cond, rhs);
    }

    /// The half of `&&` and `||` that really branches, given a left operand
    /// whose value is not known.
    ///
    /// There is no `and` or `or` instruction, and there could not usefully be
    /// one: short circuiting *is* control flow, so this produces the same
    /// diamond an `if` does. Both arms write `dst`, which is only expressible
    /// because the IR is not in SSA form — see the module comment.
    fn logic_branch(&mut self, dst: VReg, op: LogicOp, cond: Value, rhs: &Expr) {
        // The value the left operand hands back when it decides on its own.
        let settled = op.short_circuit();
        // The branch belongs to whichever block the left operand ended in.
        let entry = self.current;

        // The arm the branch continues into is laid out first, so the backend
        // reaches it by falling through instead of by jumping.
        let (then_blk, else_blk, rhs_exit, short) = match op {
            LogicOp::And => {
                let (rhs_blk, rhs_exit) = self.logic_rhs(dst, rhs);
                let short = self.logic_short(dst, settled);
                (rhs_blk, short, rhs_exit, short)
            }
            LogicOp::Or => {
                let short = self.logic_short(dst, settled);
                let (rhs_blk, rhs_exit) = self.logic_rhs(dst, rhs);
                (short, rhs_blk, rhs_exit, short)
            }
        };

        let join = self.new_block(BlockKind::Join);
        self.finish(entry, Terminator::Branch { cond, then_blk, else_blk });
        self.finish(rhs_exit, Terminator::Jump(join));
        self.finish(short, Terminator::Jump(join));
        self.switch_to(join);
    }

    /// The arm that evaluates the right operand, as `(entry, exit)`: lowering it
    /// can open blocks of its own, so where it ends is not where it began.
    fn logic_rhs(&mut self, dst: VReg, rhs: &Expr) -> (BlockId, BlockId) {
        let id = self.new_block(BlockKind::Rhs);
        self.expr_into(dst, rhs);
        (id, self.current)
    }

    /// The arm the short circuit takes, holding nothing but the answer the left
    /// operand already gave.
    fn logic_short(&mut self, dst: VReg, val: i64) -> BlockId {
        let id = self.new_block(BlockKind::Short);
        self.emit(Instr::Const { dst, val });
        id
    }

    /// The comparison that computes `!operand`, as `(op, lhs, rhs)`.
    ///
    /// There is no `not` instruction, and none is wanted: `!x` *is* `x == 0`,
    /// which folds and fuses into a branch like any other comparison. When the
    /// operand is itself a comparison the negation goes one better and inverts
    /// it in place, so `!(a < b)` costs the single `cmp` that `a >= b` does
    /// instead of a comparison followed by a comparison against its result.
    fn negated(&mut self, operand: &Expr) -> (Num, CmpOp, Value, Value) {
        if let ExprKind::Cmp { op, lhs, rhs } = &operand.kind {
            let num = self.num_of(lhs);
            let lhs = self.expr(lhs);
            let rhs = self.expr(rhs);
            return (num, op.negate(), lhs, rhs);
        }
        // Anything else `!` can be applied to is a bool, and `!b` is `b == 0`.
        (Num::Int, CmpOp::Eq, self.expr(operand), Value::Const(0))
    }

    /// How an expression's value is read, which is a question about its type.
    fn num_of(&self, expr: &Expr) -> Num {
        Num::of(self.types.of(expr.id))
    }

    /// Lower an expression used as an operand, producing a value to read.
    fn expr(&mut self, expr: &Expr) -> Value {
        match &expr.kind {
            // Literals stay immediates so the backend can fold them into the
            // instruction that consumes them.
            ExprKind::Int(v) => Value::Const(*v),
            // The bits of the double, which is what a `float` is everywhere
            // past this point — see [`Num`].
            ExprKind::Float(v) => Value::Const(v.to_bits() as i64),
            ExprKind::Bool(v) => Value::Const(i64::from(*v)),
            ExprKind::Var(name) => Value::Reg(self.lookup(name)),
            // A variant of an enum that carries nothing anywhere *is* its tag,
            // so it needs no more machinery than an integer literal does —
            // which is the whole reason such an enum costs the backend nothing.
            // One that carries something has to be built, and building takes a
            // register to build into.
            ExprKind::Variant { .. } if !self.is_boxed_enum(self.types.of(expr.id)) => {
                Value::Const(self.variant_tag(expr))
            }
            ExprKind::Variant { args, .. } => {
                let dst = self.fresh_temp();
                self.variant_into(dst, expr, args);
                Value::Reg(dst)
            }
            // A length is a fact about a type, so it is a constant here and
            // costs nothing at all — `i < len(xs)` compares against a literal.
            ExprKind::Char(c) => Value::Const(i64::from(*c as u32)),
            ExprKind::Len { array, .. } => match self.types.of(array.id) {
                // An array's length is a fact about its type, so it costs
                // nothing at all — `i < len(xs)` compares against a literal.
                // A string's is a load, because a string that had to be built
                // could not have told the compiler how long it would be.
                Ty::Array(id) => Value::Const(i64::from(self.table.array(id).len)),
                _ => {
                    let str = self.expr(array);
                    self.length_of(str)
                }
            },
            ExprKind::Index { array, index, .. } => {
                let of = self.types.of(array.id);
                let addr = self.element_address(array, index);
                if of != Ty::Str && !self.types.of(expr.id).fits_in_a_register() {
                    return Value::Reg(addr);
                }
                let dst = self.fresh_temp();
                let addr = Value::Reg(addr);
                match of {
                    Ty::Str => self.emit(Instr::LoadChar { dst, addr }),
                    _ => self.emit(Instr::Load { dst, addr }),
                }
                Value::Reg(dst)
            }
            ExprKind::Neg(operand) => {
                let num = self.num_of(expr);
                match self.expr(operand) {
                    // An operand that is already a literal folds, and so does
                    // the whole tree above it: `-(2 * 3)` never reaches an
                    // instruction.
                    Value::Const(c) => Value::Const(negate_const(num, c)),
                    val => {
                        let dst = self.fresh_temp();
                        self.emit(Instr::Bin {
                            num,
                            op: BinOp::Sub,
                            dst,
                            lhs: zero_to_subtract_from(num),
                            rhs: val,
                        });
                        Value::Reg(dst)
                    }
                }
            }
            ExprKind::Bin { .. } | ExprKind::Cmp { .. } if self.is_string_op(expr) => {
                let dst = self.fresh_temp();
                self.string_op_into(dst, expr);
                Value::Reg(dst)
            }
            ExprKind::Bin { op, lhs, rhs } => {
                let num = self.num_of(expr);
                let lhs = self.expr(lhs);
                let rhs = self.expr(rhs);
                if let Some(val) = fold_bin(num, *op, lhs, rhs) {
                    return Value::Const(val);
                }
                let dst = self.fresh_temp();
                self.emit(Instr::Bin { num, op: *op, dst, lhs, rhs });
                Value::Reg(dst)
            }
            ExprKind::Cmp { op, lhs, rhs } => {
                let num = self.num_of(lhs);
                let lhs = self.expr(lhs);
                let rhs = self.expr(rhs);
                if let Some(val) = fold_cmp(num, *op, lhs, rhs) {
                    return Value::Const(val);
                }
                let dst = self.fresh_temp();
                self.emit(Instr::Cmp { num, op: *op, dst, lhs, rhs });
                Value::Reg(dst)
            }
            // `!` is a comparison, so it takes the same path as one.
            ExprKind::Not(operand) => {
                let (num, op, lhs, rhs) = self.negated(operand);
                if let Some(val) = fold_cmp(num, op, lhs, rhs) {
                    return Value::Const(val);
                }
                let dst = self.fresh_temp();
                self.emit(Instr::Cmp { num, op, dst, lhs, rhs });
                Value::Reg(dst)
            }
            // A left operand that settles the answer leaves nothing to branch
            // on, and so nothing to hold in a register either.
            ExprKind::Logic { op, lhs, rhs } => {
                let cond = self.expr(lhs);
                if matches!(cond, Value::Const(_)) {
                    return match fold_logic(*op, cond) {
                        Some(val) => Value::Const(val),
                        None => self.expr(rhs),
                    };
                }
                let dst = self.fresh_temp();
                self.logic_branch(dst, *op, cond, rhs);
                Value::Reg(dst)
            }
            ExprKind::Str(_)
            | ExprKind::Convert { .. }
            | ExprKind::Call { .. }
            | ExprKind::Match { .. }
            | ExprKind::Array { .. }
            | ExprKind::New { .. }
            | ExprKind::Field { .. }
            | ExprKind::MethodCall { .. } => {
                let dst = self.fresh_temp();
                self.expr_into(dst, expr);
                Value::Reg(dst)
            }
        }
    }

    /// `print(...)` and `println(...)`: one write per thing written.
    ///
    /// The parts were settled by the parser, so nothing here reads a `%`. What
    /// this stage adds is the line ending, and that is where the two spellings
    /// stop being two statements: a `println` is a `print` with one more piece
    /// of text at the end. The same desugaring `for` gets, one stage after the
    /// tree has been dumped.
    ///
    /// **Every value is evaluated before anything is written.** A `print` is
    /// written like a call and is read like one, so its arguments go first —
    /// otherwise `println("n: %d", noisy())` would put whatever `noisy` writes
    /// in the middle of this line rather than before it. The values stay live
    /// across the writes, which is the register allocator's business.
    fn print_stmt(&mut self, newline: bool, parts: &[PrintPart]) {
        let mut written: Vec<Instr> = Vec::new();
        for part in parts {
            if let PrintPart::Value(expr) | PrintPart::Spec { expr, .. } = part {
                let ty = self.types.of(expr.id);
                let val = self.expr(expr);
                // What an enum prints is the *name* of its variant, which the
                // backend looks up by tag. A boxed one is a pointer, so the tag
                // is read here — and the backend goes on doing exactly what it
                // did, with the number it always expected.
                let val = self.tag_of(ty, val);
                written.push(Instr::Print { ty, val, newline: false });
            }
        }

        let mut written = written.into_iter();
        let mut text: Vec<char> = Vec::new();
        // Whether *this* statement's last piece was a value. Not the same as
        // finding no text left over: `println()` has none either, and the write
        // it would attach a line ending to belongs to the statement before it.
        let mut ended_with_a_value = false;
        for part in parts {
            match part {
                PrintPart::Text(chars) => {
                    text.extend(chars);
                    ended_with_a_value = false;
                }
                _ => {
                    self.flush_text(&mut text);
                    self.emit(written.next().expect("one per value part"));
                    ended_with_a_value = true;
                }
            }
        }
        // Where the last piece written was a value, the line ends with it: the
        // backend reaches for a format that already ends in one, so `println(n)`
        // is a single call. Where it was text — `println("done")` — the newline
        // joins that text below, exactly as it always has.
        if newline && ended_with_a_value {
            self.end_the_line_written_last();
            return;
        }
        if newline {
            text.push('\n');
        }
        self.flush_text(&mut text);
    }

    /// Make the write just emitted end its line.
    ///
    /// Only ever called straight after emitting one, which is what makes the
    /// last instruction in the block certain to be it.
    fn end_the_line_written_last(&mut self) {
        match self.blocks[self.current.0 as usize].instrs.last_mut() {
            Some(Instr::Print { newline, .. }) => *newline = true,
            other => unreachable!("a value was just written, not {other:?}"),
        }
    }

    /// Write out the literal text collected so far, if there is any.
    ///
    /// Collected rather than written piece by piece, so that the newline of
    /// `println("done")` joins the word in front of it and the whole line
    /// leaves in one call.
    fn flush_text(&mut self, text: &mut Vec<char>) {
        if text.is_empty() {
            return;
        }
        let id = self.intern_text(std::mem::take(text).into_iter().collect());
        self.emit(Instr::PrintText { id });
    }

    /// The same for a run of literal text, kept in its own table because it is
    /// laid out differently: a string literal is characters four bytes each
    /// with a count in front, and this is the UTF-8 `printf` will be handed.
    fn intern_text(&mut self, text: String) -> TextId {
        if let Some(&id) = self.strings.text_ids.get(&text) {
            return id;
        }
        let id = TextId(self.strings.texts.len() as u32);
        self.strings.texts.push(text.clone());
        self.strings.text_ids.insert(text, id);
        id
    }

    fn intern(&mut self, chars: &[char]) -> StrId {
        if let Some(&id) = self.strings.ids.get(chars) {
            return id;
        }
        let id = StrId(self.strings.chars.len() as u32);
        self.strings.chars.push(chars.to_vec());
        self.strings.ids.insert(chars.to_vec(), id);
        id
    }
}

/// Whether lowering this expression *reserves* the room its value lives in,
/// rather than naming room something else owns.
///
/// The three that do are the two literals and a call, which fills room the
/// caller reserved for it. Everything else — a variable, a field, an element —
/// points at somebody's else's, so assigning it means copying.
/// The string variables of one function that **nothing else can be holding**.
///
/// A string is read-only, so sharing one is free and the compiler has never had
/// to ask this before: two names for the same characters cannot be told apart.
/// One operation would tell them apart, and it is the one worth having —
/// growing a string *where it stands*, which bumps a count at `[p-8]` that
/// every other name for it can see.
///
/// So this asks the narrow question that makes that operation safe, and asks it
/// the cautious way round: a name is owned only if it can be *proved* to be,
/// and anything the analysis does not recognise means no. Being wrong in the
/// permissive direction would be memory corruption; being wrong in the strict
/// direction costs a program the optimisation it would have got.
///
/// A name is owned when all of this holds:
///
/// * It is a **local**, not a parameter — a parameter is a string the caller
///   still holds — and it is declared exactly once in the function, so the name
///   cannot mean two different variables in two blocks.
/// * Every value it is ever given is **freshly built**: a concat, a conversion,
///   a `read_line`. A literal counts too, and is the reason the runtime keeps a
///   check of its own: a literal lives in `.data`, which is not the arena, so
///   the in-place path simply never fires for one.
/// * It is never **kept** anywhere else: not assigned to another variable, not
///   passed to a function, not returned, not put in a list, an array or a
///   field. Reading it — its length, one of its characters, printing it,
///   comparing it, joining it to something — is not keeping it.
///
/// What this is *not* is ownership in the type system. Nothing about the
/// language changes, no program is refused that was not refused before, and a
/// name that fails any of these tests simply gets the code it got yesterday.
fn owned_strings(function: &FnDecl, types: &Types) -> HashSet<String> {
    let mut facts = Owned {
        types,
        declared: HashMap::new(),
        fresh: HashMap::new(),
        escaped: HashSet::new(),
    };
    // A parameter is the caller's, whatever the body does with it.
    for param in &function.params {
        facts.escaped.insert(param.name.clone());
    }
    facts.block(&function.body);

    facts
        .declared
        .iter()
        .filter(|(name, times)| {
            // Declared once, so the name means one variable everywhere.
            **times == 1
                && facts.fresh.get(*name).copied().unwrap_or(false)
                && !facts.escaped.contains(*name)
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// The sweep [`owned_strings`] is made of.
struct Owned<'a> {
    types: &'a Types,
    /// How many times each name is declared, so shadowing can be ruled out.
    declared: HashMap<String, usize>,
    /// Whether every value the name has been given so far was freshly built.
    fresh: HashMap<String, bool>,
    /// Names whose *pointer* reaches somewhere that outlives the read.
    escaped: HashSet<String>,
}

impl Owned<'_> {
    fn block(&mut self, block: &AstBlock) {
        for stmt in &block.stmts {
            self.stmt(stmt);
        }
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Decl { id, name, init, .. } => {
                *self.declared.entry(name.clone()).or_insert(0) += 1;
                let fresh = self.types.of(*id) == Ty::Str && self.is_fresh(init);
                let entry = self.fresh.entry(name.clone()).or_insert(true);
                *entry &= fresh;
                self.expr(init, Kept::Yes);
            }
            Stmt::Assign { target, value } => {
                if let Place::Var { name, .. } = target {
                    let fresh = self.types.of(value.id) == Ty::Str && self.is_fresh(value);
                    let entry = self.fresh.entry(name.clone()).or_insert(true);
                    *entry &= fresh;
                } else {
                    self.place(target);
                }
                self.expr(value, Kept::Yes);
            }
            // What is pushed is kept by the list; where it is pushed is a place.
            Stmt::Push { target, value, .. } => {
                self.place(target);
                self.expr(value, Kept::Yes);
            }
            // Handed outward, so whoever called this function keeps it.
            Stmt::Return { value, .. } => {
                if let Some(value) = value {
                    self.expr(value, Kept::Yes);
                }
            }
            Stmt::Print { parts, .. } => {
                for part in parts {
                    match part {
                        PrintPart::Text(_) => {}
                        PrintPart::Value(expr) | PrintPart::Spec { expr, .. } => {
                            self.expr(expr, Kept::No)
                        }
                    }
                }
            }
            Stmt::If { cond, then_block, else_block } => {
                self.expr(cond, Kept::No);
                self.block(then_block);
                if let Some(block) = else_block {
                    self.block(block);
                }
            }
            Stmt::While { cond, body } => {
                self.expr(cond, Kept::No);
                self.block(body);
            }
            Stmt::For { init, cond, step, body } => {
                self.stmt(init);
                self.expr(cond, Kept::No);
                self.stmt(step);
                self.block(body);
            }
            Stmt::Match(expr) | Stmt::Call(expr) => self.expr(expr, Kept::No),
            Stmt::Break { .. } | Stmt::Continue { .. } => {}
        }
    }

    /// The index expressions inside a place, which are read and nothing more.
    fn place(&mut self, place: &Place) {
        match place {
            Place::Var { .. } => {}
            Place::Element { base, index, .. } => {
                self.place(base);
                self.expr(index, Kept::No);
            }
            Place::Field { base, .. } => self.place(base),
        }
    }

    /// Walk an expression, knowing whether the value it produces is kept.
    ///
    /// The recursion is where the whole judgement lives: `y = x` keeps `x`,
    /// while `y = x + "!"` does not — the concat reads `x` and builds something
    /// else. So an operator passes [`Kept::No`] down to its operands, and
    /// anything that stores a value away passes [`Kept::Yes`].
    ///
    /// Every shape that could keep a value is listed. A shape this does not
    /// recognise cannot arise, but if one ever did, the wildcard treats its
    /// children as kept — the cautious answer.
    fn expr(&mut self, expr: &Expr, kept: Kept) {
        match &expr.kind {
            ExprKind::Var(name) => {
                if matches!(kept, Kept::Yes) {
                    self.escaped.insert(name.clone());
                }
            }
            // A string operator reads its operands and allocates its answer.
            ExprKind::Bin { lhs, rhs, .. } | ExprKind::Cmp { lhs, rhs, .. } => {
                self.expr(lhs, Kept::No);
                self.expr(rhs, Kept::No);
            }
            ExprKind::Logic { lhs, rhs, .. } => {
                self.expr(lhs, Kept::No);
                self.expr(rhs, Kept::No);
            }
            ExprKind::Neg(inner) | ExprKind::Not(inner) => self.expr(inner, Kept::No),
            ExprKind::Len { array, .. } => self.expr(array, Kept::No),
            ExprKind::Index { array, index } => {
                self.expr(array, Kept::No);
                self.expr(index, Kept::No);
            }
            // A conversion builds a new value out of what it reads.
            ExprKind::Convert { value, .. } => self.expr(value, Kept::No),
            // A callee may keep anything it is handed, and there is no
            // whole-program analysis here to say otherwise.
            ExprKind::Call { args, .. } => {
                for arg in args {
                    self.expr(arg, Kept::Yes);
                }
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                self.expr(receiver, Kept::Yes);
                for arg in args {
                    self.expr(arg, Kept::Yes);
                }
            }
            // Both put what they are given *into* something that outlives the
            // expression.
            ExprKind::Array { elements, .. } => {
                for element in elements {
                    self.expr(element, Kept::Yes);
                }
            }
            ExprKind::New { fields, .. } => {
                for field in fields {
                    self.expr(&field.value, Kept::Yes);
                }
            }
            // A match hands its arm's value on as its own, so the arms inherit
            // whatever was asked of the match. The scrutinee is only compared.
            ExprKind::Match { scrutinee, arms, .. } => {
                self.expr(scrutinee, Kept::No);
                for arm in arms {
                    match &arm.body {
                        ArmBody::Value(value) => self.expr(value, kept),
                        ArmBody::Block(block) => self.block(block),
                    }
                }
            }
            ExprKind::Field { object, .. } => self.expr(object, Kept::No),
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Char(_)
            | ExprKind::Bool(_)
            | ExprKind::Variant { .. } => {}
        }
    }

    /// Whether this expression *builds* the string it produces, rather than
    /// handing on one that already existed somewhere.
    ///
    /// Every routine named here allocates a block of its own and gives it to
    /// nobody else. A literal is not built at all, and is included for a
    /// different reason: it lives in `.data`, so the in-place path cannot fire
    /// for it and there is nothing to be wrong about.
    fn is_fresh(&self, expr: &Expr) -> bool {
        match &expr.kind {
            // `a + b` on strings, which is a `concat` and allocates.
            ExprKind::Bin { op: BinOp::Add, lhs, .. } => self.types.of(lhs.id) == Ty::Str,
            // `string(n)`, `string(c)` and `string(cs)` all allocate; the
            // conversions that do not produce a string never reach here.
            ExprKind::Convert { to: Prim::Str, .. } => true,
            ExprKind::Str(_) => true,
            // `read_line()` seals a fresh list of characters into a string. Its
            // name cannot mean anything else — `sema` refuses to let a program
            // redefine a built-in.
            ExprKind::Call { name, args, .. } => {
                name == Builtin::ReadLine.name() && args.is_empty()
            }
            // A match is fresh when every arm is. Anything else — a variable, a
            // call, an element, a field — hands on a string that already had a
            // name somewhere.
            ExprKind::Match { arms, .. } => arms.iter().all(|arm| match &arm.body {
                ArmBody::Value(value) => self.is_fresh(value),
                ArmBody::Block(_) => false,
            }),
            _ => false,
        }
    }
}

/// Whether the value an expression produces is stored somewhere that outlives
/// the expression itself.
#[derive(Clone, Copy)]
enum Kept {
    Yes,
    No,
}

/// Whether this expression reads the variable `name` anywhere inside it.
///
/// Asked by [`Lowering::append_chain`], which turns one expression into several
/// statements and so has to be sure nothing in it could tell the difference.
/// The one shape not walked is a block arm, which is answered `true` without
/// looking: it may hold statements, and the cautious answer costs nothing but
/// an optimisation.
fn mentions(expr: &Expr, name: &str) -> bool {
    let mut found = false;
    let mut visit = |e: &Expr| found |= mentions(e, name);
    match &expr.kind {
        ExprKind::Var(other) => return other == name,
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::Bool(_)
        | ExprKind::Variant { .. } => {}
        ExprKind::Neg(inner) | ExprKind::Not(inner) => visit(inner),
        ExprKind::Bin { lhs, rhs, .. }
        | ExprKind::Cmp { lhs, rhs, .. }
        | ExprKind::Logic { lhs, rhs, .. } => {
            visit(lhs);
            visit(rhs);
        }
        ExprKind::Index { array, index } => {
            visit(array);
            visit(index);
        }
        ExprKind::Len { array, .. } => visit(array),
        ExprKind::Convert { value, .. } => visit(value),
        ExprKind::Field { object, .. } => visit(object),
        ExprKind::Array { elements, .. } => elements.iter().for_each(visit),
        ExprKind::New { fields, .. } => fields.iter().for_each(|f| visit(&f.value)),
        ExprKind::Call { args, .. } => args.iter().for_each(visit),
        ExprKind::MethodCall { receiver, args, .. } => {
            visit(receiver);
            args.iter().for_each(visit);
        }
        ExprKind::Match { scrutinee, arms, .. } => {
            visit(scrutinee);
            let arms_mention = arms.iter().any(|arm| match &arm.body {
                ArmBody::Value(value) => mentions(value, name),
                ArmBody::Block(_) => true,
            });
            found |= arms_mention;
        }
    }
    found
}

fn builds_its_own(expr: &Expr) -> bool {
    matches!(
        expr.kind,
        ExprKind::New { .. }
            | ExprKind::Array { .. }
            | ExprKind::Call { .. }
            | ExprKind::MethodCall { .. }
    )
}

/// Evaluate `lhs op rhs` now, when both are already known.
///
/// Answers `None` whenever the machine would not agree with the answer: an
/// operation the CPU would trap on stays an instruction, so the program still
/// fails where it was written instead of in the compiler.
///
/// [`crate::sema`] has usually rejected such a program already — it evaluates
/// the same constants through the same [`BinOp::apply`]. What reaches here is
/// what sema could not see, such as an operand that only became a constant
/// during lowering.
/// What `-x` subtracts `x` from, which is the zero of whichever kind of number
/// it is.
///
/// A float's is **negative** zero, and that is not a flourish: `-0.0 - x` is
/// exactly `-x` for every value there is, while `0.0 - x` answers `+0.0` where
/// `x` was `+0.0` and `-0.0` was meant. The two zeroes compare equal, so the
/// difference is invisible until something divides by the result — and then it
/// is the difference between `+∞` and `-∞`.
fn zero_to_subtract_from(num: Num) -> Value {
    match num {
        Num::Int => Value::Const(0),
        Num::Float => Value::Const((-0.0f64).to_bits() as i64),
    }
}

/// The same negation, done here because the operand was already known.
fn negate_const(num: Num, value: i64) -> i64 {
    match num {
        // `-i64::MIN` does not fit, and wrapping is what the machine does with
        // it — where `sema` has not already refused the program for it.
        Num::Int => value.wrapping_neg(),
        Num::Float => (-f64::from_bits(value as u64)).to_bits() as i64,
    }
}

/// A float folds by the same arithmetic the machine would do — IEEE-754 in
/// double precision, which is exactly what Rust's `f64` is — so folding one
/// cannot come to a different answer from running it. It never refuses: too
/// large is an infinity and zero into zero is a NaN, and both are values.
pub(crate) fn fold_bin(num: Num, op: BinOp, lhs: Value, rhs: Value) -> Option<i64> {
    let (Value::Const(a), Value::Const(b)) = (lhs, rhs) else { return None };
    match num {
        Num::Int => op.apply(a, b),
        Num::Float => {
            let (a, b) = (f64::from_bits(a as u64), f64::from_bits(b as u64));
            let answer = match op {
                BinOp::Add => a + b,
                BinOp::Sub => a - b,
                BinOp::Mul => a * b,
                BinOp::Div => a / b,
                BinOp::Rem => unreachable!("sema rejects `%` on a float"),
            };
            Some(answer.to_bits() as i64)
        }
    }
}

/// What a short-circuiting operator answers when its left operand alone decides.
///
/// Unlike [`fold_bin`] and [`fold_cmp`] this looks at one operand, because that
/// is the whole point: `false && x` is false and `true || x` is true whatever
/// `x` would have been — the same value in both cases, which is what
/// [`LogicOp::short_circuit`] reports. `None` means the right operand still has
/// to run, and covers both "the left one is unknown" and "the left one is known
/// but decided nothing".
fn fold_logic(op: LogicOp, lhs: Value) -> Option<i64> {
    let Value::Const(c) = lhs else { return None };
    let settled = op.short_circuit();
    ((c != 0) == (settled != 0)).then_some(settled)
}

/// The same for a comparison, whose result is the 0 or 1 a `bool` is.
///
/// Comparing two floats is **not** comparing their bits: `-0.0` and `0.0` are
/// equal and spelled differently, and a NaN is equal to nothing including
/// itself. Rust's `f64` operators say exactly that, which is also what the
/// machine's `ucomisd` says, so the two agree by construction.
pub(crate) fn fold_cmp(num: Num, op: CmpOp, lhs: Value, rhs: Value) -> Option<i64> {
    let (Value::Const(a), Value::Const(b)) = (lhs, rhs) else { return None };
    let answer = match num {
        Num::Int => match op {
            CmpOp::Eq => a == b,
            CmpOp::Ne => a != b,
            CmpOp::Lt => a < b,
            CmpOp::Le => a <= b,
            CmpOp::Gt => a > b,
            CmpOp::Ge => a >= b,
        },
        Num::Float => {
            let (a, b) = (f64::from_bits(a as u64), f64::from_bits(b as u64));
            match op {
                CmpOp::Eq => a == b,
                CmpOp::Ne => a != b,
                CmpOp::Lt => a < b,
                CmpOp::Le => a <= b,
                CmpOp::Gt => a > b,
                CmpOp::Ge => a >= b,
            }
        }
    };
    Some(i64::from(answer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lexer, parser, sema};

    fn lower_src(src: &str) -> Program {
        try_lower(src).expect("the frames should fit")
    }

    fn try_lower(src: &str) -> Result<Program> {
        let ast = parser::parse(&lexer::lex(src).unwrap()).unwrap();
        let types = sema::check(&ast, 4).unwrap();
        lower(&ast, &types)
    }

    /// Lower a `main` body and return that one function.
    fn lower_main(body: &str) -> Program {
        lower_src(&format!("fn main() {{\n{body}\n}}\n"))
    }

    /// The dump of a single-function program, without its signature line and
    /// trailing blank, so the existing block-shape assertions stay readable.
    fn body_dump(program: &Program) -> String {
        let dump = program.dump();
        let start = dump.find(":\n").expect("a signature line") + 2;
        dump[start..].trim_end().to_string() + "\n"
    }

    fn labels(function: &Function) -> Vec<String> {
        function.blocks.iter().map(|b| b.label()).collect()
    }

    #[test]
    fn lowers_the_sample_program() {
        let ir = lower_main("int x = 10;\nint y = 20;\nstring s = \"hi\";\nprint(x + y);\nprint(s);");
        assert_eq!(
            body_dump(&ir),
            concat!(
                "entry0:\n",
                "  0  %x = const 10\n",
                "  1  %y = const 20\n",
                "  2  %s = straddr str0\n",
                "  3  %t3 = add %x, %y\n",
                "  4  print int %t3\n",
                "  5  print string %s\n",
                "  6  return\n",
            )
        );
    }

    #[test]
    fn an_assignment_writes_the_variables_own_register() {
        // No `%n.1`: with control flow a variable must have one home, so the
        // second write targets the same register.
        let ir = lower_main("int n = 1;\nn = n + 41;\nprint(n);");
        assert_eq!(
            body_dump(&ir),
            concat!(
                "entry0:\n",
                "  0  %n = const 1\n",
                "  1  %n = add %n, 41\n",
                "  2  print int %n\n",
                "  3  return\n",
            )
        );
    }

    #[test]
    fn an_if_produces_a_diamond() {
        let ir = lower_main("int n = 0;\nif (n < 1) {\n  n = 2;\n} else {\n  n = 3;\n}\nprint(n);");
        let main = &ir.functions[0];
        assert_eq!(labels(main), vec!["entry0", "then1", "else2", "join3"]);
        assert!(matches!(main.blocks[0].term, Terminator::Branch { .. }));
        assert!(matches!(main.blocks[1].term, Terminator::Jump(BlockId(3))));
        assert!(matches!(main.blocks[2].term, Terminator::Jump(BlockId(3))));
    }

    #[test]
    fn an_if_without_else_branches_straight_to_the_join() {
        let ir = lower_main("int n = 0;\nif (n < 1) {\n  n = 2;\n}\nprint(n);");
        let main = &ir.functions[0];
        assert_eq!(labels(main), vec!["entry0", "then1", "join2"]);
        match main.blocks[0].term {
            Terminator::Branch { then_blk, else_blk, .. } => {
                assert_eq!((then_blk, else_blk), (BlockId(1), BlockId(2)));
            }
            ref other => panic!("expected a branch, got {other:?}"),
        }
    }

    #[test]
    fn a_while_loop_closes_a_back_edge() {
        let ir = lower_main("int i = 0;\nwhile (i < 3) {\n  i = i + 1;\n}\nprint(i);");
        let main = &ir.functions[0];
        assert_eq!(labels(main), vec!["entry0", "loop1", "body2", "done3"]);
        // The body jumps back to the header, which re-tests the condition.
        assert!(matches!(main.blocks[2].term, Terminator::Jump(BlockId(1))));
        assert!(matches!(main.blocks[0].term, Terminator::Jump(BlockId(1))));
    }

    #[test]
    fn a_for_loop_desugars_into_the_same_shape() {
        let with_for = lower_main("for (int i = 0; i < 3; i = i + 1) {\n  print(i);\n}");
        let with_while = lower_main("int i = 0;\nwhile (i < 3) {\n  print(i);\n  i = i + 1;\n}");
        assert_eq!(with_for.dump(), with_while.dump());
    }

    // -- classes -----------------------------------------------------------

    #[test]
    fn an_object_is_room_a_vtable_pointer_and_its_fields() {
        let ir = lower_src(
            "class Circle {\n  int r;\n  fn area(self) -> int { return self.r; }\n}\n\
             fn main() {\n  Circle c = Circle { r: 5 };\n  print(c.r);\n}",
        );
        let main = ir.functions.iter().find(|f| f.name == "main").expect("main survives");
        let text: Vec<String> =
            main.blocks[0].instrs.iter().map(|i| ir.instr_text(main, i)).collect();
        assert_eq!(
            text,
            vec![
                "%c = frame 0",
                "%t1 = vtable Circle",
                "store %c, %t1",
                "%t2 = field %c + 8",
                "store %t2, 5",
                "%t4 = field %c + 8",
                "%t3 = load %t4",
                "print int %t3",
            ]
        );
    }

    #[test]
    fn a_field_of_the_base_comes_before_one_of_the_subclass() {
        // The prefix rule, which is what makes an upcast free.
        let ir = lower_src(
            "class Base {\n  int a;\n}\nclass Derived : Base {\n  int b;\n}\n\
             fn main() {\n  Derived d = Derived { a: 1, b: 2 };\n  print(d.a + d.b);\n}",
        );
        let offsets: Vec<u32> =
            ir.table.class(ClassId(1)).fields.iter().map(|f| f.offset).collect();
        // The vtable pointer takes offset 0.
        assert_eq!(offsets, vec![8, 16]);
    }

    #[test]
    fn an_aggregate_field_is_its_address_rather_than_something_to_read() {
        // The rule an element already followed: a value too big for a register
        // *is* where it lives. Reading eight bytes out of `s.b` would produce
        // the inner object's vtable pointer, and writing through that would
        // land in the vtable rather than in the object.
        let ir = lower_src(
            "class Point {\n  int x;\n}\n\
             class Segment {\n  Point a;\n  Point b;\n}\n\
             fn main() {\n  \
             Segment s = Segment { a: Point { x: 1 }, b: Point { x: 2 } };\n  \
             s.b.x = 3;\n  \
             print(s.b.x);\n}",
        );
        // `b` starts past the whole of `a`, rather than one register after it.
        let offsets: Vec<u32> =
            ir.table.class(ClassId(1)).fields.iter().map(|f| f.offset).collect();
        assert_eq!(offsets, vec![8, 24]);

        let main = ir.functions.iter().find(|f| f.name == "main").expect("main survives");
        let loads =
            main.blocks[0].instrs.iter().filter(|i| matches!(i, Instr::Load { .. })).count();
        // One, and it is the `print`. Reaching `s.b` to write through it reads
        // nothing at all.
        assert_eq!(loads, 1, "{}", ir.dump());
    }

    #[test]
    fn writing_through_an_element_that_is_an_object_reaches_the_element() {
        let ir = lower_src(
            "class Point {\n  int x;\n}\n\
             fn main() {\n  \
             Point[2] ps = [Point { x: 1 }, Point { x: 2 }];\n  \
             ps[1].x = 7;\n  \
             print(ps[1].x);\n}",
        );
        let main = ir.functions.iter().find(|f| f.name == "main").expect("main survives");
        let loads =
            main.blocks[0].instrs.iter().filter(|i| matches!(i, Instr::Load { .. })).count();
        assert_eq!(loads, 1, "{}", ir.dump());
    }

    #[test]
    fn a_call_on_a_class_with_subclasses_goes_through_the_vtable() {
        let ir = lower_src(
            "class Shape {\n  fn area(self) -> int { return 0; }\n}\n\
             class Circle : Shape {\n  int r;\n  fn area(self) -> int { return self.r; }\n}\n\
             fn report(Shape s) {\n  print(s.area());\n}\n\
             fn main() {\n  report(Circle { r: 1 });\n}",
        );
        let report = ir.functions.iter().find(|f| f.name == "report").expect("report survives");
        assert!(
            report.blocks[0].instrs.iter().any(|i| matches!(i, Instr::CallVirtual { slot: 0, .. })),
            "{}",
            ir.dump()
        );
    }

    #[test]
    fn a_call_on_a_class_with_none_is_settled_at_compile_time() {
        // Whole-program compilation is what makes this knowable: nothing can
        // derive from `Point` afterwards, so there is one answer and no reason
        // to ask at run time.
        let ir = lower_src(
            "class Point {\n  int x;\n  fn get(self) -> int { return self.x; }\n}\n\
             fn main() {\n  Point p = Point { x: 1 };\n  print(p.get());\n}",
        );
        let main = ir.functions.iter().find(|f| f.name == "main").expect("main survives");
        assert!(
            !main.blocks[0].instrs.iter().any(|i| matches!(i, Instr::CallVirtual { .. })),
            "{}",
            ir.dump()
        );
        assert!(
            main.blocks[0].instrs.iter().any(|i| matches!(i, Instr::Call { .. })),
            "{}",
            ir.dump()
        );
    }

    #[test]
    fn a_subclass_vtable_is_its_base_with_the_overrides_replaced() {
        let ir = lower_src(
            "class Shape {\n  fn area(self) -> int { return 0; }\n  \
             fn name(self) -> string { return \"shape\"; }\n}\n\
             class Circle : Shape {\n  int r;\n  fn area(self) -> int { return self.r; }\n}\n\
             fn main() {\n  Circle c = Circle { r: 1 };\n  print(c.area());\n}",
        );
        // Same two slots in the same order; only the first was overridden.
        let shape = ir.table.class(ClassId(0));
        let circle = ir.table.class(ClassId(1));
        let names: Vec<&str> = circle.methods.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, vec!["area", "name"]);
        assert_eq!(circle.methods[0].slot, shape.methods[0].slot);
        assert_ne!(circle.methods[0].function, shape.methods[0].function);
        assert_eq!(circle.methods[1].function, shape.methods[1].function);
    }

    #[test]
    fn storage_is_the_biggest_in_the_hierarchy() {
        // What will let a value of a base class hold any of its subclasses.
        let ir = lower_src(
            "class Shape {\n  fn area(self) -> int { return 0; }\n}\n\
             class Circle : Shape {\n  int r;\n  fn area(self) -> int { return self.r; }\n}\n\
             class Rect : Shape {\n  int w;\n  int h;\n  \
             fn area(self) -> int { return self.w; }\n}\n\
             fn main() {\n  Rect r = Rect { w: 1, h: 2 };\n  print(r.area());\n}",
        );
        // `Shape` is 8 on its own; the biggest thing that *is* one is `Rect`.
        assert_eq!(ir.table.class(ClassId(0)).size, 8);
        assert_eq!(ir.table.class(ClassId(0)).storage, 24);
        assert_eq!(ir.table.class(ClassId(2)).storage, 24);
    }

    #[test]
    fn a_method_is_named_after_its_class() {
        // Two classes may both have a `go`, so the flat list of callables has
        // to keep them apart — and so do their symbols.
        let ir = lower_src(
            "class A {\n  fn go(self) -> int { return 1; }\n}\n\
             class B {\n  fn go(self) -> int { return 2; }\n}\n\
             fn main() {\n  A a = A { };\n  B b = B { };\n  print(a.go() + b.go());\n}",
        );
        let names: Vec<&str> = ir.functions.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"A$go"), "{names:?}");
        assert!(names.contains(&"B$go"), "{names:?}");
    }

    #[test]
    fn a_class_nothing_builds_keeps_none_of_its_methods() {
        // Making an object is what makes its methods callable, and the only
        // thing that does.
        let ir = lower_src(
            "class Used {\n  fn f(self) -> int { return 1; }\n}\n\
             class Unused {\n  fn f(self) -> int { return 2; }\n}\n\
             fn main() {\n  Used u = Used { };\n  print(u.f());\n}",
        );
        let names: Vec<&str> = ir.functions.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"Used$f"), "{names:?}");
        assert!(!names.contains(&"Unused$f"), "{names:?}");
    }

    // -- arrays ------------------------------------------------------------

    #[test]
    fn an_array_is_room_in_the_frame_and_a_store_per_element() {
        let ir = lower_main("int[3] xs = [10, 20, 30];\nprint(xs[0]);");
        assert_eq!(
            body_dump(&ir),
            concat!(
                "entry0:\n",
                "  0  %xs = frame 0\n",
                "  1  %t1 = elem %xs[0] of 3 by 8\n",
                "  2  store %t1, 10\n",
                "  3  %t2 = elem %xs[1] of 3 by 8\n",
                "  4  store %t2, 20\n",
                "  5  %t3 = elem %xs[2] of 3 by 8\n",
                "  6  store %t3, 30\n",
                "  7  %t4 = elem %xs[0] of 3 by 8\n",
                "  8  %t5 = load %t4\n",
                "  9  print int %t5\n",
                " 10  return\n",
            )
        );
    }

    #[test]
    fn the_frame_is_sized_by_what_the_function_declared() {
        let ir = lower_main("int[3] a = [1, 2, 3];\nbool[2] b = [true, false];\nprint(a[0]);");
        // Two arrays, five elements, eight bytes each.
        assert_eq!(ir.functions[0].frame_bytes, 40);
        // And they do not overlap.
        let offsets: Vec<u32> = ir.functions[0].blocks[0]
            .instrs
            .iter()
            .filter_map(|i| match i {
                Instr::Frame { offset, .. } => Some(*offset),
                _ => None,
            })
            .collect();
        assert_eq!(offsets, vec![0, 24]);
    }

    /// A literal at a place that already has an address is built *there*.
    ///
    /// It used to be built in room of its own and copied, which cost the room
    /// for the whole call — the room stayed reserved whether or not anything
    /// still needed it — and a `CopyBytes` nobody asked for.
    #[test]
    fn a_literal_in_a_place_that_has_room_reserves_none_of_its_own() {
        let ir = lower_src(
            "class P { int[2] xs; }\nfn main() {\n  P p = P { xs: [1, 2] };\n  print(p.xs[0]);\n}",
        );
        let main = &ir.functions[0];
        // One reservation, for the object. The array goes inside it.
        let frames: Vec<u32> = main.blocks[0]
            .instrs
            .iter()
            .filter_map(|i| match i {
                Instr::Frame { offset, .. } => Some(*offset),
                _ => None,
            })
            .collect();
        assert_eq!(frames, vec![0], "the literal reserved room of its own: {}", ir.dump());
        // `P` is a vtable pointer and two ints, and that is the whole frame.
        assert_eq!(main.frame_bytes, 24);
        // And nothing is copied, because nothing was built anywhere else.
        assert!(
            !main.blocks[0].instrs.iter().any(|i| matches!(i, Instr::CopyBytes { .. })),
            "{}",
            ir.dump()
        );
    }

    /// The same literal *assigned* is not, because it may read what it is
    /// overwriting — see [`Room`].
    #[test]
    fn a_literal_assigned_over_a_place_is_built_elsewhere_and_copied() {
        let ir = lower_main("int[2] a = [1, 2];\na = [a[1], a[0]];\nprint(a[0]);");
        let main = &ir.functions[0];
        assert!(
            main.blocks[0].instrs.iter().any(|i| matches!(i, Instr::CopyBytes { .. })),
            "the swap has to change all at once: {}",
            ir.dump()
        );
        // Two reservations: the variable, and the room the swap is built in.
        assert_eq!(main.frame_bytes, 32);
    }

    /// Two blocks that cannot be running at once share their room.
    #[test]
    fn a_blocks_frame_goes_back_when_the_block_ends() {
        let shared = lower_main(
            "int n = 0;\nif (n == 0) {\n  int[3] a = [1, 2, 3];\n  n = a[0];\n}\n\
             else {\n  int[3] b = [4, 5, 6];\n  n = b[0];\n}\nprint(n);",
        );
        // Three ints, once, rather than once per arm.
        assert_eq!(shared.functions[0].frame_bytes, 24);

        // What is reserved is the most ever needed at *once*, so two arrays
        // that really are live together still get room each.
        let both = lower_main("int[3] a = [1, 2, 3];\nint[3] b = [4, 5, 6];\nprint(a[0] + b[0]);");
        assert_eq!(both.functions[0].frame_bytes, 48);
    }

    /// One `int[1024]` is 8,192 bytes, so the limit falls between the
    /// thirty-second and the thirty-third. The boundary is checked from both
    /// sides because an off-by-one here is a program that either crashes or is
    /// refused for no reason.
    fn arrays_worth(bytes: u32) -> String {
        let elements = vec!["0"; 1024].join(", ");
        let count = bytes.div_ceil(1024 * 8);
        let declarations: String =
            (0..count).map(|i| format!("  int[1024] a{i} = [{elements}];\n")).collect();
        format!("fn main() {{\n{declarations}  println(a0[0]);\n}}\n")
    }

    #[test]
    fn a_frame_no_stack_would_hold_is_refused() {
        let Err(errors) = try_lower(&arrays_worth(MAX_FRAME_BYTES + 1)) else {
            panic!("past the limit is past the limit");
        };
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].message.contains("needs too much stack"), "{:?}", errors[0]);
        // The caret goes on the function's name, which is the one place a
        // reader can act on: no single declaration is the culprit.
        assert!(errors[0].label.as_ref().is_some_and(|l| l.contains("262144")), "{:?}", errors[0]);
    }

    #[test]
    fn a_frame_that_only_just_fits_is_not() {
        let ir = try_lower(&arrays_worth(MAX_FRAME_BYTES)).expect("exactly the limit is allowed");
        assert_eq!(ir.functions[0].frame_bytes, MAX_FRAME_BYTES);
    }

    #[test]
    fn a_frame_is_measured_in_the_function_that_declares_it() {
        // Two functions of half the limit each. Neither is refused: a frame is
        // one call's, and these two are never on the stack at the same time
        // unless one calls the other — which is what the *runtime* check is
        // for, and it is a question about depth, not about size.
        let half = MAX_FRAME_BYTES / 2;
        let mut src = arrays_worth(half).replace("fn main()", "fn other()");
        src.push_str(&arrays_worth(half));
        try_lower(&src).expect("two half-sized frames are two frames, not one");
    }

    #[test]
    fn a_function_with_no_arrays_reserves_nothing() {
        assert_eq!(lower_main("int n = 1;\nprint(n);").functions[0].frame_bytes, 0);
    }

    #[test]
    fn len_is_a_constant_and_costs_nothing() {
        // It is a fact about the type, so nothing computes it.
        let ir = lower_main("int[4] xs = [1, 2, 3, 4];\nprint(len(xs));");
        let main = &ir.functions[0];
        assert!(
            matches!(main.blocks[0].instrs.last(), Some(Instr::Print { val: Value::Const(4), .. })),
            "{}",
            ir.dump()
        );
    }

    #[test]
    fn an_element_address_is_one_instruction_and_never_arithmetic() {
        // `base + index * 8` is an addressing mode, so it picks up none of the
        // overflow guards `Bin` carries.
        let ir = lower_main("int[3] xs = [1, 2, 3];\nint i = 1;\nprint(xs[i]);");
        let main = &ir.functions[0];
        let elems = main.blocks[0].instrs.iter().filter(|i| matches!(i, Instr::Elem { .. })).count();
        // Three to build it, one to read it.
        assert_eq!(elems, 4, "{}", ir.dump());
        assert!(
            !main.blocks[0]
                .instrs
                .iter()
                .any(|i| matches!(i, Instr::Bin { op: BinOp::Mul, .. })),
            "{}",
            ir.dump()
        );
    }

    #[test]
    fn writing_through_an_index_stores_rather_than_copying_a_register() {
        let ir = lower_main("int[2] xs = [1, 2];\nxs[1] = 7;\nprint(xs[1]);");
        let main = &ir.functions[0];
        // Two stores for the literal, one for the assignment.
        let stores = main.blocks[0].instrs.iter().filter(|i| matches!(i, Instr::Store { .. })).count();
        assert_eq!(stores, 3, "{}", ir.dump());
    }

    #[test]
    fn an_array_parameter_is_an_address_like_any_other_value() {
        let ir = lower_src(
            "fn first(int[2] xs) -> int {\n  return xs[0];\n}\n\
             fn main() {\n  int[2] xs = [1, 2];\n  print(first(xs));\n}",
        );
        let first = &ir.functions[0];
        // Nothing is copied in: the register holds the caller's address.
        assert!(matches!(first.blocks[0].instrs[0], Instr::Param { index: 0, .. }));
        assert!(!first.blocks.iter().flat_map(|b| &b.instrs).any(|i| matches!(i, Instr::Frame { .. })));
    }

    // -- enums and match ---------------------------------------------------

    /// A `Colour` enum and a `main`, so the tests about enums stay about enums.
    fn lower_colour(body: &str) -> Program {
        lower_src(&format!("enum Colour {{ Red, Green, Blue }}\nfn main() {{\n{body}\n}}\n"))
    }

    #[test]
    fn a_variant_lowers_to_its_tag_and_nothing_else() {
        // The whole representation: a variant is where it was written in the
        // declaration, so it is an immediate like any other integer.
        let ir = lower_colour("Colour c = Colour::Blue;\nprint(c);");
        assert_eq!(
            body_dump(&ir),
            concat!(
                "entry0:\n",
                "  0  %c = const 2\n",
                "  1  print Colour %c\n",
                "  2  return\n",
            )
        );
    }

    #[test]
    fn a_variant_used_as_an_operand_stays_an_immediate() {
        let ir = lower_colour("Colour c = Colour::Red;\nprint(c == Colour::Green);");
        assert!(
            ir.functions[0].blocks[0]
                .instrs
                .iter()
                .any(|i| matches!(i, Instr::Cmp { rhs: Value::Const(1), .. })),
            "{}",
            ir.dump()
        );
    }

    #[test]
    fn a_match_tests_every_variant_but_the_last() {
        // Exhaustiveness pays for itself here: there is nowhere else for the
        // value to be, so the final test would always succeed.
        let ir = lower_colour(
            "Colour c = Colour::Red;\nmatch (c) {\n  Colour::Red => { print(1); }\n  \
             Colour::Green => { print(2); }\n  Colour::Blue => { print(3); }\n}",
        );
        let main = &ir.functions[0];
        assert_eq!(labels(main), vec!["entry0", "arm1", "case2", "arm3", "arm4", "join5"]);

        let comparisons = main
            .blocks
            .iter()
            .flat_map(|b| &b.instrs)
            .filter(|i| matches!(i, Instr::Cmp { .. }))
            .count();
        assert_eq!(comparisons, 2, "three variants need two tests: {}", ir.dump());
    }

    #[test]
    fn each_match_arm_reaches_the_same_join() {
        let ir = lower_colour(
            "Colour c = Colour::Red;\nmatch (c) {\n  Colour::Red => { print(1); }\n  \
             Colour::Green => { print(2); }\n  Colour::Blue => { print(3); }\n}\nprint(9);",
        );
        let main = &ir.functions[0];
        let join = BlockId(5);
        for arm in [1usize, 3, 4] {
            assert!(
                matches!(main.blocks[arm].term, Terminator::Jump(target) if target == join),
                "arm {arm}: {}",
                ir.dump()
            );
        }
    }

    #[test]
    fn a_single_variant_enum_needs_no_test_at_all() {
        // There is only one place the value can be, so the scrutinee's block
        // simply runs into the one arm.
        let ir = lower_src(
            "enum Unit { Only }\nfn main() {\n  Unit u = Unit::Only;\n  \
             match (u) {\n    Unit::Only => { print(1); }\n  }\n}",
        );
        let main = &ir.functions[0];
        assert!(
            !main.blocks.iter().flat_map(|b| &b.instrs).any(|i| matches!(i, Instr::Cmp { .. })),
            "{}",
            ir.dump()
        );
        assert!(matches!(main.blocks[0].term, Terminator::Jump(_)), "{}", ir.dump());
    }

    #[test]
    fn the_arms_are_matched_in_the_order_they_are_written() {
        // Not in declaration order: an arm's tag comes from its own pattern.
        let ir = lower_colour(
            "Colour c = Colour::Red;\nmatch (c) {\n  Colour::Blue => { print(1); }\n  \
             Colour::Red => { print(2); }\n  Colour::Green => { print(3); }\n}",
        );
        let tags: Vec<i64> = ir.functions[0]
            .blocks
            .iter()
            .flat_map(|b| &b.instrs)
            .filter_map(|i| match i {
                Instr::Cmp { rhs: Value::Const(tag), .. } => Some(*tag),
                _ => None,
            })
            .collect();
        assert_eq!(tags, vec![2, 0], "{}", ir.dump());
    }

    #[test]
    fn a_loop_jump_inside_an_arm_belongs_to_the_loop() {
        let ir = lower_src(
            "enum A { X, Y }\nfn main() {\n  while (true) {\n    A a = A::X;\n    \
             match (a) {\n      A::X => { break; }\n      A::Y => { print(1); }\n    }\n  }\n}",
        );
        // The `break` leaves for the loop's exit, not the match's join.
        let main = &ir.functions[0];
        let done = main
            .blocks
            .iter()
            .position(|b| b.kind == BlockKind::Done)
            .expect("the loop has an exit");
        assert!(
            main.blocks.iter().any(|b| matches!(b.term, Terminator::Jump(t) if t.0 as usize == done)),
            "{}",
            ir.dump()
        );
    }

    #[test]
    fn every_value_arm_writes_the_same_register() {
        // The trick `&&` already plays, with more arms: a non-SSA IR lets the
        // join read one register that several blocks wrote.
        let ir = lower_colour(
            "string s = match (Colour::Red) {\n  Colour::Red => \"a\",\n  \
             Colour::Green => \"b\",\n  Colour::Blue => \"c\",\n};\nprint(s);",
        );
        let main = &ir.functions[0];
        let written: Vec<VReg> = main
            .blocks
            .iter()
            .filter(|b| b.kind == BlockKind::Arm)
            .filter_map(|b| b.instrs.last().and_then(|i| i.def()))
            .collect();
        assert_eq!(written.len(), 3, "{}", ir.dump());
        assert!(written.windows(2).all(|w| w[0] == w[1]), "{written:?} in {}", ir.dump());
    }

    #[test]
    fn a_block_arm_writes_nothing_and_never_reaches_the_join() {
        let ir = lower_src(
            "enum A { X, Y }\nfn f(A a) -> int {\n  return match (a) {\n    A::X => 1,\n    \
             A::Y => { print(9); return 2; }\n  };\n}\nfn main() {\n  print(f(A::X));\n}",
        );
        let f = &ir.functions[0];
        // The diverging arm ends in a `return`, not a jump to the join.
        assert!(
            f.blocks
                .iter()
                .filter(|b| b.kind == BlockKind::Arm)
                .any(|b| matches!(b.term, Terminator::Return(Some(_)))),
            "{}",
            ir.dump()
        );
    }

    #[test]
    fn a_match_statement_needs_no_destination() {
        // Nothing reads it, so no temporary is spent on one.
        let ir = lower_colour(
            "match (Colour::Red) {\n  Colour::Red => { print(1); }\n  \
             Colour::Green => { print(2); }\n  Colour::Blue => { print(3); }\n}",
        );
        let main = &ir.functions[0];
        let joins: Vec<&Block> =
            main.blocks.iter().filter(|b| b.kind == BlockKind::Join).collect();
        assert_eq!(joins.len(), 1);
        assert!(joins[0].instrs.is_empty(), "{}", ir.dump());
    }

    #[test]
    fn a_match_expression_folds_its_arms_like_any_other_operand() {
        // A variant arm is a constant, so it never reaches a register at all.
        let ir = lower_src(
            "enum A { X, Y }\nfn main() {\n  A a = match (A::X) {\n    A::X => A::Y,\n    \
             A::Y => A::X,\n  };\n  print(a);\n}",
        );
        let main = &ir.functions[0];
        assert!(
            main.blocks
                .iter()
                .flat_map(|b| &b.instrs)
                .any(|i| matches!(i, Instr::Const { val: 1, .. })),
            "{}",
            ir.dump()
        );
    }

    #[test]
    fn the_enums_travel_with_the_program() {
        // The backend needs the names to print one, and the dump to name the
        // type; the values themselves need nothing.
        let ir = lower_colour("print(Colour::Red);");
        assert_eq!(ir.table.enums.len(), 1);
        assert_eq!(ir.table.enums[0].name, "Colour");
        assert_eq!(ir.table.enums[0].names(), vec!["Red", "Green", "Blue"]);
    }

    // -- negation and remainder --------------------------------------------

    #[test]
    fn negating_a_comparison_inverts_it_in_place() {
        // `!(a < b)` is `a >= b`: one comparison, not a comparison plus a
        // comparison against its result.
        let ir = lower_main("int a = 1;\nint b = 2;\nprint(!(a < b));");
        let comparisons: Vec<&Instr> = ir.functions[0].blocks[0]
            .instrs
            .iter()
            .filter(|i| matches!(i, Instr::Cmp { .. }))
            .collect();
        assert_eq!(comparisons.len(), 1, "{}", ir.dump());
        assert!(matches!(comparisons[0], Instr::Cmp { op: CmpOp::Ge, .. }), "{comparisons:?}");
    }

    #[test]
    fn negating_anything_else_compares_it_against_zero() {
        // There is no `not` instruction, and none is needed: `!ok` *is*
        // `ok == 0`, which folds and fuses like any other comparison.
        let ir = lower_main("bool ok = true;\nint n = 1;\nok = n > 0;\nprint(!ok);");
        assert!(
            ir.functions[0].blocks[0]
                .instrs
                .iter()
                .any(|i| matches!(i, Instr::Cmp { op: CmpOp::Eq, rhs: Value::Const(0), .. })),
            "{}",
            ir.dump()
        );
    }

    #[test]
    fn negating_a_literal_is_folded() {
        let ir = lower_main("print(!true);");
        let main = &ir.functions[0];
        assert_eq!(main.blocks[0].instrs.len(), 1, "{}", ir.dump());
        assert!(matches!(main.blocks[0].instrs[0], Instr::Print { val: Value::Const(0), .. }));
    }

    #[test]
    fn a_remainder_between_literals_is_computed_at_compile_time() {
        let ir = lower_main("print(17 % 5);");
        let main = &ir.functions[0];
        assert!(
            matches!(main.blocks[0].instrs[0], Instr::Print { val: Value::Const(2), .. }),
            "{}",
            ir.dump()
        );
    }

    #[test]
    fn an_operation_on_something_unknown_stays_an_instruction() {
        // The other half: what the folder cannot answer becomes a guarded
        // instruction, and the program fails where it was written.
        for (source, op) in [
            ("int z = 0;\nprint(1 / z);", BinOp::Div),
            ("int z = 0;\nprint(1 % z);", BinOp::Rem),
            ("int n = 1;\nprint(n + 9223372036854775807);", BinOp::Add),
        ] {
            let ir = lower_main(source);
            assert!(
                ir.functions[0]
                    .blocks
                    .iter()
                    .flat_map(|b| &b.instrs)
                    .any(|i| matches!(i, Instr::Bin { op: found, .. } if *found == op)),
                "{source}: {}",
                ir.dump()
            );
        }
    }

    // -- short-circuiting operators ----------------------------------------

    #[test]
    fn a_logical_operator_lowers_to_a_diamond() {
        let ir = lower_main("int x = 5;\nbool ok = x > 1 && x < 9;\nprint(ok);");
        let main = &ir.functions[0];
        assert_eq!(labels(main), vec!["entry0", "rhs1", "short2", "join3"]);
        // Both arms write the same register, which is what makes this an
        // expression: the join needs no phi.
        assert!(matches!(main.blocks[1].instrs.last(), Some(Instr::Cmp { dst, .. }) if *dst == main.blocks[2].instrs[0].def().unwrap()));
        assert!(matches!(main.blocks[2].instrs[0], Instr::Const { val: 0, .. }));
    }

    #[test]
    fn or_lays_its_short_circuit_out_first() {
        // The arm the branch continues into comes first, so the backend reaches
        // it by falling through: for `&&` that is the right operand, for `||`
        // the short circuit.
        let ir = lower_main("int x = 5;\nbool ok = x > 1 || x < 9;\nprint(ok);");
        let main = &ir.functions[0];
        assert_eq!(labels(main), vec!["entry0", "short1", "rhs2", "join3"]);
        assert!(matches!(main.blocks[1].instrs[0], Instr::Const { val: 1, .. }));
        match main.blocks[0].term {
            Terminator::Branch { then_blk, else_blk, .. } => {
                assert_eq!((then_blk, else_blk), (BlockId(1), BlockId(2)));
            }
            ref other => panic!("expected a branch, got {other:?}"),
        }
    }

    #[test]
    fn a_left_operand_that_settles_the_answer_drops_the_right_one() {
        // Not an optimisation but the semantics: `f` must not be called.
        let ir = lower_src(
            "fn f() -> bool {\n  return true;\n}\nfn main() {\n  print(false && f());\n}",
        );
        let main = ir.functions.iter().find(|f| f.name == "main").expect("main survives");
        assert_eq!(labels(main), vec!["entry0"]);
        assert!(
            matches!(main.blocks[0].instrs[0], Instr::Print { val: Value::Const(0), .. }),
            "{}",
            ir.dump()
        );
        // Nothing calls `f` any more, so it is pruned along with unused ones.
        assert!(!ir.functions.iter().any(|f| f.name == "f"), "{}", ir.dump());
    }

    #[test]
    fn a_left_operand_that_settles_nothing_leaves_only_the_right_one() {
        // `true && e` is `e`: no branch, no temporary, no constant.
        let ir = lower_main("int x = 5;\nprint(true && x > 1);");
        let main = &ir.functions[0];
        assert_eq!(labels(main), vec!["entry0"]);
        assert!(matches!(main.blocks[0].instrs[1], Instr::Cmp { .. }), "{}", ir.dump());
    }

    #[test]
    fn logic_between_literals_is_folded_all_the_way() {
        for (source, expected) in
            [("print(true || false);", 1), ("print(true && false);", 0), ("print(1 < 2 && 3 < 4);", 1)]
        {
            let ir = lower_main(source);
            let main = &ir.functions[0];
            assert_eq!(main.blocks[0].instrs.len(), 1, "{source}: {}", ir.dump());
            assert!(
                matches!(main.blocks[0].instrs[0], Instr::Print { val: Value::Const(v), .. } if v == expected),
                "{source}: {}",
                ir.dump()
            );
        }
    }

    #[test]
    fn a_logical_operator_may_be_a_condition_of_its_own() {
        // The branch belongs to the block the condition *ended* in, which for a
        // short-circuiting operator is its join rather than the loop header.
        let ir = lower_main("int i = 0;\nwhile (i < 3 && i != 2) {\n  i = i + 1;\n}\nprint(i);");
        let main = &ir.functions[0];
        assert_eq!(labels(main), vec!["entry0", "loop1", "rhs2", "short3", "join4", "body5", "done6"]);
        // The loop is still a loop: the body jumps back to the header, not to
        // the join the condition finished in.
        assert!(matches!(main.blocks[5].term, Terminator::Jump(BlockId(1))));
    }

    // -- break and continue ------------------------------------------------

    #[test]
    fn break_jumps_to_the_block_after_the_loop() {
        let ir = lower_main("while (true) {\n  break;\n}\nprint(1);");
        let main = &ir.functions[0];
        // The block the `break` left was terminated by the loop on its way out,
        // and the unreachable block opened after it is gone.
        assert_eq!(labels(main), vec!["entry0", "loop1", "body2", "done3"]);
        assert!(matches!(main.blocks[2].term, Terminator::Jump(BlockId(3))));
    }

    #[test]
    fn continue_in_a_while_jumps_back_to_the_header() {
        let ir = lower_main("int i = 0;\nwhile (i < 3) {\n  i = i + 1;\n  continue;\n}\nprint(i);");
        let main = &ir.functions[0];
        // A `while` has no step, so its header *is* its latch and no extra
        // block appears.
        assert_eq!(labels(main), vec!["entry0", "loop1", "body2", "done3"]);
        assert!(matches!(main.blocks[2].term, Terminator::Jump(BlockId(1))));
    }

    #[test]
    fn continue_in_a_for_runs_the_step_on_its_way_past() {
        // The whole reason the step needs a block: skipping it would leave the
        // counter alone and the loop would never end.
        let ir = lower_main("for (int i = 0; i < 3; i = i + 1) {\n  continue;\n}");
        let main = &ir.functions[0];
        assert_eq!(labels(main), vec!["entry0", "loop1", "body2", "step3", "done4"]);
        assert!(matches!(main.blocks[3].instrs[0], Instr::Bin { op: BinOp::Add, .. }));
        assert!(matches!(main.blocks[2].term, Terminator::Jump(BlockId(3))));
        assert!(matches!(main.blocks[3].term, Terminator::Jump(BlockId(1))));
    }

    #[test]
    fn a_for_without_a_continue_needs_no_step_block() {
        // Which is what keeps a plain `for` lowering to exactly the `while` it
        // desugars into.
        let ir = lower_main("for (int i = 0; i < 3; i = i + 1) {\n  print(i);\n}");
        assert_eq!(labels(&ir.functions[0]), vec!["entry0", "loop1", "body2", "done3"]);
    }

    #[test]
    fn a_continue_in_a_nested_loop_belongs_to_that_loop() {
        // The inner `while` takes the `continue`, so the outer `for` still has
        // none of its own and stays step-block free.
        let ir = lower_main(
            "for (int i = 0; i < 3; i = i + 1) {\n  while (i < 2) {\n    i = i + 1;\n    continue;\n  }\n}",
        );
        let labels = labels(&ir.functions[0]);
        assert!(!labels.iter().any(|l| l.starts_with("step")), "{labels:?}");
    }

    #[test]
    fn break_leaves_only_the_innermost_loop() {
        let ir = lower_main(
            "while (true) {\n  while (true) {\n    break;\n  }\n  print(1);\n  break;\n}",
        );
        let main = &ir.functions[0];
        // The inner `break` lands where the inner loop leaves, which is where
        // the `print` still runs — not at the outer loop's exit.
        let done: Vec<usize> =
            main.blocks.iter().enumerate().filter(|(_, b)| b.kind == BlockKind::Done).map(|(i, _)| i).collect();
        assert_eq!(done.len(), 2, "{}", ir.dump());
        let inner_exit = done[0];
        assert!(
            main.blocks[inner_exit].instrs.iter().any(|i| matches!(i, Instr::Print { .. })),
            "{}",
            ir.dump()
        );
    }

    #[test]
    fn code_after_a_loop_jump_is_pruned() {
        let ir = lower_main("while (true) {\n  break;\n  print(1);\n}");
        assert!(
            !ir.functions[0].blocks.iter().flat_map(|b| &b.instrs).any(|i| matches!(i, Instr::Print { .. })),
            "{}",
            ir.dump()
        );
    }

    #[test]
    fn literal_operands_stay_immediates() {
        // `x` is unknown, so the addition survives — but the 2 next to it is
        // still an operand rather than a register of its own.
        let ir = lower_main("int x = 1;\nprint(x + 2);");
        let main = &ir.functions[0];
        assert!(
            main.blocks[0]
                .instrs
                .iter()
                .any(|i| matches!(i, Instr::Bin { rhs: Value::Const(2), .. })),
            "{}",
            ir.dump()
        );
    }

    #[test]
    fn arithmetic_between_literals_is_done_at_compile_time() {
        let ir = lower_main("print(1 + 2 * 3);");
        let main = &ir.functions[0];
        // The whole tree collapses into the operand of the print: no `add`, no
        // `mul`, and no register to hold the answer either.
        assert_eq!(main.blocks[0].instrs.len(), 1, "{}", ir.dump());
        assert!(
            matches!(main.blocks[0].instrs[0], Instr::Print { val: Value::Const(7), .. }),
            "{}",
            ir.dump()
        );
    }

    #[test]
    fn the_folder_refuses_every_answer_the_machine_would_refuse() {
        // Today `sema` rejects all of these before lowering is ever reached, so
        // this is a unit test rather than a program: it keeps the folder from
        // becoming the stage that invents an answer, should the two ever come
        // to see a different set of constants.
        for (op, a, b) in [
            (BinOp::Add, i64::MAX, 1),
            (BinOp::Sub, i64::MIN, 1),
            (BinOp::Mul, i64::MAX, 2),
            (BinOp::Div, 1, 0),
            (BinOp::Rem, 1, 0),
            (BinOp::Div, i64::MIN, -1),
            // 0 on paper, and still refused: the machine gets there through the
            // division whose quotient does not fit.
            (BinOp::Rem, i64::MIN, -1),
        ] {
            assert_eq!(
                fold_bin(Num::Int, op, Value::Const(a), Value::Const(b)),
                None,
                "{} {a}, {b}",
                op.symbol()
            );
        }
    }

    #[test]
    fn an_operation_the_machine_accepts_is_still_folded() {
        for (op, a, b, expected) in [
            (BinOp::Add, 2, 3, 5),
            (BinOp::Sub, 2, 3, -1),
            (BinOp::Mul, 6, 7, 42),
            (BinOp::Div, 17, 5, 3),
            (BinOp::Rem, 17, 5, 2),
            // The largest each operator can produce, one step short of refusing.
            (BinOp::Add, i64::MAX - 1, 1, i64::MAX),
            (BinOp::Sub, i64::MIN + 1, 1, i64::MIN),
        ] {
            assert_eq!(
                fold_bin(Num::Int, op, Value::Const(a), Value::Const(b)),
                Some(expected),
                "{} {a}, {b}",
                op.symbol()
            );
        }
    }

    #[test]
    fn a_comparison_between_literals_is_folded_too() {
        let ir = lower_main("bool b = 1 < 2;\nprint(b);");
        assert!(
            matches!(ir.functions[0].blocks[0].instrs[0], Instr::Const { val: 1, .. }),
            "{}",
            ir.dump()
        );
    }

    #[test]
    fn a_function_nothing_calls_is_dropped() {
        let ir = lower_src(
            "fn used() -> int {\n  return 1;\n}\n\
             fn unused() -> int {\n  return 2;\n}\n\
             fn main() {\n  print(used());\n}",
        );
        let names: Vec<&str> = ir.functions.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["used", "main"]);
    }

    #[test]
    fn dropping_a_function_renumbers_the_calls_that_survive() {
        // `unused` sits between the entry point and its callee, so every
        // `FuncId` after it shifts down by one.
        let ir = lower_src(
            "fn unused() {\n}\n\
             fn helper() -> int {\n  return 7;\n}\n\
             fn main() {\n  print(helper());\n}",
        );
        let main = ir.functions.iter().find(|f| f.name == "main").expect("main survives");
        let Instr::Call { callee, .. } = &main.blocks[0].instrs[0] else { panic!("a call") };
        assert_eq!(ir.function(*callee).name, "helper");
    }

    #[test]
    fn a_function_only_an_unused_one_calls_goes_too() {
        let ir = lower_src(
            "fn deep() -> int {\n  return 1;\n}\n\
             fn unused() -> int {\n  return deep();\n}\n\
             fn main() {\n}",
        );
        let names: Vec<&str> = ir.functions.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["main"]);
    }

    #[test]
    fn recursion_keeps_a_function_alive_through_itself() {
        let ir = lower_src(
            "fn fib(int n) -> int {\n  if (n < 2) {\n    return n;\n  }\n  \
             return fib(n - 1) + fib(n - 2);\n}\n\
             fn main() {\n  print(fib(5));\n}",
        );
        assert!(ir.functions.iter().any(|f| f.name == "fib"), "{}", ir.dump());
    }

    #[test]
    fn bools_lower_to_integer_constants() {
        let ir = lower_main("bool ready = true;\nbool done = false;\nprint(ready);\nprint(done);");
        assert_eq!(
            body_dump(&ir),
            concat!(
                "entry0:\n",
                "  0  %ready = const 1\n",
                "  1  %done = const 0\n",
                "  2  print bool %ready\n",
                "  3  print bool %done\n",
                "  4  return\n",
            )
        );
    }

    #[test]
    fn a_printed_bool_literal_stays_an_immediate() {
        let ir = lower_main("print(false);");
        let main = &ir.functions[0];
        assert_eq!(main.blocks[0].instrs.len(), 1);
        assert!(matches!(
            main.blocks[0].instrs[0],
            Instr::Print { ty: Ty::Bool, val: Value::Const(0), .. }
        ));
    }

    #[test]
    fn shadowed_variables_get_distinct_registers() {
        let ir = lower_main("int i = 1;\nif (true) {\n  int i = 2;\n  print(i);\n}\nprint(i);");
        let names = &ir.functions[0].vreg_names;
        assert!(names.contains(&"i".to_string()));
        assert!(names.contains(&"i.1".to_string()));
    }

    #[test]
    fn identical_strings_are_interned_once() {
        let ir = lower_main("string a = \"hi\";\nstring b = \"hi\";\nprint(a);\nprint(b);");
        assert_eq!(ir.strings.len(), 1);
    }

    #[test]
    fn a_literal_is_interned_as_characters() {
        let ir = lower_main("string s = \"é\";\nprint(s);");
        assert_eq!(ir.strings[0], vec!['é']);
    }

    /// A literal that is only ever *written* never becomes a string at all.
    ///
    /// The two tables answer different questions: `strings` holds values the
    /// program can index and join, laid out as characters four bytes each;
    /// `texts` holds bytes `printf` is handed. A literal that goes straight out
    /// needs no run-time form, and so is not given one — which is what makes
    /// `print("hi")` cost one call and no memory.
    #[test]
    fn a_literal_that_is_only_printed_becomes_text_rather_than_a_string() {
        let ir = lower_main("print(\"é\");");
        assert!(ir.strings.is_empty(), "{:?}", ir.strings);
        assert_eq!(ir.texts, vec!["é".to_string()]);
    }

    /// Every instruction a function lowered to, flattened.
    fn instrs(ir: &Program) -> Vec<&Instr> {
        ir.functions[0].blocks.iter().flat_map(|b| &b.instrs).collect()
    }

    #[test]
    fn joining_two_strings_becomes_a_call_and_not_an_add() {
        let ir = lower_main("string a = \"x\";\nprint(a + a);");
        assert!(
            instrs(&ir)
                .iter()
                .any(|i| matches!(i, Instr::RtCall { callee: Runtime::Concat, .. })),
            "{}",
            ir.dump()
        );
        assert!(!instrs(&ir).iter().any(|i| matches!(i, Instr::Bin { .. })), "{}", ir.dump());
    }

    #[test]
    fn comparing_two_strings_asks_the_runtime_rather_than_the_processor() {
        let ir = lower_main("string a = \"x\";\nprint(a == a);");
        let lowered = instrs(&ir);
        assert!(
            lowered.iter().any(|i| matches!(i, Instr::RtCall { callee: Runtime::StrEq, .. })),
            "{}",
            ir.dump()
        );
        // `!=` is the same routine read the other way round, so it costs one
        // comparison against zero and not a second routine.
        let ir = lower_main("string a = \"x\";\nprint(a != a);");
        let lowered = instrs(&ir);
        assert!(lowered.iter().any(|i| matches!(i, Instr::RtCall { callee: Runtime::StrEq, .. })));
        assert!(lowered.iter().any(|i| matches!(i, Instr::Cmp { op: CmpOp::Eq, .. })));
    }

    #[test]
    fn an_arrays_length_is_a_constant_and_a_strings_is_a_load() {
        let ir = lower_main("int[3] xs = [1, 2, 3];\nprint(len(xs));");
        assert!(!instrs(&ir).iter().any(|i| matches!(i, Instr::Count { .. })), "{}", ir.dump());

        let ir = lower_main("print(len(\"abc\"));");
        assert!(instrs(&ir).iter().any(|i| matches!(i, Instr::Count { .. })), "{}", ir.dump());
    }

    #[test]
    fn a_constant_index_into_a_string_is_still_checked() {
        // An array's length is part of its type, so `sema` settled the index
        // and the `Elem` carries a constant length the backend can drop the
        // check for. A string's length is a load, so there is nothing to settle.
        let ir = lower_main("int[3] xs = [1, 2, 3];\nprint(xs[1]);");
        let lengths: Vec<&Value> = instrs(&ir)
            .iter()
            .filter_map(|i| match i {
                Instr::Elem { len, .. } => Some(len),
                _ => None,
            })
            .collect();
        assert!(lengths.iter().all(|len| matches!(len, Value::Const(_))), "{}", ir.dump());

        let ir = lower_main("print(\"abc\"[1]);");
        let lengths: Vec<&Value> = instrs(&ir)
            .iter()
            .filter_map(|i| match i {
                Instr::Elem { len, .. } => Some(len),
                _ => None,
            })
            .collect();
        assert_eq!(lengths.len(), 1);
        assert!(matches!(lengths[0], Value::Reg(_)), "{}", ir.dump());
    }

    #[test]
    fn a_code_point_and_its_character_are_the_same_value() {
        // `int(c)` moves nothing: a character *is* its code point. Only the
        // direction that can fail costs an instruction.
        let ir = lower_main("char c = 'a';\nprint(int(c));");
        assert!(!instrs(&ir).iter().any(|i| matches!(i, Instr::RtCall { .. })), "{}", ir.dump());

        let ir = lower_main("int n = 65;\nprint(char(n));");
        assert!(
            instrs(&ir)
                .iter()
                .any(|i| matches!(i, Instr::RtCall { callee: Runtime::CheckChar, .. })),
            "{}",
            ir.dump()
        );
    }

    /// Every runtime routine a lowering reached.
    fn routines(ir: &Program) -> Vec<Runtime> {
        instrs(ir)
            .iter()
            .filter_map(|i| match i {
                Instr::RtCall { callee, .. } => Some(*callee),
                _ => None,
            })
            .collect()
    }

    /// The proof `append` rests on, read off the IR: which shapes get it and
    /// which fall back to an ordinary join.
    ///
    /// The negative half matters more than the positive one. Getting this wrong
    /// in the permissive direction would lengthen a string somebody else is
    /// holding — so every one of these is a case the analysis has to refuse.
    #[test]
    fn a_string_is_grown_in_place_only_where_nothing_else_can_hold_it() {
        let grows = |src: &str| {
            let program = format!(
                "class Box {{ string text; }}\n\
                 fn keep(string s) -> string {{ return s; }}\n\
                 fn main() {{\n{src}\n}}\n"
            );
            routines(&lower_src(&program)).contains(&Runtime::Append)
        };

        // The shape it exists for, and the chain that is how a line is written.
        assert!(grows("string s = \"\";\ns = s + \"x\";\nprint(s);"));
        assert!(grows("string s = \"\";\ns = s + string(1) + \",\";\nprint(s);"));
        // Built by a conversion or by reading, which are blocks of their own.
        assert!(grows("string s = string(1);\ns = s + \"x\";\nprint(s);"));

        // Another name for it, taken anywhere in the function — before or
        // after, since the analysis is not a flow one and does not pretend to
        // be.
        assert!(!grows("string s = \"a\";\nstring t = s;\ns = s + \"x\";\nprint(t);"));
        assert!(!grows("string s = \"a\";\ns = s + \"x\";\nstring t = s;\nprint(t);"));
        // Handed to a function, put in a list, put in an object, returned.
        assert!(!grows("string s = \"a\";\nprint(keep(s));\ns = s + \"x\";"));
        assert!(!grows(
            "string s = \"a\";\nstring[] all = [];\npush(all, s);\ns = s + \"x\";\nprint(s);"
        ));
        assert!(!grows(
            "string s = \"a\";\nBox b = Box { text: s };\ns = s + \"x\";\nprint(b.text);"
        ));
        // Given a value that already had a name.
        assert!(!grows(
            "string o = \"a\" + \"b\";\nstring s = o;\ns = s + \"x\";\nprint(s);"
        ));
        // Given the answer of a call, which may be anybody's.
        assert!(!grows("string s = keep(\"a\");\ns = s + \"x\";\nprint(s);"));
        // Prepending cannot grow a block where it stands.
        assert!(!grows("string s = \"a\";\ns = \"x\" + s;\nprint(s);"));
        // A piece that reads the variable: appending one at a time would let
        // the second piece see what the first one wrote.
        assert!(!grows(
            "string s = \"a\";\ns = s + string(len(s)) + \"!\";\nprint(s);"
        ));

        // A parameter is the caller's string, whatever the body does with it.
        let ir = lower_src(
            "fn grow(string s) -> string {\n  s = s + \"x\";\n  return s;\n}\n\
             fn main() {\n  print(grow(\"a\"));\n}\n",
        );
        assert!(!routines(&ir).contains(&Runtime::Append), "{}", ir.dump());
    }

    /// A copy is only followed by a fix-up where the type says one can be
    /// needed, so a program whose classes hold nothing but numbers carries
    /// none of the machinery.
    #[test]
    fn only_a_copy_that_can_share_is_followed_by_a_fixup() {
        let fixups = |ir: &Program| {
            ir.functions
                .iter()
                .flat_map(|f| &f.blocks)
                .flat_map(|b| &b.instrs)
                .filter(|i| matches!(i, Instr::Fixup { .. }))
                .count()
        };

        // Nothing in a `Point` is anybody else's, so its bytes are the whole
        // of it.
        let plain = lower_src(
            "class Point { int x; }\n\
             fn main() {\n  Point a = Point { x: 1 };\n  Point b = a;\n  print(b.x);\n}\n",
        );
        assert_eq!(fixups(&plain), 0, "{}", plain.dump());

        // A list field, and the copy has to be told to go and get its own.
        let sharing = lower_src(
            "class Bag { int[] items; }\n\
             fn main() {\n  Bag a = Bag { items: [1] };\n  Bag b = a;\n  \
             print(len(b.items));\n}\n",
        );
        assert_eq!(fixups(&sharing), 1, "{}", sharing.dump());

        // An array of them is one fix-up over the run rather than one each:
        // the copy was one `CopyBytes`, and this is its other half.
        let array = lower_src(
            "class Bag { int[] items; }\n\
             fn main() {\n  Bag[2] a = [Bag { items: [1] }, Bag { items: [2] }];\n  \
             Bag[2] b = a;\n  print(len(b[0].items));\n}\n",
        );
        let run = instrs(&array)
            .into_iter()
            .find_map(|i| match i {
                Instr::Fixup { count, stride, .. } => Some((*count, *stride)),
                _ => None,
            })
            .expect("the array copy has one");
        assert_eq!(run, (Value::Const(2), 16), "two objects, sixteen bytes apart");
    }

    #[test]
    fn assigning_a_list_copies_it_and_passing_one_does_not() {
        // The whole of "assignment copies, never aliases" for the one mutable
        // thing in the language, in two lowerings.
        let ir = lower_main("int[] a = [1];\nint[] b = a;\nprint(len(b));");
        assert!(routines(&ir).contains(&Runtime::ListClone), "{}", ir.dump());

        let ir = lower_src(
            "fn f(int[] xs) -> int {\n  return len(xs);\n}\n\
             fn main() {\n  int[] a = [1];\n  print(f(a));\n}\n",
        );
        let calls: Vec<Runtime> = ir
            .functions
            .iter()
            .flat_map(|f| &f.blocks)
            .flat_map(|b| &b.instrs)
            .filter_map(|i| match i {
                Instr::RtCall { callee, .. } => Some(*callee),
                _ => None,
            })
            .collect();
        assert!(!calls.contains(&Runtime::ListClone), "a parameter borrows: {}", ir.dump());
    }

    #[test]
    fn a_freshly_built_list_moves_in_rather_than_being_copied() {
        // A literal is nobody else's already, so there is nothing to copy.
        let ir = lower_main("int[] a = [1, 2];\nprint(len(a));");
        assert!(!routines(&ir).contains(&Runtime::ListClone), "{}", ir.dump());
        assert!(routines(&ir).contains(&Runtime::ListNew), "{}", ir.dump());
    }

    #[test]
    fn returning_a_borrowed_list_copies_it_at_the_return() {
        // Copied here rather than at the call site, which is what lets every
        // caller treat a returned list as its own.
        let ir = lower_src(
            "fn f(int[] xs) -> int[] {\n  return xs;\n}\nfn main() {\n  print(len(f([1])));\n}\n",
        );
        let returned: Vec<Runtime> = ir.functions[0]
            .blocks
            .iter()
            .flat_map(|b| &b.instrs)
            .filter_map(|i| match i {
                Instr::RtCall { callee, .. } => Some(*callee),
                _ => None,
            })
            .collect();
        assert!(returned.contains(&Runtime::ListClone), "{}", ir.dump());
    }

    #[test]
    fn push_writes_the_new_address_back_where_the_list_is_named() {
        let ir = lower_main("int[] a = [];\npush(a, 1);\nprint(len(a));");
        let pushes: Vec<&Instr> = instrs(&ir)
            .into_iter()
            .filter(|i| matches!(i, Instr::RtCall { callee: Runtime::ListPush, .. }))
            .collect();
        assert_eq!(pushes.len(), 1);
        // The list's own register is both what goes in and what comes back.
        let Instr::RtCall { dst: Some(dst), args, .. } = pushes[0] else { panic!("a push") };
        assert_eq!(args[0], Value::Reg(*dst), "{}", ir.dump());
    }

    #[test]
    fn a_list_of_objects_is_measured_in_whole_objects() {
        // The routines walk the elements rather than reading one, so how wide
        // an element is has to travel with every call — and the addressing has
        // to scale by the same number.
        let ir = lower_src(
            "class Point {\n  int x;\n  int y;\n}\n\
             fn main() {\n  Point[] ps = [Point { x: 1, y: 2 }];\n  print(ps[0].x);\n}\n",
        );
        // A vtable pointer and two fields.
        let width = Value::Const(24);
        let new = instrs(&ir)
            .into_iter()
            .find(|i| matches!(i, Instr::RtCall { callee: Runtime::ListNew, .. }))
            .expect("the literal builds one");
        let Instr::RtCall { args, .. } = new else { panic!("a call") };
        assert_eq!(args[1], width, "{}", ir.dump());
        assert!(
            instrs(&ir).iter().any(|i| matches!(i, Instr::Elem { scale: 24, .. })),
            "{}",
            ir.dump()
        );
    }

    #[test]
    fn pushing_an_object_hands_over_where_it_is() {
        // An element too big for a register cannot travel in one, so the push
        // that takes an address is a different routine — the compiler knows
        // which from the element's type, and the runtime is not left to guess
        // from a width that an object of one word would make ambiguous.
        let ir = lower_src(
            "class Point {\n  int x;\n}\n\
             fn main() {\n  Point[] ps = [];\n  push(ps, Point { x: 1 });\n  \
             print(len(ps));\n}\n",
        );
        assert!(routines(&ir).contains(&Runtime::ListPushBig), "{}", ir.dump());
        assert!(!routines(&ir).contains(&Runtime::ListPush), "{}", ir.dump());

        // A register-sized element still goes through the plain one.
        let ir = lower_main("int[] xs = [];\npush(xs, 1);\nprint(len(xs));");
        assert!(routines(&ir).contains(&Runtime::ListPush), "{}", ir.dump());
        assert!(!routines(&ir).contains(&Runtime::ListPushBig), "{}", ir.dump());
    }

    #[test]
    fn a_builtin_call_becomes_a_routine_and_not_a_function_call() {
        // There is no body to compile and no `FuncId` to name, so the ordinary
        // call instruction could not carry it.
        let ir = lower_main("print(read_line());");
        assert!(routines(&ir).contains(&Runtime::ReadLine), "{}", ir.dump());
        assert!(!instrs(&ir).iter().any(|i| matches!(i, Instr::Call { .. })), "{}", ir.dump());

        // Discarded on its own, which is a line skipped.
        let ir = lower_main("read_line();");
        let calls: Vec<&Instr> = instrs(&ir)
            .into_iter()
            .filter(|i| matches!(i, Instr::RtCall { callee: Runtime::ReadLine, dst: None, .. }))
            .collect();
        assert_eq!(calls.len(), 1, "{}", ir.dump());

        // A built-in that takes something is no different: the argument is
        // lowered where it stands and travels as an operand.
        let ir = lower_main("print(is_int(\"42\"));");
        assert!(routines(&ir).contains(&Runtime::IsInt), "{}", ir.dump());
        assert!(!instrs(&ir).iter().any(|i| matches!(i, Instr::Call { .. })), "{}", ir.dump());
    }

    #[test]
    fn a_list_index_is_checked_against_a_length_it_has_to_load() {
        let ir = lower_main("int[] a = [1, 2];\nprint(a[0]);");
        assert!(instrs(&ir).iter().any(|i| matches!(i, Instr::Count { .. })), "{}", ir.dump());
    }

    #[test]
    fn a_constant_character_needs_no_check_at_run_time() {
        let ir = lower_main("print(char(65));");
        assert!(!instrs(&ir).iter().any(|i| matches!(i, Instr::RtCall { .. })), "{}", ir.dump());
    }

    // -- functions ---------------------------------------------------------

    #[test]
    fn each_function_gets_its_own_graph_and_registers() {
        let ir = lower_src(
            "fn add(int a, int b) -> int {\n  return a + b;\n}\nfn main() {\n  print(add(1, 2));\n}",
        );
        assert_eq!(ir.functions.len(), 2);
        // Both functions number their blocks and registers from zero.
        assert_eq!(ir.functions[0].blocks[0].label(), "entry0");
        assert_eq!(ir.functions[1].blocks[0].label(), "entry0");
        assert_eq!(ir.functions[0].params, vec![VReg(0), VReg(1)]);
    }

    #[test]
    fn parameters_are_defined_at_the_top_of_the_entry_block() {
        let ir = lower_src("fn f(int a, int b) {\n  print(a);\n}\nfn main() {\n  f(1, 2);\n}");
        let f = &ir.functions[0];
        assert!(matches!(f.blocks[0].instrs[0], Instr::Param { index: 0, .. }));
        assert!(matches!(f.blocks[0].instrs[1], Instr::Param { index: 1, .. }));
    }

    #[test]
    fn a_call_lowers_to_an_instruction_naming_its_callee() {
        let ir = lower_src(
            "fn add(int a, int b) -> int {\n  return a + b;\n}\nfn main() {\n  print(add(1, 2));\n}",
        );
        let main = &ir.functions[1];
        match &main.blocks[0].instrs[0] {
            Instr::Call { dst: Some(_), callee, args } => {
                assert_eq!(*callee, FuncId(0));
                assert_eq!(args, &vec![Value::Const(1), Value::Const(2)]);
            }
            other => panic!("expected a call, got {other:?}"),
        }
    }

    #[test]
    fn a_call_statement_discards_its_result() {
        let ir = lower_src("fn f() -> int {\n  return 1;\n}\nfn main() {\n  f();\n}");
        assert!(matches!(ir.functions[1].blocks[0].instrs[0], Instr::Call { dst: None, .. }));
    }

    #[test]
    fn a_return_carries_its_value_in_the_terminator() {
        let ir = lower_src("fn one() -> int {\n  return 1;\n}\nfn main() {\n  print(one());\n}");
        assert!(matches!(
            ir.functions[0].blocks[0].term,
            Terminator::Return(Some(Value::Const(1)))
        ));
    }

    #[test]
    fn a_bare_return_carries_nothing() {
        let ir = lower_src("fn f() {\n  return;\n}\nfn main() {\n  f();\n}");
        assert!(matches!(ir.functions[0].blocks[0].term, Terminator::Return(None)));
    }

    #[test]
    fn code_after_a_return_is_pruned() {
        // The `print` is lowered into a block nothing jumps to, and that block
        // never reaches the backend.
        let ir = lower_src("fn f() {\n  return;\n  print(1);\n}\nfn main() {\n  f();\n}");
        assert_eq!(labels(&ir.functions[0]), vec!["entry0"]);
    }

    #[test]
    fn an_if_where_both_arms_return_keeps_both_returns() {
        // The join block is unreachable and goes away, but the two `return`
        // terminators must survive the pruning intact.
        let ir = lower_src(
            "fn f(int n) -> int {\n  if (n < 2) {\n    return 1;\n  } else {\n    \
             return 2;\n  }\n}\nfn main() {\n  print(f(1));\n}",
        );
        let f = &ir.functions[0];
        assert_eq!(labels(f), vec!["entry0", "then1", "else2"]);
        assert!(matches!(f.blocks[1].term, Terminator::Return(Some(Value::Const(1)))));
        assert!(matches!(f.blocks[2].term, Terminator::Return(Some(Value::Const(2)))));
    }

    #[test]
    fn a_recursive_call_names_its_own_function() {
        let ir = lower_src(
            "fn fib(int n) -> int {\n  if (n < 2) {\n    return n;\n  } else {\n    \
             return fib(n - 1) + fib(n - 2);\n  }\n}\nfn main() {\n  print(fib(10));\n}",
        );
        let fib = &ir.functions[0];
        let calls: Vec<&Instr> = fib
            .blocks
            .iter()
            .flat_map(|b| &b.instrs)
            .filter(|i| matches!(i, Instr::Call { .. }))
            .collect();
        assert_eq!(calls.len(), 2);
        for call in calls {
            assert!(matches!(call, Instr::Call { callee: FuncId(0), .. }), "{call:?}");
        }
    }

    #[test]
    fn strings_are_shared_across_functions() {
        let ir = lower_src(
            "fn a() {\n  string s = \"hi\";\n  print(s);\n}\n\
             fn main() {\n  string s = \"hi\";\n  print(s);\n  a();\n}",
        );
        assert_eq!(ir.strings.len(), 1);
    }

    /// And so is literal text, which has a table of its own for the same reason.
    #[test]
    fn text_is_shared_across_functions() {
        let ir = lower_src(
            "fn a() {\n  print(\"hi\");\n}\nfn main() {\n  print(\"hi\");\n  a();\n}",
        );
        assert_eq!(ir.texts.len(), 1);
    }

    #[test]
    fn a_value_used_by_a_call_does_not_cross_it_but_a_nested_one_does() {
        // In `f(g(1), 2)` the result of `g` is live across nothing; in
        // `f(g(1), h(2))` it is live across the call to `h`.
        let ir = lower_src(
            "fn g(int n) -> int {\n  return n;\n}\nfn h(int n) -> int {\n  return n;\n}\n\
             fn f(int a, int b) -> int {\n  return a;\n}\n\
             fn main() {\n  print(f(g(1), h(2)));\n}",
        );
        let main = ir.functions.last().unwrap();
        let calls = main.blocks[0].instrs.iter().filter(|i| i.is_call()).count();
        assert_eq!(calls, 4); // g, h, f, and the print
    }

    // -- writing things out -------------------------------------------------

    /// A format lowers to one write per piece, in the order they were written.
    #[test]
    fn a_format_lowers_to_one_write_per_piece() {
        let ir = lower_main("int n = 7;\nprintln(\"a %d b\", n);");
        assert_eq!(
            body_dump(&ir),
            concat!(
                "entry0:\n",
                "  0  %n = const 7\n",
                "  1  print text0 \"a \"\n",
                "  2  print int %n\n",
                "  3  print text1 \" b\\n\"\n",
                "  4  return\n",
            )
        );
    }

    /// `println` is `print` with a newline, and this is where the two become
    /// one statement. The newline joins the text in front of it rather than
    /// costing a write of its own.
    #[test]
    fn a_trailing_newline_joins_the_text_before_it() {
        assert_eq!(lower_main("println(\"done\");").texts, vec!["done\n".to_string()]);
        assert_eq!(lower_main("print(\"done\");").texts, vec!["done".to_string()]);
    }

    /// When a format ends in a specifier there is no text to join the newline
    /// to — so the *value* ends the line instead, and nothing is written after
    /// it. That is what makes `println(n)` one call rather than two.
    #[test]
    fn a_value_that_ends_a_line_says_so_rather_than_being_followed_by_one() {
        let ir = lower_main("int n = 1;\nprintln(n);\nprintln(n);");
        assert!(ir.texts.is_empty(), "there is nothing left to write: {:?}", ir.texts);
        let printed: Vec<bool> = ir.functions[0].blocks[0]
            .instrs
            .iter()
            .filter_map(|i| match i {
                Instr::Print { newline, .. } => Some(*newline),
                _ => None,
            })
            .collect();
        assert_eq!(printed, vec![true, true]);
    }

    /// Only the *last* write of a `println` ends the line, and a `print` never
    /// does — which is the whole of what the flag means.
    #[test]
    fn only_the_last_value_of_a_println_ends_the_line() {
        let ends: Vec<bool> = ["println(\"%d %d\", 1, 2);", "print(\"%d %d\", 1, 2);"]
            .iter()
            .flat_map(|body| {
                lower_main(body).functions[0].blocks[0]
                    .instrs
                    .iter()
                    .filter_map(|i| match i {
                        Instr::Print { newline, .. } => Some(*newline),
                        _ => None,
                    })
                    .collect::<Vec<bool>>()
            })
            .collect();
        assert_eq!(ends, vec![false, true, false, false]);
    }

    /// A `println()` with nothing to write is a blank line, and must not reach
    /// back and attach its line ending to the write *before* it.
    ///
    /// The first shape of this rule looked for text left over rather than for a
    /// value of its own, and an empty `println` has none either — so the blank
    /// line disappeared into the line above it. Nothing in a dump showed it;
    /// running `examples/format.tc` did.
    #[test]
    fn an_empty_println_is_a_blank_line_and_not_a_second_one_on_the_line_above() {
        let ir = lower_main("println(1);\nprintln();\nprintln(2);");
        assert_eq!(ir.texts, vec!["\n".to_string()], "the blank line is its own write");
        let kinds: Vec<&str> = ir.functions[0].blocks[0]
            .instrs
            .iter()
            .filter_map(|i| match i {
                Instr::Print { newline: true, .. } => Some("println"),
                Instr::Print { .. } => Some("print"),
                Instr::PrintText { .. } => Some("text"),
                _ => None,
            })
            .collect();
        assert_eq!(kinds, vec!["println", "text", "println"]);
    }

    /// A `println` whose last part is text still ends the line with that text,
    /// exactly as before: there is a piece to join the newline to.
    #[test]
    fn a_println_ending_in_text_still_ends_the_line_with_the_text() {
        let ir = lower_main("int n = 1;\nprintln(\"n is %d.\", n);");
        assert_eq!(ir.texts, vec!["n is ".to_string(), ".\n".to_string()]);
        assert!(
            ir.functions[0].blocks[0]
                .instrs
                .iter()
                .all(|i| !matches!(i, Instr::Print { newline: true, .. })),
            "the text ends the line, so no value does"
        );
    }

    /// Every value is evaluated before anything is written.
    ///
    /// A `print` is written like a call and read like one, so its arguments go
    /// first — otherwise a call that writes something itself would land in the
    /// middle of this line rather than before it.
    #[test]
    fn the_values_are_evaluated_before_the_first_write() {
        let ir = lower_src(
            "fn f() -> int {\n  return 1;\n}\n\
             fn main() {\n  println(\"a %d b %d\", f(), f());\n}",
        );
        let main = ir.functions.iter().find(|f| f.name == "main").expect("main");
        let kinds: Vec<&str> = main.blocks[0]
            .instrs
            .iter()
            .filter_map(|i| match i {
                Instr::Call { .. } => Some("call"),
                Instr::Print { .. } => Some("print"),
                Instr::PrintText { .. } => Some("text"),
                _ => None,
            })
            .collect();
        // Six pieces, not seven: the format ends in a specifier, so the last
        // *value* ends the line and there is nothing to write after it.
        assert_eq!(kinds, vec!["call", "call", "text", "print", "text", "print"]);
        assert_eq!(ir.texts, vec!["a ".to_string(), " b ".to_string()]);
    }

    /// A `print` with nothing to write lowers to nothing at all; a `println`
    /// with nothing to write lowers to the line ending alone.
    #[test]
    fn writing_nothing_costs_nothing() {
        assert!(lower_main("print();").functions[0].blocks[0].instrs.is_empty());
        assert_eq!(lower_main("println();").texts, vec!["\n".to_string()]);
    }

    /// Text and strings are interned in separate tables because they are laid
    /// out differently — and the same words in both roles land in both.
    #[test]
    fn the_same_words_can_be_text_in_one_place_and_a_string_in_another() {
        let ir = lower_main("string s = \"hi\";\nprint(\"hi\");\nprint(s);");
        assert_eq!(ir.texts, vec!["hi".to_string()]);
        assert_eq!(ir.strings, vec![vec!['h', 'i']]);
    }

    // -- float ---------------------------------------------------------------

    /// A float literal becomes the bits of its double, and the instructions
    /// that read those bits say so. Nothing else in the IR changes shape.
    #[test]
    fn a_float_travels_as_the_bits_of_its_double() {
        let ir = lower_main("float a = 1.5;\nfloat b = a * 2.0;\nprintln(b < a);");
        assert_eq!(
            body_dump(&ir),
            concat!(
                "entry0:\n",
                "  0  %a = const 4609434218613702656\n",
                "  1  %b = mul.f %a, 2f\n",
                "  2  %t2 = cmp.f < %b, %a\n",
                "  3  println bool %t2\n",
                "  4  return\n",
            )
        );
        assert_eq!(f64::from_bits(4609434218613702656u64), 1.5);
    }

    /// Folding a float is done in `f64`, not on the bits, so the compiler and
    /// the machine cannot come to different answers.
    #[test]
    fn float_constants_fold_as_floats() {
        let ir = lower_main("println(1.5 + 2.25);");
        assert_eq!(body_dump(&ir), "entry0:\n  0  println float 3.75f\n  1  return\n");

        // Adding the two bit patterns as integers would answer this instead,
        // which is what makes the `num` on the instruction load-bearing.
        let wrong = f64::to_bits(1.5) + f64::to_bits(2.25);
        assert_ne!(wrong, f64::to_bits(3.75));
    }

    /// `-x` is a subtraction from **negative** zero. `0.0 - x` would answer
    /// `+0.0` where `x` was `+0.0`, and the difference is invisible until
    /// something divides by the result.
    #[test]
    fn negating_a_float_subtracts_from_negative_zero() {
        let ir = lower_main("float a = 0.0;\nfloat b = -a;\nprintln(b);");
        assert!(body_dump(&ir).contains("%b = sub.f -0f, %a"), "{}", body_dump(&ir));
        assert_eq!(negate_const(Num::Float, 0.0f64.to_bits() as i64), (-0.0f64).to_bits() as i64);
    }

    /// A conversion that can be settled here is, and the one that cannot stays
    /// an instruction — the same bargain `char(n)` strikes.
    #[test]
    fn a_constant_conversion_is_settled_where_it_can_be() {
        // Both fold to a `const`, and neither leaves an instruction that could
        // stop the program.
        let up = body_dump(&lower_main("float f = float(3);\nprintln(f);"));
        assert!(up.contains("%f = const 4613937818241073152"), "{up}");
        let down = body_dump(&lower_main("int n = int(3.75);\nprintln(n);"));
        assert!(down.contains("%n = const 3"), "{down}");

        // What only the running program knows stays an instruction, and only in
        // the direction that can fail.
        let ir = lower_main("int n = 3;\nfloat f = float(n) / 2.0;\nprintln(int(f));");
        let dump = body_dump(&ir);
        assert!(dump.contains(" = int %"), "{dump}");
    }

    /// Float arithmetic answers whatever it is given — an infinity, a NaN — so
    /// there is nothing to guard and nothing keeping a result nobody reads.
    #[test]
    fn float_arithmetic_cannot_fail() {
        let division = |num| Instr::Bin {
            num,
            op: BinOp::Div,
            dst: VReg(0),
            lhs: Value::Reg(VReg(1)),
            rhs: Value::Reg(VReg(2)),
        };
        assert!(!division(Num::Float).can_fail());
        assert!(
            division(Num::Int).can_fail(),
            "an int division still has a zero divisor to worry about"
        );

        // Only one direction of the conversion can.
        let cast = |to| Instr::Cast { dst: VReg(0), to, src: Value::Reg(VReg(1)) };
        assert!(cast(Num::Int).can_fail());
        assert!(!cast(Num::Float).can_fail());
    }
}
