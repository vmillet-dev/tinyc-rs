//! A lowered function and the program that holds them.

use crate::ast::{Ty, TypeTable};
use super::{Block, BlockId, FuncId, Num, VReg, Value};

/// One function's control flow graph, virtual registers and signature.
pub struct Function {
    pub name: String,
    /// The register each parameter was lowered into, in declaration order.
    pub params: Vec<VReg>,
    /// `None` for a function that returns nothing.
    pub ret: Option<Ty>,
    /// Bytes of frame this function's arrays need, reserved once in the
    /// prologue and handed out by [`Instr::Frame`].
    pub frame_bytes: u32,
    /// Basic blocks in the order they will be emitted; block 0 is the entry.
    pub blocks: Vec<Block>,
    /// Human-readable name per virtual register, used by IR and allocator dumps.
    pub vreg_names: Vec<String>,
}

impl Function {
    pub fn vreg_count(&self) -> usize {
        self.vreg_names.len()
    }

    pub fn name_of(&self, reg: VReg) -> &str {
        &self.vreg_names[reg.0 as usize]
    }

    pub fn block(&self, id: BlockId) -> &Block {
        &self.blocks[id.0 as usize]
    }

    pub(super) fn value_name(&self, value: &Value) -> String {
        self.value_name_as(Num::Int, value)
    }

    /// The same, for an operand whose instruction says how to read it.
    ///
    /// A float constant is the bits of a double, and `4620974692658839552` in
    /// the dump beside a `printf` says nothing a reader could check. Written
    /// back out as `8.5` it does — and the `f` on the end says the number is
    /// what the word means rather than what it holds.
    pub(super) fn value_name_as(&self, num: Num, value: &Value) -> String {
        match (num, value) {
            (Num::Float, Value::Const(c)) => format!("{}f", f64::from_bits(*c as u64)),
            (_, Value::Const(c)) => c.to_string(),
            (_, Value::Reg(r)) => format!("%{}", self.name_of(*r)),
        }
    }

    /// The signature line used by the IR dump and by assembly comments.
    pub fn signature(&self, table: &TypeTable) -> String {
        let params: Vec<String> =
            self.params.iter().map(|&r| format!("%{}", self.name_of(r))).collect();
        let ret = match self.ret {
            Some(ty) => format!(" -> {}", ty.name(table)),
            None => String::new(),
        };
        format!("fn {}({}){}", self.name, params.join(", "), ret)
    }
}


pub struct Program {
    pub functions: Vec<Function>,
    /// Interned string literals, each as the characters it holds.
    pub strings: Vec<Vec<char>>,
    /// Interned runs of literal text, each already the UTF-8 it will be written
    /// as. See [`Instr::PrintText`].
    pub texts: Vec<String>,
    /// One method table per class, in `ClassId` order.
    pub vtables: Vec<Vec<FuncId>>,
    /// Every type the program has, carried through so that a type can still be named
    /// and a value of one can still be printed.
    ///
    /// An enum's *values* need nothing here: a variant is its index, so it is
    /// an integer everywhere the backend is concerned. Only the names survive.
    pub table: TypeTable,
}

impl Program {
    pub fn function(&self, id: FuncId) -> &Function {
        &self.functions[id.0 as usize]
    }

}
