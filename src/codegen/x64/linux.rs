//! What is true of x86-64 on Linux and nowhere else.
//!
//! Almost nothing, which is the point. System V AMD64 passes its arguments in
//! different registers and asks a variadic call to say how many vector
//! registers it is using; the C library spells `write` and `read` without an
//! underscore; and a terminal is a stream of UTF-8 bytes like any other file,
//! so there is no console to negotiate with at either end.
//!
//! That last one is why this file is a third the length of `windows.rs`: the
//! whole UTF-16 detour on the way in, and the code page on the way out, answer
//! a question Linux does not ask.
//!
//! ## Position-independent code
//!
//! The assembly names its symbols outright, which a position-independent
//! executable cannot do — every address there has to be reached through the GOT
//! or the PLT. So the object is linked with `-no-pie`; `scripts/build.sh` and
//! `tests/harness/elf.rs` both pass it, and it is the one flag a program built
//! from this assembly cannot do without.

use crate::codegen::PhysReg;

use super::asm::{Asm, StubFrame};
use super::runtime::{ABORT_INPUT_FAILED, INPUT, INPUT_BYTES};
use super::used::Used;
use super::{Abi, Platform, RAX, half};

/// The System V AMD64 calling convention.
///
/// It passes six arguments in registers — `r8` and `r9` follow the four below —
/// but a TinyC function may only take four, so the four are all this backend
/// ever names. See the module docs at [`super`] for why the language keeps the
/// smaller number on both platforms rather than the one each machine allows.
static ABI: Abi = Abi {
    args: &["rdi", "rsi", "rdx", "rcx"],
    // `rsi` and `rdi` are callee-saved on Windows and are handed out there;
    // here they carry the first two arguments, so the pool is two registers
    // shorter and a crowded function spills sooner. The alternative is the
    // parallel move problem — see the module docs.
    allocatable: &[
        PhysReg(3),  // rbx
        PhysReg(12), // r12
        PhysReg(13), // r13
        PhysReg(14), // r14
        PhysReg(15), // r15
    ],
    // A caller owes its callee nothing but the return address. System V has a
    // 128-byte *red zone* below `rsp` that a leaf may use without reserving it;
    // nothing here does, because a frame that is written down is one the
    // epilogue cannot forget to give back.
    shadow_space: 0,
    variadic_in_al: true,
    // Numbered by class: the format string is an integer argument and does not
    // count, so the value is the first vector one.
    vector_arg: "xmm0",
    vector_arg_shadowed: false,
    write: "write",
    read: "read",
};

/// Where `pthread_getattr_np` writes the thread's attributes, and where
/// `pthread_attr_getstack` answers out of them.
///
/// A `pthread_attr_t` is 56 bytes on x86-64 in both glibc and musl. This
/// reserves more than twice that, because the size is a fact about a C header
/// this compiler does not read, and `.bss` costs nothing in the file — a wrong
/// guess here would be a buffer the C library writes past.
const STACK_ATTR: &str = "tc$rt$stack_attr";
const STACK_ATTR_BYTES: u32 = 128;
const STACK_LOW: &str = "tc$rt$stack_low";
const STACK_SIZE: &str = "tc$rt$stack_size";

pub struct Linux;

impl Platform for Linux {
    fn name(&self) -> &'static str {
        "x86_64-linux"
    }

    fn abi(&self) -> &'static Abi {
        &ABI
    }

    /// Nothing beyond the C library every platform already imports — there is
    /// no console API here, a terminal is a file — except the four calls it
    /// takes to find out where the stack ends.
    fn externs(&self, used: &Used) -> Vec<&'static str> {
        let mut names = Vec::new();
        if used.writes_text() {
            // A real exported variable here, holding the `FILE*` — which is
            // the whole of the difference from Windows.
            names.push("stdout");
        }
        if used.checks_stack {
            names.extend([
                "pthread_self",
                "pthread_getattr_np",
                "pthread_attr_getstack",
                "pthread_attr_destroy",
            ]);
        }
        names
    }

    /// `stdout` is a variable holding the stream, so reaching it is one load.
    fn stdout_stream(&self, asm: &mut Asm) {
        asm.asm(&format!("mov  {RAX}, [stdout]"));
    }

    /// A terminal on Linux carries UTF-8 because the locale says so, and the
    /// program is in no position to argue with it — nor does it need to: what
    /// this compiler writes *is* UTF-8. So the entry point has nothing to say,
    /// and a program that only prints stays a leaf.
    fn entry_setup(&self, _asm: &mut Asm, _used: &Used) {}

    fn entry_calls(&self, _used: &Used) -> bool {
        false
    }

    /// Nothing to remember: the one way of reading is the only way.
    fn input_bss(&self, _asm: &mut Asm) {}

    fn stack_bss(&self, asm: &mut Asm) {
        asm.asm(&format!("{STACK_ATTR}: resb {STACK_ATTR_BYTES}"));
        asm.asm(&format!("{STACK_LOW}: resq 1"));
        asm.asm(&format!("{STACK_SIZE}: resq 1"));
    }

    /// `pthread_getattr_np` is the one call that reports the *main* thread's
    /// stack as the kernel actually mapped it. `getrlimit` would give the size
    /// but not where it starts, and the obvious way to fill that in — take
    /// `rsp` at the entry point as the top — is wrong by however many bytes of
    /// argument and environment strings sit above it, in the unsafe direction.
    ///
    /// It can fail, which is what the zero answer is for: a program then runs
    /// with no check at all rather than with a limit that was made up.
    fn stack_bottom(&self, asm: &mut Asm) {
        let abi = self.abi();
        asm.comment("pthread_getattr_np(pthread_self(), &attr), then read the stack out of it");
        asm.asm("call pthread_self");
        asm.asm(&format!("mov  {}, {RAX}", abi.arg(0)));
        asm.asm(&format!("lea  {}, [{STACK_ATTR}]", abi.arg(1)));
        asm.asm("call pthread_getattr_np");
        asm.comment("a refusal is answered with `no limit`, never with a guess");
        asm.asm("test eax, eax");
        asm.asm("jnz  .no_stack_bottom");

        asm.asm(&format!("lea  {}, [{STACK_ATTR}]", abi.arg(0)));
        asm.asm(&format!("lea  {}, [{STACK_LOW}]", abi.arg(1)));
        asm.asm(&format!("lea  {}, [{STACK_SIZE}]", abi.arg(2)));
        asm.asm("call pthread_attr_getstack");
        asm.asm(&format!("lea  {}, [{STACK_ATTR}]", abi.arg(0)));
        asm.asm("call pthread_attr_destroy");
        asm.asm(&format!("mov  {RAX}, [{STACK_LOW}]"));
        asm.asm("jmp  .stack_bottom_known");

        asm.line(".no_stack_bottom:");
        asm.asm(&format!("xor  {0}, {0}", half(RAX)));
        asm.line(".stack_bottom_known:");
    }

    fn refill_read(&self, asm: &mut Asm, _frame: &StubFrame) {
        let abi = self.abi();
        asm.comment("a terminal and a pipe are the same thing here: a stream of bytes.");
        asm.comment("The program asked for UTF-8, so this is exactly what was sent");
        asm.asm(&format!("xor  {0}, {0}    ; the input", half(abi.arg(0))));
        asm.asm(&format!("lea  {}, [{INPUT}]", abi.arg(1)));
        asm.asm(&format!("mov  {}, {INPUT_BYTES}", half(abi.arg(2))));
        asm.asm(&format!("call {}", abi.read));
        asm.comment("nothing left is an answer; a refusal is not");
        asm.asm("test eax, eax");
        asm.asm(&format!("js   {ABORT_INPUT_FAILED}"));
        asm.comment("a 32-bit result leaves the top half undefined, so widen it");
        asm.asm(&format!("movsxd {RAX}, eax"));
    }
}

#[cfg(test)]
mod tests {
    //! What is true here and not on Windows.
    //!
    //! Everything this backend does that is merely *x86-64* is checked once, in
    //! `tests.rs`, through the other platform — repeating it here would say the
    //! same thing twice. What is left is short, and it is exactly the list the
    //! module docs open with.

    use super::*;
    use crate::codegen::x64::X64;
    use crate::codegen::{Allocation, Backend, regalloc};

    fn compile_src(src: &str) -> String {
        let ast = crate::parser::parse(&crate::lexer::lex(src).unwrap()).unwrap();
        let types = crate::sema::check(&ast, crate::target::Machine::TEST).unwrap();
        let ir = crate::ir::lower(&ast, &types).expect("the frames should fit");
        let backend = X64::linux();
        let allocations: Vec<Allocation> =
            ir.functions.iter().map(|f| regalloc::allocate(f, backend.register_file())).collect();
        backend.emit(&ir, &allocations)
    }

    fn compile(body: &str) -> String {
        compile_src(&format!("fn main() {{\n{body}\n}}\n"))
    }

    #[test]
    fn arguments_travel_in_the_system_v_registers() {
        let asm = compile_src(
            "fn f(int a, int b, int c, int d) -> int {\n  return a;\n}\n\
             fn main() {\n  f(1, 2, 3, 4);\n}",
        );
        for (index, register) in ["rdi", "rsi", "rdx", "rcx"].into_iter().enumerate() {
            assert!(
                asm.contains(&format!("mov  {register}, {}", index + 1)),
                "argument {index} should travel in {register}: {asm}"
            );
        }
        // The registers Windows would have used are not the ones here.
        assert!(!asm.contains("mov  r8, 3"), "r8 is argument five on this platform: {asm}");
    }

    #[test]
    fn a_variadic_call_announces_that_it_passes_no_vector_register() {
        // System V reads `al` at a variadic call to decide how much of the
        // register save area to fill. Leaving whatever happened to be in `rax`
        // there is the kind of mistake that works until it does not.
        let asm = compile("println(1);\nprintln(\"hi\");\nprintln(true);");
        for call in asm.match_indices("call printf") {
            let before = &asm[..call.0];
            assert!(
                before.trim_end().ends_with("xor  eax, eax    ; no vector register is passed"),
                "a printf call is not preceded by clearing al: {asm}"
            );
        }
        assert!(asm.contains("call printf"), "the program should print: {asm}");
    }

    #[test]
    fn the_stack_is_found_through_pthread_rather_than_guessed_at() {
        // Where a stack ends is the one question `rsp` cannot answer, and the
        // two platforms ask it differently. `getrlimit` would give the size but
        // not where it starts, and taking `rsp` at the entry point as the top
        // is wrong by however many bytes of argument and environment strings
        // sit above it — in the direction that fails to catch an overflow.
        let asm = compile_src(
            "fn down(int n) -> int {\n  if (n == 0) { return 0; }\n  return down(n - 1);\n}\n\
             fn main() {\n  println(down(3));\n}",
        );
        for name in ["pthread_self", "pthread_getattr_np", "pthread_attr_getstack"] {
            assert!(asm.contains(&format!("extern {name}")), "{name} is not imported: {asm}");
            assert!(asm.contains(&format!("call {name}")), "{name} is not called: {asm}");
        }
        // The attributes are given back, and the answer is read out of them.
        assert!(asm.contains("call pthread_attr_destroy"), "{asm}");
        assert!(asm.contains(&format!("mov  {RAX}, [{STACK_LOW}]")), "{asm}");
        // Nothing Windows needs appears here.
        assert!(!asm.contains("GetCurrentThreadStackLimits"), "{asm}");
    }

    #[test]
    fn a_program_that_never_calls_asks_nothing_about_the_stack() {
        let asm = compile("println(1 + 2);");
        assert!(!asm.contains("pthread"), "{asm}");
    }

    #[test]
    fn a_caller_owes_its_callee_nothing_but_the_return_address() {
        // No shadow space, so a function that calls reserves only what it
        // spills plus whatever alignment costs. The `+ 32` a Windows prologue
        // starts from must not appear.
        assert_eq!(ABI.shadow_space, 0);
        for pushes in 0..4usize {
            for spill_slots in 0..4u32 {
                let allocation = Allocation {
                    locations: Default::default(),
                    used_callee_saved: vec![crate::codegen::PhysReg(3); pushes],
                    spill_slots,
                    intervals: Vec::new(),
                };
                let frame = super::super::func::FrameLayout::new(&allocation, 0, false, 0);
                assert_eq!(
                    (8 + 8 * pushes as u32 + frame.size) % 16,
                    0,
                    "the stack has to be aligned at a call"
                );
                assert!(frame.size >= 8 * spill_slots, "the frame has to hold every spill slot");
                // The first spill slot sits at `rsp` itself: there is nothing
                // underneath it to step over.
                assert_eq!(frame.slot_offset(0), 0);
            }
        }
    }

    #[test]
    fn nothing_is_said_to_the_terminal_before_the_program_runs() {
        // A Linux terminal carries UTF-8 because the locale says so. There is
        // no code page to set, so the entry point goes straight to work.
        let asm = compile("println(\"héllo\");\nprintln('é');");
        assert!(!asm.contains("SetConsoleOutputCP"), "{asm}");
        assert!(!asm.contains("GetConsoleMode"), "{asm}");
        assert!(!asm.contains("ReadConsoleW"), "{asm}");
        assert!(!asm.contains("WideCharToMultiByte"), "{asm}");
    }

    #[test]
    fn the_c_library_is_imported_under_the_names_it_exports() {
        // The Microsoft C runtime spells these with a leading underscore and
        // this one does not; importing the wrong spelling is a link error, and
        // a link error is what this test exists to happen here instead.
        let reads = compile("string line = read_line();\nprintln(line);");
        assert!(reads.contains("\nextern read\n"), "{reads}");
        assert!(!reads.contains("extern _read"), "{reads}");

        let aborts = compile("int n = 1;\nprintln(n + 1);");
        assert!(aborts.contains("\nextern write\n"), "{aborts}");
        assert!(!aborts.contains("extern _write"), "{aborts}");
    }

    #[test]
    fn the_input_is_read_as_the_bytes_it_already_is() {
        // One `read` into the buffer and nothing else: the whole UTF-16 detour
        // a Windows console needs answers a question that is not asked here.
        let asm = compile("println(read_line());");
        assert!(asm.contains("lea  rsi, [tc$rt$input]"), "{asm}");
        assert!(asm.contains("call read"), "{asm}");
        assert!(!asm.contains("tc$rt$input_wide"), "there is no wide buffer to need: {asm}");
        assert!(!asm.contains("tc$rt$stdin_kind"), "there is only one kind of stdin: {asm}");
    }

    #[test]
    fn no_allocatable_register_is_an_argument_register() {
        // The reason the pool is five registers here and seven on Windows, and
        // the invariant that lets a call's argument moves be emitted in any
        // order at all.
        let backend = X64::linux();
        let file = backend.register_file();
        for &reg in &file.callee_saved {
            assert!(
                !ABI.args.contains(&file.name(reg)),
                "{} is an argument register and must not be allocatable",
                file.name(reg)
            );
        }
        assert_eq!(file.callee_saved.len(), 5, "rbx and r12-r15, and nothing else");
    }
}
