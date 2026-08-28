//! Registers that are only another name for a register.
//!
//! Two shapes, and SSA is what makes both of them one-line rewrites.
//!
//! * `%b = copy %a`. Every use of `%b` becomes `%a`, and the copy is left for
//!   [`super::dce`] to sweep up. Out of SSA this was not safe at all: `%a`
//!   could be written between the copy and the use, so substituting it would
//!   read the wrong value — which is why nothing did it before.
//! * A block parameter every edge hands the *same* value. It is not a choice,
//!   so it is not a parameter; the value takes its place. An edge handing the
//!   parameter back to itself is not a disagreement — that is what a loop
//!   variable nothing in the body touches looks like — so it does not count.
//!
//! The second is what makes folding a branch pay off twice: the arm that goes
//! away takes its argument with it, and the parameter left with one incoming
//! value stops being a parameter at all.

use std::collections::HashMap;

use crate::ir::{Function, Instr, VReg, Value};

pub fn propagate(function: &mut Function) -> bool {
    let mut replacements: HashMap<VReg, Value> = HashMap::new();
    for block in &function.blocks {
        for instr in &block.instrs {
            if let Instr::Copy { dst, src } = instr {
                replacements.insert(*dst, *src);
            }
        }
    }
    let mut changed = substitute(function, replacements);
    changed |= settled_parameters(function);
    changed
}

/// Replace a parameter every edge agrees on with the value they agree on, and
/// take the parameter off the block.
fn settled_parameters(function: &mut Function) -> bool {
    let mut replacements: HashMap<VReg, Value> = HashMap::new();
    let mut dropped: Vec<Vec<usize>> = vec![Vec::new(); function.blocks.len()];

    for (index, block) in function.blocks.iter().enumerate() {
        for (position, &param) in block.params.iter().enumerate() {
            let mut agreed: Option<Value> = None;
            let mut disagree = false;
            let mut edges = 0;
            for pred in &function.blocks {
                for target in pred.term.targets() {
                    if target.block.0 as usize != index {
                        continue;
                    }
                    edges += 1;
                    let arriving = target.args[position];
                    // The parameter handed back to itself says nothing: it is
                    // the value already agreed on, one iteration later.
                    if matches!(arriving, Value::Reg(reg) if reg == param) {
                        continue;
                    }
                    match agreed {
                        None => agreed = Some(arriving),
                        Some(so_far) => disagree |= !same(so_far, arriving),
                    }
                }
            }
            // No edges at all means an entry block, whose parameters — if the
            // renaming ever gave it any — nothing hands anything.
            if let Some(value) = agreed
                && !disagree
                && edges > 0
            {
                replacements.insert(param, value);
                dropped[index].push(position);
            }
        }
    }

    if replacements.is_empty() {
        return false;
    }

    for (index, positions) in dropped.iter().enumerate() {
        for &position in positions.iter().rev() {
            function.blocks[index].params.remove(position);
            for block in &mut function.blocks {
                for target in block.term.targets_mut() {
                    if target.block.0 as usize == index {
                        target.args.remove(position);
                    }
                }
            }
        }
    }

    substitute(function, replacements);
    true
}

fn same(a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Const(x), Value::Const(y)) => x == y,
        (Value::Reg(x), Value::Reg(y)) => x == y,
        _ => false,
    }
}

/// Rewrite every operand in the function, following chains: `%c = copy %b` and
/// `%b = copy %a` together mean every `%c` is an `%a`.
///
/// A chain cannot loop. Each register has one definition and a definition
/// dominates its uses, so following one only ever moves towards the entry.
pub(super) fn substitute(function: &mut Function, replacements: HashMap<VReg, Value>) -> bool {
    if replacements.is_empty() {
        return false;
    }

    let resolve = |mut value: Value| {
        let mut steps = 0;
        while let Value::Reg(reg) = value {
            match replacements.get(&reg) {
                Some(&next) => value = next,
                None => break,
            }
            steps += 1;
            assert!(steps <= replacements.len(), "a copy chain that loops");
        }
        value
    };

    let mut changed = false;
    for block in &mut function.blocks {
        for instr in &mut block.instrs {
            instr.values_mut(|value| {
                let resolved = resolve(*value);
                changed |= !same(resolved, *value);
                *value = resolved;
            });
        }
        block.term.values_mut(|value| {
            let resolved = resolve(*value);
            changed |= !same(resolved, *value);
            *value = resolved;
        });
    }
    changed
}
