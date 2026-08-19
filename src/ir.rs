//! Stage 4: AST -> intermediate representation.
//!
//! The IR is straight-line three-address code over an unbounded supply of
//! *virtual* registers. There is no control flow in v0, so the whole program is
//! a single instruction vector and live ranges are plain intervals — which is
//! exactly what [`crate::codegen::regalloc`] needs.
//!
//! Every variable and every intermediate result gets its own virtual register,
//! and (because v0 has no reassignment) each one is written exactly once.

use std::collections::HashMap;

use crate::ast::{BinOp, Expr, ExprKind, Program as Ast, Stmt, Ty};
use crate::sema::Types;

/// A virtual register: an SSA-like value name, not yet a machine register.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VReg(pub u32);

/// Index into [`Program::strings`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StrId(pub u32);

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
    /// `dst = lhs op rhs`
    Bin { op: BinOp, dst: VReg, lhs: Value, rhs: Value },
    /// The only call site in v0; clobbers the caller-saved registers.
    Print { ty: Ty, val: Value },
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
            Instr::Bin { lhs, rhs, .. } => regs(&[*lhs, *rhs]),
            Instr::Print { val, .. } => regs(&[*val]),
        }
    }

    /// The virtual register written by this instruction, if any.
    pub fn def(&self) -> Option<VReg> {
        match self {
            Instr::Const { dst, .. } | Instr::StrAddr { dst, .. } | Instr::Bin { dst, .. } => {
                Some(*dst)
            }
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
    pub instrs: Vec<Instr>,
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

    /// Render the IR for `--emit ir`.
    pub fn dump(&self) -> String {
        let mut out = String::new();
        for (i, s) in self.strings.iter().enumerate() {
            out.push_str(&format!("str{i} = {:?}\n", String::from_utf8_lossy(s)));
        }
        if !self.strings.is_empty() {
            out.push('\n');
        }
        for (i, instr) in self.instrs.iter().enumerate() {
            let value = |v: &Value| match v {
                Value::Const(c) => c.to_string(),
                Value::Reg(r) => format!("%{}", self.name_of(*r)),
            };
            let text = match instr {
                Instr::Const { dst, val } => {
                    format!("%{} = const {val}", self.name_of(*dst))
                }
                Instr::StrAddr { dst, id } => {
                    format!("%{} = straddr str{}", self.name_of(*dst), id.0)
                }
                Instr::Bin { op, dst, lhs, rhs } => format!(
                    "%{} = {} {}, {}",
                    self.name_of(*dst),
                    op_name(*op),
                    value(lhs),
                    value(rhs)
                ),
                Instr::Print { ty, val } => format!("print {} {}", ty.name(), value(val)),
            };
            out.push_str(&format!("{i:>3}  {text}\n"));
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
        program: Program { instrs: Vec::new(), strings: Vec::new(), vreg_names: Vec::new() },
        vars: HashMap::new(),
        versions: HashMap::new(),
    };

    for stmt in &ast.stmts {
        match stmt {
            Stmt::Decl { name, init, .. } => {
                let reg = lowering.bind(name, init);
                lowering.vars.insert(name.clone(), reg);
            }
            // Assignment is renaming: the variable starts pointing at a new
            // virtual register, so every register still has exactly one
            // definition and live intervals stay exact. With no control flow
            // this is all the SSA construction the IR needs.
            Stmt::Assign { name, value, .. } => {
                let reg = lowering.bind(name, value);
                lowering.vars.insert(name.clone(), reg);
            }
            Stmt::Print { value, .. } => {
                let ty = types.of(value.id);
                let val = lowering.expr(value);
                lowering.program.instrs.push(Instr::Print { ty, val });
            }
        }
    }

    lowering.program
}

struct Lowering {
    program: Program,
    /// Variable name -> the virtual register currently holding its value.
    vars: HashMap<String, VReg>,
    /// How many times each variable has been written, so successive values can
    /// be told apart in dumps: `%s`, then `%s.1`, then `%s.2`.
    versions: HashMap<String, u32>,
}

impl Lowering {
    fn fresh(&mut self, name: &str) -> VReg {
        let reg = VReg(self.program.vreg_names.len() as u32);
        self.program.vreg_names.push(name.to_string());
        reg
    }

    /// Lower `value` and return the virtual register that becomes `name`'s new
    /// home.
    fn bind(&mut self, name: &str, value: &Expr) -> VReg {
        let version = self.versions.entry(name.to_string()).or_insert(0);
        let label = if *version == 0 { name.to_string() } else { format!("{name}.{version}") };
        *version += 1;

        match self.expr(value) {
            // The expression already produced a register: adopt it as the
            // variable's home and relabel it, so dumps read `%s` rather than
            // `%t2`. A register that is some *other* variable's home is left
            // alone — `int b = a;` simply shares `a`'s value.
            Value::Reg(reg) => {
                if !self.vars.values().any(|&v| v == reg) {
                    self.program.vreg_names[reg.0 as usize] = label;
                }
                reg
            }
            Value::Const(val) => {
                let dst = self.fresh(&label);
                self.program.instrs.push(Instr::Const { dst, val });
                dst
            }
        }
    }

    /// Temporaries are named after their own index, so `%t3` is always virtual
    /// register 3 no matter how many names variables have taken.
    fn fresh_temp(&mut self) -> VReg {
        let name = format!("t{}", self.program.vreg_names.len());
        self.fresh(&name)
    }

    fn expr(&mut self, expr: &Expr) -> Value {
        match &expr.kind {
            // Literals stay immediates so the backend can fold them into the
            // instruction that consumes them.
            ExprKind::Int(v) => Value::Const(*v),
            ExprKind::Str(bytes) => {
                let id = self.intern(bytes);
                let dst = self.fresh_temp();
                self.program.instrs.push(Instr::StrAddr { dst, id });
                Value::Reg(dst)
            }
            ExprKind::Bool(v) => Value::Const(i64::from(*v)),
            ExprKind::Var(name) => Value::Reg(self.vars[name]),
            // `-x` lowers to `0 - x`, which needs no separate instruction kind.
            ExprKind::Neg(operand) => {
                let val = self.expr(operand);
                if let Value::Const(c) = val {
                    return Value::Const(c.wrapping_neg());
                }
                let dst = self.fresh_temp();
                self.program.instrs.push(Instr::Bin {
                    op: BinOp::Sub,
                    dst,
                    lhs: Value::Const(0),
                    rhs: val,
                });
                Value::Reg(dst)
            }
            ExprKind::Bin { op, lhs, rhs } => {
                let lhs = self.expr(lhs);
                let rhs = self.expr(rhs);
                let dst = self.fresh_temp();
                self.program.instrs.push(Instr::Bin { op: *op, dst, lhs, rhs });
                Value::Reg(dst)
            }
        }
    }

    fn intern(&mut self, bytes: &[u8]) -> StrId {
        if let Some(i) = self.program.strings.iter().position(|s| s == bytes) {
            return StrId(i as u32);
        }
        self.program.strings.push(bytes.to_vec());
        StrId(self.program.strings.len() as u32 - 1)
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
                "  0  %x = const 10\n",
                "  1  %y = const 20\n",
                "  2  %s = straddr str0\n",
                "  3  %t3 = add %x, %y\n",
                "  4  print int %t3\n",
                "  5  print string %s\n",
            )
        );
    }

    #[test]
    fn literal_operands_stay_immediates() {
        let ir = lower_src("print(1 + 2);");
        assert_eq!(ir.instrs.len(), 2);
        assert!(matches!(
            ir.instrs[0],
            Instr::Bin { lhs: Value::Const(1), rhs: Value::Const(2), .. }
        ));
    }

    #[test]
    fn assignment_gives_the_variable_a_new_register() {
        let ir = lower_src("int n = 1;\nprint(n);\nn = n + 10;\nprint(n);");
        assert_eq!(
            ir.dump(),
            concat!(
                "  0  %n = const 1\n",
                "  1  print int %n\n",
                "  2  %n.1 = add %n, 10\n",
                "  3  print int %n.1\n",
            )
        );

        // Every virtual register still has exactly one definition, which is what
        // keeps the live intervals exact.
        let mut defined = std::collections::HashSet::new();
        for instr in &ir.instrs {
            if let Some(dst) = instr.def() {
                assert!(defined.insert(dst), "{dst:?} is defined twice");
            }
        }
    }

    #[test]
    fn assignment_does_not_disturb_a_variable_copied_from_it() {
        // `b` took `a`'s value, so reassigning `a` must leave `b` alone.
        let ir = lower_src("int a = 1;\nint b = a;\na = 2;\nprint(a);\nprint(b);");
        assert_eq!(
            ir.dump(),
            concat!(
                "  0  %a = const 1\n",
                "  1  %a.1 = const 2\n",
                "  2  print int %a.1\n",
                "  3  print int %a\n",
            )
        );
    }

    #[test]
    fn bools_lower_to_integer_constants() {
        // A bool is just a 0 or a 1, so it needs no instruction of its own and
        // the register allocator never has to know the type exists.
        let ir = lower_src("bool ready = true;\nbool done = false;\nprint(ready);\nprint(done);");
        assert_eq!(
            ir.dump(),
            concat!(
                "  0  %ready = const 1\n",
                "  1  %done = const 0\n",
                "  2  print bool %ready\n",
                "  3  print bool %done\n",
            )
        );
    }

    #[test]
    fn a_printed_bool_literal_stays_an_immediate() {
        let ir = lower_src("print(false);");
        assert_eq!(ir.instrs.len(), 1);
        assert!(matches!(ir.instrs[0], Instr::Print { ty: Ty::Bool, val: Value::Const(0) }));
    }

    #[test]
    fn identical_strings_are_interned_once() {
        let ir = lower_src("string a = \"hi\";\nstring b = \"hi\";\nprint(a);\nprint(b);");
        assert_eq!(ir.strings.len(), 1);
    }
}
