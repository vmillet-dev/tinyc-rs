//! Which virtual registers are live where, by backward dataflow over the CFG.
//!
//! Two stages ask: [`crate::ir::ssa`], which places a block parameter only where
//! the value it would carry is actually live, and
//! [`crate::codegen::regalloc`], which turns the answer into intervals. One copy
//! rather than two, because the two would otherwise be free to disagree about
//! what a back edge means.
//!
//! Block parameters are handled the same way an instruction's result is: a
//! parameter is written at the very top of its block, and the arguments a
//! terminator hands its successors are read at the very bottom of the block it
//! leaves. That is what makes this correct in SSA form and out of it — outside
//! SSA both lists are empty and the analysis is exactly what it was.

use super::{Function, VReg};

/// A set of virtual registers, as one bit each.
///
/// The dataflow below unions and subtracts these on every round of its
/// fixpoint, and here that is a handful of word operations rather than a hash
/// per register.
#[derive(Clone, PartialEq, Eq)]
pub struct VRegSet {
    words: Vec<u64>,
}

impl VRegSet {
    pub fn new(registers: usize) -> VRegSet {
        VRegSet { words: vec![0; registers.div_ceil(64)] }
    }

    pub fn insert(&mut self, reg: VReg) {
        let (word, bit) = (reg.0 as usize / 64, reg.0 as usize % 64);
        self.words[word] |= 1 << bit;
    }

    pub fn contains(&self, reg: VReg) -> bool {
        let (word, bit) = (reg.0 as usize / 64, reg.0 as usize % 64);
        self.words[word] & (1 << bit) != 0
    }

    /// `self |= other`, answering whether that added anything.
    pub fn union_with(&mut self, other: &VRegSet) -> bool {
        let mut grew = false;
        for (mine, theirs) in self.words.iter_mut().zip(&other.words) {
            let merged = *mine | theirs;
            grew |= merged != *mine;
            *mine = merged;
        }
        grew
    }

    /// `self |= other - excluded`, answering whether that added anything.
    pub fn union_without(&mut self, other: &VRegSet, excluded: &VRegSet) -> bool {
        let mut grew = false;
        for ((mine, theirs), gone) in self.words.iter_mut().zip(&other.words).zip(&excluded.words) {
            let merged = *mine | (theirs & !gone);
            grew |= merged != *mine;
            *mine = merged;
        }
        grew
    }

    pub fn iter(&self) -> impl Iterator<Item = VReg> + '_ {
        self.words.iter().enumerate().flat_map(|(word, bits)| {
            (0..64)
                .filter(move |bit| bits & (1 << bit) != 0)
                .map(move |bit| VReg((word * 64 + bit) as u32))
        })
    }
}

/// Live-in and live-out sets, one per block.
pub struct Liveness {
    pub live_in: Vec<VRegSet>,
    pub live_out: Vec<VRegSet>,
}

/// ```text
/// live_out(B) = union of live_in(S) for every successor S of B
/// live_in(B)  = used_before_written(B) + (live_out(B) - written(B))
/// ```
pub fn liveness(function: &Function) -> Liveness {
    let count = function.blocks.len();
    let registers = function.vreg_count();

    // Per block: registers read before being written, and registers written.
    let mut upward_exposed = vec![VRegSet::new(registers); count];
    let mut written = vec![VRegSet::new(registers); count];
    for (b, block) in function.blocks.iter().enumerate() {
        let (exposed, written) = (&mut upward_exposed[b], &mut written[b]);
        for &param in &block.params {
            written.insert(param);
        }
        for instr in &block.instrs {
            instr.uses(|used| {
                if !written.contains(used) {
                    exposed.insert(used);
                }
            });
            if let Some(def) = instr.def() {
                written.insert(def);
            }
        }
        block.term.uses(|used| {
            if !written.contains(used) {
                exposed.insert(used);
            }
        });
    }

    // Both sets only ever grow, so the fixpoint can accumulate in place: a round
    // that adds nothing anywhere is the last one.
    let mut live_in = upward_exposed;
    let mut live_out = vec![VRegSet::new(registers); count];

    let mut changed = true;
    while changed {
        changed = false;
        // Blocks are emitted roughly in forward order, so walking backwards
        // reaches the fixpoint in few passes.
        for b in (0..count).rev() {
            for successor in function.blocks[b].term.successors() {
                // Two different vectors, so both can be borrowed at once.
                let (out, entering) = (&mut live_out[b], &live_in[successor.0 as usize]);
                changed |= out.union_with(entering);
            }
            let (entering, leaving) = (&mut live_in[b], &live_out[b]);
            changed |= entering.union_without(leaving, &written[b]);
        }
    }

    Liveness { live_in, live_out }
}
