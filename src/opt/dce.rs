//! Anything nothing reads.
//!
//! A mark and sweep from the instructions that matter *whatever* is read of
//! them, back along the operands that feed them. Everything the mark never
//! reaches is removed.
//!
//! Two things are deliberately kept, and both are the one rule:
//!
//! * **Anything that can fail.** An unread `%t = mul %a, %b` still decides
//!   whether the program gets past it, so removing it would move where the
//!   program stops. This is the whole reason [`Instr::can_fail`] exists.
//! * **[`Instr::Param`].** The argument arrived whether or not the function
//!   reads it, and that instruction is what says where — [`Function::params`]
//!   names the register it defines, so dropping it would leave a parameter
//!   named by nothing.
//!
//! ## What SSA changed here
//!
//! "Read" used to mean *anywhere in the function*, which caught every dead
//! temporary and no dead assignment at all: a variable kept one register for
//! its whole life, so a write nothing read before the next write still had that
//! register read somewhere, and stayed. In SSA the write **is** the register,
//! and a dead store is as visible as a dead temporary:
//!
//! ```text
//! int n = expensive();  n = 0;  println(n);
//! ```
//!
//! A block parameter is swept the same way, and takes the arguments every edge
//! hands it out with it — which is how a value only a dead parameter kept alive
//! becomes dead in turn.

use crate::ir::{BlockId, Function, Instr, VReg};

pub fn eliminate(function: &mut Function) -> bool {
    let live = mark(function);
    sweep(function, &live)
}

struct Live {
    instrs: Vec<Vec<bool>>,
    params: Vec<Vec<bool>>,
}

/// Where each register is written, so following an operand back reaches what
/// produced it. In SSA there is exactly one such place per register.
#[derive(Clone, Copy)]
enum Written {
    Instr(BlockId, usize),
    Param(BlockId, usize),
    Nowhere,
}

fn mark(function: &Function) -> Live {
    let mut written = vec![Written::Nowhere; function.vreg_count()];
    for (index, block) in function.blocks.iter().enumerate() {
        let id = BlockId(index as u32);
        for (position, &param) in block.params.iter().enumerate() {
            written[param.0 as usize] = Written::Param(id, position);
        }
        for (position, instr) in block.instrs.iter().enumerate() {
            if let Some(def) = instr.def() {
                written[def.0 as usize] = Written::Instr(id, position);
            }
        }
    }

    let mut live = Live {
        instrs: function.blocks.iter().map(|b| vec![false; b.instrs.len()]).collect(),
        params: function.blocks.iter().map(|b| vec![false; b.params.len()]).collect(),
    };
    let mut wanted: Vec<VReg> = Vec::new();

    for (index, block) in function.blocks.iter().enumerate() {
        for (position, instr) in block.instrs.iter().enumerate() {
            if matters(instr) {
                live.instrs[index][position] = true;
                instr.uses(|reg| wanted.push(reg));
            }
        }
        // A terminator's condition is read whatever happens; its arguments are
        // read only where the parameter they feed turns out to be live, which
        // is worked out below as parameters are marked.
        if let crate::ir::Terminator::Branch { cond, .. }
        | crate::ir::Terminator::Return(Some(cond)) = &block.term
            && let crate::ir::Value::Reg(reg) = cond
        {
            wanted.push(*reg);
        }
    }

    while let Some(reg) = wanted.pop() {
        match written[reg.0 as usize] {
            Written::Instr(block, position) => {
                let at = block.0 as usize;
                if std::mem::replace(&mut live.instrs[at][position], true) {
                    continue;
                }
                function.blocks[at].instrs[position].uses(|reg| wanted.push(reg));
            }
            Written::Param(block, position) => {
                let at = block.0 as usize;
                if std::mem::replace(&mut live.params[at][position], true) {
                    continue;
                }
                // Every edge into the block now really does hand it something.
                for pred in &function.blocks {
                    for target in pred.term.targets() {
                        if target.block == block
                            && let crate::ir::Value::Reg(argument) = target.args[position]
                        {
                            wanted.push(argument);
                        }
                    }
                }
            }
            Written::Nowhere => {}
        }
    }

    live
}

/// Whether this instruction has to stay however little is read of it.
fn matters(instr: &Instr) -> bool {
    instr.def().is_none()
        || instr.can_fail()
        || instr.is_call()
        || matches!(instr, Instr::Param { .. })
}

fn sweep(function: &mut Function, live: &Live) -> bool {
    let mut changed = false;

    for (index, block) in function.blocks.iter_mut().enumerate() {
        let mut position = 0;
        block.instrs.retain(|_| {
            let keep = live.instrs[index][position];
            position += 1;
            changed |= !keep;
            keep
        });
    }

    // A dead parameter goes, and so does what every edge was handing it.
    for index in 0..function.blocks.len() {
        for position in (0..function.blocks[index].params.len()).rev() {
            if live.params[index][position] {
                continue;
            }
            function.blocks[index].params.remove(position);
            for block in &mut function.blocks {
                for target in block.term.targets_mut() {
                    if target.block.0 as usize == index {
                        target.args.remove(position);
                    }
                }
            }
            changed = true;
        }
    }

    changed
}
