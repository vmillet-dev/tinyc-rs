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
    /// Where a loop leaves.
    Done,
    /// Opened after a `return` for whatever follows it, and reached by nothing.
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
            BlockKind::Done => "done",
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

impl Instr {
    /// Show `visit` every virtual register this instruction reads.
    ///
    /// See [`Terminator::uses`] for why this hands registers to a callback
    /// rather than collecting them.
    pub fn uses(&self, mut visit: impl FnMut(VReg)) {
        let mut reg = |value: &Value| {
            if let Value::Reg(r) = value {
                visit(*r);
            }
        };
        match self {
            Instr::Const { .. } | Instr::StrAddr { .. } | Instr::Param { .. } => {}
            Instr::Copy { src, .. } => reg(src),
            Instr::Bin { lhs, rhs, .. } | Instr::Cmp { lhs, rhs, .. } => {
                reg(lhs);
                reg(rhs);
            }
            Instr::Print { val, .. } => reg(val),
            Instr::Call { args, .. } => args.iter().for_each(reg),
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
                out.push_str(&format!("{}:\n", block.label()));
                for instr in &block.instrs {
                    let text = self.instr_text(function, instr);
                    out.push_str(&format!("{index:>3}  {text}\n"));
                    index += 1;
                }

                let text = match &block.term {
                    Terminator::Jump(target) => {
                        format!("jump {}", function.block(*target).label())
                    }
                    Terminator::Branch { cond, then_blk, else_blk } => format!(
                        "branch {} ? {} : {}",
                        function.value_name(cond),
                        function.block(*then_blk).label(),
                        function.block(*else_blk).label()
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

    /// One instruction, in the form `--emit ir` prints it. The backend echoes
    /// these as comments, so a reader can line the assembly up against the IR
    /// dump instruction by instruction.
    pub fn instr_text(&self, function: &Function, instr: &Instr) -> String {
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
    //
    // The first declaration of a name wins, matching the table `sema` built:
    // a duplicate is an error, and the two stages must at least agree on which
    // one they were talking about.
    let mut ids: HashMap<String, FuncId> = HashMap::new();
    for (index, function) in ast.functions.iter().enumerate() {
        ids.entry(function.name.clone()).or_insert(FuncId(index as u32));
    }

    let mut strings = Strings::default();
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

    let functions = prune_unreachable_functions(functions, ids.get(crate::sema::ENTRY_POINT));
    Program { functions, strings: strings.bytes }
}

/// The literals every function shares, plus the index that keeps interning them
/// a lookup rather than a scan.
#[derive(Default)]
struct Strings {
    bytes: Vec<Vec<u8>>,
    ids: HashMap<Vec<u8>, StrId>,
}

/// Drop the functions nothing can call, and renumber the survivors.
///
/// The same walk as [`prune_unreachable`], one level up: the call graph instead
/// of the control flow graph, rooted at the entry point rather than at block 0.
/// A helper nobody calls costs a label, a prologue and an epilogue otherwise.
fn prune_unreachable_functions(functions: Vec<Function>, entry: Option<&FuncId>) -> Vec<Function> {
    let Some(&entry) = entry else {
        // No entry point: `sema` has already rejected the program, and there is
        // no root to walk from.
        return functions;
    };

    let mut reachable = vec![false; functions.len()];
    let mut stack = vec![entry];
    while let Some(id) = stack.pop() {
        let index = id.0 as usize;
        if std::mem::replace(&mut reachable[index], true) {
            continue;
        }
        for block in &functions[index].blocks {
            for instr in &block.instrs {
                if let Instr::Call { callee, .. } = instr {
                    stack.push(*callee);
                }
            }
        }
    }

    // Old index -> new index, for the calls that name them.
    let mut renumber = vec![FuncId(0); functions.len()];
    let mut next = 0;
    for (index, keep) in reachable.iter().enumerate() {
        if *keep {
            renumber[index] = FuncId(next);
            next += 1;
        }
    }

    functions
        .into_iter()
        .zip(&reachable)
        .filter(|(_, keep)| **keep)
        .map(|(mut function, _)| {
            for block in &mut function.blocks {
                for instr in &mut block.instrs {
                    if let Instr::Call { callee, .. } = instr {
                        *callee = renumber[callee.0 as usize];
                    }
                }
            }
            function
        })
        .collect()
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

/// A block while it is still being filled in.
///
/// The terminator is an `Option` on purpose: "not decided yet" is a state the
/// lowering really is in, and giving it a `Terminator` value instead would mean
/// a block whose terminator was never patched still assembles — into a plausible
/// but wrong `ret`. [`Lowering::run`] turns the `None` that should be impossible
/// into a panic rather than a miscompilation.
struct PendingBlock {
    kind: BlockKind,
    instrs: Vec<Instr>,
    term: Option<Terminator>,
}

struct Lowering<'a> {
    blocks: Vec<PendingBlock>,
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
    strings: &'a mut Strings,
    ids: &'a HashMap<String, FuncId>,
}

impl Lowering<'_> {
    fn run(mut self, decl: &FnDecl) -> Function {
        self.new_block(BlockKind::Entry);

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

        // Every block that stopped being the current one was finished by the
        // construct that moved away from it, and the last one was just finished
        // above. A `None` here would mean a path forgot to.
        let blocks = self
            .blocks
            .into_iter()
            .enumerate()
            .map(|(index, block)| Block {
                kind: block.kind,
                index: index as u32,
                instrs: block.instrs,
                term: block.term.unwrap_or_else(|| {
                    panic!("block {index} of `{}` was left unterminated", decl.name)
                }),
            })
            .collect();

        Function {
            name: decl.name.clone(),
            params,
            ret: decl.ret,
            blocks: prune_unreachable(blocks),
            vreg_names: self.vreg_names,
        }
    }

    // -- blocks ------------------------------------------------------------

    /// Append a new block, make it current, and return its id. It has no
    /// terminator until [`Self::terminate`] gives it one.
    fn new_block(&mut self, kind: BlockKind) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        self.blocks.push(PendingBlock { kind, instrs: Vec::new(), term: None });
        self.current = id;
        id
    }

    fn emit(&mut self, instr: Instr) {
        self.blocks[self.current.0 as usize].instrs.push(instr);
    }

    /// Finish the current block.
    fn terminate(&mut self, term: Terminator) {
        self.finish(self.current, term);
    }

    /// Finish a block that is no longer the current one.
    fn finish(&mut self, block: BlockId, term: Terminator) {
        self.blocks[block.0 as usize].term = Some(term);
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
                self.new_block(BlockKind::Unreachable);
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

        let then_id = self.new_block(BlockKind::Then);
        self.block_stmts(then_block);
        let then_exit = self.current;

        // `None` all the way through: with no `else`, there is no block to name
        // and no exit to send to the join — the branch goes straight there.
        let alternative = else_block.as_ref().map(|block| {
            let id = self.new_block(BlockKind::Else);
            self.block_stmts(block);
            (id, self.current)
        });

        let join = self.new_block(BlockKind::Join);

        self.finish(
            entry,
            Terminator::Branch {
                cond,
                then_blk: then_id,
                else_blk: alternative.map_or(join, |(id, _)| id),
            },
        );

        self.finish(then_exit, Terminator::Jump(join));
        if let Some((_, exit)) = alternative {
            self.finish(exit, Terminator::Jump(join));
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
        let header = self.new_block(BlockKind::Loop);
        let cond = self.expr(cond);
        let header_exit = self.current;

        let body_id = self.new_block(BlockKind::Body);
        self.block_stmts(body);
        if let Some(step) = step {
            self.stmt(step);
        }
        let body_exit = self.current;

        let after = self.new_block(BlockKind::Done);

        self.finish(before, Terminator::Jump(header));
        self.finish(header_exit, Terminator::Branch { cond, then_blk: body_id, else_blk: after });
        // The back edge: this is what makes liveness need a fixpoint.
        self.finish(body_exit, Terminator::Jump(header));

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
                match fold_bin(*op, lhs, rhs) {
                    Some(val) => self.emit(Instr::Const { dst, val }),
                    None => self.emit(Instr::Bin { op: *op, dst, lhs, rhs }),
                }
            }
            ExprKind::Cmp { op, lhs, rhs } => {
                let lhs = self.expr(lhs);
                let rhs = self.expr(rhs);
                match fold_cmp(*op, lhs, rhs) {
                    Some(val) => self.emit(Instr::Const { dst, val }),
                    None => self.emit(Instr::Cmp { op: *op, dst, lhs, rhs }),
                }
            }
            ExprKind::Neg(operand) => {
                let val = self.expr(operand);
                match val {
                    Value::Const(c) => self.emit(Instr::Const { dst, val: c.wrapping_neg() }),
                    val => self.emit(Instr::Bin {
                        op: BinOp::Sub,
                        dst,
                        lhs: Value::Const(0),
                        rhs: val,
                    }),
                }
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
            ExprKind::Neg(operand) => match self.expr(operand) {
                // An operand that is already a literal folds, and so does the
                // whole tree above it: `-(2 * 3)` never reaches an instruction.
                Value::Const(c) => Value::Const(c.wrapping_neg()),
                val => {
                    let dst = self.fresh_temp();
                    self.emit(Instr::Bin {
                        op: BinOp::Sub,
                        dst,
                        lhs: Value::Const(0),
                        rhs: val,
                    });
                    Value::Reg(dst)
                }
            },
            ExprKind::Bin { op, lhs, rhs } => {
                let lhs = self.expr(lhs);
                let rhs = self.expr(rhs);
                if let Some(val) = fold_bin(*op, lhs, rhs) {
                    return Value::Const(val);
                }
                let dst = self.fresh_temp();
                self.emit(Instr::Bin { op: *op, dst, lhs, rhs });
                Value::Reg(dst)
            }
            ExprKind::Cmp { op, lhs, rhs } => {
                let lhs = self.expr(lhs);
                let rhs = self.expr(rhs);
                if let Some(val) = fold_cmp(*op, lhs, rhs) {
                    return Value::Const(val);
                }
                let dst = self.fresh_temp();
                self.emit(Instr::Cmp { op: *op, dst, lhs, rhs });
                Value::Reg(dst)
            }
            ExprKind::Str(_) | ExprKind::Call { .. } => {
                let dst = self.fresh_temp();
                self.expr_into(dst, expr);
                Value::Reg(dst)
            }
        }
    }

    fn intern(&mut self, bytes: &[u8]) -> StrId {
        if let Some(&id) = self.strings.ids.get(bytes) {
            return id;
        }
        let id = StrId(self.strings.bytes.len() as u32);
        self.strings.bytes.push(bytes.to_vec());
        self.strings.ids.insert(bytes.to_vec(), id);
        id
    }
}

/// Evaluate `lhs op rhs` now, when both are already known.
///
/// Answers `None` whenever the machine would not agree with the answer: a
/// division the CPU would trap on stays an instruction, so the program still
/// fails where it was written to instead of in the compiler.
fn fold_bin(op: BinOp, lhs: Value, rhs: Value) -> Option<i64> {
    let (Value::Const(a), Value::Const(b)) = (lhs, rhs) else { return None };
    match op {
        // Wrapping, because that is what the hardware does with these operands.
        BinOp::Add => Some(a.wrapping_add(b)),
        BinOp::Sub => Some(a.wrapping_sub(b)),
        BinOp::Mul => Some(a.wrapping_mul(b)),
        // `checked_div` answers `None` for both `x / 0` and `MIN / -1`, which
        // are exactly the two divisions `idiv` refuses.
        BinOp::Div => a.checked_div(b),
    }
}

/// The same for a comparison, whose result is the 0 or 1 a `bool` is.
fn fold_cmp(op: CmpOp, lhs: Value, rhs: Value) -> Option<i64> {
    let (Value::Const(a), Value::Const(b)) = (lhs, rhs) else { return None };
    let answer = match op {
        CmpOp::Eq => a == b,
        CmpOp::Ne => a != b,
        CmpOp::Lt => a < b,
        CmpOp::Le => a <= b,
        CmpOp::Gt => a > b,
        CmpOp::Ge => a >= b,
    };
    Some(i64::from(answer))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{lexer, parser, sema};

    fn lower_src(src: &str) -> Program {
        let ast = parser::parse(&lexer::lex(src).unwrap()).unwrap();
        let types = sema::check(&ast, 4).unwrap();
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

    fn labels(function: &Function) -> Vec<String> {
        function.blocks.iter().map(|b| b.label()).collect()
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
        // `x` is unknown, so the addition survives — but the 2 next to it is
        // still an operand rather than a register of its own.
        let ir = lower_main("int x = 1;\nprint(x + 2);");
        let main = &ir.functions[0];
        assert!(
            main.blocks[0]
                .instrs
                .iter()
                .any(|i| matches!(i, Instr::Bin { rhs: Value::Const(2), .. })),
            "{}",
            ir.dump()
        );
    }

    #[test]
    fn arithmetic_between_literals_is_done_at_compile_time() {
        let ir = lower_main("print(1 + 2 * 3);");
        let main = &ir.functions[0];
        // The whole tree collapses into the operand of the print: no `add`, no
        // `mul`, and no register to hold the answer either.
        assert_eq!(main.blocks[0].instrs.len(), 1, "{}", ir.dump());
        assert!(
            matches!(main.blocks[0].instrs[0], Instr::Print { val: Value::Const(7), .. }),
            "{}",
            ir.dump()
        );
    }

    #[test]
    fn a_division_the_machine_would_refuse_is_never_folded() {
        // `checked_div` answers `None` for both, so the instruction survives and
        // the program fails where it was written rather than in the compiler.
        for source in ["print(1 / (3 - 3));", "print((0 - 9223372036854775807 - 1) / (0 - 1));"] {
            let ir = lower_main(source);
            assert!(
                ir.functions[0].blocks[0]
                    .instrs
                    .iter()
                    .any(|i| matches!(i, Instr::Bin { op: BinOp::Div, .. })),
                "{source}: {}",
                ir.dump()
            );
        }
    }

    #[test]
    fn a_comparison_between_literals_is_folded_too() {
        let ir = lower_main("bool b = 1 < 2;\nprint(b);");
        assert!(
            matches!(ir.functions[0].blocks[0].instrs[0], Instr::Const { val: 1, .. }),
            "{}",
            ir.dump()
        );
    }

    #[test]
    fn a_function_nothing_calls_is_dropped() {
        let ir = lower_src(
            "fn used() -> int {\n  return 1;\n}\n\
             fn unused() -> int {\n  return 2;\n}\n\
             fn main() {\n  print(used());\n}",
        );
        let names: Vec<&str> = ir.functions.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["used", "main"]);
    }

    #[test]
    fn dropping_a_function_renumbers_the_calls_that_survive() {
        // `unused` sits between the entry point and its callee, so every
        // `FuncId` after it shifts down by one.
        let ir = lower_src(
            "fn unused() {\n}\n\
             fn helper() -> int {\n  return 7;\n}\n\
             fn main() {\n  print(helper());\n}",
        );
        let main = ir.functions.iter().find(|f| f.name == "main").expect("main survives");
        let Instr::Call { callee, .. } = &main.blocks[0].instrs[0] else { panic!("a call") };
        assert_eq!(ir.function(*callee).name, "helper");
    }

    #[test]
    fn a_function_only_an_unused_one_calls_goes_too() {
        let ir = lower_src(
            "fn deep() -> int {\n  return 1;\n}\n\
             fn unused() -> int {\n  return deep();\n}\n\
             fn main() {\n}",
        );
        let names: Vec<&str> = ir.functions.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["main"]);
    }

    #[test]
    fn recursion_keeps_a_function_alive_through_itself() {
        let ir = lower_src(
            "fn fib(int n) -> int {\n  if (n < 2) {\n    return n;\n  }\n  \
             return fib(n - 1) + fib(n - 2);\n}\n\
             fn main() {\n  print(fib(5));\n}",
        );
        assert!(ir.functions.iter().any(|f| f.name == "fib"), "{}", ir.dump());
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
        assert_eq!(ir.functions[0].blocks[0].label(), "entry0");
        assert_eq!(ir.functions[1].blocks[0].label(), "entry0");
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
        let ir = lower_src("fn one() -> int {\n  return 1;\n}\nfn main() {\n  print(one());\n}");
        assert!(matches!(
            ir.functions[0].blocks[0].term,
            Terminator::Return(Some(Value::Const(1)))
        ));
    }

    #[test]
    fn a_bare_return_carries_nothing() {
        let ir = lower_src("fn f() {\n  return;\n}\nfn main() {\n  f();\n}");
        assert!(matches!(ir.functions[0].blocks[0].term, Terminator::Return(None)));
    }

    #[test]
    fn code_after_a_return_is_pruned() {
        // The `print` is lowered into a block nothing jumps to, and that block
        // never reaches the backend.
        let ir = lower_src("fn f() {\n  return;\n  print(1);\n}\nfn main() {\n  f();\n}");
        assert_eq!(labels(&ir.functions[0]), vec!["entry0"]);
    }

    #[test]
    fn an_if_where_both_arms_return_keeps_both_returns() {
        // The join block is unreachable and goes away, but the two `return`
        // terminators must survive the pruning intact.
        let ir = lower_src(
            "fn f(int n) -> int {\n  if (n < 2) {\n    return 1;\n  } else {\n    \
             return 2;\n  }\n}\nfn main() {\n  print(f(1));\n}",
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
