//! Stage 4: AST -> intermediate representation.
//!
//! The IR is three-address code over an unbounded supply of *virtual*
//! registers, arranged as a **control flow graph**: a list of basic blocks,
//! each a straight run of instructions ending in a terminator that names its
//! successors.
//!
//! ## One function, one world
//!
//! A [`Function`] owns its blocks *and* its virtual registers, so [`BlockId`]
//! and [`VReg`] are indices **into that function**, never across the program.
//! That is what lets [`crate::codegen::regalloc`] run once per function and
//! give each one its own frame: nothing a function does can be observed by
//! another except through a call.
//!
//! String literals are the exception. They are interned once for the whole
//! [`Program`], because they end up in a single `.data` section.
//!
//! ## Why variables are not in SSA form
//!
//! Before control flow existed, each assignment could simply introduce a new
//! virtual register (`%n`, `%n.1`, ...) and every register had exactly one
//! definition. That breaks as soon as two paths can reach the same point:
//!
//! ```text
//! if (c) { n = 1; } else { n = 2; }
//! print(n);                          // which register is `n`?
//! ```
//!
//! Answering that in SSA needs phi nodes. Instead a variable keeps **one**
//! virtual register for its whole life and may be written many times, so both
//! branches assign the same register and the join needs nothing. The cost is
//! that live ranges can no longer be read off in one forward pass — see
//! [`crate::codegen::regalloc`], which computes them with a dataflow analysis.
//!
//! Temporaries are still written exactly once each.

use std::collections::HashMap;

use crate::ast::{
    BinOp, Block as AstBlock, CmpOp, Expr, ExprKind, FnDecl, Program as Ast, Stmt, Ty,
};
use crate::sema::Types;

/// A virtual register: a value name, not yet a machine register. Scoped to one
/// [`Function`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VReg(pub u32);

/// Index into [`Program::strings`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StrId(pub u32);

/// Index into [`Function::blocks`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId(pub u32);

/// Index into [`Program::functions`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FuncId(pub u32);

/// An instruction operand: either an immediate or a virtual register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value {
    Const(i64),
    Reg(VReg),
}

#[derive(Clone, Debug)]
pub enum Instr {
    /// `dst = val`
    Const { dst: VReg, val: i64 },
    /// `dst = &strings[id]`
    StrAddr { dst: VReg, id: StrId },
    /// `dst = src`
    Copy { dst: VReg, src: Value },
    /// `dst = lhs op rhs`
    Bin { op: BinOp, dst: VReg, lhs: Value, rhs: Value },
    /// `dst = (lhs op rhs)`, producing 0 or 1.
    Cmp { op: CmpOp, dst: VReg, lhs: Value, rhs: Value },
    /// `dst = arg(index)`: the incoming parameter that arrived in the ABI's
    /// register for `index`.
    ///
    /// These are the first instructions of the entry block, and they exist so
    /// that a parameter has a *definition point* at the top of the function.
    /// Without one, liveness would start a parameter's interval at its first
    /// use and happily hand its register to something else in the meantime.
    Param { dst: VReg, index: u32 },
    /// `dst = callee(args)`, with `dst` absent when the result is discarded.
    Call { dst: Option<VReg>, callee: FuncId, args: Vec<Value> },
    Print { ty: Ty, val: Value },
}

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

    /// Virtual registers read by the terminator itself.
    pub fn uses(&self) -> Vec<VReg> {
        match self {
            Terminator::Branch { cond: Value::Reg(reg), .. }
            | Terminator::Return(Some(Value::Reg(reg))) => vec![*reg],
            _ => Vec::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Block {
    /// Assembly label and dump name, e.g. `then0` or `loop2`.
    pub label: String,
    pub instrs: Vec<Instr>,
    pub term: Terminator,
}

impl Instr {
    /// Virtual registers read by this instruction.
    pub fn uses(&self) -> Vec<VReg> {
        let regs = |values: &[Value]| {
            values
                .iter()
                .filter_map(|v| match v {
                    Value::Reg(r) => Some(*r),
                    Value::Const(_) => None,
                })
                .collect()
        };
        match self {
            Instr::Const { .. } | Instr::StrAddr { .. } | Instr::Param { .. } => Vec::new(),
            Instr::Copy { src, .. } => regs(&[*src]),
            Instr::Bin { lhs, rhs, .. } | Instr::Cmp { lhs, rhs, .. } => regs(&[*lhs, *rhs]),
            Instr::Print { val, .. } => regs(&[*val]),
            Instr::Call { args, .. } => regs(args),
        }
    }

    /// The virtual register written by this instruction, if any.
    pub fn def(&self) -> Option<VReg> {
        match self {
            Instr::Const { dst, .. }
            | Instr::StrAddr { dst, .. }
            | Instr::Copy { dst, .. }
            | Instr::Bin { dst, .. }
            | Instr::Param { dst, .. }
            | Instr::Cmp { dst, .. } => Some(*dst),
            Instr::Call { dst, .. } => *dst,
            Instr::Print { .. } => None,
        }
    }

    /// Whether this instruction performs a call, and therefore destroys the
    /// caller-saved registers.
    pub fn is_call(&self) -> bool {
        matches!(self, Instr::Print { .. } | Instr::Call { .. })
    }
}

/// One function's control flow graph, virtual registers and signature.
pub struct Function {
    pub name: String,
    /// The register each parameter was lowered into, in declaration order.
    pub params: Vec<VReg>,
    /// `None` for a function that returns nothing.
    pub ret: Option<Ty>,
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

    fn value_name(&self, value: &Value) -> String {
        match value {
            Value::Const(c) => c.to_string(),
            Value::Reg(r) => format!("%{}", self.name_of(*r)),
        }
    }

    /// The signature line used by the IR dump and by assembly comments.
    pub fn signature(&self) -> String {
        let params: Vec<String> =
            self.params.iter().map(|&r| format!("%{}", self.name_of(r))).collect();
        let ret = match self.ret {
            Some(ty) => format!(" -> {}", ty.name()),
            None => String::new(),
        };
        format!("fn {}({}){}", self.name, params.join(", "), ret)
    }
}

pub struct Program {
    pub functions: Vec<Function>,
    /// Interned string literals, each stored without its NUL terminator.
    pub strings: Vec<Vec<u8>>,
}

impl Program {
    pub fn function(&self, id: FuncId) -> &Function {
        &self.functions[id.0 as usize]
    }

    /// Render the IR for `--emit ir`.
    pub fn dump(&self) -> String {
        let mut out = String::new();
        for (i, s) in self.strings.iter().enumerate() {
            out.push_str(&format!("str{i} = {:?}\n", String::from_utf8_lossy(s)));
        }
        if !self.strings.is_empty() {
            out.push('\n');
        }

        for function in &self.functions {
            out.push_str(&format!("{}:\n", function.signature()));

            // Numbering restarts per function, matching the indices the
            // allocator works with.
            let mut index = 0;
            for block in &function.blocks {
                out.push_str(&format!("{}:\n", block.label));
                for instr in &block.instrs {
                    let text = self.instr_text(function, instr);
                    out.push_str(&format!("{index:>3}  {text}\n"));
                    index += 1;
                }

                let text = match &block.term {
                    Terminator::Jump(target) => {
                        format!("jump {}", function.block(*target).label)
                    }
                    Terminator::Branch { cond, then_blk, else_blk } => format!(
                        "branch {} ? {} : {}",
                        function.value_name(cond),
                        function.block(*then_blk).label,
                        function.block(*else_blk).label
                    ),
                    Terminator::Return(None) => "return".to_string(),
                    Terminator::Return(Some(value)) => {
                        format!("return {}", function.value_name(value))
                    }
                };
                out.push_str(&format!("{index:>3}  {text}\n"));
                index += 1;
            }
            out.push('\n');
        }
        out
    }

    fn instr_text(&self, function: &Function, instr: &Instr) -> String {
        let value = |v: &Value| function.value_name(v);
        match instr {
            Instr::Const { dst, val } => format!("%{} = const {val}", function.name_of(*dst)),
            Instr::StrAddr { dst, id } => {
                format!("%{} = straddr str{}", function.name_of(*dst), id.0)
            }
            Instr::Copy { dst, src } => {
                format!("%{} = copy {}", function.name_of(*dst), value(src))
            }
            Instr::Bin { op, dst, lhs, rhs } => format!(
                "%{} = {} {}, {}",
                function.name_of(*dst),
                op_name(*op),
                value(lhs),
                value(rhs)
            ),
            Instr::Cmp { op, dst, lhs, rhs } => format!(
                "%{} = cmp {} {}, {}",
                function.name_of(*dst),
                op.symbol(),
                value(lhs),
                value(rhs)
            ),
            Instr::Param { dst, index } => {
                format!("%{} = param {index}", function.name_of(*dst))
            }
            Instr::Call { dst, callee, args } => {
                let args: Vec<String> = args.iter().map(value).collect();
                let call = format!("call {}({})", self.function(*callee).name, args.join(", "));
                match dst {
                    Some(dst) => format!("%{} = {call}", function.name_of(*dst)),
                    None => call,
                }
            }
            Instr::Print { ty, val } => format!("print {} {}", ty.name(), value(val)),
        }
    }
}

fn op_name(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "add",
        BinOp::Sub => "sub",
        BinOp::Mul => "mul",
        BinOp::Div => "div",
    }
}

/// Lower a type-checked AST to IR. Assumes [`crate::sema::check`] succeeded.
pub fn lower(ast: &Ast, types: &Types) -> Program {
    // Function ids follow declaration order, so a call can be lowered to an
    // index without caring whether the callee has been lowered yet — which is
    // exactly what recursion and forward calls need.
    let ids: HashMap<String, FuncId> = ast
        .functions
        .iter()
        .enumerate()
        .map(|(index, f)| (f.name.clone(), FuncId(index as u32)))
        .collect();

    let mut strings = Vec::new();
    let mut functions = Vec::new();
    for decl in &ast.functions {
        let lowering = Lowering {
            blocks: Vec::new(),
            vreg_names: Vec::new(),
            current: BlockId(0),
            scopes: vec![HashMap::new()],
            name_counts: HashMap::new(),
            types,
            strings: &mut strings,
            ids: &ids,
        };
        functions.push(lowering.run(decl));
    }

    Program { functions, strings }
}

/// Drop the blocks nothing can reach, and renumber the survivors.
///
/// Lowering a `return` opens a fresh block for whatever follows it, which is
/// usually nothing at all. Without this pass every function ending in a
/// `return` would carry a stray block, and the backend would dutifully emit a
/// second, unreachable epilogue for it.
fn prune_unreachable(blocks: Vec<Block>) -> Vec<Block> {
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
            // A label carries the block's number, so it has to follow the
            // renumbering: `else3` becomes `else2` when a block ahead of it went
            // away. Only the digits change; the prefix still says where the
            // block came from.
            let stem = block.label.trim_end_matches(|c: char| c.is_ascii_digit());
            block.label = format!("{stem}{index}");

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

struct Lowering<'a> {
    blocks: Vec<Block>,
    vreg_names: Vec<String>,
    /// The block instructions are currently appended to.
    current: BlockId,
    /// Variable name -> virtual register, one map per open scope.
    scopes: Vec<HashMap<String, VReg>>,
    /// How many registers have borne each name, so a shadowing declaration in
    /// another scope gets a distinct dump name (`i`, then `i.1`).
    name_counts: HashMap<String, u32>,
    types: &'a Types,
    /// Shared with every other function: the strings all land in one section.
    strings: &'a mut Vec<Vec<u8>>,
    ids: &'a HashMap<String, FuncId>,
}

impl Lowering<'_> {
    fn run(mut self, decl: &FnDecl) -> Function {
        self.new_block("entry");

        // Parameters first, so each one is defined at the top of the function.
        let mut params = Vec::new();
        for (index, param) in decl.params.iter().enumerate() {
            let dst = self.declare(&param.name);
            self.emit(Instr::Param { dst, index: index as u32 });
            params.push(dst);
        }

        for stmt in &decl.body.stmts {
            self.stmt(stmt);
        }
        // Falling off the end returns nothing. For a function with a return
        // type sema has already proved this is unreachable.
        self.terminate(Terminator::Return(None));

        Function {
            name: decl.name.clone(),
            params,
            ret: decl.ret,
            blocks: prune_unreachable(self.blocks),
            vreg_names: self.vreg_names,
        }
    }

    // -- blocks ------------------------------------------------------------

    /// Append a new block and return its id. It starts with a placeholder
    /// terminator that [`Self::terminate`] replaces.
    fn new_block(&mut self, label: &str) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        let label = format!("{label}{}", id.0);
        self.blocks.push(Block { label, instrs: Vec::new(), term: Terminator::Return(None) });
        self.current = id;
        id
    }

    fn emit(&mut self, instr: Instr) {
        self.blocks[self.current.0 as usize].instrs.push(instr);
    }

    /// Finish the current block.
    fn terminate(&mut self, term: Terminator) {
        self.blocks[self.current.0 as usize].term = term;
    }

    fn switch_to(&mut self, block: BlockId) {
        self.current = block;
    }

    // -- names -------------------------------------------------------------

    fn fresh(&mut self, name: &str) -> VReg {
        let reg = VReg(self.vreg_names.len() as u32);
        let count = self.name_counts.entry(name.to_string()).or_insert(0);
        let label = if *count == 0 { name.to_string() } else { format!("{name}.{count}") };
        *count += 1;
        self.vreg_names.push(label);
        reg
    }

    /// Temporaries are named after their own index, so `%t7` is always virtual
    /// register 7.
    fn fresh_temp(&mut self) -> VReg {
        let reg = VReg(self.vreg_names.len() as u32);
        self.vreg_names.push(format!("t{}", reg.0));
        reg
    }

    fn declare(&mut self, name: &str) -> VReg {
        let reg = self.fresh(name);
        self.scopes.last_mut().expect("a scope is always open").insert(name.to_string(), reg);
        reg
    }

    fn lookup(&self, name: &str) -> VReg {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
            .expect("sema rejects undeclared variables")
    }

    // -- statements --------------------------------------------------------

    fn block_stmts(&mut self, block: &AstBlock) {
        self.scopes.push(HashMap::new());
        for stmt in &block.stmts {
            self.stmt(stmt);
        }
        self.scopes.pop();
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Decl { name, init, .. } => {
                let dst = self.declare(name);
                self.expr_into(dst, init);
            }
            // The variable keeps its register; the assignment overwrites it.
            Stmt::Assign { name, value, .. } => {
                let dst = self.lookup(name);
                self.expr_into(dst, value);
            }
            Stmt::Print { value, .. } => {
                let ty = self.types.of(value.id);
                let val = self.expr(value);
                self.emit(Instr::Print { ty, val });
            }
            Stmt::If { cond, then_block, else_block } => self.if_stmt(cond, then_block, else_block),
            Stmt::While { cond, body } => self.while_stmt(cond, body),
            // `for (init; cond; step) body` is exactly `init; while (cond) { body; step; }`
            // with the initialiser's variable scoped to the loop.
            Stmt::For { init, cond, step, body } => {
                self.scopes.push(HashMap::new());
                self.stmt(init);
                self.loop_with_step(cond, body, Some(step));
                self.scopes.pop();
            }
            Stmt::Return { value, .. } => {
                let value = value.as_ref().map(|expr| self.expr(expr));
                self.terminate(Terminator::Return(value));
                // Anything written after a `return` still needs somewhere to
                // go. This block has no predecessor, so it is dead code the
                // backend simply never reaches.
                self.new_block("unreachable");
            }
            Stmt::Call(call) => {
                let (callee, args) = self.call_parts(call);
                self.emit(Instr::Call { dst: None, callee, args });
            }
        }
    }

    fn if_stmt(&mut self, cond: &Expr, then_block: &AstBlock, else_block: &Option<AstBlock>) {
        let cond = self.expr(cond);
        // The branch belongs to whichever block the condition was computed in.
        let entry = self.current;

        let then_id = self.new_block("then");
        self.block_stmts(then_block);
        let then_exit = self.current;

        let (else_id, else_exit) = match else_block {
            Some(block) => {
                let id = self.new_block("else");
                self.block_stmts(block);
                (id, Some(self.current))
            }
            None => (BlockId(0), None), // patched below
        };

        let join = self.new_block("join");

        self.blocks[entry.0 as usize].term = Terminator::Branch {
            cond,
            then_blk: then_id,
            else_blk: if else_block.is_some() { else_id } else { join },
        };

        self.blocks[then_exit.0 as usize].term = Terminator::Jump(join);
        if let Some(exit) = else_exit {
            self.blocks[exit.0 as usize].term = Terminator::Jump(join);
        }

        self.switch_to(join);
    }

    fn while_stmt(&mut self, cond: &Expr, body: &AstBlock) {
        self.loop_with_step(cond, body, None);
    }

    /// The shape shared by `while` and `for`: a header that re-tests the
    /// condition on every iteration, a body, and an optional step run at the
    /// end of the body.
    fn loop_with_step(&mut self, cond: &Expr, body: &AstBlock, step: Option<&Stmt>) {
        let before = self.current;

        // The condition must be re-evaluated each time round, so it gets a
        // block of its own that the body jumps back to.
        let header = self.new_block("loop");
        let cond = self.expr(cond);
        let header_exit = self.current;

        let body_id = self.new_block("body");
        self.block_stmts(body);
        if let Some(step) = step {
            self.stmt(step);
        }
        let body_exit = self.current;

        let after = self.new_block("done");

        self.blocks[before.0 as usize].term = Terminator::Jump(header);
        self.blocks[header_exit.0 as usize].term =
            Terminator::Branch { cond, then_blk: body_id, else_blk: after };
        // The back edge: this is what makes liveness need a fixpoint.
        self.blocks[body_exit.0 as usize].term = Terminator::Jump(header);

        self.switch_to(after);
    }

    // -- expressions -------------------------------------------------------

    /// The callee and evaluated arguments of a call expression.
    fn call_parts(&mut self, call: &Expr) -> (FuncId, Vec<Value>) {
        let ExprKind::Call { name, args, .. } = &call.kind else {
            unreachable!("sema guarantees this is a call");
        };
        // Arguments are evaluated left to right, before the call itself; a
        // nested call therefore finishes first and leaves its result in a
        // temporary that the outer call reads as an operand.
        let args: Vec<Value> = args.iter().map(|arg| self.expr(arg)).collect();
        let callee = self.ids[name.as_str()];
        (callee, args)
    }

    /// Lower an expression whose result must land in `dst`.
    fn expr_into(&mut self, dst: VReg, expr: &Expr) {
        match &expr.kind {
            ExprKind::Int(v) => self.emit(Instr::Const { dst, val: *v }),
            ExprKind::Bool(v) => self.emit(Instr::Const { dst, val: i64::from(*v) }),
            ExprKind::Str(bytes) => {
                let id = self.intern(bytes);
                self.emit(Instr::StrAddr { dst, id });
            }
            ExprKind::Bin { op, lhs, rhs } => {
                let lhs = self.expr(lhs);
                let rhs = self.expr(rhs);
                self.emit(Instr::Bin { op: *op, dst, lhs, rhs });
            }
            ExprKind::Cmp { op, lhs, rhs } => {
                let lhs = self.expr(lhs);
                let rhs = self.expr(rhs);
                self.emit(Instr::Cmp { op: *op, dst, lhs, rhs });
            }
            ExprKind::Neg(operand) => {
                let val = self.expr(operand);
                self.emit(Instr::Bin { op: BinOp::Sub, dst, lhs: Value::Const(0), rhs: val });
            }
            ExprKind::Call { .. } => {
                let (callee, args) = self.call_parts(expr);
                self.emit(Instr::Call { dst: Some(dst), callee, args });
            }
            ExprKind::Var(_) => {
                let src = self.expr(expr);
                self.emit(Instr::Copy { dst, src });
            }
        }
    }

    /// Lower an expression used as an operand, producing a value to read.
    fn expr(&mut self, expr: &Expr) -> Value {
        match &expr.kind {
            // Literals stay immediates so the backend can fold them into the
            // instruction that consumes them.
            ExprKind::Int(v) => Value::Const(*v),
            ExprKind::Bool(v) => Value::Const(i64::from(*v)),
            ExprKind::Var(name) => Value::Reg(self.lookup(name)),
            ExprKind::Neg(operand) => {
                if let ExprKind::Int(v) = operand.kind {
                    return Value::Const(v.wrapping_neg());
                }
                let dst = self.fresh_temp();
                self.expr_into(dst, expr);
                Value::Reg(dst)
            }
            ExprKind::Str(_)
            | ExprKind::Bin { .. }
            | ExprKind::Cmp { .. }
            | ExprKind::Call { .. } => {
                let dst = self.fresh_temp();
                self.expr_into(dst, expr);
                Value::Reg(dst)
            }
        }
    }

    fn intern(&mut self, bytes: &[u8]) -> StrId {
        if let Some(i) = self.strings.iter().position(|s| s == bytes) {
            return StrId(i as u32);
        }
        self.strings.push(bytes.to_vec());
        StrId(self.strings.len() as u32 - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lexer, parser, sema};

    fn lower_src(src: &str) -> Program {
        let ast = parser::parse(&lexer::lex(src).unwrap()).unwrap();
        let types = sema::check(&ast).unwrap();
        lower(&ast, &types)
    }

    /// Lower a `main` body and return that one function.
    fn lower_main(body: &str) -> Program {
        lower_src(&format!("fn main() {{\n{body}\n}}\n"))
    }

    /// The dump of a single-function program, without its signature line and
    /// trailing blank, so the existing block-shape assertions stay readable.
    fn body_dump(program: &Program) -> String {
        let dump = program.dump();
        let start = dump.find(":\n").expect("a signature line") + 2;
        dump[start..].trim_end().to_string() + "\n"
    }

    fn labels(function: &Function) -> Vec<&str> {
        function.blocks.iter().map(|b| b.label.as_str()).collect()
    }

    #[test]
    fn lowers_the_sample_program() {
        let ir = lower_main("int x = 10;\nint y = 20;\nstring s = \"hi\";\nprint(x + y);\nprint(s);");
        assert_eq!(
            body_dump(&ir),
            concat!(
                "entry0:\n",
                "  0  %x = const 10\n",
                "  1  %y = const 20\n",
                "  2  %s = straddr str0\n",
                "  3  %t3 = add %x, %y\n",
                "  4  print int %t3\n",
                "  5  print string %s\n",
                "  6  return\n",
            )
        );
    }

    #[test]
    fn an_assignment_writes_the_variables_own_register() {
        // No `%n.1`: with control flow a variable must have one home, so the
        // second write targets the same register.
        let ir = lower_main("int n = 1;\nn = n + 41;\nprint(n);");
        assert_eq!(
            body_dump(&ir),
            concat!(
                "entry0:\n",
                "  0  %n = const 1\n",
                "  1  %n = add %n, 41\n",
                "  2  print int %n\n",
                "  3  return\n",
            )
        );
    }

    #[test]
    fn an_if_produces_a_diamond() {
        let ir = lower_main("int n = 0;\nif (n < 1) {\n  n = 2;\n} else {\n  n = 3;\n}\nprint(n);");
        let main = &ir.functions[0];
        assert_eq!(labels(main), vec!["entry0", "then1", "else2", "join3"]);
        assert!(matches!(main.blocks[0].term, Terminator::Branch { .. }));
        assert!(matches!(main.blocks[1].term, Terminator::Jump(BlockId(3))));
        assert!(matches!(main.blocks[2].term, Terminator::Jump(BlockId(3))));
    }

    #[test]
    fn an_if_without_else_branches_straight_to_the_join() {
        let ir = lower_main("int n = 0;\nif (n < 1) {\n  n = 2;\n}\nprint(n);");
        let main = &ir.functions[0];
        assert_eq!(labels(main), vec!["entry0", "then1", "join2"]);
        match main.blocks[0].term {
            Terminator::Branch { then_blk, else_blk, .. } => {
                assert_eq!((then_blk, else_blk), (BlockId(1), BlockId(2)));
            }
            ref other => panic!("expected a branch, got {other:?}"),
        }
    }

    #[test]
    fn a_while_loop_closes_a_back_edge() {
        let ir = lower_main("int i = 0;\nwhile (i < 3) {\n  i = i + 1;\n}\nprint(i);");
        let main = &ir.functions[0];
        assert_eq!(labels(main), vec!["entry0", "loop1", "body2", "done3"]);
        // The body jumps back to the header, which re-tests the condition.
        assert!(matches!(main.blocks[2].term, Terminator::Jump(BlockId(1))));
        assert!(matches!(main.blocks[0].term, Terminator::Jump(BlockId(1))));
    }

    #[test]
    fn a_for_loop_desugars_into_the_same_shape() {
        let with_for = lower_main("for (int i = 0; i < 3; i = i + 1) {\n  print(i);\n}");
        let with_while = lower_main("int i = 0;\nwhile (i < 3) {\n  print(i);\n  i = i + 1;\n}");
        assert_eq!(with_for.dump(), with_while.dump());
    }

    #[test]
    fn literal_operands_stay_immediates() {
        let ir = lower_main("print(1 + 2);");
        let main = &ir.functions[0];
        assert_eq!(main.blocks[0].instrs.len(), 2);
        assert!(matches!(
            main.blocks[0].instrs[0],
            Instr::Bin { lhs: Value::Const(1), rhs: Value::Const(2), .. }
        ));
    }

    #[test]
    fn bools_lower_to_integer_constants() {
        let ir = lower_main("bool ready = true;\nbool done = false;\nprint(ready);\nprint(done);");
        assert_eq!(
            body_dump(&ir),
            concat!(
                "entry0:\n",
                "  0  %ready = const 1\n",
                "  1  %done = const 0\n",
                "  2  print bool %ready\n",
                "  3  print bool %done\n",
                "  4  return\n",
            )
        );
    }

    #[test]
    fn a_printed_bool_literal_stays_an_immediate() {
        let ir = lower_main("print(false);");
        let main = &ir.functions[0];
        assert_eq!(main.blocks[0].instrs.len(), 1);
        assert!(matches!(
            main.blocks[0].instrs[0],
            Instr::Print { ty: Ty::Bool, val: Value::Const(0) }
        ));
    }

    #[test]
    fn shadowed_variables_get_distinct_registers() {
        let ir = lower_main("int i = 1;\nif (true) {\n  int i = 2;\n  print(i);\n}\nprint(i);");
        let names = &ir.functions[0].vreg_names;
        assert!(names.contains(&"i".to_string()));
        assert!(names.contains(&"i.1".to_string()));
    }

    #[test]
    fn identical_strings_are_interned_once() {
        let ir = lower_main("string a = \"hi\";\nstring b = \"hi\";\nprint(a);\nprint(b);");
        assert_eq!(ir.strings.len(), 1);
    }

    // -- functions ---------------------------------------------------------

    #[test]
    fn each_function_gets_its_own_graph_and_registers() {
        let ir = lower_src(
            "fn add(int a, int b) -> int {\n  return a + b;\n}\nfn main() {\n  print(add(1, 2));\n}",
        );
        assert_eq!(ir.functions.len(), 2);
        // Both functions number their blocks and registers from zero.
        assert_eq!(ir.functions[0].blocks[0].label, "entry0");
        assert_eq!(ir.functions[1].blocks[0].label, "entry0");
        assert_eq!(ir.functions[0].params, vec![VReg(0), VReg(1)]);
    }

    #[test]
    fn parameters_are_defined_at_the_top_of_the_entry_block() {
        let ir = lower_src("fn f(int a, int b) {\n  print(a);\n}\nfn main() {\n  f(1, 2);\n}");
        let f = &ir.functions[0];
        assert!(matches!(f.blocks[0].instrs[0], Instr::Param { index: 0, .. }));
        assert!(matches!(f.blocks[0].instrs[1], Instr::Param { index: 1, .. }));
    }

    #[test]
    fn a_call_lowers_to_an_instruction_naming_its_callee() {
        let ir = lower_src(
            "fn add(int a, int b) -> int {\n  return a + b;\n}\nfn main() {\n  print(add(1, 2));\n}",
        );
        let main = &ir.functions[1];
        match &main.blocks[0].instrs[0] {
            Instr::Call { dst: Some(_), callee, args } => {
                assert_eq!(*callee, FuncId(0));
                assert_eq!(args, &vec![Value::Const(1), Value::Const(2)]);
            }
            other => panic!("expected a call, got {other:?}"),
        }
    }

    #[test]
    fn a_call_statement_discards_its_result() {
        let ir = lower_src("fn f() -> int {\n  return 1;\n}\nfn main() {\n  f();\n}");
        assert!(matches!(ir.functions[1].blocks[0].instrs[0], Instr::Call { dst: None, .. }));
    }

    #[test]
    fn a_return_carries_its_value_in_the_terminator() {
        let ir = lower_src("fn one() -> int {\n  return 1;\n}\nfn main() {\n}");
        assert!(matches!(
            ir.functions[0].blocks[0].term,
            Terminator::Return(Some(Value::Const(1)))
        ));
    }

    #[test]
    fn a_bare_return_carries_nothing() {
        let ir = lower_src("fn f() {\n  return;\n}\nfn main() {\n}");
        assert!(matches!(ir.functions[0].blocks[0].term, Terminator::Return(None)));
    }

    #[test]
    fn code_after_a_return_is_pruned() {
        // The `print` is lowered into a block nothing jumps to, and that block
        // never reaches the backend.
        let ir = lower_src("fn f() {\n  return;\n  print(1);\n}\nfn main() {\n}");
        assert_eq!(labels(&ir.functions[0]), vec!["entry0"]);
    }

    #[test]
    fn an_if_where_both_arms_return_keeps_both_returns() {
        // The join block is unreachable and goes away, but the two `return`
        // terminators must survive the pruning intact.
        let ir = lower_src(
            "fn f(int n) -> int {\n  if (n < 2) {\n    return 1;\n  } else {\n    \
             return 2;\n  }\n}\nfn main() {\n}",
        );
        let f = &ir.functions[0];
        assert_eq!(labels(f), vec!["entry0", "then1", "else2"]);
        assert!(matches!(f.blocks[1].term, Terminator::Return(Some(Value::Const(1)))));
        assert!(matches!(f.blocks[2].term, Terminator::Return(Some(Value::Const(2)))));
    }

    #[test]
    fn a_recursive_call_names_its_own_function() {
        let ir = lower_src(
            "fn fib(int n) -> int {\n  if (n < 2) {\n    return n;\n  } else {\n    \
             return fib(n - 1) + fib(n - 2);\n  }\n}\nfn main() {\n  print(fib(10));\n}",
        );
        let fib = &ir.functions[0];
        let calls: Vec<&Instr> = fib
            .blocks
            .iter()
            .flat_map(|b| &b.instrs)
            .filter(|i| matches!(i, Instr::Call { .. }))
            .collect();
        assert_eq!(calls.len(), 2);
        for call in calls {
            assert!(matches!(call, Instr::Call { callee: FuncId(0), .. }), "{call:?}");
        }
    }

    #[test]
    fn strings_are_shared_across_functions() {
        let ir = lower_src(
            "fn a() {\n  print(\"hi\");\n}\nfn main() {\n  print(\"hi\");\n  a();\n}",
        );
        assert_eq!(ir.strings.len(), 1);
    }

    #[test]
    fn a_value_used_by_a_call_does_not_cross_it_but_a_nested_one_does() {
        // In `f(g(1), 2)` the result of `g` is live across nothing; in
        // `f(g(1), h(2))` it is live across the call to `h`.
        let ir = lower_src(
            "fn g(int n) -> int {\n  return n;\n}\nfn h(int n) -> int {\n  return n;\n}\n\
             fn f(int a, int b) -> int {\n  return a;\n}\n\
             fn main() {\n  print(f(g(1), h(2)));\n}",
        );
        let main = ir.functions.last().unwrap();
        let calls = main.blocks[0].instrs.iter().filter(|i| i.is_call()).count();
        assert_eq!(calls, 4); // g, h, f, and the print
    }
}
