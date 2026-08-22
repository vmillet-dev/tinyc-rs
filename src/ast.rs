//! The abstract syntax tree produced by the parser.

use crate::diag::Span;

/// Index of an enum in [`Program::enums`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EnumId(pub u32);

/// What every stage after the parser needs to know about a declared enum: what
/// it is called, and what its variants are called *in order*.
///
/// The order is the whole representation — a variant's tag is its position — so
/// this is also the table the runtime prints a value's name from.
#[derive(Clone, Debug)]
pub struct EnumInfo {
    pub name: String,
    pub variants: Vec<String>,
}

impl EnumInfo {
    /// The tag a variant is represented by, which is where it was written.
    pub fn tag(&self, variant: &str) -> Option<i64> {
        self.variants.iter().position(|v| v == variant).map(|at| at as i64)
    }
}

/// The types of TinyC.
///
/// Every variant is a type a *value* can have, which is why there is no `Void`:
/// a function that returns nothing has no return type at all. See [`FnDecl::ret`].
///
/// [`Ty::Enum`] holds an *index* rather than a name, which is what keeps `Ty`
/// `Copy` and its equality an integer comparison. The price is that a `Ty`
/// cannot name itself — see [`Ty::name`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ty {
    /// 64-bit signed integer.
    Int,
    /// Pointer to NUL-terminated static bytes.
    Str,
    /// `true` or `false`.
    Bool,
    /// One of the variants of a declared enum, held as its index.
    Enum(EnumId),
}

impl Ty {
    /// This type's name, given the enum names of the program it belongs to,
    /// indexed by [`EnumId`].
    ///
    /// The table has to be handed in because an enum's name comes from the
    /// source rather than from this enum, and [`Ty`] is deliberately too small
    /// to carry one.
    pub fn name(self, enums: &[EnumInfo]) -> &str {
        match self {
            Ty::Int => "int",
            Ty::Str => "string",
            Ty::Bool => "bool",
            Ty::Enum(id) => &enums[id.0 as usize].name,
        }
    }

    /// The quoted type name with its indefinite article, for prose in messages.
    pub fn with_article(self, enums: &[EnumInfo]) -> String {
        match self {
            Ty::Int => "an `int`".to_string(),
            Ty::Str => "a `string`".to_string(),
            Ty::Bool => "a `bool`".to_string(),
            Ty::Enum(id) => format!("a `{}`", enums[id.0 as usize].name),
        }
    }

    /// Whether values of this type can be ordered, and so compared with `<` and
    /// its relatives rather than only with `==`.
    ///
    /// An enum's variants have an order in the declaration, but it is not one
    /// the program said anything about, so TinyC declines to invent it.
    pub fn is_ordered(self) -> bool {
        matches!(self, Ty::Int)
    }

    /// Whether two values of this type can be compared for equality at all.
    /// Strings cannot: it would need a runtime routine, and comparing the
    /// pointers would quietly answer a different question.
    pub fn has_equality(self) -> bool {
        !matches!(self, Ty::Str)
    }
}

/// A type as it was written, before anything knows what it names.
///
/// The parser cannot produce a [`Ty`]: `Color c = ...` and `int c = ...` are the
/// same shape, and only [`crate::sema`] knows whether `Color` was declared. So
/// syntax carries the name and the span, and resolution happens once, in the
/// stage that has the table.
#[derive(Clone, Debug)]
pub struct TypeRef {
    pub name: String,
    pub span: Span,
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
    /// Remainder. Paired with [`BinOp::Div`] everywhere, because on x86 they are
    /// the same instruction read from two different registers.
    Rem,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
        }
    }

    /// Whether this operator divides, and so has a zero divisor to worry about.
    pub fn divides(self) -> bool {
        matches!(self, BinOp::Div | BinOp::Rem)
    }

    /// The English name of the operation, for a diagnostic that talks about it.
    pub fn noun(self) -> &'static str {
        match self {
            BinOp::Add => "addition",
            BinOp::Sub => "subtraction",
            BinOp::Mul => "multiplication",
            BinOp::Div => "division",
            BinOp::Rem => "remainder",
        }
    }

    /// This operator's answer for two known operands, or `None` when the
    /// machine would refuse to produce one.
    ///
    /// **The one place TinyC's arithmetic is defined.** [`crate::sema`] rejects
    /// with it and [`crate::ir`] folds with it, so the two cannot come to
    /// different conclusions about what `MAX + 1` means — which they would
    /// eventually, kept apart.
    ///
    /// `Rem` refuses `MIN % -1` even though the answer is 0, because the machine
    /// reaches that 0 through the `idiv` whose *quotient* does not fit.
    pub fn apply(self, a: i64, b: i64) -> Option<i64> {
        match self {
            BinOp::Add => a.checked_add(b),
            BinOp::Sub => a.checked_sub(b),
            BinOp::Mul => a.checked_mul(b),
            BinOp::Div => a.checked_div(b),
            BinOp::Rem => a.checked_rem(b),
        }
    }

    /// The answer an integer of unlimited width would give.
    ///
    /// Only a diagnostic wants this: it is how "`9223372036854775808` does not
    /// fit in an `int`" gets to name the value that did not fit. `None` is a
    /// division by zero, which has no answer at any width.
    pub fn apply_exact(self, a: i64, b: i64) -> Option<i128> {
        // Two `i64`s cannot overflow an `i128` under any of these, so the plain
        // operators are safe here in a way they are not above.
        let (a, b) = (i128::from(a), i128::from(b));
        match self {
            BinOp::Add => Some(a + b),
            BinOp::Sub => Some(a - b),
            BinOp::Mul => Some(a * b),
            BinOp::Div => a.checked_div(b),
            BinOp::Rem => a.checked_rem(b),
        }
    }

    /// Whether the operands may be exchanged, which lets a backend pick
    /// whichever order needs fewer moves.
    pub fn commutes(self) -> bool {
        matches!(self, BinOp::Add | BinOp::Mul)
    }
}

/// Comparison operators. These take two values of the same type and produce a
/// `bool`, which is what `if` and the loops consume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    pub fn symbol(self) -> &'static str {
        match self {
            CmpOp::Eq => "==",
            CmpOp::Ne => "!=",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
    }

    /// Ordering comparisons need operands that can be ordered; `==` and `!=`
    /// only need equality.
    pub fn is_ordering(self) -> bool {
        !matches!(self, CmpOp::Eq | CmpOp::Ne)
    }

    /// The comparison that is true exactly when this one is false.
    ///
    /// Every comparison has one, which is what lets `!(a < b)` be lowered as
    /// `a >= b` — one instruction where negating the *result* would be two.
    pub fn negate(self) -> CmpOp {
        match self {
            CmpOp::Eq => CmpOp::Ne,
            CmpOp::Ne => CmpOp::Eq,
            CmpOp::Lt => CmpOp::Ge,
            CmpOp::Le => CmpOp::Gt,
            CmpOp::Gt => CmpOp::Le,
            CmpOp::Ge => CmpOp::Lt,
        }
    }
}

/// The short-circuiting operators.
///
/// Deliberately not a [`BinOp`]: `&&` and `||` do not evaluate both operands,
/// so they are not operations on two values at all. [`crate::ir`] lowers them
/// to a branch rather than to an instruction, which is why no `Instr` matches
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogicOp {
    And,
    Or,
}

impl LogicOp {
    pub fn symbol(self) -> &'static str {
        match self {
            LogicOp::And => "&&",
            LogicOp::Or => "||",
        }
    }

    /// The answer the left operand gives when it settles the whole expression
    /// on its own, as the 0 or 1 a `bool` is.
    ///
    /// It doubles as the *condition* for stopping there, because the two happen
    /// to coincide: `false && x` is false, and `true || x` is true.
    pub fn short_circuit(self) -> i64 {
        match self {
            LogicOp::And => 0,
            LogicOp::Or => 1,
        }
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
    /// Unary minus, on an `int`.
    Neg(Box<Expr>),
    /// Logical negation, on a `bool`.
    Not(Box<Expr>),
    Bin { op: BinOp, lhs: Box<Expr>, rhs: Box<Expr> },
    Cmp { op: CmpOp, lhs: Box<Expr>, rhs: Box<Expr> },
    /// `lhs && rhs` or `lhs || rhs`. `rhs` is evaluated only when `lhs` did not
    /// already decide the answer, which is why this is not a [`Self::Bin`].
    Logic { op: LogicOp, lhs: Box<Expr>, rhs: Box<Expr> },
    /// A variant of an enum, `Color::Red`.
    ///
    /// Always written qualified, so that two enums may use the same variant
    /// name and so that a variant never has to be told apart from a variable.
    /// Both halves keep a span: the enum name is what "no enum called `Color`"
    /// underlines, the variant what "`Color` has no variant `Purple`" does.
    Variant { enum_name: String, enum_span: Span, variant: String, variant_span: Span },
    /// `match (value) { Colour::Red => "warm", ... }`.
    ///
    /// A match is an expression, and a statement only in the way a call is one:
    /// [`Stmt::Match`] wraps this node when it is written for its effect rather
    /// than for its value.
    ///
    /// Every variant of the scrutinee's enum must have an arm, and no variant
    /// may have two. There is deliberately no catch-all pattern: the whole
    /// value of the check is that adding a variant makes every `match` that
    /// does not handle it stop compiling, and a `_` would silently swallow it.
    Match {
        /// Span of the `match` keyword, which is what "this match does not
        /// cover ..." underlines — the mistake is the arms taken together, not
        /// any one of them.
        keyword: Span,
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
    },
    /// A call, `add(1, 2)`. The enclosing [`Expr::span`] covers the whole call;
    /// `name_span` covers just the callee, which is what "unknown function"
    /// underlines. Each argument keeps its own span, so an argument of the
    /// wrong type is reported on the argument rather than on the call.
    Call { name: String, name_span: Span, args: Vec<Expr> },
}

/// A braced sequence of statements. Declarations inside a block go out of scope
/// at its closing brace.
#[derive(Clone, Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Clone, Debug)]
pub enum Stmt {
    Decl {
        ty: TypeRef,
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
    If {
        cond: Expr,
        then_block: Block,
        else_block: Option<Block>,
    },
    While {
        cond: Expr,
        body: Block,
    },
    /// `for (init; cond; step) body`. Lowering desugars it into a `while`.
    For {
        init: Box<Stmt>,
        cond: Expr,
        step: Box<Stmt>,
        body: Block,
    },
    /// `return;` or `return expr;`.
    ///
    /// The parser accepts both spellings anywhere; whether this one agrees with
    /// the enclosing function's [`FnDecl::ret`] is a question for
    /// [`crate::sema`], which is the first stage that knows which function this
    /// statement is in.
    Return {
        /// Span of the `return` keyword, so "this function returns nothing" can
        /// underline the statement even when it carries no value.
        span: Span,
        value: Option<Expr>,
    },
    /// A `match` evaluated for its effect rather than its value.
    ///
    /// The second expression statement in the language, and it exists for the
    /// same reason the first does: [`Stmt::Call`] is what lets a function
    /// returning nothing be called at all, and this is what lets a match whose
    /// arms only *do* things be written at all. The parser only ever puts an
    /// [`ExprKind::Match`] here.
    Match(Expr),
    /// `break;` — leave the innermost enclosing loop.
    ///
    /// The parser accepts it anywhere, exactly as it does `return`; whether
    /// there *is* a loop to leave is a question for [`crate::sema`], which is
    /// the first stage that tracks how deeply the statement is nested.
    Break { span: Span },
    /// `continue;` — start the innermost enclosing loop's next iteration.
    Continue { span: Span },
    /// A call evaluated for its effect, `greet("hi");`.
    ///
    /// TinyC has no general expression statements — this is one of two, the
    /// other being [`Stmt::Match`], and it exists because a function returning
    /// nothing could not be called otherwise. The parser only ever puts an
    /// [`ExprKind::Call`] here.
    Call(Expr),
}

/// What an arm does once its pattern matches.
///
/// The two spellings answer two different needs, and the token after `=>`
/// settles which is meant: a `{` opens a block, anything else begins an
/// expression.
#[derive(Clone, Debug)]
pub enum ArmBody {
    /// `Colour::Red => "warm"` — the arm's value, and the match's.
    Value(Expr),
    /// `Colour::Red => { print("hi"); return "warm"; }` — statements run for
    /// their effect.
    ///
    /// A block never *produces* a value: `return` keeps its one meaning of
    /// leaving the function, rather than gaining a second one inside a match.
    /// So where a match is used as an expression, a block arm has to be one
    /// that never reaches the end — see `sema`'s divergence check.
    Block(Block),
}

/// One arm of a `match`: a qualified variant and what it does.
///
/// The pattern is spelled exactly as the expression would be, `Color::Red`, and
/// carries the same two spans for the same two diagnostics.
#[derive(Clone, Debug)]
pub struct MatchArm {
    pub enum_name: String,
    pub enum_span: Span,
    pub variant: String,
    pub variant_span: Span,
    pub body: ArmBody,
}

/// One variant of an enum declaration.
#[derive(Clone, Debug)]
pub struct Variant {
    pub name: String,
    pub name_span: Span,
}

/// `enum Color { Red, Green, Blue }`.
///
/// A variant carries no payload, so a value of the type is its variant's index
/// and nothing more — which is why enums cost the backend nothing but a table
/// of names to print.
#[derive(Clone, Debug)]
pub struct EnumDecl {
    pub name: String,
    pub name_span: Span,
    pub variants: Vec<Variant>,
}

/// One parameter in a signature: `int a`.
///
/// The spans mirror [`Stmt::Decl`], because a parameter *is* a declaration —
/// the type is underlined when an argument does not match it, the name when two
/// parameters collide.
#[derive(Clone, Debug)]
pub struct Param {
    pub ty: TypeRef,
    pub name: String,
    pub name_span: Span,
}

/// A function declaration: `fn add(int a, int b) -> int { ... }`.
///
/// This is the only thing that may appear at the top level of a program.
#[derive(Clone, Debug)]
pub struct FnDecl {
    pub name: String,
    pub name_span: Span,
    pub params: Vec<Param>,
    /// The declared return type, or `None` for a function that returns nothing.
    ///
    /// Void is the *absence* of a type rather than a `Ty` variant, so every
    /// `match` on [`Ty`] elsewhere in the compiler stays about types that
    /// values really have.
    pub ret: Option<TypeRef>,
    /// Where a diagnostic about the return type points: the type itself in
    /// `-> int`, or the closing `)` when the signature declares none. Always
    /// present, so "this function returns nothing" has something to underline.
    pub ret_span: Span,
    pub body: Block,
}

#[derive(Clone, Debug)]
pub struct Program {
    /// Declared enums, in source order; an [`EnumId`] indexes this.
    pub enums: Vec<EnumDecl>,
    pub functions: Vec<FnDecl>,
    /// Number of [`NodeId`]s handed out, i.e. the size of the type table.
    ///
    /// Ids are unique across the whole file rather than per function, so
    /// [`crate::sema`] keeps one flat table for every expression in the program.
    pub node_count: usize,
}

/// Render the tree for `--emit ast`.
pub fn dump(program: &Program) -> String {
    let mut out = String::new();
    for declaration in &program.enums {
        let variants: Vec<&str> = declaration.variants.iter().map(|v| v.name.as_str()).collect();
        out.push_str(&format!("enum {} {{ {} }}\n", declaration.name, variants.join(", ")));
    }
    for function in &program.functions {
        dump_fn(&mut out, function);
    }
    out
}

fn dump_fn(out: &mut String, function: &FnDecl) {
    let params: Vec<String> =
        function.params.iter().map(|p| format!("{} {}", p.ty.name, p.name)).collect();
    let ret = match &function.ret {
        Some(ty) => format!(" -> {}", ty.name),
        None => String::new(),
    };
    out.push_str(&format!("fn {}({}){}\n", function.name, params.join(", "), ret));
    dump_block(out, &function.body, 1);
}

fn dump_stmt(out: &mut String, stmt: &Stmt, depth: usize) {
    let pad = "  ".repeat(depth);
    match stmt {
        Stmt::Decl { ty, name, init, .. } => {
            out.push_str(&format!("{pad}decl {} {name}\n", ty.name));
            dump_expr(out, init, depth + 1);
        }
        Stmt::Assign { name, value, .. } => {
            out.push_str(&format!("{pad}assign {name}\n"));
            dump_expr(out, value, depth + 1);
        }
        Stmt::Print { value, .. } => {
            out.push_str(&format!("{pad}print\n"));
            dump_expr(out, value, depth + 1);
        }
        Stmt::If { cond, then_block, else_block } => {
            out.push_str(&format!("{pad}if\n"));
            dump_expr(out, cond, depth + 1);
            out.push_str(&format!("{pad}then\n"));
            dump_block(out, then_block, depth + 1);
            if let Some(block) = else_block {
                out.push_str(&format!("{pad}else\n"));
                dump_block(out, block, depth + 1);
            }
        }
        Stmt::While { cond, body } => {
            out.push_str(&format!("{pad}while\n"));
            dump_expr(out, cond, depth + 1);
            dump_block(out, body, depth + 1);
        }
        Stmt::For { init, cond, step, body } => {
            out.push_str(&format!("{pad}for\n"));
            dump_stmt(out, init, depth + 1);
            dump_expr(out, cond, depth + 1);
            dump_stmt(out, step, depth + 1);
            dump_block(out, body, depth + 1);
        }
        Stmt::Return { value, .. } => {
            out.push_str(&format!("{pad}return\n"));
            if let Some(expr) = value {
                dump_expr(out, expr, depth + 1);
            }
        }
        Stmt::Match(expr) => dump_expr(out, expr, depth),
        Stmt::Break { .. } => out.push_str(&format!("{pad}break\n")),
        Stmt::Continue { .. } => out.push_str(&format!("{pad}continue\n")),
        Stmt::Call(call) => dump_expr(out, call, depth),
    }
}

fn dump_block(out: &mut String, block: &Block, depth: usize) {
    for stmt in &block.stmts {
        dump_stmt(out, stmt, depth);
    }
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
        ExprKind::Variant { enum_name, variant, .. } => {
            out.push_str(&format!("{pad}variant {enum_name}::{variant}\n"))
        }
        ExprKind::Neg(operand) => {
            out.push_str(&format!("{pad}neg\n"));
            dump_expr(out, operand, depth + 1);
        }
        ExprKind::Not(operand) => {
            out.push_str(&format!("{pad}not\n"));
            dump_expr(out, operand, depth + 1);
        }
        ExprKind::Cmp { op, lhs, rhs } => {
            out.push_str(&format!("{pad}{}\n", op.symbol()));
            dump_expr(out, lhs, depth + 1);
            dump_expr(out, rhs, depth + 1);
        }
        ExprKind::Bin { op, lhs, rhs } => {
            out.push_str(&format!("{pad}{}\n", op.symbol()));
            dump_expr(out, lhs, depth + 1);
            dump_expr(out, rhs, depth + 1);
        }
        ExprKind::Logic { op, lhs, rhs } => {
            out.push_str(&format!("{pad}{}\n", op.symbol()));
            dump_expr(out, lhs, depth + 1);
            dump_expr(out, rhs, depth + 1);
        }
        ExprKind::Match { scrutinee, arms, .. } => {
            out.push_str(&format!("{pad}match\n"));
            dump_expr(out, scrutinee, depth + 1);
            for arm in arms {
                out.push_str(&format!("{pad}  {}::{}\n", arm.enum_name, arm.variant));
                match &arm.body {
                    ArmBody::Value(value) => dump_expr(out, value, depth + 2),
                    ArmBody::Block(block) => dump_block(out, block, depth + 2),
                }
            }
        }
        ExprKind::Call { name, args, .. } => {
            out.push_str(&format!("{pad}call {name}\n"));
            for arg in args {
                dump_expr(out, arg, depth + 1);
            }
        }
    }
}
