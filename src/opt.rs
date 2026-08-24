//! Stage 4b: IR -> IR.
//!
//! The stage that did not exist. Lowering already folded what it could see —
//! `print(1 + 2 * 3)` reaches the backend as `print int 7` — but it folded
//! *syntax*: as soon as a value went through a variable, it stopped looking.
//!
//! ```text
//! int a = 6;  int b = 7;  println(a + b);   // was: mov, mov, add, jo, printf
//! ```
//!
//! Answering that needs a *pass*: a walk over the finished control flow graph,
//! which knows what reaches each point rather than what was written there. Two
//! run here.
//!
//! * [`propagate`] works out which registers hold a value known at compile
//!   time, and rewrites what it can — an operation on two known operands
//!   becomes its answer, a branch on a known condition becomes a jump, and a
//!   block nothing can reach any more goes away.
//! * [`eliminate_dead`] removes what nothing reads.
//!
//! They run in that order and to a fixpoint, because each feeds the other: a
//! branch folded to a jump can leave a block unreachable, whose disappearance
//! makes a variable constant on the only path that is left.
//!
//! ## The one rule
//!
//! **A pass may change how long a program takes and how much it spells out. It
//! may not change what the program does — including where it stops.**
//!
//! That reads like a platitude until it meets guarded arithmetic. TinyC stops
//! rather than answer wrongly, so an overflow is *observable behaviour*, and
//! two things follow that a language with wrapping arithmetic would not have to
//! think about:
//!
//! * **Folding is allowed exactly when the answer exists.** [`ir::fold_bin`] is
//!   the same function lowering uses, and it answers `None` for anything the
//!   machine would refuse — so `a * b` that overflows stays an instruction, and
//!   the program still stops where it was written.
//! * **Dead code that can fail is not dead.** An unread `%t = mul %a, %b` still
//!   decides whether the program gets any further, so [`Instr::can_fail`] is
//!   what [`eliminate_dead`] asks before removing anything.
//!
//! The same rule settles the one genuinely dangerous rewrite here — see
//! [`substitute`] on why an index this pass just worked out is sometimes
//! deliberately *not* substituted.

use std::collections::HashMap;

use crate::ir::{
    Block, BlockId, Function, Instr, Program, Terminator, VReg, Value, fold_bin, fold_cmp,
    prune_unreachable,
};

/// Optimise every function in the program, in place.
pub fn optimise(program: &mut Program) {
    for function in &mut program.functions {
        optimise_function(function);
    }
}

fn optimise_function(function: &mut Function) {
    // Each round either replaces a register operand with a constant or drops a
    // block, and neither can be undone — so the loop runs out of things to do
    // rather than needing a limit to stop it.
    while propagate(function) {}
    while eliminate_dead(function) {}
}

// -- what a register is known to hold --------------------------------------

/// What is known about one register where the analysis stands.
///
/// The third value of the lattice is *absence*: a register no fact mentions has
/// not been reached yet, which is what lets a loop's back edge start out saying
/// nothing and be corrected on the next round.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Known {
    Const(i64),
    /// Reached by two paths that disagree, or produced by something no pass can
    /// evaluate — a load, a call, a parameter.
    Unknown,
}

/// What is known about every register at one point in the function.
type Facts = HashMap<VReg, Known>;

/// Fold `other` into `into`, answering whether anything changed.
///
/// Two paths that agree on a value keep it; two that disagree lose it. A
/// register only one side mentions is taken as it stands: the other side has
/// not reached a definition of it, and a program cannot read a register on a
/// path that never wrote one.
fn meet_into(into: &mut Facts, other: &Facts) -> bool {
    let mut changed = false;
    for (&reg, &value) in other {
        match into.get(&reg) {
            None => {
                into.insert(reg, value);
                changed = true;
            }
            Some(&held) if held != value => {
                into.insert(reg, Known::Unknown);
                changed |= held != Known::Unknown;
            }
            Some(_) => {}
        }
    }
    changed
}

/// What `value` is known to be.
fn known(value: &Value, facts: &Facts) -> Known {
    match value {
        Value::Const(c) => Known::Const(*c),
        Value::Reg(reg) => facts.get(reg).copied().unwrap_or(Known::Unknown),
    }
}

/// What this instruction is known to produce, given what is known coming in.
///
/// Folding goes through the same two functions lowering uses, which is what
/// keeps the two from ever disagreeing about what `i64::MIN / -1` is — and what
/// makes "the answer does not exist" the reason an operation survives this
/// pass.
fn evaluate(instr: &Instr, facts: &Facts) -> Known {
    let operand = |value: &Value| match known(value, facts) {
        Known::Const(c) => Value::Const(c),
        // A register the fold will refuse, since it looks only at constants.
        Known::Unknown => Value::Reg(VReg(u32::MAX)),
    };
    let answer = match instr {
        Instr::Const { val, .. } => Some(*val),
        Instr::Copy { src, .. } => return known(src, facts),
        Instr::Bin { op, lhs, rhs, .. } => fold_bin(*op, operand(lhs), operand(rhs)),
        Instr::Cmp { op, lhs, rhs, .. } => fold_cmp(*op, operand(lhs), operand(rhs)),
        // Everything else produces an address, a length, or whatever a callee
        // decided — none of them answerable here.
        _ => None,
    };
    answer.map_or(Known::Unknown, Known::Const)
}

/// Walk a block, updating what is known as each instruction writes.
fn transfer(facts: &mut Facts, block: &Block) {
    for instr in &block.instrs {
        let value = evaluate(instr, facts);
        if let Some(dst) = instr.def() {
            facts.insert(dst, value);
        }
    }
}

// -- the pass ---------------------------------------------------------------

/// Work out what is constant everywhere, rewrite what that settles, and drop
/// whatever became unreachable. Answers whether anything changed.
pub fn propagate(function: &mut Function) -> bool {
    let entries = analyse(function);
    let changed = rewrite(function, &entries);
    // A branch that became a jump may have left its other arm reachable by
    // nothing. Blocks are renumbered by the same routine lowering uses.
    let before = function.blocks.len();
    let blocks = std::mem::take(&mut function.blocks);
    function.blocks = prune_unreachable(blocks);
    changed || function.blocks.len() != before
}

/// The forward dataflow: what is known on entry to each block.
///
/// Every block starts knowing nothing at all rather than knowing everything is
/// unknown, which is what makes a loop answerable: the back edge says nothing
/// on the first round and contributes properly once the body has been walked.
/// The lattice is three deep, so the rounds run out.
fn analyse(function: &Function) -> Vec<Facts> {
    let count = function.blocks.len();
    let predecessors = predecessors(function);
    let mut entries: Vec<Facts> = vec![Facts::new(); count];
    let mut exits: Vec<Facts> = vec![Facts::new(); count];

    let mut changed = true;
    while changed {
        changed = false;
        // Forward order, so a block usually sees its predecessors' answers on
        // the same round rather than the next one.
        for b in 0..count {
            for predecessor in &predecessors[b] {
                let (entering, leaving) = (&mut entries[b], &exits[predecessor.0 as usize]);
                changed |= meet_into(entering, leaving);
            }
            let mut leaving = entries[b].clone();
            transfer(&mut leaving, &function.blocks[b]);
            if leaving != exits[b] {
                exits[b] = leaving;
                changed = true;
            }
        }
    }

    entries
}

fn predecessors(function: &Function) -> Vec<Vec<BlockId>> {
    let mut into = vec![Vec::new(); function.blocks.len()];
    for (b, block) in function.blocks.iter().enumerate() {
        for successor in block.term.successors() {
            into[successor.0 as usize].push(BlockId(b as u32));
        }
    }
    into
}

/// Replace what is known, and reduce what that makes reducible.
fn rewrite(function: &mut Function, entries: &[Facts]) -> bool {
    let mut changed = false;
    for (block, entering) in function.blocks.iter_mut().zip(entries) {
        let mut facts = entering.clone();
        for instr in &mut block.instrs {
            changed |= substitute(instr, &facts);
            let value = evaluate(instr, &facts);
            if let Some(dst) = instr.def() {
                // An instruction whose answer is known *is* its answer. Only
                // reached when the fold succeeded, so nothing that could have
                // stopped the program is replaced by something that cannot.
                if let (Known::Const(val), false) =
                    (value, matches!(instr, Instr::Const { .. }))
                {
                    *instr = Instr::Const { dst, val };
                    changed = true;
                }
                facts.insert(dst, value);
            }
        }

        if let Terminator::Branch { cond, .. } | Terminator::Return(Some(cond)) = &mut block.term
            && let Known::Const(c) = known(cond, &facts)
            && !matches!(cond, Value::Const(_))
        {
            *cond = Value::Const(c);
            changed = true;
        }
        // A condition settled here is not a choice any more.
        if let Terminator::Branch { cond: Value::Const(c), then_blk, else_blk } = block.term {
            block.term = Terminator::Jump(if c != 0 { then_blk } else { else_blk });
            changed = true;
        }
    }
    changed
}

/// Put known constants in place of the registers holding them.
///
/// ## The index that is deliberately left alone
///
/// An [`Instr::Elem`] whose index *and* length are both constants carries no
/// bounds check: `sema` settled it while the program was being checked, so
/// there is nothing left to ask. That bargain holds for an index written as a
/// literal. It does not hold for one this pass worked out — nobody proved it is
/// in range, and substituting it would delete the very check that catches it.
///
/// ```text
/// int i = 5;  int[3] xs = [1, 2, 3];  println(xs[i]);
/// ```
///
/// So an index known to be out of range is left as the register it was. The
/// program stops at exactly the place and with exactly the message it would
/// have without this pass — which is the rule, and is why this is not instead
/// reported as an error the compiler could now see: an optimiser that refused
/// programs would make `--no-optimise` a different language, and would refuse
/// code on a path nothing ever takes.
fn substitute(instr: &mut Instr, facts: &Facts) -> bool {
    if let Instr::Elem { base, index, len, .. } = instr {
        let mut changed = fold_value(base, facts) | fold_value(len, facts);
        if let Known::Const(at) = known(index, facts)
            && !matches!(index, Value::Const(_))
            && match len {
                Value::Const(elements) => (0..*elements).contains(&at),
                // A string's or a list's length is never known here, so the
                // check is emitted either way and the index may as well be
                // spelled out.
                Value::Reg(_) => true,
            }
        {
            *index = Value::Const(at);
            changed = true;
        }
        return changed;
    }

    let mut changed = false;
    instr.values_mut(|value| changed |= fold_value(value, facts));
    changed
}

fn fold_value(value: &mut Value, facts: &Facts) -> bool {
    if let Known::Const(c) = known(value, facts)
        && !matches!(value, Value::Const(_))
    {
        *value = Value::Const(c);
        return true;
    }
    false
}

// -- what nothing reads -----------------------------------------------------

/// Remove every instruction whose answer nothing reads. Answers whether
/// anything changed.
///
/// Two things are deliberately kept.
///
/// * **Anything that can fail.** An unread `%t = mul %a, %b` still decides
///   whether the program gets past it, so removing it would move where the
///   program stops. This is the whole reason [`Instr::can_fail`] exists.
/// * **[`Instr::Param`].** The argument arrived whether or not the function
///   reads it, and that instruction is what says where — [`Function::params`]
///   names the register it defines, so dropping it would leave a parameter
///   named by nothing.
///
/// "Read" here means *anywhere in the function*, not "read after this point".
/// A variable keeps one register for its whole life and may be written many
/// times, so a write nothing reads before the next one is dead and is kept
/// anyway. Seeing that needs the backward liveness the register allocator
/// computes, which is a pass this one does not have; every dead *temporary* is
/// caught regardless, because a temporary is written exactly once.
pub fn eliminate_dead(function: &mut Function) -> bool {
    let mut read = vec![false; function.vreg_count()];
    for block in &function.blocks {
        for instr in &block.instrs {
            instr.uses(|reg| read[reg.0 as usize] = true);
        }
        block.term.uses(|reg| read[reg.0 as usize] = true);
    }

    let mut removed = false;
    for block in &mut function.blocks {
        block.instrs.retain(|instr| {
            let dead = match instr.def() {
                Some(dst) => !read[dst.0 as usize],
                None => false,
            };
            let keep = !dead
                || instr.can_fail()
                || instr.is_call()
                || matches!(instr, Instr::Param { .. });
            removed |= !keep;
            keep
        });
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::lower;
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

    fn dump_main(body: &str, optimise_it: bool) -> String {
        let mut ir = ir_of(&format!("{UNKNOWN}fn main() {{\n{body}\n}}\n"));
        if optimise_it {
            optimise(&mut ir);
        }
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
        assert!(printed.contains("add %i, 1"), "{printed}");
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
}
