//! Registers a copy is the only thing keeping apart.
//!
//! Leaving SSA turns every block parameter into copies at the end of the blocks
//! that jump to it, and most of those copies are between two names for one
//! variable — `%i.1 = copy %i.2` at the bottom of a loop body is the counter
//! being handed back to the header it came from. Emitted as written, each is a
//! `mov` on every iteration, for a value that never needed moving.
//!
//! Two registers may share one if they are **never live at the same time**.
//! That is exactly what the interference graph below answers, and the rule that
//! makes it work for a copy is the one line in the walk that looks odd:
//!
//! ```text
//! %a = copy %b        // %a and %b interfere only if %b is read *after* this
//! ```
//!
//! At the copy, `%a` is written and `%b` read; they hold the same value from
//! here on, so unless `%b` is wanted again later there is no moment where the
//! two have to be different registers.
//!
//! This is where the cost of SSA is paid back. Without it, going into SSA and
//! out again would leave a function with more `mov`s than it started with, and
//! everything the form bought in the middle would be spent at the exit.
//!
//! It is not only for the copies destruction made. A copy lowering emitted —
//! `int b = a;` — goes the same way, which the compiler never used to do.

use std::collections::HashSet;

use super::{Function, Instr, VReg, Value, liveness};

/// Give the registers a copy joins one register between them, wherever they are
/// never both live. Answers whether anything changed.
pub fn copies(function: &mut Function) -> bool {
    let interference = interference(function);
    let mut merged: Vec<VReg> = (0..function.vreg_count() as u32).map(VReg).collect();
    let mut neighbours = interference;
    let mut changed = false;

    for block in &function.blocks {
        for instr in &block.instrs {
            let Instr::Copy { dst, src: Value::Reg(src) } = instr else { continue };
            let (a, b) = (find(&merged, *dst), find(&merged, *src));
            if a == b || neighbours[a.0 as usize].contains(&b) {
                continue;
            }
            // `a` takes `b` over, and inherits everything `b` could not share
            // with — otherwise a later merge would be judged on a stale answer.
            let moved: Vec<VReg> = neighbours[b.0 as usize].drain().collect();
            for other in moved {
                neighbours[other.0 as usize].remove(&b);
                neighbours[other.0 as usize].insert(a);
                neighbours[a.0 as usize].insert(other);
            }
            neighbours[a.0 as usize].remove(&a);
            merged[b.0 as usize] = a;
            changed = true;
        }
    }

    if !changed {
        return false;
    }

    for block in &mut function.blocks {
        for instr in &mut block.instrs {
            instr.values_mut(|value| {
                if let Value::Reg(reg) = value {
                    *value = Value::Reg(find(&merged, *reg));
                }
            });
            if let Some(def) = instr.def() {
                super::ssa::set_def(instr, find(&merged, def));
            }
        }
        block.term.values_mut(|value| {
            if let Value::Reg(reg) = value {
                *value = Value::Reg(find(&merged, *reg));
            }
        });
        // What is left of a copy whose two ends became one register.
        block.instrs.retain(|instr| !matches!(instr, Instr::Copy { dst, src: Value::Reg(src) } if dst == src));
    }
    function.params = function.params.iter().map(|&p| find(&merged, p)).collect();
    true
}

fn find(merged: &[VReg], reg: VReg) -> VReg {
    let mut at = reg;
    while merged[at.0 as usize] != at {
        at = merged[at.0 as usize];
    }
    at
}

/// Which registers are live at the same moment as which.
///
/// Walked backwards through each block from what leaves it, because that is the
/// direction liveness runs: a register is live where it is read and stays live
/// back to where it was written. Everything live at the point a register is
/// **written** is something it cannot share a machine register with.
fn interference(function: &Function) -> Vec<HashSet<VReg>> {
    let live = liveness(function);
    let mut graph: Vec<HashSet<VReg>> = vec![HashSet::new(); function.vreg_count()];

    for (index, block) in function.blocks.iter().enumerate() {
        let mut alive: HashSet<VReg> = live.live_out[index].iter().collect();
        block.term.uses(|reg| {
            alive.insert(reg);
        });

        for instr in block.instrs.iter().rev() {
            if let Some(def) = instr.def() {
                for &other in &alive {
                    if other != def {
                        graph[def.0 as usize].insert(other);
                        graph[other.0 as usize].insert(def);
                    }
                }
                alive.remove(&def);
            }
            // The operands are read here, so they are live from here backwards.
            // A copy's source is added *after* its destination was taken out,
            // which is what leaves the two free to share a register when the
            // source is not wanted again.
            instr.uses(|reg| {
                alive.insert(reg);
            });
        }
    }

    graph
}
