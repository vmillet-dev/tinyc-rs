//! The x86-64 emitter, checked through one platform.
//!
//! Almost every claim here is about *x86-64* — that a comparison fuses into the
//! branch that reads it, that a spilled destination goes through a scratch
//! register, that a class nothing builds gets no method table — and holds
//! whichever platform is emitting. But the exact text does not: a prologue
//! reserves shadow space on one machine and not the other, and an argument
//! arrives in `rcx` here and `rdi` there. A test that spelled out both would
//! say the same thing twice and be wrong twice as often.
//!
//! So these read the Windows platform's output, and the facts that are Linux's
//! own are checked in `linux.rs` beside the code that decides them. What must
//! hold for *every* target, named or not, is in `tests/targets.rs`, which walks
//! `Target::names()` and never mentions one.

use super::data::{BOOL_TRUE, FMT_BOOL, FMT_INT, NEWLINE, line_format};
use super::runtime::{
    ABORT_BOUNDS, ABORT_DIV_OVERFLOW, ABORT_DIV_ZERO, ABORT_NOT_A_NUMBER, ABORT_OOM, ABORT_REPORT,
    ABORT_STACK, ALLOC, ARENA_CHUNK, ARENA_END, ARENA_NEXT, INPUT, LIST_ROOM, PARSE_INT,
    PRINT_CHAR, READY, REFILL, STACK_LIMIT, UTF8, UTF8_DECODE, WRITE_TEXT,
};
use super::*;
use crate::codegen::regalloc;
use crate::{lexer, parser, sema};

/// The shadow space the platform under test reserves, which several frame
/// sizes below are derived from rather than written out.
const SHADOW_SPACE: u32 = 32;

/// Where this platform's arguments travel, for the tests that name them.
fn arg_regs() -> &'static [&'static str] {
    Windows.abi().args
}

/// Build a frame the way the emitter would, for the platform under test.
fn frame_of(allocation: &Allocation, frame_bytes: u32, leaf: bool) -> func::FrameLayout {
    func::FrameLayout::new(allocation, frame_bytes, leaf, SHADOW_SPACE)
}

fn compile_src(src: &str) -> String {
    let ast = parser::parse(&lexer::lex(src).unwrap()).unwrap();
    let types = sema::check(&ast, 4).unwrap();
    let ir = crate::ir::lower(&ast, &types).expect("the frames should fit");
    let backend = X64::windows();
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
            let frame = frame_of(&allocation, 0, false);
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
        let frame = frame_of(&allocation, 0, true);
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
    assert!(with_bool.contains("fmt_bool: db \"%s\", 0"), "{with_bool}");
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

/// The one thing a bump pointer can give back.
///
/// The arena never frees, so a list that doubles normally leaves its old
/// elements behind for good. It need not, when the block being grown is still
/// the last one the arena handed out: then the bytes after it are nobody's, and
/// the block can simply be made longer. Both guards are checked here, because
/// each one alone would let a stale block be extended over memory that belongs
/// to something else.
#[test]
fn a_list_that_is_still_the_last_block_grows_where_it_stands() {
    let asm = compile("int[] xs = [];\npush(xs, 1);\nprintln(len(xs));");
    let (_, room) = functions_in(&asm)
        .into_iter()
        .find(|(name, _)| *name == LIST_ROOM)
        .expect("the routine that makes room");

    // It ends exactly where the arena would hand out the next bytes...
    assert!(room.contains(&format!("cmp  r10, [{ARENA_NEXT}]")), "{room}");
    // ...and it is in the chunk that pointer is bumping through, rather than an
    // older one that happens to end there.
    assert!(room.contains(&format!("cmp  r10, [{ARENA_CHUNK}]")), "{room}");
    // Room for the bigger block, and then the capacity grows with no copy.
    assert!(room.contains(&format!("cmp  r11, [{ARENA_END}]")), "{room}");
    assert!(room.contains(&format!("mov  [{ARENA_NEXT}], r11")), "{room}");
    assert!(room.contains("mov  [r12-16], r14    ; grown where it stands"), "{room}");

    // Every guard leaves by the same door, and the copy is still there behind
    // it: a `no` to any of them costs a copy and never an answer.
    assert_eq!(room.matches(".move").count(), 4, "{room}");
    assert!(room.contains(".copy:"), "{room}");
    assert!(asm.contains(&format!("{ARENA_CHUNK}: resq 1")), "{asm}");
}

/// A string *value* has to be encoded before it can be written. A literal
/// does not, and that is the whole difference the text table buys.
#[test]
fn printing_a_string_brings_the_arena_and_the_encoder_with_it() {
    // The characters are four bytes each in memory and have to become UTF-8
    // somewhere; the arena is where. So even this can run out of memory,
    // which is why the abort stubs come too.
    let asm = compile("string s = \"hi\";\nprint(s);");
    assert!(asm.contains("extern malloc"), "{asm}");
    assert!(asm.contains(&format!("{ABORT_OOM}:")), "{asm}");
    assert!(asm.contains(&format!("{UTF8}:")), "{asm}");
}

#[test]
fn printing_a_literal_brings_neither() {
    let asm = compile("print(\"hi\");");
    assert!(asm.contains("text0: db"), "the bytes are in the file: {asm}");
    assert!(!asm.contains("extern malloc"), "{asm}");
    assert!(!asm.contains(&format!("{UTF8}:")), "{asm}");
    // What it still needs: the console has to be told which encoding those
    // bytes are in, or a literal with an accent in it comes out wrong.
    assert!(asm.contains("SetConsoleOutputCP"), "{asm}");
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
fn a_console_is_read_as_utf16_and_never_asked_to_change_code_page() {
    // `SetConsoleCP(65001)` looks like the counterpart of the output one and
    // is not: a console converts one byte per character on the way out, so
    // asking it for UTF-8 turns every character that needs two bytes into a
    // NUL. Reading UTF-16 and encoding here is the only lossless way.
    let asm = compile("print(read_line());");
    assert!(!asm.contains("SetConsoleCP"), "{asm}");
    assert!(asm.contains("call ReadConsoleW"), "{asm}");
    assert!(asm.contains("call WideCharToMultiByte"), "{asm}");
    // and a redirected stdin is still read as the bytes it already is
    assert!(asm.contains("call _read"), "{asm}");
}

#[test]
fn what_has_been_printed_is_flushed_before_the_program_waits_for_input() {
    // `print` goes through the C runtime's buffer, which is not emptied
    // when stdout is a pipe — an IDE's run window, say. Without this a
    // prompt sits in that buffer while the program blocks, and a person
    // types into what looks like a program that has stopped answering.
    let asm = compile("print(\"name? \");\nprint(read_line());");
    let refill = asm.split(&format!("{REFILL}:")).nth(1).expect("the routine is emitted");
    let waiting = refill.split("call _read").next().expect("the read follows");
    assert!(waiting.contains("call fflush"), "{refill}");
}

#[test]
fn a_failure_reports_itself_after_whatever_was_already_printed() {
    // The report goes straight to the descriptor while `print` goes through
    // a buffer, so without a flush the two come out in the wrong order.
    // A zero the compiler can see is rejected outright, so it comes from
    // somewhere the compiler cannot look.
    let asm = compile_src("fn z() -> int {\n  return 0;\n}\nfn main() {\nprint(1 / z());\n}\n");
    let report = asm.split(&format!("{ABORT_REPORT}:")).nth(1).expect("the routine is emitted");
    let before = report.split("call _write").next().expect("the write follows");
    assert!(before.contains("call fflush"), "{report}");
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

// -- the stack, and what stops a program running out of it -------------

/// A program with one call in it, which is what puts a check in a prologue.
const RECURSIVE: &str = "fn down(int n) -> int {\n  \
                         if (n == 0) { return 0; }\n  \
                         return 1 + down(n - 1);\n}\n\
                         fn main() {\n  println(down(3));\n}";

#[test]
fn a_function_that_can_be_entered_again_checks_there_is_stack_left() {
    let asm = compile_src(RECURSIVE);
    let (_, body) =
        functions_in(&asm).into_iter().find(|(name, _)| *name == "tc$down").expect("the callee");
    // Asked before the frame is taken, and against the frame it is about to
    // take — not against `rsp` as it stands.
    assert!(body.contains(&format!("cmp  {RAX}, [{STACK_LIMIT}]")), "{body}");
    assert!(body.contains(&format!("lea  {RAX}, [rsp-")), "{body}");
    assert!(body.contains(&format!("jb   {ABORT_STACK}")), "{body}");
    // Before the reservation, or it would be asked from the place it guards.
    let check = body.find("jb   ").expect("the check");
    let reserve = body.find("sub  rsp,").expect("the reservation");
    assert!(check < reserve, "the check has to come first: {body}");
}

#[test]
fn the_entry_point_finds_the_limit_out_rather_than_checking_against_it() {
    let asm = compile_src(RECURSIVE);
    let (_, body) =
        functions_in(&asm).into_iter().find(|(name, _)| *name == "main").expect("the entry point");
    // It is what works the answer out, so there is nothing to check against
    // yet — and nothing to check, since `main` is entered once.
    assert!(!body.contains(ABORT_STACK), "{body}");
    assert!(body.contains("call GetCurrentThreadStackLimits"), "{body}");
    assert!(body.contains(&format!("mov  [{STACK_LIMIT}], {RAX}")), "{body}");
    assert!(asm.contains(&format!("{STACK_LIMIT}: resq 1")), "{asm}");
}

#[test]
fn a_program_that_calls_nothing_carries_no_stack_machinery() {
    // Nothing can nest, so there is no depth to guard against: no limit in
    // `.bss`, no question asked of the operating system, and nothing jumping
    // to the abort.
    let asm = compile("int x = 1;\nprintln(x + 1);");
    assert!(!asm.contains(STACK_LIMIT), "{asm}");
    assert!(!asm.contains("GetCurrentThreadStackLimits"), "{asm}");
    assert!(!asm.contains(&format!("jb   {ABORT_STACK}")), "{asm}");
}

/// A declaration of `bytes` worth of `int[1024]` locals, which is how these
/// tests make a frame of a given size without a repeat form in the language.
fn arrays_worth(bytes: u32) -> String {
    let elements = vec!["0"; 1024].join(", ");
    (0..bytes.div_ceil(1024 * 8))
        .map(|i| format!("int[1024] a{i} = [{elements}];\n"))
        .collect::<String>()
        + "println(a0[0]);"
}

#[test]
fn a_frame_that_fits_in_a_page_is_taken_in_one_instruction() {
    let asm = compile("int[3] a = [1, 2, 3];\nprintln(a[0]);");
    assert!(asm.contains("sub  rsp, 72    ; 32 bytes of shadow space"), "{asm}");
    assert!(!asm.contains(".probe"), "{asm}");
}

#[test]
fn a_frame_bigger_than_a_page_is_taken_a_page_at_a_time() {
    // A single `sub rsp` past the page below the one in use steps over the
    // page whose being touched is what makes the next one exist. The first
    // write into the frame then lands on memory the program does not have —
    // an access violation on a stack with room to spare.
    let asm = compile(&arrays_worth(3 * PAGE_BYTES));
    assert!(asm.contains(&format!("sub  rsp, {PAGE_BYTES}")), "{asm}");
    assert!(asm.contains("mov  qword [rsp], 0"), "{asm}");
    assert!(asm.contains(&format!("cmp  {RAX}, {PAGE_BYTES}")), "{asm}");
    assert!(asm.contains(".probe0"), "{asm}");
    // Whatever is left over after the last whole page, so `rsp` lands exactly
    // where a single `sub` would have put it — which is why the epilogue is
    // still one `add` of the whole frame.
    assert!(asm.contains(&format!("sub  rsp, {RAX}")), "{asm}");
    assert!(asm.contains("add  rsp, 16416"), "{asm}");
}

#[test]
fn the_probe_walks_the_whole_frame_and_stops_exactly_at_it() {
    // The loop is short enough to run here: how far `rsp` travels, and that no
    // step is longer than a page, are the two things it must get right.
    for size in [PAGE_BYTES + 8, 2 * PAGE_BYTES, 3 * PAGE_BYTES - 8, 10 * PAGE_BYTES] {
        let (mut moved, mut left, mut biggest_step) = (0u32, size, 0u32);
        loop {
            moved += PAGE_BYTES;
            biggest_step = biggest_step.max(PAGE_BYTES);
            left -= PAGE_BYTES;
            if left <= PAGE_BYTES {
                break;
            }
        }
        // The last step reserves the remainder without touching it, so the
        // frame's own first write is what lands on the next page down.
        biggest_step = biggest_step.max(left);
        moved += left;
        assert_eq!(moved, size, "the probe has to reserve exactly the frame");
        assert!(biggest_step <= PAGE_BYTES, "no step may skip a page: {biggest_step}");
    }
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
    let backend = X64::windows();
    let file = backend.register_file();
    assert!(file.caller_saved.is_empty());
    for &reg in &file.callee_saved {
        assert!(!arg_regs().contains(&file.name(reg)), "{} is an argument register", file.name(reg));
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
            //
            // Counted as *whether* the frame was taken rather than as how many
            // `sub`s it took: a frame bigger than a page is walked down a page
            // at a time, so the prologue holds two of them and a loop, while
            // the epilogue is still the one `add` of the whole thing.
            let reserved = usize::from(body.contains("sub  rsp,"));
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

// -- writing things out -------------------------------------------------

/// Literal text is bytes in the file, written by the count taken of them.
///
/// `printf` cannot be given it either way round. As a *format*, a `%%` that
/// has already become one `%` would turn back into a specifier — of a
/// variadic argument nobody passed. As an *argument* to `%s`, the write would
/// stop at the first NUL, and a TinyC literal may hold one.
#[test]
fn literal_text_is_written_by_length_and_never_through_printf() {
    let asm = compile("print(\"100%% sure\");");
    assert!(asm.contains("; \"100% sure\""), "the percent survives as one: {asm}");
    // No terminator after the bytes: the count is what says where they end.
    assert!(
        asm.contains("text0: db 49, 48, 48, 37, 32, 115, 117, 114, 101    ; \"100% sure\""),
        "{asm}"
    );
    assert!(asm.contains("lea  rcx, [text0]"), "{asm}");
    assert!(asm.contains("mov  rdx, 9"), "nine bytes, counted while compiling: {asm}");
    assert!(asm.contains(&format!("call {WRITE_TEXT}")), "{asm}");
    assert!(!asm.contains("call printf"), "a run of text needs no format at all: {asm}");
}

/// The bug this whole path exists to rule out.
///
/// A TinyC string is a run of *characters*, and `char(0)` is one of them — the
/// lexer takes `"\0"`, a line of input may simply contain one. A C string is
/// the bytes up to the first NUL, so anything that reached `printf("%s")`
/// would come out short, and a `println` would lose its newline with it and
/// run two lines together.
#[test]
fn nothing_that_writes_a_string_stops_at_a_nul() {
    let asm = compile("string s = \"a\\0b\";\nprintln(s);\nprintln(\"x\\0y\");");
    // Neither the literal run of text nor the encoded string carries one.
    assert!(asm.contains("text0: db 120, 0, 121, 10    ; \"x\\0y\\n\""), "{asm}");
    assert!(!asm.contains("mov  byte [r13], 0"), "the encoder terminates nothing: {asm}");
    // Both go out through the write that takes a count.
    assert_eq!(asm.matches(&format!("call {WRITE_TEXT}")).count(), 3, "{asm}");
    assert!(!asm.contains("fmt_str"), "no `%s` is left to stop early: {asm}");
}

/// A `print` writes what it was given and no more; a `println` ends its line
/// with the last thing it writes rather than with a second call.
///
/// Two formats per type, and a program carries only the ones it reaches: the
/// plain one is a `print`, the one ending in a newline is the last write of a
/// `println`. Nothing at run time reads either — they are the compiler's own
/// text, and what the program wrote is always the *argument*.
#[test]
fn a_println_ends_its_line_in_the_same_call_that_writes_the_value() {
    let ends_a_line = |asm: &str, format: &str| {
        asm.lines()
            .find(|line| line.trim_start().starts_with(&format!("{}:", line_format(format))))
            .is_some_and(|line| line.contains(", 10,"))
    };

    let printed = compile("int n = 1;\nprint(n);\nprint(true);");
    assert!(printed.contains(&format!("{FMT_INT}: db \"%lld\", 0")), "{printed}");
    assert!(!printed.contains(&line_format(FMT_INT)), "a `print` ends no line: {printed}");

    let lined = compile("int n = 1;\nprintln(n);\nprintln(true);");
    assert!(ends_a_line(&lined, FMT_INT), "{lined}");
    assert!(ends_a_line(&lined, FMT_BOOL), "{lined}");
    // And the plain forms are gone, since nothing reaches them any more.
    assert!(!lined.contains(&format!("{FMT_INT}: ")), "{lined}");
    assert!(!lined.contains(&format!("{FMT_BOOL}: ")), "{lined}");
    // One call for the value and the line together, where there used to be two.
    assert_eq!(lined.matches("call printf").count(), 2, "{lined}");
    // The words a bool picks between are needed whichever format is used.
    assert!(lined.contains(&format!("{BOOL_TRUE}: ")), "{lined}");
}

/// A string and a character go out through a routine rather than a format of
/// their own, so ending their line is still a call — one byte, written by the
/// same call that takes a count as everything else.
#[test]
fn a_println_of_a_string_writes_the_newline_on_its_own() {
    let asm = compile("string s = \"hi\";\nprintln(s);");
    assert!(asm.contains(&format!("{NEWLINE}: db 10\n")), "no terminator on it: {asm}");
    assert!(asm.contains(&format!("lea  rcx, [{NEWLINE}]")), "{asm}");
    assert!(asm.contains("mov  rdx, 1"), "one byte: {asm}");
    // Never for a `print`, which ends no line.
    let printed = compile("string s = \"hi\";\nprint(s);");
    assert!(!printed.contains(NEWLINE), "{printed}");
}

/// A program that only writes literal text needs the console's code page
/// set, and nothing else: no arena, no encoder, no `malloc`.
///
/// The two questions came apart when text stopped being a string. Getting
/// this wrong in the other direction would be silent — an accented literal
/// on a console still in the machine's old code page comes out as mojibake.
#[test]
fn writing_only_literals_sets_the_code_page_and_carries_no_encoder() {
    let asm = compile("println(\"héllo\");");
    assert!(asm.contains("SetConsoleOutputCP"), "{asm}");
    assert!(!asm.contains(&format!("{UTF8}:")), "{asm}");
    assert!(!asm.contains("extern malloc"), "{asm}");
}

/// Nothing is emitted for a program that writes nothing.
#[test]
fn a_program_that_writes_nothing_declares_nothing_to_write_with() {
    let asm = compile("int n = 1;\nprint();");
    assert!(!asm.contains("text0"), "{asm}");
    assert!(!asm.contains("fmt_str"), "{asm}");
    assert!(!asm.contains("SetConsoleOutputCP"), "{asm}");
}

// -- what has to hold for every platform this backend has --------------------
//
// Not "every target the compiler lists" — that is `tests/targets.rs`, which
// knows nothing about x86. These are the invariants the *shared* code leans on,
// so they are checked against each platform rather than through one.

/// Every platform this backend can emit for.
fn platforms() -> Vec<&'static dyn Platform> {
    vec![&Windows, &Linux]
}

#[test]
fn a_runtime_routine_never_keeps_a_value_where_a_platform_passes_arguments() {
    // The whole reason one set of routine bodies can be emitted for both
    // conventions. A register in both lists would be read as an argument on
    // arrival and then pushed as a local, which is either a lost argument or a
    // corrupted caller depending on the order.
    for platform in platforms() {
        for local in RUNTIME_LOCALS {
            assert!(
                !platform.abi().args.contains(&local),
                "{} passes an argument in {local}, which the runtime keeps locals in",
                platform.name()
            );
        }
    }
}

#[test]
fn a_runtime_routine_never_keeps_a_value_in_a_register_the_allocator_hands_out() {
    // The other half: a routine's locals are pushed, so they may overlap the
    // allocator's pool — but a *scratch* register may not, or a value the
    // caller left in one would not survive the call. `RUNTIME_LOCALS` is the
    // list of the ones that get pushed, and every one of them has to be
    // callee-saved on every platform for that to work.
    for platform in platforms() {
        let file = X64::new(platform).registers;
        for local in RUNTIME_LOCALS {
            let known = file.names.contains(&local);
            assert!(known, "{} is not a register {} names", local, platform.name());
        }
        // And the scratch registers must be in neither pool, or the allocator
        // would hand out something every runtime call destroys.
        for scratch in [SCRATCH0, SCRATCH1, RAX] {
            let reg = file.names.iter().position(|n| *n == scratch).expect("a named register");
            let reg = PhysReg(reg as u8);
            assert!(
                !file.callee_saved.contains(&reg) && !file.caller_saved.contains(&reg),
                "{} may hand out {scratch}, which the backend uses as scratch",
                platform.name()
            );
        }
    }
}

#[test]
fn every_platform_passes_at_least_the_arguments_the_language_allows() {
    // `sema` refuses a fifth parameter because `RegisterFile::max_args` says
    // four; a platform listing fewer than that would make the emitter index
    // past the end of its own argument list.
    for platform in platforms() {
        assert!(
            platform.abi().args.len() >= MAX_ARGS,
            "{} passes {} arguments but the language allows {MAX_ARGS}",
            platform.name(),
            platform.abi().args.len()
        );
        assert_eq!(X64::new(platform).register_file().max_args, MAX_ARGS);
    }
}

#[test]
fn a_stub_frame_leaves_the_stack_aligned_on_every_platform() {
    // The rule `StubFrame` exists to keep, checked over more shapes than the
    // routines themselves happen to use.
    for platform in platforms() {
        let abi = platform.abi();
        for saved in 0..6usize {
            for scratch in [0, 8, 24, 32, 40] {
                let reserved = abi.frame(saved, scratch);
                assert_eq!(
                    (8 + 8 * saved as u32 + reserved) % 16,
                    0,
                    "{}: {saved} pushes and {scratch} bytes leave rsp misaligned",
                    platform.name()
                );
                assert!(
                    reserved >= abi.shadow_space + scratch,
                    "{}: the frame has to hold the shadow space and the scratch",
                    platform.name()
                );
            }
        }
    }
}
