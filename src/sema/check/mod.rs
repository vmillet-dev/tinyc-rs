//! Pass 3: checking one function body, split by what is being checked.

mod aggregate;
mod expr;
mod matching;
mod place;
mod stmt;

use std::collections::{HashMap, HashSet};

use crate::ast::{
    ArmBody, BinOp, Block, EnumId, Expr, ExprKind, FieldInit, FnDecl,
    MatchArm, NodeId, Pattern, Place, Prim, PrintPart, Spec, Stmt, Ty, TypeRef,
    fits_in_an_int, is_scalar_value,
};
use crate::diag::{Diagnostic, Span};

use super::diagnostics::{
    Binding, CONVERSIONS, PARAMETER, missing_verb, no_such_member, plural, scalar_range_label,
};
use super::signature::{always_returns, const_float, const_int, diverges};
use super::{
    Declared, MAX_ARRAY_LEN, Signature, Types, came_from, count, list,
    resolve_type,
};

/// What the whole program shares: one type table, one signature table, one list
/// of diagnostics.
pub(super) struct Checker<'a> {
    pub(super) types: Types,
    /// Every signature in the program, immutable once pass 1 is done.
    pub(super) signatures: &'a HashMap<String, Signature>,
    /// Every declared enum, immutable once pass 0 is done.
    pub(super) declared: Declared,
    pub(super) errors: Vec<Diagnostic>,
}

/// One function's body, and the state that means nothing outside it.
///
/// Scopes, the return type and the names already complained about are all
/// per-function. Kept as fields of the program-wide [`Checker`] they would have
/// to be reset at the top of every body, and forgetting one would leak a
/// previous function's answer into the next — so they live here instead, and are
/// created and dropped with the body they describe.
pub(super) struct FnChecker<'a, 'c> {
    pub(super) shared: &'c mut Checker<'a>,
    /// Where this function sits in the program, which is how its signature is
    /// found whether it is a method or not.
    pub(super) at: usize,
    /// Return type of this function, and where it was written.
    pub(super) ret: Option<Ty>,
    pub(super) ret_span: Span,
    /// One map per open block, innermost last. A name is looked up from the
    /// inside out, and a block's declarations disappear when it closes.
    pub(super) scopes: Vec<HashMap<String, Binding>>,
    /// Names already reported as undeclared. One missing declaration is one
    /// mistake, however many times the name is mentioned afterwards.
    pub(super) undeclared: HashSet<String>,
    /// How many loops enclose the statement being checked. `break` and
    /// `continue` need one; the count rather than a flag is what makes them
    /// legal in a loop nested inside an `if` inside a loop.
    pub(super) loop_depth: u32,
}

impl<'a, 'c> FnChecker<'a, 'c> {
    /// `at` is the function's place in the program, which is how a method is
    /// reached: its signature lives in its class rather than in the program's
    /// namespace, and only the position identifies it either way.
    pub(super) fn new(shared: &'c mut Checker<'a>, at: usize, function: &FnDecl) -> FnChecker<'a, 'c> {
        // Pass 1 resolved this already; re-resolving would report an unknown
        // return type a second time.
        let ret = shared.types.fn_ret[at];
        FnChecker {
            shared,
            at,
            ret,
            ret_span: function.ret_span,
            scopes: Vec::new(),
            undeclared: HashSet::new(),
            loop_depth: 0,
        }
    }

    pub(super) fn error(&mut self, diagnostic: Diagnostic) {
        self.shared.errors.push(diagnostic);
    }

    /// Resolve a written type, falling back on the recovery type when it names
    /// nothing. The mistake is reported by [`resolve_type`] itself.
    pub(super) fn resolve(&mut self, ty: &TypeRef) -> Ty {
        resolve_type(&mut self.shared.declared, ty, &mut self.shared.errors).unwrap_or(Ty::Int)
    }

    /// A type's name. [`Ty`] cannot answer this alone — an enum's name is the
    /// program's, not the compiler's — so every diagnostic asks here.
    pub(super) fn ty_name(&self, ty: Ty) -> String {
        ty.name(&self.shared.declared.table).to_string()
    }

    /// The same with its indefinite article, for prose.
    pub(super) fn ty_article(&self, ty: Ty) -> String {
        ty.with_article(&self.shared.declared.table)
    }

    /// The signature of a function anywhere in the program.
    ///
    /// The table outlives the checker, so what comes back does not borrow
    /// `self` and the caller may keep reporting errors while holding it.
    pub(super) fn signature(&self, name: &str) -> Option<&'a Signature> {
        self.shared.signatures.get(name)
    }

    /// The type and declaration span of a visible variable.
    pub(super) fn lookup(&self, name: &str) -> Option<(Ty, Span)> {
        self.binding(name).map(|b| (b.ty, b.name_span))
    }

    pub(super) fn binding(&self, name: &str) -> Option<Binding> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name).copied())
    }

    /// Add a name to the innermost scope, reporting a clash inside that same
    /// scope. Shared by declarations and parameters, which is the whole reason
    /// [`crate::ast::Param`] carries the same spans as a `Decl`.
    pub(super) fn declare(&mut self, name: &str, ty: Ty, name_span: Span, what: &str) {
        let innermost = self.scopes.last_mut().expect("a scope is always open");
        if let Some(previous) = innermost.get(name) {
            let previous = previous.name_span;
            self.error(
                Diagnostic::new(format!("`{name}` is already declared"), name_span)
                    .with_label(format!("declared a second time here, as {what}"))
                    .with_note("previous declaration", Some(previous)),
            );
        } else {
            let parameter = what == PARAMETER;
            innermost.insert(name.to_string(), Binding { ty, name_span, parameter });
        }
    }

    /// Report `name` as undeclared, unless this function has already been told.
    ///
    /// The diagnostic is built lazily, because on every mention after the first
    /// it would only be thrown away.
    pub(super) fn report_undeclared(&mut self, name: &str, diagnostic: impl FnOnce() -> Diagnostic) {
        if self.undeclared.insert(name.to_string()) {
            self.error(diagnostic());
        }
    }

    /// Record an expression's type in the side table, and hand it back so
    /// callers can keep using it.
    pub(super) fn record(&mut self, id: NodeId, ty: Ty) -> Ty {
        self.shared.types.expr_ty[id.0 as usize] = ty;
        ty
    }

    pub(super) fn run(&mut self, function: &FnDecl) {
        // Parameters live in the body's outermost scope, so a local of the same
        // name at the top level of the body is a redeclaration, not a shadow.
        self.scopes.push(HashMap::new());
        // Pass 1 resolved these too, and reported any that named nothing.
        let params = self.shared.types.fn_params[self.at].clone();
        for (param, ty) in function.params.iter().zip(params) {
            self.declare(&param.name, ty, param.name_span, PARAMETER);
        }
        for stmt in &function.body.stmts {
            self.stmt(stmt);
        }
        self.scopes.pop();

        if function.ret.is_some() && !always_returns(&function.body) {
            // Point at the closing brace: that is the "end of the body" the
            // message talks about, and it is where a `return` would have to go.
            let body = function.body.span;
            let closing_brace = Span::new((body.offset + body.len - 1) as usize, 1);
            self.error(
                Diagnostic::new(
                    format!("`{}` may finish without returning a value", function.name),
                    closing_brace,
                )
                .with_label("control can reach the end of this body")
                .with_note("this return type was declared here", Some(function.ret_span)),
            );
        }
    }

    pub(super) fn block(&mut self, block: &Block) {
        self.scopes.push(HashMap::new());
        for stmt in &block.stmts {
            self.stmt(stmt);
        }
        self.scopes.pop();
    }

    /// `if`, `while` and `for` all require a `bool` here.
    pub(super) fn condition(&mut self, cond: &Expr, keyword: &str) {
        let ty = self.expr(cond);
        if ty != Ty::Bool {
            self.error(
                Diagnostic::new(
                    format!("the condition of `{keyword}` must be a `bool`"),
                    cond.span,
                )
                .with_label(format!("expected bool, found {}", self.ty_name(ty)))
                .with_note("comparisons like `i < 10` produce a bool", None),
            );
        }
    }

}
