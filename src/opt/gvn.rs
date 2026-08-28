//! A computation an earlier, identical one already made.
//!
//! ```text
//! xs[i] = xs[i] + 1;      // the address of xs[i], worked out twice
//! ```
//!
//! Two instructions are the same computation when they are the same operation
//! on the same operands — which is a question about *registers* only because
//! SSA makes a register stand for a value. Out of SSA, `add %a, %b` here and
//! `add %a, %b` there could be adding four different numbers, and answering
//! this needed an analysis of what had been written in between.
//!
//! ## Why dominance is the whole of the safety argument
//!
//! An earlier computation may only replace a later one when it **dominates**
//! it, and the walk below is over the dominator tree precisely so that the only
//! candidates in scope are the ones that do. That is what makes this safe for
//! an operation that can fail: `%t = mul %a, %b` may stop the program, and
//! reusing an earlier one is sound exactly because the earlier one is on every
//! path here — if it was going to overflow, the program already stopped.
//!
//! Anything that reads memory is left alone. `load`, `loadchar` and `count`
//! answer whatever was written last, and nothing here tracks what a `store`, a
//! `copy bytes` or a call did to it.

use std::collections::HashMap;

use crate::ast::{BinOp, CmpOp};
use crate::ir::{Function, Instr, Num, VReg, Value, ssa};

use super::copies;

/// Replace each computation with the dominating one that already made it.
/// Answers whether anything changed.
pub fn eliminate(function: &mut Function) -> bool {
    let mut available: HashMap<Key, VReg> = HashMap::new();
    let mut replacements: HashMap<VReg, Value> = HashMap::new();
    let mut redundant: Vec<Vec<usize>> = vec![Vec::new(); function.blocks.len()];
    let children = ssa::dominator_children(function);

    // Entering a block adds what it computes; leaving takes it back out, since
    // a sibling in the tree is not dominated by it.
    let mut steps = vec![Visit::Enter(0)];
    let mut undo: Vec<Vec<Key>> = vec![Vec::new(); function.blocks.len()];
    while let Some(step) = steps.pop() {
        let at = match step {
            Visit::Leave(at) => {
                for key in undo[at].drain(..) {
                    available.remove(&key);
                }
                continue;
            }
            Visit::Enter(at) => at,
        };
        steps.push(Visit::Leave(at));

        for (position, instr) in function.blocks[at].instrs.iter().enumerate() {
            let Some(dst) = instr.def() else { continue };
            let Some(key) = key(instr) else { continue };
            match available.get(&key) {
                Some(&already) => {
                    replacements.insert(dst, Value::Reg(already));
                    redundant[at].push(position);
                }
                None => {
                    available.insert(key.clone(), dst);
                    undo[at].push(key);
                }
            }
        }

        for &child in &children[at] {
            steps.push(Visit::Enter(child.0 as usize));
        }
    }

    if replacements.is_empty() {
        return false;
    }
    copies::substitute(function, replacements);

    // Removed here rather than left for `dce`, which would have to keep the
    // ones that can fail. This pass is the only one that knows why they may go:
    // an identical computation already ran on every path here, so if this one
    // was going to stop the program, the program has already stopped.
    for (block, positions) in redundant.into_iter().enumerate() {
        for position in positions.into_iter().rev() {
            function.blocks[block].instrs.remove(position);
        }
    }
    true
}

enum Visit {
    Enter(usize),
    Leave(usize),
}

/// What makes two instructions the same computation.
///
/// `None` for anything that reads memory, calls, or has an effect — everything,
/// in other words, whose answer is not decided by its operands alone.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Key {
    Const(i64),
    StrAddr(u32),
    Frame(u32),
    VTable(u32),
    Variant(u32, u32),
    Bin(Num, BinOp, Operand, Operand),
    Cmp(Num, CmpOp, Operand, Operand),
    Cast(Num, Operand),
    Field(Operand, u32),
    Elem(Operand, Operand, Operand, u32),
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Operand {
    Const(i64),
    Reg(u32),
}

fn operand(value: &Value) -> Operand {
    match value {
        Value::Const(c) => Operand::Const(*c),
        Value::Reg(reg) => Operand::Reg(reg.0),
    }
}

fn key(instr: &Instr) -> Option<Key> {
    Some(match instr {
        Instr::Const { val, .. } => Key::Const(*val),
        Instr::StrAddr { id, .. } => Key::StrAddr(id.0),
        Instr::Frame { offset, .. } => Key::Frame(*offset),
        Instr::VTable { class, .. } => Key::VTable(class.0),
        Instr::VariantAddr { id, tag, .. } => Key::Variant(id.0, *tag),
        // Commutative operators sort their operands, so `a + b` and `b + a` are
        // one computation and not two.
        Instr::Bin { num, op, lhs, rhs, .. } => {
            let (lhs, rhs) = ordered(*op, operand(lhs), operand(rhs));
            Key::Bin(*num, *op, lhs, rhs)
        }
        Instr::Cmp { num, op, lhs, rhs, .. } => Key::Cmp(*num, *op, operand(lhs), operand(rhs)),
        Instr::Cast { to, src, .. } => Key::Cast(*to, operand(src)),
        Instr::Field { base, offset, .. } => Key::Field(operand(base), *offset),
        Instr::Elem { base, index, len, scale, .. } => {
            Key::Elem(operand(base), operand(index), operand(len), *scale)
        }
        // A copy is the business of `copies`, and everything left reads memory,
        // calls something, or writes something.
        _ => return None,
    })
}

fn ordered(op: BinOp, lhs: Operand, rhs: Operand) -> (Operand, Operand) {
    let swap = match op {
        BinOp::Add | BinOp::Mul => (lhs, rhs) > (rhs, lhs),
        BinOp::Sub | BinOp::Div | BinOp::Rem => false,
    };
    match swap {
        true => (rhs, lhs),
        false => (lhs, rhs),
    }
}

impl PartialOrd for Operand {
    fn partial_cmp(&self, other: &Operand) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Operand {
    fn cmp(&self, other: &Operand) -> std::cmp::Ordering {
        let rank = |op: &Operand| match op {
            Operand::Const(c) => (0, *c),
            Operand::Reg(r) => (1, *r as i64),
        };
        rank(self).cmp(&rank(other))
    }
}
