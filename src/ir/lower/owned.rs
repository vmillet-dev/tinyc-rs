//! Which string variables nothing else can be holding, so growing one in place is safe.

use std::collections::{HashMap, HashSet};

use crate::ast::{
    ArmBody, BinOp, Block as AstBlock, Builtin, Expr, ExprKind, FnDecl, Place, Prim, PrintPart,
    Stmt, Ty,
};
use crate::sema::Types;

/// The string variables of one function that **nothing else can be holding**.
///
/// A string is read-only, so sharing one is free and the compiler has never had
/// to ask this before: two names for the same characters cannot be told apart.
/// One operation would tell them apart, and it is the one worth having —
/// growing a string *where it stands*, which bumps a count at `[p-8]` that
/// every other name for it can see.
///
/// So the question is asked the cautious way round: a name is owned only if it
/// can be *proved* to be, and anything the analysis does not recognise means
/// no. Wrong permissively is memory corruption; wrong strictly costs a program
/// an optimisation. All of this has to hold:
///
/// * It is a **local**, not a parameter — a parameter is a string the caller
///   still holds — and it is declared exactly once in the function, so the name
///   cannot mean two different variables in two blocks.
/// * Every value it is ever given is **freshly built**: a concat, a conversion,
///   a `read_line`. A literal counts too, and is the reason the runtime keeps a
///   check of its own: a literal lives in `.data`, which is not the arena, so
///   the in-place path simply never fires for one.
/// * It is never **kept** anywhere else: not assigned to another variable, not
///   passed to a function, not returned, not put in a list, an array or a
///   field. Reading it — its length, one of its characters, printing it,
///   comparing it, joining it to something — is not keeping it.
///
/// This is not ownership in the type system: no program is refused that was not
/// refused before, and a name failing any of these tests gets the code it
/// always got.
pub(super) fn owned_strings(function: &FnDecl, types: &Types) -> HashSet<String> {
    let mut facts = Owned {
        types,
        declared: HashMap::new(),
        fresh: HashMap::new(),
        escaped: HashSet::new(),
    };
    // A parameter is the caller's, whatever the body does with it.
    for param in &function.params {
        facts.escaped.insert(param.name.clone());
    }
    facts.block(&function.body);

    facts
        .declared
        .iter()
        .filter(|(name, times)| {
            // Declared once, so the name means one variable everywhere.
            **times == 1
                && facts.fresh.get(*name).copied().unwrap_or(false)
                && !facts.escaped.contains(*name)
        })
        .map(|(name, _)| name.clone())
        .collect()
}

/// The sweep [`owned_strings`] is made of.
struct Owned<'a> {
    types: &'a Types,
    /// How many times each name is declared, so shadowing can be ruled out.
    declared: HashMap<String, usize>,
    /// Whether every value the name has been given so far was freshly built.
    fresh: HashMap<String, bool>,
    /// Names whose *pointer* reaches somewhere that outlives the read.
    escaped: HashSet<String>,
}

impl Owned<'_> {
    fn block(&mut self, block: &AstBlock) {
        for stmt in &block.stmts {
            self.stmt(stmt);
        }
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Decl { id, name, init, .. } => {
                *self.declared.entry(name.clone()).or_insert(0) += 1;
                let fresh = self.types.of(*id) == Ty::Str && self.is_fresh(init);
                let entry = self.fresh.entry(name.clone()).or_insert(true);
                *entry &= fresh;
                self.expr(init, Kept::Yes);
            }
            Stmt::Assign { target, value } => {
                if let Place::Var { name, .. } = target {
                    let fresh = self.types.of(value.id) == Ty::Str && self.is_fresh(value);
                    let entry = self.fresh.entry(name.clone()).or_insert(true);
                    *entry &= fresh;
                } else {
                    self.place(target);
                }
                self.expr(value, Kept::Yes);
            }
            // What is pushed is kept by the list; where it is pushed is a place.
            Stmt::Push { target, value, .. } => {
                self.place(target);
                self.expr(value, Kept::Yes);
            }
            // Handed outward, so whoever called this function keeps it.
            Stmt::Return { value, .. } => {
                if let Some(value) = value {
                    self.expr(value, Kept::Yes);
                }
            }
            Stmt::Print { parts, .. } => {
                for part in parts {
                    match part {
                        PrintPart::Text(_) => {}
                        PrintPart::Value(expr) | PrintPart::Spec { expr, .. } => {
                            self.expr(expr, Kept::No)
                        }
                    }
                }
            }
            Stmt::If { cond, then_block, else_block } => {
                self.expr(cond, Kept::No);
                self.block(then_block);
                if let Some(block) = else_block {
                    self.block(block);
                }
            }
            Stmt::While { cond, body } => {
                self.expr(cond, Kept::No);
                self.block(body);
            }
            Stmt::For { init, cond, step, body } => {
                self.stmt(init);
                self.expr(cond, Kept::No);
                self.stmt(step);
                self.block(body);
            }
            Stmt::Match(expr) | Stmt::Call(expr) => self.expr(expr, Kept::No),
            Stmt::Break { .. } | Stmt::Continue { .. } => {}
        }
    }

    /// The index expressions inside a place, which are read and nothing more.
    fn place(&mut self, place: &Place) {
        match place {
            Place::Var { .. } => {}
            Place::Element { base, index, .. } => {
                self.place(base);
                self.expr(index, Kept::No);
            }
            Place::Field { base, .. } => self.place(base),
        }
    }

    /// Walk an expression, knowing whether the value it produces is kept.
    ///
    /// The recursion is where the whole judgement lives: `y = x` keeps `x`,
    /// while `y = x + "!"` does not — the concat reads `x` and builds something
    /// else. So an operator passes [`Kept::No`] down to its operands, and
    /// anything that stores a value away passes [`Kept::Yes`].
    ///
    /// Every shape that could keep a value is listed. A shape this does not
    /// recognise cannot arise, but if one ever did, the wildcard treats its
    /// children as kept — the cautious answer.
    fn expr(&mut self, expr: &Expr, kept: Kept) {
        match &expr.kind {
            ExprKind::Var(name) => {
                if matches!(kept, Kept::Yes) {
                    self.escaped.insert(name.clone());
                }
            }
            // A string operator reads its operands and allocates its answer.
            ExprKind::Bin { lhs, rhs, .. } | ExprKind::Cmp { lhs, rhs, .. } => {
                self.expr(lhs, Kept::No);
                self.expr(rhs, Kept::No);
            }
            ExprKind::Logic { lhs, rhs, .. } => {
                self.expr(lhs, Kept::No);
                self.expr(rhs, Kept::No);
            }
            ExprKind::Neg(inner) | ExprKind::Not(inner) => self.expr(inner, Kept::No),
            ExprKind::Len { array, .. } => self.expr(array, Kept::No),
            ExprKind::Index { array, index } => {
                self.expr(array, Kept::No);
                self.expr(index, Kept::No);
            }
            // A conversion builds a new value out of what it reads.
            ExprKind::Convert { value, .. } => self.expr(value, Kept::No),
            // A callee may keep anything it is handed, and there is no
            // whole-program analysis here to say otherwise.
            ExprKind::Call { args, .. } => {
                for arg in args {
                    self.expr(arg, Kept::Yes);
                }
            }
            ExprKind::MethodCall { receiver, args, .. } => {
                self.expr(receiver, Kept::Yes);
                for arg in args {
                    self.expr(arg, Kept::Yes);
                }
            }
            // Both put what they are given *into* something that outlives the
            // expression.
            ExprKind::Array { elements, .. } => {
                for element in elements {
                    self.expr(element, Kept::Yes);
                }
            }
            ExprKind::New { fields, .. } => {
                for field in fields {
                    self.expr(&field.value, Kept::Yes);
                }
            }
            // A match hands its arm's value on as its own, so the arms inherit
            // whatever was asked of the match. The scrutinee is only compared.
            ExprKind::Match { scrutinee, arms, .. } => {
                self.expr(scrutinee, Kept::No);
                for arm in arms {
                    match &arm.body {
                        ArmBody::Value(value) => self.expr(value, kept),
                        ArmBody::Block(block) => self.block(block),
                    }
                }
            }
            ExprKind::Field { object, .. } => self.expr(object, Kept::No),
            ExprKind::Int(_)
            | ExprKind::Float(_)
            | ExprKind::Str(_)
            | ExprKind::Char(_)
            | ExprKind::Bool(_)
            | ExprKind::Variant { .. } => {}
        }
    }

    /// Whether this expression *builds* the string it produces, rather than
    /// handing on one that already existed somewhere.
    ///
    /// Every routine named here allocates a block of its own and gives it to
    /// nobody else. A literal is not built at all, and is included for a
    /// different reason: it lives in `.data`, so the in-place path cannot fire
    /// for it and there is nothing to be wrong about.
    fn is_fresh(&self, expr: &Expr) -> bool {
        match &expr.kind {
            // `a + b` on strings, which is a `concat` and allocates.
            ExprKind::Bin { op: BinOp::Add, lhs, .. } => self.types.of(lhs.id) == Ty::Str,
            // `string(n)`, `string(c)` and `string(cs)` all allocate; the
            // conversions that do not produce a string never reach here.
            ExprKind::Convert { to: Prim::Str, .. } => true,
            ExprKind::Str(_) => true,
            // `read_line()` seals a fresh list of characters into a string. Its
            // name cannot mean anything else — `sema` refuses to let a program
            // redefine a built-in.
            ExprKind::Call { name, args, .. } => {
                name == Builtin::ReadLine.name() && args.is_empty()
            }
            // A match is fresh when every arm is. Anything else — a variable, a
            // call, an element, a field — hands on a string that already had a
            // name somewhere.
            ExprKind::Match { arms, .. } => arms.iter().all(|arm| match &arm.body {
                ArmBody::Value(value) => self.is_fresh(value),
                ArmBody::Block(_) => false,
            }),
            _ => false,
        }
    }
}

/// Whether the value an expression produces is stored somewhere that outlives
/// the expression itself.
#[derive(Clone, Copy)]
enum Kept {
    Yes,
    No,
}

/// Whether this expression reads the variable `name` anywhere inside it.
///
/// Asked by [`Lowering::append_chain`], which turns one expression into several
/// statements and so has to be sure nothing in it could tell the difference.
/// The one shape not walked is a block arm, which is answered `true` without
/// looking: it may hold statements, and the cautious answer costs nothing but
/// an optimisation.
pub(super) fn mentions(expr: &Expr, name: &str) -> bool {
    let mut found = false;
    let mut visit = |e: &Expr| found |= mentions(e, name);
    match &expr.kind {
        ExprKind::Var(other) => return other == name,
        ExprKind::Int(_)
        | ExprKind::Float(_)
        | ExprKind::Str(_)
        | ExprKind::Char(_)
        | ExprKind::Bool(_)
        | ExprKind::Variant { .. } => {}
        ExprKind::Neg(inner) | ExprKind::Not(inner) => visit(inner),
        ExprKind::Bin { lhs, rhs, .. }
        | ExprKind::Cmp { lhs, rhs, .. }
        | ExprKind::Logic { lhs, rhs, .. } => {
            visit(lhs);
            visit(rhs);
        }
        ExprKind::Index { array, index } => {
            visit(array);
            visit(index);
        }
        ExprKind::Len { array, .. } => visit(array),
        ExprKind::Convert { value, .. } => visit(value),
        ExprKind::Field { object, .. } => visit(object),
        ExprKind::Array { elements, .. } => elements.iter().for_each(visit),
        ExprKind::New { fields, .. } => fields.iter().for_each(|f| visit(&f.value)),
        ExprKind::Call { args, .. } => args.iter().for_each(visit),
        ExprKind::MethodCall { receiver, args, .. } => {
            visit(receiver);
            args.iter().for_each(visit);
        }
        ExprKind::Match { scrutinee, arms, .. } => {
            visit(scrutinee);
            let arms_mention = arms.iter().any(|arm| match &arm.body {
                ArmBody::Value(value) => mentions(value, name),
                ArmBody::Block(_) => true,
            });
            found |= arms_mention;
        }
    }
    found
}


/// Whether lowering this expression *reserves* the room its value lives in,
/// rather than naming room something else owns.
///
/// The three that do are the two literals and a call, which fills room the
/// caller reserved for it. Everything else — a variable, a field, an element —
/// points at somebody's else's, so assigning it means copying.
pub(super) fn builds_its_own(expr: &Expr) -> bool {
    matches!(
        expr.kind,
        ExprKind::New { .. }
            | ExprKind::Array { .. }
            | ExprKind::Call { .. }
            | ExprKind::MethodCall { .. }
    )
}

