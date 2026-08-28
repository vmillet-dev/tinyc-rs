//! Sparse conditional constant propagation.
//!
//! Two questions answered at once, because neither can be answered alone:
//!
//! * **What is each register?** A constant, or not.
//! * **Which blocks can run?** A branch whose condition is known takes one arm,
//!   and the other arm's block may then be reachable by nothing at all.
//!
//! Doing them separately loses answers in both directions. A block nothing can
//! reach contributes a value to the parameters of its successors, and that
//! value has no business making a variable look unknown; and a variable is only
//! known to be constant once the block that disagreed has been ruled out. So
//! the two run as one analysis — Wegman and Zadeck's — where a value is only
//! ever taken from an edge already shown to be executable.
//!
//! The lattice is three deep and every register only ever moves down it, which
//! is what makes the fixpoint terminate:
//!
//! ```text
//! Unreached          nothing that can run has defined it yet
//!     |
//!  Const(c)          every path that can run gives it c
//!     |
//!  Unknown           two paths disagree, or nothing here can say
//! ```
//!
//! Starting at `Unreached` rather than `Unknown` is the whole reason a loop is
//! answerable: a back edge says nothing on the first round, and is taken into
//! account once the body has actually been walked.

use std::collections::HashMap;

use crate::ir::{
    BlockId, Function, Instr, Terminator, VReg, Value, fold_bin, fold_cmp, prune_unreachable,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Known {
    Unreached,
    Const(i64),
    Unknown,
}

impl Known {
    /// What two paths agreeing on nothing more than this can say.
    fn meet(self, other: Known) -> Known {
        match (self, other) {
            (Known::Unreached, answer) | (answer, Known::Unreached) => answer,
            (Known::Const(a), Known::Const(b)) if a == b => Known::Const(a),
            _ => Known::Unknown,
        }
    }
}

/// Work out what is constant, rewrite what that settles, and drop whatever
/// nothing can reach. Answers whether anything changed.
pub fn propagate(function: &mut Function) -> bool {
    let facts = analyse(function);
    let changed = rewrite(function, &facts);

    let before = function.blocks.len();
    let blocks = std::mem::take(&mut function.blocks);
    function.blocks = prune_unreachable(blocks);
    changed || function.blocks.len() != before
}

struct Facts {
    /// What each register is, once and for the whole function — which is what
    /// SSA is for. Before it, this had to be a map per program point.
    value: Vec<Known>,
    /// Which edges can be taken, one flag per exit of each block.
    ///
    /// Per **edge** and not per block, because a block four edges lead into has
    /// four answers to give its parameters and they arrive one at a time.
    /// Taking "this block can run" as the signal would settle a parameter on
    /// whichever arm was shown executable first and never look at the rest —
    /// which is a `match` returning its first arm's value whatever it matched.
    taken: Vec<Vec<bool>>,
    /// Which blocks some executable edge leads into. The entry is reached by
    /// being where the function starts.
    reached: Vec<bool>,
}

fn analyse(function: &Function) -> Facts {
    let mut facts = Facts {
        value: vec![Known::Unreached; function.vreg_count()],
        taken: function.blocks.iter().map(|b| vec![false; b.term.successors().len()]).collect(),
        reached: vec![false; function.blocks.len()],
    };
    facts.reached[0] = true;

    // Which blocks read each register, so a value that moves down the lattice
    // wakes exactly the blocks that could care. This is what "sparse" means:
    // the alternative is re-walking the whole function on every change.
    //
    // An argument counts as read by the block it is handed *to* as well as by
    // the one handing it over, since that block's parameter is what it decides.
    let mut readers: HashMap<VReg, Vec<BlockId>> = HashMap::new();
    let note = |reg: VReg, id: BlockId, readers: &mut HashMap<VReg, Vec<BlockId>>| {
        let blocks = readers.entry(reg).or_default();
        if !blocks.contains(&id) {
            blocks.push(id);
        }
    };
    for (index, block) in function.blocks.iter().enumerate() {
        let id = BlockId(index as u32);
        for instr in &block.instrs {
            instr.uses(|reg| note(reg, id, &mut readers));
        }
        block.term.uses(|reg| note(reg, id, &mut readers));
        for target in block.term.targets() {
            for argument in &target.args {
                if let Value::Reg(reg) = argument {
                    note(*reg, target.block, &mut readers);
                }
            }
        }
    }

    let mut worklist = vec![BlockId(0)];
    while let Some(block) = worklist.pop() {
        let at = block.0 as usize;
        let mut woken = Vec::new();

        let settle = |reg: VReg, to: Known, facts: &mut Facts, woken: &mut Vec<BlockId>| {
            let held = &mut facts.value[reg.0 as usize];
            let merged = held.meet(to);
            if merged != *held {
                *held = merged;
                woken.extend(readers.get(&reg).into_iter().flatten().copied());
            }
        };

        // A parameter is whatever the edges that can actually run hand it.
        for (position, &param) in function.blocks[at].params.iter().enumerate() {
            let mut arriving = Known::Unreached;
            for (index, pred) in function.blocks.iter().enumerate() {
                for (exit, target) in pred.term.targets().enumerate() {
                    if target.block == block && facts.taken[index][exit] {
                        arriving = arriving.meet(known(&target.args[position], &facts));
                    }
                }
            }
            settle(param, arriving, &mut facts, &mut woken);
        }

        for instr in &function.blocks[at].instrs {
            if let Some(def) = instr.def() {
                let produced = evaluate(instr, &facts);
                settle(def, produced, &mut facts, &mut woken);
            }
        }

        // A block is revisited only when an edge into it becomes executable or
        // a value it reads moves down the lattice. Waking one unconditionally
        // would never terminate, since a graph with a cycle would keep waking
        // itself; an edge, on the other hand, becomes executable exactly once.
        for exit in taken_exits(&function.blocks[at].term, &facts) {
            if !std::mem::replace(&mut facts.taken[at][exit], true) {
                let successor = function.blocks[at].term.targets().nth(exit).expect("an exit");
                facts.reached[successor.block.0 as usize] = true;
                woken.push(successor.block);
            }
        }

        for block in woken {
            if facts.reached[block.0 as usize] && !worklist.contains(&block) {
                worklist.push(block);
            }
        }
    }

    facts
}

/// Which exits this terminator can actually take, given what is known.
///
/// A branch on a condition still `Unreached` takes neither yet: nothing that
/// runs has produced the condition, so nothing has decided the branch either.
fn taken_exits(term: &Terminator, facts: &Facts) -> Vec<usize> {
    match term {
        Terminator::Jump(_) => vec![0],
        Terminator::Branch { cond, .. } => match known(cond, facts) {
            Known::Const(0) => vec![1],
            Known::Const(_) => vec![0],
            Known::Unknown => vec![0, 1],
            Known::Unreached => Vec::new(),
        },
        Terminator::Return(_) => Vec::new(),
    }
}

fn known(value: &Value, facts: &Facts) -> Known {
    match value {
        Value::Const(c) => Known::Const(*c),
        Value::Reg(reg) => facts.value[reg.0 as usize],
    }
}

/// What this instruction produces, given what is known about its operands.
///
/// Folding goes through the same two functions lowering uses, which is what
/// keeps the two from ever disagreeing about what `i64::MIN / -1` is — and what
/// makes "the answer does not exist" the reason an operation survives.
fn evaluate(instr: &Instr, facts: &Facts) -> Known {
    let operand = |value: &Value| match known(value, facts) {
        Known::Const(c) => Value::Const(c),
        // A register the fold will refuse, since it looks only at constants.
        _ => Value::Reg(VReg(u32::MAX)),
    };
    let answer = match instr {
        Instr::Const { val, .. } => Some(*val),
        Instr::Copy { src, .. } => return known(src, facts),
        // `num` travels with the instruction for exactly this reason: the bits
        // of two doubles added as integers are not the sum of anything, and
        // nothing downstream would notice — the answer would simply be wrong.
        Instr::Bin { num, op, lhs, rhs, .. } => fold_bin(*num, *op, operand(lhs), operand(rhs)),
        Instr::Cmp { num, op, lhs, rhs, .. } => fold_cmp(*num, *op, operand(lhs), operand(rhs)),
        // Both directions are folded where lowering could see the operand; what
        // reaches here is a constant only this pass discovered, and it is worth
        // no second copy of the rules deciding when a float has an `int`.
        Instr::Cast { .. } => None,
        // Everything else produces an address, a length, or whatever a callee
        // decided — none of them answerable here.
        _ => None,
    };
    answer.map_or(Known::Unknown, Known::Const)
}

/// Replace what is known, and reduce what that makes reducible.
fn rewrite(function: &mut Function, facts: &Facts) -> bool {
    let mut changed = false;
    for (index, block) in function.blocks.iter_mut().enumerate() {
        if !facts.reached[index] {
            continue;
        }
        for instr in &mut block.instrs {
            changed |= substitute(instr, facts);
            if let Some(dst) = instr.def()
                && let Known::Const(val) = facts.value[dst.0 as usize]
                && !matches!(instr, Instr::Const { .. })
            {
                // An instruction whose answer is known *is* its answer. Only
                // reached when the fold succeeded, so nothing that could have
                // stopped the program is replaced by something that cannot.
                *instr = Instr::Const { dst, val };
                changed = true;
            }
        }

        block.term.values_mut(|value| changed |= fold_value(value, facts));
        // A condition settled here is not a choice any more.
        if let Terminator::Branch { cond: Value::Const(c), then_blk, else_blk } = &block.term {
            let taken = if *c != 0 { then_blk.clone() } else { else_blk.clone() };
            block.term = Terminator::Jump(taken);
            changed = true;
        }
    }
    changed
}

/// Put known constants in place of the registers holding them.
///
/// ## The index that is deliberately left alone
///
/// An [`Instr::Elem`] whose index *and* length are constants carries no bounds
/// check: `sema` settled it. That holds for an index written as a literal, not
/// for one this pass worked out — nobody proved that one is in range, and
/// substituting it would delete the check that catches it.
///
/// ```text
/// int i = 5;  int[3] xs = [1, 2, 3];  println(xs[i]);
/// ```
///
/// So an index known to be out of range stays the register it was, and the
/// program stops where it would have without this pass. Nor is it reported as
/// an error: an optimiser that refused programs would make `--no-optimise` a
/// different language, and would refuse code on a path nothing takes.
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
