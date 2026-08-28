//! Naming the registers SSA introduces.
//!
//! A definition made here stands for the same variable as the one it was split
//! from, and the dump is much easier to read if it says so: `%n`, `%n.1`,
//! `%n.2` rather than three unrelated numbers. Lowering already spells a second
//! `n` in a second scope that way, so the two agree on what a suffix means, and
//! the counter below is what keeps them from choosing the same one twice.
//!
//! The **first** version of a variable keeps the variable's own name. That is
//! not only tidier: a temporary is written once and so has exactly one version,
//! and without this every `%t3` in every dump in the test suite would have
//! become `%t3.1` for no reason a reader could see.

use std::collections::{HashMap, HashSet};

use crate::ir::{Function, VReg};

pub struct Names {
    names: Vec<String>,
    taken: HashSet<String>,
    /// Originals that have had their name handed to a version already.
    spent: HashSet<VReg>,
    /// Next suffix to try for each stem, so finding a free name stays a lookup
    /// rather than a scan of everything named so far.
    next: HashMap<String, u32>,
}

impl Names {
    /// Take a function's names, to be given back by [`Names::finish`].
    pub fn of(function: &mut Function) -> Names {
        let names = std::mem::take(&mut function.vreg_names);
        let taken = names.iter().cloned().collect();
        Names { names, taken, spent: HashSet::new(), next: HashMap::new() }
    }

    /// A register for one definition of the variable `of` names.
    ///
    /// The first one takes the variable's name outright — nothing refers to the
    /// original once renaming is done, so the name is free — and every one
    /// after it is suffixed.
    pub fn version(&mut self, of: VReg) -> VReg {
        match self.spent.insert(of) {
            true => self.push(self.names[of.0 as usize].clone()),
            false => self.suffixed(of),
        }
    }

    /// A register holding a copy of what `reg` holds. Always a new name: `reg`
    /// is still live, so it still needs its own.
    pub fn copy_of(&mut self, reg: VReg) -> VReg {
        self.suffixed(reg)
    }

    pub fn finish(self) -> Vec<String> {
        self.names
    }

    fn suffixed(&mut self, of: VReg) -> VReg {
        let base = &self.names[of.0 as usize];
        let stem = match base.rsplit_once('.') {
            Some((stem, suffix)) if suffix.chars().all(|c| c.is_ascii_digit()) => stem,
            _ => base.as_str(),
        }
        .to_string();

        let mut suffix = *self.next.get(&stem).unwrap_or(&1);
        let mut candidate = format!("{stem}.{suffix}");
        while self.taken.contains(&candidate) {
            suffix += 1;
            candidate = format!("{stem}.{suffix}");
        }
        self.next.insert(stem, suffix + 1);
        self.push(candidate)
    }

    fn push(&mut self, name: String) -> VReg {
        self.taken.insert(name.clone());
        let fresh = VReg(self.names.len() as u32);
        self.names.push(name);
        fresh
    }
}
