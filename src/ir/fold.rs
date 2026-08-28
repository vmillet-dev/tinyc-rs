//! Evaluating an operation now, when its operands are already known.

use crate::ast::{BinOp, CmpOp, LogicOp};
use super::{Num, Value};

/// Evaluate `lhs op rhs` now, when both are already known.
///
/// Answers `None` whenever the machine would not agree with the answer: an
/// operation the CPU would trap on stays an instruction, so the program still
/// fails where it was written instead of in the compiler.
///
/// [`crate::sema`] has usually rejected such a program already, through the
/// same [`BinOp::apply`]; what reaches here is what it could not see.
/// What `-x` subtracts `x` from, which is the zero of whichever kind of number
/// it is.
///
/// A float's is **negative** zero, and that is not a flourish: `-0.0 - x` is
/// exactly `-x` for every value there is, while `0.0 - x` answers `+0.0` where
/// `x` was `+0.0` and `-0.0` was meant. The two zeroes compare equal, so the
/// difference is invisible until something divides by the result — and then it
/// is the difference between `+∞` and `-∞`.
pub(crate) fn zero_to_subtract_from(num: Num) -> Value {
    match num {
        Num::Int => Value::Const(0),
        Num::Float => Value::Const((-0.0f64).to_bits() as i64),
    }
}

/// The same negation, done here because the operand was already known.
pub(crate) fn negate_const(num: Num, value: i64) -> i64 {
    match num {
        // `-i64::MIN` does not fit, and wrapping is what the machine does with
        // it — where `sema` has not already refused the program for it.
        Num::Int => value.wrapping_neg(),
        Num::Float => (-f64::from_bits(value as u64)).to_bits() as i64,
    }
}

/// A float folds by the same arithmetic the machine would do — IEEE-754 in
/// double precision, which is exactly what Rust's `f64` is — so folding one
/// cannot come to a different answer from running it. It never refuses: too
/// large is an infinity and zero into zero is a NaN, and both are values.
pub fn fold_bin(num: Num, op: BinOp, lhs: Value, rhs: Value) -> Option<i64> {
    let (Value::Const(a), Value::Const(b)) = (lhs, rhs) else { return None };
    match num {
        Num::Int => op.apply(a, b),
        Num::Float => {
            let (a, b) = (f64::from_bits(a as u64), f64::from_bits(b as u64));
            let answer = match op {
                BinOp::Add => a + b,
                BinOp::Sub => a - b,
                BinOp::Mul => a * b,
                BinOp::Div => a / b,
                BinOp::Rem => unreachable!("sema rejects `%` on a float"),
            };
            Some(answer.to_bits() as i64)
        }
    }
}

/// What a short-circuiting operator answers when its left operand alone decides.
///
/// Unlike [`fold_bin`] and [`fold_cmp`] this looks at one operand, because that
/// is the whole point: `false && x` is false and `true || x` is true whatever
/// `x` would have been — the same value in both cases, which is what
/// [`LogicOp::short_circuit`] reports. `None` means the right operand still has
/// to run, and covers both "the left one is unknown" and "the left one is known
/// but decided nothing".
pub(crate) fn fold_logic(op: LogicOp, lhs: Value) -> Option<i64> {
    let Value::Const(c) = lhs else { return None };
    let settled = op.short_circuit();
    ((c != 0) == (settled != 0)).then_some(settled)
}

/// The same for a comparison, whose result is the 0 or 1 a `bool` is.
///
/// Comparing two floats is **not** comparing their bits: `-0.0` and `0.0` are
/// equal and spelled differently, and a NaN is equal to nothing including
/// itself. Rust's `f64` operators say exactly that, which is also what the
/// machine's `ucomisd` says, so the two agree by construction.
pub fn fold_cmp(num: Num, op: CmpOp, lhs: Value, rhs: Value) -> Option<i64> {
    let (Value::Const(a), Value::Const(b)) = (lhs, rhs) else { return None };
    let answer = match num {
        Num::Int => match op {
            CmpOp::Eq => a == b,
            CmpOp::Ne => a != b,
            CmpOp::Lt => a < b,
            CmpOp::Le => a <= b,
            CmpOp::Gt => a > b,
            CmpOp::Ge => a >= b,
        },
        Num::Float => {
            let (a, b) = (f64::from_bits(a as u64), f64::from_bits(b as u64));
            match op {
                CmpOp::Eq => a == b,
                CmpOp::Ne => a != b,
                CmpOp::Lt => a < b,
                CmpOp::Le => a <= b,
                CmpOp::Gt => a > b,
                CmpOp::Ge => a >= b,
            }
        }
    };
    Some(i64::from(answer))
}

