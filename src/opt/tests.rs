use super::*;
use crate::ir::{lower, ssa};
use crate::{lexer, parser, sema};

fn ir_of(src: &str) -> Program {
    let ast = parser::parse(&lexer::lex(src).unwrap()).unwrap();
    let types = sema::check(&ast, 4).unwrap();
    lower(&ast, &types).expect("the frames should fit")
}

/// A function whose answer the compiler cannot possibly know, in a language
/// that has no such thing built in. Every test about what the pass must
/// *not* do needs one.
const UNKNOWN: &str = "fn unknown() -> int {\n  return int(read_line());\n}\n";

/// The dump of `main`, without its signature line, as the backend gets it.
fn optimised(body: &str) -> String {
    dump_main(body, true)
}

/// The same, straight from lowering — the other half of every `--emit ir`
/// comparison.
fn raw(body: &str) -> String {
    dump_main(body, false)
}

/// The whole middle of the pipeline, in the order it actually runs. Optimising
/// is only defined on SSA form, so a test that skipped `construct` would be
/// measuring a pass on input it is never given.
fn dump_main(body: &str, optimise_it: bool) -> String {
    let mut ir = ir_of(&format!("{UNKNOWN}fn main() {{\n{body}\n}}\n"));
    ssa::construct(&mut ir);
    if optimise_it {
        optimise(&mut ir);
    }
    ssa::destruct(&mut ir);
    let dump = ir.dump();
    let main = dump.find("fn main(").expect("the entry point");
    let start = main + dump[main..].find(":\n").expect("a signature line") + 2;
    dump[start..].trim_end().to_string() + "\n"
}

/// The same, left in SSA form — for the passes whose whole point is a block
/// parameter.
fn in_ssa(body: &str) -> String {
    let mut ir = ir_of(&format!("{UNKNOWN}fn main() {{\n{body}\n}}\n"));
    ssa::construct(&mut ir);
    optimise(&mut ir);
    let dump = ir.dump();
    let main = dump.find("fn main(").expect("the entry point");
    let start = main + dump[main..].find(":\n").expect("a signature line") + 2;
    dump[start..].trim_end().to_string() + "\n"
}

#[test]
fn a_value_that_went_through_a_variable_is_still_constant() {
    // What lowering cannot see: it folds syntax, and `a` is not a literal.
    assert_eq!(
        optimised("int a = 6;\nint b = 7;\nint c = 2;\nprint(a + b * c);"),
        concat!("entry0:\n", "  0  print int 20\n", "  1  return\n")
    );
}

#[test]
fn the_whole_of_arith_reduces_to_the_numbers_it_prints() {
    // Every line of `examples/arith.tc`, which used to emit an `imul`, a
    // `jo`, an `add` and another `jo` to work out a number written in the
    // source.
    let printed = optimised(
        "int a = 6;\nint b = 7;\nint c = 2;\n\
         print(a + b * c);\nprint((a + b) * c);\nprint(-a + b);\n\
         print(a * b - c * 100);\nprint(a * b / c);\nprint(b % c);",
    );
    for answer in ["20", "26", "1", "-158", "21", "1"] {
        assert!(printed.contains(&format!("print int {answer}\n")), "{printed}");
    }
    // Nothing is left to compute.
    for operation in ["add", "sub", "mul", "div", "rem", "const"] {
        assert!(!printed.contains(operation), "{operation} survived:\n{printed}");
    }
}

#[test]
fn a_condition_settled_here_stops_being_a_branch() {
    let printed = optimised("bool debug = false;\nif (debug) {\n  print(1);\n}\nprint(2);");
    assert!(!printed.contains("branch"), "{printed}");
    assert!(!printed.contains("print int 1"), "the dead arm survived:\n{printed}");
    assert!(printed.contains("print int 2"), "{printed}");
    // And the block it guarded is gone, not merely unreached.
    assert!(!printed.contains("then"), "{printed}");
}

#[test]
fn a_loop_whose_variable_changes_keeps_its_arithmetic() {
    // The back edge disagrees with the entry, so `i` is not constant in the
    // body however constant it starts. Getting this wrong is how an
    // optimiser turns a loop into a wrong answer.
    let printed = optimised("for (int i = 0; i < 3; i = i + 1) {\n  print(i);\n}");
    assert!(printed.contains("add %i.1, 1"), "{printed}");
    assert!(printed.contains("branch"), "{printed}");
    assert!(!printed.contains("print int 0"), "`i` is not 0 in the body:\n{printed}");
}

#[test]
fn what_nothing_reads_and_cannot_fail_is_removed() {
    let printed = optimised("int a = 6;\nbool unread = a < 7;\nprint(a);");
    assert_eq!(printed, concat!("entry0:\n", "  0  print int 6\n", "  1  return\n"));
}

#[test]
fn an_operation_that_could_stop_the_program_is_never_removed() {
    // Nothing reads `unread`, and that is not a reason to drop it: whether
    // it overflows is where this program ends.
    let printed = optimised("int n = unknown();\nint unread = n * n;\nprint(n);");
    assert!(printed.contains("mul"), "the multiplication was dropped:\n{printed}");
}

#[test]
fn an_index_this_pass_worked_out_keeps_its_check_when_it_is_out_of_range() {
    // Substituting the 5 would leave an `elem` with two constants, which
    // the backend emits without a bounds check — for an index nobody ever
    // proved was in range.
    let printed = optimised("int i = 5;\nint[3] xs = [1, 2, 3];\nprint(xs[i]);");
    assert!(printed.contains("elem %xs[%i]"), "the check was optimised away:\n{printed}");
    // The one just inside stays a check-free access, as it always was.
    let fine = optimised("int i = 2;\nint[3] xs = [1, 2, 3];\nprint(xs[i]);");
    assert!(fine.contains("elem %xs[2]"), "{fine}");
}

#[test]
fn nothing_a_program_does_is_left_out() {
    // Every instruction with an effect survives, however little the pass
    // can say about it.
    let printed = optimised("string s = \"hi\";\nprint(s);\nprint(len(s));");
    assert!(printed.contains("straddr"), "{printed}");
    assert_eq!(printed.matches("print").count(), 2, "{printed}");
    assert!(printed.contains("count"), "{printed}");
}

#[test]
fn a_program_with_nothing_to_fold_comes_out_as_it_went_in() {
    // The pass must be a no-op where it has nothing to say, or every dump
    // in the test suite would be measuring the optimiser instead.
    let body = "int n = unknown();\nprint(n + 1);";
    assert_eq!(optimised(body), raw(body));
}

#[test]
fn a_call_is_never_dropped_even_when_its_answer_is_unread() {
    // It reads a line, which is not something an unread result undoes.
    let printed = optimised("int n = unknown();\nprint(1);");
    assert!(printed.contains("call unknown"), "{printed}");
}

// -- what SSA made possible -------------------------------------------------

#[test]
fn a_write_nothing_reads_before_the_next_one_is_dead() {
    // The pass could never see this before. A variable kept one register for
    // its whole life, so `%n` was read *somewhere* and the first write stayed —
    // however plainly the second one overwrote it. In SSA the write is the
    // register, and nothing reads this one.
    let printed = optimised("int n = unknown();\nn = 0;\nprint(n);");
    assert!(printed.contains("print int 0"), "{printed}");
    // The call is still there: it reads a line, which an unread answer does not
    // undo. What went is the assignment of its result.
    assert!(printed.contains("call unknown"), "{printed}");
}

#[test]
fn a_register_that_is_only_another_name_for_one_goes_away() {
    // Out of SSA this substitution was not even safe: `%a` could be written
    // between the copy and the use.
    let printed = optimised("int a = unknown();\nint b = a;\nprint(b + 1);");
    assert!(!printed.contains("copy"), "the copy survived:\n{printed}");
    assert_eq!(printed.matches("call unknown").count(), 1, "{printed}");
}

#[test]
fn the_same_computation_twice_is_computed_once() {
    let printed = optimised("int n = unknown();\nprint(n * n);\nprint(n * n);");
    assert_eq!(printed.matches("mul").count(), 1, "{printed}");
}

#[test]
fn a_computation_is_only_reused_where_the_earlier_one_must_have_run() {
    // `unknown() * unknown()` in one arm cannot stand in for the same
    // expression after the `if`: the arm may not have run, and the earlier
    // multiplication is exactly the thing that decides whether the program got
    // this far at all.
    let printed = optimised(
        "int n = unknown();\nif (n > 0) {\n  print(n * n);\n}\nprint(n * n);",
    );
    assert_eq!(printed.matches("mul").count(), 2, "{printed}");
}

#[test]
fn a_parameter_every_edge_agrees_on_stops_being_one() {
    // Both arms give `n` the same value, so the join is not deciding anything.
    let printed = in_ssa(
        "int c = unknown();\nint n = 0;\nif (c > 0) {\n  n = 7;\n} else {\n  n = 7;\n}\nprint(n);",
    );
    assert!(printed.contains("print int 7"), "{printed}");
    // The join block is still there for control flow to land in; what went is
    // its parameter, and with it the choice it was standing for.
    assert!(!printed.contains("join3("), "a settled parameter survived:\n{printed}");
}

#[test]
fn a_match_keeps_one_arm_per_answer() {
    // Every arm hands the join a different value, so the parameter is a real
    // choice and none of them may be taken for the others. Getting the meet
    // wrong here answers every match with its first arm.
    let printed = in_ssa(
        "int n = unknown();\nprint(match (n) {\n  1 => 10,\n  2 => 20,\n  _ => 30,\n});",
    );
    for answer in ["10", "20", "30"] {
        assert!(printed.contains(&format!("({answer})")), "arm {answer} is gone:\n{printed}");
    }
}

#[test]
fn a_loop_counter_is_not_mistaken_for_the_value_it_starts_at() {
    // The back edge disagrees with the entry, which is the one thing a
    // parameter is there to record.
    let printed = in_ssa("int t = 0;\nfor (int i = 0; i < 3; i = i + 1) {\n  t = t + i;\n}\nprint(t);");
    assert!(printed.contains("loop1(%t.1, %i.1):"), "{printed}");
    assert!(!printed.contains("print int 0"), "the counter was taken as constant:\n{printed}");
}
