//! Dominance: who must be gone through to reach whom.
//!
//! Two answers come out of this and SSA needs both. The **dominator tree** is
//! the order the renaming walk goes in, because a definition is visible exactly
//! where it dominates. The **dominance frontier** of a block is where its
//! dominance runs out — which is precisely the set of blocks that can be
//! reached both through it and around it, and therefore where two definitions
//! meet and a block parameter is needed.
//!
//! The iterative algorithm is Cooper, Harvey and Kennedy's. It is slower than
//! Lengauer-Tarjan in the worst case and faster on the graphs a compiler
//! actually sees, and it is short enough to read.

use crate::ir::{BlockId, Function};

pub struct Dominators {
    /// Immediate dominator of each block. The entry is its own, which is what
    /// makes the walk up terminate without a special case.
    idom: Vec<BlockId>,
    /// Blocks in reverse post-order.
    order: Vec<BlockId>,
    predecessors: Vec<Vec<BlockId>>,
}

impl Dominators {
    pub fn of(function: &Function) -> Dominators {
        let count = function.blocks.len();
        let order = reverse_post_order(function);
        let mut rank = vec![u32::MAX; count];
        for (position, block) in order.iter().enumerate() {
            rank[block.0 as usize] = position as u32;
        }

        let predecessors = predecessors(function);
        let mut idom = vec![None; count];
        idom[0] = Some(BlockId(0));

        let mut changed = true;
        while changed {
            changed = false;
            // The entry dominates itself and nothing decides that, so it is
            // skipped rather than recomputed.
            for &block in order.iter().skip(1) {
                let mut new: Option<BlockId> = None;
                for &pred in &predecessors[block.0 as usize] {
                    if idom[pred.0 as usize].is_none() {
                        continue;
                    }
                    new = Some(match new {
                        None => pred,
                        Some(so_far) => intersect(&idom, &rank, pred, so_far),
                    });
                }
                if new.is_some() && new != idom[block.0 as usize] {
                    idom[block.0 as usize] = new;
                    changed = true;
                }
            }
        }

        Dominators {
            // Every block is reachable — lowering prunes the ones that are not
            // — so every one of them has an immediate dominator by now.
            idom: idom.into_iter().map(|d| d.expect("a reachable block")).collect(),
            order,
            predecessors,
        }
    }

    pub fn idom(&self, block: BlockId) -> BlockId {
        self.idom[block.0 as usize]
    }

    pub fn predecessors(&self, block: BlockId) -> &[BlockId] {
        &self.predecessors[block.0 as usize]
    }

    /// The dominator tree, as the children of each block.
    pub fn children(&self) -> Vec<Vec<BlockId>> {
        let mut children = vec![Vec::new(); self.idom.len()];
        for (index, &parent) in self.idom.iter().enumerate().skip(1) {
            children[parent.0 as usize].push(BlockId(index as u32));
        }
        children
    }

    /// Where each block's dominance runs out.
    ///
    /// A block `b` joining two or more paths is on the frontier of everything
    /// on those paths from each predecessor up to — but not including — `b`'s
    /// own immediate dominator, which is where the paths had not yet parted.
    pub fn frontiers(&self) -> Vec<Vec<BlockId>> {
        let mut frontiers: Vec<Vec<BlockId>> = vec![Vec::new(); self.idom.len()];
        for &block in &self.order {
            let preds = self.predecessors(block);
            if preds.len() < 2 {
                continue;
            }
            for &pred in preds {
                let mut runner = pred;
                while runner != self.idom(block) {
                    let frontier = &mut frontiers[runner.0 as usize];
                    if !frontier.contains(&block) {
                        frontier.push(block);
                    }
                    runner = self.idom(runner);
                }
            }
        }
        frontiers
    }
}

/// The lowest block dominating both, found by walking each up the tree until
/// they meet. Deeper means a larger reverse post-order rank, so "walk the lower
/// one up" is a comparison.
fn intersect(idom: &[Option<BlockId>], rank: &[u32], mut a: BlockId, mut b: BlockId) -> BlockId {
    while a != b {
        while rank[a.0 as usize] > rank[b.0 as usize] {
            a = idom[a.0 as usize].expect("a processed block");
        }
        while rank[b.0 as usize] > rank[a.0 as usize] {
            b = idom[b.0 as usize].expect("a processed block");
        }
    }
    a
}

/// Blocks so that every block comes before all its successors, back edges
/// aside. An explicit stack, because a deeply nested function would otherwise
/// recurse as deep as it nests.
fn reverse_post_order(function: &Function) -> Vec<BlockId> {
    let mut seen = vec![false; function.blocks.len()];
    let mut post = Vec::with_capacity(function.blocks.len());
    let mut stack = vec![(BlockId(0), 0usize)];
    seen[0] = true;

    while let Some((block, next)) = stack.pop() {
        let successors = function.block(block).term.successors();
        match successors.get(next) {
            Some(&successor) => {
                stack.push((block, next + 1));
                if !seen[successor.0 as usize] {
                    seen[successor.0 as usize] = true;
                    stack.push((successor, 0));
                }
            }
            None => post.push(block),
        }
    }

    post.reverse();
    post
}

fn predecessors(function: &Function) -> Vec<Vec<BlockId>> {
    let mut into = vec![Vec::new(); function.blocks.len()];
    for (index, block) in function.blocks.iter().enumerate() {
        for successor in block.term.successors() {
            into[successor.0 as usize].push(BlockId(index as u32));
        }
    }
    into
}
