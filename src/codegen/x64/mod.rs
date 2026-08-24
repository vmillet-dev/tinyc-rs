//! The x86-64 backend, and the two platforms it is emitted for.
//!
//! There is **one** code generator here, not two. Instruction selection, the
//! register allocator's view of the machine, the arena, and every routine a
//! string or a list turns into are facts about *x86-64*, and they are written
//! once. What Windows and Linux disagree about is a short, enumerable list —
//! [`Abi`] holds the part that is data, [`Platform`] the part that is code:
//!
//! | | Windows (Microsoft x64) | Linux (System V AMD64) |
//! |---|---|---|
//! | argument registers | `rcx`, `rdx`, `r8`, `r9` | `rdi`, `rsi`, `rdx`, `rcx` |
//! | shadow space | 32 bytes, reserved by the caller | none |
//! | callee-saved | `rbx`, `rsi`, `rdi`, `rbp`, `r12`-`r15` | `rbx`, `rbp`, `r12`-`r15` |
//! | a variadic call | says nothing | sets `al` to the vector registers used |
//! | write, read | `_write`, `_read` | `write`, `read` |
//! | the console | has a code page, and it is not UTF-8 | is a byte stream already |
//!
//! Everything else in this directory is written against those six rows and
//! never asks which platform it is emitting for.
//!
//! ## Register policy
//!
//! | Role | Windows | Linux |
//! |------|---------|-------|
//! | call arguments (never allocated) | `rcx`, `rdx`, `r8`, `r9` | `rdi`, `rsi`, `rdx`, `rcx` |
//! | `idiv` dividend and return value (never allocated) | `rax` | `rax` |
//! | scratch for spilled operands (never allocated) | `r10`, `r11` | `r10`, `r11` |
//! | allocatable, callee-saved | `rbx`, `rsi`, `rdi`, `r12`-`r15` | `rbx`, `r12`-`r15` |
//!
//! Keeping the ABI-critical registers out of the allocator's hands is what lets
//! the allocator stay target-independent: it only ever sees the last row.
//!
//! ### Why nothing is allocatable and caller-saved
//!
//! `r8` and `r9` used to be handed out by the allocator. They are also
//! argument registers, and that is a contradiction as soon as calls exist:
//! setting up `f(x, y, z)` writes `r8`, which may be exactly where `z` is still
//! waiting to be read. Solving that in general is the *parallel move* problem —
//! you have to order the moves, and break cycles with a temporary.
//!
//! Withdrawing every argument register from the pool sidesteps it entirely.
//! Every value the allocator hands out lives in a callee-saved register or a
//! spill slot, so no source of an argument move can ever be an argument
//! register, and the moves can be emitted in any order. The cost is a
//! `push`/`pop` pair in the prologue for each register used, which is a good
//! trade for a compiler this size.
//!
//! Linux pays more for it than Windows does: `rsi` and `rdi` are argument
//! registers there, so the pool is five registers rather than seven and a
//! crowded function spills sooner. The alternative is the parallel move
//! problem, and it is not worth two registers.
//!
//! ## How many arguments a TinyC function may take
//!
//! Four, on both platforms — see [`MAX_ARGS`]. System V passes six, and the
//! backend could honestly say so, since [`crate::sema`] asks the target rather
//! than deciding for itself. It says four anyway: TinyC is one language, and a
//! five-parameter function that compiles on one machine and is refused on
//! another would be a portability trap the compiler could see and did not
//! mention.
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
//! calls, on both platforms. Nothing this backend generates is called `main`,
//! so it is safe to leave alone.
//!
//! ## The runtime
//!
//! Everything under `tc$rt$` is emitted here and called like an ordinary
//! function. There are two families. The *aborts* are jumped to, never called,
//! and never return — see `runtime::abort_stubs`. The rest are real routines,
//! and they exist for one reason each: they are **loops**. Everything a string
//! does in a straight line is emitted inline; joining two, comparing two,
//! encoding one for output and writing a number out have to walk characters, so
//! they become calls.
//!
//! Their memory comes from an arena that never frees — see `runtime::arena`,
//! which is also where the trade that buys it is written down.
//!
//! Each routine is emitted only when the program reaches it, so a program that
//! touches no string links no `malloc` and carries no encoder.

mod asm;
mod data;
mod func;
mod linux;
mod runtime;
mod used;
mod windows;

#[cfg(test)]
mod tests;

use crate::ast::{CmpOp, Ty};
use crate::codegen::{Allocation, Backend, PhysReg, RegisterFile};
use crate::ir::{Instr, Program, Runtime, Value};

use asm::Asm;
pub use linux::Linux;
use used::Used;
pub use windows::Windows;

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
const RDX: &str = "rdx";
/// Holds a spilled destination while it is being computed.
const SCRATCH0: &str = "r10";
/// Holds an immediate or divisor that cannot be an instruction operand.
const SCRATCH1: &str = "r11";
/// The low byte of [`SCRATCH1`], which is where `setcc` deposits its result.
const SCRATCH1_8: &str = "r11b";

/// How many arguments a TinyC function may take, on every platform.
///
/// A language limit rather than an ABI one — see the module docs for why the
/// System V backend does not claim the six it could pass.
pub const MAX_ARGS: usize = 4;

/// The registers a runtime routine may keep a value in across a call it makes.
///
/// Callee-saved in *both* conventions and an argument register in *neither*, so
/// a routine written against these reads the same whichever platform it is
/// emitted for. [`asm::StubFrame`] refuses to save anything else, which is what
/// keeps a routine from quietly acquiring a register that only works on one of
/// them.
///
/// The rule the routine bodies follow, and the reason this list is short:
///
/// * `rax`, `rcx`, `rdx`, `r8`-`r11` are destroyed by a call on both platforms,
///   so between calls they are free scratch — an argument register only matters
///   at a call boundary, and `rcx` in the middle of a copy loop is not one.
/// * `rsi` and `rdi` are **not** scratch. They are callee-saved on Windows,
///   where the allocator hands them out, so a routine that clobbered one would
///   corrupt a variable of whichever function called it.
/// * Anything that has to survive a call goes in this list and is pushed.
const RUNTIME_LOCALS: [&str; 5] = ["rbx", "r12", "r13", "r14", "r15"];

/// What every TinyC function's symbol starts with. See the module docs.
const SYMBOL_PREFIX: &str = "tc$";

/// What the compiler's own helpers start with. A TinyC identifier cannot
/// contain a `$`, so `tc$rt$…` is a name only this backend can produce — even a
/// function called `rt` becomes `tc$rt`, not `tc$rt$anything`.
const RUNTIME_PREFIX: &str = "tc$rt$";

/// The entry point, which returns 0 to the C runtime rather than a value of
/// its own. The same name on both platforms.
const ENTRY_POINT: &str = "main";

/// The file descriptor a runtime failure is reported on.
const STDERR: u32 = 2;

// -- what the two platforms disagree about ---------------------------------

/// The calling convention, as data.
///
/// Everything the shared emitter needs to know about a platform that can be
/// written down rather than emitted. [`Platform`] carries the rest.
pub struct Abi {
    /// Where the first arguments travel, in order. Only the first [`MAX_ARGS`]
    /// are ever used for a TinyC function; the runtime's own routines take at
    /// most three.
    pub args: &'static [&'static str],
    /// The registers the allocator may hand out: callee-saved, and an argument
    /// register in neither convention.
    pub allocatable: &'static [PhysReg],
    /// Bytes every caller reserves for its callee before a `call`, which the
    /// callee may use as it likes. Windows calls this *shadow space*; System V
    /// has no such thing.
    pub shadow_space: u32,
    /// Whether a variadic call has to announce how many vector registers it
    /// passes. System V reads `al`; Windows does not look.
    pub variadic_in_al: bool,
    /// `write(2)`, under the name this platform's C library exports it as.
    pub write: &'static str,
    /// `read(2)`, likewise.
    pub read: &'static str,
}

impl Abi {
    /// Where argument `index` travels.
    fn arg(&self, index: usize) -> &'static str {
        self.args[index]
    }

    /// What a routine that has pushed `saved` registers must reserve so that
    /// `rsp` is 16-byte aligned at every `call` it makes, and its callees have
    /// the shadow space they expect above `scratch` bytes of its own.
    ///
    /// A routine is reached by `call`, so `rsp % 16 == 8` on arrival and each
    /// push takes eight more. Deriving this rather than writing the number down
    /// is what lets one routine body be emitted for a platform with shadow
    /// space and one without.
    fn frame(&self, saved: usize, scratch: u32) -> u32 {
        let wanted = self.shadow_space + scratch;
        let so_far = 8 + 8 * saved as u32;
        wanted + (16 - (so_far + wanted) % 16) % 16
    }

    /// The same, for a routine that pushes nothing and makes one call.
    fn bare_call_frame(&self) -> u32 {
        self.frame(0, 0)
    }

    /// What [`Self::bare_call_frame`] is made of, for the comment beside it.
    fn bare_call_note(&self) -> &'static str {
        match self.shadow_space {
            0 => "alignment",
            _ => "shadow space + alignment",
        }
    }
}

/// The half of a platform that is code rather than data.
///
/// Four things, and they are the only four the shared emitter ever hands back:
/// which symbols to import, what the entry point owes the console, where the
/// input path keeps its own state, and how to fill the input buffer.
pub trait Platform: Send + Sync {
    /// Target triple-ish name, as `--target` accepts it and the header prints it.
    fn name(&self) -> &'static str;

    fn abi(&self) -> &'static Abi;

    /// The symbols this platform's emitted code calls, beyond the ones every
    /// platform needs. Declared in the header, in the order given.
    fn externs(&self, used: &Used) -> Vec<&'static str>;

    /// Whatever the entry point must do before the program's first statement.
    ///
    /// A Windows console has a code page and it is not UTF-8 unless it is told
    /// so; a Linux terminal is a byte stream and there is nothing to say.
    fn entry_setup(&self, asm: &mut Asm, used: &Used);

    /// Whether [`Self::entry_setup`] emits a `call`, and so whether the entry
    /// point owes a frame even when nothing else in it calls anything.
    fn entry_calls(&self, used: &Used) -> bool;

    /// Storage only this platform's input path needs, emitted into `.bss`.
    fn input_bss(&self, asm: &mut Asm);

    /// Bytes of its own that [`Self::refill_read`] needs on the stack, above
    /// the shadow space. A call with more arguments than fit in registers
    /// passes the rest here.
    fn refill_scratch(&self) -> u32 {
        0
    }

    /// Fill the input buffer, answering in `rax` how many bytes are now in it,
    /// or zero at the end of the input.
    ///
    /// Emitted inside a routine the shared code owns, which has already
    /// flushed what was printed and will strip a byte order mark from whatever
    /// this leaves behind. `frame` is that routine's frame, for reaching the
    /// scratch this platform asked for.
    fn refill_read(&self, asm: &mut Asm, frame: &asm::StubFrame);
}

// -- the backend -----------------------------------------------------------

/// The x86-64 backend, for one platform.
pub struct X64 {
    platform: &'static dyn Platform,
    registers: RegisterFile,
}

impl X64 {
    pub fn new(platform: &'static dyn Platform) -> X64 {
        let abi = platform.abi();
        assert!(
            abi.args.len() >= MAX_ARGS,
            "{} passes fewer registers than a TinyC function may take",
            platform.name()
        );
        X64 {
            platform,
            registers: RegisterFile {
                names: NAMES.to_vec(),
                // See the module docs: every caller-saved register that remains
                // free is an argument register or a scratch register, so none is
                // allocatable.
                caller_saved: Vec::new(),
                callee_saved: abi.allocatable.to_vec(),
                max_args: MAX_ARGS,
            },
        }
    }

    pub fn windows() -> X64 {
        X64::new(&Windows)
    }

    pub fn linux() -> X64 {
        X64::new(&Linux)
    }

    fn abi(&self) -> &'static Abi {
        self.platform.abi()
    }
}

impl Backend for X64 {
    fn name(&self) -> &'static str {
        self.platform.name()
    }

    fn register_file(&self) -> &RegisterFile {
        &self.registers
    }

    fn emit(&self, program: &Program, allocations: &[Allocation]) -> String {
        let mut asm = Asm::new();
        let used = Used::of(program);
        let abi = self.abi();

        data::header(&mut asm, self.platform, &used);
        data::data_section(&mut asm, self.platform, program, &used);
        asm.line("section .text");

        for (function, allocation) in program.functions.iter().zip(allocations) {
            func::FnEmitter::new(
                program,
                function,
                allocation,
                &self.registers,
                self.platform,
                &used,
                &mut asm,
            )
            .run();
        }

        if used.allocates() {
            runtime::arena(&mut asm, abi);
        }
        runtime::string_stubs(&mut asm, abi, &used);
        runtime::list_stubs(&mut asm, abi, &used);
        if used.encodes_text() {
            runtime::text_stubs(&mut asm, abi, &used);
        }
        if used.reads_text() {
            runtime::input_stubs(&mut asm, self.platform, &used);
        }
        if used.aborts {
            runtime::abort_stubs(&mut asm, abi);
        }
        asm.finish()
    }
}

// -- shared helpers --------------------------------------------------------

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

/// The assembly name of one of the compiler's own routines.
fn runtime_symbol(callee: Runtime) -> String {
    format!("{RUNTIME_PREFIX}{}", callee.name())
}

/// The 32-bit half of a 64-bit register this backend named.
fn half(name: &str) -> &'static str {
    let index = NAMES.iter().position(|&n| n == name).expect("a register from this backend");
    NAMES32[index]
}

/// The 32-bit half of a register, which is what reads a character out of a
/// string and, in doing so, widens it.
fn narrow(reg: &str) -> &'static str {
    half(reg)
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

/// Whether an instruction may jump to a runtime abort, and so whether the
/// program needs the abort routine emitted at all.
///
/// Guarded arithmetic is nearly everything now: only a division both of whose
/// faults a literal operand rules out escapes, and the operators that cannot
/// fail at all.
fn may_abort(instr: &Instr) -> bool {
    match instr {
        // A constant index into an array of known length was settled at
        // compile time; anything else — including every index into a string,
        // whose length is never known here — is checked where it lands.
        Instr::Elem { index, len, .. } => {
            !matches!((index, len), (Value::Const(_), Value::Const(_)))
        }
        Instr::Bin { op, lhs, rhs, .. } if op.divides() => DivGuards::of(lhs, rhs).any(),
        // `add`, `sub` and `imul` are all guarded; a folded result never
        // reaches an instruction in the first place.
        Instr::Bin { .. } => true,
        // Every runtime routine can fail: the two that allocate run out of
        // memory, and the one that checks a character refuses.
        Instr::RtCall { .. } => true,
        _ => false,
    }
}

/// Which format string a type is printed with.
///
/// An enum shares the string one: printing a value of one means printing the
/// name of its variant, which is a C string in `.data`. A `string` and a `char`
/// share it too, one step further removed — they are encoded into a buffer
/// first, and it is that buffer `printf` is given.
fn format_index(ty: Ty) -> usize {
    match ty {
        Ty::Int => 0,
        Ty::Bool => 2,
        // Listed rather than left to a wildcard, so that a new type has to be
        // thought about here instead of quietly getting `%s` applied to it.
        Ty::Str | Ty::Char | Ty::Enum(_) | Ty::Array(_) | Ty::List(_) | Ty::Class(_) => 1,
    }
}
