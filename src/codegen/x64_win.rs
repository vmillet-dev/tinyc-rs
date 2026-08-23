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
//!
//! ## The runtime
//!
//! Everything under `tc$rt$` is emitted by this module and called like an
//! ordinary function. There are two families. The *aborts* are jumped to, never
//! called, and never return — see [`abort_stubs`]. The rest are real routines,
//! and they exist for one reason each: they are **loops**. Everything a string
//! does in a straight line is emitted inline; joining two, comparing two,
//! encoding one for output and writing a number out have to walk characters, so
//! they become calls.
//!
//! Their memory comes from an arena that never frees — see [`arena`], which is
//! also where the trade that buys it is written down.
//!
//! Each routine is emitted only when the program reaches it, so a program that
//! touches no string links no `malloc` and carries no encoder.

use crate::ast::{BinOp, ClassId, CmpOp, EnumId, Ty};
use crate::codegen::{Allocation, Backend, Location, PhysReg, RegisterFile};
use crate::ir::{
    Block, CHAR_BYTES, Function, Instr, Program, Runtime, STR_HEADER, Terminator, VReg, Value,
};

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

/// Where an operation that cannot be performed lands.
///
/// Each is a label the failing instruction jumps to, paired with the text it
/// reports. They are kept together so that adding a way to fail means adding
/// one row rather than touching four places.
const ABORTS: [Abort; 10] = [
    Abort::new(ABORT_DIV_ZERO, "division by zero"),
    Abort::new(ABORT_DIV_OVERFLOW, "division overflows an int"),
    Abort::new(ABORT_OVERFLOW, "arithmetic overflows an int"),
    Abort::new(ABORT_BOUNDS, "index out of bounds"),
    Abort::new(ABORT_OOM, "out of memory"),
    Abort::new(ABORT_BAD_CHAR, "this number is not a character"),
    Abort::new(ABORT_NO_INPUT, "there is no more input to read"),
    Abort::new(ABORT_BAD_UTF8, "the input is not valid UTF-8"),
    Abort::new(ABORT_INPUT_FAILED, "the input could not be read"),
    Abort::new(ABORT_NOT_A_NUMBER, "this text is not a number an int can hold"),
];

/// Where a line of input is read into before it is picked apart.
///
/// The compiler does its own buffering rather than going through a `FILE*`,
/// which keeps it to one import and makes [`Runtime::Eof`] answerable without
/// pushing a character back: "is there more" is a question about this buffer.
const INPUT: &str = "tc$rt$input";
const INPUT_POS: &str = "tc$rt$input_pos";
const INPUT_LEN: &str = "tc$rt$input_len";
const INPUT_BYTES: u32 = 4096;
/// Whether a byte is waiting, refilling the buffer if it has run dry.
const READY: &str = "tc$rt$ready";
const NEXT_BYTE: &str = "tc$rt$next_byte";
/// Reads one character's worth of UTF-8, which is where the outside world's
/// encoding becomes the language's.
const UTF8_DECODE: &str = "tc$rt$utf8_decode";

/// The code page that makes a Windows console *hand over* what is typed into it
/// as UTF-8, which is [`UTF8_CODE_PAGE`]'s counterpart for input.
const CONSOLE_INPUT_CP: &str = "SetConsoleCP";

const ABORT_DIV_ZERO: &str = "tc$rt$div_by_zero";
const ABORT_DIV_OVERFLOW: &str = "tc$rt$div_overflow";
const ABORT_OVERFLOW: &str = "tc$rt$overflow";
const ABORT_BOUNDS: &str = "tc$rt$bounds";
const ABORT_OOM: &str = "tc$rt$out_of_memory";
const ABORT_BAD_CHAR: &str = "tc$rt$bad_char";
const ABORT_NO_INPUT: &str = "tc$rt$no_input";
const ABORT_BAD_UTF8: &str = "tc$rt$bad_utf8";
const ABORT_INPUT_FAILED: &str = "tc$rt$input_failed";
const ABORT_NOT_A_NUMBER: &str = "tc$rt$not_a_number";
/// The one routine `int(s)` and `is_int(s)` are both built on. Internal: no
/// `Runtime` names it, because no program calls it directly.
const PARSE_INT: &str = "tc$rt$parse_int";
/// The same for the two pushes: this makes the room and says where it is, and
/// they differ only in what they put there.
const LIST_ROOM: &str = "tc$rt$list_room";
const ABORT_REPORT: &str = "tc$rt$abort";

/// One way a program can stop rather than answer wrongly.
struct Abort {
    /// What the failing instruction jumps to.
    label: &'static str,
    /// What went wrong, as the program reports it.
    what: &'static str,
}

impl Abort {
    const fn new(label: &'static str, what: &'static str) -> Abort {
        Abort { label, what }
    }

    /// The label of this failure's text in `.data`, derived from its own so
    /// that a new way to fail is one row here and nothing anywhere else.
    fn message(&self) -> String {
        format!("{}_text", self.label)
    }

    fn text(&self) -> String {
        format!("runtime error: {}\n", self.what)
    }
}

/// The arena every string built at run time is cut from, and the routine that
/// cuts it. See [`arena`] for what it is and why it never gives anything back.
const ARENA_NEXT: &str = "tc$rt$arena_next";
const ARENA_END: &str = "tc$rt$arena_end";
const ALLOC: &str = "tc$rt$alloc";

/// Bytes the arena asks the C runtime for at a time.
///
/// Big enough that a program doing string work calls `malloc` a handful of
/// times, small enough that a program doing none pays nothing — the first
/// chunk is only asked for when something is first allocated.
const CHUNK_BYTES: u32 = 1 << 16;

/// Encodes one character as UTF-8, which is the only place the language's
/// representation meets the one the outside world reads.
/// The smallest capacity a list is given, so that the first few `push`es cost
/// no move at all.
const LIST_MIN_CAPACITY: u32 = 4;

const UTF8: &str = "tc$rt$utf8";
const PRINT_STR: &str = "tc$rt$print_str";
const PRINT_CHAR: &str = "tc$rt$print_char";
/// Where `print` builds the UTF-8 of a string, kept between calls so that
/// printing in a loop allocates once rather than once per line.
const SCRATCH: &str = "tc$rt$scratch";
const SCRATCH_CAP: &str = "tc$rt$scratch_cap";

/// The code page that makes a Windows console read what is written to it as
/// UTF-8. Without it a program that prints `é` prints two bytes of mojibake,
/// and the language's promise about characters would stop at the terminal.
const UTF8_CODE_PAGE: u32 = 65001;

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
            FnEmitter::new(program, function, allocation, &self.registers, &used, &mut asm).run();
        }

        if used.allocates() {
            arena(&mut asm);
        }
        string_stubs(&mut asm, &used);
        list_stubs(&mut asm, &used);
        if used.writes_text() {
            text_stubs(&mut asm, &used);
        }
        if used.reads_text() {
            input_stubs(&mut asm, &used);
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
    /// Which enums have a value printed, and so need their table of variant
    /// names emitted. An enum used only in a `match` needs none: matching is
    /// arithmetic on the tag, and never asks what the tag is called.
    enums: Vec<bool>,
    /// Which classes are ever instantiated, and so need their method table
    /// emitted. A class nothing builds has no objects to dispatch on.
    vtables: Vec<bool>,
    aborts: bool,
    /// Which of the compiler's own routines the program reaches, directly or
    /// through another one. A program that never touches a string pays for
    /// none of them — not even the arena.
    concat: bool,
    str_eq: bool,
    check_char: bool,
    char_str: bool,
    int_str: bool,
    str_int: bool,
    is_int: bool,
    list_new: bool,
    list_push: bool,
    list_push_big: bool,
    list_clone: bool,
    chars_str: bool,
    read_line: bool,
    eof: bool,
    print_str: bool,
    print_char: bool,
}

impl Used {
    fn of(program: &Program) -> Used {
        let mut used = Used {
            formats: [false; 3],
            enums: vec![false; program.table.enums.len()],
            vtables: vec![false; program.vtables.len()],
            aborts: false,
            concat: false,
            str_eq: false,
            check_char: false,
            char_str: false,
            int_str: false,
            str_int: false,
            is_int: false,
            list_new: false,
            list_push: false,
            list_push_big: false,
            list_clone: false,
            chars_str: false,
            read_line: false,
            eof: false,
            print_str: false,
            print_char: false,
        };
        for instr in program.functions.iter().flat_map(|f| &f.blocks).flat_map(|b| &b.instrs) {
            match instr {
                Instr::Print { ty, .. } => {
                    used.formats[format_index(*ty)] = true;
                    match ty {
                        Ty::Enum(id) => used.enums[id.0 as usize] = true,
                        Ty::Str => used.print_str = true,
                        Ty::Char => used.print_char = true,
                        _ => {}
                    }
                }
                Instr::VTable { class, .. } => used.vtables[class.0 as usize] = true,
                other => {
                    used.aborts |= may_abort(other);
                    if let Instr::RtCall { callee, .. } = other {
                        match callee {
                            Runtime::Concat => used.concat = true,
                            Runtime::StrEq => used.str_eq = true,
                            Runtime::CheckChar => used.check_char = true,
                            Runtime::CharToStr => used.char_str = true,
                            Runtime::IntToStr => used.int_str = true,
                            Runtime::StrToInt => used.str_int = true,
                            Runtime::IsInt => used.is_int = true,
                            Runtime::ListNew => used.list_new = true,
                            Runtime::ListPush => used.list_push = true,
                            Runtime::ListPushBig => used.list_push_big = true,
                            Runtime::ListClone => used.list_clone = true,
                            Runtime::CharsToStr => used.chars_str = true,
                            Runtime::ReadLine => used.read_line = true,
                            Runtime::Eof => used.eof = true,
                        }
                    }
                }
            }
        }
        // Some routines are reached without any instruction naming them: a
        // `print` encodes into a buffer cut from the arena, cloning a list
        // builds the new one with `list_new`, and reading a line accumulates
        // its characters in a list before sealing it into a string.
        used.list_new |= used.list_clone | used.read_line;
        used.list_push |= used.read_line;
        used.chars_str |= used.read_line;
        // Asking the arena for memory is a way to fail like any other.
        used.aborts |= used.allocates();
        used
    }

    fn prints(&self, ty: Ty) -> bool {
        self.formats[format_index(ty)]
    }

    /// Whether the routine `int(s)` and `is_int(s)` are both wrappers around is
    /// needed. Nothing in the IR names it: it is reached only through them.
    fn parse_int(&self) -> bool {
        self.str_int || self.is_int
    }

    /// The same for the routine both pushes are wrappers around: it makes the
    /// room, and only what goes into it differs.
    fn list_room(&self) -> bool {
        self.list_push || self.list_push_big
    }

    /// Whether anything cuts memory out of the arena — which is what decides
    /// whether the program calls `malloc` at all.
    fn allocates(&self) -> bool {
        self.concat
            || self.char_str
            || self.int_str
            || self.list_new
            || self.list_room()
            || self.list_clone
            || self.chars_str
            || self.print_str
    }

    /// Whether any character is ever written out, and so whether the encoder
    /// and the console's code page are needed.
    /// Whether anything is read from the input, and so whether the buffer and
    /// the decoder are needed.
    fn reads_text(&self) -> bool {
        self.read_line || self.eof
    }

    fn writes_text(&self) -> bool {
        self.print_str || self.print_char
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

/// The label of a class's method table.
fn vtable_label(class: ClassId) -> String {
    format!("vtable{}", class.0)
}

/// The label of the table that maps an enum's tags to its variants' names.
fn enum_table(id: EnumId) -> String {
    format!("enum{}_names", id.0)
}

/// The label of one variant's name within that table.
fn enum_variant_text(id: EnumId, tag: usize) -> String {
    format!("enum{}_v{tag}", id.0)
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

/// One runtime routine's stack frame: the registers it saves and the bytes it
/// reserves under them.
///
/// The prologue and the epilogue are one value so that they cannot come apart.
/// A `push` whose `pop` was forgotten, or a `sub rsp` whose `add` was, leaves
/// `rsp` wrong for whoever runs next — a crash a long way from its cause, and
/// the kind of mistake that only shows up in the routine nobody exercised.
///
/// The alignment rule is checked here too, once, instead of being re-derived at
/// every routine: a routine is reached by `call`, so `rsp % 16 == 8` on
/// arrival, each push takes eight more, and what is reserved has to bring the
/// total back to a multiple of sixteen before this routine calls anything of
/// its own.
struct StubFrame {
    saved: &'static [&'static str],
    reserved: u32,
}

impl StubFrame {
    /// Push `saved` in order and reserve `reserved` bytes under them. `note`
    /// says what the reserved bytes are for, beside the `sub`.
    fn enter(
        asm: &mut Asm,
        saved: &'static [&'static str],
        reserved: u32,
        note: &str,
    ) -> StubFrame {
        assert_eq!(
            (8 + 8 * saved.len() as u32 + reserved) % 16,
            0,
            "a runtime routine's frame has to leave rsp aligned for the calls it makes"
        );
        for register in saved {
            asm.asm(&format!("push {register}"));
        }
        asm.asm(&format!("sub  rsp, {reserved}    ; {note}"));
        StubFrame { saved, reserved }
    }

    /// Give the frame back and return — the exact reverse of [`Self::enter`],
    /// which is why the two are never written out by hand.
    fn ret(&self, asm: &mut Asm) {
        asm.asm(&format!("add  rsp, {}", self.reserved));
        for register in self.saved.iter().rev() {
            asm.asm(&format!("pop  {register}"));
        }
        asm.asm("ret");
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
    if used.allocates() {
        // The arena's chunks. Nothing is ever given back, so there is no `free`
        // to declare — see `arena`.
        asm.line("extern malloc");
    }
    if used.writes_text() {
        asm.line("extern SetConsoleOutputCP");
    }
    if used.reads_text() {
        // The input is buffered here rather than through a `FILE*`, so what is
        // needed is the low-level read and nothing else.
        asm.line("extern _read");
        asm.line(&format!("extern {CONSOLE_INPUT_CP}"));
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
    // One method table per class that a `New` builds. Its entries are settled
    // at compile time — a subclass's table is its base's with the overridden
    // slots replaced — so dispatch is a load and an indirect call, and there is
    // nothing to install at startup.
    for (index, slots) in program.vtables.iter().enumerate() {
        if !used.vtables[index] {
            continue;
        }
        let entries: Vec<String> = slots
            .iter()
            .map(|&at| symbol(&program.function(at).name))
            .collect();
        let label = vtable_label(ClassId(index as u32));
        match entries.is_empty() {
            // NASM needs something to put there, and nothing will read it.
            true => asm.asm(&format!("{label}: dq 0    ; {} has no methods", label)),
            false => asm.asm(&format!("{label}: dq {}", entries.join(", "))),
        }
    }
    // One table per printed enum: the variant names, and the array of pointers
    // a tag indexes. A tag can only have come from a variant of this very enum,
    // so the index is in range by construction and needs no check.
    for (index, info) in program.table.enums.iter().enumerate() {
        if !used.enums[index] {
            continue;
        }
        let id = EnumId(index as u32);
        for (tag, variant) in info.variants.iter().enumerate() {
            asm.asm(&format!(
                "{}: db {}, 0",
                enum_variant_text(id, tag),
                bytes_of(variant)
            ));
        }
        let entries: Vec<String> =
            (0..info.variants.len()).map(|tag| enum_variant_text(id, tag)).collect();
        asm.asm(&format!("{}: dq {}", enum_table(id), entries.join(", ")));
    }
    if used.aborts {
        // No NUL: `_write` is given a length, not a C string.
        for abort in &ABORTS {
            asm.asm(&format!("{}: db {}", abort.message(), bytes_of(&abort.text())));
        }
    }
    // A literal is laid out exactly as a string built at run time is: the
    // character count in the eight bytes before the characters, and the
    // characters four bytes each. That is the whole reason nothing downstream
    // has to know which kind of string it holds.
    for (index, chars) in program.strings.iter().enumerate() {
        let text = chars.iter().collect::<String>().escape_debug().to_string();
        asm.asm("align 8");
        asm.asm(&format!("str{index}_len: dq {}", chars.len()));
        match chars.is_empty() {
            // Nothing may follow the label, and nothing will read it either:
            // its length says there is no character to fetch.
            true => asm.asm(&format!("str{index}:    ; \"\"")),
            false => {
                let values: Vec<String> =
                    chars.iter().map(|c| (*c as u32).to_string()).collect();
                asm.asm(&format!("str{index}: dd {}    ; \"{text}\"", values.join(", ")));
            }
        }
    }
    asm.blank();

    // Uninitialised, so the arena's bookkeeping costs nothing in the file: a
    // next pointer and an end pointer that start equal at zero, which is what
    // sends the very first allocation to ask for a chunk.
    if used.allocates() || used.print_str || used.reads_text() {
        asm.line("section .bss");
        if used.allocates() {
            asm.asm(&format!("{ARENA_NEXT}: resq 1"));
            asm.asm(&format!("{ARENA_END}: resq 1"));
        }
        if used.print_str {
            asm.asm(&format!("{SCRATCH}: resq 1"));
            asm.asm(&format!("{SCRATCH_CAP}: resq 1"));
        }
        // The input buffer, and how much of it has been read. Both start at
        // zero, which is what sends the first question to the operating system.
        if used.reads_text() {
            asm.asm(&format!("{INPUT}: resb {INPUT_BYTES}"));
            asm.asm(&format!("{INPUT_POS}: resq 1"));
            asm.asm(&format!("{INPUT_LEN}: resq 1"));
        }
        asm.blank();
    }
}

fn bytes_of(text: &str) -> String {
    text.bytes().map(|b| b.to_string()).collect::<Vec<_>>().join(", ")
}

/// The out-of-line ends every failing operation jumps to.
///
/// They are reached by `jmp`, not `call`, so `rsp` on arrival is whatever the
/// failing function happened to be using. Rather than oblige every one of those
/// functions to keep a frame a call could be made from — which, now that any
/// addition can fail, would be nearly all of them — the report builds its own
/// out of thin air. It can: it never returns, so `rsp` is not worth preserving.
fn abort_stubs(asm: &mut Asm) {
    asm.blank();
    asm.comment("runtime failures: report on stderr, then leave with a non-zero status");

    for (at, abort) in ABORTS.iter().enumerate() {
        asm.line(&format!("{}:", abort.label));
        asm.asm(&format!("lea  {RDX}, [{}]", abort.message()));
        asm.asm(&format!("mov  r8d, {}", abort.text().len()));
        // The last one falls straight through into the report rather than
        // jumping to the instruction after itself.
        if at + 1 < ABORTS.len() {
            asm.asm(&format!("jmp  {ABORT_REPORT}"));
        }
    }

    asm.line(&format!("{ABORT_REPORT}:"));
    asm.comment("a frame of its own, so nothing that jumps here owes one");
    // `and` forces the alignment a `call` needs whatever the jumper's `rsp`
    // was, and `sub` buys the shadow space. Both destroy `rsp`, which costs
    // nothing at all: this routine never returns.
    asm.asm("and  rsp, -16");
    asm.asm(&format!("sub  rsp, {SHADOW_SPACE}"));
    asm.comment("_write(2, message, length), then exit(1)");
    asm.asm(&format!("mov  {RCX}, {STDERR}"));
    asm.asm("call _write");
    asm.asm(&format!("mov  {RCX}, 1"));
    asm.asm("call exit");
}

/// The arena: where every string the program builds lives.
///
/// A bump pointer through chunks asked of `malloc`, and **nothing is ever given
/// back**. That is not an omission but the design: a string built here may be
/// returned, stored, and passed on, so its address travels outward — which the
/// rest of the language never lets an address do, precisely because an address
/// that outlives what it points at is the one mistake no type checker here
/// would catch. Memory that is never freed cannot dangle, so the question does
/// not arise, and no lifetimes, no reference counts and no collector are
/// needed to answer it.
///
/// What it costs is the memory a long-running program stops using and never
/// reclaims. TinyC programs run and finish, so the trade is a good one — and it
/// is the *whole* reason strings can be values here.
fn arena(asm: &mut Asm) {
    asm.blank();
    asm.comment("the arena: a bump pointer through chunks, and nothing is ever freed");
    asm.line(&format!("{ALLOC}:"));
    asm.comment("rcx = bytes wanted -> rax = their address");
    let frame = StubFrame::enter(asm, &["rbx", "rsi"], 40, "shadow space + alignment");
    asm.comment("round up, so every block starts 16-aligned like malloc's own");
    asm.asm("lea  rbx, [rcx+15]");
    asm.asm("and  rbx, -16");
    asm.asm(&format!("mov  {RAX}, [{ARENA_NEXT}]"));
    asm.asm(&format!("lea  {RDX}, [{RAX}+rbx]"));
    asm.asm(&format!("cmp  {RDX}, [{ARENA_END}]"));
    asm.asm("ja   .refill");
    asm.asm(&format!("mov  [{ARENA_NEXT}], {RDX}"));
    frame.ret(asm);

    asm.line(".refill:");
    asm.comment("what is left of the old chunk is abandoned: nothing here frees");
    asm.asm(&format!("mov  rsi, {CHUNK_BYTES}"));
    asm.asm("cmp  rsi, rbx");
    asm.asm("jae  .big_enough");
    asm.comment("a single string can be bigger than a chunk, so ask for it whole");
    asm.asm("mov  rsi, rbx");
    asm.line(".big_enough:");
    asm.asm(&format!("mov  {RCX}, rsi"));
    asm.asm("call malloc");
    asm.asm(&format!("test {RAX}, {RAX}"));
    asm.asm(&format!("jz   {ABORT_OOM}"));
    asm.asm(&format!("lea  {RDX}, [{RAX}+rsi]"));
    asm.asm(&format!("mov  [{ARENA_END}], {RDX}"));
    asm.asm(&format!("lea  {RDX}, [{RAX}+rbx]"));
    asm.asm(&format!("mov  [{ARENA_NEXT}], {RDX}"));
    frame.ret(asm);
}

/// The routines a string's operators become.
///
/// Each is here because it is a *loop*. Everything a string does in a straight
/// line — its length, one of its characters — is an instruction the backend
/// emits inline; only the operations that have to walk the characters are worth
/// a call.
fn string_stubs(asm: &mut Asm, used: &Used) {
    if used.concat {
        asm.blank();
        asm.comment("a + b: a new string in the arena, holding a's characters then b's");
        asm.line(&format!("{}:", runtime_symbol(Runtime::Concat)));
        asm.comment("rcx = a, rdx = b -> rax = the joined string");
        let frame =

            StubFrame::enter(asm, &["rbx", "rsi", "rdi", "r12"], 40, "shadow space + alignment");
        asm.asm(&format!("mov  rsi, {RCX}"));
        asm.asm(&format!("mov  rdi, {RDX}"));
        asm.asm("mov  rbx, [rsi-8]    ; how many characters a has");
        asm.asm("mov  r12, [rdi-8]");
        asm.asm(&format!("lea  {RCX}, [rbx+r12]"));
        asm.asm(&format!(
            "lea  {RCX}, [{RCX}*{CHAR_BYTES}+{STR_HEADER}]    ; the header, then four bytes each"
        ));
        asm.asm(&format!("call {ALLOC}"));
        asm.asm(&format!("lea  {RDX}, [rbx+r12]"));
        asm.asm(&format!("mov  [{RAX}], {RDX}    ; the count goes in front"));
        asm.asm(&format!("add  {RAX}, 8    ; and the value is where the characters start"));
        asm.asm(&format!("mov  {RDX}, {RAX}"));
        asm.asm(&format!("mov  {RCX}, rbx"));
        asm.line(".left:");
        asm.asm(&format!("test {RCX}, {RCX}"));
        asm.asm("jz   .right_start");
        asm.asm("mov  r8d, [rsi]");
        asm.asm(&format!("mov  [{RDX}], r8d"));
        asm.asm("add  rsi, 4");
        asm.asm(&format!("add  {RDX}, 4"));
        asm.asm(&format!("dec  {RCX}"));
        asm.asm("jmp  .left");
        asm.line(".right_start:");
        asm.asm(&format!("mov  {RCX}, r12"));
        asm.line(".right:");
        asm.asm(&format!("test {RCX}, {RCX}"));
        asm.asm("jz   .done");
        asm.asm("mov  r8d, [rdi]");
        asm.asm(&format!("mov  [{RDX}], r8d"));
        asm.asm("add  rdi, 4");
        asm.asm(&format!("add  {RDX}, 4"));
        asm.asm(&format!("dec  {RCX}"));
        asm.asm("jmp  .right");
        asm.line(".done:");
        frame.ret(asm);
    }

    if used.str_eq {
        asm.blank();
        asm.comment("a == b: the same length and the same characters, never the same address");
        asm.line(&format!("{}:", runtime_symbol(Runtime::StrEq)));
        asm.comment("rcx = a, rdx = b -> rax = 1 when they hold the same characters");
        asm.asm(&format!("mov  {RAX}, [{RCX}-8]"));
        asm.asm(&format!("cmp  {RAX}, [{RDX}-8]"));
        asm.comment("different lengths settle it without looking at a character");
        asm.asm("jne  .differ");
        asm.asm(&format!("mov  r8, {RAX}"));
        asm.line(".next:");
        asm.asm("test r8, r8");
        asm.asm("jz   .same");
        asm.asm(&format!("mov  r9d, [{RCX}]"));
        asm.asm(&format!("cmp  r9d, [{RDX}]"));
        asm.asm("jne  .differ");
        asm.asm(&format!("add  {RCX}, 4"));
        asm.asm(&format!("add  {RDX}, 4"));
        asm.asm("dec  r8");
        asm.asm("jmp  .next");
        asm.line(".same:");
        asm.asm("mov  eax, 1");
        asm.asm("ret");
        asm.line(".differ:");
        asm.asm("xor  eax, eax");
        asm.asm("ret");
    }

    if used.check_char {
        asm.blank();
        asm.comment("char(n): n itself, unless n names no character");
        asm.line(&format!("{}:", runtime_symbol(Runtime::CheckChar)));
        asm.comment("rcx = n -> rax = n");
        asm.asm(&format!("mov  {RAX}, {RCX}"));
        asm.comment("unsigned, so a negative n is enormous and fails the same test");
        asm.asm(&format!("cmp  {RCX}, 0x10FFFF"));
        asm.asm(&format!("ja   {ABORT_BAD_CHAR}"));
        asm.comment("the surrogate block in the middle names nothing either");
        asm.asm(&format!("mov  {RDX}, {RCX}"));
        asm.asm(&format!("sub  {RDX}, 0xD800"));
        asm.asm(&format!("cmp  {RDX}, 0x7FF"));
        asm.asm(&format!("jbe  {ABORT_BAD_CHAR}"));
        asm.asm("ret");
    }

    if used.char_str {
        asm.blank();
        asm.comment("string(c): a string of exactly one character");
        asm.line(&format!("{}:", runtime_symbol(Runtime::CharToStr)));
        asm.comment("rcx = the character -> rax = the string");
        let frame = StubFrame::enter(asm, &["rbx"], 32, "shadow space");
        asm.asm(&format!("mov  rbx, {RCX}"));
        asm.asm(&format!(
            "mov  {RCX}, {}    ; the count, then the one character",
            STR_HEADER + CHAR_BYTES
        ));
        asm.asm(&format!("call {ALLOC}"));
        asm.asm(&format!("mov  qword [{RAX}], 1"));
        asm.asm(&format!("add  {RAX}, 8"));
        asm.asm(&format!("mov  [{RAX}], ebx"));
        frame.ret(asm);
    }

    if used.int_str {
        asm.blank();
        asm.comment("string(n): the number written out in decimal");
        asm.line(&format!("{}:", runtime_symbol(Runtime::IntToStr)));
        asm.comment("rcx = n -> rax = its text");
        let saved: &[&str] = &["rbx", "rsi", "rdi", "r12"];
        let frame = StubFrame::enter(asm, saved, 72, "shadow space + room for the digits");
        asm.asm("xor  r12, r12    ; how many digits so far");
        asm.asm("lea  rdi, [rsp+32]");
        asm.asm(&format!("mov  rbx, {RCX}    ; keep n, to read its sign back"));
        asm.asm(&format!("mov  {RAX}, {RCX}"));
        asm.comment("work on the negative side: the most negative int has no positive twin");
        asm.asm(&format!("test {RAX}, {RAX}"));
        asm.asm("js   .digits");
        asm.asm(&format!("neg  {RAX}"));
        asm.line(".digits:");
        asm.asm("mov  rsi, 10");
        asm.line(".next:");
        asm.asm("cqo");
        asm.asm("idiv rsi");
        asm.comment("the remainder carries the dividend's sign, so it comes back up");
        asm.asm(&format!("neg  {RDX}"));
        asm.asm(&format!("add  {RDX}, 48    ; '0'"));
        asm.asm("mov  [rdi+r12], dl");
        asm.asm("inc  r12");
        asm.asm(&format!("test {RAX}, {RAX}"));
        asm.asm("jnz  .next");
        asm.asm("test rbx, rbx");
        asm.asm("jns  .unsigned");
        asm.asm("mov  byte [rdi+r12], 45    ; '-'");
        asm.asm("inc  r12");
        asm.line(".unsigned:");
        asm.asm(&format!("lea  {RCX}, [r12*{CHAR_BYTES}+{STR_HEADER}]"));
        asm.asm(&format!("call {ALLOC}"));
        asm.asm(&format!("mov  [{RAX}], r12"));
        asm.asm(&format!("add  {RAX}, 8"));
        asm.comment("the digits came out backwards, so they go back front to back");
        asm.asm(&format!("mov  {RCX}, r12"));
        asm.asm(&format!("mov  {RDX}, {RAX}"));
        asm.line(".copy:");
        asm.asm(&format!("dec  {RCX}"));
        asm.asm(&format!("movzx r8d, byte [rdi+{RCX}]"));
        asm.asm(&format!("mov  [{RDX}], r8d"));
        asm.asm(&format!("add  {RDX}, 4"));
        asm.asm(&format!("test {RCX}, {RCX}"));
        asm.asm("jnz  .copy");
        frame.ret(asm);
    }
}

/// The routines a list needs, on the same arena the strings use.
///
/// A list is laid out as a capacity, then a length, then its elements — and the
/// *value* is the address of the elements, so `[p-8]` is the length exactly as
/// it is for a string. That is not a coincidence but the point: `len` is one
/// instruction and never asks which of the two it was handed.
///
/// ```text
/// [ capacity : 8 ][ length : 8 ][ elements, `bytes` each ]
///                               ^ this is the value
/// ```
///
/// Both counts are in *elements*, and how wide one is comes in as an argument:
/// a list holds its elements where it is, so a list of objects holds whole
/// objects. Every width is a multiple of eight — everything that fits in a
/// register is eight, and an object's storage is a sum of those — which is why
/// every copy below walks words rather than bytes.
fn list_stubs(asm: &mut Asm, used: &Used) {
    if used.list_new {
        asm.blank();
        asm.comment("room for n elements, with the length already set");
        asm.line(&format!("{}:", runtime_symbol(Runtime::ListNew)));
        asm.comment("rcx = how many elements, rdx = bytes each -> rax = the list");
        let frame = StubFrame::enter(asm, &["rbx", "rsi", "rdi"], 32, "shadow space + alignment");
        asm.asm(&format!("mov  rsi, {RCX}    ; the length"));
        asm.asm(&format!("mov  rdi, {RDX}    ; bytes per element"));
        asm.asm(&format!("mov  rbx, {RCX}"));
        asm.comment("never less than four, so the first few pushes need no move");
        asm.asm(&format!("cmp  rbx, {LIST_MIN_CAPACITY}"));
        asm.asm("jae  .big_enough");
        asm.asm(&format!("mov  rbx, {LIST_MIN_CAPACITY}"));
        asm.line(".big_enough:");
        asm.asm(&format!("mov  {RCX}, rbx"));
        asm.asm(&format!("imul {RCX}, rdi"));
        asm.asm(&format!("add  {RCX}, 16    ; the two counts, then the elements"));
        asm.asm(&format!("call {ALLOC}"));
        asm.asm(&format!("mov  [{RAX}], rbx    ; capacity"));
        asm.asm(&format!("mov  [{RAX}+8], rsi    ; length"));
        asm.asm(&format!("add  {RAX}, 16    ; and the value is where the elements start"));
        frame.ret(asm);
    }

    // One routine makes the room; the two pushes differ only in what they put
    // in it. Growing is the whole of the difficulty, and it happens once.
    if used.list_room() {
        asm.blank();
        asm.comment("one more element's worth of room, and where it goes");
        asm.line(&format!("{LIST_ROOM}:"));
        asm.comment("rcx = the list, rdx = bytes each -> rax = the list, rdx = the new slot");
        let frame =
            StubFrame::enter(asm, &["rbx", "rsi", "rdi", "r12"], 40, "shadow space + alignment");
        asm.asm(&format!("mov  rsi, {RCX}"));
        asm.asm(&format!("mov  r12, {RDX}    ; bytes per element"));
        asm.asm("mov  rbx, [rsi-8]    ; length");
        asm.asm("mov  rdi, [rsi-16]   ; capacity");
        asm.asm("cmp  rbx, rdi");
        asm.asm("jb   .room");
        asm.comment("full: twice the room, and the old block is left where it is");
        asm.asm("lea  rdi, [rdi*2]");
        asm.asm(&format!("mov  {RCX}, rdi"));
        asm.asm(&format!("imul {RCX}, r12"));
        asm.asm(&format!("add  {RCX}, 16"));
        asm.asm(&format!("call {ALLOC}"));
        asm.asm(&format!("mov  [{RAX}], rdi"));
        asm.asm(&format!("add  {RAX}, 16"));
        asm.comment("the elements, as words: every width is a multiple of eight");
        asm.asm(&format!("mov  {RCX}, rbx"));
        asm.asm(&format!("imul {RCX}, r12"));
        asm.asm(&format!("shr  {RCX}, 3"));
        asm.asm(&format!("xor  {RDX}, {RDX}"));
        asm.line(".copy:");
        asm.asm(&format!("cmp  {RDX}, {RCX}"));
        asm.asm("jae  .copied");
        asm.asm(&format!("mov  r8, [rsi+{RDX}*8]"));
        asm.asm(&format!("mov  [{RAX}+{RDX}*8], r8"));
        asm.asm(&format!("inc  {RDX}"));
        asm.asm("jmp  .copy");
        asm.line(".copied:");
        asm.asm(&format!("mov  rsi, {RAX}"));
        asm.line(".room:");
        asm.comment("the new element goes where the ones before it stop");
        asm.asm(&format!("mov  {RAX}, rbx"));
        asm.asm(&format!("imul {RAX}, r12"));
        asm.asm(&format!("lea  {RDX}, [rsi+{RAX}]"));
        asm.asm("inc  rbx");
        asm.asm("mov  [rsi-8], rbx    ; the length lives with the elements");
        asm.asm(&format!("mov  {RAX}, rsi"));
        frame.ret(asm);
    }

    if used.list_push {
        asm.blank();
        asm.comment("one more element, answering where the list ended up");
        asm.line(&format!("{}:", runtime_symbol(Runtime::ListPush)));
        asm.comment("rcx = the list, rdx = the value -> rax = the list, possibly moved");
        let frame = StubFrame::enter(asm, &["rsi"], 32, "shadow space + alignment");
        asm.asm(&format!("mov  rsi, {RDX}    ; the value, over the call"));
        asm.asm(&format!("mov  {RDX}, 8      ; what fits in a register is eight bytes"));
        asm.asm(&format!("call {LIST_ROOM}"));
        asm.asm(&format!("mov  [{RDX}], rsi"));
        frame.ret(asm);
    }

    if used.list_push_big {
        asm.blank();
        asm.comment("the same, for an element that arrives as an address");
        asm.line(&format!("{}:", runtime_symbol(Runtime::ListPushBig)));
        asm.comment("rcx = the list, rdx = where the element is, r8 = its size");
        let frame = StubFrame::enter(asm, &["rbx", "rsi"], 40, "shadow space + alignment");
        asm.asm(&format!("mov  rsi, {RDX}    ; where it is now"));
        asm.asm("mov  rbx, r8       ; how much of it");
        asm.asm(&format!("mov  {RDX}, r8"));
        asm.asm(&format!("call {LIST_ROOM}"));
        asm.comment("the list may have moved out from under the source, and the block it");
        asm.comment("moved from is still there — the arena gives nothing back, so a push");
        asm.comment("of one of the list's own elements copies what it was handed");
        asm.asm("shr  rbx, 3");
        asm.asm(&format!("xor  {RCX}, {RCX}"));
        asm.line(".copy:");
        asm.asm(&format!("cmp  {RCX}, rbx"));
        asm.asm("jae  .copied");
        asm.asm(&format!("mov  r8, [rsi+{RCX}*8]"));
        asm.asm(&format!("mov  [{RDX}+{RCX}*8], r8"));
        asm.asm(&format!("inc  {RCX}"));
        asm.asm("jmp  .copy");
        asm.line(".copied:");
        frame.ret(asm);
    }

    // `int(s)` and `is_int(s)` are one routine asked two ways. They have to
    // agree about what a number is — down to whether a nineteen-digit one
    // fits — and the only way to be sure of that is for one of them to answer
    // the question for both.
    if used.parse_int() {
        asm.blank();
        asm.comment("the number a string spells, and whether it spells one at all");
        asm.line(&format!("{PARSE_INT}:"));
        asm.comment("rcx = the text -> rax = the number, rdx = 1 when there was one");
        let frame =
            StubFrame::enter(asm, &["rbx", "rsi", "rdi", "r12"], 40, "shadow space + alignment");
        asm.asm(&format!("mov  rsi, {RCX}"));
        asm.asm("mov  rbx, [rsi-8]    ; how many characters");
        asm.asm("xor  rdi, rdi        ; where we are");
        asm.asm("xor  r12, r12        ; was there a minus?");
        asm.asm("test rbx, rbx");
        asm.asm("jz   .no             ; the empty string spells nothing");
        asm.asm("mov  eax, [rsi]");
        asm.asm("cmp  eax, 45    ; '-'");
        asm.asm("jne  .digits");
        asm.asm("mov  r12, 1");
        asm.asm("inc  rdi");
        asm.asm("cmp  rdi, rbx");
        asm.asm("jae  .no             ; a minus on its own");
        asm.line(".digits:");
        asm.comment("built on the negative side, so the most negative int needs no special case");
        asm.asm(&format!("xor  {RAX}, {RAX}"));
        asm.line(".next:");
        asm.asm("cmp  rdi, rbx");
        asm.asm("jae  .complete");
        asm.asm("mov  ecx, [rsi+rdi*4]");
        asm.asm("sub  ecx, 48    ; '0'");
        asm.asm("cmp  ecx, 9");
        asm.asm("ja   .no             ; unsigned, so it catches both ends");
        asm.asm(&format!("imul {RAX}, {RAX}, 10"));
        asm.asm("jo   .no");
        asm.asm(&format!("movsxd {RDX}, ecx"));
        asm.asm(&format!("sub  {RAX}, {RDX}"));
        asm.asm("jo   .no");
        asm.asm("inc  rdi");
        asm.asm("jmp  .next");
        asm.line(".complete:");
        asm.asm("test r12, r12");
        asm.asm("jnz  .signed");
        asm.asm(&format!("neg  {RAX}"));
        asm.asm("jo   .no             ; only the one with no positive twin");
        asm.line(".signed:");
        asm.asm(&format!("mov  {RDX}, 1"));
        frame.ret(asm);
        asm.line(".no:");
        asm.comment("no number to hand back, and the caller decides what that means");
        asm.asm(&format!("xor  {RAX}, {RAX}"));
        asm.asm(&format!("xor  {RDX}, {RDX}"));
        frame.ret(asm);
    }

    if used.str_int {
        asm.blank();
        asm.comment("int(s): the number a string spells, and nothing else will do");
        asm.line(&format!("{}:", runtime_symbol(Runtime::StrToInt)));
        asm.comment("rcx = the text -> rax = the number");
        let frame = StubFrame::enter(asm, &[], 40, "shadow space + alignment");
        asm.asm(&format!("call {PARSE_INT}"));
        asm.asm(&format!("test {RDX}, {RDX}"));
        asm.asm(&format!("jz   {ABORT_NOT_A_NUMBER}"));
        frame.ret(asm);
    }

    if used.is_int {
        asm.blank();
        asm.comment("is_int(s): whether `int(s)` would answer rather than stop the program");
        asm.line(&format!("{}:", runtime_symbol(Runtime::IsInt)));
        asm.comment("rcx = the text -> rax = whether it spells a number");
        let frame = StubFrame::enter(asm, &[], 40, "shadow space + alignment");
        asm.asm(&format!("call {PARSE_INT}"));
        asm.asm(&format!("mov  {RAX}, {RDX}"));
        frame.ret(asm);
    }

    if used.chars_str {
        asm.blank();
        asm.comment("string(cs): a list of characters sealed into a string");
        asm.line(&format!("{}:", runtime_symbol(Runtime::CharsToStr)));
        asm.comment("rcx = a char[] -> rax = a string of the same characters");
        let frame = StubFrame::enter(asm, &["rbx", "rsi"], 40, "shadow space + alignment");
        asm.asm(&format!("mov  rsi, {RCX}"));
        asm.asm("mov  rbx, [rsi-8]");
        asm.asm(&format!("lea  {RCX}, [rbx*4+8]"));
        asm.asm(&format!("call {ALLOC}"));
        asm.asm(&format!("mov  [{RAX}], rbx"));
        asm.asm(&format!("add  {RAX}, 8"));
        asm.asm(&format!("xor  {RCX}, {RCX}"));
        asm.line(".copy:");
        asm.asm(&format!("cmp  {RCX}, rbx"));
        asm.asm("jae  .copied");
        asm.comment("eight bytes wide in a list, four in a string");
        asm.asm(&format!("mov  edx, [rsi+{RCX}*8]"));
        asm.asm(&format!("mov  [{RAX}+{RCX}*4], edx"));
        asm.asm(&format!("inc  {RCX}"));
        asm.asm("jmp  .copy");
        asm.line(".copied:");
        frame.ret(asm);
    }

    if used.list_clone {
        asm.blank();
        asm.comment("a second list holding the same elements — what assigning one costs");
        asm.line(&format!("{}:", runtime_symbol(Runtime::ListClone)));
        asm.comment("rcx = the list, rdx = bytes each -> rax = a list of its own");
        let frame = StubFrame::enter(asm, &["rbx", "rsi", "rdi"], 32, "shadow space + alignment");
        asm.asm(&format!("mov  rsi, {RCX}"));
        asm.asm(&format!("mov  rdi, {RDX}    ; bytes per element"));
        asm.asm("mov  rbx, [rsi-8]");
        asm.asm(&format!("mov  {RCX}, rbx"));
        asm.asm(&format!("mov  {RDX}, rdi"));
        asm.asm(&format!("call {}", runtime_symbol(Runtime::ListNew)));
        asm.comment("as words, so an element wider than a register costs no more code");
        asm.asm(&format!("mov  {RCX}, rbx"));
        asm.asm(&format!("imul {RCX}, rdi"));
        asm.asm(&format!("shr  {RCX}, 3"));
        asm.asm(&format!("xor  {RDX}, {RDX}"));
        asm.line(".copy:");
        asm.asm(&format!("cmp  {RDX}, {RCX}"));
        asm.asm("jae  .copied");
        asm.asm(&format!("mov  r8, [rsi+{RDX}*8]"));
        asm.asm(&format!("mov  [{RAX}+{RDX}*8], r8"));
        asm.asm(&format!("inc  {RDX}"));
        asm.asm("jmp  .copy");
        asm.line(".copied:");
        frame.ret(asm);
    }
}

/// Writing text out: the one place the language's characters meet UTF-8.
///
/// A string is four bytes per character inside the program, because that is
/// what makes `s[i]` an address and not a walk. The outside world reads UTF-8,
/// so it is encoded here, at the boundary, and nowhere else.
fn text_stubs(asm: &mut Asm, used: &Used) {
    asm.blank();
    asm.comment("encode one character as UTF-8");
    asm.line(&format!("{UTF8}:"));
    asm.comment("ecx = character, rdx = where to put it -> rax = bytes written");
    asm.asm("cmp  ecx, 0x80");
    asm.asm("jae  .two");
    asm.asm(&format!("mov  [{RDX}], cl"));
    asm.asm("mov  eax, 1");
    asm.asm("ret");
    asm.line(".two:");
    asm.asm("cmp  ecx, 0x800");
    asm.asm("jae  .three");
    for (shift, mask, tag, at) in [(6, 0x1F, 0xC0, 0), (0, 0x3F, 0x80, 1)] {
        utf8_byte(asm, shift, mask, tag, at);
    }
    asm.asm("mov  eax, 2");
    asm.asm("ret");
    asm.line(".three:");
    asm.asm("cmp  ecx, 0x10000");
    asm.asm("jae  .four");
    for (shift, mask, tag, at) in [(12, 0x0F, 0xE0, 0), (6, 0x3F, 0x80, 1), (0, 0x3F, 0x80, 2)] {
        utf8_byte(asm, shift, mask, tag, at);
    }
    asm.asm("mov  eax, 3");
    asm.asm("ret");
    asm.line(".four:");
    for (shift, mask, tag, at) in
        [(18, 0x07, 0xF0, 0), (12, 0x3F, 0x80, 1), (6, 0x3F, 0x80, 2), (0, 0x3F, 0x80, 3)]
    {
        utf8_byte(asm, shift, mask, tag, at);
    }
    asm.asm("mov  eax, 4");
    asm.asm("ret");

    if used.print_str {
        asm.blank();
        asm.comment("print a string: encode the whole of it, then hand printf one C string");
        asm.line(&format!("{PRINT_STR}:"));
        asm.comment("rcx = the string");
        let frame = StubFrame::enter(asm, &["rbx", "rsi", "rdi"], 32, "shadow space");
        asm.asm(&format!("mov  rsi, {RCX}"));
        asm.asm(&format!("mov  rbx, [{RCX}-8]"));
        asm.comment("four bytes per character is the most UTF-8 can need, plus the NUL");
        asm.asm(&format!("lea  {RCX}, [rbx*4+1]"));
        asm.asm(&format!("cmp  {RCX}, [{SCRATCH_CAP}]"));
        asm.asm("jbe  .room");
        asm.comment("the buffer is kept between calls, so a loop of prints allocates once");
        asm.asm(&format!("call {ALLOC}"));
        asm.asm(&format!("mov  [{SCRATCH}], {RAX}"));
        asm.asm(&format!("lea  {RCX}, [rbx*4+1]"));
        asm.asm(&format!("mov  [{SCRATCH_CAP}], {RCX}"));
        asm.line(".room:");
        asm.asm(&format!("mov  rdi, [{SCRATCH}]"));
        asm.line(".next:");
        asm.asm("test rbx, rbx");
        asm.asm("jz   .done");
        asm.asm("mov  ecx, [rsi]");
        asm.asm(&format!("mov  {RDX}, rdi"));
        asm.asm(&format!("call {UTF8}"));
        asm.asm(&format!("add  rdi, {RAX}"));
        asm.asm("add  rsi, 4");
        asm.asm("dec  rbx");
        asm.asm("jmp  .next");
        asm.line(".done:");
        asm.asm("mov  byte [rdi], 0");
        asm.asm(&format!("lea  {RCX}, [{FMT_STR}]"));
        asm.asm(&format!("mov  {RDX}, [{SCRATCH}]"));
        asm.asm("call printf");
        frame.ret(asm);
    }

    if used.print_char {
        asm.blank();
        asm.comment("print one character: at most four bytes, so the frame is buffer enough");
        asm.line(&format!("{PRINT_CHAR}:"));
        asm.comment("rcx = the character");
        let frame =
            StubFrame::enter(asm, &[], 56, "shadow space + a five-byte buffer + alignment");
        asm.asm(&format!("lea  {RDX}, [rsp+32]"));
        asm.asm(&format!("call {UTF8}"));
        asm.asm(&format!("mov  byte [rsp+{RAX}+32], 0"));
        asm.asm(&format!("lea  {RCX}, [{FMT_STR}]"));
        asm.asm(&format!("lea  {RDX}, [rsp+32]"));
        asm.asm("call printf");
        frame.ret(asm);
    }
}

/// Reading input: the other edge, where UTF-8 becomes characters.
///
/// The compiler buffers stdin itself, in [`INPUT`], rather than going through a
/// `FILE*`. That is what makes `eof` answerable at all without pushing a
/// character back: "has the input run out" becomes a question about this
/// buffer, and the answer costs nothing when the buffer is not empty.
fn input_stubs(asm: &mut Asm, used: &Used) {
    asm.blank();
    asm.comment("is a byte waiting? refills the buffer when it has run dry");
    asm.line(&format!("{READY}:"));
    asm.comment("-> rax = 1 when there is a byte to take, 0 at the end of the input");
    asm.asm(&format!("mov  {RAX}, [{INPUT_POS}]"));
    asm.asm(&format!("cmp  {RAX}, [{INPUT_LEN}]"));
    asm.asm("jb   .waiting");
    asm.asm("sub  rsp, 40    ; shadow space + alignment");
    asm.asm("mov  ecx, 0    ; the input");
    asm.asm(&format!("lea  {RDX}, [{INPUT}]"));
    asm.asm(&format!("mov  r8d, {INPUT_BYTES}"));
    asm.asm("call _read");
    asm.asm("add  rsp, 40");
    asm.comment("nothing left is an answer; a refusal is not");
    asm.asm("test eax, eax");
    asm.asm(&format!("js   {ABORT_INPUT_FAILED}"));
    asm.asm("jz   .spent");
    asm.comment("a 32-bit result leaves the top half undefined, so widen it");
    asm.asm(&format!("movsxd {RAX}, eax"));
    asm.asm(&format!("mov  [{INPUT_LEN}], {RAX}"));
    asm.asm(&format!("mov  qword [{INPUT_POS}], 0"));
    asm.line(".waiting:");
    asm.asm("mov  eax, 1");
    asm.asm("ret");
    asm.line(".spent:");
    asm.asm("xor  eax, eax");
    asm.asm("ret");

    asm.blank();
    asm.comment("take one byte");
    asm.line(&format!("{NEXT_BYTE}:"));
    asm.comment("-> rax = the byte, or -1 at the end of the input");
    asm.asm("sub  rsp, 40    ; shadow space + alignment");
    asm.asm(&format!("call {READY}"));
    asm.asm("add  rsp, 40");
    asm.asm(&format!("test {RAX}, {RAX}"));
    asm.asm("jz   .spent");
    asm.asm(&format!("mov  {RAX}, [{INPUT_POS}]"));
    asm.asm(&format!("lea  {RCX}, [{INPUT}]"));
    asm.asm(&format!("movzx edx, byte [{RCX}+{RAX}]"));
    asm.asm(&format!("inc  {RAX}"));
    asm.asm(&format!("mov  [{INPUT_POS}], {RAX}"));
    asm.asm(&format!("mov  {RAX}, {RDX}"));
    asm.asm("ret");
    asm.line(".spent:");
    asm.asm(&format!("mov  {RAX}, -1"));
    asm.asm("ret");

    if used.eof {
        asm.blank();
        asm.comment("eof(): the same question, asked the other way round");
        asm.line(&format!("{}:", runtime_symbol(Runtime::Eof)));
        asm.asm("sub  rsp, 40    ; shadow space + alignment");
        asm.asm(&format!("call {READY}"));
        asm.asm("add  rsp, 40");
        asm.comment("nothing was consumed, which is the whole point of asking");
        asm.asm(&format!("xor  {RAX}, 1"));
        asm.asm("ret");
    }

    if !used.read_line {
        return;
    }

    asm.blank();
    asm.comment("one character's worth of UTF-8, taken from the input");
    asm.line(&format!("{UTF8_DECODE}:"));
    asm.comment("rcx = the first byte -> rax = the character it starts");
    let frame = StubFrame::enter(asm, &["rbx", "rsi", "rdi"], 32, "shadow space");
    asm.asm("cmp  ecx, 0x80");
    asm.asm("jb   .plain");
    asm.asm(&format!("mov  rbx, {RCX}"));
    // Which of the three multi-byte forms this is, read off the lead byte.
    for (mask, tag, label) in [(0xE0, 0xC0, ".two"), (0xF0, 0xE0, ".three"), (0xF8, 0xF0, ".four")]
    {
        asm.asm("mov  eax, ecx");
        asm.asm(&format!("and  eax, {mask:#x}"));
        asm.asm(&format!("cmp  eax, {tag:#x}"));
        asm.asm(&format!("je   {label}"));
    }
    asm.comment("anything else is a byte no character starts with");
    asm.asm(&format!("jmp  {ABORT_BAD_UTF8}"));

    asm.line(".plain:");
    asm.asm(&format!("mov  {RAX}, {RCX}"));
    asm.asm("jmp  .decoded");

    // `rdi` counts the continuation bytes still owed, `rsi` is the smallest
    // value this many bytes is allowed to encode.
    for (label, mask, owed, least) in
        [(".two", 0x1F, 1, 0x80), (".three", 0x0F, 2, 0x800), (".four", 0x07, 3, 0x10000)]
    {
        asm.line(&format!("{label}:"));
        asm.asm(&format!("and  ebx, {mask:#x}"));
        asm.asm(&format!("mov  rdi, {owed}"));
        asm.asm(&format!("mov  rsi, {least:#x}"));
        if label != ".four" {
            asm.asm("jmp  .continuation");
        }
    }

    asm.line(".continuation:");
    asm.asm("test rdi, rdi");
    asm.asm("jz   .complete");
    asm.asm(&format!("call {NEXT_BYTE}"));
    asm.asm(&format!("cmp  {RAX}, 0"));
    asm.asm(&format!("jl   {ABORT_BAD_UTF8}    ; the input stopped mid-character"));
    asm.asm("mov  edx, eax");
    asm.asm("and  edx, 0xC0");
    asm.asm("cmp  edx, 0x80");
    asm.asm(&format!("jne  {ABORT_BAD_UTF8}"));
    asm.asm("and  eax, 0x3F");
    asm.asm("shl  rbx, 6");
    asm.asm(&format!("or   rbx, {RAX}"));
    asm.asm("dec  rdi");
    asm.asm("jmp  .continuation");

    asm.line(".complete:");
    asm.comment("an overlong encoding spells a character in more bytes than it needs");
    asm.asm("cmp  rbx, rsi");
    asm.asm(&format!("jb   {ABORT_BAD_UTF8}"));
    asm.asm("cmp  rbx, 0x10FFFF");
    asm.asm(&format!("ja   {ABORT_BAD_UTF8}"));
    asm.asm(&format!("mov  {RAX}, rbx"));
    asm.asm(&format!("sub  {RAX}, 0xD800"));
    asm.asm(&format!("cmp  {RAX}, 0x7FF"));
    asm.asm(&format!("jbe  {ABORT_BAD_UTF8}    ; a surrogate names no character"));
    asm.asm(&format!("mov  {RAX}, rbx"));
    asm.line(".decoded:");
    frame.ret(asm);

    asm.blank();
    asm.comment("read_line(): characters accumulate in a list, then become a string");
    asm.line(&format!("{}:", runtime_symbol(Runtime::ReadLine)));
    asm.comment("-> rax = the line, without its ending");
    let frame =

        StubFrame::enter(asm, &["rbx", "rsi", "rdi", "r12"], 40, "shadow space + alignment");
    asm.comment("the first byte decides whether there is a line at all");
    asm.asm(&format!("call {NEXT_BYTE}"));
    asm.asm(&format!("cmp  {RAX}, 0"));
    asm.asm(&format!("jl   {ABORT_NO_INPUT}"));
    asm.asm(&format!("mov  rsi, {RAX}"));
    asm.asm(&format!("xor  {RCX}, {RCX}"));
    asm.asm(&format!("mov  {RDX}, 8    ; characters are what fits in a register"));
    asm.asm(&format!("call {}", runtime_symbol(Runtime::ListNew)));
    asm.asm(&format!("mov  rbx, {RAX}"));

    asm.line(".next:");
    asm.asm("cmp  rsi, 10");
    asm.asm("je   .ended    ; a newline ends the line and is not part of it");
    asm.asm(&format!("mov  {RCX}, rsi"));
    asm.asm(&format!("call {UTF8_DECODE}"));
    asm.asm(&format!("mov  {RCX}, rbx"));
    asm.asm(&format!("mov  {RDX}, {RAX}"));
    asm.asm(&format!("call {}", runtime_symbol(Runtime::ListPush)));
    asm.asm(&format!("mov  rbx, {RAX}"));
    asm.asm(&format!("call {NEXT_BYTE}"));
    asm.asm(&format!("mov  rsi, {RAX}"));
    asm.asm("cmp  rsi, 0");
    asm.asm("jge  .next    ; the input running out ends the line too");

    asm.line(".ended:");
    asm.comment("a carriage return before the newline belongs to the ending");
    asm.asm(&format!("mov  {RCX}, [rbx-8]"));
    asm.asm(&format!("test {RCX}, {RCX}"));
    asm.asm("jz   .seal");
    asm.asm(&format!("mov  edx, [rbx+{RCX}*8-8]"));
    asm.asm("cmp  edx, 13");
    asm.asm("jne  .seal");
    asm.asm(&format!("dec  {RCX}"));
    asm.asm(&format!("mov  [rbx-8], {RCX}"));

    asm.line(".seal:");
    asm.asm(&format!("mov  {RCX}, rbx"));
    asm.asm(&format!("call {}", runtime_symbol(Runtime::CharsToStr)));
    frame.ret(asm);
}

/// One byte of a UTF-8 sequence: some bits of the character, under the tag that
/// says which byte of how many this is.
fn utf8_byte(asm: &mut Asm, shift: u32, mask: u32, tag: u32, at: u32) {
    asm.asm("mov  eax, ecx");
    if shift > 0 {
        asm.asm(&format!("shr  eax, {shift}"));
    }
    asm.asm(&format!("and  eax, {mask:#x}"));
    asm.asm(&format!("or   eax, {tag:#x}"));
    match at {
        0 => asm.asm(&format!("mov  [{RDX}], al")),
        _ => asm.asm(&format!("mov  [{RDX}+{at}], al")),
    }
}

/// The assembly name of one of the compiler's own routines.
fn runtime_symbol(callee: Runtime) -> String {
    format!("{RUNTIME_PREFIX}{}", callee.name())
}

/// The 32-bit half of a register, which is what reads a character out of a
/// string and, in doing so, widens it.
fn narrow(reg: &str) -> &'static str {
    let at = NAMES.iter().position(|&n| n == reg).expect("a name this backend gave a register");
    NAMES32[at]
}

// -- one function ----------------------------------------------------------

struct FnEmitter<'a, 'o> {
    program: &'a Program,
    function: &'a Function,
    allocation: &'a Allocation,
    registers: &'a RegisterFile,
    used: &'a Used,
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
        used: &'a Used,
        asm: &'o mut Asm,
    ) -> FnEmitter<'a, 'o> {
        // A leaf makes no call, so it owes neither shadow space nor alignment.
        // Jumping to a runtime abort does not count: that routine builds its
        // own frame, precisely so a function full of guarded arithmetic does
        // not have to carry one for a path it almost never takes.
        //
        // The entry point of a program that writes text is never a leaf: it
        // calls the console's code page into shape first, even when every
        // print happens somewhere else.
        let entry = function.name == ENTRY_POINT;
        let leaf = !(entry && (used.writes_text() || used.reads_text()))
            && !function
                .blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .any(|instr| instr.is_call());

        let frame = FrameLayout::new(allocation, function.frame_bytes, leaf);
        FnEmitter { program, function, allocation, registers, used, frame, asm, next_label: 0 }
    }

    fn run(&mut self) {
        self.asm.blank();
        for line in self.allocation.dump(self.function, self.registers).lines() {
            self.asm.comment(line);
        }
        self.asm.line(&format!("{}:", symbol(&self.function.name)));
        self.prologue();
        if self.is_entry_point() {
            // Both directions, and for the same reason: a console speaks a code
            // page, and unless it is told otherwise it is not this one. Without
            // this a program that prints `é` prints mojibake, and one that
            // reads it is handed bytes no character starts with.
            if self.used.writes_text() {
                self.asm.comment("tell the console the bytes it is about to receive are UTF-8");
                self.asm.asm(&format!("mov  ecx, {UTF8_CODE_PAGE}"));
                self.asm.asm("call SetConsoleOutputCP");
            }
            if self.used.reads_text() {
                self.asm.comment("and to hand over what is typed into it as UTF-8");
                self.asm.asm(&format!("mov  ecx, {UTF8_CODE_PAGE}"));
                self.asm.asm(&format!("call {CONSOLE_INPUT_CP}"));
            }
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
                let src = ARG_REGS[*index as usize];
                self.produce(*dst, |e, work| e.asm.mov(work, src));
            }
            // `cmp` sets the flags; `setcc` turns the one that matters into a
            // 0 or 1 byte, and `movzx` widens it to the 64-bit value a bool is.
            // A comparison that only feeds a branch never gets here — see
            // `fusable_compare`.
            Instr::Cmp { op, dst, lhs, rhs } => {
                self.compare(lhs, rhs);
                self.asm.asm(&format!("{} {SCRATCH1_8}", setcc(*op)));
                self.produce(*dst, |e, work| {
                    e.asm.asm(&format!("movzx {work}, {SCRATCH1_8}"));
                });
            }
            // One `idiv`, read from `rax` for the quotient or `rdx` for the
            // remainder.
            Instr::Bin { op: BinOp::Div, dst, lhs, rhs } => self.division(*dst, RAX, lhs, rhs),
            Instr::Bin { op: BinOp::Rem, dst, lhs, rhs } => self.division(*dst, RDX, lhs, rhs),
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
            // No source of these moves can be an argument register, because the
            // allocator is never given one — see the module docs.
            Instr::Call { dst, callee, args } => {
                for (index, arg) in args.iter().enumerate() {
                    let src = self.value(arg);
                    self.asm.mov(ARG_REGS[index], &src);
                }
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
                for (index, arg) in args.iter().enumerate() {
                    let src = self.value(arg);
                    self.asm.mov(ARG_REGS[index], &src);
                }
                self.asm.asm(&format!("call [{SCRATCH0}+{}]", slot * 8));
                self.take_result(*dst);
            }
            // The same call an ordinary one is, with a name only this module
            // can produce. Nothing about the sequence differs, which is the
            // point: a routine of the compiler's own is not a special form.
            Instr::RtCall { dst, callee, args } => {
                for (index, arg) in args.iter().enumerate() {
                    let src = self.value(arg);
                    self.asm.mov(ARG_REGS[index], &src);
                }
                self.asm.asm(&format!("call {}", runtime_symbol(*callee)));
                self.take_result(*dst);
            }
            // A string and a character are written by a routine rather than by
            // `printf`, because what they hold has to be encoded first. The
            // routine ends in the same `printf` all the same, so everything
            // still reaches one buffered stream and nothing interleaves.
            Instr::Print { ty: ty @ (Ty::Str | Ty::Char), val } => {
                let value = self.value(val);
                self.asm.mov(RCX, &value);
                let routine = match ty {
                    Ty::Str => PRINT_STR,
                    _ => PRINT_CHAR,
                };
                self.asm.asm(&format!("call {routine}"));
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
                    // The same lookup one step further: a bool picks between
                    // two names, an enum indexes a table of however many it has.
                    // The tag came from a variant of this enum and nothing else
                    // can produce one, so the index cannot be out of range.
                    Ty::Enum(id) => {
                        self.asm.mov(SCRATCH0, &value);
                        self.asm.asm(&format!("lea  {SCRATCH1}, [{}]", enum_table(*id)));
                        self.asm.asm(&format!("mov  {RDX}, [{SCRATCH1}+{SCRATCH0}*8]"));
                    }
                    _ => self.asm.mov(RDX, &value),
                }
                let format = match ty {
                    Ty::Int => FMT_INT,
                    Ty::Bool => FMT_BOOL,
                    // A string and a character left through the arm above.
                    _ => FMT_STR,
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
    fn produce_into(
        &mut self,
        dst: VReg,
        force_scratch: bool,
        emit: impl FnOnce(&mut Self, &str),
    ) {
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
struct FrameLayout {
    size: u32,
    /// Where the spill slots start, measured from `rsp`.
    slots_at: u32,
    /// Where this function's arrays start, just above the spill slots.
    arrays_at: u32,
}

impl FrameLayout {
    fn new(allocation: &Allocation, frame_bytes: u32, leaf: bool) -> FrameLayout {
        let slots_at = if leaf { 0 } else { SHADOW_SPACE };
        let arrays_at = slots_at + 8 * allocation.spill_slots;
        let locals = arrays_at + frame_bytes;
        if leaf {
            return FrameLayout { size: locals, slots_at, arrays_at };
        }

        let mut size = locals.div_ceil(16) * 16;
        if allocation.used_callee_saved.len().is_multiple_of(2) {
            size += 8;
        }
        FrameLayout { size, slots_at, arrays_at }
    }

    /// Spill slots sit just above the shadow space.
    fn slot_offset(&self, slot: u32) -> u32 {
        self.slots_at + 8 * slot
    }

    /// Arrays sit above the spill slots, which is what keeps their addresses
    /// stable: nothing between them and `rsp` moves for the life of the call.
    fn array_offset(&self, offset: u32) -> u32 {
        self.arrays_at + offset
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
                let frame = FrameLayout::new(&allocation, 0, false);
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
            let frame = FrameLayout::new(&allocation, 0, true);
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
    fn a_remainder_reads_rdx_where_a_division_reads_rax() {
        // One `idiv` produces both, so the two differ by a single `mov`.
        let quotient = compile("int a = 17;\nint b = 5;\nprint(a / b);");
        let remainder = compile("int a = 17;\nint b = 5;\nprint(a % b);");
        assert_eq!(quotient.matches("idiv").count(), 1, "{quotient}");
        assert_eq!(remainder.matches("idiv").count(), 1, "{remainder}");
        assert!(remainder.contains(", rdx"), "{remainder}");
        assert!(!remainder.contains(", rax"), "{remainder}");
    }

    #[test]
    fn a_remainder_carries_the_same_guards_a_division_does() {
        // Including the overflow one: `MIN % -1` is 0 on paper, but the `idiv`
        // that computes it still faults.
        let asm = compile("int a = 17;\nint b = 5;\nprint(a % b);");
        assert!(asm.contains(ABORT_DIV_ZERO), "{asm}");
        assert!(asm.contains(ABORT_DIV_OVERFLOW), "{asm}");
    }

    #[test]
    fn a_negated_condition_costs_nothing_at_all() {
        // `!(a < b)` is emitted as `a >= b`, so the only difference from the
        // un-negated form is which way the conditional jump goes.
        let plain = compile("int a = 1;\nint b = 2;\nif (a < b) {\n  print(1);\n}");
        let negated = compile("int a = 1;\nint b = 2;\nif (!(a < b)) {\n  print(1);\n}");
        assert_eq!(plain.matches("cmp  ").count(), negated.matches("cmp  ").count(), "{negated}");
        assert!(plain.contains("jge  ."), "{plain}");
        assert!(negated.contains("jl   ."), "{negated}");
        // Neither ever materialises the comparison as a 0 or a 1.
        assert!(!negated.contains("setl"), "{negated}");
    }

    #[test]
    fn negating_a_value_fuses_into_the_branch_that_reads_it() {
        // `!ok` lowers to `ok == 0`, which is the same fusable shape an `if`
        // already had: one `cmp`, one `jcc`, and no `setcc`.
        let asm = compile("int n = 1;\nbool ok = n > 0;\nif (!ok) {\n  print(1);\n}");
        assert!(asm.contains(", 0"), "{asm}");
        assert!(asm.contains("jne  ."), "{asm}");
        assert_eq!(asm.matches("sete").count(), 0, "{asm}");
    }

    #[test]
    fn a_short_circuit_costs_one_conditional_jump_either_way() {
        // The arm the branch continues into is laid out first, so it is reached
        // by falling through — for `&&` the right operand, for `||` the short
        // circuit. Getting the layout backwards would show up as a `jz` to the
        // very next block followed by a `jmp`.
        for (source, skipped) in [("x > 1 && x < 9", ".short2"), ("x > 1 || x < 9", ".rhs2")] {
            let asm = compile(&format!("int x = 5;\nbool ok = {source};\nprint(ok);"));
            // One `jcc` out of the entry block, and one `jmp` from the arm that
            // is not next to the join.
            assert_eq!(asm.matches("jmp  .join3").count(), 1, "{source}: {asm}");
            assert!(asm.contains(skipped), "{source}: {asm}");
        }
    }

    #[test]
    fn a_short_circuits_condition_is_folded_into_its_branch() {
        // The comparison feeding a short circuit is the same fusable shape as
        // an `if`'s: it never becomes a 0 or a 1 in a register.
        let asm = compile("int x = 5;\nbool ok = x > 1 && x < 9;\nprint(ok);");
        assert!(asm.contains("cmp  rbx, 1"), "{asm}");
        assert!(!asm.contains("setg"), "{asm}");
    }

    #[test]
    fn a_break_jumps_to_the_loops_exit_and_a_continue_to_its_step() {
        let asm = compile(
            "for (int i = 0; i < 9; i = i + 1) {\n  if (i == 2) {\n    continue;\n  }\n  \
             if (i == 5) {\n    break;\n  }\n  print(i);\n}",
        );
        // The step block exists because a `continue` needs one, and the back
        // edge leaves from it rather than from the body.
        assert!(asm.contains(".step"), "{asm}");
        assert!(asm.contains(".done"), "{asm}");
        assert!(asm.contains("jmp  .loop1"), "{asm}");
    }

    #[test]
    fn a_virtual_call_reads_the_object_then_jumps_through_its_table() {
        let asm = compile_src(
            "class Shape {\n  fn area(self) -> int { return 0; }\n}\n\
             class Circle : Shape {\n  int r;\n  fn area(self) -> int { return self.r; }\n}\n\
             fn report(Shape s) {\n  print(s.area());\n}\n\
             fn main() {\n  report(Circle { r: 1 });\n}",
        );
        // The table is settled at compile time; nothing installs it at startup.
        assert!(asm.contains("vtable1: dq tc$Circle$area"), "{asm}");
        // The vtable comes out of the object before the argument registers are
        // set up, since one of them is about to hold the receiver.
        assert!(asm.contains(&format!("mov  {SCRATCH0}, [{SCRATCH0}]")), "{asm}");
        assert!(asm.contains(&format!("call [{SCRATCH0}+0]")), "{asm}");
    }

    #[test]
    fn a_call_on_a_sealed_class_is_a_direct_one() {
        let asm = compile_src(
            "class Point {\n  int x;\n  fn get(self) -> int { return self.x; }\n}\n\
             fn main() {\n  Point p = Point { x: 1 };\n  print(p.get());\n}",
        );
        assert!(asm.contains("call tc$Point$get"), "{asm}");
        assert!(!asm.contains("call ["), "{asm}");
    }

    #[test]
    fn a_field_address_is_a_lea_with_no_check() {
        // Its place was settled by `sema`, so there is no question to ask.
        let asm = compile_src(
            "class Point {\n  int x;\n  int y;\n}\n\
             fn main() {\n  Point p = Point { x: 1, y: 2 };\n  print(p.y);\n}",
        );
        assert!(asm.contains("+16]"), "{asm}");
        assert!(!asm.contains(ABORT_BOUNDS), "{asm}");
    }

    #[test]
    fn a_class_nothing_builds_gets_no_table() {
        let asm = compile_src(
            "class Used {\n  fn f(self) -> int { return 1; }\n}\n\
             class Unused {\n  fn f(self) -> int { return 2; }\n}\n\
             fn main() {\n  Used u = Used { };\n  print(u.f());\n}",
        );
        assert!(!asm.contains("vtable1"), "{asm}");
    }

    #[test]
    fn an_element_address_is_a_single_lea() {
        // `base + index * 8` is an addressing mode on x86, so the multiply and
        // the add cost nothing at all.
        let asm = compile("int[3] xs = [1, 2, 3];\nint i = 1;\nprint(xs[i]);");
        assert!(asm.contains("*8]"), "{asm}");
        assert!(!asm.contains("imul"), "{asm}");
    }

    #[test]
    fn a_constant_index_folds_into_the_offset_and_carries_no_check() {
        // `sema` settled it, so what is left is the address arithmetic — and
        // even that is a constant the addressing mode absorbs.
        let asm = compile("int[3] xs = [1, 2, 3];\nprint(xs[2]);");
        assert!(asm.contains("+16]"), "{asm}");
        assert!(!asm.contains(ABORT_BOUNDS), "{asm}");
    }

    #[test]
    fn an_index_the_compiler_cannot_see_is_checked_with_one_comparison() {
        // Unsigned, so a negative index fails the same test that catches one
        // past the end: there is no second branch for the other side.
        let asm = compile("int[3] xs = [1, 2, 3];\nint i = 1;\nprint(xs[i]);");
        assert!(asm.contains("cmp  rdi, 3") || asm.contains(", 3"), "{asm}");
        assert!(asm.contains(&format!("jae  {ABORT_BOUNDS}")), "{asm}");
        assert_eq!(asm.matches(&format!("jae  {ABORT_BOUNDS}")).count(), 1, "{asm}");
    }

    #[test]
    fn an_array_gets_room_above_the_spill_slots() {
        let asm = compile("int[3] xs = [1, 2, 3];\nprint(xs[0]);");
        // `main` calls `printf`, so it owes shadow space; it spills nothing, so
        // the array starts immediately above.
        assert!(asm.contains(&format!("lea  rbx, [rsp+{SHADOW_SPACE}]")), "{asm}");
        // Shadow space plus three elements, rounded for alignment.
        assert!(asm.contains("sub  rsp, 72"), "{asm}");
    }

    #[test]
    fn printing_an_enum_indexes_a_table_of_its_variant_names() {
        // The same lookup a bool does, one step further: a tag indexes an array
        // of pointers instead of choosing between two.
        let asm = compile_src(
            "enum Colour { Red, Green, Blue }\nfn main() {\n  print(Colour::Green);\n}",
        );
        assert!(asm.contains("enum0_v1: db 71, 114, 101, 101, 110, 0"), "{asm}");
        assert!(asm.contains("enum0_names: dq enum0_v0, enum0_v1, enum0_v2"), "{asm}");
        assert!(asm.contains("lea  r11, [enum0_names]"), "{asm}");
        assert!(asm.contains("mov  rdx, [r11+r10*8]"), "{asm}");
        // It prints as a string, so it borrows that format rather than its own.
        assert!(asm.contains("lea  rcx, [fmt_str]"), "{asm}");
    }

    #[test]
    fn an_enum_that_is_never_printed_needs_no_table() {
        // Matching is arithmetic on the tag; it never asks what a tag is called.
        let asm = compile_src(
            "enum Colour { Red, Green }\nfn main() {\n  Colour c = Colour::Red;\n  \
             match (c) {\n    Colour::Red => { print(1); }\n    Colour::Green => { print(2); }\n  }\n}",
        );
        assert!(!asm.contains("enum0_names"), "{asm}");
        assert!(!asm.contains("enum0_v0"), "{asm}");
    }

    #[test]
    fn a_match_is_a_chain_of_compares_against_the_tag() {
        let asm = compile_src(
            "enum Colour { Red, Green, Blue }\nfn main() {\n  Colour c = Colour::Blue;\n  \
             match (c) {\n    Colour::Red => { print(1); }\n    Colour::Green => { print(2); }\n    \
             Colour::Blue => { print(3); }\n  }\n}",
        );
        // Two tests for three variants, each fused into its branch.
        assert_eq!(asm.matches("cmp  ").count(), 2, "{asm}");
        assert!(!asm.contains("sete"), "no comparison is ever materialised: {asm}");
    }

    #[test]
    fn a_literal_is_laid_out_like_a_string_built_at_run_time() {
        let asm = compile("string s = \"hi\";\nprint(s);");
        // The count first, then one four-byte character each — the same shape
        // the arena produces, so nothing downstream tells the two apart.
        assert!(asm.contains("str0_len: dq 2"), "{asm}");
        assert!(asm.contains("str0: dd 104, 105"), "{asm}");
    }

    #[test]
    fn a_literal_counts_characters_rather_than_bytes() {
        let asm = compile("string s = \"é\";\nprint(s);");
        assert!(asm.contains("str0_len: dq 1"), "{asm}");
        assert!(asm.contains("str0: dd 233"), "{asm}");
    }

    #[test]
    fn a_program_that_touches_no_string_gets_no_arena() {
        // Nothing unused is emitted, and the arena is the largest thing there
        // is to leave out: a program of pure arithmetic never calls `malloc`.
        let asm = compile("int x = 1;\nprint(x + 1);");
        assert!(!asm.contains("extern malloc"), "{asm}");
        assert!(!asm.contains(ALLOC), "{asm}");
        assert!(!asm.contains("SetConsoleOutputCP"), "{asm}");
    }

    #[test]
    fn printing_a_string_brings_the_arena_and_the_encoder_with_it() {
        // Printing encodes into a buffer cut from the arena, so even a program
        // that only prints a literal allocates — and can therefore run out of
        // memory, which is why the abort stubs come too.
        let asm = compile("print(\"hi\");");
        assert!(asm.contains("extern malloc"), "{asm}");
        assert!(asm.contains(&format!("{ABORT_OOM}:")), "{asm}");
        assert!(asm.contains(&format!("{UTF8}:")), "{asm}");
    }

    #[test]
    fn a_string_operator_that_is_not_used_is_not_emitted() {
        let asm = compile("print(\"a\" == \"b\");");
        assert!(asm.contains(&runtime_symbol(Runtime::StrEq)), "{asm}");
        assert!(!asm.contains(&runtime_symbol(Runtime::Concat)), "{asm}");
    }

    #[test]
    fn the_entry_point_sets_the_console_up_before_writing_any_text() {
        let asm = compile("print('a');");
        let main = asm.find("\nmain:").expect("an entry point");
        let setup = asm.find("call SetConsoleOutputCP").expect("the console setup");
        let print = asm.find(PRINT_CHAR).expect("the print routine");
        assert!(main < setup && setup < print, "{asm}");
    }

    #[test]
    fn cloning_a_list_brings_the_routine_that_builds_one_with_it() {
        // `list_clone` calls `list_new`, and no instruction says so — the kind
        // of dependency that only shows up when the assembler cannot find a
        // symbol, so it is asserted here instead.
        let asm = compile("int[] a = [1];\nint[] b = a;\nprint(len(b));");
        assert!(asm.contains(&format!("{}:", runtime_symbol(Runtime::ListClone))), "{asm}");
        assert!(asm.contains(&format!("{}:", runtime_symbol(Runtime::ListNew))), "{asm}");
    }

    #[test]
    fn reading_a_line_brings_everything_it_leans_on() {
        // `read_line` accumulates characters in a list and seals them into a
        // string, so it needs three routines no instruction names.
        let asm = compile("print(read_line());");
        for routine in [Runtime::ListNew, Runtime::ListPush, Runtime::CharsToStr] {
            assert!(asm.contains(&format!("{}:", runtime_symbol(routine))), "{routine:?}: {asm}");
        }
        assert!(asm.contains("extern _read"), "{asm}");
        assert!(asm.contains(&format!("{INPUT}:")), "{asm}");
    }

    #[test]
    fn converting_and_asking_first_are_one_routine() {
        // What makes `is_int(s)` trustworthy is that it is not a second opinion:
        // both go through the same parse, so neither can come to disagree about
        // what a number is. Either one alone brings it.
        for src in ["print(int(t(\"1\")));", "print(is_int(t(\"1\")));"] {
            let asm = compile_src(&format!(
                "fn t(string v) -> string {{\n  return v;\n}}\nfn main() {{\n{src}\n}}\n"
            ));
            assert!(asm.contains(&format!("{PARSE_INT}:")), "{asm}");
        }

        // And the abort belongs to the wrapper, not to the parse: the routine
        // that answers `is_int` has nowhere to stop the program.
        let asm = compile_src(
            "fn t(string v) -> string {\n  return v;\n}\n\
             fn main() {\nprint(is_int(t(\"1\")));\n}\n",
        );
        let parse = asm.split(&format!("{PARSE_INT}:")).nth(1).expect("the routine is emitted");
        let parse = parse.split(&runtime_symbol(Runtime::IsInt)).next().expect("something follows");
        assert!(!parse.contains(ABORT_NOT_A_NUMBER), "{parse}");
    }

    #[test]
    fn asking_whether_the_input_ran_out_needs_no_decoder() {
        // `eof` is a question about the buffer, so a program that only asks it
        // carries neither the decoder nor the list routines.
        let asm = compile("print(eof());");
        assert!(asm.contains(&format!("{READY}:")), "{asm}");
        assert!(!asm.contains(&format!("{UTF8_DECODE}:")), "{asm}");
        assert!(!asm.contains(&runtime_symbol(Runtime::ListPush)), "{asm}");
    }

    #[test]
    fn a_program_that_reads_nothing_carries_no_input_buffer() {
        let asm = compile("print(1);");
        assert!(!asm.contains("extern _read"), "{asm}");
        assert!(!asm.contains(INPUT), "{asm}");
        assert!(!asm.contains("SetConsoleCP"), "{asm}");
    }

    #[test]
    fn a_program_without_lists_carries_none_of_their_routines() {
        let asm = compile("int[3] a = [1, 2, 3];\nprint(a[0]);");
        for routine in [Runtime::ListNew, Runtime::ListPush, Runtime::ListClone] {
            assert!(!asm.contains(&runtime_symbol(routine)), "{routine:?} in {asm}");
        }
    }

    #[test]
    fn a_string_knows_its_own_length_without_being_told() {
        // One load, from behind the address the value holds. That is the whole
        // reason the count lives in front of the characters.
        let asm = compile("string s = \"abc\";\nprint(len(s));");
        assert!(asm.contains("-8]"), "{asm}");
        assert!(!asm.contains(&format!("call {}", runtime_symbol(Runtime::StrEq))), "{asm}");
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
    fn a_divisor_only_the_running_program_knows_is_guarded() {
        // A divisor this stage could see to be zero never reaches it — `sema`
        // evaluates constant arithmetic and rejects the program. What is left
        // is a value that has to be tested where it lands.
        let asm = compile_src(
            "fn zero() -> int {\n  return 0;\n}\nfn main() {\n  int n = 1;\n  print(n / zero());\n}",
        );
        assert!(asm.contains("idiv"), "{asm}");
        assert!(asm.contains(&format!("jz   {ABORT_DIV_ZERO}")), "{asm}");
    }

    #[test]
    fn a_function_that_can_abort_is_still_a_leaf() {
        // The abort routine is jumped to, not called, and builds its own frame
        // on arrival. So a function whose only way out to the runtime is a
        // failed check owes nothing — which matters now that every addition has
        // one.
        let asm = compile_src(
            "fn d(int a, int b) -> int {\n  return a / b + 1;\n}\nfn main() {\n  print(d(6, 3));\n}",
        );
        let (name, body) = functions_in(&asm)
            .into_iter()
            .find(|(name, _)| *name == "tc$d")
            .expect("the dividing function");
        assert!(body.contains("jo   "), "{name} should be guarded: {body}");
        assert!(body.contains(ABORT_DIV_ZERO), "{name} should be guarded: {body}");
        assert!(!body.contains("sub  rsp,"), "{name} should still be a leaf: {body}");
    }

    #[test]
    fn the_abort_routine_builds_the_frame_its_calls_need() {
        // It arrives by `jmp` with somebody else's `rsp`, so it aligns and
        // reserves shadow space itself. Destroying `rsp` is free: it exits.
        let asm = compile("int n = 1;\nprint(n + 1);");
        let (_, body) = functions_in(&asm)
            .into_iter()
            .find(|(name, _)| *name == ABORT_REPORT)
            .expect("the abort routine");
        assert!(body.contains("and  rsp, -16"), "{body}");
        assert!(body.contains(&format!("sub  rsp, {SHADOW_SPACE}")), "{body}");
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

    // -- the shape of a whole file ----------------------------------------
    //
    // Everything above compiles a fragment and looks for one instruction.
    // These four are about the file as a whole, over the checked-in examples:
    // the assertions that used to live in `tests/error_positions.rs` and did
    // not belong there, because `push`, `ret` and a `$` in a symbol are facts
    // about *this* backend and no other.

    /// The examples these tests compile.
    ///
    /// `examples/hello.tc` is deliberately absent: it is a scratch file for
    /// trying things out by hand, so it is allowed to be broken at any time.
    const EXAMPLES: [&str; 12] = [
        "arith.tc",
        "spill.tc",
        "reassign.tc",
        "bool.tc",
        "control_flow.tc",
        "functions.tc",
        "enums.tc",
        "arrays.tc",
        "classes.tc",
        "strings.tc",
        "lists.tc",
        "interactive.tc",
    ];

    fn compile_example(file: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples").join(file);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        compile_src(&text)
    }

    #[test]
    fn the_file_declares_the_entry_point_the_c_runtime_calls() {
        for file in EXAMPLES {
            let asm = compile_example(file);
            assert!(asm.contains("global main"), "{file}: nothing for the linker to start from");
            assert!(asm.contains("section .text"), "{file}: no code section");
            assert!(!functions_in(&asm).is_empty(), "{file}: no function was recognised");
        }
    }

    /// Every path out of a function undoes its prologue exactly once.
    ///
    /// A function with two `return`s has two epilogues, so the counts balance
    /// per exit, not per file. Getting this wrong does not produce wrong
    /// arithmetic — it corrupts the caller's stack, which is the kind of
    /// mistake that shows up somewhere else entirely.
    #[test]
    fn every_function_undoes_its_prologue_on_every_path() {
        for file in EXAMPLES {
            let asm = compile_example(file);
            for (name, body) in functions_in(&asm) {
                // The runtime helpers are jumped to and never come back, so
                // they have neither a prologue nor an epilogue to balance.
                if is_runtime_symbol(name) {
                    continue;
                }
                let pushes = body.matches("push ").count();
                let exits = body.matches("\n    ret\n").count();
                assert!(exits > 0, "{file}: {name} never returns");
                assert_eq!(
                    body.matches("pop  ").count(),
                    pushes * exits,
                    "unbalanced prologue in {file}: {name}"
                );
                // A leaf that spills nothing reserves no frame, and so has
                // nothing to release — but a `sub` without a matching `add` on
                // some path would leave the stack wrong.
                let reserved = body.matches("sub  rsp,").count();
                assert_eq!(
                    body.matches("add  rsp,").count(),
                    reserved * exits,
                    "{file}: {name} does not release its frame on every path"
                );
            }
        }
    }

    /// Every symbol the assembly defines, other than the entry point, must be
    /// one no TinyC function and no string literal can also claim.
    ///
    /// See the module docs: a program with `fn printf()` would otherwise define
    /// the very label `print` compiles a call to — something that assembles,
    /// links, runs, and silently does the wrong thing.
    #[test]
    fn generated_symbols_cannot_collide_with_a_tinyc_function() {
        for file in EXAMPLES {
            let asm = compile_example(file);
            for (name, _) in functions_in(&asm) {
                assert!(
                    name == "main" || name.contains('$'),
                    "{file}: `{name}` is a bare symbol a TinyC function could also define"
                );
            }
        }
    }

    /// A `$` is what separates the two namespaces, and only the entry point is
    /// allowed to sit outside them.
    #[test]
    fn the_prefixes_keep_the_two_namespaces_apart() {
        // A TinyC function called `rt` must not become a runtime symbol, which
        // is the collision `tc$rt$` is shaped to make impossible.
        let asm = compile_src(
            "fn rt() -> int {\n  return 1;\n}\nfn main() {\n  print(rt());\n}\n",
        );
        let names: Vec<&str> = functions_in(&asm).into_iter().map(|(name, _)| name).collect();
        assert!(names.contains(&"tc$rt"), "{names:?}");
        assert!(!names.iter().any(|n| is_runtime_symbol(n) && *n == "tc$rt"), "{names:?}");
        assert!(!is_runtime_symbol("tc$rt"), "a function called `rt` is not a runtime helper");
        assert!(is_runtime_symbol("tc$rt$concat"), "a real helper is one");
    }
}
