//! Stage 4b: IR -> IR, in SSA form.
//!
//! Lowering folds what it can *see* — `print(1 + 2 * 3)` reaches the backend as
//! `print int 7` — but it folds syntax, and stops looking the moment a value
//! goes through a variable. Four passes run over the finished graph, in a loop,
//! because each makes work for the others: a folded branch leaves a block
//! unreachable, whose disappearance leaves a block parameter with one incoming
//! value, whose removal makes a variable constant.
//!
//! | Pass | What it removes |
//! |------|-----------------|
//! | [`sccp`] | operations on values known at compile time, and arms nothing can take |
//! | [`copies`] | a register that is only another name for one |
//! | [`gvn`] | a computation an earlier, identical one already made |
//! | [`dce`] | anything nothing reads |
//!
//! All four are written knowing that **a register has exactly one definition**,
//! which [`crate::ir::ssa`] establishes just before this stage runs. Each
//! module says what that bought it.
//!
//! ## The one rule
//!
//! **A pass may change how long a program takes and how much it spells out. It
//! may not change what the program does — including where it stops.**
//!
//! TinyC stops rather than answer wrongly, so an overflow is *observable
//! behaviour*, and two things follow:
//!
//! * **Folding is allowed exactly when the answer exists.** [`ir::fold_bin`] is
//!   the same function lowering uses, and it answers `None` for anything the
//!   machine would refuse, so `a * b` that overflows stays an instruction.
//! * **Dead code that can fail is not dead.** An unread `%t = mul %a, %b` still
//!   decides whether the program gets any further — [`Instr::can_fail`].
//!
//! The same rule settles the one genuinely dangerous rewrite here: see
//! [`sccp::substitute`] on the index that is deliberately *not* substituted.

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
