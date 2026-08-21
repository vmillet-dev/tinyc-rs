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

use crate::ast::{BinOp, CmpOp, Ty};
use crate::codegen::{Allocation, Backend, Location, PhysReg, RegisterFile};
use crate::ir::{Block, Function, Instr, Program, Terminator, VReg, Value};

/// Register numbers follow the usual x86-64 encoding order.
const NAMES: [&str; 16] = [
    "rax", "rcx", "rdx", "rbx", "rsp", "rbp", "rsi", "rdi", //
    "r8", "r9", "r10", "r11", "r12", "r13", "r14", "r15",
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

/// Bytes of shadow space every caller must reserve on Windows.
const SHADOW_SPACE: u32 = 32;

/// The entry point, which returns 0 to the C runtime rather than a value of
/// its own.
const ENTRY_POINT: &str = "main";

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
        Emitter { program, allocations, backend: self, current: 0, out: String::new() }.run()
    }
}

struct Emitter<'a> {
    program: &'a Program,
    allocations: &'a [Allocation],
    backend: &'a X64Windows,
    /// Index of the function being emitted.
    current: usize,
    out: String,
}

impl<'a> Emitter<'a> {
    fn run(mut self) -> String {
        self.header();
        self.data_section();
        self.line("section .text");

        for index in 0..self.program.functions.len() {
            self.current = index;
            self.function();
        }
        self.out
    }

    /// The function being emitted, and its allocation.
    fn function(&mut self) {
        let function = self.func();
        let allocation = self.alloc();
        let frame = FrameLayout::new(allocation);

        self.blank();
        for line in allocation.dump(function, self.backend.register_file()).lines() {
            self.comment(line);
        }
        self.line(&format!("{}:", function.name));
        self.prologue(&frame);

        for (index, block) in function.blocks.iter().enumerate() {
            self.blank();
            self.line(&format!(".{}:", block.label));
            for instr in &block.instrs {
                let described = self.describe(instr);
                self.comment(&described);
                self.instr(instr, &frame);
            }
            self.terminator(block, index, &frame);
        }
    }

    fn func(&self) -> &'a Function {
        &self.program.functions[self.current]
    }

    fn alloc(&self) -> &'a Allocation {
        &self.allocations[self.current]
    }

    fn is_entry_point(&self) -> bool {
        self.func().name == ENTRY_POINT
    }

    /// Emit a block's exit. A jump to the very next block is left out: control
    /// simply falls through.
    fn terminator(&mut self, block: &Block, index: usize, frame: &FrameLayout) {
        let next = index + 1;
        match &block.term {
            Terminator::Jump(target) => {
                let label = self.func().block(*target).label.clone();
                if target.0 as usize != next {
                    self.comment(&format!("jump {label}"));
                    self.asm(&format!("jmp  .{label}"));
                }
            }
            Terminator::Branch { cond, then_blk, else_blk } => {
                let then_label = self.func().block(*then_blk).label.clone();
                let else_label = self.func().block(*else_blk).label.clone();
                self.comment(&format!("branch to {then_label} or {else_label}"));

                // `test` needs a register, and the condition may be a literal.
                let cond = self.value(cond, frame);
                self.mov(SCRATCH0, &cond);
                self.asm(&format!("test {SCRATCH0}, {SCRATCH0}"));
                self.asm(&format!("jz   .{else_label}"));
                if then_blk.0 as usize != next {
                    self.asm(&format!("jmp  .{then_label}"));
                }
            }
            Terminator::Return(value) => {
                match value {
                    // `main` always reports success, whatever it computed.
                    _ if self.is_entry_point() => {
                        self.comment("return 0 to the CRT");
                        self.asm("xor  eax, eax");
                    }
                    Some(value) => {
                        self.comment("return value in rax");
                        let value = self.value(value, frame);
                        self.mov(RAX, &value);
                    }
                    None => self.comment("return"),
                }
                self.epilogue(frame);
            }
        }
    }

    // -- output helpers ----------------------------------------------------

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

    // -- sections ----------------------------------------------------------

    fn header(&mut self) {
        let rule = format!("; {}", "-".repeat(68));
        let name = self.backend.name();

        self.line(&rule);
        self.line(&format!("; Generated by tinyc for {name} (NASM syntax)"));
        self.line(&rule);
        // Without `default rel`, `lea rcx, [label]` would use a 32-bit absolute
        // address instead of the RIP-relative form 64-bit code wants.
        self.line("default rel");
        self.blank();
        self.line("extern printf");
        self.line(&format!("global {ENTRY_POINT}"));
        self.blank();
    }

    fn data_section(&mut self) {
        let program = self.program;

        self.line("section .data");
        if self.uses_format(Ty::Int) {
            self.asm(&format!("{FMT_INT}: db \"%lld\", 10, 0"));
        }
        if self.uses_format(Ty::Str) {
            self.asm(&format!("{FMT_STR}: db \"%s\", 10, 0"));
        }
        if self.uses_format(Ty::Bool) {
            self.asm(&format!("{FMT_BOOL}: db \"%s\", 10, 0"));
            self.asm(&format!("{BOOL_TRUE}: db \"true\", 0"));
            self.asm(&format!("{BOOL_FALSE}: db \"false\", 0"));
        }
        for (index, bytes) in program.strings.iter().enumerate() {
            // Emitting raw bytes avoids every string-quoting corner case.
            let values: Vec<String> = bytes
                .iter()
                .map(|b| b.to_string())
                .chain(std::iter::once("0".to_string()))
                .collect();
            let text = String::from_utf8_lossy(bytes).escape_debug().to_string();
            self.asm(&format!("str{index}: db {}    ; \"{text}\"", values.join(", ")));
        }
        self.blank();
    }

    fn uses_format(&self, ty: Ty) -> bool {
        self.program
            .functions
            .iter()
            .flat_map(|function| &function.blocks)
            .flat_map(|block| &block.instrs)
            .any(|instr| matches!(instr, Instr::Print { ty: t, .. } if *t == ty))
    }

    fn prologue(&mut self, frame: &FrameLayout) {
        let saved: Vec<&str> = self
            .alloc()
            .used_callee_saved
            .iter()
            .map(|&reg| self.backend.registers.name(reg))
            .collect();
        let slots = self.alloc().spill_slots;

        self.comment("prologue: save callee-saved registers, then reserve the frame");
        for reg in saved {
            self.asm(&format!("push {reg}"));
        }
        self.asm(&format!(
            "sub  rsp, {}    ; {SHADOW_SPACE} bytes of shadow space + {slots} spill slot(s) + alignment",
            frame.size
        ));
    }

    fn epilogue(&mut self, frame: &FrameLayout) {
        let saved: Vec<&str> = self
            .alloc()
            .used_callee_saved
            .iter()
            .rev()
            .map(|&reg| self.backend.registers.name(reg))
            .collect();

        self.asm(&format!("add  rsp, {}", frame.size));
        for reg in saved {
            self.asm(&format!("pop  {reg}"));
        }
        self.asm("ret");
    }

    // -- instruction selection --------------------------------------------

    fn instr(&mut self, instr: &Instr, frame: &FrameLayout) {
        match instr {
            Instr::Const { dst, val } => {
                let work = self.work_reg(*dst, false);
                self.asm(&format!("mov  {work}, {val}"));
                self.store_back(*dst, &work, frame);
            }
            Instr::StrAddr { dst, id } => {
                let work = self.work_reg(*dst, false);
                self.asm(&format!("lea  {work}, [str{}]", id.0));
                self.store_back(*dst, &work, frame);
            }
            Instr::Copy { dst, src } => {
                let src = self.value(src, frame);
                let work = self.work_reg(*dst, false);
                self.mov(&work, &src);
                self.store_back(*dst, &work, frame);
            }
            // The argument is already sitting in its ABI register; this just
            // moves it to wherever the allocator decided it should live.
            Instr::Param { dst, index } => {
                let src = ARG_REGS[*index as usize];
                let work = self.work_reg(*dst, false);
                self.mov(&work, src);
                self.store_back(*dst, &work, frame);
            }
            // `cmp` sets the flags; `setcc` turns the one that matters into a
            // 0 or 1 byte, and `movzx` widens it to the 64-bit value a bool is.
            Instr::Cmp { op, dst, lhs, rhs } => {
                let lhs = self.value(lhs, frame);
                self.mov(SCRATCH0, &lhs);
                let (rhs, _) = self.operand_for_alu(rhs, frame);
                self.asm(&format!("cmp  {SCRATCH0}, {rhs}"));
                self.asm(&format!("{} {SCRATCH1_8}", setcc(*op)));

                let work = self.work_reg(*dst, false);
                self.asm(&format!("movzx {work}, {SCRATCH1_8}"));
                self.store_back(*dst, &work, frame);
            }
            Instr::Bin { op: BinOp::Div, dst, lhs, rhs } => self.division(*dst, lhs, rhs, frame),
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
                let lhs = self.value(lhs, frame);
                self.mov(&work, &lhs);

                let (rhs, immediate) = self.operand_for_alu(rhs, frame);
                match op {
                    BinOp::Add => self.asm(&format!("add  {work}, {rhs}")),
                    BinOp::Sub => self.asm(&format!("sub  {work}, {rhs}")),
                    // `imul` has no two-operand immediate form.
                    BinOp::Mul if immediate => self.asm(&format!("imul {work}, {work}, {rhs}")),
                    BinOp::Mul => self.asm(&format!("imul {work}, {rhs}")),
                    BinOp::Div => unreachable!("handled above"),
                }
                self.store_back(*dst, &work, frame);
            }
            // No source of these moves can be an argument register, because the
            // allocator is never given one — see the module docs.
            Instr::Call { dst, callee, args } => {
                for (index, arg) in args.iter().enumerate() {
                    let src = self.value(arg, frame);
                    self.mov(ARG_REGS[index], &src);
                }
                self.asm(&format!("call {}", self.program.function(*callee).name));
                if let Some(dst) = dst {
                    let work = self.work_reg(*dst, false);
                    self.mov(&work, RAX);
                    self.store_back(*dst, &work, frame);
                }
            }
            Instr::Print { ty, val } => {
                // Load the value first: the format string is a constant, so it
                // can never be clobbered by the argument move.
                let value = self.value(val, frame);
                match ty {
                    Ty::Bool => {
                        self.mov(SCRATCH0, &value);
                        self.asm(&format!("lea  {RDX}, [{BOOL_FALSE}]"));
                        self.asm(&format!("lea  {SCRATCH1}, [{BOOL_TRUE}]"));
                        self.asm(&format!("test {SCRATCH0}, {SCRATCH0}"));
                        self.asm(&format!("cmovnz {RDX}, {SCRATCH1}"));
                    }
                    _ => self.mov(RDX, &value),
                }
                let format = match ty {
                    Ty::Int => FMT_INT,
                    Ty::Str => FMT_STR,
                    Ty::Bool => FMT_BOOL,
                };
                self.asm(&format!("lea  {RCX}, [{format}]"));
                self.asm("call printf");
            }
        }
    }

    /// `idiv` is fixed to `rdx:rax`, so the dividend is moved into `rax`, sign
    /// extended with `cqo`, and the quotient moved back out.
    fn division(&mut self, dst: VReg, lhs: &Value, rhs: &Value, frame: &FrameLayout) {
        let lhs = self.value(lhs, frame);
        self.mov(RAX, &lhs);
        self.asm("cqo");

        // `idiv` has no immediate form either.
        let divisor = match rhs {
            Value::Const(c) => {
                self.asm(&format!("mov  {SCRATCH1}, {c}"));
                SCRATCH1.to_string()
            }
            Value::Reg(reg) => self.location(*reg, frame),
        };
        self.asm(&format!("idiv {divisor}"));

        // The divisor has already been read, so the quotient can go straight
        // into the destination even if the two share a register.
        let work = self.work_reg(dst, false);
        self.mov(&work, RAX);
        self.store_back(dst, &work, frame);
    }

    /// Where a virtual register lives, as an assembly operand.
    fn location(&self, vreg: VReg, frame: &FrameLayout) -> String {
        match self.alloc().location(vreg) {
            Location::Reg(reg) => self.backend.registers.name(reg).to_string(),
            Location::Spill(slot) => format!("qword [rsp+{}]", frame.slot_offset(slot)),
        }
    }

    fn value(&self, value: &Value, frame: &FrameLayout) -> String {
        match value {
            Value::Const(c) => c.to_string(),
            Value::Reg(reg) => self.location(*reg, frame),
        }
    }

    /// Like [`Self::value`], but an immediate too wide for an ALU instruction is
    /// materialised in a scratch register first. Returns whether the resulting
    /// operand is still an immediate.
    fn operand_for_alu(&mut self, value: &Value, frame: &FrameLayout) -> (String, bool) {
        match value {
            Value::Const(c) if i32::try_from(*c).is_err() => {
                self.asm(&format!("mov  {SCRATCH1}, {c}"));
                (SCRATCH1.to_string(), false)
            }
            Value::Const(c) => (c.to_string(), true),
            other => (self.value(other, frame), false),
        }
    }

    /// Whether `value` lives exactly where `dst` will.
    fn shares_register(&self, dst: VReg, value: &Value) -> bool {
        matches!(
            value,
            Value::Reg(other) if self.alloc().location(*other) == self.alloc().location(dst)
        )
    }

    /// The register an instruction computes its result in: the destination's own
    /// register, or a scratch register when the destination is spilled or would
    /// clobber an operand that has not been read yet.
    fn work_reg(&self, dst: VReg, force_scratch: bool) -> String {
        match self.alloc().location(dst) {
            Location::Reg(reg) if !force_scratch => self.backend.registers.name(reg).to_string(),
            _ => SCRATCH0.to_string(),
        }
    }

    /// Move the result out of the scratch register or into its stack slot, if
    /// it was not computed in place.
    fn store_back(&mut self, dst: VReg, work: &str, frame: &FrameLayout) {
        match self.alloc().location(dst) {
            Location::Reg(reg) => {
                let home = self.backend.registers.name(reg);
                if home != work {
                    self.asm(&format!("mov  {home}, {work}"));
                }
            }
            Location::Spill(slot) => {
                self.asm(&format!("mov  qword [rsp+{}], {work}", frame.slot_offset(slot)));
            }
        }
    }

    /// The IR instruction, echoed into the assembly as a comment.
    fn describe(&self, instr: &Instr) -> String {
        let function = self.func();
        let value = |v: &Value| match v {
            Value::Const(c) => c.to_string(),
            Value::Reg(r) => format!("%{}", function.name_of(*r)),
        };
        match instr {
            Instr::Const { dst, val } => format!("%{} = {val}", function.name_of(*dst)),
            Instr::StrAddr { dst, id } => {
                format!("%{} = &str{}", function.name_of(*dst), id.0)
            }
            Instr::Copy { dst, src } => {
                format!("%{} = {}", function.name_of(*dst), value(src))
            }
            Instr::Param { dst, index } => {
                format!("%{} = argument {index}", function.name_of(*dst))
            }
            Instr::Bin { op, dst, lhs, rhs } => format!(
                "%{} = {} {} {}",
                function.name_of(*dst),
                value(lhs),
                op.symbol(),
                value(rhs)
            ),
            Instr::Cmp { op, dst, lhs, rhs } => format!(
                "%{} = {} {} {}",
                function.name_of(*dst),
                value(lhs),
                op.symbol(),
                value(rhs)
            ),
            Instr::Call { dst, callee, args } => {
                let args: Vec<String> = args.iter().map(value).collect();
                let call = format!("{}({})", self.program.function(*callee).name, args.join(", "));
                match dst {
                    Some(dst) => format!("%{} = {call}", function.name_of(*dst)),
                    None => call,
                }
            }
            Instr::Print { ty, val } => format!("print {} {}", ty.name(), value(val)),
        }
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

/// Stack frame layout for one function.
///
/// At function entry `rsp % 16 == 8` (the call pushed a return address). Each
/// pushed register subtracts 8 more, so the frame size is chosen to bring `rsp`
/// back to a 16-byte boundary before any `call`.
///
/// Every frame reserves shadow space, including a function that never calls
/// anything — 32 bytes is a small price for not having to prove a function is
/// a leaf.
struct FrameLayout {
    size: u32,
}

impl FrameLayout {
    fn new(allocation: &Allocation) -> FrameLayout {
        let locals = SHADOW_SPACE + 8 * allocation.spill_slots;
        let mut size = locals.div_ceil(16) * 16;
        if allocation.used_callee_saved.len() % 2 == 0 {
            size += 8;
        }
        FrameLayout { size }
    }

    /// Spill slots sit just above the shadow space.
    fn slot_offset(&self, slot: u32) -> u32 {
        SHADOW_SPACE + 8 * slot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::regalloc;
    use crate::{lexer, parser, sema};

    fn compile_src(src: &str) -> String {
        let ast = parser::parse(&lexer::lex(src).unwrap()).unwrap();
        let types = sema::check(&ast).unwrap();
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
                let frame = FrameLayout::new(&allocation);
                // 8 (return address) + 8*pushes + frame must be a multiple of 16.
                assert_eq!((8 + 8 * pushes as u32 + frame.size) % 16, 0);
                // The frame must still cover shadow space and every spill slot.
                assert!(frame.size >= SHADOW_SPACE + 8 * spill_slots);
            }
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
        assert!(asm.contains("jz   .else2"), "{asm}");
        // The `then` block falls through to `else` unless it jumps past it.
        assert!(asm.contains("jmp  .join3"), "{asm}");
    }

    #[test]
    fn a_comparison_becomes_cmp_plus_setcc() {
        let asm = compile("bool ok = 1 < 2;\nprint(ok);");
        assert!(asm.contains("cmp  r10, 2"), "{asm}");
        assert!(asm.contains("setl r11b"), "{asm}");
        assert!(asm.contains("movzx"), "{asm}");
    }

    #[test]
    fn each_comparison_picks_its_own_setcc() {
        for (source, expected) in [
            ("1 == 2", "sete"),
            ("1 != 2", "setne"),
            ("1 < 2", "setl"),
            ("1 <= 2", "setle"),
            ("1 > 2", "setg"),
            ("1 >= 2", "setge"),
        ] {
            let asm = compile(&format!("bool ok = {source};\nprint(ok);"));
            assert!(asm.contains(&format!("{expected} r11b")), "{source}: {asm}");
        }
    }

    #[test]
    fn a_loop_body_jumps_back_to_its_header() {
        let asm = compile("int i = 0;\nwhile (i < 3) {\n  i = i + 1;\n}\nprint(i);");
        assert!(asm.contains(".loop1:"), "{asm}");
        assert!(asm.contains(".body2:"), "{asm}");
        assert!(asm.contains("jmp  .loop1"), "{asm}");
        assert!(asm.contains("jz   .done3"), "{asm}");
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
        assert!(asm.contains("\nadd:\n"), "{asm}");
        assert!(asm.contains("\nmain:\n"), "{asm}");
        // One `ret` per function, and only `main` is exported.
        assert_eq!(asm.matches("\n    ret\n").count(), 2, "{asm}");
        assert!(asm.contains("global main"), "{asm}");
        assert!(!asm.contains("global add"), "{asm}");
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
