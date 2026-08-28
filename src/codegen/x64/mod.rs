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
//! | a `double` in one | `xmm`*n* **and** the integer register for *n* | `xmm0`, counted apart from the integer arguments |
//! | write, read | `_write`, `_read` | `write`, `read` |
//! | the console | has a code page, and it is not UTF-8 | is a byte stream already |
//!
//! Everything else in this directory is written against those seven rows and
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
//! An argument register in the pool is a contradiction as soon as calls exist:
//! setting up `f(x, y, z)` writes `r8`, which may be where `z` is still waiting
//! to be read. Ordering those moves and breaking their cycles is the *parallel
//! move* problem. Withdrawing every argument register sidesteps it — no source
//! of an argument move can then be an argument register — at the cost of a
//! `push`/`pop` pair per register used. Linux pays more: `rsi` and `rdi` are
//! argument registers there, so the pool is five rather than seven.
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
//! would define a label the `print` statement's own `call printf` then reaches,
//! and `fn str0()` would collide with a string literal's label. A `$` is valid
//! in a NASM identifier and is not one TinyC's lexer can produce, so the two
//! namespaces cannot meet.
//!
//! `main` is the exception, and has to be: it is the name the C runtime startup
//! calls, on both platforms. Nothing this backend generates is called `main`,
//! so it is safe to leave alone.
//!
//! ## The runtime
//!
//! Everything under `tc$rt$` is emitted here. The *aborts* are jumped to, never
//! called, and never return — see `runtime::abort_stubs`. The rest exist for one
//! reason each: they are **loops**. Everything a string does in a straight line
//! is emitted inline; joining two, comparing two, encoding one for output and
//! writing a number out have to walk characters, so they become calls.
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
use crate::ir::{Program, Runtime};
use crate::target::{Layout, Machine};

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
/// The low bytes of the two, which is where `setcc` deposits its result.
const SCRATCH0_8: &str = "r10b";
const SCRATCH1_8: &str = "r11b";

/// The two vector registers float arithmetic borrows, and the reason the
/// allocator never hears about a float at all.
///
/// A float travels in a general register like everything else — see
/// [`crate::ir::Num`] — so these hold an operand only for the instant between
/// the `movq` that brings it in and the one that takes the answer back out.
/// Nothing lives here across an instruction, which is what lets them be
/// hard-wired rather than allocated: `xmm0`-`xmm5` are destroyed by a call on
/// both platforms, and no value of ours is in one when a call happens.
const XMM0: &str = "xmm0";
const XMM1: &str = "xmm1";

/// How many arguments a TinyC function may take, on every platform.
///
/// A language limit rather than an ABI one — see the module docs for why the
/// System V backend does not claim the six it could pass.
pub const MAX_ARGS: usize = 4;

/// Bytes of header in front of a string's characters, holding the count.
///
/// A word, and this backend's word is eight — the same eight
/// [`Backend::machine`] reports as [`Layout::LP64`]. It lives here rather than
/// beside the IR because only code that emits instructions ever reads it: the
/// header is a `[p-8]` in an addressing mode, not something the lowering has an
/// opinion about.
const STR_HEADER: u32 = Layout::LP64.word;

/// The registers a runtime routine may keep a value in across a call it makes.
///
/// Callee-saved in *both* conventions and an argument register in *neither*, so
/// a routine written against these reads the same whichever platform it is
/// emitted for. [`asm::StubFrame`] refuses to save anything else, which is what
/// keeps a routine from quietly acquiring a register that only works on one of
/// them.
///
/// The list is short because `rax`, `rcx`, `rdx` and `r8`-`r11` are destroyed
/// by a call on both platforms and so are free scratch between calls, while
/// `rsi` and `rdi` are callee-saved on Windows and would corrupt a caller's
/// variable. Anything that has to survive a call goes here and is pushed.
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

/// Bytes in a page of memory, on x86-64 and on both platforms.
///
/// A stack does not simply exist down to its end: the pages below the one in
/// use are not there yet, and what puts them there is *touching* the one right
/// after the last. So a frame bigger than this cannot be taken in a single
/// `sub rsp` — that would step over the page whose job is to notice, and the
/// first write into the frame would land on memory the program does not have.
/// See `func::FnEmitter::reserve_frame`.
const PAGE_BYTES: u32 = 4096;

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
    /// Where the `double` that follows a format string travels, and whether the
    /// integer register for that position has to carry it as well.
    ///
    /// The two conventions number vector registers differently, and this is the
    /// whole of the difference as far as this backend is concerned — it makes
    /// exactly one variadic call, `printf`, with at most one double in it.
    ///
    /// Windows numbers them by argument *position*: the format string is
    /// argument 0 and the value is argument 1, so the value goes in `xmm1` —
    /// and in `rdx` too, because the callee of a variadic function is free to
    /// read either and `printf` reads whichever its format told it to expect.
    /// System V numbers them by *class*: the format string is an integer
    /// argument and the double is the first vector one whatever preceded it, so
    /// it goes in `xmm0` and nothing else carries it.
    pub vector_arg: &'static str,
    pub vector_arg_shadowed: bool,
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

    /// Leave the `FILE*` standard output goes to in `rax`.
    ///
    /// Writing by length means `fwrite`, and `fwrite` wants a stream. Every C
    /// library has one called `stdout` and the two spell it differently enough
    /// that it cannot be a name in a table: on one it is a variable to load,
    /// on the other a function to call. Emitted inside a routine that already
    /// owns a frame, so a `call` is fine here.
    fn stdout_stream(&self, asm: &mut Asm);

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

    /// Storage only this platform's [`Self::stack_bottom`] needs, emitted into
    /// `.bss`. Both answers arrive through out-parameters, and the two calls
    /// want different room for them.
    fn stack_bss(&self, asm: &mut Asm);

    /// Leave the lowest address of this thread's stack in `rax`, or zero if
    /// this platform could not say.
    ///
    /// The one question about the machine that `rsp` cannot answer. A stack is
    /// a range the program was *given*, and nothing in the running program
    /// records where it ends — so both platforms have to ask, and they ask
    /// different things. Guessing instead (a fixed budget below the first
    /// `rsp`, say) would be a number that is right on one machine and silently
    /// wrong on the next, which is the kind of answer this compiler does not
    /// give.
    ///
    /// Emitted in the entry point once its frame exists, so a `call` is fine
    /// here. Zero is a permitted answer and disables the check rather than
    /// guessing at one — see [`runtime::STACK_LIMIT`].
    fn stack_bottom(&self, asm: &mut Asm);

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

    fn machine(&self) -> Machine {
        // Both platforms are x86-64: eight-byte words, and four arguments in
        // registers because that is what the narrower of the two conventions
        // passes — see [`MAX_ARGS`].
        Machine { layout: Layout::LP64, max_args: MAX_ARGS }
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
        if used.writes_text() {
            runtime::output_stubs(&mut asm, self.platform, &used);
        }
        if used.fixup {
            runtime::fixup_stubs(&mut asm, abi, &program.table, &used);
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
            runtime::abort_stubs(&mut asm, abi, &used);
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

/// How a float comparison is made: which way round `ucomisd` takes its
/// operands, and which condition reads the flags it leaves.
///
/// `ucomisd` writes CF, ZF and PF, and reports "unordered" — either operand is
/// a NaN — as **all three set**, which is also what "below or equal" looks
/// like. So the conditions here are the *unsigned* ones, and every ordering
/// question is asked as `above`: `seta` and `setae` are the two that are false
/// when the flags say unordered, which is what IEEE asks for. `a < b` gets
/// there by comparing `b` against `a` rather than by reaching for `setb`,
/// which would answer true for a NaN.
///
/// Equality is the one that cannot be a single `setcc`: `sete` reads ZF, and
/// unordered sets it. `NaN == NaN` has to be false, so the answer is combined
/// with PF — see the `Cmp` arm, which is the only caller.
fn float_setcc(op: CmpOp) -> (bool, &'static str) {
    match op {
        CmpOp::Eq => (false, "sete"),
        CmpOp::Ne => (false, "setne"),
        CmpOp::Lt => (true, "seta"),
        CmpOp::Le => (true, "setae"),
        CmpOp::Gt => (false, "seta"),
        CmpOp::Ge => (false, "setae"),
    }
}

/// Which format string a type is printed with.
///
/// An enum goes out through `%s`: printing a value of one means printing the
/// name of its variant, which is a run of bytes the *compiler* wrote and can
/// therefore promise holds no NUL.
///
/// A `string` and a `char` are the ones that cannot promise that, and so the
/// ones that no longer go through `printf` at all — they are written by
/// length, through [`runtime::WRITE_TEXT`]. They keep a slot here so that
/// [`Used::ends_a_line`] can still be asked about them; nothing reads a format
/// out of it.
///
/// A `float` has a slot of its own and not the `int`'s. Sharing one is what the
/// two halves of this backend used to do about it, and they did not agree: the
/// table of formats to emit was told an `int` had been printed while the code
/// asked for the string one, and the program failed to assemble over a symbol
/// nothing had defined.
fn format_index(ty: Ty) -> usize {
    match ty {
        Ty::Int => 0,
        Ty::Enum(_) => 1,
        Ty::Bool => 2,
        // Listed rather than left to a wildcard, so that a new type has to be
        // thought about here instead of quietly getting `%s` applied to it.
        Ty::Str | Ty::Char | Ty::Array(_) | Ty::List(_) | Ty::Class(_) => 3,
        Ty::Float => 4,
    }
}

/// How many formats there are, which is what [`Used`] keeps one flag each of.
const FORMATS: usize = 5;

/// The slot an enum's format lives in, for the questions asked about it by
/// name rather than about a value in hand.
fn enum_slot() -> usize {
    format_index(Ty::Enum(crate::ast::EnumId(0)))
}

/// The slot a string and a character share — the two written by length.
fn text_slot() -> usize {
    format_index(Ty::Str)
}
