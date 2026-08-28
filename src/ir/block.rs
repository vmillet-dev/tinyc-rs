//! Basic blocks: how one ends, what it is for, and dropping the ones nothing reaches.

use super::{BlockId, Instr, Value, VReg};

/// How a basic block ends. Every block has exactly one.
#[derive(Clone, Debug)]
pub enum Terminator {
    Jump(BlockId),
    /// Continue at `then_blk` when `cond` is non-zero, `else_blk` otherwise.
    Branch { cond: Value, then_blk: BlockId, else_blk: BlockId },
    /// Leave the function, with a value for a function that returns one.
    Return(Option<Value>),
}

impl Terminator {
    pub fn successors(&self) -> Vec<BlockId> {
        match self {
            Terminator::Jump(target) => vec![*target],
            Terminator::Branch { then_blk, else_blk, .. } => vec![*then_blk, *else_blk],
            Terminator::Return(_) => Vec::new(),
        }
    }

    /// Show `visit` every virtual register the terminator reads.
    ///
    /// A callback rather than an iterator or a `Vec`: liveness asks this of
    /// every terminator on every round of its fixpoint, and there is nothing to
    /// allocate for at most one register.
    pub fn uses(&self, mut visit: impl FnMut(VReg)) {
        if let Terminator::Branch { cond: Value::Reg(reg), .. }
        | Terminator::Return(Some(Value::Reg(reg))) = self
        {
            visit(*reg);
        }
    }
}

/// What a block is *for*, which is the half of its name that survives
/// renumbering.
///
/// A label is derived from this and the block's index rather than stored as
/// text, so pruning renumbers a block by assigning a number instead of by
/// editing a string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    /// Where the function starts. Always block 0.
    Entry,
    Then,
    Else,
    /// Where the arms of an `if` meet again.
    Join,
    /// A loop header: it re-tests the condition on every iteration.
    Loop,
    Body,
    /// A `for`'s step, on the occasions it needs a block of its own: a
    /// `continue` has to jump somewhere that still runs it.
    Step,
    /// Where a loop leaves.
    Done,
    /// One `match` arm's body.
    Arm,
    /// Where a `match` tests the next variant, having ruled out the ones before.
    Case,
    /// The right operand of `&&` or `||`, reached only when the left one did
    /// not already settle the answer.
    Rhs,
    /// Where a short-circuited `&&` or `||` lands, carrying the answer its left
    /// operand gave on its own.
    Short,
    /// Opened after a `return`, `break` or `continue` for whatever follows it,
    /// and reached by nothing.
    Unreachable,
}

impl BlockKind {
    fn prefix(self) -> &'static str {
        match self {
            BlockKind::Entry => "entry",
            BlockKind::Then => "then",
            BlockKind::Else => "else",
            BlockKind::Join => "join",
            BlockKind::Loop => "loop",
            BlockKind::Body => "body",
            BlockKind::Step => "step",
            BlockKind::Done => "done",
            BlockKind::Arm => "arm",
            BlockKind::Case => "case",
            BlockKind::Rhs => "rhs",
            BlockKind::Short => "short",
            BlockKind::Unreachable => "unreachable",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Block {
    pub kind: BlockKind,
    /// Position in [`Function::blocks`], repeated here so a block can name
    /// itself.
    pub index: u32,
    pub instrs: Vec<Instr>,
    pub term: Terminator,
}

impl Block {
    /// Assembly label and dump name, e.g. `then0` or `loop2`.
    pub fn label(&self) -> String {
        format!("{}{}", self.kind.prefix(), self.index)
    }
}


/// Drop the blocks nothing can reach, and renumber the survivors.
///
/// Lowering a `return` opens a fresh block for whatever follows it, which is
/// usually nothing at all. Without this pass every function ending in a
/// `return` would carry a stray block, and the backend would dutifully emit a
/// second, unreachable epilogue for it.
pub fn prune_unreachable(blocks: Vec<Block>) -> Vec<Block> {
    let mut reachable = vec![false; blocks.len()];
    let mut stack = vec![BlockId(0)];
    while let Some(id) = stack.pop() {
        let index = id.0 as usize;
        // `replace` answers what the flag was *before* it was set, which is the
        // "have I already been here?" a graph walk needs.
        if std::mem::replace(&mut reachable[index], true) {
            continue;
        }
        stack.extend(blocks[index].term.successors());
    }

    // Old index -> new index, for the terminators that name them.
    let mut renumber = vec![BlockId(0); blocks.len()];
    let mut next = 0;
    for (index, keep) in reachable.iter().enumerate() {
        if *keep {
            renumber[index] = BlockId(next);
            next += 1;
        }
    }

    blocks
        .into_iter()
        .zip(&reachable)
        .filter(|(_, keep)| **keep)
        .enumerate()
        .map(|(index, (mut block, _))| {
            // A label is derived from the index, so renumbering is a single
            // assignment: `else3` becomes `else2` when a block ahead of it went
            // away, and the kind still says where the block came from.
            block.index = index as u32;

            block.term = match block.term {
                Terminator::Jump(target) => Terminator::Jump(renumber[target.0 as usize]),
                Terminator::Branch { cond, then_blk, else_blk } => Terminator::Branch {
                    cond,
                    then_blk: renumber[then_blk.0 as usize],
                    else_blk: renumber[else_blk.0 as usize],
                },
                term @ Terminator::Return(_) => term,
            };
            block
        })
        .collect()
}
