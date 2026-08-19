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

use std::collections::HashMap;

use crate::ir::{Program, VReg};

/// Index of a machine register, interpreted by the backend that supplied the
/// [`RegisterFile`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhysReg(pub u8);

/// What the backend tells the allocator about its machine registers.
pub struct RegisterFile {
    /// Name of every machine register, indexed by [`PhysReg`].
    pub names: Vec<&'static str>,
    /// Allocatable registers destroyed by a call.
    pub caller_saved: Vec<PhysReg>,
    /// Allocatable registers that survive a call, at the cost of being saved
    /// and restored in the prologue and epilogue.
    pub callee_saved: Vec<PhysReg>,
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
    pub fn dump(&self, program: &Program, rf: &RegisterFile) -> String {
        let mut out = String::from("vreg  live range   across call  location\n");
        for interval in &self.intervals {
            let location = match self.location(interval.vreg) {
                Location::Reg(reg) => rf.name(reg).to_string(),
                Location::Spill(slot) => format!("spill slot {slot}"),
            };
            out.push_str(&format!(
                "{:<6}[{:>2}, {:>2}]{:>10}  {:>10}\n",
                format!("%{}", program.name_of(interval.vreg)),
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

/// Compute one live interval per virtual register, in order of definition.
pub fn live_intervals(program: &Program) -> Vec<Interval> {
    let mut start = vec![u32::MAX; program.vreg_count()];
    let mut end = vec![0u32; program.vreg_count()];

    for (index, instr) in program.instrs.iter().enumerate() {
        let index = index as u32;
        if let Some(dst) = instr.def() {
            let slot = dst.0 as usize;
            start[slot] = start[slot].min(index);
            end[slot] = end[slot].max(index);
        }
        for used in instr.uses() {
            end[used.0 as usize] = index;
        }
    }

    let call_sites: Vec<u32> = program
        .instrs
        .iter()
        .enumerate()
        .filter(|(_, instr)| instr.is_call())
        .map(|(index, _)| index as u32)
        .collect();

    let mut intervals: Vec<Interval> = (0..program.vreg_count() as u32)
        .map(VReg)
        .filter(|reg| start[reg.0 as usize] != u32::MAX)
        .map(|vreg| {
            let (start, end) = (start[vreg.0 as usize], end[vreg.0 as usize]);
            Interval {
                vreg,
                start,
                end,
                // A value used *by* the call does not survive it.
                crosses_call: call_sites.iter().any(|&call| start < call && call < end),
            }
        })
        .collect();

    intervals.sort_by_key(|i| (i.start, i.vreg));
    intervals
}

pub fn allocate(program: &Program, rf: &RegisterFile) -> Allocation {
    let intervals = live_intervals(program);

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
        self.active.push(Active { vreg: interval.vreg, end: interval.end, location, pool });
        self.active.sort_by_key(|entry| entry.end);
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
    use crate::{lexer, parser, sema};

    /// A deliberately tiny register file: 2 caller-saved, 2 callee-saved.
    fn tiny_file() -> RegisterFile {
        RegisterFile {
            names: vec!["v0", "v1", "s0", "s1"],
            caller_saved: vec![PhysReg(0), PhysReg(1)],
            callee_saved: vec![PhysReg(2), PhysReg(3)],
        }
    }

    fn ir_of(src: &str) -> Program {
        let ast = parser::parse(&lexer::lex(src).unwrap()).unwrap();
        let types = sema::check(&ast).unwrap();
        crate::ir::lower(&ast, &types)
    }

    #[test]
    fn intervals_end_at_the_last_use() {
        let ir = ir_of("int x = 10;\nint y = 20;\nprint(x + y);");
        let intervals = live_intervals(&ir);
        assert_eq!((intervals[0].start, intervals[0].end), (0, 2)); // x
        assert_eq!((intervals[1].start, intervals[1].end), (1, 2)); // y
        assert_eq!((intervals[2].start, intervals[2].end), (2, 3)); // x + y
    }

    #[test]
    fn a_value_live_across_a_call_gets_a_callee_saved_register() {
        let ir = ir_of("string s = \"hi\";\nprint(1 + 2);\nprint(s);");
        let rf = tiny_file();
        let allocation = allocate(&ir, &rf);
        let s = ir.instrs.iter().find_map(|i| i.def()).unwrap();
        assert!(live_intervals(&ir)[0].crosses_call);
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
        let ir = ir_of("int a = 2;\nprint(a * 3 + a * 4 + a * 5 + a * 6);");
        let rf = tiny_file();
        let allocation = allocate(&ir, &rf);
        assert_eq!(allocation.spill_slots, 0, "{}", allocation.dump(&ir, &rf));
        assert!(verify(&allocation, &rf).is_ok());
    }

    #[test]
    fn spills_when_the_register_file_runs_out() {
        let src = "int a = 1;\nint b = 2;\nint c = 3;\nint d = 4;\nint e = 5;\n\
                   print(a + b + c + d + e);";
        let ir = ir_of(src);
        let rf = tiny_file();
        let allocation = allocate(&ir, &rf);
        assert!(allocation.spill_slots > 0);
        assert!(verify(&allocation, &rf).is_ok(), "{}", allocation.dump(&ir, &rf));
    }
}
