//! The compiler's own routines: the arena, what a string operator becomes, what
//! a list operator becomes, and the two edges where characters meet UTF-8.
//!
//! **Nothing in this file knows which platform it is emitting for.** Every
//! routine here is x86-64 and nothing else; the only concessions to a
//! convention are that arguments are named through [`Abi::arg`] rather than
//! written out, and that stack frames are sized by [`StubFrame`] rather than by
//! hand. That is the whole of it — the algorithms are the same instructions on
//! both machines.
//!
//! The register rules these bodies follow are written down at
//! [`super::RUNTIME_LOCALS`]. In short: anything that must survive a call is
//! pushed and comes from that list, `rax`/`r10`/`r11` are free scratch between
//! calls, and `rsi`/`rdi` are never touched except as an argument register,
//! because on Windows they belong to the caller.

use crate::ast::{ClassId, Ty, TypeTable};
use crate::ir::{CHAR_BYTES, Runtime};

use super::asm::{Asm, StubFrame};
use super::used::{Used, fixup_label};
use super::{Abi, Platform, RAX, RDX, STDERR, STR_HEADER, half, runtime_symbol};

// -- where a program can stop ----------------------------------------------

pub const ABORT_DIV_ZERO: &str = "tc$rt$div_by_zero";
pub const ABORT_DIV_OVERFLOW: &str = "tc$rt$div_overflow";
pub const ABORT_OVERFLOW: &str = "tc$rt$overflow";
pub const ABORT_BOUNDS: &str = "tc$rt$bounds";
pub const ABORT_OOM: &str = "tc$rt$out_of_memory";
pub const ABORT_BAD_CHAR: &str = "tc$rt$bad_char";
pub const ABORT_NO_INPUT: &str = "tc$rt$no_input";
pub const ABORT_BAD_UTF8: &str = "tc$rt$bad_utf8";
pub const ABORT_INPUT_FAILED: &str = "tc$rt$input_failed";
pub const ABORT_NOT_A_NUMBER: &str = "tc$rt$not_a_number";
pub const ABORT_NO_INT: &str = "tc$rt$no_int";
pub const ABORT_STACK: &str = "tc$rt$stack_exhausted";
pub const ABORT_REPORT: &str = "tc$rt$abort";

/// Where an operation that cannot be performed lands.
///
/// Each is a label the failing instruction jumps to, paired with the text it
/// reports. They are kept together so that adding a way to fail means adding
/// one row rather than touching four places.
pub const ABORTS: [Abort; 12] = [
    Abort::new(ABORT_DIV_ZERO, "division by zero", |u| u.div_zero),
    Abort::new(ABORT_DIV_OVERFLOW, "division overflows an int", |u| u.div_overflow),
    Abort::new(ABORT_OVERFLOW, "arithmetic overflows an int", |u| u.overflow),
    Abort::new(ABORT_BOUNDS, "index out of bounds", |u| u.bounds),
    Abort::new(ABORT_OOM, "out of memory", Used::allocates),
    Abort::new(ABORT_BAD_CHAR, "this number is not a character", |u| u.check_char),
    // Only a `read_line` past the end reports this; `eof()` is the question
    // that avoids it, and asking it can never be the thing that stops.
    Abort::new(ABORT_NO_INPUT, "there is no more input to read", |u| u.read_line),
    Abort::new(ABORT_BAD_UTF8, "the input is not valid UTF-8", Used::reads_text),
    Abort::new(ABORT_INPUT_FAILED, "the input could not be read", Used::reads_text),
    // `is_int(s)` is the same routine asked a different way, and it answers
    // rather than stopping — so only the conversion reaches this.
    Abort::new(ABORT_NOT_A_NUMBER, "this text is not a number an int can hold", |u| u.str_int),
    // The same sentence about the other conversion that can be asked for a
    // number there is not: a float too large, or a NaN, has no `int`.
    Abort::new(ABORT_NO_INT, "this float is not a number an int can hold", |u| {
        u.float_to_int
    }),
    Abort::new(ABORT_STACK, "the stack is exhausted, so this call cannot be made", |u| {
        u.checks_stack
    }),
];

/// One way a program can stop rather than answer wrongly.
pub struct Abort {
    /// What the failing instruction jumps to.
    label: &'static str,
    /// What went wrong, as the program reports it.
    what: &'static str,
    /// What in the program makes this one reachable.
    ///
    /// It travels in the row rather than in a `match` somewhere else for the
    /// same reason the message does: adding a way to fail should be adding a
    /// line here and nothing anywhere. Without it the whole table went into
    /// every program that could fail at all — a `hello.tc` that indexes nothing
    /// and allocates nothing still carried 483 bytes of messages about both.
    reached: fn(&Used) -> bool,
}

impl Abort {
    const fn new(label: &'static str, what: &'static str, reached: fn(&Used) -> bool) -> Abort {
        Abort { label, what, reached }
    }

    /// Whether this program can reach this way of failing.
    pub fn reached_by(&self, used: &Used) -> bool {
        (self.reached)(used)
    }

    /// The label of this failure's text in `.data`, derived from its own so
    /// that a new way to fail is one row here and nothing anywhere else.
    pub fn message(&self) -> String {
        format!("{}_text", self.label)
    }

    pub fn text(&self) -> String {
        format!("runtime error: {}\n", self.what)
    }
}

// -- the stack -------------------------------------------------------------

/// The lowest address a function may leave `rsp` at, worked out once by the
/// entry point and read by every prologue afterwards.
///
/// Zero means it could not be worked out, and a zero here can never fire the
/// check: every real `rsp` is above it. That is the honest degradation — the
/// program behaves exactly as it did before this existed — and it is why
/// [`super::Platform::stack_bottom`] is allowed to answer "I do not know".
pub const STACK_LIMIT: &str = "tc$rt$stack_limit";

/// How much of the stack is left unspent below [`STACK_LIMIT`].
///
/// The check is made before a function's frame is reserved, so once it passes,
/// this much is still there — and it has to cover everything that runs *without*
/// checking: the runtime's own routines, the C library calls they make, and
/// above all the abort path itself, which has to be able to call `fflush`,
/// `write` and `exit` to say what happened. A report that overflowed the stack
/// while reporting a stack overflow is the one failure this must not have.
pub const STACK_MARGIN: u32 = 64 * 1024;

// -- the arena and its bookkeeping -----------------------------------------

pub const ARENA_NEXT: &str = "tc$rt$arena_next";
pub const ARENA_END: &str = "tc$rt$arena_end";
/// Where the chunk [`ARENA_NEXT`] is bumping through begins.
///
/// Only [`LIST_ROOM`] reads it, and only to be sure of something it could
/// otherwise nearly prove: that the block it is about to extend really is the
/// last one this chunk handed out, rather than an older one that happens to end
/// where the bump pointer now stands. "Nearly" is not the standard here.
pub const ARENA_CHUNK: &str = "tc$rt$arena_chunk";
pub const ALLOC: &str = "tc$rt$alloc";

/// Bytes the arena asks the C runtime for at a time.
///
/// Big enough that a program doing string work calls `malloc` a handful of
/// times, small enough that a program doing none pays nothing — the first
/// chunk is only asked for when something is first allocated.
const CHUNK_BYTES: u32 = 1 << 16;

/// The smallest capacity a list is given, so that the first few `push`es cost
/// no move at all.
const LIST_MIN_CAPACITY: u32 = 4;

/// The one routine `int(s)` and `is_int(s)` are both built on. Internal: no
/// `Runtime` names it, because no program calls it directly.
pub const PARSE_INT: &str = "tc$rt$parse_int";
/// The same for the two pushes: this makes the room and says where it is, and
/// they differ only in what they put there.
pub const LIST_ROOM: &str = "tc$rt$list_room";

// -- writing out -----------------------------------------------------------

/// The `FILE*` standard output goes to, worked out on the first write and kept.
///
/// Asked for lazily rather than by the entry point, so a program that prints
/// nothing carries neither the question nor the eight bytes to hold the answer.
pub const STDOUT: &str = "tc$rt$stdout";

/// Write out exactly `len` bytes: **the one routine every `print` ends in.**
///
/// It takes a length, and that is the whole point. A TinyC string is a run of
/// characters and `char(0)` is one of them — the lexer accepts `"\0"`, `char(0)`
/// converts to it, and a line of input may simply contain one. A *C* string is
/// the bytes up to the first NUL, so `printf("%s", …)` cannot say what this
/// language can hold: it stopped at the first `\0`, and for a `println` it
/// swallowed the newline after it too, silently running two lines together.
///
/// `fwrite` is the same buffered stream `printf` writes to, so a program that
/// mixes an `int` and a `string` still comes out in the order it was written.
pub const WRITE_TEXT: &str = "tc$rt$write_text";

/// Encodes one character as UTF-8, which is the only place the language's
/// representation meets the one the outside world reads.
pub const UTF8: &str = "tc$rt$utf8";
pub const PRINT_STR: &str = "tc$rt$print_str";
pub const PRINT_CHAR: &str = "tc$rt$print_char";
/// Where `print` builds the UTF-8 of a string, kept between calls so that
/// printing in a loop allocates once rather than once per line.
pub const SCRATCH: &str = "tc$rt$scratch";
pub const SCRATCH_CAP: &str = "tc$rt$scratch_cap";

/// Where a line of input is read into before it is picked apart.
///
/// The compiler does its own buffering rather than going through a `FILE*`,
/// which keeps it to one import and makes [`Runtime::Eof`] answerable without
/// pushing a character back: "is there more" is a question about this buffer.
pub const INPUT: &str = "tc$rt$input";
pub const INPUT_POS: &str = "tc$rt$input_pos";
pub const INPUT_LEN: &str = "tc$rt$input_len";
pub const INPUT_BYTES: u32 = 4096;
/// Set once the input has been declared over. Only a platform that can be told
/// so in the middle of a read — a Windows console, by a `Ctrl+Z` — ever writes
/// it; on the others it stays zero and costs eight bytes of `.bss`.
pub const INPUT_DONE: &str = "tc$rt$input_done";
/// Set once the first bytes of the input have been looked at, which is the
/// only moment a byte order mark could be among them.
pub const FIRST_READ: &str = "tc$rt$first_read";

/// Whether a byte is waiting, refilling the buffer if it has run dry.
pub const READY: &str = "tc$rt$ready";
pub const NEXT_BYTE: &str = "tc$rt$next_byte";
/// Reads one character's worth of UTF-8, which is where the outside world's
/// encoding becomes the language's.
pub const UTF8_DECODE: &str = "tc$rt$utf8_decode";
/// Fills [`INPUT`] from whatever stdin turns out to be. A shared skeleton
/// around one platform-specific read — see [`input_stubs`].
pub const REFILL: &str = "tc$rt$refill";

/// The registers [`refill`] pushes, and so the ones a platform's own read may
/// use freely inside it.
pub const REFILL_LOCALS: [&str; 3] = ["rbx", "r12", "r13"];

// -- runtime failures ------------------------------------------------------

/// The out-of-line ends every failing operation jumps to.
///
/// They are reached by `jmp`, not `call`, so `rsp` on arrival is whatever the
/// failing function happened to be using. Rather than oblige every one of those
/// functions to keep a frame a call could be made from — which, now that any
/// addition can fail, would be nearly all of them — the report builds its own
/// out of thin air. It can: it never returns, so `rsp` is not worth preserving,
/// and neither are the callee-saved registers it takes for itself.
pub fn abort_stubs(asm: &mut Asm, abi: &Abi, used: &Used) {
    asm.blank();
    asm.comment("runtime failures: report on stderr, then leave with a non-zero status");

    // Only the ones this program can reach, so a program that never indexes
    // carries nothing about an index — the same bargain every other routine
    // here is emitted under.
    let reached: Vec<&Abort> = ABORTS.iter().filter(|a| a.reached_by(used)).collect();
    for (at, abort) in reached.iter().enumerate() {
        asm.line(&format!("{}:", abort.label));
        asm.asm(&format!("lea  {}, [{}]", abi.arg(1), abort.message()));
        asm.asm(&format!("mov  {}, {}", half(abi.arg(2)), abort.text().len()));
        // The last one falls straight through into the report rather than
        // jumping to the instruction after itself.
        if at + 1 < reached.len() {
            asm.asm(&format!("jmp  {ABORT_REPORT}"));
        }
    }

    asm.line(&format!("{ABORT_REPORT}:"));
    asm.comment("a frame of its own, so nothing that jumps here owes one");
    // `and` forces the alignment a `call` needs whatever the jumper's `rsp`
    // was, and the reservation buys whatever shadow space the callees expect.
    // Both destroy `rsp`, which costs nothing at all: this routine never
    // returns.
    asm.asm("and  rsp, -16");
    if abi.shadow_space > 0 {
        asm.asm(&format!("sub  rsp, {}    ; shadow space", abi.shadow_space));
    }
    asm.comment("empty what `print` has left buffered, so this lands after it");
    // `write` goes straight to the descriptor while `print` goes through the C
    // runtime's buffer. Without this the report is written first and whatever
    // the program had already printed follows it out at `exit`, which reads as
    // if the failure happened earlier than it did.
    //
    // The message and its length arrived in argument registers, which a call
    // destroys, so they wait in registers a call does not. This routine never
    // returns, so those registers are nobody's to get back.
    asm.asm(&format!("mov  rbx, {}    ; the message", abi.arg(1)));
    asm.asm(&format!("mov  r12, {}    ; its length", abi.arg(2)));
    asm.asm(&format!("xor  {0}, {0}    ; fflush(NULL) empties every stream", abi.arg(0)));
    asm.asm("call fflush");
    asm.asm(&format!("mov  {}, rbx", abi.arg(1)));
    asm.asm(&format!("mov  {}, r12", abi.arg(2)));
    asm.comment("write(2, message, length), then exit(1)");
    asm.asm(&format!("mov  {}, {STDERR}", abi.arg(0)));
    asm.asm(&format!("call {}", abi.write));
    asm.asm(&format!("mov  {}, 1", abi.arg(0)));
    asm.asm("call exit");
}

// -- the arena -------------------------------------------------------------

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
pub fn arena(asm: &mut Asm, abi: &Abi) {
    asm.blank();
    asm.comment("the arena: a bump pointer through chunks, and nothing is ever freed");
    asm.line(&format!("{ALLOC}:"));
    asm.comment(&format!("{} = bytes wanted -> rax = their address", abi.arg(0)));
    let frame = StubFrame::enter(asm, abi, &["rbx", "r12"], 0, "");
    asm.comment("round up, so every block starts 16-aligned like malloc's own");
    asm.asm(&format!("lea  rbx, [{}+15]", abi.arg(0)));
    asm.asm("and  rbx, -16");
    asm.asm(&format!("mov  {RAX}, [{ARENA_NEXT}]"));
    asm.asm(&format!("lea  r10, [{RAX}+rbx]"));
    asm.asm(&format!("cmp  r10, [{ARENA_END}]"));
    asm.asm("ja   .refill");
    asm.asm(&format!("mov  [{ARENA_NEXT}], r10"));
    frame.ret(asm);

    asm.line(".refill:");
    asm.comment("what is left of the old chunk is abandoned: nothing here frees");
    asm.asm(&format!("mov  r12, {CHUNK_BYTES}"));
    asm.comment("A single block can be bigger than a chunk, and then the chunk is asked");
    asm.comment("for half again as much as it needs. That spare room is not waste: it is");
    asm.comment("what the *next* block gets bumped into, and in particular what a string");
    asm.comment("or a list growing in place grows into. Without it, something that has");
    asm.comment("outgrown a chunk asks for an exact fit every time it gains a character,");
    asm.comment("and starts copying itself on every step again.");
    asm.asm("mov  r10, rbx");
    asm.asm("shr  r10, 1");
    asm.asm("add  r10, rbx");
    asm.asm("cmp  r12, r10");
    asm.asm("jae  .big_enough");
    asm.asm("mov  r12, r10");
    asm.line(".big_enough:");
    asm.asm(&format!("mov  {}, r12", abi.arg(0)));
    asm.asm("call malloc");
    asm.asm(&format!("test {RAX}, {RAX}"));
    asm.asm(&format!("jz   {ABORT_OOM}"));
    asm.asm(&format!("mov  [{ARENA_CHUNK}], {RAX}    ; where this chunk begins"));
    asm.asm(&format!("lea  r10, [{RAX}+r12]"));
    asm.asm(&format!("mov  [{ARENA_END}], r10"));
    asm.asm(&format!("lea  r10, [{RAX}+rbx]"));
    asm.asm(&format!("mov  [{ARENA_NEXT}], r10"));
    frame.ret(asm);
}

// -- writing out -----------------------------------------------------------

/// [`WRITE_TEXT`], and the one question it needs answered first.
///
/// Everything the language writes arrives here as *bytes and a count*: the
/// literal words of a format, a string encoded into the scratch buffer, one
/// encoded character, the newline a `println` ends with. Nothing anywhere
/// looks for a NUL, which is why a `\0` inside a string is now written like
/// any other character rather than cutting the line short.
pub fn output_stubs(asm: &mut Asm, platform: &dyn Platform, _used: &Used) {
    let abi = platform.abi();
    let (a0, a1, a2, a3) = (abi.arg(0), abi.arg(1), abi.arg(2), abi.arg(3));

    asm.blank();
    asm.comment("write out exactly this many bytes — never up to a NUL");
    asm.line(&format!("{WRITE_TEXT}:"));
    asm.comment(&format!("{a0} = the bytes, {a1} = how many"));
    let frame = StubFrame::enter(asm, abi, &["rbx", "r12"], 0, "");
    asm.asm(&format!("mov  rbx, {a0}"));
    asm.asm(&format!("mov  r12, {a1}"));
    asm.comment("the stream, asked for once and remembered");
    asm.asm(&format!("mov  {RAX}, [{STDOUT}]"));
    asm.asm(&format!("test {RAX}, {RAX}"));
    asm.asm("jnz  .have");
    platform.stdout_stream(asm);
    asm.asm(&format!("mov  [{STDOUT}], {RAX}"));
    asm.line(".have:");
    asm.comment("fwrite(bytes, 1, how many, stdout)");
    asm.asm(&format!("mov  {a3}, {RAX}"));
    asm.asm(&format!("mov  {a0}, rbx"));
    asm.asm(&format!("mov  {a1}, 1"));
    asm.asm(&format!("mov  {a2}, r12"));
    asm.asm("call fwrite");
    frame.ret(asm);
}

// -- giving a copy its own elements ----------------------------------------

/// Walk a run of freshly copied objects and give each one what it does not yet
/// own. `(at, count, stride)`, and nothing comes back.
///
/// The dispatch is the point. A field declared `Reading` may hold a `Frost`,
/// and a `Frost` has fields a `Reading` does not — so what to fix up cannot be
/// read off the *hole*, only off the value in it. Every object carries its
/// vtable pointer at offset 0 and the routine sits in the word in front of the
/// slots, so one indirect call answers it for every class at once.
pub const FIXUP: &str = "tc$rt$fixup";

/// [`FIXUP`], and one routine per class that has something to give.
///
/// Each of those is straight-line code over the class's own fields, because
/// the class is known while the program is compiled and its layout is settled.
/// The only loop is this one, over the run.
pub fn fixup_stubs(asm: &mut Asm, abi: &Abi, table: &TypeTable, used: &Used) {
    let (a0, a1, a2) = (abi.arg(0), abi.arg(1), abi.arg(2));

    asm.blank();
    asm.comment("give each object in a fresh copy what the copy does not yet own");
    asm.line(&format!("{FIXUP}:"));
    asm.comment(&format!("{a0} = the first, {a1} = how many, {a2} = bytes apart"));
    let frame = StubFrame::enter(asm, abi, &["rbx", "r12", "r13"], 0, "");
    asm.asm(&format!("mov  rbx, {a0}"));
    asm.asm(&format!("mov  r12, {a1}"));
    asm.asm(&format!("mov  r13, {a2}"));
    asm.line(".next:");
    asm.asm("test r12, r12");
    asm.asm("jz   .done");
    asm.comment("the object's own vtable says what it owns; the word before the slots");
    asm.asm(&format!("mov  {RAX}, [rbx]"));
    asm.asm(&format!("mov  {RAX}, [{RAX}-8]"));
    asm.asm(&format!("test {RAX}, {RAX}"));
    asm.asm("jz   .step    ; a class that shares nothing");
    asm.asm(&format!("mov  {a0}, rbx"));
    asm.asm(&format!("call {RAX}"));
    asm.line(".step:");
    asm.asm("add  rbx, r13");
    asm.asm("dec  r12");
    asm.asm("jmp  .next");
    asm.line(".done:");
    frame.ret(asm);

    for (index, class) in table.classes.iter().enumerate() {
        let id = ClassId(index as u32);
        if !used.owns_elements(table, id) {
            continue;
        }
        asm.blank();
        asm.comment(&format!("what a fresh `{}` has to be given of its own", class.name));
        asm.line(&format!("{}:", fixup_label(id)));
        asm.comment(&format!("{a0} = the object"));
        let frame = StubFrame::enter(asm, abi, &["rbx"], 0, "");
        asm.asm(&format!("mov  rbx, {a0}"));
        for field in class.fields.iter().filter(|f| table.holds_a_list(f.ty)) {
            let at = field.offset;
            asm.comment(&format!("`{}`, at {at}", field.name));
            match field.ty {
                // The field holds an address, so the copy holds the *same*
                // address. This is the one place a list is duplicated for a
                // reason other than an assignment, and the reason is the same:
                // two names for one list would be observable.
                Ty::List(list) => {
                    let elem = table.element(list);
                    let deep = i64::from(table.holds_a_list(elem));
                    asm.asm(&format!("mov  {a0}, [rbx+{at}]"));
                    asm.asm(&format!("mov  {a1}, {}", table.size_of(elem)));
                    asm.asm(&format!("mov  {a2}, {deep}"));
                    asm.asm(&format!("call {}", runtime_symbol(Runtime::ListClone)));
                    asm.asm(&format!("mov  [rbx+{at}], {RAX}"));
                }
                // Held whole, inside these very bytes — so there is nothing to
                // replace, only something further in to go and look at.
                Ty::Class(_) => {
                    asm.asm(&format!("lea  {a0}, [rbx+{at}]"));
                    asm.asm(&format!("mov  {a1}, 1"));
                    asm.asm(&format!("mov  {a2}, 0"));
                    asm.asm(&format!("call {FIXUP}"));
                }
                // The same, once per element. An array's length is part of its
                // type, so both numbers are written out here.
                Ty::Array(array) => {
                    let info = table.array(array);
                    asm.asm(&format!("lea  {a0}, [rbx+{at}]"));
                    asm.asm(&format!("mov  {a1}, {}", info.len));
                    asm.asm(&format!("mov  {a2}, {}", table.size_of(info.elem)));
                    asm.asm(&format!("call {FIXUP}"));
                }
                other => unreachable!("nothing else can hold a list: {other:?}"),
            }
        }
        frame.ret(asm);
    }
}

// -- strings ---------------------------------------------------------------

/// The routines a string's operators become.
///
/// Each is here because it is a *loop*. Everything a string does in a straight
/// line — its length, one of its characters — is an instruction the backend
/// emits inline; only the operations that have to walk the characters are worth
/// a call.
pub fn string_stubs(asm: &mut Asm, abi: &Abi, used: &Used) {
    let (a0, a1, a2) = (abi.arg(0), abi.arg(1), abi.arg(2));

    if used.concat {
        asm.blank();
        asm.comment("a + b: a new string in the arena, holding a's characters then b's");
        asm.line(&format!("{}:", runtime_symbol(Runtime::Concat)));
        asm.comment(&format!("{a0} = a, {a1} = b -> rax = the joined string"));
        let frame = StubFrame::enter(
            asm,
            abi,
            &["rbx", "r12", "r13", "r14"],
            0,
            "",
        );
        asm.asm(&format!("mov  r13, {a0}"));
        asm.asm(&format!("mov  r14, {a1}"));
        asm.asm("mov  rbx, [r13-8]    ; how many characters a has");
        asm.asm("mov  r12, [r14-8]");
        asm.asm(&format!("lea  {a0}, [rbx+r12]"));
        asm.asm(&format!(
            "lea  {a0}, [{a0}*{CHAR_BYTES}+{STR_HEADER}]    ; the header, then four bytes each"
        ));
        asm.asm(&format!("call {ALLOC}"));
        asm.asm("lea  r10, [rbx+r12]");
        asm.asm(&format!("mov  [{RAX}], r10    ; the count goes in front"));
        asm.asm(&format!("add  {RAX}, 8    ; and the value is where the characters start"));
        asm.asm(&format!("mov  r10, {RAX}    ; where the next character goes"));
        asm.asm("mov  r11, rbx");
        asm.line(".left:");
        asm.asm("test r11, r11");
        asm.asm("jz   .right_start");
        asm.asm("mov  edx, [r13]");
        asm.asm("mov  [r10], edx");
        asm.asm("add  r13, 4");
        asm.asm("add  r10, 4");
        asm.asm("dec  r11");
        asm.asm("jmp  .left");
        asm.line(".right_start:");
        asm.asm("mov  r11, r12");
        asm.line(".right:");
        asm.asm("test r11, r11");
        asm.asm("jz   .done");
        asm.asm("mov  edx, [r14]");
        asm.asm("mov  [r10], edx");
        asm.asm("add  r14, 4");
        asm.asm("add  r10, 4");
        asm.asm("dec  r11");
        asm.asm("jmp  .right");
        asm.line(".done:");
        frame.ret(asm);
    }

    if used.append {
        asm.blank();
        asm.comment("s = s + t, where nothing else can be holding s");
        asm.comment("");
        asm.comment("The same answer `concat` gives, made where `s` already is when the");
        asm.comment("arena can still give that room back. A string's length lives with its");
        asm.comment("characters, so growing one where it stands bumps a count every other");
        asm.comment("name for it can see — which is why lowering only reaches this after");
        asm.comment("proving there is no other name. See `ir::owned_strings`.");
        asm.line(&format!("{}:", runtime_symbol(Runtime::Append)));
        asm.comment(&format!(
            "{a0} = the accumulator, {a1} = what to add, {a2} = whether {a1} is this \
             statement's own -> rax = the answer"
        ));
        let frame =
            StubFrame::enter(asm, abi, &["rbx", "r12", "r13", "r14", "r15"], 0, "");
        asm.asm(&format!("mov  rbx, {a0}"));
        asm.asm(&format!("mov  r12, {a1}"));
        asm.asm(&format!("mov  r13, {a2}"));
        asm.asm("mov  r14, [rbx-8]    ; how many characters it has");
        asm.asm("mov  r15, [r12-8]    ; and how many are being added");

        asm.comment("where the accumulator's block ends, the arena's rounding included");
        asm.asm(&format!("lea  {RAX}, [r14*{CHAR_BYTES}+{}+15]", STR_HEADER));
        asm.asm(&format!("and  {RAX}, -16"));
        asm.asm(&format!("lea  {RAX}, [rbx+{RAX}-{STR_HEADER}]"));
        asm.asm(&format!("cmp  {RAX}, [{ARENA_NEXT}]"));
        asm.asm("je   .last");

        asm.comment("Not the last block — but there is one more case that can still be");
        asm.comment("given back. What was just built to be added sits immediately after,");
        asm.comment("and it is this statement's own, so consuming it costs nobody");
        asm.comment("anything: `s = s + string(n)` in a loop stays linear because of it.");
        asm.asm("test r13, r13");
        asm.asm("jz   .fallback");
        asm.asm(&format!("lea  r11, [r12-{STR_HEADER}]"));
        asm.asm(&format!("cmp  r11, {RAX}"));
        asm.asm("jne  .fallback    ; something else is in between");
        asm.asm(&format!("lea  r11, [r15*{CHAR_BYTES}+{}+15]", STR_HEADER));
        asm.asm("and  r11, -16");
        asm.asm(&format!("lea  r11, [r12+r11-{STR_HEADER}]"));
        asm.asm(&format!("cmp  r11, [{ARENA_NEXT}]"));
        asm.asm("jne  .fallback    ; nor is it the last");

        asm.line(".last:");
        asm.comment("and it has to lie in the chunk the bump pointer is walking, rather");
        asm.comment("than in an older one that ends where it happens to stand");
        asm.asm(&format!("lea  r11, [rbx-{STR_HEADER}]"));
        asm.asm(&format!("cmp  r11, [{ARENA_CHUNK}]"));
        asm.asm("jb   .fallback");

        asm.comment("the room the answer needs, from the same start");
        asm.asm("lea  r10, [r14+r15]");
        asm.asm(&format!("lea  r11, [r10*{CHAR_BYTES}+{}+15]", STR_HEADER));
        asm.asm("and  r11, -16");
        asm.asm(&format!("lea  r11, [rbx+r11-{STR_HEADER}]"));
        asm.asm(&format!("cmp  r11, [{ARENA_END}]"));
        asm.asm("ja   .fallback    ; this chunk has no room for the rest");
        asm.asm(&format!("mov  [{ARENA_NEXT}], r11"));

        asm.comment("the characters go where the ones already there stop. Copied forward,");
        asm.comment("and the destination is never above the source — so the one case where");
        asm.comment("the two runs overlap, which is the block being given back, is safe.");
        asm.asm(&format!("lea  r11, [rbx+r14*{CHAR_BYTES}]"));
        asm.asm(&format!("xor  {RAX}, {RAX}"));
        asm.line(".copy:");
        asm.asm(&format!("cmp  {RAX}, r15"));
        asm.asm("jae  .copied");
        asm.asm(&format!("mov  r10d, [r12+{RAX}*{CHAR_BYTES}]"));
        asm.asm(&format!("mov  [r11+{RAX}*{CHAR_BYTES}], r10d"));
        asm.asm(&format!("inc  {RAX}"));
        asm.asm("jmp  .copy");
        asm.line(".copied:");
        asm.comment("the count last of all, once the characters it promises are there");
        asm.asm("lea  r10, [r14+r15]");
        asm.asm(&format!("mov  [rbx-{STR_HEADER}], r10"));
        asm.asm(&format!("mov  {RAX}, rbx"));
        frame.ret(asm);

        asm.line(".fallback:");
        asm.comment("nothing to grow into, so this is an ordinary join");
        asm.asm(&format!("mov  {a0}, rbx"));
        asm.asm(&format!("mov  {a1}, r12"));
        asm.asm(&format!("call {}", runtime_symbol(Runtime::Concat)));
        frame.ret(asm);
    }

    if used.str_eq {
        asm.blank();
        asm.comment("a == b: the same length and the same characters, never the same address");
        asm.line(&format!("{}:", runtime_symbol(Runtime::StrEq)));
        asm.comment(&format!("{a0} = a, {a1} = b -> rax = 1 when they hold the same characters"));
        asm.asm(&format!("mov  {RAX}, [{a0}-8]"));
        asm.asm(&format!("cmp  {RAX}, [{a1}-8]"));
        asm.comment("different lengths settle it without looking at a character");
        asm.asm("jne  .differ");
        asm.asm(&format!("mov  r10, {RAX}"));
        asm.line(".next:");
        asm.asm("test r10, r10");
        asm.asm("jz   .same");
        // The arguments themselves walk the two strings: an argument register
        // is caller-saved in both conventions, so it is this routine's to spend.
        asm.asm(&format!("mov  r11d, [{a0}]"));
        asm.asm(&format!("cmp  r11d, [{a1}]"));
        asm.asm("jne  .differ");
        asm.asm(&format!("add  {a0}, 4"));
        asm.asm(&format!("add  {a1}, 4"));
        asm.asm("dec  r10");
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
        asm.comment(&format!("{a0} = n -> rax = n"));
        asm.asm(&format!("mov  {RAX}, {a0}"));
        asm.comment("unsigned, so a negative n is enormous and fails the same test");
        asm.asm(&format!("cmp  {a0}, 0x10FFFF"));
        asm.asm(&format!("ja   {ABORT_BAD_CHAR}"));
        asm.comment("the surrogate block in the middle names nothing either");
        asm.asm(&format!("mov  r10, {a0}"));
        asm.asm("sub  r10, 0xD800");
        asm.asm("cmp  r10, 0x7FF");
        asm.asm(&format!("jbe  {ABORT_BAD_CHAR}"));
        asm.asm("ret");
    }

    if used.char_str {
        asm.blank();
        asm.comment("string(c): a string of exactly one character");
        asm.line(&format!("{}:", runtime_symbol(Runtime::CharToStr)));
        asm.comment(&format!("{a0} = the character -> rax = the string"));
        let frame = StubFrame::enter(asm, abi, &["rbx"], 0, "");
        asm.asm(&format!("mov  rbx, {a0}"));
        asm.asm(&format!(
            "mov  {a0}, {}    ; the count, then the one character",
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
        asm.comment(&format!("{a0} = n -> rax = its text"));
        // Twenty digits and a sign is the longest an int can be.
        let frame = StubFrame::enter(
            asm,
            abi,
            &["rbx", "r12", "r13", "r14"],
            24,
            "the digits",
        );
        asm.asm("xor  r14, r14    ; how many digits so far");
        asm.asm(&format!("lea  r13, {}", frame.local(0)));
        asm.asm(&format!("mov  rbx, {a0}    ; keep n, to read its sign back"));
        asm.asm(&format!("mov  {RAX}, {a0}"));
        asm.comment("work on the negative side: the most negative int has no positive twin");
        asm.asm(&format!("test {RAX}, {RAX}"));
        asm.asm("js   .digits");
        asm.asm(&format!("neg  {RAX}"));
        asm.line(".digits:");
        asm.asm("mov  r12, 10");
        asm.line(".next:");
        asm.asm("cqo");
        asm.asm("idiv r12");
        asm.comment("the remainder carries the dividend's sign, so it comes back up");
        asm.asm(&format!("neg  {RDX}"));
        asm.asm(&format!("add  {RDX}, 48    ; '0'"));
        asm.asm("mov  [r13+r14], dl");
        asm.asm("inc  r14");
        asm.asm(&format!("test {RAX}, {RAX}"));
        asm.asm("jnz  .next");
        asm.asm("test rbx, rbx");
        asm.asm("jns  .unsigned");
        asm.asm("mov  byte [r13+r14], 45    ; '-'");
        asm.asm("inc  r14");
        asm.line(".unsigned:");
        asm.asm(&format!("lea  {a0}, [r14*{CHAR_BYTES}+{STR_HEADER}]"));
        asm.asm(&format!("call {ALLOC}"));
        asm.asm(&format!("mov  [{RAX}], r14"));
        asm.asm(&format!("add  {RAX}, 8"));
        asm.comment("the digits came out backwards, so they go back front to back");
        asm.asm("mov  r10, r14");
        asm.asm(&format!("mov  r11, {RAX}"));
        asm.line(".copy:");
        asm.asm("dec  r10");
        asm.asm("movzx edx, byte [r13+r10]");
        asm.asm("mov  [r11], edx");
        asm.asm("add  r11, 4");
        asm.asm("test r10, r10");
        asm.asm("jnz  .copy");
        frame.ret(asm);
    }
}

// -- lists -----------------------------------------------------------------

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
pub fn list_stubs(asm: &mut Asm, abi: &Abi, used: &Used) {
    let (a0, a1, a2, a3) = (abi.arg(0), abi.arg(1), abi.arg(2), abi.arg(3));

    if used.list_new {
        asm.blank();
        asm.comment("room for n elements, with the length already set");
        asm.line(&format!("{}:", runtime_symbol(Runtime::ListNew)));
        asm.comment(&format!("{a0} = how many elements, {a1} = bytes each -> rax = the list"));
        let frame =
            StubFrame::enter(asm, abi, &["rbx", "r12", "r13"], 0, "");
        asm.asm(&format!("mov  r12, {a0}    ; the length"));
        asm.asm(&format!("mov  r13, {a1}    ; bytes per element"));
        asm.asm(&format!("mov  rbx, {a0}"));
        asm.comment("never less than four, so the first few pushes need no move");
        asm.asm(&format!("cmp  rbx, {LIST_MIN_CAPACITY}"));
        asm.asm("jae  .big_enough");
        asm.asm(&format!("mov  rbx, {LIST_MIN_CAPACITY}"));
        asm.line(".big_enough:");
        asm.asm(&format!("mov  {a0}, rbx"));
        asm.asm(&format!("imul {a0}, r13"));
        asm.asm(&format!("add  {a0}, 16    ; the two counts, then the elements"));
        asm.asm(&format!("call {ALLOC}"));
        asm.asm(&format!("mov  [{RAX}], rbx    ; capacity"));
        asm.asm(&format!("mov  [{RAX}+8], r12    ; length"));
        asm.asm(&format!("add  {RAX}, 16    ; and the value is where the elements start"));
        frame.ret(asm);
    }

    // One routine makes the room; the two pushes differ only in what they put
    // in it. Growing is the whole of the difficulty, and it happens once.
    if used.list_room() {
        asm.blank();
        asm.comment("one more element's worth of room, and where it goes");
        asm.line(&format!("{LIST_ROOM}:"));
        // `rdx` answers the second half. It is an argument register in both
        // conventions, and that is exactly why it is free here: this routine
        // takes two arguments, so nothing is waiting in the third.
        asm.comment(&format!(
            "{a0} = the list, {a1} = bytes each -> rax = the list, {RDX} = the new slot"
        ));
        let frame = StubFrame::enter(
            asm,
            abi,
            &["rbx", "r12", "r13", "r14"],
            0,
            "",
        );
        asm.asm(&format!("mov  r12, {a0}"));
        asm.asm(&format!("mov  r13, {a1}    ; bytes per element"));
        asm.asm("mov  rbx, [r12-8]    ; length");
        asm.asm("mov  r14, [r12-16]   ; capacity");
        asm.asm("cmp  rbx, r14");
        asm.asm("jb   .room");

        asm.comment("Full. The arena only ever hands out the bytes after the last");
        asm.comment("ones, so a block that is *still* the last it gave can simply be");
        asm.comment("made longer where it stands: no second block, no copy, and");
        asm.comment("nothing abandoned. That is the whole of what a bump pointer can");
        asm.comment("give back, and it is what a list built one push at a time — a");
        asm.comment("line of input, say — does on every doubling.");
        asm.comment("");
        asm.comment("Two things have to hold, and a `no` to either costs a copy and");
        asm.comment("never an answer: the block ends exactly where the next one would");
        asm.comment("begin, and it lies in the chunk that pointer is bumping through");
        asm.comment("rather than in an older one that happens to end there.");
        asm.asm("mov  r10, r14");
        asm.asm("imul r10, r13");
        asm.asm("add  r10, 16+15    ; the header, and the arena's rounding");
        asm.asm("and  r10, -16");
        asm.asm("lea  r10, [r12+r10-16]    ; where this block ends");
        asm.asm("lea  r14, [r14*2]    ; the room wanted, from here on");
        asm.asm(&format!("cmp  r10, [{ARENA_NEXT}]"));
        asm.asm("jne  .move    ; something has been handed out since");
        asm.asm("lea  r10, [r12-16]");
        asm.asm(&format!("cmp  r10, [{ARENA_CHUNK}]"));
        asm.asm("jb   .move    ; an older chunk's block, ending by coincidence");
        asm.asm("mov  r11, r14");
        asm.asm("imul r11, r13");
        asm.asm("add  r11, 16+15");
        asm.asm("and  r11, -16");
        asm.asm("lea  r11, [r12+r11-16]    ; where it would end");
        asm.asm(&format!("cmp  r11, [{ARENA_END}]"));
        asm.asm("ja   .move    ; this chunk has no room for the rest");
        asm.asm(&format!("mov  [{ARENA_NEXT}], r11"));
        asm.asm("mov  [r12-16], r14    ; grown where it stands");
        asm.asm("jmp  .room");

        asm.line(".move:");
        asm.comment("something else has been handed out since, so the elements move");
        asm.comment("and the old block is left where it is");
        asm.asm(&format!("mov  {a0}, r14"));
        asm.asm(&format!("imul {a0}, r13"));
        asm.asm(&format!("add  {a0}, 16"));
        asm.asm(&format!("call {ALLOC}"));
        asm.asm(&format!("mov  [{RAX}], r14"));
        asm.asm(&format!("add  {RAX}, 16"));
        asm.comment("the elements, as words: every width is a multiple of eight");
        asm.asm(&format!("mov  r10, {RAX}    ; where they go"));
        asm.asm("mov  r11, rbx");
        asm.asm("imul r11, r13");
        asm.asm("shr  r11, 3");
        asm.asm(&format!("xor  {RDX}, {RDX}"));
        asm.line(".copy:");
        asm.asm(&format!("cmp  {RDX}, r11"));
        asm.asm("jae  .copied");
        asm.asm(&format!("mov  {RAX}, [r12+{RDX}*8]"));
        asm.asm(&format!("mov  [r10+{RDX}*8], {RAX}"));
        asm.asm(&format!("inc  {RDX}"));
        asm.asm("jmp  .copy");
        asm.line(".copied:");
        asm.asm("mov  r12, r10");
        asm.line(".room:");
        asm.comment("the new element goes where the ones before it stop");
        asm.asm(&format!("mov  {RAX}, rbx"));
        asm.asm(&format!("imul {RAX}, r13"));
        asm.asm(&format!("lea  {RDX}, [r12+{RAX}]"));
        asm.asm("inc  rbx");
        asm.asm("mov  [r12-8], rbx    ; the length lives with the elements");
        asm.asm(&format!("mov  {RAX}, r12"));
        frame.ret(asm);
    }

    if used.list_push {
        asm.blank();
        asm.comment("one more element, answering where the list ended up");
        asm.line(&format!("{}:", runtime_symbol(Runtime::ListPush)));
        asm.comment(&format!("{a0} = the list, {a1} = the value -> rax = the list, possibly moved"));
        let frame = StubFrame::enter(asm, abi, &["rbx"], 0, "");
        asm.asm(&format!("mov  rbx, {a1}    ; the value, over the call"));
        asm.asm(&format!("mov  {a1}, 8      ; what fits in a register is eight bytes"));
        asm.asm(&format!("call {LIST_ROOM}"));
        asm.asm(&format!("mov  [{RDX}], rbx"));
        frame.ret(asm);
    }

    if used.list_push_big {
        asm.blank();
        asm.comment("the same, for an element that arrives as an address");
        asm.line(&format!("{}:", runtime_symbol(Runtime::ListPushBig)));
        asm.comment(&format!(
            "{a0} = the list, {a1} = where the element is, {a2} = its size, \
             {a3} = whether it owns anything"
        ));
        // The fourth argument, and the branch that reads it, exist only where
        // some class in the program holds a list. Everywhere else the flag is
        // a constant zero at every call site, so the branch is dead before it
        // is written.
        let saved: &'static [&'static str] = match used.fixup {
            true => &["rbx", "r12", "r13"],
            false => &["rbx", "r12"],
        };
        let frame = StubFrame::enter(asm, abi, saved, 0, "");
        asm.asm(&format!("mov  r12, {a1}    ; where it is now"));
        asm.asm(&format!("mov  rbx, {a2}    ; how much of it"));
        if used.fixup {
            asm.asm(&format!("mov  r13, {a3}    ; and whether a copy of it shares"));
        }
        asm.asm(&format!("mov  {a1}, {a2}"));
        asm.asm(&format!("call {LIST_ROOM}"));
        asm.comment("the list may have moved out from under the source, and the block it");
        asm.comment("moved from is still there — the arena gives nothing back, so a push");
        asm.comment("of one of the list's own elements copies what it was handed");
        asm.asm("shr  rbx, 3");
        asm.asm("xor  r10, r10");
        asm.line(".copy:");
        asm.asm("cmp  r10, rbx");
        asm.asm("jae  .copied");
        asm.asm("mov  r11, [r12+r10*8]");
        asm.asm(&format!("mov  [{RDX}+r10*8], r11"));
        asm.asm("inc  r10");
        asm.asm("jmp  .copy");
        asm.line(".copied:");
        if used.fixup {
            asm.asm("test r13, r13");
            asm.asm("jz   .kept");
            asm.comment("what went in is a copy, and a copy owns nothing yet");
            asm.asm(&format!("mov  r12, {RAX}    ; the list, over the call"));
            asm.asm(&format!("mov  {a0}, {RDX}"));
            asm.asm(&format!("mov  {a1}, 1"));
            asm.asm(&format!("mov  {a2}, 0"));
            asm.asm(&format!("call {FIXUP}"));
            asm.asm(&format!("mov  {RAX}, r12"));
            asm.line(".kept:");
        }
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
        asm.comment(&format!("{a0} = the text -> rax = the number, {RDX} = 1 when there was one"));
        let frame = StubFrame::enter(
            asm,
            abi,
            &["rbx", "r12", "r13", "r14"],
            0,
            "",
        );
        asm.asm(&format!("mov  r12, {a0}"));
        asm.asm("mov  rbx, [r12-8]    ; how many characters");
        asm.asm("xor  r13, r13        ; where we are");
        asm.asm("xor  r14, r14        ; was there a minus?");
        asm.asm("test rbx, rbx");
        asm.asm("jz   .no             ; the empty string spells nothing");
        asm.asm("mov  eax, [r12]");
        asm.asm("cmp  eax, 45    ; '-'");
        asm.asm("jne  .digits");
        asm.asm("mov  r14, 1");
        asm.asm("inc  r13");
        asm.asm("cmp  r13, rbx");
        asm.asm("jae  .no             ; a minus on its own");
        asm.line(".digits:");
        asm.comment("built on the negative side, so the most negative int needs no special case");
        asm.asm(&format!("xor  {RAX}, {RAX}"));
        asm.line(".next:");
        asm.asm("cmp  r13, rbx");
        asm.asm("jae  .complete");
        asm.asm("mov  r10d, [r12+r13*4]");
        asm.asm("sub  r10d, 48    ; '0'");
        asm.asm("cmp  r10d, 9");
        asm.asm("ja   .no             ; unsigned, so it catches both ends");
        asm.asm(&format!("imul {RAX}, {RAX}, 10"));
        asm.asm("jo   .no");
        asm.asm("movsxd r11, r10d");
        asm.asm(&format!("sub  {RAX}, r11"));
        asm.asm("jo   .no");
        asm.asm("inc  r13");
        asm.asm("jmp  .next");
        asm.line(".complete:");
        asm.asm("test r14, r14");
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
        asm.comment(&format!("{a0} = the text -> rax = the number"));
        let frame = StubFrame::enter(asm, abi, &[], 0, "");
        asm.asm(&format!("call {PARSE_INT}"));
        asm.asm(&format!("test {RDX}, {RDX}"));
        asm.asm(&format!("jz   {ABORT_NOT_A_NUMBER}"));
        frame.ret(asm);
    }

    if used.is_int {
        asm.blank();
        asm.comment("is_int(s): whether `int(s)` would answer rather than stop the program");
        asm.line(&format!("{}:", runtime_symbol(Runtime::IsInt)));
        asm.comment(&format!("{a0} = the text -> rax = whether it spells a number"));
        let frame = StubFrame::enter(asm, abi, &[], 0, "");
        asm.asm(&format!("call {PARSE_INT}"));
        asm.asm(&format!("mov  {RAX}, {RDX}"));
        frame.ret(asm);
    }

    if used.chars_str {
        asm.blank();
        asm.comment("string(cs): a list of characters sealed into a string");
        asm.line(&format!("{}:", runtime_symbol(Runtime::CharsToStr)));
        asm.comment(&format!("{a0} = a char[] -> rax = a string of the same characters"));
        let frame = StubFrame::enter(asm, abi, &["rbx", "r12"], 0, "");
        asm.asm(&format!("mov  r12, {a0}"));
        asm.asm("mov  rbx, [r12-8]");
        asm.asm(&format!("lea  {a0}, [rbx*4+8]"));
        asm.asm(&format!("call {ALLOC}"));
        asm.asm(&format!("mov  [{RAX}], rbx"));
        asm.asm(&format!("add  {RAX}, 8"));
        asm.asm("xor  r10, r10");
        asm.line(".copy:");
        asm.asm("cmp  r10, rbx");
        asm.asm("jae  .copied");
        asm.comment("eight bytes wide in a list, four in a string");
        asm.asm("mov  r11d, [r12+r10*8]");
        asm.asm(&format!("mov  [{RAX}+r10*4], r11d"));
        asm.asm("inc  r10");
        asm.asm("jmp  .copy");
        asm.line(".copied:");
        frame.ret(asm);
    }

    if used.list_clone {
        asm.blank();
        asm.comment("a second list holding the same elements — what assigning one costs");
        asm.line(&format!("{}:", runtime_symbol(Runtime::ListClone)));
        asm.comment(&format!(
            "{a0} = the list, {a1} = bytes each, {a2} = whether the elements own \
             anything -> rax = a list of its own"
        ));
        // As with the push: the third argument is only ever anything but zero
        // where some class in the program holds a list.
        let saved: &'static [&'static str] = match used.fixup {
            true => &["rbx", "r12", "r13", "r14"],
            false => &["rbx", "r12", "r13"],
        };
        let frame = StubFrame::enter(asm, abi, saved, 0, "");
        asm.asm(&format!("mov  r12, {a0}"));
        asm.asm(&format!("mov  r13, {a1}    ; bytes per element"));
        if used.fixup {
            asm.asm(&format!("mov  r14, {a2}"));
        }
        asm.asm("mov  rbx, [r12-8]");
        asm.asm(&format!("mov  {a0}, rbx"));
        asm.asm(&format!("mov  {a1}, r13"));
        asm.asm(&format!("call {}", runtime_symbol(Runtime::ListNew)));
        asm.comment("as words, so an element wider than a register costs no more code");
        asm.asm("mov  r10, rbx");
        asm.asm("imul r10, r13");
        asm.asm("shr  r10, 3");
        asm.asm("xor  r11, r11");
        asm.line(".copy:");
        asm.asm("cmp  r11, r10");
        asm.asm("jae  .copied");
        asm.asm(&format!("mov  {RDX}, [r12+r11*8]"));
        asm.asm(&format!("mov  [{RAX}+r11*8], {RDX}"));
        asm.asm("inc  r11");
        asm.asm("jmp  .copy");
        asm.line(".copied:");
        if used.fixup {
            asm.asm("test r14, r14");
            asm.asm("jz   .kept");
            asm.comment("the elements are copies too, and share what the originals hold");
            asm.asm(&format!("mov  r12, {RAX}    ; the new list, over the call"));
            asm.asm(&format!("mov  {a0}, {RAX}"));
            asm.asm(&format!("mov  {a1}, rbx"));
            asm.asm(&format!("mov  {a2}, r13"));
            asm.asm(&format!("call {FIXUP}"));
            asm.asm(&format!("mov  {RAX}, r12"));
            asm.line(".kept:");
        }
        frame.ret(asm);
    }
}

// -- writing text out ------------------------------------------------------

/// Writing text out: the one place the language's characters meet UTF-8.
///
/// A string is four bytes per character inside the program, because that is
/// what makes `s[i]` an address and not a walk. The outside world reads UTF-8,
/// so it is encoded here, at the boundary, and nowhere else.
pub fn text_stubs(asm: &mut Asm, abi: &Abi, used: &Used) {
    let (a0, a1) = (abi.arg(0), abi.arg(1));
    let a0_32 = half(a0);

    asm.blank();
    asm.comment("encode one character as UTF-8");
    asm.line(&format!("{UTF8}:"));
    asm.comment(&format!("{a0} = character, {a1} = where to put it -> rax = bytes written"));
    asm.asm(&format!("cmp  {a0_32}, 0x80"));
    asm.asm("jae  .two");
    asm.asm(&format!("mov  eax, {a0_32}"));
    asm.asm(&format!("mov  [{a1}], al"));
    asm.asm("mov  eax, 1");
    asm.asm("ret");
    asm.line(".two:");
    asm.asm(&format!("cmp  {a0_32}, 0x800"));
    asm.asm("jae  .three");
    for (shift, mask, tag, at) in [(6, 0x1F, 0xC0, 0), (0, 0x3F, 0x80, 1)] {
        utf8_byte(asm, abi, shift, mask, tag, at);
    }
    asm.asm("mov  eax, 2");
    asm.asm("ret");
    asm.line(".three:");
    asm.asm(&format!("cmp  {a0_32}, 0x10000"));
    asm.asm("jae  .four");
    for (shift, mask, tag, at) in [(12, 0x0F, 0xE0, 0), (6, 0x3F, 0x80, 1), (0, 0x3F, 0x80, 2)] {
        utf8_byte(asm, abi, shift, mask, tag, at);
    }
    asm.asm("mov  eax, 3");
    asm.asm("ret");
    asm.line(".four:");
    for (shift, mask, tag, at) in
        [(18, 0x07, 0xF0, 0), (12, 0x3F, 0x80, 1), (6, 0x3F, 0x80, 2), (0, 0x3F, 0x80, 3)]
    {
        utf8_byte(asm, abi, shift, mask, tag, at);
    }
    asm.asm("mov  eax, 4");
    asm.asm("ret");

    if used.print_str {
        asm.blank();
        asm.comment("print a string: encode the whole of it, then write those bytes by count");
        asm.line(&format!("{PRINT_STR}:"));
        asm.comment(&format!("{a0} = the string"));
        let frame = StubFrame::enter(asm, abi, &["rbx", "r12", "r13"], 0, "");
        asm.asm(&format!("mov  r12, {a0}"));
        asm.asm(&format!("mov  rbx, [{a0}-8]"));
        asm.comment("four bytes per character is the most UTF-8 can need; the spare byte");
        asm.comment("keeps an empty string from asking for a buffer of nothing");
        asm.asm(&format!("lea  {a0}, [rbx*4+1]"));
        asm.asm(&format!("cmp  {a0}, [{SCRATCH_CAP}]"));
        asm.asm("jbe  .room");
        asm.comment("the buffer is kept between calls, so a loop of prints allocates once");
        asm.asm(&format!("call {ALLOC}"));
        asm.asm(&format!("mov  [{SCRATCH}], {RAX}"));
        asm.asm(&format!("lea  {a0}, [rbx*4+1]"));
        asm.asm(&format!("mov  [{SCRATCH_CAP}], {a0}"));
        asm.line(".room:");
        asm.asm(&format!("mov  r13, [{SCRATCH}]"));
        asm.line(".next:");
        asm.asm("test rbx, rbx");
        asm.asm("jz   .done");
        asm.asm(&format!("mov  {a0_32}, [r12]"));
        asm.asm(&format!("mov  {a1}, r13"));
        asm.asm(&format!("call {UTF8}"));
        asm.asm(&format!("add  r13, {RAX}"));
        asm.asm("add  r12, 4");
        asm.asm("dec  rbx");
        asm.asm("jmp  .next");
        asm.line(".done:");
        asm.comment("what the encoder wrote is where it stopped, less where it began");
        asm.asm(&format!("mov  {a0}, [{SCRATCH}]"));
        asm.asm(&format!("mov  {a1}, r13"));
        asm.asm(&format!("sub  {a1}, {a0}"));
        asm.asm(&format!("call {WRITE_TEXT}"));
        frame.ret(asm);
    }

    if used.print_char {
        asm.blank();
        asm.comment("print one character: at most four bytes, so the frame is buffer enough");
        asm.line(&format!("{PRINT_CHAR}:"));
        asm.comment(&format!("{a0} = the character"));
        let frame =
            StubFrame::enter(asm, abi, &[], 8, "the encoded character");
        asm.asm(&format!("lea  {a1}, {}", frame.local(0)));
        asm.asm(&format!("call {UTF8}"));
        asm.comment("the encoder answers how many bytes it took, which is what to write");
        asm.asm(&format!("mov  {a1}, {RAX}"));
        asm.asm(&format!("lea  {a0}, {}", frame.local(0)));
        asm.asm(&format!("call {WRITE_TEXT}"));
        frame.ret(asm);
    }
}

/// One byte of a UTF-8 sequence: some bits of the character, under the tag that
/// says which byte of how many this is.
fn utf8_byte(asm: &mut Asm, abi: &Abi, shift: u32, mask: u32, tag: u32, at: u32) {
    asm.asm(&format!("mov  eax, {}", half(abi.arg(0))));
    if shift > 0 {
        asm.asm(&format!("shr  eax, {shift}"));
    }
    asm.asm(&format!("and  eax, {mask:#x}"));
    asm.asm(&format!("or   eax, {tag:#x}"));
    match at {
        0 => asm.asm(&format!("mov  [{}], al", abi.arg(1))),
        _ => asm.asm(&format!("mov  [{}+{at}], al", abi.arg(1))),
    }
}

// -- reading input in ------------------------------------------------------

/// Reading input: the other edge, where UTF-8 becomes characters.
///
/// The compiler buffers stdin itself, in [`INPUT`], rather than going through a
/// `FILE*`. That is what makes `eof` answerable at all without pushing a
/// character back: "has the input run out" becomes a question about this
/// buffer, and the answer costs nothing when the buffer is not empty.
///
/// Everything here is shared but one step. [`refill`] owns the shape — flush
/// what has been printed, read, strip a byte order mark, answer how many bytes
/// arrived — and asks the platform only to *fill the buffer*. That one step is
/// where the two machines genuinely differ: a Linux terminal is a byte stream
/// like any other, and a Windows console is not. See [`super::windows`].
pub fn input_stubs(asm: &mut Asm, platform: &dyn Platform, used: &Used) {
    let abi = platform.abi();
    let bare = abi.bare_call_frame();

    asm.blank();
    asm.comment("is a byte waiting? refills the buffer when it has run dry");
    asm.line(&format!("{READY}:"));
    asm.comment("-> rax = 1 when there is a byte to take, 0 at the end of the input");
    asm.asm(&format!("mov  {RAX}, [{INPUT_POS}]"));
    asm.asm(&format!("cmp  {RAX}, [{INPUT_LEN}]"));
    asm.asm("jb   .waiting");
    asm.comment("the input has been declared over once; it stays over");
    asm.asm(&format!("cmp  qword [{INPUT_DONE}], 0"));
    asm.asm("jne  .spent");
    asm.asm(&format!("sub  rsp, {bare}    ; {}", abi.bare_call_note()));
    asm.asm(&format!("call {REFILL}"));
    asm.asm(&format!("add  rsp, {bare}"));
    asm.asm(&format!("test {RAX}, {RAX}"));
    asm.asm("jz   .spent");
    asm.asm(&format!("mov  [{INPUT_LEN}], {RAX}"));
    asm.asm(&format!("mov  qword [{INPUT_POS}], 0"));
    asm.line(".waiting:");
    asm.asm("mov  eax, 1");
    asm.asm("ret");
    asm.line(".spent:");
    asm.asm("xor  eax, eax");
    asm.asm("ret");

    refill(asm, platform);

    asm.blank();
    asm.comment("take one byte");
    asm.line(&format!("{NEXT_BYTE}:"));
    asm.comment("-> rax = the byte, or -1 at the end of the input");
    asm.asm(&format!("sub  rsp, {bare}    ; {}", abi.bare_call_note()));
    asm.asm(&format!("call {READY}"));
    asm.asm(&format!("add  rsp, {bare}"));
    asm.asm(&format!("test {RAX}, {RAX}"));
    asm.asm("jz   .spent");
    asm.asm(&format!("mov  {RAX}, [{INPUT_POS}]"));
    asm.asm(&format!("lea  r10, [{INPUT}]"));
    asm.asm(&format!("movzx r11d, byte [r10+{RAX}]"));
    asm.asm(&format!("inc  {RAX}"));
    asm.asm(&format!("mov  [{INPUT_POS}], {RAX}"));
    asm.asm(&format!("mov  {RAX}, r11"));
    asm.asm("ret");
    asm.line(".spent:");
    asm.asm(&format!("mov  {RAX}, -1"));
    asm.asm("ret");

    if used.eof {
        asm.blank();
        asm.comment("eof(): the same question, asked the other way round");
        asm.line(&format!("{}:", runtime_symbol(Runtime::Eof)));
        asm.asm(&format!("sub  rsp, {bare}    ; {}", abi.bare_call_note()));
        asm.asm(&format!("call {READY}"));
        asm.asm(&format!("add  rsp, {bare}"));
        asm.comment("nothing was consumed, which is the whole point of asking");
        asm.asm(&format!("xor  {RAX}, 1"));
        asm.asm("ret");
    }

    if !used.read_line {
        return;
    }

    let (a0, a1) = (abi.arg(0), abi.arg(1));

    asm.blank();
    asm.comment("one character's worth of UTF-8, taken from the input");
    asm.line(&format!("{UTF8_DECODE}:"));
    asm.comment(&format!("{a0} = the first byte -> rax = the character it starts"));
    let frame = StubFrame::enter(asm, abi, &["rbx", "r12", "r13"], 0, "");
    asm.asm(&format!("cmp  {}, 0x80", half(a0)));
    asm.asm("jb   .plain");
    asm.asm(&format!("mov  rbx, {a0}"));
    // Which of the three multi-byte forms this is, read off the lead byte.
    for (mask, tag, label) in [(0xE0, 0xC0, ".two"), (0xF0, 0xE0, ".three"), (0xF8, 0xF0, ".four")]
    {
        asm.asm(&format!("mov  eax, {}", half(a0)));
        asm.asm(&format!("and  eax, {mask:#x}"));
        asm.asm(&format!("cmp  eax, {tag:#x}"));
        asm.asm(&format!("je   {label}"));
    }
    asm.comment("anything else is a byte no character starts with");
    asm.asm(&format!("jmp  {ABORT_BAD_UTF8}"));

    asm.line(".plain:");
    asm.asm(&format!("mov  {RAX}, {a0}"));
    asm.asm("jmp  .decoded");

    // `r12` counts the continuation bytes still owed, `r13` is the smallest
    // value this many bytes is allowed to encode.
    for (label, mask, owed, least) in
        [(".two", 0x1F, 1, 0x80), (".three", 0x0F, 2, 0x800), (".four", 0x07, 3, 0x10000)]
    {
        asm.line(&format!("{label}:"));
        asm.asm(&format!("and  ebx, {mask:#x}"));
        asm.asm(&format!("mov  r12, {owed}"));
        asm.asm(&format!("mov  r13, {least:#x}"));
        if label != ".four" {
            asm.asm("jmp  .continuation");
        }
    }

    asm.line(".continuation:");
    asm.asm("test r12, r12");
    asm.asm("jz   .complete");
    asm.asm(&format!("call {NEXT_BYTE}"));
    asm.asm(&format!("cmp  {RAX}, 0"));
    asm.asm(&format!("jl   {ABORT_BAD_UTF8}    ; the input stopped mid-character"));
    asm.asm("mov  r10d, eax");
    asm.asm("and  r10d, 0xC0");
    asm.asm("cmp  r10d, 0x80");
    asm.asm(&format!("jne  {ABORT_BAD_UTF8}"));
    asm.asm("and  eax, 0x3F");
    asm.asm("shl  rbx, 6");
    asm.asm(&format!("or   rbx, {RAX}"));
    asm.asm("dec  r12");
    asm.asm("jmp  .continuation");

    asm.line(".complete:");
    asm.comment("an overlong encoding spells a character in more bytes than it needs");
    asm.asm("cmp  rbx, r13");
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
    let frame = StubFrame::enter(asm, abi, &["rbx", "r12"], 0, "");
    asm.comment("the first byte decides whether there is a line at all");
    asm.asm(&format!("call {NEXT_BYTE}"));
    asm.asm(&format!("cmp  {RAX}, 0"));
    asm.asm(&format!("jl   {ABORT_NO_INPUT}"));
    asm.asm(&format!("mov  r12, {RAX}"));
    asm.asm(&format!("xor  {a0}, {a0}"));
    asm.asm(&format!("mov  {a1}, 8    ; characters are what fits in a register"));
    asm.asm(&format!("call {}", runtime_symbol(Runtime::ListNew)));
    asm.asm(&format!("mov  rbx, {RAX}"));

    asm.line(".next:");
    asm.asm("cmp  r12, 10");
    asm.asm("je   .ended    ; a newline ends the line and is not part of it");
    asm.asm(&format!("mov  {a0}, r12"));
    asm.asm(&format!("call {UTF8_DECODE}"));
    asm.asm(&format!("mov  {a0}, rbx"));
    asm.asm(&format!("mov  {a1}, {RAX}"));
    asm.asm(&format!("call {}", runtime_symbol(Runtime::ListPush)));
    asm.asm(&format!("mov  rbx, {RAX}"));
    asm.asm(&format!("call {NEXT_BYTE}"));
    asm.asm(&format!("mov  r12, {RAX}"));
    asm.asm("cmp  r12, 0");
    asm.asm("jge  .next    ; the input running out ends the line too");

    asm.line(".ended:");
    asm.comment("a carriage return before the newline belongs to the ending");
    asm.asm("mov  r10, [rbx-8]");
    asm.asm("test r10, r10");
    asm.asm("jz   .seal");
    asm.asm("mov  r11d, [rbx+r10*8-8]");
    asm.asm("cmp  r11d, 13");
    asm.asm("jne  .seal");
    asm.asm("dec  r10");
    asm.asm("mov  [rbx-8], r10");

    asm.line(".seal:");
    asm.asm(&format!("mov  {a0}, rbx"));
    asm.asm(&format!("call {}", runtime_symbol(Runtime::CharsToStr)));
    frame.ret(asm);
}

/// Fill the input buffer: flush, read, strip a byte order mark, answer.
///
/// Only the read itself belongs to a platform. Everything around it is the same
/// question on either machine, and the byte order mark in particular is not an
/// operating system's doing at all — it is how some editors spell "this file is
/// UTF-8", and it arrives in a pipe on Linux exactly as it does on Windows.
fn refill(asm: &mut Asm, platform: &dyn Platform) {
    let abi = platform.abi();

    asm.blank();
    asm.comment("fill the input buffer, and answer how much of it is now worth reading");
    asm.line(&format!("{REFILL}:"));
    asm.comment("-> rax = how many bytes are now in the buffer, 0 at the end of the input");
    let frame = StubFrame::enter(
        asm,
        abi,
        &REFILL_LOCALS,
        platform.refill_scratch(),
        "stack arguments",
    );

    asm.comment("about to wait: anything printed so far has to be visible first,");
    asm.comment("or a prompt sits in the C runtime's buffer while the program blocks");
    asm.asm(&format!("xor  {0}, {0}    ; fflush(NULL) empties every stream", abi.arg(0)));
    asm.asm("call fflush");

    // Reached again when a read turned out to hold nothing but a byte order
    // mark, which is not the end of anything.
    asm.line(".read:");
    platform.refill_read(asm, &frame);
    asm.asm(&format!("test {RAX}, {RAX}"));
    asm.asm("jz   .over");

    asm.comment("a byte order mark is how some editors spell `this file is UTF-8`.");
    asm.comment("It is not a character of the text, and only the very first bytes of");
    asm.comment("the input can carry one — so the question is asked exactly once");
    asm.asm(&format!("cmp  qword [{FIRST_READ}], 0"));
    asm.asm("jne  .done");
    asm.asm(&format!("mov  qword [{FIRST_READ}], 1"));
    asm.asm(&format!("cmp  {RAX}, 3"));
    asm.asm("jb   .done");
    asm.asm(&format!("lea  rbx, [{INPUT}]"));
    // Two comparisons rather than one on a doubleword: a doubleword reads a
    // fourth byte that a three-byte read never wrote, and whatever was left
    // there would decide the answer.
    asm.asm("cmp  word [rbx], 0xBBEF");
    asm.asm("jne  .done");
    asm.asm("cmp  byte [rbx+2], 0xBF");
    asm.asm("jne  .done");
    asm.asm(&format!("sub  {RAX}, 3"));
    asm.comment("a mark can arrive on its own — some writers send it in a write of its");
    asm.comment("own — and a read that held nothing else is not the end of anything");
    asm.asm("jz   .read");
    asm.comment("shuffle what is left down over the mark");
    asm.asm("xor  r10, r10");
    asm.line(".shift:");
    asm.asm(&format!("cmp  r10, {RAX}"));
    asm.asm("jae  .done");
    asm.asm("movzx r11d, byte [rbx+r10+3]");
    asm.asm("mov  [rbx+r10], r11b");
    asm.asm("inc  r10");
    asm.asm("jmp  .shift");

    asm.line(".over:");
    asm.asm(&format!("xor  {RAX}, {RAX}"));
    asm.line(".done:");
    frame.ret(asm);
}
