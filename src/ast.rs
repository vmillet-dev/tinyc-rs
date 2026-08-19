//! The abstract syntax tree produced by the parser.

use crate::diag::Span;

/// The types of TinyC v0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ty {
    /// 64-bit signed integer.
    Int,
    /// Pointer to NUL-terminated static bytes.
    Str,
    ///
    Bool
}

impl Ty {
    pub fn name(self) -> &'static str {
        match self {
            Ty::Int => "int",
            Ty::Str => "string",
            Ty::Bool => "bool"
        }
    }

    /// The quoted type name with its indefinite article, for prose in messages.
    pub fn with_article(self) -> &'static str {
        match self {
            Ty::Int => "an `int`",
            Ty::Str => "a `string`",
            Ty::Bool => "a `boolean`"
        }
    }
}

/// Index of an expression node, used by [`crate::sema`] to record its type
/// without mutating the tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
        }
    }

    /// Whether the operands may be exchanged, which lets a backend pick
    /// whichever order needs fewer moves.
    pub fn commutes(self) -> bool {
        matches!(self, BinOp::Add | BinOp::Mul)
    }
}

#[derive(Clone, Debug)]
pub struct Expr {
    pub id: NodeId,
    /// Covers the whole subexpression, so diagnostics can underline it.
    pub span: Span,
    pub kind: ExprKind,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Int(i64),
    Str(Vec<u8>),
    Bool(bool),
    Var(String),
    /// Unary minus; the only unary operator in v0.
    Neg(Box<Expr>),
    Bin { op: BinOp, lhs: Box<Expr>, rhs: Box<Expr> },
}

#[derive(Clone, Debug)]
pub enum Stmt {
    Decl {
        ty: Ty,
        ty_span: Span,
        name: String,
        name_span: Span,
        init: Expr,
    },
    /// Assignment to an already declared variable.
    Assign {
        name: String,
        name_span: Span,
        value: Expr,
    },
    Print {
        /// Span of the `print` keyword, for diagnostics about the statement.
        span: Span,
        value: Expr,
    },
}

#[derive(Clone, Debug)]
pub struct Program {
    pub stmts: Vec<Stmt>,
    /// Number of [`NodeId`]s handed out, i.e. the size of the type table.
    pub node_count: usize,
}

/// Render the tree for `--emit ast`.
pub fn dump(program: &Program) -> String {
    let mut out = String::new();
    for stmt in &program.stmts {
        match stmt {
            Stmt::Decl { ty, name, init, .. } => {
                out.push_str(&format!("decl {} {name}\n", ty.name()));
                dump_expr(&mut out, init, 1);
            }
            Stmt::Assign { name, value, .. } => {
                out.push_str(&format!("assign {name}\n"));
                dump_expr(&mut out, value, 1);
            }
            Stmt::Print { value, .. } => {
                out.push_str("print\n");
                dump_expr(&mut out, value, 1);
            }
        }
    }
    out
}

fn dump_expr(out: &mut String, expr: &Expr, depth: usize) {
    let pad = "  ".repeat(depth);
    match &expr.kind {
        ExprKind::Int(v) => out.push_str(&format!("{pad}int {v}\n")),
        ExprKind::Str(bytes) => {
            out.push_str(&format!("{pad}string {:?}\n", String::from_utf8_lossy(bytes)))
        }
        ExprKind::Bool(v) => out.push_str(&format!("{pad}bool {v}\n")),
        ExprKind::Var(name) => out.push_str(&format!("{pad}var {name}\n")),
        ExprKind::Neg(operand) => {
            out.push_str(&format!("{pad}neg\n"));
            dump_expr(out, operand, depth + 1);
        }
        ExprKind::Bin { op, lhs, rhs } => {
            out.push_str(&format!("{pad}{}\n", op.symbol()));
            dump_expr(out, lhs, depth + 1);
            dump_expr(out, rhs, depth + 1);
        }
    }
}
