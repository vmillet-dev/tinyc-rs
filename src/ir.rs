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

use std::collections::HashMap;

use crate::ast::{
    ArmBody, BinOp, Block as AstBlock, Builtin, ClassId, CmpOp, Expr, ExprKind, FnDecl, LogicOp,
    MatchArm,
    Place, Prim, Program as Ast, Stmt, Ty, TypeTable, is_scalar_value,
};
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

/// Index into [`Program::strings`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StrId(pub u32);

/// Index into [`Function::blocks`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId(pub u32);

/// Index into [`Program::functions`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FuncId(pub u32);

/// An instruction operand: either an immediate or a virtual register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value {
    Const(i64),
    Reg(VReg),
}

#[derive(Clone, Debug)]
pub enum Instr {
    /// `dst = val`
    Const { dst: VReg, val: i64 },
    /// `dst = &strings[id]`
    StrAddr { dst: VReg, id: StrId },
    /// `dst = src`
    Copy { dst: VReg, src: Value },
    /// `dst = lhs op rhs`
    Bin { op: BinOp, dst: VReg, lhs: Value, rhs: Value },
    /// `dst = (lhs op rhs)`, producing 0 or 1.
    Cmp { op: CmpOp, dst: VReg, lhs: Value, rhs: Value },
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
    Print { ty: Ty, val: Value },
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
}

impl Runtime {
    /// What this routine is called, without the prefix a backend adds.
    pub fn name(self) -> &'static str {
        match self {
            Runtime::Concat => "concat",
            Runtime::StrEq => "str_eq",
            Runtime::CheckChar => "check_char",
            Runtime::CharToStr => "char_str",
            Runtime::IntToStr => "int_str",
            Runtime::ListNew => "list_new",
            Runtime::ListPush => "list_push",
            Runtime::ListPushBig => "list_push_big",
            Runtime::ListClone => "list_clone",
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
            | Instr::VTable { .. } => {}
            Instr::Copy { src, .. }
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
            | Instr::Bin { dst, .. }
            | Instr::Param { dst, .. }
            | Instr::Frame { dst, .. }
            | Instr::VTable { dst, .. }
            | Instr::Elem { dst, .. }
            | Instr::Field { dst, .. }
            | Instr::Load { dst, .. }
            | Instr::LoadChar { dst, .. }
            | Instr::Count { dst, .. }
            | Instr::Cmp { dst, .. } => Some(*dst),
            Instr::Call { dst, .. } | Instr::CallVirtual { dst, .. } | Instr::RtCall { dst, .. } => {
                *dst
            }
            Instr::Print { .. } | Instr::Store { .. } | Instr::CopyBytes { .. } => None,
        }
    }

    /// Whether this instruction performs a call, and therefore destroys the
    /// caller-saved registers.
    pub fn is_call(&self) -> bool {
        matches!(
            self,
            Instr::Print { .. }
                | Instr::Call { .. }
                | Instr::CallVirtual { .. }
                | Instr::RtCall { .. }
        )
    }
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
        match value {
            Value::Const(c) => c.to_string(),
            Value::Reg(r) => format!("%{}", self.name_of(*r)),
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
            Instr::Bin { op, dst, lhs, rhs } => format!(
                "%{} = {} {}, {}",
                function.name_of(*dst),
                op_name(*op),
                value(lhs),
                value(rhs)
            ),
            Instr::Cmp { op, dst, lhs, rhs } => format!(
                "%{} = cmp {} {}, {}",
                function.name_of(*dst),
                op.symbol(),
                value(lhs),
                value(rhs)
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
            Instr::Print { ty, val } => format!("print {} {}", ty.name(&self.table), value(val)),
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
pub fn lower(ast: &Ast, types: &Types) -> Program {
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
    for (index, decl) in ast.functions.iter().enumerate() {
        let lowering = Lowering {
            blocks: Vec::new(),
            vreg_names: Vec::new(),
            current: BlockId(0),
            scopes: vec![HashMap::new()],
            loops: Vec::new(),
            frame_bytes: 0,
            out_pointer: None,
            name_counts: HashMap::new(),
            types,
            table: types.table(),
            func_ids: &func_ids,
            strings: &mut strings,
            ids: &ids,
        };
        let mut lowered =
            lowering.run(decl, types.ret_of(index), types.params_of(index));
        lowered.name = names[index].clone();
        functions.push(lowered);
    }

    // One vtable per class, holding the implementation each slot resolved to.
    let vtables: Vec<Vec<FuncId>> = types
        .table()
        .classes
        .iter()
        .map(|class| class.methods.iter().map(|m| func_ids[m.function]).collect())
        .collect();

    let (functions, vtables) =
        prune_unreachable_functions(functions, vtables, ids.get(crate::sema::ENTRY_POINT));
    Program { functions, strings: strings.chars, table: types.table().clone(), vtables }
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
fn prune_unreachable(blocks: Vec<Block>) -> Vec<Block> {
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
    /// Bytes of frame handed out so far, which is the offset the next aggregate
    /// gets. Never reclaimed: the room is reserved for the whole call, which
    /// costs a few bytes and spares the lowering a lifetime analysis.
    frame_bytes: u32,
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
            frame_bytes: self.frame_bytes,
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

    fn block_stmts(&mut self, block: &AstBlock) {
        self.scopes.push(HashMap::new());
        for stmt in &block.stmts {
            self.stmt(stmt);
        }
        self.scopes.pop();
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
                        self.write_through(Value::Reg(dst), init);
                    }
                }
            }
            Stmt::Assign { target, value } => match target {
                Place::Var { name, .. } => {
                    let (dst, ty) = self.binding(name);
                    match ty.fits_in_a_register() {
                        // The variable keeps its register; the assignment
                        // overwrites it.
                        true => self.keep_into(dst, value),
                        // An aggregate variable keeps its *room*, so the value
                        // is copied into it rather than the address swapped.
                        // Anything else would make assignment aliasing.
                        false => self.write_through(Value::Reg(dst), value),
                    }
                }
                // Everything else names memory rather than a register, so the
                // write goes through an address.
                target => {
                    let addr = self.place_address(target);
                    self.write_through(Value::Reg(addr), value);
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
                    false => {
                        (Runtime::ListPushBig, vec![value, Value::Const(i64::from(bytes))])
                    }
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
            Stmt::Print { value, .. } => {
                let ty = self.types.of(value.id);
                let val = self.expr(value);
                self.emit(Instr::Print { ty, val });
            }
            Stmt::If { cond, then_block, else_block } => self.if_stmt(cond, then_block, else_block),
            Stmt::While { cond, body } => self.while_stmt(cond, body),
            // `for (init; cond; step) body` is exactly `init; while (cond) { body; step; }`
            // with the initialiser's variable scoped to the loop.
            Stmt::For { init, cond, step, body } => {
                self.scopes.push(HashMap::new());
                self.stmt(init);
                self.loop_with_step(cond, body, Some(step));
                self.scopes.pop();
            }
            // An aggregate answer is copied into the room the caller reserved,
            // and the function then leaves with nothing — there is no address
            // to hand back, which is exactly why none can dangle.
            Stmt::Return { value: Some(expr), .. } if self.out_pointer.is_some() => {
                let out = self.out_pointer.expect("just matched");
                self.write_through(Value::Reg(out), expr);
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
        let before = self.current;

        // Where each arm's decision begins, so a failing test knows where to
        // send control; and the tests themselves, which cannot be finished
        // until the arm *after* them exists.
        let mut entries = Vec::new();
        let mut tests = Vec::new();
        let mut exits = Vec::new();

        for (index, arm) in arms.iter().enumerate() {
            let tag = self.arm_tag(scrutinee, arm);
            if index + 1 == arms.len() {
                entries.push(self.new_block(BlockKind::Arm));
            } else {
                // The first test belongs to the block the scrutinee was
                // computed in; each later one gets a block of its own for the
                // previous test to fail into.
                if index > 0 {
                    self.new_block(BlockKind::Case);
                }
                entries.push(self.current);
                let cond = self.fresh_temp();
                self.emit(Instr::Cmp {
                    op: CmpOp::Eq,
                    dst: cond,
                    lhs: value,
                    rhs: Value::Const(tag),
                });
                let test = self.current;
                let arm_block = self.new_block(BlockKind::Arm);
                tests.push((test, Value::Reg(cond), arm_block));
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
    fn variant_tag(&self, expr: &Expr) -> i64 {
        let ExprKind::Variant { variant, .. } = &expr.kind else {
            unreachable!("the caller matched a variant");
        };
        let Ty::Enum(id) = self.types.of(expr.id) else {
            unreachable!("sema gives a variant its enum's type");
        };
        self.table.enum_info(id).tag(variant).expect("sema rejects an unknown variant")
    }

    /// The tag an arm's pattern selects.
    ///
    /// Both halves were checked by [`crate::sema`], which is what makes the
    /// lookup here an `expect` rather than a diagnostic.
    fn arm_tag(&self, scrutinee: &Expr, arm: &MatchArm) -> i64 {
        let Ty::Enum(id) = self.types.of(scrutinee.id) else {
            unreachable!("sema rejects a match on anything but an enum");
        };
        self.table.enum_info(id).tag(&arm.variant).expect("sema rejects an unknown variant")
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
            let (_, bytes) = self.element_of(ty);
            self.emit(Instr::RtCall {
                dst: Some(dst),
                callee: Runtime::ListClone,
                args: vec![Value::Reg(dst), Value::Const(i64::from(bytes))],
            });
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
    fn write_through(&mut self, addr: Value, value: &Expr) {
        let ty = self.types.of(value.id);
        if ty.fits_in_a_register() {
            let value = self.expr(value);
            self.emit(Instr::Store { addr, value });
            return;
        }
        let src = self.expr(value);
        let bytes = self.table.size_of(ty);
        self.emit(Instr::CopyBytes { dst: addr, src, bytes });
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
    fn allocate(&mut self, dst: VReg, bytes: u32) {
        let offset = self.frame_bytes;
        self.frame_bytes += bytes;
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
            ExprKind::Bool(v) => self.emit(Instr::Const { dst, val: i64::from(*v) }),
            ExprKind::Variant { .. } => {
                let val = self.variant_tag(expr);
                self.emit(Instr::Const { dst, val });
            }
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
                let info = self.table.class(id).clone();
                self.allocate(dst, info.storage);

                // The vtable pointer goes in first, at offset 0. It is what
                // makes the object *this* class rather than merely its shape,
                // and it is what travels with a copy.
                let vptr = self.fresh_temp();
                self.emit(Instr::VTable { dst: vptr, class: id });
                self.emit(Instr::Store { addr: Value::Reg(dst), value: Value::Reg(vptr) });

                for init in fields {
                    let offset = info
                        .field(&init.name)
                        .expect("sema rejects an unknown field")
                        .offset;
                    let addr = self.fresh_temp();
                    self.emit(Instr::Field { dst: addr, base: Value::Reg(dst), offset });
                    self.write_through(Value::Reg(addr), &init.value);
                }
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
                    self.write_through(Value::Reg(addr), element);
                }
            }
            ExprKind::Array { elements, .. } => {
                let ty = self.types.of(expr.id);
                let (len, scale) = self.shape_of(ty, Value::Reg(dst));
                self.allocate_for(dst, ty);
                for (index, element) in elements.iter().enumerate() {
                    let addr = self.fresh_temp();
                    self.emit(Instr::Elem {
                        dst: addr,
                        base: Value::Reg(dst),
                        index: Value::Const(index as i64),
                        len,
                        scale,
                    });
                    self.write_through(Value::Reg(addr), element);
                }
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
                let lhs = self.expr(lhs);
                let rhs = self.expr(rhs);
                match fold_bin(*op, lhs, rhs) {
                    Some(val) => self.emit(Instr::Const { dst, val }),
                    None => self.emit(Instr::Bin { op: *op, dst, lhs, rhs }),
                }
            }
            ExprKind::Cmp { op, lhs, rhs } => {
                let lhs = self.expr(lhs);
                let rhs = self.expr(rhs);
                match fold_cmp(*op, lhs, rhs) {
                    Some(val) => self.emit(Instr::Const { dst, val }),
                    None => self.emit(Instr::Cmp { op: *op, dst, lhs, rhs }),
                }
            }
            ExprKind::Neg(operand) => {
                let val = self.expr(operand);
                match val {
                    Value::Const(c) => self.emit(Instr::Const { dst, val: c.wrapping_neg() }),
                    val => self.emit(Instr::Bin {
                        op: BinOp::Sub,
                        dst,
                        lhs: Value::Const(0),
                        rhs: val,
                    }),
                }
            }
            ExprKind::Not(operand) => {
                let (op, lhs, rhs) = self.negated(operand);
                match fold_cmp(op, lhs, rhs) {
                    Some(val) => self.emit(Instr::Const { dst, val }),
                    None => self.emit(Instr::Cmp { op, dst, lhs, rhs }),
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
    fn negated(&mut self, operand: &Expr) -> (CmpOp, Value, Value) {
        if let ExprKind::Cmp { op, lhs, rhs } = &operand.kind {
            let lhs = self.expr(lhs);
            let rhs = self.expr(rhs);
            return (op.negate(), lhs, rhs);
        }
        (CmpOp::Eq, self.expr(operand), Value::Const(0))
    }

    /// Lower an expression used as an operand, producing a value to read.
    fn expr(&mut self, expr: &Expr) -> Value {
        match &expr.kind {
            // Literals stay immediates so the backend can fold them into the
            // instruction that consumes them.
            ExprKind::Int(v) => Value::Const(*v),
            ExprKind::Bool(v) => Value::Const(i64::from(*v)),
            ExprKind::Var(name) => Value::Reg(self.lookup(name)),
            // A variant *is* its tag, so it needs no more machinery than an
            // integer literal does — which is the whole reason a payload-free
            // enum costs the backend nothing.
            ExprKind::Variant { .. } => Value::Const(self.variant_tag(expr)),
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
            ExprKind::Neg(operand) => match self.expr(operand) {
                // An operand that is already a literal folds, and so does the
                // whole tree above it: `-(2 * 3)` never reaches an instruction.
                Value::Const(c) => Value::Const(c.wrapping_neg()),
                val => {
                    let dst = self.fresh_temp();
                    self.emit(Instr::Bin {
                        op: BinOp::Sub,
                        dst,
                        lhs: Value::Const(0),
                        rhs: val,
                    });
                    Value::Reg(dst)
                }
            },
            ExprKind::Bin { .. } | ExprKind::Cmp { .. } if self.is_string_op(expr) => {
                let dst = self.fresh_temp();
                self.string_op_into(dst, expr);
                Value::Reg(dst)
            }
            ExprKind::Bin { op, lhs, rhs } => {
                let lhs = self.expr(lhs);
                let rhs = self.expr(rhs);
                if let Some(val) = fold_bin(*op, lhs, rhs) {
                    return Value::Const(val);
                }
                let dst = self.fresh_temp();
                self.emit(Instr::Bin { op: *op, dst, lhs, rhs });
                Value::Reg(dst)
            }
            ExprKind::Cmp { op, lhs, rhs } => {
                let lhs = self.expr(lhs);
                let rhs = self.expr(rhs);
                if let Some(val) = fold_cmp(*op, lhs, rhs) {
                    return Value::Const(val);
                }
                let dst = self.fresh_temp();
                self.emit(Instr::Cmp { op: *op, dst, lhs, rhs });
                Value::Reg(dst)
            }
            // `!` is a comparison, so it takes the same path as one.
            ExprKind::Not(operand) => {
                let (op, lhs, rhs) = self.negated(operand);
                if let Some(val) = fold_cmp(op, lhs, rhs) {
                    return Value::Const(val);
                }
                let dst = self.fresh_temp();
                self.emit(Instr::Cmp { op, dst, lhs, rhs });
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
fn fold_bin(op: BinOp, lhs: Value, rhs: Value) -> Option<i64> {
    let (Value::Const(a), Value::Const(b)) = (lhs, rhs) else { return None };
    op.apply(a, b)
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
fn fold_cmp(op: CmpOp, lhs: Value, rhs: Value) -> Option<i64> {
    let (Value::Const(a), Value::Const(b)) = (lhs, rhs) else { return None };
    let answer = match op {
        CmpOp::Eq => a == b,
        CmpOp::Ne => a != b,
        CmpOp::Lt => a < b,
        CmpOp::Le => a <= b,
        CmpOp::Gt => a > b,
        CmpOp::Ge => a >= b,
    };
    Some(i64::from(answer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lexer, parser, sema};

    fn lower_src(src: &str) -> Program {
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
        assert_eq!(ir.table.enums[0].variants, vec!["Red", "Green", "Blue"]);
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
                fold_bin(op, Value::Const(a), Value::Const(b)),
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
                fold_bin(op, Value::Const(a), Value::Const(b)),
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
            Instr::Print { ty: Ty::Bool, val: Value::Const(0) }
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
        let ir = lower_main("print(\"é\");");
        assert_eq!(ir.strings[0], vec!['é']);
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
            "fn a() {\n  print(\"hi\");\n}\nfn main() {\n  print(\"hi\");\n  a();\n}",
        );
        assert_eq!(ir.strings.len(), 1);
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
}
