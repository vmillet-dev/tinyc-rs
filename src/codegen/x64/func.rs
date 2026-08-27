//! One TinyC function, from IR to instructions.
//!
//! Instruction selection is the same on both platforms, and nearly all of this
//! file never asks which one it is emitting for. What does is short and it is
//! all about calls: which registers arguments travel in, how much room a caller
//! owes its callee, and whether a variadic call has to announce itself. Those
//! come from [`Abi`]; the entry point's one platform-specific duty comes from
//! [`Platform::entry_setup`].

use crate::ast::{BinOp, CmpOp, Ty};
use crate::codegen::{Allocation, Location, RegisterFile};
use crate::ir::{
    Block, DivGuards, Function, Instr, Num, Program, STR_HEADER, Terminator, VReg, Value,
};

use super::asm::Asm;
use super::data::{
    BOOL_FALSE, BOOL_TRUE, FMT_BOOL, FMT_FLOAT, FMT_INT, FMT_STR, NEWLINE, line_format,
};
use super::runtime::{
    ABORT_BOUNDS, ABORT_DIV_OVERFLOW, ABORT_DIV_ZERO, ABORT_NO_INT, ABORT_OVERFLOW, ABORT_STACK,
    PRINT_CHAR, FIXUP, PRINT_STR, STACK_LIMIT, STACK_MARGIN, WRITE_TEXT,
};
use super::used::{Used, enum_table, text_label, variant_value, vtable_label};
use super::{
    Abi, ENTRY_POINT, PAGE_BYTES, Platform, RAX, RDX, SCRATCH0, SCRATCH0_8, SCRATCH1, SCRATCH1_8,
    XMM0, XMM1, float_setcc, half, jump_if_false, narrow, runtime_symbol, setcc, symbol,
};

pub struct FnEmitter<'a, 'o> {
    program: &'a Program,
    function: &'a Function,
    allocation: &'a Allocation,
    registers: &'a RegisterFile,
    platform: &'a dyn Platform,
    abi: &'static Abi,
    used: &'a Used,
    frame: FrameLayout,
    asm: &'o mut Asm,
    /// Serial number for the local labels a division guard needs.
    next_label: u32,
}

impl<'a, 'o> FnEmitter<'a, 'o> {
    pub fn new(
        program: &'a Program,
        function: &'a Function,
        allocation: &'a Allocation,
        registers: &'a RegisterFile,
        platform: &'a dyn Platform,
        used: &'a Used,
        asm: &'o mut Asm,
    ) -> FnEmitter<'a, 'o> {
        // A leaf makes no call, so it owes neither shadow space nor alignment.
        // Jumping to a runtime abort does not count: that routine builds its
        // own frame, precisely so a function full of guarded arithmetic does
        // not have to carry one for a path it almost never takes.
        //
        // The entry point may not be a leaf even when nothing in it calls
        // anything: on a platform whose console has to be told what encoding is
        // coming, saying so is a call, and so is asking the operating system
        // where the stack ends. Reading never adds one here — whatever a
        // console needs is settled inside `tc$rt$refill`.
        //
        // The second of those is the shared one: both platforms ask, so the
        // condition is `Used`'s rather than the platform's.
        let entry = function.name == ENTRY_POINT;
        let entry_calls = platform.entry_calls(used) || used.checks_stack;
        let leaf = !(entry && entry_calls)
            && !function
                .blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .any(|instr| instr.is_call());

        let abi = platform.abi();
        let frame = FrameLayout::new(allocation, function.frame_bytes, leaf, abi.shadow_space);
        FnEmitter {
            program,
            function,
            allocation,
            registers,
            platform,
            abi,
            used,
            frame,
            asm,
            next_label: 0,
        }
    }

    pub fn run(&mut self) {
        self.asm.blank();
        for line in self.allocation.dump(self.function, self.registers).lines() {
            self.asm.comment(line);
        }
        self.asm.line(&format!("{}:", symbol(&self.function.name)));
        self.prologue();
        if self.is_entry_point() {
            self.set_stack_limit();
            self.platform.entry_setup(self.asm, self.used);
        }

        for (index, block) in self.function.blocks.iter().enumerate() {
            self.asm.blank();
            self.asm.line(&format!(".{}:", block.label()));

            let fused = self.fusable_compare(block);
            for (position, instr) in block.instrs.iter().enumerate() {
                if Some(position) == fused {
                    // Emitted by the terminator instead, as a `cmp` and a jump.
                    continue;
                }
                let text = self.program.instr_text(self.function, instr);
                self.asm.comment(&text);
                self.instr(instr);
            }
            self.terminator(block, index, fused.map(|at| &block.instrs[at]));
        }
    }

    fn is_entry_point(&self) -> bool {
        self.function.name == ENTRY_POINT
    }

    /// Where argument `index` travels on this platform.
    fn arg(&self, index: usize) -> &'static str {
        self.abi.arg(index)
    }

    /// A local label nothing else in this function will claim.
    fn local_label(&mut self, stem: &str) -> String {
        let label = format!("{stem}{}", self.next_label);
        self.next_label += 1;
        label
    }

    // -- frame -------------------------------------------------------------

    fn prologue(&mut self) {
        let saved: Vec<&str> = self
            .allocation
            .used_callee_saved
            .iter()
            .map(|&reg| self.registers.name(reg))
            .collect();

        self.asm.comment("prologue: save callee-saved registers, then reserve the frame");
        for reg in saved {
            self.asm.asm(&format!("push {reg}"));
        }
        self.stack_check();
        self.reserve_frame();
    }

    /// Whether this function's prologue asks whether there is stack left.
    ///
    /// Every function does, except the entry point — which cannot, because it
    /// is what works the limit out, and its own frame is taken before the
    /// answer exists. That is not a hole: `main` is entered exactly once, so
    /// the depth this guards against is the one thing it cannot reach. What
    /// bounds *its* frame is [`crate::ir::MAX_FRAME_BYTES`], at compile time.
    ///
    /// Checking in every other function rather than only in the ones that
    /// recurse is what keeps [`STACK_MARGIN`] small. What runs unchecked below
    /// a passing check is then only the runtime's own routines and the C
    /// library — never another TinyC frame, which could be a quarter of a
    /// megabyte on its own.
    fn needs_stack_check(&self) -> bool {
        self.used.checks_stack && !self.is_entry_point()
    }

    /// Stop rather than take a frame the stack has no room for.
    ///
    /// Asked *before* the frame is reserved, so what it proves is that after
    /// the prologue there is still [`STACK_MARGIN`] left — which is what the
    /// abort path needs in order to be able to say what happened. A check made
    /// afterwards would be asked from the very place it was meant to protect.
    ///
    /// Unsigned, because these are addresses: `jb`, not `jl`.
    fn stack_check(&mut self) {
        if !self.needs_stack_check() {
            return;
        }
        self.asm.comment("stop rather than take a frame the stack has no room for");
        match self.frame.size {
            // Nothing to reserve, so `rsp` is already where it will stay.
            0 => self.asm.asm(&format!("cmp  rsp, [{STACK_LIMIT}]")),
            size => {
                self.asm.asm(&format!("lea  {RAX}, [rsp-{size}]"));
                self.asm.asm(&format!("cmp  {RAX}, [{STACK_LIMIT}]"));
            }
        }
        self.asm.asm(&format!("jb   {ABORT_STACK}"));
    }

    /// Take the frame — one page at a time when it is bigger than one.
    ///
    /// A stack is not simply *there* down to its end. Below the pages in use
    /// sits one whose whole purpose is to be touched, and touching it is what
    /// makes the next one exist. So `sub rsp, 245792` followed by a write is
    /// two mistakes in a row: it steps over that page without touching it, and
    /// then writes to memory the program was never given. On Windows that is an
    /// access violation on a stack with room to spare — a program the compiler
    /// accepted, crashing for a reason nothing in it could explain.
    ///
    /// Walking down instead touches every page in order, which is exactly what
    /// the mechanism is waiting for. The loop leaves `rsp` at the same place
    /// the single `sub` would have, so the epilogue is unchanged.
    fn reserve_frame(&mut self) {
        let size = self.frame.size;
        if size == 0 {
            return;
        }
        if size <= PAGE_BYTES {
            self.asm.asm(&format!("sub  rsp, {size}    ; {}", self.frame.describe()));
            return;
        }

        let again = self.local_label("probe");
        self.asm.comment(&format!("{}, which is more than one page:", self.frame.describe()));
        self.asm.comment("walk down it touching each one, because a stack only");
        self.asm.comment("reaches as far as it has been written to");
        self.asm.asm(&format!("mov  {RAX}, {size}    ; still to reserve"));
        self.asm.line(&format!(".{again}:"));
        self.asm.asm(&format!("sub  rsp, {PAGE_BYTES}"));
        self.asm.asm("mov  qword [rsp], 0    ; the touch; nothing is there yet to lose");
        self.asm.asm(&format!("sub  {RAX}, {PAGE_BYTES}"));
        self.asm.asm(&format!("cmp  {RAX}, {PAGE_BYTES}"));
        self.asm.asm(&format!("ja   .{again}"));
        self.asm.comment("what is left is under a page, so the frame's own first write");
        self.asm.comment("lands on the page after the last one touched");
        self.asm.asm(&format!("sub  rsp, {RAX}"));
    }

    /// Work out where the stack ends, once, for every prologue after this one.
    ///
    /// The entry point is the only place this can happen: it is the first thing
    /// to run, and the answer has to be there before the second function is
    /// called. Asking per call would be a system call per call.
    fn set_stack_limit(&mut self) {
        if !self.used.checks_stack {
            return;
        }
        self.asm.comment("where this thread's stack ends, asked once and read by every prologue");
        self.platform.stack_bottom(self.asm);
        let known = self.local_label("stack_limit");
        self.asm.comment("a platform that could not say leaves zero, and a zero never fires");
        self.asm.asm(&format!("test {RAX}, {RAX}"));
        self.asm.asm(&format!("jz   .{known}"));
        let margin = format!("add  {RAX}, {STACK_MARGIN}    ; left over for the report to run in");
        self.asm.asm(&margin);
        self.asm.asm(&format!("mov  [{STACK_LIMIT}], {RAX}"));
        self.asm.line(&format!(".{known}:"));
    }

    fn epilogue(&mut self) {
        if self.frame.size > 0 {
            self.asm.asm(&format!("add  rsp, {}", self.frame.size));
        }
        let saved: Vec<&str> = self
            .allocation
            .used_callee_saved
            .iter()
            .rev()
            .map(|&reg| self.registers.name(reg))
            .collect();
        for reg in saved {
            self.asm.asm(&format!("pop  {reg}"));
        }
        self.asm.asm("ret");
    }

    // -- terminators -------------------------------------------------------

    /// The comparison this block's branch can be folded into, if any.
    ///
    /// x86 compares by setting flags and `jcc` reads them straight back, so a
    /// comparison whose only reader is the branch right after it never has to
    /// become a 0 or a 1 at all. That is the shape of every `if` and every loop
    /// header, which is what makes the check worth making.
    ///
    /// The allocator has already given the comparison's destination a register
    /// by the time this runs, so that register stays reserved even though
    /// nothing writes it any more. Fixing that would mean deciding the fusion
    /// before allocation, which would put x86's flags register into the
    /// target-independent half of the compiler.
    /// A float comparison is never fused, and that is deliberate. `ucomisd`
    /// reports "either operand is a NaN" in a flag of its own, so `==` and `!=`
    /// are two conditions combined rather than one — there is no single `jcc`
    /// that means either of them. Materialising the 0 or 1 costs an instruction
    /// and makes the four orderings and the two equalities one shape instead of
    /// two, one of which would have to invent a label to jump around.
    fn fusable_compare(&self, block: &Block) -> Option<usize> {
        let Terminator::Branch { cond: Value::Reg(cond), .. } = &block.term else { return None };
        let at = block.instrs.len().checked_sub(1)?;
        let Instr::Cmp { num: Num::Int, dst, .. } = &block.instrs[at] else { return None };
        (dst == cond && self.reads(*dst) == 1).then_some(at)
    }

    /// How many times `reg` is read in the whole function.
    fn reads(&self, reg: VReg) -> usize {
        let mut count = 0;
        for block in &self.function.blocks {
            for instr in &block.instrs {
                instr.uses(|read| count += usize::from(read == reg));
            }
            block.term.uses(|read| count += usize::from(read == reg));
        }
        count
    }

    /// Emit a block's exit. A jump to the very next block is left out: control
    /// simply falls through.
    fn terminator(&mut self, block: &Block, index: usize, fused: Option<&Instr>) {
        let next = index + 1;
        match &block.term {
            Terminator::Jump(target) => {
                if target.0 as usize != next {
                    let label = self.function.block(*target).label();
                    self.asm.comment(&format!("jump {label}"));
                    self.asm.asm(&format!("jmp  .{label}"));
                }
            }
            Terminator::Branch { cond, then_blk, else_blk } => {
                let then_label = self.function.block(*then_blk).label();
                let else_label = self.function.block(*else_blk).label();

                // A folded condition settles the branch outright.
                if let Value::Const(c) = cond {
                    let (taken, label) =
                        if *c != 0 { (then_blk, &then_label) } else { (else_blk, &else_label) };
                    self.asm.comment(&format!("branch always taken to {label}"));
                    if taken.0 as usize != next {
                        self.asm.asm(&format!("jmp  .{label}"));
                    }
                    return;
                }

                self.asm.comment(&format!("branch to {then_label} or {else_label}"));
                match fused {
                    Some(Instr::Cmp { op, lhs, rhs, .. }) => {
                        self.compare(lhs, rhs);
                        self.asm.asm(&format!("{:<4} .{else_label}", jump_if_false(*op)));
                    }
                    _ => {
                        // Anything non-zero is true, which `test` answers in one
                        // instruction — on the value where it already lives.
                        let cond = self.value(cond);
                        if cond.starts_with("qword ") {
                            self.asm.asm(&format!("cmp  {cond}, 0"));
                        } else {
                            self.asm.asm(&format!("test {cond}, {cond}"));
                        }
                        self.asm.asm(&format!("jz   .{else_label}"));
                    }
                }
                if then_blk.0 as usize != next {
                    self.asm.asm(&format!("jmp  .{then_label}"));
                }
            }
            Terminator::Return(value) => {
                match value {
                    // `main` always reports success, whatever it computed.
                    _ if self.is_entry_point() => {
                        self.asm.comment("return 0 to the CRT");
                        self.asm.asm("xor  eax, eax");
                    }
                    Some(value) => {
                        self.asm.comment("return value in rax");
                        let value = self.value(value);
                        self.asm.mov(RAX, &value);
                    }
                    None => self.asm.comment("return"),
                }
                self.epilogue();
            }
        }
    }

    // -- instruction selection --------------------------------------------

    fn instr(&mut self, instr: &Instr) {
        match instr {
            Instr::Const { dst, val } => self.produce(*dst, |e, work| {
                if *val == 0 {
                    // Clearing the 32-bit half zeroes the whole register, and
                    // costs three bytes fewer than an immediate zero.
                    e.asm.asm(&format!("xor  {0}, {0}", half(work)));
                } else {
                    e.asm.asm(&format!("mov  {work}, {val}"));
                }
            }),
            Instr::StrAddr { dst, id } => {
                self.produce(*dst, |e, work| e.asm.asm(&format!("lea  {work}, [str{}]", id.0)));
            }
            Instr::Copy { dst, src } => {
                let src = self.value(src);
                self.produce(*dst, |e, work| e.asm.mov(work, &src));
            }
            // The argument is already sitting in its ABI register; this just
            // moves it to wherever the allocator decided it should live.
            Instr::Param { dst, index } => {
                let src = self.arg(*index as usize);
                self.produce(*dst, |e, work| e.asm.mov(work, src));
            }
            // `cmp` sets the flags; `setcc` turns the one that matters into a
            // 0 or 1 byte, and `movzx` widens it to the 64-bit value a bool is.
            // A comparison that only feeds a branch never gets here — see
            // `fusable_compare`.
            Instr::Cmp { num: Num::Int, op, dst, lhs, rhs } => {
                self.compare(lhs, rhs);
                self.asm.asm(&format!("{} {SCRATCH1_8}", setcc(*op)));
                self.produce(*dst, |e, work| {
                    e.asm.asm(&format!("movzx {work}, {SCRATCH1_8}"));
                });
            }
            // The same shape, with the answer still arriving in `SCRATCH1_8`,
            // and two things `cmp` never has to do: the operands come out of
            // general registers into the vector ones, and a NaN has to be
            // ruled out by hand. See `float_setcc` for why the conditions are
            // the unsigned ones.
            Instr::Cmp { num: Num::Float, op, dst, lhs, rhs } => {
                let (swap, setcc) = float_setcc(*op);
                let (first, second) = match swap {
                    true => (rhs, lhs),
                    false => (lhs, rhs),
                };
                self.load_vector(first, XMM0);
                self.load_vector(second, XMM1);
                self.asm.asm(&format!("ucomisd {XMM0}, {XMM1}"));
                self.asm.asm(&format!("{setcc} {SCRATCH1_8}"));
                // `ucomisd` sets the parity flag when either operand is a NaN,
                // and sets ZF along with it — so `sete` alone would answer that
                // a NaN equals itself, and `setne` that it does not differ from
                // anything. Combining with PF is what makes both false, which
                // is what IEEE-754 asks for. The orderings need none of this:
                // `seta` and `setae` are already false when PF is set.
                match op {
                    CmpOp::Eq => {
                        self.asm.asm(&format!("setnp {SCRATCH0_8}"));
                        self.asm.asm(&format!("and  {SCRATCH1_8}, {SCRATCH0_8}"));
                    }
                    CmpOp::Ne => {
                        self.asm.asm(&format!("setp {SCRATCH0_8}"));
                        self.asm.asm(&format!("or   {SCRATCH1_8}, {SCRATCH0_8}"));
                    }
                    _ => {}
                }
                self.produce(*dst, |e, work| {
                    e.asm.asm(&format!("movzx {work}, {SCRATCH1_8}"));
                });
            }
            // Float arithmetic is one instruction on the vector registers, with
            // a `movq` at each end. No guard: an answer too large is an
            // infinity and zero into zero is a NaN, and both are values the
            // program may go on to use — see `Instr::can_fail`.
            Instr::Bin { num: Num::Float, op, dst, lhs, rhs } => {
                self.load_vector(lhs, XMM0);
                self.load_vector(rhs, XMM1);
                let mnemonic = match op {
                    BinOp::Add => "addsd",
                    BinOp::Sub => "subsd",
                    BinOp::Mul => "mulsd",
                    BinOp::Div => "divsd",
                    BinOp::Rem => unreachable!("sema rejects `%` on a float"),
                };
                self.asm.asm(&format!("{mnemonic} {XMM0}, {XMM1}"));
                self.produce(*dst, |e, work| e.asm.asm(&format!("movq {work}, {XMM0}")));
            }
            // One `idiv`, read from `rax` for the quotient or `rdx` for the
            // remainder.
            Instr::Bin { op: BinOp::Div, dst, lhs, rhs, .. } => self.division(*dst, RAX, lhs, rhs),
            Instr::Bin { op: BinOp::Rem, dst, lhs, rhs, .. } => self.division(*dst, RDX, lhs, rhs),
            Instr::Cast { dst, to, src } => self.cast(*dst, *to, src),
            Instr::Bin { op, dst, lhs, rhs, .. } => {
                // The result usually lands in a register that just held one of
                // the operands — that is register reuse working as intended.
                // Landing on `lhs` is free, because `mov work, lhs` then becomes
                // a no-op; landing on `rhs` would destroy it, so a commutative
                // operator swaps its operands and the rest use a scratch
                // register.
                let (lhs, rhs) = if op.commutes() && self.shares_register(*dst, rhs) {
                    (rhs, lhs)
                } else {
                    (lhs, rhs)
                };
                let clobbers_rhs =
                    self.shares_register(*dst, rhs) && !self.shares_register(*dst, lhs);

                self.produce_into(*dst, clobbers_rhs, |e, work| {
                    let lhs = e.value(lhs);
                    e.asm.mov(work, &lhs);

                    let (rhs, immediate) = e.operand_for_alu(rhs);
                    match op {
                        BinOp::Add => e.asm.asm(&format!("add  {work}, {rhs}")),
                        BinOp::Sub => e.asm.asm(&format!("sub  {work}, {rhs}")),
                        // `imul` has no two-operand immediate form.
                        BinOp::Mul if immediate => {
                            e.asm.asm(&format!("imul {work}, {work}, {rhs}"))
                        }
                        BinOp::Mul => e.asm.asm(&format!("imul {work}, {rhs}")),
                        BinOp::Div | BinOp::Rem => unreachable!("handled above"),
                    }
                    // `add`, `sub` and `imul` all set the overflow flag on a
                    // result that does not fit, and set it on nothing else. One
                    // never-taken branch is what keeps a wrong answer from being
                    // handed on as if it were right.
                    e.asm.asm(&format!("jo   {ABORT_OVERFLOW}"));
                });
            }
            // The address of this function's own room, which the prologue has
            // already reserved. One `lea`, and never a call.
            Instr::Frame { dst, offset } => {
                let at = self.frame.array_offset(*offset);
                self.produce(*dst, |e, work| e.asm.asm(&format!("lea  {work}, [rsp+{at}]")));
            }
            // A field's place was settled at compile time, so this is a `lea`
            // and nothing else — no check, because there is no question.
            Instr::Field { dst, base, offset } => {
                let base = self.in_register(base, SCRATCH0);
                self.produce(*dst, |e, work| {
                    e.asm.asm(&format!("lea  {work}, [{base}+{offset}]"));
                });
            }
            // `base + index * 8` is an addressing mode, so an element's address
            // is one instruction and not arithmetic — which is also why it
            // carries none of the overflow guards `Bin` does.
            Instr::Elem { dst, base, index, len, scale } => {
                let base = self.in_register(base, SCRATCH0);
                if let (Value::Const(at), Value::Const(_)) = (index, len) {
                    // `sema` proved this one is in range, so it needs no check
                    // and its offset folds into the addressing mode. Only an
                    // array can reach here: a string's length is not known
                    // until the program runs, so an index into one is checked
                    // however plainly it is written.
                    self.produce(*dst, |e, work| {
                        e.asm.asm(&format!("lea  {work}, [{base}+{}]", at * i64::from(*scale)));
                    });
                    return;
                }

                let index = self.in_register(index, SCRATCH1);
                // One *unsigned* comparison rules out both ends: a negative
                // index read as unsigned is enormous, so it fails the same
                // test that catches one past the end.
                let len = self.value(len);
                self.asm.asm(&format!("cmp  {index}, {len}"));
                self.asm.asm(&format!("jae  {ABORT_BOUNDS}"));

                // `lea` reads its operands before it writes, so the result may
                // land on the base's own register.
                self.produce(*dst, |e, work| {
                    if matches!(scale, 1 | 2 | 4 | 8) {
                        e.asm.asm(&format!("lea  {work}, [{base}+{index}*{scale}]"));
                    } else {
                        // An array of objects scales by the hierarchy's room,
                        // which is the one width x86's addressing modes cannot
                        // express.
                        //
                        // The product goes to a scratch register and never to
                        // the index's own: that one may be a variable's home,
                        // and a loop counter multiplied by the element size is
                        // no longer a loop counter.
                        e.asm.asm(&format!("imul {SCRATCH1}, {index}, {scale}"));
                        e.asm.asm(&format!("lea  {work}, [{base}+{SCRATCH1}]"));
                    }
                });
            }
            // Eight bytes at a time, unrolled: the count is a multiple of eight
            // and known here, and an object is small.
            Instr::CopyBytes { dst, src, bytes } => {
                let source = self.in_register(src, SCRATCH1);
                let target = self.in_register(dst, SCRATCH0);
                for at in (0..*bytes).step_by(8) {
                    self.asm.asm(&format!("mov  {RAX}, [{source}+{at}]"));
                    self.asm.asm(&format!("mov  [{target}+{at}], {RAX}"));
                }
            }
            Instr::Load { dst, addr } => {
                let addr = self.in_register(addr, SCRATCH0);
                self.produce(*dst, |e, work| e.asm.asm(&format!("mov  {work}, [{addr}]")));
            }
            // A character is four bytes inside a string and eight everywhere
            // else, so reading one widens it. Writing the 32-bit half of a
            // register clears the top half, which is the widening: a character
            // is never negative, so there is nothing to sign extend.
            Instr::LoadChar { dst, addr } => {
                let addr = self.in_register(addr, SCRATCH0);
                self.produce(*dst, |e, work| {
                    e.asm.asm(&format!("mov  {}, [{addr}]", narrow(work)));
                });
            }
            // The count a string carries in the eight bytes before its
            // characters. One load, which is why `len` is not a routine.
            Instr::Count { dst, of } => {
                let of = self.in_register(of, SCRATCH0);
                self.produce(*dst, |e, work| {
                    e.asm.asm(&format!("mov  {work}, [{of}-{STR_HEADER}]"));
                });
            }
            Instr::Store { addr, value } => {
                let addr = self.in_register(addr, SCRATCH0);
                let (source, _) = self.operand_for_alu(value);
                let source = if is_memory(&source) {
                    self.asm.mov(SCRATCH1, &source);
                    SCRATCH1.to_string()
                } else {
                    source
                };
                self.asm.asm(&format!("mov  qword [{addr}], {source}"));
            }
            // The address of a class's method table, which an object carries at
            // offset 0 and a virtual call reads back.
            Instr::VTable { dst, class } => self.produce(*dst, |e, work| {
                e.asm.asm(&format!("lea  {work}, [{}]", vtable_label(*class)));
            }),
            // The one value a variant that carries nothing has, written down
            // once in `.data` rather than allocated afresh every time.
            Instr::VariantAddr { dst, id, tag } => self.produce(*dst, |e, work| {
                e.asm.asm(&format!("lea  {work}, [{}]", variant_value(*id, *tag as usize)));
            }),
            // No source of these moves can be an argument register, because the
            // allocator is never given one — see the module docs.
            Instr::Call { dst, callee, args } => {
                self.pass(args);
                let callee = symbol(&self.program.function(*callee).name);
                self.asm.asm(&format!("call {callee}"));
                self.take_result(*dst);
            }
            // The receiver is argument zero *and* where the target comes from,
            // so its vtable is read before the argument registers are set up —
            // after that its own register may already have been overwritten.
            Instr::CallVirtual { dst, slot, receiver, args } => {
                let receiver = self.value(receiver);
                self.asm.mov(SCRATCH0, &receiver);
                self.asm.asm(&format!("mov  {SCRATCH0}, [{SCRATCH0}]"));
                self.pass(args);
                self.asm.asm(&format!("call [{SCRATCH0}+{}]", slot * 8));
                self.take_result(*dst);
            }
            // The same call an ordinary one is, with a name only this backend
            // can produce. Nothing about the sequence differs, which is the
            // point: a routine of the compiler's own is not a special form.
            Instr::RtCall { dst, callee, args } => {
                self.pass(args);
                self.asm.asm(&format!("call {}", runtime_symbol(*callee)));
                self.take_result(*dst);
            }
            // A copy has just been made, and the objects in it still name the
            // original's elements. What has to be done to one is the *object's*
            // business rather than this instruction's, so the run is handed to
            // the dispatcher and it reads each vtable in turn.
            Instr::Fixup { at, count, stride } => {
                self.pass(&[*at, *count]);
                let arg2 = self.arg(2);
                self.asm.asm(&format!("mov  {arg2}, {stride}"));
                self.asm.asm(&format!("call {FIXUP}"));
            }
            // Bytes that are already in the file: one call, no arena, no
            // encoder. How many there are was counted while the program was
            // compiled, so the write needs no terminator and the text may hold
            // anything a TinyC literal can — a `\0` included.
            Instr::PrintText { id } => {
                let bytes = self.program.texts[id.0 as usize].len();
                self.asm
                    .asm(&format!("lea  {}, [{}]", self.arg(0), text_label(id.0 as usize)));
                self.asm.asm(&format!("mov  {}, {bytes}", self.arg(1)));
                self.asm.asm(&format!("call {WRITE_TEXT}"));
            }
            // A string and a character are written by a routine rather than by
            // `printf`, because what they hold has to be encoded first — and
            // because what they hold may include the one byte a C string uses
            // to mean "no more". Both end in the write that takes a count.
            Instr::Print { ty: ty @ (Ty::Str | Ty::Char), val, newline } => {
                let value = self.value(val);
                let arg0 = self.arg(0);
                self.asm.mov(arg0, &value);
                let routine = match ty {
                    Ty::Str => PRINT_STR,
                    _ => PRINT_CHAR,
                };
                self.asm.asm(&format!("call {routine}"));
                // These two go out through a routine rather than a format of
                // their own, so ending the line is still a call — one byte,
                // written the same way as every other.
                if *newline {
                    let (arg0, arg1) = (self.arg(0), self.arg(1));
                    self.asm.asm(&format!("lea  {arg0}, [{NEWLINE}]"));
                    self.asm.asm(&format!("mov  {arg1}, 1"));
                    self.asm.asm(&format!("call {WRITE_TEXT}"));
                }
            }
            Instr::Print { ty, val, newline } => {
                // Load the value first: the format string is a constant, so it
                // can never be clobbered by the argument move.
                let value = self.value(val);
                let (arg0, arg1) = (self.arg(0), self.arg(1));
                match ty {
                    Ty::Bool => {
                        self.asm.mov(SCRATCH0, &value);
                        self.asm.asm(&format!("lea  {arg1}, [{BOOL_FALSE}]"));
                        self.asm.asm(&format!("lea  {SCRATCH1}, [{BOOL_TRUE}]"));
                        self.asm.asm(&format!("test {SCRATCH0}, {SCRATCH0}"));
                        self.asm.asm(&format!("cmovnz {arg1}, {SCRATCH1}"));
                    }
                    // The same lookup one step further: a bool picks between
                    // two names, an enum indexes a table of however many it has.
                    // The tag came from a variant of this enum and nothing else
                    // can produce one, so the index cannot be out of range.
                    Ty::Enum(id) => {
                        self.asm.mov(SCRATCH0, &value);
                        self.asm.asm(&format!("lea  {SCRATCH1}, [{}]", enum_table(*id)));
                        self.asm.asm(&format!("mov  {arg1}, [{SCRATCH1}+{SCRATCH0}*8]"));
                    }
                    // A double travels to a variadic callee in a vector
                    // register, and on Windows in the integer one for that
                    // position as well — see `Abi::vector_arg`, which is where
                    // the two conventions' one disagreement about this is
                    // written down.
                    Ty::Float => {
                        self.asm.mov(SCRATCH0, &value);
                        self.asm.asm(&format!("movq {}, {SCRATCH0}", self.abi.vector_arg));
                        if self.abi.vector_arg_shadowed {
                            self.asm.mov(arg1, SCRATCH0);
                        }
                    }
                    _ => self.asm.mov(arg1, &value),
                }
                let format = match ty {
                    Ty::Int => FMT_INT,
                    Ty::Float => FMT_FLOAT,
                    Ty::Bool => FMT_BOOL,
                    // A string and a character left through the arm above.
                    _ => FMT_STR,
                };
                // The last value a `println` writes ends the line itself, which
                // is one call rather than two.
                let format = match newline {
                    true => line_format(format),
                    false => format.to_string(),
                };
                self.asm.asm(&format!("lea  {arg0}, [{format}]"));
                self.asm.variadic(self.abi, usize::from(*ty == Ty::Float));
                self.asm.asm("call printf");
            }
        }
    }

    /// Put a call's arguments in the registers this platform passes them in.
    ///
    /// The moves may be emitted in any order because no *source* can be an
    /// argument register: the allocator is never given one. See the module
    /// docs, where that is the whole reason the pool is only callee-saved.
    fn pass(&mut self, args: &[Value]) {
        for (index, arg) in args.iter().enumerate() {
            let src = self.value(arg);
            let dst = self.arg(index);
            self.asm.mov(dst, &src);
        }
    }

    /// Put a value in a vector register, which is where a float has to be for
    /// anything to be done to it.
    ///
    /// The value is a machine word wherever it lives — see [`Num`] — so this is
    /// one `movq` out of a general register or a spill slot. An immediate is
    /// the one case that takes two: no `movq` takes one, so the bits go through
    /// [`SCRATCH1`] first.
    ///
    /// `SCRATCH1` and not `SCRATCH0`, because a caller may already be holding a
    /// result there — [`Self::work_reg`] hands out `SCRATCH0` for a destination
    /// that was spilled.
    fn load_vector(&mut self, value: &Value, vector: &str) {
        let operand = match value {
            Value::Const(c) => {
                self.asm.asm(&format!("mov  {SCRATCH1}, {c}"));
                SCRATCH1.to_string()
            }
            other => self.value(other),
        };
        self.asm.asm(&format!("movq {vector}, {operand}"));
    }

    /// `float(n)` and `int(f)`: the one place a machine word changes what it
    /// means rather than what it holds.
    ///
    /// `cvttsd2si` truncates toward zero — the second `t` — which is the
    /// direction TinyC promises, and it is also the direction that makes the
    /// range asymmetric: everything from `-2^63` up to but not including `2^63`
    /// has an answer.
    ///
    /// For anything else the machine answers the "integer indefinite" value,
    /// which is `i64::MIN` — and `i64::MIN` is also the right answer for the
    /// one float that really is `-2^63`. So the guard cannot read the answer
    /// alone: it asks whether the *source* was that number, and stops the
    /// program when it was not. A NaN lands here too, and fails the question
    /// twice over: `ucomisd` reports it as unordered, which sets the parity
    /// flag as well as leaving the equality unmet.
    fn cast(&mut self, dst: VReg, to: Num, src: &Value) {
        match to {
            // The source is an ordinary integer, so it goes straight in: this
            // is the one instruction here that reads a general register.
            Num::Float => {
                let src = self.in_register(src, SCRATCH1);
                self.asm.asm(&format!("cvtsi2sd {XMM0}, {src}"));
                self.produce(dst, |e, work| e.asm.asm(&format!("movq {work}, {XMM0}")));
            }
            Num::Int => {
                self.load_vector(src, XMM0);
                let past = self.local_label("cast");
                self.asm.asm(&format!("cvttsd2si {SCRATCH1}, {XMM0}"));
                self.asm.asm(&format!("mov  {SCRATCH0}, {}", i64::MIN));
                self.asm.asm(&format!("cmp  {SCRATCH1}, {SCRATCH0}"));
                self.asm.asm(&format!("jne  .{past}"));
                // `-2^63` as a double, which is the only source this answer may
                // legitimately have come from.
                let bound = (-9223372036854775808.0f64).to_bits() as i64;
                self.asm.asm(&format!("mov  {SCRATCH0}, {bound}"));
                self.asm.asm(&format!("movq {XMM1}, {SCRATCH0}"));
                self.asm.asm(&format!("ucomisd {XMM0}, {XMM1}"));
                self.asm.asm(&format!("jp   {ABORT_NO_INT}"));
                self.asm.asm(&format!("jne  {ABORT_NO_INT}"));
                self.asm.line(&format!(".{past}:"));
                self.produce(dst, |e, work| e.asm.mov(work, SCRATCH1));
            }
        }
    }

    /// Compare two values, leaving the answer in the flags.
    ///
    /// `cmp` refuses an immediate on the left and memory on both sides; those
    /// are the only two cases that need the scratch register.
    fn compare(&mut self, lhs: &Value, rhs: &Value) {
        let mut left = self.value(lhs);
        let (right, _) = self.operand_for_alu(rhs);
        if matches!(lhs, Value::Const(_)) || (is_memory(&left) && is_memory(&right)) {
            self.asm.asm(&format!("mov  {SCRATCH0}, {left}"));
            left = SCRATCH0.to_string();
        }
        self.asm.asm(&format!("cmp  {left}, {right}"));
    }

    /// `idiv` is fixed to `rdx:rax`, so the dividend is moved into `rax`, sign
    /// extended with `cqo`, and the answer moved back out.
    ///
    /// One instruction produces both answers: the quotient in `rax` and the
    /// remainder in `rdx`. `/` and `%` therefore differ by nothing but which
    /// register `result` names, which is why they share this routine — and why
    /// `%` inherits the guards below unchanged. It needs them: `i64::MIN % -1`
    /// is 0 mathematically, but the machine still reaches it through the `idiv`
    /// whose *quotient* does not fit.
    /// A divisor this stage can see to be zero never arrives: `sema` evaluates
    /// constant arithmetic and rejects the program first, so what is guarded
    /// here is only ever a divisor the running program alone knows.
    fn division(&mut self, dst: VReg, result: &str, lhs: &Value, rhs: &Value) {
        let guards = DivGuards::of(lhs, rhs);

        let dividend = self.value(lhs);
        self.asm.mov(RAX, &dividend);

        // `idiv` takes no immediate, so the divisor is always materialised —
        // which is also what makes each guard below a single instruction.
        let divisor = self.value(rhs);
        self.asm.mov(SCRATCH1, &divisor);

        if guards.zero {
            self.asm.asm(&format!("test {SCRATCH1}, {SCRATCH1}"));
            self.asm.asm(&format!("jz   {ABORT_DIV_ZERO}"));
        }
        if guards.overflow {
            let past = self.local_label("div");
            self.asm.asm(&format!("cmp  {SCRATCH1}, -1"));
            self.asm.asm(&format!("jne  .{past}"));
            // `cmp` takes no 64-bit immediate, so the bound goes through a
            // register.
            self.asm.asm(&format!("mov  {SCRATCH0}, {}", i64::MIN));
            self.asm.asm(&format!("cmp  {RAX}, {SCRATCH0}"));
            self.asm.asm(&format!("je   {ABORT_DIV_OVERFLOW}"));
            self.asm.line(&format!(".{past}:"));
        }

        self.asm.asm("cqo");
        self.asm.asm(&format!("idiv {SCRATCH1}"));

        // The divisor has already been read, so the answer can go straight into
        // the destination even if the two share a register.
        self.produce(dst, |e, work| e.asm.mov(work, result));
    }

    // -- operands ----------------------------------------------------------

    /// Where a virtual register lives, as an assembly operand.
    fn location(&self, vreg: VReg) -> String {
        match self.allocation.location(vreg) {
            Location::Reg(reg) => self.registers.name(reg).to_string(),
            Location::Spill(slot) => format!("qword [rsp+{}]", self.frame.slot_offset(slot)),
        }
    }

    fn value(&self, value: &Value) -> String {
        match value {
            Value::Const(c) => c.to_string(),
            Value::Reg(reg) => self.location(*reg),
        }
    }

    /// Like [`Self::value`], but an immediate too wide for an ALU instruction is
    /// materialised in a scratch register first. Returns whether the resulting
    /// operand is still an immediate.
    fn operand_for_alu(&mut self, value: &Value) -> (String, bool) {
        match value {
            Value::Const(c) if i32::try_from(*c).is_err() => {
                self.asm.asm(&format!("mov  {SCRATCH1}, {c}"));
                (SCRATCH1.to_string(), false)
            }
            Value::Const(c) => (c.to_string(), true),
            other => (self.value(other), false),
        }
    }

    /// Move a call's answer out of `rax`, when the caller wanted one.
    fn take_result(&mut self, dst: Option<VReg>) {
        if let Some(dst) = dst {
            self.produce(dst, |emitter, work| emitter.asm.mov(work, RAX));
        }
    }

    /// Compute `dst`'s value with `emit`, then put it where the allocator
    /// decided `dst` lives.
    ///
    /// Every instruction that produces a value ends this way, and the two
    /// halves have to stay together: a [`Self::work_reg`] without its
    /// [`Self::store_back`] leaves a spilled result in a scratch register and
    /// drops it, which is a wrong number rather than a crash. Going through one
    /// place is what makes leaving the second half out impossible.
    ///
    /// Whatever `emit` needs to read is worked out *before* the call, because
    /// the working register may be an operand's own — see the arms that take an
    /// operand into a scratch register first.
    fn produce(&mut self, dst: VReg, emit: impl FnOnce(&mut Self, &str)) {
        self.produce_into(dst, false, emit);
    }

    /// [`Self::produce`] for the one instruction that may need its result kept
    /// *off* its destination's own register: see the `Bin` arm, where writing
    /// the result would destroy an operand still to be read.
    fn produce_into(&mut self, dst: VReg, force_scratch: bool, emit: impl FnOnce(&mut Self, &str)) {
        let work = self.work_reg(dst, force_scratch);
        emit(self, &work);
        self.store_back(dst, &work);
    }

    /// The value as something an addressing mode can use, which is to say a
    /// register. A spilled one or an immediate is materialised in `scratch`.
    fn in_register(&mut self, value: &Value, scratch: &str) -> String {
        let operand = self.value(value);
        if is_memory(&operand) || matches!(value, Value::Const(_)) {
            self.asm.mov(scratch, &operand);
            return scratch.to_string();
        }
        operand
    }

    /// Whether `value` lives exactly where `dst` will.
    fn shares_register(&self, dst: VReg, value: &Value) -> bool {
        matches!(
            value,
            Value::Reg(other) if self.allocation.location(*other) == self.allocation.location(dst)
        )
    }

    /// The register an instruction computes its result in: the destination's own
    /// register, or a scratch register when the destination is spilled or would
    /// clobber an operand that has not been read yet.
    fn work_reg(&self, dst: VReg, force_scratch: bool) -> String {
        match self.allocation.location(dst) {
            Location::Reg(reg) if !force_scratch => self.registers.name(reg).to_string(),
            _ => SCRATCH0.to_string(),
        }
    }

    /// Move the result out of the scratch register or into its stack slot, if
    /// it was not computed in place.
    fn store_back(&mut self, dst: VReg, work: &str) {
        match self.allocation.location(dst) {
            Location::Reg(reg) => {
                let home = self.registers.name(reg);
                if home != work {
                    self.asm.asm(&format!("mov  {home}, {work}"));
                }
            }
            Location::Spill(slot) => {
                let offset = self.frame.slot_offset(slot);
                self.asm.asm(&format!("mov  qword [rsp+{offset}], {work}"));
            }
        }
    }
}

/// Whether an operand names memory rather than a register or an immediate.
fn is_memory(operand: &str) -> bool {
    operand.starts_with("qword ")
}

/// Stack frame layout for one function.
///
/// At function entry `rsp % 16 == 8` (the call pushed a return address). Each
/// pushed register subtracts 8 more, so the frame size is chosen to bring `rsp`
/// back to a 16-byte boundary before any `call`.
///
/// A *leaf* — a function that calls nothing, not even the runtime's abort — has
/// no call to be aligned for and no callee to leave shadow space for, so it
/// reserves room for its spill slots and nothing else. Most leaves spill nothing
/// and get no frame at all.
pub struct FrameLayout {
    pub size: u32,
    /// Where the spill slots start, measured from `rsp`: above the shadow space
    /// a callee would claim, which on a platform that has none is zero.
    slots_at: u32,
    /// Where this function's arrays start, just above the spill slots.
    arrays_at: u32,
    /// How many spill slots and how many bytes of arrays are in there, kept
    /// only so the prologue can say what it reserved. A reader looking at
    /// `sub rsp, 40` wants to know which part of it is theirs.
    slots: u32,
    locals: u32,
}

impl FrameLayout {
    pub fn new(
        allocation: &Allocation,
        frame_bytes: u32,
        leaf: bool,
        shadow_space: u32,
    ) -> FrameLayout {
        let slots_at = if leaf { 0 } else { shadow_space };
        let arrays_at = slots_at + 8 * allocation.spill_slots;
        let total = arrays_at + frame_bytes;
        let slots = allocation.spill_slots;
        if leaf {
            return FrameLayout { size: total, slots_at, arrays_at, slots, locals: frame_bytes };
        }

        let mut size = total.div_ceil(16) * 16;
        if allocation.used_callee_saved.len().is_multiple_of(2) {
            size += 8;
        }
        FrameLayout { size, slots_at, arrays_at, slots, locals: frame_bytes }
    }

    /// What the reservation is made of, for the comment beside it.
    ///
    /// Built from the parts that are actually there rather than from a
    /// sentence: on a platform with no shadow space, saying "shadow space +
    /// 0 spill slots" for eight bytes of pure alignment would be three kinds
    /// of wrong at once.
    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.slots_at > 0 {
            parts.push(format!("{} bytes of shadow space", self.slots_at));
        }
        if self.slots > 0 {
            parts.push(format!("{} spill slot(s)", self.slots));
        }
        if self.locals > 0 {
            parts.push(format!("{} bytes of locals", self.locals));
        }
        // Whatever is left over is what the 16-byte boundary cost.
        let accounted = self.slots_at + 8 * self.slots + self.locals;
        if parts.is_empty() || accounted < self.size {
            parts.push("alignment".to_string());
        }
        parts.join(" + ")
    }

    /// Spill slots sit just above the shadow space.
    pub fn slot_offset(&self, slot: u32) -> u32 {
        self.slots_at + 8 * slot
    }

    /// Arrays sit above the spill slots, which is what keeps their addresses
    /// stable: nothing between them and `rsp` moves for the life of the call.
    fn array_offset(&self, offset: u32) -> u32 {
        self.arrays_at + offset
    }
}
