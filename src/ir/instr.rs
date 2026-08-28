//! The instruction set, and what each instruction reads, writes and can refuse.

use crate::ast::{BinOp, Builtin, ClassId, CmpOp, EnumId, Ty};
use super::{FuncId, Num, StrId, TextId, VReg, Value};

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
    /// The frame region is reserved once, in the prologue, so this is an
    /// address computation and never an allocation.
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
    /// `scale` is a word for everything that fits in a register,
    /// [`CHAR_BYTES`] for the characters of a string, and the object.s room for
    /// an array or a list of them — so it is **any** number of bytes, not one
    /// of a machine.s handful. Turning an arbitrary scale into an address is
    /// the backend.s problem: one that has a scaled addressing mode uses it
    /// where the number fits and multiplies where it does not, and one that has
    /// none multiplies every time.
    Elem { dst: VReg, base: Value, index: Value, len: Value, scale: u32 },
    /// `dst = len(of)`: the count that a string or a list carries in the word
    /// in front of its elements.
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
            // Addition, subtraction and multiplication are all guarded; a
            // folded result never reaches an instruction in the first place.
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

