//! Stage 4b: IR -> IR, in SSA form.
//!
//! Lowering already folds what it can *see* — `print(1 + 2 * 3)` reaches the
//! backend as `print int 7` — but it folds syntax, and stops looking the moment
//! a value goes through a variable. Answering
//!
//! ```text
//! int a = 6;  int b = 7;  println(a + b);
//! ```
//!
//! needs a pass over the finished graph. Four run here, in a loop, because each
//! one makes work for the others: a folded branch leaves a block unreachable,
//! whose disappearance leaves a block parameter with one incoming value, whose
//! removal makes a variable constant, and so on until nothing changes.
//!
//! | Pass | What it removes |
//! |------|-----------------|
//! | [`sccp`] | operations on values known at compile time, and the arms of branches nothing can take |
//! | [`copies`] | a register that is only another name for one |
//! | [`gvn`] | a computation an earlier, identical one already made |
//! | [`dce`] | anything nothing reads |
//!
//! ## What SSA buys them
//!
//! All four are written knowing that **a register has exactly one definition**,
//! which is what [`crate::ir::ssa`] establishes just before this stage runs.
//! Without it each of them would be a dataflow analysis over blocks:
//!
//! * A constant would have to be re-derived at every point, because `%n` here
//!   and `%n` there need not be the same value. In SSA it is a fact about the
//!   register, worked out once.
//! * A copy could not be propagated at all — substituting `%a` for `%b`
//!   requires knowing that `%a` has not been written in between.
//! * Two identical computations could not be merged for the same reason.
//! * A write nothing reads *before the next write* is dead, and seeing that
//!   took a backward liveness pass the old code did not have — so it kept every
//!   dead write to a variable and only ever caught dead temporaries.
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
//!   what [`dce`] asks before removing anything.
//!
//! The same rule settles the one genuinely dangerous rewrite here — see
//! [`sccp::substitute`] on why an index this stage just worked out is sometimes
//! deliberately *not* substituted.

mod copies;
mod dce;
mod gvn;
mod sccp;

#[cfg(test)]
mod tests;

use crate::ir::{Function, Program};

/// Optimise every function in the program, in place.
pub fn optimise(program: &mut Program) {
    for function in &mut program.functions {
        optimise_function(function);
    }
}

fn optimise_function(function: &mut Function) {
    // Each pass only ever removes something — an operand that was a register,
    // a block, a parameter, an instruction — and nothing puts any of them back,
    // so the loop runs out of work rather than needing a limit to stop it.
    while sccp::propagate(function)
        | copies::propagate(function)
        | gvn::eliminate(function)
        | dce::eliminate(function)
    {}
}
