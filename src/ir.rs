//! Stage 4: AST -> intermediate representation.
//!
//! The IR is three-address code over an unbounded supply of *virtual*
//! registers, arranged as a **control flow graph**: a list of basic blocks,
//! each a straight run of instructions ending in a terminator that names its
//! successors.
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

use crate::ast::{BinOp, Block as AstBlock, CmpOp, Expr, ExprKind, Program as Ast, Stmt, Ty};
use crate::sema::Types;

/// A virtual register: a value name, not yet a machine register.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VReg(pub u32);

/// Index into [`Program::strings`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StrId(pub u32);

/// Index into [`Program::blocks`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockId(pub u32);

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
    /// The only call site, and so the only thing that clobbers caller-saved
    /// registers.
    Print { ty: Ty, val: Value },
}

/// How a basic block ends. Every block has exactly one.
#[derive(Clone, Debug)]
pub enum Terminator {
    Jump(BlockId),
    /// Continue at `then_blk` when `cond` is non-zero, `else_blk` otherwise.
    Branch { cond: Value, then_blk: BlockId, else_blk: BlockId },
    /// Leave `main`. Lowering produces exactly one of these, in the last block.
    Return,
}

impl Terminator {
    pub fn successors(&self) -> Vec<BlockId> {
        match self {
            Terminator::Jump(target) => vec![*target],
            Terminator::Branch { then_blk, else_blk, .. } => vec![*then_blk, *else_blk],
            Terminator::Return => Vec::new(),
        }
    }

    /// Virtual registers read by the terminator itself.
    pub fn uses(&self) -> Vec<VReg> {
        match self {
            Terminator::Branch { cond: Value::Reg(reg), .. } => vec![*reg],
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
            Instr::Const { .. } | Instr::StrAddr { .. } => Vec::new(),
            Instr::Copy { src, .. } => regs(&[*src]),
            Instr::Bin { lhs, rhs, .. } | Instr::Cmp { lhs, rhs, .. } => regs(&[*lhs, *rhs]),
            Instr::Print { val, .. } => regs(&[*val]),
        }
    }

    /// The virtual register written by this instruction, if any.
    pub fn def(&self) -> Option<VReg> {
        match self {
            Instr::Const { dst, .. }
            | Instr::StrAddr { dst, .. }
            | Instr::Copy { dst, .. }
            | Instr::Bin { dst, .. }
            | Instr::Cmp { dst, .. } => Some(*dst),
            Instr::Print { .. } => None,
        }
    }

    /// Whether this instruction performs a call, and therefore destroys the
    /// caller-saved registers.
    pub fn is_call(&self) -> bool {
        matches!(self, Instr::Print { .. })
    }
}

pub struct Program {
    /// Basic blocks in the order they will be emitted; block 0 is the entry.
    pub blocks: Vec<Block>,
    /// Interned string literals, each stored without its NUL terminator.
    pub strings: Vec<Vec<u8>>,
    /// Human-readable name per virtual register, used by IR and allocator dumps.
    pub vreg_names: Vec<String>,
}

impl Program {
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

    /// Render the IR for `--emit ir`.
    pub fn dump(&self) -> String {
        let mut out = String::new();
        for (i, s) in self.strings.iter().enumerate() {
            out.push_str(&format!("str{i} = {:?}\n", String::from_utf8_lossy(s)));
        }
        if !self.strings.is_empty() {
            out.push('\n');
        }

        let mut index = 0;
        for block in &self.blocks {
            out.push_str(&format!("{}:\n", block.label));
            for instr in &block.instrs {
                let text = match instr {
                    Instr::Const { dst, val } => format!("%{} = const {val}", self.name_of(*dst)),
                    Instr::StrAddr { dst, id } => {
                        format!("%{} = straddr str{}", self.name_of(*dst), id.0)
                    }
                    Instr::Copy { dst, src } => {
                        format!("%{} = copy {}", self.name_of(*dst), self.value_name(src))
                    }
                    Instr::Bin { op, dst, lhs, rhs } => format!(
                        "%{} = {} {}, {}",
                        self.name_of(*dst),
                        op_name(*op),
                        self.value_name(lhs),
                        self.value_name(rhs)
                    ),
                    Instr::Cmp { op, dst, lhs, rhs } => format!(
                        "%{} = cmp {} {}, {}",
                        self.name_of(*dst),
                        op.symbol(),
                        self.value_name(lhs),
                        self.value_name(rhs)
                    ),
                    Instr::Print { ty, val } => {
                        format!("print {} {}", ty.name(), self.value_name(val))
                    }
                };
                out.push_str(&format!("{index:>3}  {text}\n"));
                index += 1;
            }

            let text = match &block.term {
                Terminator::Jump(target) => format!("jump {}", self.block(*target).label),
                Terminator::Branch { cond, then_blk, else_blk } => format!(
                    "branch {} ? {} : {}",
                    self.value_name(cond),
                    self.block(*then_blk).label,
                    self.block(*else_blk).label
                ),
                Terminator::Return => "return".to_string(),
            };
            out.push_str(&format!("{index:>3}  {text}\n"));
            index += 1;
        }
        out
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
    let mut lowering = Lowering {
        blocks: Vec::new(),
        strings: Vec::new(),
        vreg_names: Vec::new(),
        current: BlockId(0),
        scopes: vec![HashMap::new()],
        name_counts: HashMap::new(),
        types,
    };

    lowering.new_block("entry");
    for stmt in &ast.stmts {
        lowering.stmt(stmt);
    }
    lowering.terminate(Terminator::Return);

    Program {
        blocks: lowering.blocks,
        strings: lowering.strings,
        vreg_names: lowering.vreg_names,
    }
}

struct Lowering<'a> {
    blocks: Vec<Block>,
    strings: Vec<Vec<u8>>,
    vreg_names: Vec<String>,
    /// The block instructions are currently appended to.
    current: BlockId,
    /// Variable name -> virtual register, one map per open scope.
    scopes: Vec<HashMap<String, VReg>>,
    /// How many registers have borne each name, so a shadowing declaration in
    /// another scope gets a distinct dump name (`i`, then `i.1`).
    name_counts: HashMap<String, u32>,
    types: &'a Types,
}

impl<'a> Lowering<'a> {
    // -- blocks ------------------------------------------------------------

    /// Append a new block and return its id. It starts with a placeholder
    /// terminator that [`Self::terminate`] replaces.
    fn new_block(&mut self, label: &str) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        let label = format!("{label}{}", id.0);
        self.blocks.push(Block { label, instrs: Vec::new(), term: Terminator::Return });
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
        }
    }

    fn if_stmt(&mut self, cond: &Expr, then_block: &AstBlock, else_block: &Option<AstBlock>) {
        let cond = self.expr(cond);

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

        // The branch belongs to the block that computed the condition, which is
        // whatever block preceded `then`.
        let entry = BlockId(then_id.0 - 1);
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
            _ => {
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

    #[test]
    fn lowers_the_sample_program() {
        let ir = lower_src("int x = 10;\nint y = 20;\nstring s = \"hi\";\nprint(x + y);\nprint(s);");
        assert_eq!(
            ir.dump(),
            concat!(
                "str0 = \"hi\"\n",
                "\n",
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
        let ir = lower_src("int n = 1;\nn = n + 41;\nprint(n);");
        assert_eq!(
            ir.dump(),
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
        let ir = lower_src("int n = 0;\nif (n < 1) {\n  n = 2;\n} else {\n  n = 3;\n}\nprint(n);");
        let labels: Vec<&str> = ir.blocks.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(labels, vec!["entry0", "then1", "else2", "join3"]);
        assert!(matches!(ir.blocks[0].term, Terminator::Branch { .. }));
        assert!(matches!(ir.blocks[1].term, Terminator::Jump(BlockId(3))));
        assert!(matches!(ir.blocks[2].term, Terminator::Jump(BlockId(3))));
    }

    #[test]
    fn an_if_without_else_branches_straight_to_the_join() {
        let ir = lower_src("int n = 0;\nif (n < 1) {\n  n = 2;\n}\nprint(n);");
        let labels: Vec<&str> = ir.blocks.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(labels, vec!["entry0", "then1", "join2"]);
        match ir.blocks[0].term {
            Terminator::Branch { then_blk, else_blk, .. } => {
                assert_eq!((then_blk, else_blk), (BlockId(1), BlockId(2)));
            }
            ref other => panic!("expected a branch, got {other:?}"),
        }
    }

    #[test]
    fn a_while_loop_closes_a_back_edge() {
        let ir = lower_src("int i = 0;\nwhile (i < 3) {\n  i = i + 1;\n}\nprint(i);");
        let labels: Vec<&str> = ir.blocks.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(labels, vec!["entry0", "loop1", "body2", "done3"]);
        // The body jumps back to the header, which re-tests the condition.
        assert!(matches!(ir.blocks[2].term, Terminator::Jump(BlockId(1))));
        assert!(matches!(ir.blocks[0].term, Terminator::Jump(BlockId(1))));
    }

    #[test]
    fn a_for_loop_desugars_into_the_same_shape() {
        let with_for = lower_src("for (int i = 0; i < 3; i = i + 1) {\n  print(i);\n}");
        let with_while =
            lower_src("int i = 0;\nwhile (i < 3) {\n  print(i);\n  i = i + 1;\n}");
        assert_eq!(with_for.dump(), with_while.dump());
    }

    #[test]
    fn literal_operands_stay_immediates() {
        let ir = lower_src("print(1 + 2);");
        assert_eq!(ir.blocks[0].instrs.len(), 2);
        assert!(matches!(
            ir.blocks[0].instrs[0],
            Instr::Bin { lhs: Value::Const(1), rhs: Value::Const(2), .. }
        ));
    }

    #[test]
    fn bools_lower_to_integer_constants() {
        let ir = lower_src("bool ready = true;\nbool done = false;\nprint(ready);\nprint(done);");
        assert_eq!(
            ir.dump(),
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
        let ir = lower_src("print(false);");
        assert_eq!(ir.blocks[0].instrs.len(), 1);
        assert!(matches!(
            ir.blocks[0].instrs[0],
            Instr::Print { ty: Ty::Bool, val: Value::Const(0) }
        ));
    }

    #[test]
    fn shadowed_variables_get_distinct_registers() {
        let ir = lower_src("int i = 1;\nif (true) {\n  int i = 2;\n  print(i);\n}\nprint(i);");
        assert!(ir.vreg_names.contains(&"i".to_string()));
        assert!(ir.vreg_names.contains(&"i.1".to_string()));
    }

    #[test]
    fn identical_strings_are_interned_once() {
        let ir = lower_src("string a = \"hi\";\nstring b = \"hi\";\nprint(a);\nprint(b);");
        assert_eq!(ir.strings.len(), 1);
    }
}
