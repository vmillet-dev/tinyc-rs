//! What is true of x86-64 on Windows and nowhere else.
//!
//! The Microsoft x64 convention, four symbols the C library spells its own way,
//! and one genuinely different piece of code: a Windows console cannot be read
//! as a stream of bytes, and everything below the ABI table is about that.

use crate::codegen::PhysReg;

use super::asm::{Asm, StubFrame};
use super::runtime::{ABORT_INPUT_FAILED, INPUT, INPUT_BYTES, INPUT_DONE};
use super::used::Used;
use super::{Abi, Platform, RAX};

/// The Microsoft x64 calling convention.
static ABI: Abi = Abi {
    args: &["rcx", "rdx", "r8", "r9"],
    allocatable: &[
        PhysReg(3),  // rbx
        PhysReg(6),  // rsi
        PhysReg(7),  // rdi
        PhysReg(12), // r12
        PhysReg(13), // r13
        PhysReg(14), // r14
        PhysReg(15), // r15
    ],
    shadow_space: 32,
    variadic_in_al: false,
    // Numbered by position: the value follows the format string, so it is
    // argument 1 in both files of registers at once.
    vector_arg: "xmm1",
    vector_arg_shadowed: true,
    // The Microsoft C runtime spells the POSIX names with a leading underscore.
    write: "_write",
    read: "_read",
};

/// The code page that makes a Windows console read what is written to it as
/// UTF-8. Without it a program that prints `é` prints two bytes of mojibake,
/// and the language's promise about characters would stop at the terminal.
const UTF8_CODE_PAGE: u32 = 65001;

/// What stdin turned out to be, worked out once and remembered:
/// [`STDIN_CONSOLE`] or [`STDIN_BYTES`], and zero until the first read.
const STDIN_KIND: &str = "tc$rt$stdin_kind";
const STDIN_CONSOLE: u32 = 1;
const STDIN_BYTES: u32 = 2;
/// The console handle, and somewhere for the two console calls to answer into.
const STDIN_HANDLE: &str = "tc$rt$stdin_handle";
const CONSOLE_SCRATCH: &str = "tc$rt$console_scratch";
/// What a console is read into: UTF-16, because that is the only encoding a
/// Windows console will hand over without losing characters.
const INPUT_WIDE: &str = "tc$rt$input_wide";
/// Sized so that the worst case still fits [`INPUT`]: a UTF-16 unit becomes at
/// most three UTF-8 bytes, and a surrogate pair — two units — becomes four.
const INPUT_WCHARS: u32 = INPUT_BYTES / 3;
/// The character a console sends for `Ctrl+Z`, which is how a person says that
/// what they have typed is all there is.
const CTRL_Z: u32 = 0x1A;
/// `GetStdHandle(STD_INPUT_HANDLE)`.
const STD_INPUT_HANDLE: i32 = -10;

/// Where `GetCurrentThreadStackLimits` answers: the two ends of the range this
/// thread's stack was reserved in. Only the low one is read; the high one has
/// to be somewhere, because the call writes both.
const STACK_LOW: &str = "tc$rt$stack_low";
const STACK_HIGH: &str = "tc$rt$stack_high";

pub struct Windows;

impl Platform for Windows {
    fn name(&self) -> &'static str {
        "x86_64-windows"
    }

    fn abi(&self) -> &'static Abi {
        &ABI
    }

    fn externs(&self, used: &Used) -> Vec<&'static str> {
        let mut names = Vec::new();
        if used.writes_text() {
            names.push("SetConsoleOutputCP");
            // The UCRT has no `stdout` variable to load: the macro every C
            // program writes expands to this call, and this is the only way to
            // reach the same `FILE*` `printf` writes to.
            names.push("__acrt_iob_func");
        }
        if used.checks_stack {
            // Where this thread's stack ends. Windows 8 and later; nothing
            // older is a target here.
            names.push("GetCurrentThreadStackLimits");
        }
        if used.reads_text() {
            // A console is not a file and cannot be read as one — see
            // `refill_read`.
            names.extend(["GetStdHandle", "GetConsoleMode", "ReadConsoleW", "WideCharToMultiByte"]);
        }
        names
    }

    /// `stdout` is a macro here, not a symbol: it expands to the first entry of
    /// a table the runtime owns, reached through this call.
    fn stdout_stream(&self, asm: &mut Asm) {
        asm.asm(&format!("mov  {}, 1    ; stdout is stream 1", ABI.arg(0)));
        asm.asm("call __acrt_iob_func");
    }

    fn entry_setup(&self, asm: &mut Asm, used: &Used) {
        if !used.writes_text() {
            return;
        }
        // A console speaks a code page, and unless it is told otherwise it is
        // not this one. Without this a program that prints `é` prints mojibake.
        //
        // Nothing is said to the console about *input*: what it hands over is
        // asked for as UTF-16 at the point of reading instead, which is the
        // only way to get every character back. See `refill_read`.
        asm.comment("tell the console the bytes it is about to receive are UTF-8");
        asm.asm(&format!("mov  ecx, {UTF8_CODE_PAGE}"));
        asm.asm("call SetConsoleOutputCP");
    }

    fn entry_calls(&self, used: &Used) -> bool {
        used.writes_text()
    }

    fn input_bss(&self, asm: &mut Asm) {
        asm.asm(&format!("{INPUT_WIDE}: resw {INPUT_WCHARS}"));
        asm.asm(&format!("{STDIN_KIND}: resq 1"));
        asm.asm(&format!("{STDIN_HANDLE}: resq 1"));
        asm.asm(&format!("{CONSOLE_SCRATCH}: resq 1"));
    }

    fn stack_bss(&self, asm: &mut Asm) {
        asm.asm(&format!("{STACK_LOW}: resq 1"));
        asm.asm(&format!("{STACK_HIGH}: resq 1"));
    }

    /// `GetCurrentThreadStackLimits` reports the range the thread's stack was
    /// *reserved* in, which is the number wanted: the committed part grows
    /// towards the low end as the stack is used, so asking how much is
    /// committed right now would answer a question about the past.
    ///
    /// It returns nothing and cannot fail, so there is no "I do not know" case
    /// on this platform.
    fn stack_bottom(&self, asm: &mut Asm) {
        asm.comment("GetCurrentThreadStackLimits(&low, &high) -> the reserved range");
        asm.asm(&format!("lea  rcx, [{STACK_LOW}]"));
        asm.asm(&format!("lea  rdx, [{STACK_HIGH}]"));
        asm.asm("call GetCurrentThreadStackLimits");
        asm.asm(&format!("mov  {RAX}, [{STACK_LOW}]"));
    }

    /// Eight arguments is the widest call made here, so four of them travel on
    /// the stack, in the routine's own bytes just above the shadow space.
    fn refill_scratch(&self) -> u32 {
        32
    }

    /// ## Why a console is not read like a file
    ///
    /// A redirected stdin is bytes, and the program asked for UTF-8, so `_read`
    /// hands over exactly what is there. A **console is not bytes**: what a
    /// person types is characters, and the byte encoding is invented on the way
    /// out. Which encoding that is, is a property of the console, and every
    /// answer it can give is wrong here — the OEM code page turns `é` into one
    /// byte no character starts with, and asking for UTF-8 with `SetConsoleCP`
    /// turns it into a *NUL*, because the conversion is done one byte per
    /// character into a buffer that has no room for a second.
    ///
    /// So the console is read as what it is. `ReadConsoleW` gives UTF-16, which
    /// is the only encoding it will part with losing nothing, and
    /// `WideCharToMultiByte` turns that into the UTF-8 the shared decoder
    /// already knows how to read. That decoder never learns which of the two
    /// happened — which is the whole reason this is the only Windows-specific
    /// routine in the input path.
    fn refill_read(&self, asm: &mut Asm, frame: &StubFrame) {
        asm.comment("what stdin is cannot change, so it is worked out once");
        asm.asm(&format!("mov  {RAX}, [{STDIN_KIND}]"));
        asm.asm(&format!("test {RAX}, {RAX}"));
        asm.asm("jnz  .known");
        asm.asm(&format!("mov  ecx, {STD_INPUT_HANDLE}"));
        asm.asm("call GetStdHandle");
        asm.asm(&format!("mov  [{STDIN_HANDLE}], {RAX}"));
        asm.comment("only a console answers this one, which is the question being asked");
        asm.asm(&format!("mov  rcx, {RAX}"));
        asm.asm(&format!("lea  rdx, [{CONSOLE_SCRATCH}]"));
        asm.asm("call GetConsoleMode");
        asm.asm(&format!("mov  ecx, {STDIN_BYTES}"));
        asm.asm("test eax, eax");
        asm.asm("jz   .settled");
        asm.asm(&format!("mov  ecx, {STDIN_CONSOLE}"));
        asm.line(".settled:");
        asm.asm(&format!("mov  [{STDIN_KIND}], rcx"));
        asm.asm(&format!("mov  {RAX}, rcx"));

        asm.line(".known:");
        asm.asm(&format!("cmp  {RAX}, {STDIN_CONSOLE}"));
        asm.asm("je   .console");

        asm.comment("a redirected stdin is bytes already");
        asm.asm("mov  ecx, 0    ; the input");
        asm.asm(&format!("lea  rdx, [{INPUT}]"));
        asm.asm(&format!("mov  r8d, {INPUT_BYTES}"));
        asm.asm("call _read");
        asm.comment("nothing left is an answer; a refusal is not");
        asm.asm("test eax, eax");
        asm.asm(&format!("js   {ABORT_INPUT_FAILED}"));
        asm.comment("a 32-bit result leaves the top half undefined, so widen it");
        asm.asm(&format!("movsxd {RAX}, eax"));
        asm.asm("jmp  .filled");

        asm.line(".console:");
        asm.asm(&format!("mov  rcx, [{STDIN_HANDLE}]"));
        asm.asm(&format!("lea  rdx, [{INPUT_WIDE}]"));
        asm.asm(&format!("mov  r8d, {INPUT_WCHARS}"));
        asm.asm(&format!("lea  r9, [{CONSOLE_SCRATCH}]"));
        asm.asm(&format!("mov  qword {}, 0    ; no input control", frame.local(0)));
        asm.asm("call ReadConsoleW");
        asm.asm("test eax, eax");
        asm.asm(&format!("jz   {ABORT_INPUT_FAILED}"));
        asm.asm(&format!("mov  r12d, dword [{CONSOLE_SCRATCH}]    ; characters read"));
        asm.asm("test r12d, r12d");
        asm.asm("jz   .none");

        asm.comment("Ctrl+Z arrives as a character, not as a short read: everything");
        asm.comment("before it is input, and there is nothing after it");
        asm.asm(&format!("lea  rbx, [{INPUT_WIDE}]"));
        asm.asm("xor  r13, r13");
        asm.line(".scan:");
        asm.asm("cmp  r13, r12");
        asm.asm("jae  .encode");
        asm.asm("movzx eax, word [rbx+r13*2]");
        asm.asm(&format!("cmp  eax, {CTRL_Z:#x}"));
        asm.asm("je   .ended");
        asm.asm("inc  r13");
        asm.asm("jmp  .scan");
        asm.line(".ended:");
        asm.asm(&format!("mov  qword [{INPUT_DONE}], 1"));
        asm.asm("mov  r12, r13");
        asm.asm("test r12, r12");
        asm.asm("jz   .none");

        asm.line(".encode:");
        asm.comment("WideCharToMultiByte(65001, 0, wide, count, input, capacity, 0, 0)");
        asm.asm(&format!("mov  ecx, {UTF8_CODE_PAGE}"));
        asm.asm("xor  rdx, rdx");
        asm.asm(&format!("lea  r8, [{INPUT_WIDE}]"));
        asm.asm("mov  r9d, r12d");
        asm.asm(&format!("lea  {RAX}, [{INPUT}]"));
        asm.asm(&format!("mov  {}, {RAX}", frame.local(0)));
        asm.asm(&format!("mov  qword {}, {INPUT_BYTES}", frame.local(8)));
        asm.asm(&format!("mov  qword {}, 0", frame.local(16)));
        asm.asm(&format!("mov  qword {}, 0", frame.local(24)));
        asm.asm("call WideCharToMultiByte");
        asm.asm("test eax, eax");
        asm.asm(&format!("jz   {ABORT_INPUT_FAILED}"));
        asm.asm(&format!("movsxd {RAX}, eax"));
        asm.asm("jmp  .filled");

        asm.line(".none:");
        asm.asm(&format!("xor  {RAX}, {RAX}"));
        asm.line(".filled:");
    }
}
