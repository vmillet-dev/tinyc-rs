//! Stage 4a: into SSA form, and back out of it.
//!
//! Lowering gives a variable **one** virtual register for its whole life and
//! writes it as often as the program assigns it. That is the smallest thing
//! that works, and it is what makes every interesting question about a value
//! unanswerable: `%n` at one point and `%n` at another need not hold the same
//! thing, so nothing can be said about `%n` — only about `%n` *here*, which
//! takes one dataflow analysis to find out and another to use.
//!
//! [`construct`] rewrites the function so that **every virtual register is
//! written exactly once**. Where two definitions meet, the block they meet in
//! grows a *parameter*, and each edge into it carries the definition that
//! reaches along that edge:
//!
//! ```text
//! entry0:                            entry0:
//!   %n = const 1                       %n = const 1
//!   branch %c ? then1 : else2          branch %c ? then1 : else2
//! then1:                             then1:
//!   %n = add %n, 1                     %n.1 = add %n, 1
//!   jump join3                         jump join3(%n.1)
//! else2:                             else2:
//!   jump join3                         jump join3(%n)
//! join3:                             join3(%n.2):
//!   print int %n                       print int %n.2
//! ```
//!
//! Everything after it gets easier. A register now *has* a value rather than
//! holding a different one at each point, so constant propagation is a walk
//! over definitions rather than a fixpoint over blocks; a copy is removable by
//! substituting its source everywhere; and a write nothing reads is dead on its
//! own account rather than needing a backward liveness pass to prove it.
//!
//! [`destruct`] undoes it before the register allocator ever sees the function,
//! by turning each edge's arguments into copies at the end of the block the
//! edge leaves. Machines have no block parameters: SSA is a form the *middle*
//! of the compiler works in, not something a backend has to understand.
//!
//! ## Where the parameters go
//!
//! At the **iterated dominance frontier** of a variable's definitions — the
//! standard answer, and the one place a value written on one path can meet a
//! value written on another. Two refinements keep the result small:
//!
//! * Only variables written in more than one block are considered. A temporary
//!   is written once, and one definition cannot meet another.
//! * A parameter is placed only where the variable is **live**, which is what
//!   stops a value computed inside a loop from growing a parameter on a header
//!   it is not live on — where the entry edge would have had to hand it a value
//!   that does not exist yet.

mod dom;
mod names;

#[cfg(test)]
mod tests;

use dom::Dominators;
use names::Names;

use super::{
    Block, BlockId, BlockKind, Function, Instr, Program, Target, Terminator, VReg, Value, liveness,
};

/// Put every function into SSA form.
pub fn construct(program: &mut Program) {
    for function in &mut program.functions {
        construct_function(function);
    }
}

/// Take every function back out of SSA form.
pub fn destruct(program: &mut Program) {
    for function in &mut program.functions {
        destruct_function(function);
    }
}

// -- into SSA ---------------------------------------------------------------

fn construct_function(function: &mut Function) {
    let dominators = Dominators::of(function);
    let wanted = parameters_wanted(function, &dominators);
    rename(function, &dominators, &wanted);
}

/// Which variable each block needs a parameter for, in a fixed order.
///
/// The order is the one thing every edge has to agree on, an argument list
/// being positional. Sorting by register number settles it for all of them at
/// once, without anything having to be passed between the edges.
fn parameters_wanted(function: &Function, dominators: &Dominators) -> Vec<Vec<VReg>> {
    let frontiers = dominators.frontiers();
    let live = liveness(function);

    // Where each register is written. A register written in one block only can
    // never meet a second definition of itself.
    let mut written_in: Vec<Vec<BlockId>> = vec![Vec::new(); function.vreg_count()];
    for (index, block) in function.blocks.iter().enumerate() {
        let id = BlockId(index as u32);
        for instr in &block.instrs {
            if let Some(def) = instr.def()
                && !written_in[def.0 as usize].contains(&id)
            {
                written_in[def.0 as usize].push(id);
            }
        }
    }

    let mut wanted: Vec<Vec<VReg>> = vec![Vec::new(); function.blocks.len()];
    for (index, blocks) in written_in.iter().enumerate() {
        if blocks.len() < 2 {
            continue;
        }
        let reg = VReg(index as u32);

        // Iterated, because placing a parameter is itself a definition and has
        // a frontier of its own.
        let mut queue = blocks.clone();
        let mut placed = vec![false; function.blocks.len()];
        while let Some(block) = queue.pop() {
            for &frontier in &frontiers[block.0 as usize] {
                let at = frontier.0 as usize;
                if placed[at] || !live.live_in[at].contains(reg) {
                    continue;
                }
                placed[at] = true;
                wanted[at].push(reg);
                queue.push(frontier);
            }
        }
    }

    for params in &mut wanted {
        params.sort();
    }
    wanted
}

/// Give each definition a register of its own, by walking the dominator tree
/// with a stack of the names each variable currently goes by.
///
/// The stack *is* dominance: a definition is pushed when its block is entered
/// and popped when that block's subtree is left, so whatever is on top at any
/// point is the definition reaching there.
fn rename(function: &mut Function, dominators: &Dominators, wanted: &[Vec<VReg>]) {
    let children = dominators.children();
    let original = function.vreg_count();
    let mut names = Names::of(function);
    let mut reaching: Vec<Vec<VReg>> = vec![Vec::new(); original];
    // What each block pushed, so leaving it pops exactly that.
    let mut pushed: Vec<Vec<VReg>> = vec![Vec::new(); function.blocks.len()];

    // An explicit stack: a function nested a thousand deep would otherwise
    // recurse a thousand deep here.
    let mut steps = vec![Step::Enter(BlockId(0))];
    while let Some(step) = steps.pop() {
        let at = match step {
            Step::Leave(block) => {
                for reg in pushed[block.0 as usize].drain(..) {
                    reaching[reg.0 as usize].pop();
                }
                continue;
            }
            Step::Enter(block) => block.0 as usize,
        };
        steps.push(Step::Leave(BlockId(at as u32)));

        let mut define = |old: VReg, names: &mut Names, reaching: &mut Vec<Vec<VReg>>| {
            let fresh = names.version(old);
            reaching[old.0 as usize].push(fresh);
            pushed[at].push(old);
            fresh
        };

        function.blocks[at].params =
            wanted[at].iter().map(|&old| define(old, &mut names, &mut reaching)).collect();

        let mut instrs = std::mem::take(&mut function.blocks[at].instrs);
        for instr in &mut instrs {
            instr.values_mut(|value| substitute(value, &reaching, original));
            if let Some(old) = instr.def() {
                let fresh = define(old, &mut names, &mut reaching);
                set_def(instr, fresh);
            }
        }
        function.blocks[at].instrs = instrs;

        let mut term = std::mem::replace(&mut function.blocks[at].term, Terminator::Return(None));
        term.values_mut(|value| substitute(value, &reaching, original));
        // What each successor is handed: for each of its parameters, the
        // definition of that variable reaching along this edge — which is
        // whatever is on top of the variable's stack right here.
        for target in term.targets_mut() {
            target.args = wanted[target.block.0 as usize]
                .iter()
                .map(|&reg| match reaching[reg.0 as usize].last() {
                    Some(&name) => Value::Reg(name),
                    // A parameter is placed only where the variable is live, so
                    // a definition reaches on every edge that can be taken. One
                    // that cannot still has to be written down, and zero is as
                    // good an answer as any for an edge nothing runs.
                    None => Value::Const(0),
                })
                .collect();
        }
        function.blocks[at].term = term;

        for &child in &children[at] {
            steps.push(Step::Enter(child));
        }
    }

    function.vreg_names = names.finish();

    // A parameter is defined by an `Instr::Param` in the entry block, which the
    // walk above has just renamed; the signature has to follow it. The argument
    // index is what survived the renaming, so it is what the two are matched on.
    let leading = u32::from(function.ret.is_some_and(|ty| !ty.fits_in_a_register()));
    function.params = (0..function.params.len() as u32)
        .map(|index| {
            arrived_in(function, index + leading).expect("a parameter arrives in the entry block")
        })
        .collect();
}

enum Step {
    Enter(BlockId),
    Leave(BlockId),
}

/// The register the argument at `index` arrived in.
fn arrived_in(function: &Function, index: u32) -> Option<VReg> {
    function.blocks[0].instrs.iter().find_map(|instr| match instr {
        Instr::Param { dst, index: at } if *at == index => Some(*dst),
        _ => None,
    })
}

/// Replace a register operand with the definition reaching it. A register at or
/// past `original` is one this walk just created, and is already a definition.
fn substitute(value: &mut Value, reaching: &[Vec<VReg>], original: usize) {
    if let Value::Reg(reg) = value
        && (reg.0 as usize) < original
        && let Some(&name) = reaching[reg.0 as usize].last()
    {
        *value = Value::Reg(name);
    }
}

fn set_def(instr: &mut Instr, to: VReg) {
    match instr {
        Instr::Const { dst, .. }
        | Instr::StrAddr { dst, .. }
        | Instr::Copy { dst, .. }
        | Instr::Cast { dst, .. }
        | Instr::Bin { dst, .. }
        | Instr::Param { dst, .. }
        | Instr::Frame { dst, .. }
        | Instr::VTable { dst, .. }
        | Instr::VariantAddr { dst, .. }
        | Instr::Elem { dst, .. }
        | Instr::Field { dst, .. }
        | Instr::Load { dst, .. }
        | Instr::LoadChar { dst, .. }
        | Instr::Count { dst, .. }
        | Instr::Cmp { dst, .. } => *dst = to,
        Instr::Call { dst, .. } | Instr::CallVirtual { dst, .. } | Instr::RtCall { dst, .. } => {
            *dst = Some(to)
        }
        Instr::Print { .. }
        | Instr::PrintText { .. }
        | Instr::Store { .. }
        | Instr::CopyBytes { .. }
        | Instr::Fixup { .. } => unreachable!("an instruction that defines nothing"),
    }
}

// -- out of SSA -------------------------------------------------------------

/// Replace every block parameter with copies at the end of the blocks jumping
/// to it, leaving a function the register allocator can read.
fn destruct_function(function: &mut Function) {
    split_argument_edges(function);

    // What each predecessor has to copy before it leaves, collected before
    // anything is emitted: writing into one block while reading another's
    // terminator would borrow the same vector twice.
    let mut copies: Vec<Vec<(VReg, Value)>> = vec![Vec::new(); function.blocks.len()];
    for index in 0..function.blocks.len() {
        let params = std::mem::take(&mut function.blocks[index].params);
        if params.is_empty() {
            continue;
        }
        for (block, waiting) in function.blocks.iter_mut().zip(&mut copies) {
            for target in block.term.targets_mut() {
                if target.block.0 as usize == index {
                    let args = std::mem::take(&mut target.args);
                    waiting.extend(params.iter().copied().zip(args));
                }
            }
        }
    }

    let mut names = Names::of(function);
    for (index, parallel) in copies.into_iter().enumerate() {
        if !parallel.is_empty() {
            let sequenced = sequence(&mut names, parallel);
            function.blocks[index].instrs.extend(sequenced);
        }
    }
    function.vreg_names = names.finish();
}

/// Give every argument-carrying edge that leaves a block by one of several
/// exits a block of its own to put the copies in.
///
/// Without it the copies would land at the end of the branching block and run
/// on the other edge too. It also keeps the copies out of a block ending in a
/// branch, where the backend fuses the comparison with the jump and an
/// instruction in between would stop it.
fn split_argument_edges(function: &mut Function) {
    for index in 0..function.blocks.len() {
        let exits = function.blocks[index].term.successors().len();
        if exits < 2 {
            continue;
        }
        for position in 0..exits {
            let target = at(function, index, position);
            if target.args.is_empty() {
                continue;
            }
            let moved = std::mem::replace(target, Target::to(BlockId(0)));
            let split = BlockId(function.blocks.len() as u32);
            function.blocks.push(Block {
                kind: BlockKind::Edge,
                params: Vec::new(),
                index: split.0,
                instrs: Vec::new(),
                term: Terminator::Jump(moved),
            });
            *at(function, index, position) = Target::to(split);
        }
    }
}

fn at(function: &mut Function, block: usize, position: usize) -> &mut Target {
    function.blocks[block].term.targets_mut().nth(position).expect("an exit that exists")
}

/// Turn a parallel assignment into a sequence of copies meaning the same thing.
///
/// Every copy on one edge happens **at once**: a block whose parameters are
/// `(%a, %b)` reached with `(%b, %a)` is a swap, and writing `%a = %b` first
/// would lose `%b`'s old value and copy it back into itself. So a source that
/// is also a destination is read into a register of its own first, and only
/// then is anything written.
fn sequence(names: &mut Names, parallel: Vec<(VReg, Value)>) -> Vec<Instr> {
    let destinations: Vec<VReg> = parallel.iter().map(|(dst, _)| *dst).collect();
    let mut out = Vec::new();
    let mut writes = Vec::with_capacity(parallel.len());

    for (dst, src) in parallel {
        match src {
            Value::Reg(reg) if reg != dst && destinations.contains(&reg) => {
                let saved = names.copy_of(reg);
                out.push(Instr::Copy { dst: saved, src: Value::Reg(reg) });
                writes.push((dst, Value::Reg(saved)));
            }
            _ => writes.push((dst, src)),
        }
    }

    for (dst, src) in writes {
        // A parameter handed its own definition back — a loop variable that did
        // not change on this path — is a copy of a register to itself.
        if !matches!(src, Value::Reg(reg) if reg == dst) {
            out.push(Instr::Copy { dst, src });
        }
    }
    out
}
