//! Basic blocks: how one ends, where it sends control, and dropping the ones
//! nothing reaches.

use super::{BlockId, Instr, VReg, Value};

/// Where a terminator sends control, and what it hands the block on arrival.
///
/// `args` is one value per [`Block::params`] of `block`, and is empty
/// everywhere outside SSA form — which is to say everywhere except between
/// [`crate::ir::ssa::construct`] and [`crate::ir::ssa::destruct`]. Carrying the
/// values on the *edge* rather than in a phi node at the top of the target is
/// what keeps them in step with the graph: a pass that redirects a jump moves
/// its arguments with it, and one that drops a block drops its arguments with
/// it, neither of which is something to remember to do.
#[derive(Clone, Debug)]
pub struct Target {
    pub block: BlockId,
    pub args: Vec<Value>,
}

impl Target {
    /// A jump that hands the block nothing, which is every jump outside SSA.
    pub fn to(block: BlockId) -> Target {
        Target { block, args: Vec::new() }
    }
}

impl From<BlockId> for Target {
    fn from(block: BlockId) -> Target {
        Target::to(block)
    }
}

/// How a basic block ends. Every block has exactly one.
#[derive(Clone, Debug)]
pub enum Terminator {
    Jump(Target),
    /// Continue at `then_blk` when `cond` is non-zero, `else_blk` otherwise.
    Branch { cond: Value, then_blk: Target, else_blk: Target },
    /// Leave the function, with a value for a function that returns one.
    Return(Option<Value>),
}

impl Terminator {
    /// A jump carrying no arguments.
    pub fn jump(block: BlockId) -> Terminator {
        Terminator::Jump(Target::to(block))
    }

    /// A two-way branch carrying no arguments on either edge.
    pub fn branch(cond: Value, then_blk: BlockId, else_blk: BlockId) -> Terminator {
        Terminator::Branch {
            cond,
            then_blk: Target::to(then_blk),
            else_blk: Target::to(else_blk),
        }
    }

    pub fn successors(&self) -> Vec<BlockId> {
        self.targets().map(|target| target.block).collect()
    }

    pub fn targets(&self) -> impl Iterator<Item = &Target> {
        let (a, b) = match self {
            Terminator::Jump(target) => (Some(target), None),
            Terminator::Branch { then_blk, else_blk, .. } => (Some(then_blk), Some(else_blk)),
            Terminator::Return(_) => (None, None),
        };
        a.into_iter().chain(b)
    }

    pub fn targets_mut(&mut self) -> impl Iterator<Item = &mut Target> {
        let (a, b) = match self {
            Terminator::Jump(target) => (Some(target), None),
            Terminator::Branch { then_blk, else_blk, .. } => (Some(then_blk), Some(else_blk)),
            Terminator::Return(_) => (None, None),
        };
        a.into_iter().chain(b)
    }

    /// Show `visit` every virtual register the terminator reads, the arguments
    /// it hands its successors included.
    ///
    /// A callback rather than an iterator or a `Vec`: liveness asks this of
    /// every terminator on every round of its fixpoint.
    pub fn uses(&self, mut visit: impl FnMut(VReg)) {
        self.values(|value| {
            if let Value::Reg(reg) = value {
                visit(*reg);
            }
        });
    }

    /// Show `visit` every operand, so that it may be read.
    pub fn values(&self, mut visit: impl FnMut(&Value)) {
        if let Terminator::Branch { cond, .. } | Terminator::Return(Some(cond)) = self {
            visit(cond);
        }
        for target in self.targets() {
            target.args.iter().for_each(&mut visit);
        }
    }

    /// Show `visit` every operand, so that it may be replaced.
    pub fn values_mut(&mut self, mut visit: impl FnMut(&mut Value)) {
        if let Terminator::Branch { cond, .. } | Terminator::Return(Some(cond)) = self {
            visit(cond);
        }
        for target in self.targets_mut() {
            target.args.iter_mut().for_each(&mut visit);
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
    /// A block interposed on an edge, to hold the copies SSA leaves behind.
    Edge,
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
            BlockKind::Edge => "edge",
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
    /// Values this block is handed on arrival, one per argument every target
    /// naming it carries. Empty outside SSA form.
    pub params: Vec<VReg>,
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

            for target in block.term.targets_mut() {
                target.block = renumber[target.block.0 as usize];
            }
            block
        })
        .collect()
}
