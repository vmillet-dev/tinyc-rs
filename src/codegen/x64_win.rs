//! x86-64 backend for Windows (NASM syntax, assembled by `nasm -f win64`).
//!
//! ## Calling convention
//!
//! Generated functions follow the Microsoft x64 ABI:
//!
//! * integer arguments go in `rcx`, `rdx`, `r8`, `r9`, and a return value comes
//!   back in `rax`;
//! * the caller reserves 32 bytes of *shadow space* for the callee;
//! * `rsp` must be 16-byte aligned at every `call`;
//! * `rax`, `rcx`, `rdx`, `r8`-`r11` are destroyed by a call, the rest survive.
//!
//! ## Register policy
//!
//! | Role | Registers |
//! |------|-----------|
//! | call arguments, `idiv` remainder (never allocated) | `rcx`, `rdx`, `r8`, `r9` |
//! | `idiv` dividend and return value (never allocated) | `rax` |
//! | scratch for spilled operands (never allocated)     | `r10`, `r11` |
//! | allocatable, callee-saved                          | `rbx`, `rsi`, `rdi`, `r12`-`r15` |
//!
//! Keeping the ABI-critical registers out of the allocator's hands is what lets
//! the allocator stay target-independent: it only ever sees the last row.
//!
//! ### Why nothing is allocatable and caller-saved
//!
//! `r8` and `r9` used to be handed out by the allocator. They are also
//! argument registers three and four, and that is a contradiction as soon as
//! calls exist: setting up `f(x, y, z)` writes `r8`, which may be exactly where
//! `z` is still waiting to be read. Solving that in general is the *parallel
//! move* problem — you have to order the moves, and break cycles with a
//! temporary.
//!
//! Withdrawing `r8` and `r9` from the pool sidesteps it entirely. Every value
//! the allocator hands out now lives in a callee-saved register or a spill
//! slot, so no source of an argument move can ever be an argument register, and
//! the moves can be emitted in any order. The cost is a `push`/`pop` pair in the
//! prologue for each register used, which is a good trade for a compiler this
//! size.
//!
//! ## Symbol names
//!
//! Every TinyC function is emitted as `tc$name`. Without that, `fn printf()`
//! would define a label the `print` statement's own `call printf` then reaches
//! — a program that compiles, links, runs, and silently does the wrong thing —
//! and `fn str0()` would collide with a string literal's label outright.
//! A `$` is a valid character in a NASM identifier and is not one TinyC's lexer
//! will ever produce, so the two namespaces cannot meet.
//!
//! `main` is the exception, and has to be: it is the name the C runtime startup
//! calls. Nothing this module generates is called `main`, so it is safe to leave
//! alone.

use crate::ast::{BinOp, CmpOp, Ty};
use crate::codegen::{Allocation, Backend, Location, PhysReg, RegisterFile};
use crate::ir::{Block, Function, Instr, Program, Terminator, VReg, Value};

/// Register numbers follow the usual x86-64 encoding order.
const NAMES: [&str; 16] = [
    "rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", //
    "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15",
];

/// The 32-bit half of each register in [`NAMES`], at the same index. Writing one
/// of these clears the whole 64-bit register, which is how a zero is produced in
/// fewer bytes than an immediate would take.
const NAMES32: [&str; 16] = [
    "eax", "ecx", "edx", "ebx", "esp", "ebp", "esi", "edi", //
    "r8d", "r9d", "r10d", "r11d", "r12d", "r13d", "r14d", "r15d",
];

const RAX: &str = "rax";
const RCX: &str = "rcx";
const RDX: &str = "rdx";
/// Holds a spilled destination while it is being computed.
const SCRATCH0: &str = "r10";
/// Holds an immediate or divisor that cannot be an instruction operand.
const SCRATCH1: &str = "r11";
/// The low byte of [`SCRATCH1`], which is where `setcc` deposits its result.
const SCRATCH1_8: &str = "r11b";

/// Where the first four integer arguments travel, in order.
const ARG_REGS: [&str; 4] = ["rcx", "rdx", "r8", "r9"];

const FMT_INT: &str = "fmt_int";
const FMT_STR: &str = "fmt_str";
const FMT_BOOL: &str = "fmt_bool";
const BOOL_TRUE: &str = "bool_true";
const BOOL_FALSE: &str = "bool_false";

/// What every TinyC function's symbol starts with. See the module docs.
const SYMBOL_PREFIX: &str = "tc$";

/// What the compiler's own helpers start with. A TinyC identifier cannot
/// contain a `$`, so `tc$rt$…` is a name only this module can produce — even a
/// function called `rt` becomes `tc$rt`, not `tc$rt$anything`.
const RUNTIME_PREFIX: &str = "tc$rt$";

/// Where a division that cannot be performed lands.
const ABORT_DIV_ZERO: &str = "tc$rt$div_by_zero";
const ABORT_DIV_OVERFLOW: &str = "tc$rt$div_overflow";
const ABORT_REPORT: &str = "tc$rt$abort";
const MSG_DIV_ZERO: &str = "tc$rt$msg_div_zero";
const MSG_DIV_OVERFLOW: &str = "tc$rt$msg_div_overflow";

const DIV_ZERO_TEXT: &str = "runtime error: division by zero\n";
const DIV_OVERFLOW_TEXT: &str = "runtime error: division overflows an int\n";

/// The file descriptor `_write` should report a runtime failure on.
const STDERR: u32 = 2;

/// Bytes of shadow space every caller must reserve on Windows.
const SHADOW_SPACE: u32 = 32;

/// The entry point, which returns 0 to the C runtime rather than a value of
/// its own.
const ENTRY_POINT: &str = "main";

/// The assembly name of a TinyC function.
pub fn symbol(name: &str) -> String {
    if name == ENTRY_POINT {
        // The C runtime startup calls this one by name.
        name.to_string()
    } else {
        format!("{SYMBOL_PREFIX}{name}")
    }
}

/// Whether an emitted symbol is one of the compiler's own helpers rather than a
/// function the program declared.
pub fn is_runtime_symbol(name: &str) -> bool {
    name.starts_with(RUNTIME_PREFIX)
}

/// The 32-bit half of a 64-bit register this backend named.
fn half(name: &str) -> &'static str {
    let index = NAMES.iter().position(|&n| n == name).expect("a register from this backend");
    NAMES32[index]
}

/// The jump that leaves when a comparison is *false*, which is the direction a
/// branch to the `else` block needs.
fn jump_if_false(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "jne",
        CmpOp::Ne => "je",
        CmpOp::Lt => "jge",
        CmpOp::Le => "jg",
        CmpOp::Gt => "jle",
        CmpOp::Ge => "jl",
    }
}

/// The `setcc` variant that materialises each comparison. Signed, because
/// `int` is signed.
fn setcc(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Eq => "sete",
        CmpOp::Ne => "setne",
        CmpOp::Lt => "setl",
        CmpOp::Le => "setle",
        CmpOp::Gt => "setg",
        CmpOp::Ge => "setge",
    }
}

/// Which of `idiv`'s two faults a division still has to rule out.
///
/// `idiv` traps on a zero divisor, and also on `i64::MIN / -1`, whose quotient
/// does not fit in the register it would have to go in. Every check that a
/// literal operand already answers is one the emitted code does not carry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct DivGuards {
    zero: bool,
    overflow: bool,
}

impl DivGuards {
    fn of(lhs: &Value, rhs: &Value) -> DivGuards {
        let zero = !matches!(rhs, Value::Const(c) if *c != 0);
        let overflow = match (lhs, rhs) {
            // Only `i64::MIN` can overflow, and only when divided by -1.
            (Value::Const(dividend), _) if *dividend != i64::MIN => false,
            (_, Value::Const(divisor)) => *divisor == -1,
            _ => true,
        };
        DivGuards { zero, overflow }
    }

    fn any(self) -> bool {
        self.zero || self.overflow
    }
}

/// Whether an instruction may jump to a runtime abort, and so needs a frame it
/// can make a call from.
fn may_abort(instr: &Instr) -> bool {
    matches!(instr, Instr::Bin { op: BinOp::Div, lhs, rhs, .. } if DivGuards::of(lhs, rhs).any())
}

pub struct X64Windows {
    registers: RegisterFile,
}

impl X64Windows {
    pub fn new() -> X64Windows {
        X64Windows {
            registers: RegisterFile {
                names: NAMES.to_vec(),
                // See the module docs: the caller-saved registers that remain
                // free are all argument registers, so none is allocatable.
                caller_saved: Vec::new(),
                callee_saved: vec![
                    PhysReg(3),  // rbx
                    PhysReg(6),  // rsi
                    PhysReg(7),  // rdi
                    PhysReg(12), // r12
                    PhysReg(13), // r13
                    PhysReg(14), // r14
                    PhysReg(15), // r15
                ],
                max_args: ARG_REGS.len(),
            },
        }
    }
}

impl Default for X64Windows {
    fn default() -> Self {
        X64Windows::new()
    }
}

impl Backend for X64Windows {
    fn name(&self) -> &'static str {
        "x86_64-windows"
    }

    fn register_file(&self) -> &RegisterFile {
        &self.registers
    }

    fn emit(&self, program: &Program, allocations: &[Allocation]) -> String {
        let mut asm = Asm { out: String::new() };
        let used = Used::of(program);

        header(&mut asm, self.name(), &used);
        data_section(&mut asm, program, &used);
        asm.line("section .text");

        for (function, allocation) in program.functions.iter().zip(allocations) {
            FnEmitter::new(program, function, allocation, &self.registers, &mut asm).run();
        }

        if used.aborts {
            abort_stubs(&mut asm);
        }
        asm.out
    }
}

/// What the program actually needs out of the runtime, so nothing unused is
/// declared or emitted. Answered in one sweep rather than one per question.
struct Used {
    formats: [bool; 3],
    aborts: bool,
}

impl Used {
    fn of(program: &Program) -> Used {
        let mut used = Used { formats: [false; 3], aborts: false };
        for instr in program.functions.iter().flat_map(|f| &f.blocks).flat_map(|b| &b.instrs) {
            match instr {
                Instr::Print { ty, .. } => used.formats[format_index(*ty)] = true,
                other => used.aborts |= may_abort(other),
            }
        }
        used
    }

    fn prints(&self, ty: Ty) -> bool {
        self.formats[format_index(ty)]
    }
}

fn format_index(ty: Ty) -> usize {
    match ty {
        Ty::Int => 0,
        Ty::Str => 1,
        Ty::Bool => 2,
    }
}

// -- output ----------------------------------------------------------------

/// The assembly listing as it is written.
struct Asm {
    out: String,
}

impl Asm {
    fn line(&mut self, text: &str) {
        self.out.push_str(text);
        self.out.push('\n');
    }

    fn blank(&mut self) {
        self.out.push('\n');
    }

    fn comment(&mut self, text: &str) {
        self.line(&format!("    ; {text}"));
    }

    fn asm(&mut self, text: &str) {
        self.line(&format!("    {text}"));
    }

    /// `mov` that drops the no-op a reused register often produces.
    fn mov(&mut self, dst: &str, src: &str) {
        if dst != src {
            self.asm(&format!("mov  {dst}, {src}"));
        }
    }
}

// -- sections --------------------------------------------------------------

fn header(asm: &mut Asm, target: &str, used: &Used) {
    let rule = format!("; {}", "-".repeat(68));

    asm.line(&rule);
    asm.line(&format!("; Generated by tinyc for {target} (NASM syntax)"));
    asm.line(&rule);
    // Without `default rel`, `lea rcx, [label]` would use a 32-bit absolute
    // address instead of the RIP-relative form 64-bit code wants.
    asm.line("default rel");
    asm.blank();
    asm.line("extern printf");
    if used.aborts {
        // Reporting a runtime failure needs a stream the compiler can write to
        // without a `FILE*`, and a way out that skips the rest of the program.
        asm.line("extern _write");
        asm.line("extern exit");
    }
    asm.line(&format!("global {ENTRY_POINT}"));
    asm.blank();
}

fn data_section(asm: &mut Asm, program: &Program, used: &Used) {
    asm.line("section .data");
    if used.prints(Ty::Int) {
        asm.asm(&format!("{FMT_INT}: db \"%lld\", 10, 0"));
    }
    if used.prints(Ty::Str) {
        asm.asm(&format!("{FMT_STR}: db \"%s\", 10, 0"));
    }
    if used.prints(Ty::Bool) {
        asm.asm(&format!("{FMT_BOOL}: db \"%s\", 10, 0"));
        asm.asm(&format!("{BOOL_TRUE}: db \"true\", 0"));
        asm.asm(&format!("{BOOL_FALSE}: db \"false\", 0"));
    }
    if used.aborts {
        // No NUL: `_write` is given a length, not a C string.
        asm.asm(&format!("{MSG_DIV_ZERO}: db {}", bytes_of(DIV_ZERO_TEXT)));
        asm.asm(&format!("{MSG_DIV_OVERFLOW}: db {}", bytes_of(DIV_OVERFLOW_TEXT)));
    }
    for (index, bytes) in program.strings.iter().enumerate() {
        // Emitting raw bytes avoids every string-quoting corner case.
        let values: Vec<String> = bytes
            .iter()
            .map(|b| b.to_string())
            .chain(std::iter::once("0".to_string()))
            .collect();
        let text = String::from_utf8_lossy(bytes).escape_debug().to_string();
        asm.asm(&format!("str{index}: db {}    ; \"{text}\"", values.join(", ")));
    }
    asm.blank();
}

fn bytes_of(text: &str) -> String {
    text.bytes().map(|b| b.to_string()).collect::<Vec<_>>().join(", ")
}

/// The out-of-line ends every failing division jumps to.
///
/// They are reached by `jmp`, not `call`, so the frame in place is the one the
/// dividing function set up — which is why a function that can abort is never
/// treated as a leaf.
fn abort_stubs(asm: &mut Asm) {
    asm.blank();
    asm.comment("runtime failures: report on stderr, then leave with a non-zero status");

    asm.line(&format!("{ABORT_DIV_ZERO}:"));
    asm.asm(&format!("lea  {RDX}, [{MSG_DIV_ZERO}]"));
    asm.asm(&format!("mov  r8d, {}", DIV_ZERO_TEXT.len()));
    asm.asm(&format!("jmp  {ABORT_REPORT}"));

    asm.line(&format!("{ABORT_DIV_OVERFLOW}:"));
    asm.asm(&format!("lea  {RDX}, [{MSG_DIV_OVERFLOW}]"));
    asm.asm(&format!("mov  r8d, {}", DIV_OVERFLOW_TEXT.len()));

    asm.line(&format!("{ABORT_REPORT}:"));
    asm.comment("_write(2, message, length), then exit(1)");
    asm.asm(&format!("mov  {RCX}, {STDERR}"));
    asm.asm("call _write");
    asm.asm(&format!("mov  {RCX}, 1"));
    asm.asm("call exit");
}

// -- one function ----------------------------------------------------------

struct FnEmitter<'a, 'o> {
    program: &'a Program,
    function: &'a Function,
    allocation: &'a Allocation,
    registers: &'a RegisterFile,
    frame: FrameLayout,
    asm: &'o mut Asm,
    /// Serial number for the local labels a division guard needs.
    next_label: u32,
}

impl<'a, 'o> FnEmitter<'a, 'o> {
    fn new(
        program: &'a Program,
        function: &'a Function,
        allocation: &'a Allocation,
        registers: &'a RegisterFile,
        asm: &'o mut Asm,
    ) -> FnEmitter<'a, 'o> {
        // A leaf makes no call — not to another function, and not to the
        // runtime's abort — so it owes neither shadow space nor alignment.
        let leaf = !function
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .any(|instr| instr.is_call() || may_abort(instr));

        let frame = FrameLayout::new(allocation, leaf);
        FnEmitter { program, function, allocation, registers, frame, asm, next_label: 0 }
    }

    fn run(&mut self) {
        self.asm.blank();
        for line in self.allocation.dump(self.function, self.registers).lines() {
            self.asm.comment(line);
        }
        self.asm.line(&format!("{}:", symbol(&self.function.name)));
        self.prologue();

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
        if self.frame.size > 0 {
            let slots = self.allocation.spill_slots;
            let what = if self.frame.slots_at == 0 {
                // A leaf: no callee to leave shadow space for, and no call to
                // align the stack ahead of.
                format!("{slots} spill slot(s)")
            } else {
                format!("{SHADOW_SPACE} bytes of shadow space + {slots} spill slot(s) + alignment")
            };
            self.asm.asm(&format!("sub  rsp, {}    ; {what}", self.frame.size));
        }
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
    fn fusable_compare(&self, block: &Block) -> Option<usize> {
        let Terminator::Branch { cond: Value::Reg(cond), .. } = &block.term else { return None };
        let at = block.instrs.len().checked_sub(1)?;
        let Instr::Cmp { dst, .. } = &block.instrs[at] else { return None };
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
                    let (taken, label) = if *c != 0 {
                        (then_blk, &then_label)
                    } else {
                        (else_blk, &else_label)
                    };
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
            Instr::Const { dst, val } => {
                let work = self.work_reg(*dst, false);
                if *val == 0 {
                    // Clearing the 32-bit half zeroes the whole register, and
                    // costs three bytes fewer than an immediate zero.
                    self.asm.asm(&format!("xor  {0}, {0}", half(&work)));
                } else {
                    self.asm.asm(&format!("mov  {work}, {val}"));
                }
                self.store_back(*dst, &work);
            }
            Instr::StrAddr { dst, id } => {
                let work = self.work_reg(*dst, false);
                self.asm.asm(&format!("lea  {work}, [str{}]", id.0));
                self.store_back(*dst, &work);
            }
            Instr::Copy { dst, src } => {
                let src = self.value(src);
                let work = self.work_reg(*dst, false);
                self.asm.mov(&work, &src);
                self.store_back(*dst, &work);
            }
            // The argument is already sitting in its ABI register; this just
            // moves it to wherever the allocator decided it should live.
            Instr::Param { dst, index } => {
                let src = ARG_REGS[*index as usize];
                let work = self.work_reg(*dst, false);
                self.asm.mov(&work, src);
                self.store_back(*dst, &work);
            }
            // `cmp` sets the flags; `setcc` turns the one that matters into a
            // 0 or 1 byte, and `movzx` widens it to the 64-bit value a bool is.
            // A comparison that only feeds a branch never gets here — see
            // `fusable_compare`.
            Instr::Cmp { op, dst, lhs, rhs } => {
                self.compare(lhs, rhs);
                self.asm.asm(&format!("{} {SCRATCH1_8}", setcc(*op)));

                let work = self.work_reg(*dst, false);
                self.asm.asm(&format!("movzx {work}, {SCRATCH1_8}"));
                self.store_back(*dst, &work);
            }
            Instr::Bin { op: BinOp::Div, dst, lhs, rhs } => self.division(*dst, lhs, rhs),
            Instr::Bin { op, dst, lhs, rhs } => {
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

                let work = self.work_reg(*dst, clobbers_rhs);
                let lhs = self.value(lhs);
                self.asm.mov(&work, &lhs);

                let (rhs, immediate) = self.operand_for_alu(rhs);
                match op {
                    BinOp::Add => self.asm.asm(&format!("add  {work}, {rhs}")),
                    BinOp::Sub => self.asm.asm(&format!("sub  {work}, {rhs}")),
                    // `imul` has no two-operand immediate form.
                    BinOp::Mul if immediate => {
                        self.asm.asm(&format!("imul {work}, {work}, {rhs}"))
                    }
                    BinOp::Mul => self.asm.asm(&format!("imul {work}, {rhs}")),
                    BinOp::Div => unreachable!("handled above"),
                }
                self.store_back(*dst, &work);
            }
            // No source of these moves can be an argument register, because the
            // allocator is never given one — see the module docs.
            Instr::Call { dst, callee, args } => {
                for (index, arg) in args.iter().enumerate() {
                    let src = self.value(arg);
                    self.asm.mov(ARG_REGS[index], &src);
                }
                let callee = symbol(&self.program.function(*callee).name);
                self.asm.asm(&format!("call {callee}"));
                if let Some(dst) = dst {
                    let work = self.work_reg(*dst, false);
                    self.asm.mov(&work, RAX);
                    self.store_back(*dst, &work);
                }
            }
            Instr::Print { ty, val } => {
                // Load the value first: the format string is a constant, so it
                // can never be clobbered by the argument move.
                let value = self.value(val);
                match ty {
                    Ty::Bool => {
                        self.asm.mov(SCRATCH0, &value);
                        self.asm.asm(&format!("lea  {RDX}, [{BOOL_FALSE}]"));
                        self.asm.asm(&format!("lea  {SCRATCH1}, [{BOOL_TRUE}]"));
                        self.asm.asm(&format!("test {SCRATCH0}, {SCRATCH0}"));
                        self.asm.asm(&format!("cmovnz {RDX}, {SCRATCH1}"));
                    }
                    _ => self.asm.mov(RDX, &value),
                }
                let format = match ty {
                    Ty::Int => FMT_INT,
                    Ty::Str => FMT_STR,
                    Ty::Bool => FMT_BOOL,
                };
                self.asm.asm(&format!("lea  {RCX}, [{format}]"));
                self.asm.asm("call printf");
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
    /// extended with `cqo`, and the quotient moved back out.
    ///
    /// It also faults rather than producing a wrong answer, on a zero divisor
    /// and on `i64::MIN / -1`. Both are ruled out first, unless a literal
    /// operand already rules them out for free.
    fn division(&mut self, dst: VReg, lhs: &Value, rhs: &Value) {
        let guards = DivGuards::of(lhs, rhs);

        // A literal zero divisor can do nothing but fault.
        if matches!(rhs, Value::Const(0)) {
            self.asm.asm(&format!("jmp  {ABORT_DIV_ZERO}"));
            return;
        }

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

        // The divisor has already been read, so the quotient can go straight
        // into the destination even if the two share a register.
        let work = self.work_reg(dst, false);
        self.asm.mov(&work, RAX);
        self.store_back(dst, &work);
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
struct FrameLayout {
    size: u32,
    /// Where the spill slots start, measured from `rsp`.
    slots_at: u32,
}

impl FrameLayout {
    fn new(allocation: &Allocation, leaf: bool) -> FrameLayout {
        let slots_at = if leaf { 0 } else { SHADOW_SPACE };
        let locals = slots_at + 8 * allocation.spill_slots;
        if leaf {
            return FrameLayout { size: locals, slots_at };
        }

        let mut size = locals.div_ceil(16) * 16;
        if allocation.used_callee_saved.len().is_multiple_of(2) {
            size += 8;
        }
        FrameLayout { size, slots_at }
    }

    /// Spill slots sit just above the shadow space.
    fn slot_offset(&self, slot: u32) -> u32 {
        self.slots_at + 8 * slot
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::regalloc;
    use crate::{lexer, parser, sema};

    fn compile_src(src: &str) -> String {
        let ast = parser::parse(&lexer::lex(src).unwrap()).unwrap();
        let types = sema::check(&ast, 4).unwrap();
        let ir = crate::ir::lower(&ast, &types);
        let backend = X64Windows::new();
        let allocations: Vec<Allocation> =
            ir.functions.iter().map(|f| regalloc::allocate(f, backend.register_file())).collect();
        backend.emit(&ir, &allocations)
    }

    /// Compile a `main` body; most of these tests are about one function.
    fn compile(body: &str) -> String {
        compile_src(&format!("fn main() {{\n{body}\n}}\n"))
    }

    #[test]
    fn frame_keeps_the_stack_aligned_at_calls() {
        for spill_slots in 0..4u32 {
            for pushes in 0..4usize {
                let allocation = Allocation {
                    locations: Default::default(),
                    used_callee_saved: vec![PhysReg(3); pushes],
                    spill_slots,
                    intervals: Vec::new(),
                };
                let frame = FrameLayout::new(&allocation, false);
                // 8 (return address) + 8*pushes + frame must be a multiple of 16.
                assert_eq!((8 + 8 * pushes as u32 + frame.size) % 16, 0);
                // The frame must still cover shadow space and every spill slot.
                assert!(frame.size >= SHADOW_SPACE + 8 * spill_slots);
            }
        }
    }

    #[test]
    fn a_leaf_reserves_only_what_it_spills() {
        for spill_slots in 0..4u32 {
            let allocation = Allocation {
                locations: Default::default(),
                used_callee_saved: Vec::new(),
                spill_slots,
                intervals: Vec::new(),
            };
            let frame = FrameLayout::new(&allocation, true);
            // No call to align for and no callee to leave shadow space for.
            assert_eq!(frame.size, 8 * spill_slots);
            assert_eq!(frame.slot_offset(0), 0);
        }
    }

    #[test]
    fn emits_a_call_per_print() {
        let asm = compile("int x = 1;\nprint(x);\nprint(x + 1);");
        assert_eq!(asm.matches("call printf").count(), 2);
    }

    #[test]
    fn a_value_live_across_a_call_uses_a_callee_saved_register() {
        let asm = compile("string s = \"hi\";\nprint(1 + 2);\nprint(s);");
        // `s` survives the first printf, so it must be pushed in the prologue.
        assert!(asm.contains("push rbx"), "{asm}");
        assert!(asm.contains("pop  rbx"), "{asm}");
    }

    #[test]
    fn printing_a_bool_picks_its_text_without_branching() {
        let asm = compile("bool ready = true;\nprint(ready);");
        // `false` is loaded first and overwritten only when the value is not 0.
        assert!(asm.contains("lea  rdx, [bool_false]"), "{asm}");
        assert!(asm.contains("lea  r11, [bool_true]"), "{asm}");
        assert!(asm.contains("cmovnz rdx, r11"), "{asm}");
        assert!(asm.contains("lea  rcx, [fmt_bool]"), "{asm}");
        // A conditional move, not a jump.
        assert!(!asm.contains("jmp"), "{asm}");
        assert!(!asm.contains("jnz"), "{asm}");
    }

    #[test]
    fn a_bool_literal_reaches_a_register_before_being_tested() {
        // `test` has no form that takes an immediate on both sides, so a
        // literal has to be materialised into the scratch register first.
        let asm = compile("print(true);");
        assert!(asm.contains("mov  r10, 1"), "{asm}");
        assert!(asm.contains("test r10, r10"), "{asm}");
    }

    #[test]
    fn bool_data_is_emitted_only_when_a_bool_is_printed() {
        let with_bool = compile("print(true);");
        assert!(with_bool.contains("fmt_bool: db \"%s\", 10, 0"), "{with_bool}");
        assert!(with_bool.contains("bool_true: db \"true\", 0"), "{with_bool}");
        assert!(with_bool.contains("bool_false: db \"false\", 0"), "{with_bool}");

        // A bool-free program must not carry the strings, and a bool-only
        // program must not depend on the string format it never emits.
        let without = compile("print(1 + 2);");
        assert!(!without.contains("bool_true"), "{without}");
        assert!(!without.contains("fmt_bool"), "{without}");
        assert!(!with_bool.contains("fmt_str"), "{with_bool}");
    }

    #[test]
    fn a_branch_tests_the_condition_and_jumps() {
        let asm = compile("int n = 0;\nif (n < 1) {\n  print(1);\n} else {\n  print(2);\n}");
        assert!(asm.contains(".then1:"), "{asm}");
        assert!(asm.contains(".else2:"), "{asm}");
        assert!(asm.contains(".join3:"), "{asm}");
        // The comparison and the branch are one: `cmp` sets the flags and the
        // jump reads them, with no 0 or 1 in between.
        assert!(asm.contains("cmp  rbx, 1"), "{asm}");
        assert!(asm.contains("jge  .else2"), "{asm}");
        assert!(!asm.contains("setl"), "the comparison should not be materialised: {asm}");
        // The `then` block falls through to `else` unless it jumps past it.
        assert!(asm.contains("jmp  .join3"), "{asm}");
    }

    #[test]
    fn a_condition_that_is_not_a_comparison_is_tested() {
        // Nothing to fuse here: the condition is a variable, so it has to be
        // tested for zero on its own.
        let asm = compile("bool go = 1 < 2;\nif (go) {\n  print(1);\n}");
        assert!(asm.contains("test rbx, rbx"), "{asm}");
        assert!(asm.contains("jz   .join"), "{asm}");
    }

    #[test]
    fn each_branch_picks_the_jump_that_leaves_when_the_test_fails() {
        // The jump goes to the `else` block, so it is the *negation* of the
        // comparison that has to be encoded.
        for (source, expected) in [
            ("n == 2", "jne"),
            ("n != 2", "je"),
            ("n < 2", "jge"),
            ("n <= 2", "jg"),
            ("n > 2", "jle"),
            ("n >= 2", "jl"),
        ] {
            let asm = compile(&format!("int n = 1;\nif ({source}) {{\n  print(1);\n}}"));
            assert!(asm.contains(&format!("{expected:<4} .join")), "{source}: {asm}");
        }
    }

    #[test]
    fn a_comparison_kept_as_a_value_becomes_cmp_plus_setcc() {
        // `ok` is printed rather than branched on, so the 0 or 1 really has to
        // exist.
        let asm = compile("int x = 1;\nbool ok = x < 2;\nprint(ok);");
        assert!(asm.contains("cmp  rbx, 2"), "{asm}");
        assert!(asm.contains("setl r11b"), "{asm}");
        assert!(asm.contains("movzx"), "{asm}");
    }

    #[test]
    fn each_comparison_picks_its_own_setcc() {
        for (source, expected) in [
            ("x == 2", "sete"),
            ("x != 2", "setne"),
            ("x < 2", "setl"),
            ("x <= 2", "setle"),
            ("x > 2", "setg"),
            ("x >= 2", "setge"),
        ] {
            let asm = compile(&format!("int x = 1;\nbool ok = {source};\nprint(ok);"));
            assert!(asm.contains(&format!("{expected} r11b")), "{source}: {asm}");
        }
    }

    #[test]
    fn a_comparison_between_literals_never_reaches_the_backend() {
        // Lowering folded it, so there is no comparison left to emit.
        let asm = compile("bool ok = 1 < 2;\nprint(ok);");
        assert!(!asm.contains("cmp "), "{asm}");
        assert!(!asm.contains("setl"), "{asm}");
    }

    #[test]
    fn a_zero_is_produced_by_clearing_the_register() {
        let asm = compile("int n = 0;\nprint(n);");
        assert!(asm.contains("xor  ebx, ebx"), "{asm}");
        assert!(!asm.contains("mov  rbx, 0"), "{asm}");
    }

    #[test]
    fn a_loop_body_jumps_back_to_its_header() {
        let asm = compile("int i = 0;\nwhile (i < 3) {\n  i = i + 1;\n}\nprint(i);");
        assert!(asm.contains(".loop1:"), "{asm}");
        assert!(asm.contains(".body2:"), "{asm}");
        assert!(asm.contains("jmp  .loop1"), "{asm}");
        assert!(asm.contains("jge  .done3"), "{asm}");
    }

    #[test]
    fn a_condition_that_folded_to_true_is_not_tested_at_all() {
        let asm = compile("while (true) {\n  print(1);\n}");
        assert!(!asm.contains("test"), "{asm}");
        assert!(!asm.contains("jz"), "{asm}");
        // The loop is still a loop.
        assert!(asm.contains("jmp  .loop1"), "{asm}");
    }

    #[test]
    fn a_jump_to_the_next_block_is_left_out() {
        // `entry` is followed immediately by the loop header, so the jump
        // between them is a fallthrough and must not be emitted.
        let asm = compile("int i = 0;\nwhile (i < 3) {\n  i = i + 1;\n}\nprint(i);");
        assert_eq!(asm.matches("jmp  .loop1").count(), 1, "{asm}");
    }

    #[test]
    fn strings_are_emitted_as_bytes() {
        let asm = compile("string s = \"hi\";\nprint(s);");
        assert!(asm.contains("str0: db 104, 105, 0"), "{asm}");
    }

    // -- functions ---------------------------------------------------------

    #[test]
    fn every_function_gets_a_label_a_prologue_and_an_epilogue() {
        let asm = compile_src(
            "fn add(int a, int b) -> int {\n  return a + b;\n}\nfn main() {\n  print(add(1, 2));\n}",
        );
        assert!(asm.contains("\ntc$add:\n"), "{asm}");
        assert!(asm.contains("\nmain:\n"), "{asm}");
        // One `ret` per function, and only `main` is exported.
        assert_eq!(asm.matches("\n    ret\n").count(), 2, "{asm}");
        assert!(asm.contains("global main"), "{asm}");
        assert!(!asm.contains("global tc$add"), "{asm}");
    }

    // -- aliasing between a destination and its operands -------------------

    #[test]
    fn a_destination_that_would_clobber_the_right_operand_uses_a_scratch() {
        // `x = y - x` puts the result where `x` already is, and `sub` writes its
        // destination before reading its source. Landing directly on `x` would
        // turn this into `x - x`.
        // `y` is declared first, so it gets rbx and `x` gets rsi.
        let asm = compile("int y = 3;\nint x = 10;\nx = y - x;\nprint(x);");
        assert!(asm.contains("mov  r10, rbx"), "{asm}");
        assert!(asm.contains("sub  r10, rsi"), "{asm}");
        assert!(asm.contains("mov  rsi, r10"), "{asm}");
    }

    #[test]
    fn a_commutative_operator_swaps_instead_of_taking_a_scratch() {
        // `x = y + x` has the same shape, but addition may read its operands in
        // either order, so `x` becomes the left one and no scratch is needed.
        let asm = compile("int y = 3;\nint x = 10;\nx = y + x;\nprint(x);");
        assert!(asm.contains("add  rsi, rbx"), "{asm}");
        assert!(!asm.contains("mov  r10, rbx"), "{asm}");
    }

    #[test]
    fn a_destination_landing_on_the_left_operand_needs_no_move_at_all() {
        let asm = compile("int y = 3;\nint x = 10;\nx = x - y;\nprint(x);");
        assert!(asm.contains("sub  rsi, rbx"), "{asm}");
        assert!(!asm.contains("mov  r10,"), "{asm}");
    }

    #[test]
    fn an_immediate_too_wide_for_an_operand_goes_through_a_register() {
        // ALU instructions take a 32-bit immediate at most.
        let asm = compile("int a = 1;\nprint(a + 4611686018427387904);");
        assert!(asm.contains("mov  r11, 4611686018427387904"), "{asm}");
        assert!(asm.contains("add  rbx, r11"), "{asm}");
    }

    // -- symbol names ------------------------------------------------------

    #[test]
    fn a_function_cannot_shadow_the_runtime_it_is_compiled_against() {
        // `printf` is what `print` calls. Before the names were kept apart, this
        // program defined the very symbol `print` reaches, and then quietly did
        // nothing at all.
        let asm =
            compile_src("fn printf() -> int {\n  return 1;\n}\nfn main() {\n  print(printf());\n}");
        assert!(asm.contains("\ntc$printf:\n"), "{asm}");
        assert!(!asm.contains("\nprintf:\n"), "{asm}");
        assert!(asm.contains("call tc$printf"), "{asm}");
        assert!(asm.contains("call printf"), "the print statement still reaches the CRT: {asm}");
    }

    #[test]
    fn a_function_cannot_collide_with_a_generated_data_label() {
        // These are the labels the backend itself emits; a TinyC function of the
        // same name used to redefine them and stop NASM outright.
        for name in ["str0", "fmt_int", "fmt_str", "fmt_bool", "bool_true", "bool_false"] {
            let asm = compile_src(&format!(
                "fn {name}() -> int {{\n  return 1;\n}}\n\
                 fn main() {{\n  print(\"hi\");\n  print(true);\n  print({name}());\n}}"
            ));
            assert!(asm.contains(&format!("\ntc${name}:\n")), "{name}: {asm}");
            assert!(!asm.contains(&format!("\n{name}:\n")), "{name}: {asm}");
        }
    }

    #[test]
    fn the_entry_point_keeps_the_name_the_runtime_calls() {
        let asm = compile("print(1);");
        assert!(asm.contains("\nmain:\n"), "{asm}");
        assert!(!asm.contains("tc$main"), "{asm}");
    }

    // -- runtime failures --------------------------------------------------

    #[test]
    fn a_division_by_an_unknown_value_is_guarded() {
        let asm = compile_src(
            "fn d(int a, int b) -> int {\n  return a / b;\n}\nfn main() {\n  print(d(6, 3));\n}",
        );
        assert!(asm.contains("jz   tc$rt$div_by_zero"), "{asm}");
        assert!(asm.contains("je   tc$rt$div_overflow"), "{asm}");
        assert!(asm.contains("tc$rt$div_by_zero:"), "the stub is emitted once: {asm}");
        assert!(asm.contains("call _write"), "{asm}");
        assert!(asm.contains("call exit"), "{asm}");
    }

    #[test]
    fn a_division_by_a_harmless_literal_carries_no_check() {
        // 7 is neither 0 nor -1, so `idiv` cannot fault and nothing is emitted
        // to prove it.
        let asm = compile("int n = 100;\nprint(n / 7);");
        assert!(!asm.contains("tc$rt$div"), "{asm}");
        assert!(!asm.contains("extern _write"), "{asm}");
        assert!(asm.contains("idiv"), "{asm}");
    }

    #[test]
    fn dividing_by_minus_one_only_checks_for_overflow() {
        let asm = compile("int n = 100;\nprint(n / (0 - 1));");
        assert!(asm.contains("je   tc$rt$div_overflow"), "{asm}");
        assert!(!asm.contains("jz   tc$rt$div_by_zero"), "a literal -1 is never zero: {asm}");
    }

    #[test]
    fn a_division_by_a_literal_zero_can_only_fail() {
        // Lowering refuses to fold it, so the backend has to say what happens.
        let asm = compile("int n = 1;\nprint(n / (3 - 3));");
        assert!(asm.contains("jmp  tc$rt$div_by_zero"), "{asm}");
        assert!(!asm.contains("idiv"), "there is nothing to divide: {asm}");
    }

    #[test]
    fn a_function_that_can_abort_still_gets_a_frame_to_call_from() {
        // The abort stub is jumped to, not called, so it runs on the frame of
        // whoever jumped — which therefore has to have one.
        let asm = compile_src(
            "fn d(int a, int b) -> int {\n  return a / b;\n}\nfn main() {\n  print(d(6, 3));\n}",
        );
        let (name, body) = functions_in(&asm)
            .into_iter()
            .find(|(name, _)| *name == "tc$d")
            .expect("the dividing function");
        assert!(body.contains("sub  rsp,"), "{name}: {body}");
    }

    // -- frames ------------------------------------------------------------

    #[test]
    fn a_leaf_that_spills_nothing_reserves_no_frame() {
        let asm = compile_src(
            "fn double(int n) -> int {\n  return n * 2;\n}\nfn main() {\n  print(double(21));\n}",
        );
        let (_, body) = functions_in(&asm)
            .into_iter()
            .find(|(name, _)| *name == "tc$double")
            .expect("the leaf");
        assert!(!body.contains("sub  rsp,"), "{body}");
        assert!(!body.contains("add  rsp,"), "{body}");
        assert!(body.contains("ret"), "{body}");
    }

    #[test]
    fn a_function_that_calls_still_reserves_shadow_space() {
        let asm = compile("print(1);");
        assert!(asm.contains("sub  rsp,"), "{asm}");
    }

    /// Split emitted assembly into `(name, body)` per function, the same way
    /// NASM scopes its `.labels`.
    fn functions_in(asm: &str) -> Vec<(&str, &str)> {
        let starts: Vec<(usize, &str)> = asm
            .match_indices('\n')
            .filter_map(|(offset, _)| {
                let line = asm[offset + 1..].lines().next()?;
                let name = line.strip_suffix(':')?;
                let plain = !name.is_empty()
                    && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
                    && !name.starts_with(|c: char| c.is_ascii_digit());
                plain.then_some((offset + 1, name))
            })
            .collect();

        starts
            .iter()
            .enumerate()
            .map(|(index, &(offset, name))| {
                let end = starts.get(index + 1).map_or(asm.len(), |&(next, _)| next);
                (name, &asm[offset..end])
            })
            .collect()
    }

    #[test]
    fn block_labels_are_local_so_two_functions_may_share_them() {
        // NASM scopes a `.label` to the preceding global one, so `.entry0`
        // inside `a` and inside `main` are different labels.
        let asm = compile_src("fn a() {\n  print(1);\n}\nfn main() {\n  a();\n}");
        assert_eq!(asm.matches(".entry0:").count(), 2, "{asm}");
    }

    #[test]
    fn arguments_travel_in_the_abi_registers() {
        let asm = compile_src(
            "fn f(int a, int b, int c, int d) {\n  print(a);\n}\n\
             fn main() {\n  f(1, 2, 3, 4);\n}",
        );
        assert!(asm.contains("mov  rcx, 1"), "{asm}");
        assert!(asm.contains("mov  rdx, 2"), "{asm}");
        assert!(asm.contains("mov  r8, 3"), "{asm}");
        assert!(asm.contains("mov  r9, 4"), "{asm}");
        assert!(asm.contains("call f"), "{asm}");
    }

    #[test]
    fn a_parameter_is_moved_out_of_its_argument_register_on_entry() {
        let asm = compile_src("fn f(int a) -> int {\n  return a;\n}\nfn main() {\n  f(1);\n}");
        // `a` lands in the first allocatable callee-saved register.
        assert!(asm.contains("mov  rbx, rcx"), "{asm}");
    }

    #[test]
    fn a_returned_value_comes_back_in_rax() {
        let asm = compile_src("fn one() -> int {\n  return 1;\n}\nfn main() {\n  print(one());\n}");
        assert!(asm.contains("mov  rax, 1"), "{asm}");
        // The caller reads the result out of rax.
        assert!(asm.contains("call one"), "{asm}");
    }

    #[test]
    fn main_always_returns_zero_whatever_it_computed() {
        let asm = compile("print(1);");
        assert!(asm.contains("xor  eax, eax"), "{asm}");
    }

    #[test]
    fn a_void_function_returns_without_touching_rax() {
        let asm = compile_src("fn greet() {\n  print(1);\n}\nfn main() {\n  greet();\n}");
        // Only `main`'s epilogue zeroes eax.
        assert_eq!(asm.matches("xor  eax, eax").count(), 1, "{asm}");
    }

    #[test]
    fn no_allocatable_register_is_an_argument_register() {
        // This is the invariant the whole argument-move story rests on: if the
        // allocator could hand out rcx/rdx/r8/r9, setting up a call could
        // clobber a value that call still has to read.
        let backend = X64Windows::new();
        let file = backend.register_file();
        assert!(file.caller_saved.is_empty());
        for &reg in &file.callee_saved {
            assert!(!ARG_REGS.contains(&file.name(reg)), "{} is an argument register", file.name(reg));
        }
    }

    #[test]
    fn a_recursive_function_calls_itself_by_name() {
        let asm = compile_src(
            "fn fib(int n) -> int {\n  if (n < 2) {\n    return n;\n  } else {\n    \
             return fib(n - 1) + fib(n - 2);\n  }\n}\nfn main() {\n  print(fib(10));\n}",
        );
        assert_eq!(asm.matches("call fib").count(), 3, "{asm}"); // twice inside, once from main
    }
}
