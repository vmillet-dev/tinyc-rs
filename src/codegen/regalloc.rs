//! Linear-scan register allocation over the IR's virtual registers.
//!
//! This module is deliberately target-independent: it is handed a
//! [`RegisterFile`] describing how many registers exist and which of them
//! survive a call, and it answers with a [`Location`] for every virtual
//! register. Adding a new backend means describing its register file, not
//! rewriting the allocator.
//!
//! The algorithm is the classic linear scan (Poletto & Sarkar):
//!
//! 1. Compute one live interval per virtual register. Straight-line code makes
//!    these exact: `[definition, last use]`.
//! 2. Note which intervals are live *across* a call. Those must live in
//!    callee-saved registers, since a call destroys the caller-saved ones.
//! 3. Walk the intervals in order of definition, handing back registers as
//!    intervals expire — this is what makes a dead temporary's register
//!    available to the next temporary.
//! 4. When nothing is free, spill the interval that lives longest to a stack
//!    slot. Stack slots are recycled on expiry too.
//!
//! Within one instruction, operands are read before the result is written, so
//! an operand's register is free to become the result's register. That is
//! precisely how a dead temporary's register gets reused by the temporary that
//! replaces it — at the cost of one aliasing case the backend has to guard
//! against (see `x64_win`'s `work_reg`).
//!
//! ## The invariant that rule rests on
//!
//! Two intervals that merely *touch*, `a.end == b.start`, are allowed to share a
//! register: whatever happens at that index reads `a` and writes `b`. That is
//! only sound because two operands of the same instruction can never be a
//! touching pair, and they cannot because **[`crate::ir`] always emits a
//! register's definition before any of its uses in the flat layout**. A lowering
//! that broke that — one that could reach a use before its definition, the way
//! short-circuit `&&` or a `continue` might — would need this rule revisited,
//! not just the backend's aliasing guards. [`verify`] checks the consequence;
//! `definitions_precede_uses` in the tests checks the cause.

use std::collections::HashMap;

use crate::ir::{Function, VReg};

/// Index of a machine register, interpreted by the backend that supplied the
/// [`RegisterFile`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhysReg(pub u8);

/// What the backend tells the allocator about its machine registers.
#[derive(Clone)]
pub struct RegisterFile {
    /// Name of every machine register, indexed by [`PhysReg`].
    pub names: Vec<&'static str>,
    /// Allocatable registers destroyed by a call.
    pub caller_saved: Vec<PhysReg>,
    /// Allocatable registers that survive a call, at the cost of being saved
    /// and restored in the prologue and epilogue.
    pub callee_saved: Vec<PhysReg>,
    /// How many arguments this target passes in registers, and therefore the
    /// most parameters a function may declare.
    ///
    /// It lives here rather than in [`crate::sema`] because it is an ABI fact,
    /// not a language one: the type checker enforces the number the target
    /// reports instead of hard-coding one backend's answer.
    pub max_args: usize,
}

impl RegisterFile {
    pub fn name(&self, reg: PhysReg) -> &'static str {
        self.names[reg.0 as usize]
    }
}

/// Where a virtual register lives for its whole lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Location {
    Reg(PhysReg),
    /// Index of a stack slot; the backend decides the actual frame offset.
    Spill(u32),
}

/// The live range of one virtual register.
#[derive(Clone, Copy, Debug)]
pub struct Interval {
    pub vreg: VReg,
    /// Index of the instruction that defines it.
    pub start: u32,
    /// Index of the instruction that last uses it (`start` if never used).
    pub end: u32,
    /// True if a call happens strictly inside `(start, end)`.
    pub crosses_call: bool,
}

pub struct Allocation {
    pub locations: HashMap<VReg, Location>,
    /// Callee-saved registers actually used, in a stable order: exactly the
    /// ones the prologue must push.
    pub used_callee_saved: Vec<PhysReg>,
    /// Number of stack slots the frame must reserve.
    pub spill_slots: u32,
    /// The live intervals this allocation was computed from, in definition order.
    pub intervals: Vec<Interval>,
}

impl Allocation {
    pub fn location(&self, vreg: VReg) -> Location {
        self.locations[&vreg]
    }

    /// Render intervals and assignments for `--dump-regalloc`.
    pub fn dump(&self, function: &Function, rf: &RegisterFile) -> String {
        // Names grow with reassignment (`%n`, `%n.1`, ...), so size the first
        // column to the widest one.
        let width = self
            .intervals
            .iter()
            .map(|i| function.name_of(i.vreg).len() + 2)
            .chain(std::iter::once(6))
            .max()
            .unwrap_or(6);

        let mut out = format!("{:<width$}live range   across call  location\n", "vreg");
        for interval in &self.intervals {
            let location = match self.location(interval.vreg) {
                Location::Reg(reg) => rf.name(reg).to_string(),
                Location::Spill(slot) => format!("spill slot {slot}"),
            };
            out.push_str(&format!(
                "{:<width$}[{:>2}, {:>2}]{:>10}  {:>10}\n",
                format!("%{}", function.name_of(interval.vreg)),
                interval.start,
                interval.end,
                if interval.crosses_call { "yes" } else { "no" },
                location,
            ));
        }
        out.push_str(&format!(
            "\n{} spill slot(s), callee-saved used: {}\n",
            self.spill_slots,
            if self.used_callee_saved.is_empty() {
                "none".to_string()
            } else {
                self.used_callee_saved
                    .iter()
                    .map(|&r| rf.name(r))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        ));
        out
    }
}

/// Where each block sits once the CFG is laid out as a flat instruction list.
struct Layout {
    /// Index of a block's first instruction.
    start: Vec<u32>,
    /// Index of a block's terminator, i.e. its last index.
    end: Vec<u32>,
    /// Indices of instructions that perform a call.
    call_sites: Vec<u32>,
}

impl Layout {
    fn new(function: &Function) -> Layout {
        let (mut start, mut end, mut call_sites) = (Vec::new(), Vec::new(), Vec::new());
        let mut index = 0;
        for block in &function.blocks {
            start.push(index);
            for instr in &block.instrs {
                if instr.is_call() {
                    call_sites.push(index);
                }
                index += 1;
            }
            end.push(index); // the terminator
            index += 1;
        }
        Layout { start, end, call_sites }
    }
}

/// A set of virtual registers, one bit each.
///
/// [`VReg`]s are dense indices from zero, which is exactly what a bitmap wants.
/// The dataflow below unions and subtracts these sets on every round of its
/// fixpoint, and here that is a handful of word operations rather than a hash
/// per register.
#[derive(Clone, PartialEq, Eq)]
struct VRegSet {
    words: Vec<u64>,
}

impl VRegSet {
    fn new(registers: usize) -> VRegSet {
        VRegSet { words: vec![0; registers.div_ceil(64)] }
    }

    fn insert(&mut self, reg: VReg) {
        let (word, bit) = (reg.0 as usize / 64, reg.0 as usize % 64);
        self.words[word] |= 1 << bit;
    }

    fn contains(&self, reg: VReg) -> bool {
        let (word, bit) = (reg.0 as usize / 64, reg.0 as usize % 64);
        self.words[word] & (1 << bit) != 0
    }

    /// `self |= other`, answering whether that added anything.
    fn union_with(&mut self, other: &VRegSet) -> bool {
        let mut grew = false;
        for (mine, theirs) in self.words.iter_mut().zip(&other.words) {
            let merged = *mine | theirs;
            grew |= merged != *mine;
            *mine = merged;
        }
        grew
    }

    /// `self |= other - excluded`, answering whether that added anything.
    fn union_without(&mut self, other: &VRegSet, excluded: &VRegSet) -> bool {
        let mut grew = false;
        for ((mine, theirs), gone) in
            self.words.iter_mut().zip(&other.words).zip(&excluded.words)
        {
            let merged = *mine | (theirs & !gone);
            grew |= merged != *mine;
            *mine = merged;
        }
        grew
    }

    fn iter(&self) -> impl Iterator<Item = VReg> + '_ {
        self.words.iter().enumerate().flat_map(|(word, bits)| {
            (0..64).filter(move |bit| bits & (1 << bit) != 0).map(move |bit| VReg((word * 64 + bit) as u32))
        })
    }
}

/// Live-in and live-out sets, one per block.
struct Liveness {
    live_in: Vec<VRegSet>,
    live_out: Vec<VRegSet>,
}

/// Which registers are live where, by backward dataflow over the CFG.
///
/// A single forward pass was enough while the program was one straight run of
/// instructions, but a loop's back edge means a value can be live *before* the
/// instruction that defines it is reached again. The standard answer is to
/// iterate to a fixpoint:
///
/// ```text
/// live_out(B) = union of live_in(S) for every successor S of B
/// live_in(B)  = used_before_written(B) + (live_out(B) - written(B))
/// ```
fn liveness(function: &Function) -> Liveness {
    let count = function.blocks.len();
    let registers = function.vreg_count();

    // Per block: registers read before being written, and registers written.
    let mut upward_exposed = vec![VRegSet::new(registers); count];
    let mut written = vec![VRegSet::new(registers); count];
    for (b, block) in function.blocks.iter().enumerate() {
        let (exposed, written) = (&mut upward_exposed[b], &mut written[b]);
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

/// Compute one live interval per virtual register, in order of definition.
///
/// Intervals are contiguous ranges over the flattened block layout, which is a
/// conservative approximation: a register live at the top and bottom of a loop
/// is treated as live throughout, so it cannot be handed to anything else in
/// between. That is exactly what a back edge requires.
pub fn live_intervals(function: &Function) -> Vec<Interval> {
    let layout = Layout::new(function);
    let live = liveness(function);

    let mut start = vec![u32::MAX; function.vreg_count()];
    let mut end = vec![0u32; function.vreg_count()];
    let mut seen = vec![false; function.vreg_count()];

    let mut extend = |reg: VReg, at: u32, start: &mut Vec<u32>, end: &mut Vec<u32>| {
        let slot = reg.0 as usize;
        start[slot] = start[slot].min(at);
        end[slot] = end[slot].max(at);
        seen[slot] = true;
    };

    for (b, block) in function.blocks.iter().enumerate() {
        // Live across the whole block boundary, so the interval must span it.
        for reg in live.live_in[b].iter() {
            extend(reg, layout.start[b], &mut start, &mut end);
        }
        for reg in live.live_out[b].iter() {
            extend(reg, layout.end[b], &mut start, &mut end);
        }

        for (index, instr) in (layout.start[b]..).zip(&block.instrs) {
            if let Some(def) = instr.def() {
                extend(def, index, &mut start, &mut end);
            }
            instr.uses(|used| extend(used, index, &mut start, &mut end));
        }
        block.term.uses(|used| extend(used, layout.end[b], &mut start, &mut end));
    }

    let mut intervals: Vec<Interval> = (0..function.vreg_count() as u32)
        .map(VReg)
        .filter(|reg| seen[reg.0 as usize])
        .map(|vreg| {
            let (start, end) = (start[vreg.0 as usize], end[vreg.0 as usize]);
            Interval {
                vreg,
                start,
                end,
                // A value used *by* the call does not survive it.
                crosses_call: layout.call_sites.iter().any(|&call| start < call && call < end),
            }
        })
        .collect();

    intervals.sort_by_key(|i| (i.start, i.vreg));
    intervals
}

pub fn allocate(function: &Function, rf: &RegisterFile) -> Allocation {
    let intervals = live_intervals(function);

    let mut state = Scan {
        free_caller_saved: rf.caller_saved.iter().rev().copied().collect(),
        free_callee_saved: rf.callee_saved.iter().rev().copied().collect(),
        free_slots: Vec::new(),
        next_slot: 0,
        active: Vec::new(),
        locations: HashMap::new(),
        used_callee_saved: Vec::new(),
    };

    for &interval in &intervals {
        state.expire_through(interval.start);

        // A value live across a call cannot sit in a caller-saved register.
        let (location, pool) = match state.take_register(interval.crosses_call) {
            Some((reg, pool)) => (Location::Reg(reg), Some(pool)),
            // Out of registers: steal from the longest-lived active interval if
            // it outlives this one, otherwise spill this one.
            None => match state.steal_candidate(interval) {
                Some(position) => {
                    let victim = state.active.remove(position);
                    let Location::Reg(reg) = victim.location else { unreachable!() };
                    let slot = state.alloc_slot();
                    state.locations.insert(victim.vreg, Location::Spill(slot));
                    (Location::Reg(reg), victim.pool)
                }
                None => (Location::Spill(state.alloc_slot()), None),
            },
        };

        if let (Location::Reg(reg), Some(Pool::CalleeSaved)) = (location, pool)
            && !state.used_callee_saved.contains(&reg)
        {
            state.used_callee_saved.push(reg);
        }

        state.locations.insert(interval.vreg, location);
        state.activate(interval, location, pool);
    }

    let mut used_callee_saved = state.used_callee_saved;
    used_callee_saved.sort();

    Allocation {
        locations: state.locations,
        used_callee_saved,
        spill_slots: state.next_slot,
        intervals,
    }
}

/// Which pool a register came from, so it can be handed back to the right one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pool {
    CallerSaved,
    CalleeSaved,
}

/// One interval currently holding a location, kept sorted by increasing `end`.
struct Active {
    vreg: VReg,
    end: u32,
    location: Location,
    /// `None` for a spilled interval.
    pool: Option<Pool>,
}

struct Scan {
    /// Free registers, used as stacks so allocation order stays deterministic.
    free_caller_saved: Vec<PhysReg>,
    free_callee_saved: Vec<PhysReg>,
    free_slots: Vec<u32>,
    next_slot: u32,
    active: Vec<Active>,
    locations: HashMap<VReg, Location>,
    used_callee_saved: Vec<PhysReg>,
}

impl Scan {
    /// Release everything dead by the end of instruction `index`, including an
    /// interval whose last use *is* that instruction: its value has been read
    /// by the time the instruction writes its result.
    fn expire_through(&mut self, index: u32) {
        let mut still_active = Vec::with_capacity(self.active.len());
        for entry in std::mem::take(&mut self.active) {
            if entry.end <= index {
                match (entry.location, entry.pool) {
                    (Location::Reg(reg), Some(Pool::CallerSaved)) => {
                        self.free_caller_saved.push(reg)
                    }
                    (Location::Reg(reg), Some(Pool::CalleeSaved)) => {
                        self.free_callee_saved.push(reg)
                    }
                    (Location::Spill(slot), _) => self.free_slots.push(slot),
                    (Location::Reg(_), None) => unreachable!("a register always has a pool"),
                }
            } else {
                still_active.push(entry);
            }
        }
        self.active = still_active;
    }

    fn take_register(&mut self, crosses_call: bool) -> Option<(PhysReg, Pool)> {
        if crosses_call {
            return self.free_callee_saved.pop().map(|reg| (reg, Pool::CalleeSaved));
        }
        // Prefer the caller-saved pool: those registers cost nothing to use,
        // while every callee-saved one used adds a push/pop to the prologue.
        match self.free_caller_saved.pop() {
            Some(reg) => Some((reg, Pool::CallerSaved)),
            None => self.free_callee_saved.pop().map(|reg| (reg, Pool::CalleeSaved)),
        }
    }

    /// Index into `active` of the register holder worth evicting, if any: the
    /// longest-lived one whose register this interval could legally use.
    fn steal_candidate(&self, interval: Interval) -> Option<usize> {
        self.active
            .iter()
            .enumerate()
            .filter(|(_, entry)| match entry.pool {
                Some(Pool::CalleeSaved) => true,
                Some(Pool::CallerSaved) => !interval.crosses_call,
                None => false,
            })
            .max_by_key(|(_, entry)| entry.end)
            .filter(|(_, entry)| entry.end > interval.end)
            .map(|(position, _)| position)
    }

    fn alloc_slot(&mut self) -> u32 {
        match self.free_slots.pop() {
            Some(slot) => slot,
            None => {
                let slot = self.next_slot;
                self.next_slot += 1;
                slot
            }
        }
    }

    fn activate(&mut self, interval: Interval, location: Location, pool: Option<Pool>) {
        // `active` is kept sorted by increasing `end`, so inserting at the right
        // place keeps the invariant without re-sorting the whole list.
        let entry = Active { vreg: interval.vreg, end: interval.end, location, pool };
        let at = self.active.partition_point(|other| other.end <= entry.end);
        self.active.insert(at, entry);
    }
}

/// Verify an allocation: no two intervals that overlap may share a location,
/// and nothing live across a call may sit in a caller-saved register. Used by
/// the tests, and cheap enough to be worth keeping around.
pub fn verify(allocation: &Allocation, rf: &RegisterFile) -> std::result::Result<(), String> {
    for (i, a) in allocation.intervals.iter().enumerate() {
        for b in &allocation.intervals[i + 1..] {
            // Touching at a single instruction is not an overlap: the earlier
            // value is read there, the later one written there.
            let overlap = a.start.max(b.start) < a.end.min(b.end);
            if overlap && allocation.location(a.vreg) == allocation.location(b.vreg) {
                return Err(format!(
                    "overlapping intervals {:?} and {:?} share {:?}",
                    a,
                    b,
                    allocation.location(a.vreg)
                ));
            }
        }
        if a.crosses_call
            && let Location::Reg(reg) = allocation.location(a.vreg)
            && rf.caller_saved.contains(&reg)
        {
            return Err(format!("{:?} is live across a call in caller-saved {}", a, rf.name(reg)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Program;
    use crate::{lexer, parser, sema};

    /// A deliberately tiny register file: 2 caller-saved, 2 callee-saved.
    fn tiny_file() -> RegisterFile {
        RegisterFile {
            names: vec!["v0", "v1", "s0", "s1"],
            caller_saved: vec![PhysReg(0), PhysReg(1)],
            callee_saved: vec![PhysReg(2), PhysReg(3)],
            max_args: 4,
        }
    }

    fn ir_of(src: &str) -> Program {
        let ast = parser::parse(&lexer::lex(src).unwrap()).unwrap();
        let types = sema::check(&ast, 4).unwrap();
        crate::ir::lower(&ast, &types)
    }

    /// Lower a `main` body; every test below is about one function's frame.
    fn ir_of_main(body: &str) -> Program {
        ir_of(&format!("fn main() {{\n{body}\n}}\n"))
    }

    /// The invariant the "touching intervals may share a register" rule rests
    /// on — see the module docs.
    ///
    /// If lowering could ever reach a use before the definition it belongs to,
    /// two operands of one instruction could end up in the same register, and
    /// no amount of care in the backend would save them.
    #[test]
    fn definitions_precede_uses() {
        let programs = [
            "fn main() {\n  int i = 0;\n  while (i < 3) {\n    i = i + 1;\n  }\n  print(i);\n}",
            "fn main() {\n  for (int j = 1; j <= 5; j = j + 1) {\n    print(j);\n  }\n}",
            "fn main() {\n  int n = 1;\n  if (n < 2) {\n    n = 2;\n  } else {\n    n = 3;\n  }\n  \
             print(n);\n}",
            "fn f(int a, int b) -> int {\n  return a * b - a;\n}\n\
             fn main() {\n  print(f(6, 7));\n}",
            "fn fib(int n) -> int {\n  if (n < 2) {\n    return n;\n  }\n  \
             return fib(n - 1) + fib(n - 2);\n}\nfn main() {\n  print(fib(10));\n}",
            "fn main() {\n  int a = 1;\n  int b = 2;\n  int c = 3;\n  \
             while (a < 100) {\n    a = a + b * c;\n    b = a - b;\n  }\n  print(b);\n}",
        ];

        for source in programs {
            let ir = ir_of(source);
            for function in &ir.functions {
                let mut defined = vec![false; function.vreg_count()];
                let check = |defined: &Vec<bool>, reg: VReg| {
                    assert!(
                        defined[reg.0 as usize],
                        "%{} is read before it is written in `{}`:\n{}",
                        function.name_of(reg),
                        function.name,
                        ir.dump()
                    );
                };
                for block in &function.blocks {
                    for instr in &block.instrs {
                        // Reading and writing the same register in one
                        // instruction is fine: operands are read first.
                        instr.uses(|reg| check(&defined, reg));
                        if let Some(def) = instr.def() {
                            defined[def.0 as usize] = true;
                        }
                    }
                    block.term.uses(|reg| check(&defined, reg));
                }
            }
        }
    }

    #[test]
    fn intervals_end_at_the_last_use() {
        let ir = ir_of_main("int x = 10;\nint y = 20;\nprint(x + y);");
        let intervals = live_intervals(&ir.functions[0]);
        assert_eq!((intervals[0].start, intervals[0].end), (0, 2)); // x
        assert_eq!((intervals[1].start, intervals[1].end), (1, 2)); // y
        assert_eq!((intervals[2].start, intervals[2].end), (2, 3)); // x + y
    }

    #[test]
    fn a_value_live_across_a_call_gets_a_callee_saved_register() {
        let ir = ir_of_main("string s = \"hi\";\nprint(1 + 2);\nprint(s);");
        let main = &ir.functions[0];
        let rf = tiny_file();
        let allocation = allocate(main, &rf);
        let s = main.blocks[0].instrs.iter().find_map(|i| i.def()).unwrap();
        assert!(live_intervals(main)[0].crosses_call);
        match allocation.location(s) {
            Location::Reg(reg) => assert!(rf.callee_saved.contains(&reg)),
            other => panic!("expected a register, got {other:?}"),
        }
        assert!(verify(&allocation, &rf).is_ok());
    }

    #[test]
    fn a_dead_temporary_hands_its_register_to_the_next_one() {
        // Each product dies at the sum that consumes it, so the later temporaries
        // reuse the earlier ones' registers instead of growing the frame.
        let ir = ir_of_main("int a = 2;\nprint(a * 3 + a * 4 + a * 5 + a * 6);");
        let main = &ir.functions[0];
        let rf = tiny_file();
        let allocation = allocate(main, &rf);
        assert_eq!(allocation.spill_slots, 0, "{}", allocation.dump(main, &rf));
        assert!(verify(&allocation, &rf).is_ok());
    }

    #[test]
    fn a_loop_carried_value_stays_live_across_the_back_edge() {
        // `i` is written at the bottom of the body and read at the top of the
        // header, so its interval has to cover the whole loop even though the
        // definition comes *after* the use in program order. A single forward
        // pass would end its interval at the last instruction that mentions it.
        let ir = ir_of_main("int i = 0;\nwhile (i < 3) {\n  i = i + 1;\n}\nprint(i);");
        let main = &ir.functions[0];
        let intervals = live_intervals(main);
        let i = intervals.iter().find(|interval| main.name_of(interval.vreg) == "i").unwrap();

        // The interval has to reach past the bottom of the body, where `i` is
        // redefined, and back to the header that reads it again.
        assert_eq!(i.start, 0, "`i` is defined before the loop");
        assert!(i.end > block_end(main, "body2"), "`i` must outlive the loop body, got {i:?}");
    }

    /// Index of a block's terminator in the flattened layout.
    fn block_end(function: &Function, label: &str) -> u32 {
        let mut index = 0;
        for block in &function.blocks {
            index += block.instrs.len() as u32;
            if block.label() == label {
                return index;
            }
            index += 1;
        }
        panic!("no block labelled {label}");
    }

    #[test]
    fn a_temporary_inside_a_loop_does_not_escape_it() {
        let ir = ir_of_main("int i = 0;\nwhile (i < 3) {\n  i = i + 1;\n}\nprint(i);");
        let main = &ir.functions[0];
        let intervals = live_intervals(main);
        let t =
            intervals.iter().find(|interval| main.name_of(interval.vreg).starts_with('t')).unwrap();
        // The comparison result is consumed by the branch in the same block.
        assert!(t.end - t.start <= 1, "{t:?}");
    }

    #[test]
    fn allocations_for_loops_are_valid() {
        let rf = tiny_file();
        for src in [
            "int i = 0;\nwhile (i < 3) {\n  i = i + 1;\n}\nprint(i);",
            "int t = 0;\nfor (int i = 0; i < 4; i = i + 1) {\n  t = t + i;\n  print(t);\n}",
            "int a = 1;\nif (a < 2) {\n  a = 2;\n} else {\n  a = 3;\n}\nprint(a);",
        ] {
            let ir = ir_of_main(src);
            let main = &ir.functions[0];
            let allocation = allocate(main, &rf);
            assert!(verify(&allocation, &rf).is_ok(), "{}", allocation.dump(main, &rf));
        }
    }

    #[test]
    fn spills_when_the_register_file_runs_out() {
        let ir = ir_of_main(
            "int a = 1;\nint b = 2;\nint c = 3;\nint d = 4;\nint e = 5;\n\
             print(a + b + c + d + e);",
        );
        let main = &ir.functions[0];
        let rf = tiny_file();
        let allocation = allocate(main, &rf);
        assert!(allocation.spill_slots > 0);
        assert!(verify(&allocation, &rf).is_ok(), "{}", allocation.dump(main, &rf));
    }

    // -- functions ---------------------------------------------------------

    #[test]
    fn every_function_is_allocated_on_its_own() {
        // Two frames, computed independently: `main` spills nothing just
        // because `crowded` had to.
        let ir = ir_of(
            "fn crowded(int a, int b, int c, int d) -> int {\n  return a + b + c + d;\n}\n\
             fn main() {\n  print(crowded(1, 2, 3, 4));\n}",
        );
        let rf = tiny_file();
        for function in &ir.functions {
            let allocation = allocate(function, &rf);
            assert!(verify(&allocation, &rf).is_ok(), "{}", allocation.dump(function, &rf));
        }
    }

    #[test]
    fn a_parameter_is_live_from_the_top_of_its_function() {
        // `Instr::Param` is what gives a parameter a definition point. Without
        // it the interval would start at the first use, and the register the
        // argument arrived in could be handed to something else in between.
        let ir = ir_of(
            "fn f(int a) -> int {\n  int filler = 1;\n  print(filler);\n  return a;\n}\n\
             fn main() {\n  print(f(1));\n}",
        );
        let f = &ir.functions[0];
        let intervals = live_intervals(f);
        let a = intervals.iter().find(|i| f.name_of(i.vreg) == "a").unwrap();
        assert_eq!(a.start, 0, "a parameter is defined by the entry block's first instruction");
        // It survives the `print`, so it may not sit in a caller-saved register.
        assert!(a.crosses_call, "{a:?}");
    }

    #[test]
    fn an_argument_of_a_call_does_not_cross_it() {
        // A value read *by* the call dies at the call, so it can stay in a
        // cheap caller-saved register.
        let ir = ir_of(
            "fn g(int n) -> int {\n  return n;\n}\n\
             fn main() {\n  int x = 1;\n  print(g(x));\n}",
        );
        let main = ir.functions.last().unwrap();
        let intervals = live_intervals(main);
        let x = intervals.iter().find(|i| main.name_of(i.vreg) == "x").unwrap();
        assert!(!x.crosses_call, "{x:?}");
    }

    #[test]
    fn a_nested_call_forces_the_outer_argument_into_a_callee_saved_register() {
        // In `f(g(1), h(2))` the result of `g` is live across the call to `h`.
        let ir = ir_of(
            "fn g(int n) -> int {\n  return n;\n}\nfn h(int n) -> int {\n  return n;\n}\n\
             fn f(int a, int b) -> int {\n  return a + b;\n}\n\
             fn main() {\n  print(f(g(1), h(2)));\n}",
        );
        let main = ir.functions.last().unwrap();
        let rf = tiny_file();
        let allocation = allocate(main, &rf);
        assert!(
            live_intervals(main).iter().any(|i| i.crosses_call),
            "{}",
            allocation.dump(main, &rf)
        );
        assert!(verify(&allocation, &rf).is_ok(), "{}", allocation.dump(main, &rf));
    }
}
