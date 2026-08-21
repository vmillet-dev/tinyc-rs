//! Stage 3: type checking.
//!
//! Walks the AST with a symbol table and records the type of every expression
//! node in a side table keyed by [`NodeId`], so the tree itself stays immutable.
//! Unlike the lexer and parser, this stage collects *all* errors before giving
//! up: statements are independent enough that later ones are still worth
//! checking.
//!
//! ## Two passes, and why
//!
//! Signatures are collected before any body is checked. That is what lets a
//! function call another one declared further down the file, and what lets a
//! function call *itself* — by the time `fib`'s body is checked, `fib` is
//! already in the table. A single pass could only ever see backwards.

use std::collections::{HashMap, HashSet};

use crate::ast::{BinOp, Block, Expr, ExprKind, FnDecl, NodeId, Program, Stmt, Ty};
use crate::diag::{Diagnostic, Result, Span};

/// The name of the entry point.
pub const ENTRY_POINT: &str = "main";

/// Type of every expression node, indexed by [`NodeId`].
#[derive(Debug)]
pub struct Types {
    expr_ty: Vec<Ty>,
}

impl Types {
    pub fn of(&self, id: NodeId) -> Ty {
        self.expr_ty[id.0 as usize]
    }
}

/// Everything a call site needs to know about its callee.
#[derive(Clone, Debug)]
struct Signature {
    params: Vec<Ty>,
    /// `None` for a function that returns nothing.
    ret: Option<Ty>,
    /// Where the function was declared, for "defined here" notes.
    name_span: Span,
}

/// Type check a program.
///
/// `max_params` is how many arguments the target can pass in registers; the
/// front end has no opinion of its own about that, it just enforces what
/// [`crate::codegen::RegisterFile::max_args`] reports.
pub fn check(program: &Program, max_params: usize) -> Result<Types> {
    let mut errors = Vec::new();

    // Pass 1: every signature, before any body.
    let signatures = collect_signatures(program, max_params, &mut errors);
    check_entry_point(program, &signatures, &mut errors);

    // Pass 2: the bodies, each with the whole table visible.
    let mut checker = Checker {
        // `Int` is the recovery type: an expression that failed to check is
        // treated as an int so a single mistake does not cascade.
        types: Types { expr_ty: vec![Ty::Int; program.node_count] },
        signatures: &signatures,
        errors,
    };
    for function in &program.functions {
        FnChecker::new(&mut checker, function).run(function);
    }

    if checker.errors.is_empty() {
        return Ok(checker.types);
    }

    // Errors are produced in traversal order, which is not source order: a
    // statement's value is checked before its name, so `y = y + 1` would report
    // column 7 before column 3. Sorting is what makes a list of diagnostics read
    // down the file.
    let mut errors = checker.errors;
    errors.sort_by_key(|d| d.span.offset);
    Err(errors)
}

/// Pass 1: one entry per function name, with the first declaration winning.
fn collect_signatures(
    program: &Program,
    max_params: usize,
    errors: &mut Vec<Diagnostic>,
) -> HashMap<String, Signature> {
    let mut signatures: HashMap<String, Signature> = HashMap::new();

    for function in &program.functions {
        if function.params.len() > max_params {
            // Point at the first parameter that does not fit.
            let offending = &function.params[max_params];
            errors.push(
                Diagnostic::new(
                    format!(
                        "`{}` takes {} parameters, but at most {max_params} are supported",
                        function.name,
                        function.params.len()
                    ),
                    offending.ty_span.to(offending.name_span),
                )
                .with_label(format!("parameter {} is one too many", max_params + 1))
                .with_note(
                    format!(
                        "the first {max_params} arguments travel in registers; passing more \
                         would need stack arguments"
                    ),
                    None,
                ),
            );
        }

        if let Some(previous) = signatures.get(&function.name) {
            let previous = previous.name_span;
            errors.push(
                Diagnostic::new(
                    format!("`{}` is already defined", function.name),
                    function.name_span,
                )
                .with_label("defined a second time here")
                .with_note("previous definition", Some(previous)),
            );
            continue;
        }

        signatures.insert(
            function.name.clone(),
            Signature {
                params: function.params.iter().map(|p| p.ty).collect(),
                ret: function.ret,
                name_span: function.name_span,
            },
        );
    }

    signatures
}

/// `main` must exist, take nothing and return nothing: it is what the C runtime
/// calls, and [`crate::codegen`] returns 0 from it.
fn check_entry_point(
    program: &Program,
    signatures: &HashMap<String, Signature>,
    errors: &mut Vec<Diagnostic>,
) {
    let Some(main) = signatures.get(ENTRY_POINT) else {
        // Nothing to underline in a file with no `main`, so the diagnostic
        // points at the very start of it.
        errors.push(
            Diagnostic::new(format!("this program has no `{ENTRY_POINT}` function"), Span::new(0, 0))
                .with_label("a program starts here")
                .with_note(format!("add `fn {ENTRY_POINT}() {{ ... }}`"), None),
        );
        return;
    };

    // The declaration itself, for spans the signature does not carry.
    let decl = program
        .functions
        .iter()
        .find(|f| f.name == ENTRY_POINT)
        .expect("the table was built from these declarations");

    if !main.params.is_empty() {
        let first = &decl.params[0];
        errors.push(
            Diagnostic::new(
                format!("`{ENTRY_POINT}` must not take parameters"),
                first.ty_span.to(decl.params[decl.params.len() - 1].name_span),
            )
            .with_label("the runtime calls it with no arguments"),
        );
    }

    if let Some(ty) = main.ret {
        errors.push(
            Diagnostic::new(
                format!("`{ENTRY_POINT}` must not return a value"),
                decl.ret_span,
            )
            .with_label(format!("expected no return type, found {}", ty.name()))
            .with_note("the process exit code is always 0", None),
        );
    }
}

/// Whether every path out of a block ends in a `return`.
///
/// Deliberately simple: a loop is never assumed to run, so `while (true)` does
/// not count. That can only reject a program that would in fact be fine, never
/// accept one that would fall off the end.
fn always_returns(block: &Block) -> bool {
    block.stmts.iter().any(|stmt| match stmt {
        Stmt::Return { .. } => true,
        // Both arms must return, and an `if` without an `else` has a path that
        // skips it entirely.
        Stmt::If { then_block, else_block: Some(else_block), .. } => {
            always_returns(then_block) && always_returns(else_block)
        }
        _ => false,
    })
}

/// What the whole program shares: one type table, one signature table, one list
/// of diagnostics.
struct Checker<'a> {
    types: Types,
    /// Every signature in the program, immutable once pass 1 is done.
    signatures: &'a HashMap<String, Signature>,
    errors: Vec<Diagnostic>,
}

/// One function's body, and the state that means nothing outside it.
///
/// Scopes, the return type and the names already complained about are all
/// per-function. Kept as fields of the program-wide [`Checker`] they would have
/// to be reset at the top of every body, and forgetting one would leak a
/// previous function's answer into the next — so they live here instead, and are
/// created and dropped with the body they describe.
struct FnChecker<'a, 'c> {
    shared: &'c mut Checker<'a>,
    /// Return type of this function, and where it was written.
    ret: Option<Ty>,
    ret_span: Span,
    /// One map per open block, innermost last. A name is looked up from the
    /// inside out, and a block's declarations disappear when it closes.
    scopes: Vec<HashMap<String, (Ty, Span)>>,
    /// Names already reported as undeclared. One missing declaration is one
    /// mistake, however many times the name is mentioned afterwards.
    undeclared: HashSet<String>,
    /// How many loops enclose the statement being checked. `break` and
    /// `continue` need one; the count rather than a flag is what makes them
    /// legal in a loop nested inside an `if` inside a loop.
    loop_depth: u32,
}

impl<'a, 'c> FnChecker<'a, 'c> {
    fn new(shared: &'c mut Checker<'a>, function: &FnDecl) -> FnChecker<'a, 'c> {
        FnChecker {
            shared,
            ret: function.ret,
            ret_span: function.ret_span,
            scopes: Vec::new(),
            undeclared: HashSet::new(),
            loop_depth: 0,
        }
    }

    fn error(&mut self, diagnostic: Diagnostic) {
        self.shared.errors.push(diagnostic);
    }

    /// The signature of a function anywhere in the program.
    ///
    /// The table outlives the checker, so what comes back does not borrow
    /// `self` and the caller may keep reporting errors while holding it.
    fn signature(&self, name: &str) -> Option<&'a Signature> {
        self.shared.signatures.get(name)
    }

    /// The type and declaration span of a visible variable.
    fn lookup(&self, name: &str) -> Option<(Ty, Span)> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name).copied())
    }

    /// Add a name to the innermost scope, reporting a clash inside that same
    /// scope. Shared by declarations and parameters, which is the whole reason
    /// [`crate::ast::Param`] carries the same spans as a `Decl`.
    fn declare(&mut self, name: &str, ty: Ty, name_span: Span, what: &str) {
        let innermost = self.scopes.last_mut().expect("a scope is always open");
        if let Some((_, previous)) = innermost.get(name) {
            let previous = *previous;
            self.error(
                Diagnostic::new(format!("`{name}` is already declared"), name_span)
                    .with_label(format!("declared a second time here, as {what}"))
                    .with_note("previous declaration", Some(previous)),
            );
        } else {
            innermost.insert(name.to_string(), (ty, name_span));
        }
    }

    /// Report `name` as undeclared, unless this function has already been told.
    ///
    /// The diagnostic is built lazily, because on every mention after the first
    /// it would only be thrown away.
    fn report_undeclared(&mut self, name: &str, diagnostic: impl FnOnce() -> Diagnostic) {
        if self.undeclared.insert(name.to_string()) {
            self.error(diagnostic());
        }
    }

    /// Record an expression's type in the side table, and hand it back so
    /// callers can keep using it.
    fn record(&mut self, id: NodeId, ty: Ty) -> Ty {
        self.shared.types.expr_ty[id.0 as usize] = ty;
        ty
    }

    fn run(&mut self, function: &FnDecl) {
        // Parameters live in the body's outermost scope, so a local of the same
        // name at the top level of the body is a redeclaration, not a shadow.
        self.scopes.push(HashMap::new());
        for param in &function.params {
            self.declare(&param.name, param.ty, param.name_span, "a parameter");
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

    fn block(&mut self, block: &Block) {
        self.scopes.push(HashMap::new());
        for stmt in &block.stmts {
            self.stmt(stmt);
        }
        self.scopes.pop();
    }

    /// `if`, `while` and `for` all require a `bool` here.
    fn condition(&mut self, cond: &Expr, keyword: &str) {
        let ty = self.expr(cond);
        if ty != Ty::Bool {
            self.error(
                Diagnostic::new(
                    format!("the condition of `{keyword}` must be a `bool`"),
                    cond.span,
                )
                .with_label(format!("expected bool, found {}", ty.name()))
                .with_note("comparisons like `i < 10` produce a bool", None),
            );
        }
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Decl { ty, name, name_span, init, .. } => {
                let actual = self.expr(init);
                if actual != *ty {
                    self.error(
                        Diagnostic::new(
                            format!(
                                "cannot initialize {} variable with {} value",
                                ty.with_article(),
                                actual.with_article()
                            ),
                            init.span,
                        )
                        .with_label(format!("expected {}, found {}", ty.name(), actual.name())),
                    );
                }
                self.declare(name, *ty, *name_span, "a variable");
            }
            Stmt::Assign { name, name_span, value } => {
                // The target is looked up *before* the value is checked, so
                // that `y = y + 1` reports the assignment rather than the use
                // inside it — the assignment is the better place to point.
                let target = self.lookup(name);
                if target.is_none() {
                    let (name, span) = (name.clone(), *name_span);
                    self.report_undeclared(&name, || {
                        Diagnostic::new(format!("undeclared variable `{name}`"), span)
                            .with_label("assign to it after declaring it")
                            .with_note(
                                format!("a declaration gives it a type, as in `int {name} = 0;`"),
                                None,
                            )
                    });
                }

                let actual = self.expr(value);
                // A variable keeps the type it was declared with. An undeclared
                // one has no type to disagree with, and was reported above.
                if let Some((declared, declared_span)) = target
                    && actual != declared
                {
                    self.error(
                        Diagnostic::new(
                            format!(
                                "cannot assign {} value to {} variable",
                                actual.with_article(),
                                declared.with_article()
                            ),
                            value.span,
                        )
                        .with_label(format!(
                            "expected {}, found {}",
                            declared.name(),
                            actual.name()
                        ))
                        .with_note(format!("`{name}` was declared here"), Some(declared_span)),
                    );
                }
            }
            Stmt::Print { value, .. } => {
                self.expr(value);
            }
            Stmt::If { cond, then_block, else_block } => {
                self.condition(cond, "if");
                self.block(then_block);
                if let Some(block) = else_block {
                    self.block(block);
                }
            }
            Stmt::While { cond, body } => {
                self.condition(cond, "while");
                self.loop_body(body);
            }
            Stmt::For { init, cond, step, body } => {
                // The initialiser's variable is visible to the condition, the
                // step and the body, but not after the loop — so the whole
                // `for` gets a scope of its own.
                self.scopes.push(HashMap::new());
                self.stmt(init);
                self.condition(cond, "for");
                self.stmt(step);
                self.loop_body(body);
                self.scopes.pop();
            }
            Stmt::Return { span, value } => self.return_stmt(*span, value.as_ref()),
            Stmt::Break { span } => self.loop_jump(*span, "break", "leaves"),
            Stmt::Continue { span } => self.loop_jump(*span, "continue", "restarts"),
            // The result of a call statement is discarded, so a function that
            // returns nothing is exactly as welcome as one that returns a value.
            // The type still goes in the table: every expression node has an
            // entry, and a hole here would be a trap for whoever reads one next.
            Stmt::Call(call) => {
                let ty = self.call(call, true).unwrap_or(Ty::Int);
                self.record(call.id, ty);
            }
        }
    }

    /// A loop's body, checked with one more loop open around it.
    fn loop_body(&mut self, body: &Block) {
        self.loop_depth += 1;
        self.block(body);
        self.loop_depth -= 1;
    }

    /// `break` and `continue` need a loop to talk about. `verb` completes
    /// "there is no loop this ... " for the one being checked.
    fn loop_jump(&mut self, span: Span, keyword: &str, verb: &str) {
        if self.loop_depth == 0 {
            self.error(
                Diagnostic::new(format!("`{keyword}` outside of a loop"), span)
                    .with_label(format!("there is no loop this {verb}"))
                    .with_note(
                        format!("`{keyword}` only means something inside `while` or `for`"),
                        None,
                    ),
            );
        }
    }

    fn return_stmt(&mut self, span: Span, value: Option<&Expr>) {
        match (self.ret, value) {
            (Some(expected), Some(expr)) => {
                let actual = self.expr(expr);
                if actual != expected {
                    self.error(
                        Diagnostic::new(
                            format!(
                                "cannot return {} value from a function returning {}",
                                actual.with_article(),
                                expected.with_article()
                            ),
                            expr.span,
                        )
                        .with_label(format!(
                            "expected {}, found {}",
                            expected.name(),
                            actual.name()
                        ))
                        .with_note("declared here", Some(self.ret_span)),
                    );
                }
            }
            (Some(expected), None) => self.error(
                Diagnostic::new("this `return` needs a value", span)
                    .with_label(format!("expected {}", expected.with_article()))
                    .with_note("this return type was declared here", Some(self.ret_span)),
            ),
            (None, Some(expr)) => {
                self.expr(expr);
                self.error(
                    Diagnostic::new("this function returns nothing", expr.span)
                        .with_label("so this value has nowhere to go")
                        .with_note(
                            "add a return type after `)` to return a value",
                            Some(self.ret_span),
                        ),
                );
            }
            (None, None) => {}
        }
    }

    /// Check a call and answer the type it produces, or `None` when the callee
    /// returns nothing.
    ///
    /// `as_statement` says whether "nothing" is acceptable here: it is for
    /// `greet("hi");`, it is not for `int n = greet("hi");`.
    fn call(&mut self, expr: &Expr, as_statement: bool) -> Option<Ty> {
        let ExprKind::Call { name, name_span, args } = &expr.kind else {
            unreachable!("the parser only builds `Stmt::Call` around a call");
        };

        // Arguments are checked first, so their own mistakes are reported even
        // when the callee turns out not to exist.
        let actual: Vec<Ty> = args.iter().map(|arg| self.expr(arg)).collect();

        // The signature table outlives this checker, so holding a signature does
        // not borrow `self` for the rest of the call.
        let Some(signature) = self.signature(name) else {
            self.error(
                Diagnostic::new(format!("unknown function `{name}`"), *name_span)
                    .with_label("not defined anywhere in this file"),
            );
            return Some(Ty::Int);
        };
        let (params, ret, declared_at) =
            (signature.params.clone(), signature.ret, signature.name_span);

        if params.len() != args.len() {
            self.error(
                Diagnostic::new(
                    format!(
                        "`{name}` takes {}, but {} supplied",
                        plural(params.len(), "argument"),
                        plural(args.len(), "was")
                    ),
                    expr.span,
                )
                .with_label(format!("expected {} here", plural(params.len(), "argument")))
                .with_note(format!("`{name}` is defined here"), Some(declared_at)),
            );
        } else {
            for ((expected, found), arg) in params.iter().zip(&actual).zip(args) {
                if expected != found {
                    self.error(
                        Diagnostic::new(
                            format!(
                                "cannot pass {} value where {} is expected",
                                found.with_article(),
                                expected.with_article()
                            ),
                            arg.span,
                        )
                        .with_label(format!("expected {}, found {}", expected.name(), found.name()))
                        .with_note(format!("`{name}` is defined here"), Some(declared_at)),
                    );
                }
            }
        }

        if ret.is_none() && !as_statement {
            self.error(
                Diagnostic::new(format!("`{name}` returns nothing"), expr.span)
                    .with_label("so this call produces no value to use")
                    .with_note(format!("`{name}` is defined here"), Some(declared_at)),
            );
        }
        ret
    }

    fn expr(&mut self, expr: &Expr) -> Ty {
        let ty = match &expr.kind {
            ExprKind::Int(_) => Ty::Int,
            ExprKind::Str(_) => Ty::Str,
            ExprKind::Bool(_) => Ty::Bool,
            ExprKind::Var(name) => match self.lookup(name) {
                Some((ty, _)) => ty,
                None => {
                    let span = expr.span;
                    self.report_undeclared(name, || {
                        Diagnostic::new(format!("undeclared variable `{name}`"), span)
                            .with_label("not declared anywhere above this point")
                    });
                    Ty::Int
                }
            },
            ExprKind::Neg(operand) => {
                let inner = self.expr(operand);
                if inner != Ty::Int {
                    self.error(
                        Diagnostic::new(
                            format!("cannot apply `-` to {} value", inner.with_article()),
                            operand.span,
                        )
                        .with_label(format!("expected int, found {}", inner.name())),
                    );
                }
                Ty::Int
            }
            ExprKind::Bin { op, lhs, rhs } => {
                let lhs_ty = self.expr(lhs);
                let rhs_ty = self.expr(rhs);

                // Arithmetic is int-only in v0; report the offending operand.
                for (ty, operand) in [(lhs_ty, lhs), (rhs_ty, rhs)] {
                    if ty != Ty::Int {
                        self.error(
                            Diagnostic::new(
                                format!(
                                    "cannot apply `{}` to `{}` and `{}`",
                                    op.symbol(),
                                    lhs_ty.name(),
                                    rhs_ty.name()
                                ),
                                operand.span,
                            )
                            .with_label(format!("expected int, found {}", ty.name())),
                        );
                    }
                }

                if *op == BinOp::Div && matches!(rhs.kind, ExprKind::Int(0)) {
                    self.error(
                        Diagnostic::new("division by zero", rhs.span)
                            .with_label("this divisor is always zero"),
                    );
                }
                Ty::Int
            }
            ExprKind::Cmp { op, lhs, rhs } => {
                let lhs_ty = self.expr(lhs);
                let rhs_ty = self.expr(rhs);

                if lhs_ty != rhs_ty {
                    self.error(
                        Diagnostic::new(
                            format!(
                                "cannot compare `{}` with `{}`",
                                lhs_ty.name(),
                                rhs_ty.name()
                            ),
                            rhs.span,
                        )
                        .with_label(format!("expected {}, found {}", lhs_ty.name(), rhs_ty.name())),
                    );
                } else if lhs_ty == Ty::Str || (lhs_ty == Ty::Bool && op.is_ordering()) {
                    // `==` works on bools; ordering does not, and strings have
                    // no comparison at all without a runtime routine.
                    self.error(
                        Diagnostic::new(
                            format!("`{}` values cannot be compared with `{}`", lhs_ty.name(), op.symbol()),
                            expr.span,
                        )
                        .with_label(if lhs_ty == Ty::Str {
                            "strings support no comparisons yet".to_string()
                        } else {
                            "bools only support `==` and `!=`".to_string()
                        }),
                    );
                }
                Ty::Bool
            }
            ExprKind::Logic { op, lhs, rhs } => {
                // Both operands are checked even though only one may be
                // *evaluated*: short circuiting is a runtime matter, and a type
                // error in the right operand is a mistake either way.
                let lhs_ty = self.expr(lhs);
                let rhs_ty = self.expr(rhs);

                for (ty, operand) in [(lhs_ty, lhs), (rhs_ty, rhs)] {
                    if ty != Ty::Bool {
                        self.error(
                            Diagnostic::new(
                                format!(
                                    "cannot apply `{}` to `{}` and `{}`",
                                    op.symbol(),
                                    lhs_ty.name(),
                                    rhs_ty.name()
                                ),
                                operand.span,
                            )
                            .with_label(format!("expected bool, found {}", ty.name()))
                            .with_note(
                                format!(
                                    "`{}` combines two bools, as in `i < 10 {} ok`",
                                    op.symbol(),
                                    op.symbol()
                                ),
                                None,
                            ),
                        );
                    }
                }
                Ty::Bool
            }
            // A call in expression position must produce a value; `Int` is the
            // recovery type when it does not.
            ExprKind::Call { .. } => self.call(expr, false).unwrap_or(Ty::Int),
        };

        self.record(expr.id, ty)
    }
}

/// `1 argument` / `2 arguments`, so messages read like prose.
fn plural(count: usize, noun: &str) -> String {
    match (count, noun) {
        (1, "was") => "1 was".to_string(),
        (n, "was") => format!("{n} were"),
        (1, noun) => format!("1 {noun}"),
        (n, noun) => format!("{n} {noun}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;
    use crate::parser::parse;

    fn check_src(src: &str) -> Result<Types> {
        // Four is what every backend in the tree reports today.
        check(&parse(&lex(src)?)?, 4)
    }

    /// Wrap statements in a `main`, so the tests about statements stay about
    /// statements.
    fn check_main(body: &str) -> Result<Types> {
        check_src(&format!("fn main() {{\n{body}\n}}\n"))
    }

    fn errors_in_main(body: &str) -> Vec<Diagnostic> {
        check_main(body).unwrap_err()
    }

    // -- how many diagnostics, and in what order ---------------------------

    #[test]
    fn one_missing_declaration_is_one_diagnostic() {
        // The name is undeclared on both sides of the `=`, but the mistake is
        // the same one: a reader should be told once.
        let errors = errors_in_main("y = y + 1;");
        assert_eq!(errors.len(), 1, "{errors:#?}");
        assert!(errors[0].label.as_deref().unwrap().contains("assign to it"), "{errors:#?}");
    }

    #[test]
    fn a_name_mentioned_many_times_is_still_reported_once() {
        let errors = errors_in_main("print(nope);\nprint(nope);\nprint(nope);");
        assert_eq!(errors.len(), 1, "{errors:#?}");
    }

    #[test]
    fn each_function_gets_its_own_say_about_the_same_name() {
        // Two functions, two independent mistakes.
        let errors = check_src(
            "fn a() {\n  print(nope);\n}\nfn b() {\n  print(nope);\n}\nfn main() {\n  a();\n}",
        )
        .unwrap_err();
        assert_eq!(errors.len(), 2, "{errors:#?}");
    }

    #[test]
    fn diagnostics_are_reported_in_source_order() {
        // A statement's value is checked before its name, so without sorting
        // these would come out back to front.
        let errors = errors_in_main("int x = 1;\nx = \"a\";\nstring s = 1;\nprint(s + 1);");
        assert!(errors.len() > 1, "this program should produce several: {errors:#?}");
        let offsets: Vec<u32> = errors.iter().map(|d| d.span.offset).collect();
        let mut sorted = offsets.clone();
        sorted.sort_unstable();
        assert_eq!(offsets, sorted, "{errors:#?}");
    }

    #[test]
    fn a_call_statement_records_the_type_it_produced() {
        // Nothing reads it today, but every expression node has an entry and a
        // hole here would be a trap for whoever reads one next.
        let types = check_src(
            "fn label() -> string {\n  return \"hi\";\n}\nfn main() {\n  label();\n}",
        )
        .unwrap();
        let ast = parse(&lex("fn label() -> string {\n  return \"hi\";\n}\nfn main() {\n  label();\n}")
            .unwrap())
        .unwrap();
        let Stmt::Call(call) = &ast.functions[1].body.stmts[0] else { panic!("a call statement") };
        assert_eq!(types.of(call.id), Ty::Str);
    }

    #[test]
    fn accepts_the_sample_program() {
        assert!(
            check_main("int x = 10;\nint y = 20;\nstring s = \"hi\";\nprint(x + y);\nprint(s);")
                .is_ok()
        );
    }

    #[test]
    fn rejects_arithmetic_on_strings() {
        let errors = errors_in_main("string s = \"a\";\nprint(1 + s);");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("cannot apply `+`"));
    }

    #[test]
    fn rejects_a_mistyped_initializer() {
        assert!(errors_in_main("int x = \"nope\";")[0].message.contains("cannot initialize"));
    }

    #[test]
    fn rejects_undeclared_variables() {
        assert!(errors_in_main("print(nope);")[0].message.contains("undeclared variable `nope`"));
    }

    #[test]
    fn rejects_redeclaration_and_points_at_the_original() {
        let errors = errors_in_main("int x = 1;\nint x = 2;");
        assert!(errors[0].message.contains("already declared"));
        assert!(errors[0].note.as_ref().unwrap().1.is_some());
    }

    #[test]
    fn accepts_assignment_of_the_declared_type() {
        assert!(check_main("string s = \"a\";\ns = \"b\";\nprint(s);").is_ok());
        assert!(check_main("int n = 1;\nn = n * 2;\nprint(n);").is_ok());
    }

    #[test]
    fn rejects_assignment_of_the_wrong_type() {
        let errors = errors_in_main("int n = 1;\nn = \"two\";");
        assert!(errors[0].message.contains("cannot assign"), "{}", errors[0].message);
        assert!(errors[0].note.as_ref().unwrap().1.is_some());
    }

    #[test]
    fn rejects_assignment_to_an_undeclared_variable() {
        assert!(errors_in_main("nope = 1;")[0].message.contains("undeclared variable `nope`"));
    }

    #[test]
    fn accepts_bool_declarations_assignment_and_printing() {
        assert!(check_main("bool ready = true;\nready = false;\nprint(ready);").is_ok());
        assert!(check_main("print(true);").is_ok());
    }

    #[test]
    fn rejects_an_int_initializer_for_a_bool() {
        assert!(errors_in_main("bool ready = 1;")[0].message.contains("cannot initialize"));
    }

    #[test]
    fn rejects_a_bool_initializer_for_an_int() {
        assert!(errors_in_main("int n = true;")[0].message.contains("cannot initialize"));
    }

    #[test]
    fn rejects_assigning_a_bool_to_a_string() {
        assert!(errors_in_main("string s = \"hi\";\ns = true;")[0].message.contains("cannot assign"));
    }

    #[test]
    fn rejects_arithmetic_on_bools() {
        let errors = errors_in_main("bool ready = true;\nprint(ready + 1);");
        assert!(errors[0].message.contains("cannot apply `+`"), "{}", errors[0].message);
    }

    #[test]
    fn rejects_negating_a_bool() {
        let errors = errors_in_main("bool ready = true;\nprint(-ready);");
        assert!(errors[0].message.contains("cannot apply `-`"), "{}", errors[0].message);
    }

    #[test]
    fn a_comparison_produces_a_bool() {
        assert!(check_main("bool ok = 1 < 2;\nprint(ok);").is_ok());
        assert!(check_main("if (1 == 2) {\n  print(1);\n}").is_ok());
    }

    #[test]
    fn rejects_a_condition_that_is_not_a_bool() {
        for src in ["if (1) {\n}", "while (1) {\n}", "for (int i = 0; i; i = i + 1) {\n}"] {
            let errors = errors_in_main(src);
            assert!(errors[0].message.contains("must be a `bool`"), "{src}: {}", errors[0].message);
        }
    }

    #[test]
    fn rejects_comparing_different_types() {
        let errors = errors_in_main("string s = \"a\";\nprint(s == 1);");
        assert!(errors[0].message.contains("cannot compare"), "{}", errors[0].message);
    }

    #[test]
    fn rejects_ordering_comparisons_that_make_no_sense() {
        assert!(errors_in_main("print(true < false);")[0].message.contains("cannot be compared"));
        assert!(errors_in_main("print(\"a\" == \"b\");")[0].message.contains("cannot be compared"));
    }

    #[test]
    fn a_block_scopes_its_declarations() {
        let errors = errors_in_main("if (true) {\n  int inner = 1;\n}\nprint(inner);");
        assert!(errors[0].message.contains("undeclared variable `inner`"));
    }

    #[test]
    fn an_inner_block_may_shadow_an_outer_name() {
        assert!(
            check_main("int i = 1;\nif (true) {\n  string i = \"x\";\n  print(i);\n}\nprint(i);")
                .is_ok()
        );
    }

    #[test]
    fn a_for_variable_does_not_escape_its_loop() {
        assert!(
            check_main("for (int i = 0; i < 1; i = i + 1) {\n}\nfor (int i = 0; i < 1; i = i + 1) {\n}")
                .is_ok()
        );
        let errors = errors_in_main("for (int i = 0; i < 1; i = i + 1) {\n}\nprint(i);");
        assert!(errors[0].message.contains("undeclared variable `i`"));
    }

    // -- logical operators -------------------------------------------------

    #[test]
    fn logical_operators_take_bools_and_produce_one() {
        assert!(check_main("bool ok = true && false;\nprint(ok);").is_ok());
        assert!(check_main("int n = 5;\nif (n > 1 && n < 10) {\n  print(n);\n}").is_ok());
        assert!(check_main("bool a = true;\nwhile (a || 1 < 2) {\n  a = false;\n}").is_ok());
    }

    #[test]
    fn rejects_a_non_bool_operand_of_a_logical_operator() {
        for body in ["print(1 && true);", "print(true || 1);", "print(\"a\" && \"b\");"] {
            let errors = errors_in_main(body);
            assert!(errors[0].message.contains("cannot apply"), "{body}: {}", errors[0].message);
        }
    }

    #[test]
    fn a_mistake_in_the_right_operand_is_reported_even_though_it_may_not_run() {
        // Short circuiting decides what is *evaluated*, not what is checked.
        let errors = errors_in_main("print(false && nope);");
        assert!(errors[0].message.contains("undeclared variable `nope`"), "{errors:#?}");
    }

    #[test]
    fn a_logical_operator_is_a_bool_wherever_one_is_wanted() {
        assert!(check_main("bool b = 1 < 2 || 3 < 4;\nif (b && true) {\n}").is_ok());
        assert!(errors_in_main("int n = true && false;")[0].message.contains("cannot initialize"));
    }

    // -- break and continue ------------------------------------------------

    #[test]
    fn accepts_break_and_continue_inside_a_loop() {
        assert!(check_main("while (true) {\n  break;\n}").is_ok());
        assert!(check_main("for (int i = 0; i < 3; i = i + 1) {\n  continue;\n}").is_ok());
        // Nested inside an `if`, which is the usual way they are written.
        assert!(
            check_main("for (int i = 0; i < 3; i = i + 1) {\n  if (i == 1) {\n    continue;\n  }\n}")
                .is_ok()
        );
    }

    #[test]
    fn rejects_break_and_continue_outside_a_loop() {
        for (body, keyword) in [("break;", "break"), ("continue;", "continue")] {
            let errors = errors_in_main(body);
            assert!(
                errors[0].message.contains(&format!("`{keyword}` outside of a loop")),
                "{body}: {}",
                errors[0].message
            );
        }
        // An `if` is not a loop, and neither is a function body.
        assert!(errors_in_main("if (true) {\n  break;\n}")[0].message.contains("outside of a loop"));
    }

    #[test]
    fn a_loop_that_has_closed_no_longer_counts() {
        // The depth is decremented on the way out, so a `break` after the loop
        // is as wrong as one that was never inside it.
        let errors = errors_in_main("while (true) {\n  break;\n}\nbreak;");
        assert_eq!(errors.len(), 1, "{errors:#?}");
    }

    #[test]
    fn an_inner_loop_satisfies_break_for_a_body_nested_in_an_outer_one() {
        assert!(
            check_main(
                "while (true) {\n  if (false) {\n    while (true) {\n      break;\n    }\n  }\n  break;\n}"
            )
            .is_ok()
        );
    }

    #[test]
    fn rejects_division_by_zero() {
        assert!(errors_in_main("print(1 / 0);")[0].message.contains("division by zero"));
    }

    #[test]
    fn collects_several_errors() {
        assert_eq!(errors_in_main("print(a);\nprint(b);").len(), 2);
    }

    // -- functions ---------------------------------------------------------

    #[test]
    fn accepts_a_call_with_matching_arguments() {
        assert!(
            check_src(
                "fn add(int a, int b) -> int {\n  return a + b;\n}\n\
                 fn main() {\n  print(add(1, 2));\n}"
            )
            .is_ok()
        );
    }

    #[test]
    fn a_function_may_be_called_before_it_is_declared() {
        // This is what the first pass buys: `helper` is in the table before
        // `main`'s body is looked at.
        assert!(
            check_src(
                "fn main() {\n  print(helper());\n}\n\
                 fn helper() -> int {\n  return 1;\n}"
            )
            .is_ok()
        );
    }

    #[test]
    fn a_function_may_call_itself() {
        assert!(
            check_src(
                "fn fib(int n) -> int {\n  if (n < 2) {\n    return n;\n  } else {\n    \
                 return fib(n - 1) + fib(n - 2);\n  }\n}\n\
                 fn main() {\n  print(fib(10));\n}"
            )
            .is_ok()
        );
    }

    #[test]
    fn parameters_are_visible_in_the_body() {
        assert!(check_src("fn f(int a) {\n  print(a);\n}\nfn main() {\n  f(1);\n}").is_ok());
    }

    #[test]
    fn a_parameter_does_not_escape_its_function() {
        let errors =
            check_src("fn f(int a) {\n  print(a);\n}\nfn main() {\n  print(a);\n}").unwrap_err();
        assert!(errors[0].message.contains("undeclared variable `a`"));
    }

    #[test]
    fn rejects_a_local_that_collides_with_a_parameter() {
        let errors = check_src("fn f(int a) {\n  int a = 1;\n}\nfn main() {\n}").unwrap_err();
        assert!(errors[0].message.contains("already declared"), "{}", errors[0].message);
    }

    #[test]
    fn rejects_two_parameters_with_the_same_name() {
        let errors = check_src("fn f(int a, int a) {\n}\nfn main() {\n}").unwrap_err();
        assert!(errors[0].message.contains("already declared"), "{}", errors[0].message);
    }

    #[test]
    fn rejects_an_unknown_callee() {
        let errors = check_src("fn main() {\n  nope();\n}").unwrap_err();
        assert!(errors[0].message.contains("unknown function `nope`"));
    }

    #[test]
    fn rejects_the_wrong_number_of_arguments() {
        let errors =
            check_src("fn add(int a, int b) -> int {\n  return a + b;\n}\nfn main() {\n  print(add(1));\n}")
                .unwrap_err();
        assert!(errors[0].message.contains("takes 2 arguments"), "{}", errors[0].message);
    }

    #[test]
    fn rejects_an_argument_of_the_wrong_type() {
        let errors =
            check_src("fn f(int a) {\n}\nfn main() {\n  f(\"hi\");\n}").unwrap_err();
        assert!(errors[0].message.contains("cannot pass"), "{}", errors[0].message);
    }

    #[test]
    fn rejects_two_functions_with_the_same_name() {
        let errors = check_src("fn f() {\n}\nfn f() {\n}\nfn main() {\n}").unwrap_err();
        assert!(errors[0].message.contains("already defined"), "{}", errors[0].message);
        assert!(errors[0].note.as_ref().unwrap().1.is_some());
    }

    #[test]
    fn rejects_more_than_four_parameters() {
        let errors =
            check_src("fn f(int a, int b, int c, int d, int e) {\n}\nfn main() {\n}").unwrap_err();
        assert!(errors[0].message.contains("at most 4"), "{}", errors[0].message);
    }

    #[test]
    fn accepts_exactly_four_parameters() {
        assert!(check_src("fn f(int a, int b, int c, int d) {\n}\nfn main() {\n}").is_ok());
    }

    #[test]
    fn rejects_a_program_without_main() {
        let errors = check_src("fn f() {\n}").unwrap_err();
        assert!(errors[0].message.contains("no `main` function"), "{}", errors[0].message);
    }

    #[test]
    fn rejects_a_main_that_takes_parameters_or_returns() {
        assert!(
            check_src("fn main(int a) {\n}").unwrap_err()[0]
                .message
                .contains("must not take parameters")
        );
        assert!(
            check_src("fn main() -> int {\n  return 0;\n}").unwrap_err()[0]
                .message
                .contains("must not return a value")
        );
    }

    // -- returns -----------------------------------------------------------

    #[test]
    fn rejects_a_return_value_of_the_wrong_type() {
        let errors = check_src("fn f() -> int {\n  return \"hi\";\n}\nfn main() {\n}").unwrap_err();
        assert!(errors[0].message.contains("cannot return"), "{}", errors[0].message);
    }

    #[test]
    fn rejects_a_bare_return_from_a_function_with_a_return_type() {
        let errors = check_src("fn f() -> int {\n  return;\n}\nfn main() {\n}").unwrap_err();
        assert!(errors[0].message.contains("needs a value"), "{}", errors[0].message);
    }

    #[test]
    fn rejects_returning_a_value_from_a_void_function() {
        let errors = check_src("fn f() {\n  return 1;\n}\nfn main() {\n}").unwrap_err();
        assert!(errors[0].message.contains("returns nothing"), "{}", errors[0].message);
    }

    #[test]
    fn a_bare_return_is_fine_in_a_void_function() {
        assert!(check_src("fn f() {\n  return;\n}\nfn main() {\n}").is_ok());
    }

    #[test]
    fn rejects_a_body_that_can_finish_without_returning() {
        let errors =
            check_src("fn f(int n) -> int {\n  if (n > 0) {\n    return 1;\n  }\n}\nfn main() {\n}")
                .unwrap_err();
        assert!(errors[0].message.contains("may finish without returning"), "{}", errors[0].message);
    }

    #[test]
    fn both_arms_of_an_if_else_count_as_returning() {
        assert!(
            check_src(
                "fn f(int n) -> int {\n  if (n > 0) {\n    return 1;\n  } else {\n    \
                 return 2;\n  }\n}\nfn main() {\n}"
            )
            .is_ok()
        );
    }

    #[test]
    fn a_loop_is_never_assumed_to_run() {
        // Conservative on purpose: this program is in fact fine, but proving it
        // needs more than the syntax.
        let errors = check_src(
            "fn f() -> int {\n  while (true) {\n    return 1;\n  }\n}\nfn main() {\n}",
        )
        .unwrap_err();
        assert!(errors[0].message.contains("may finish without returning"));
    }

    // -- void in expression position ---------------------------------------

    #[test]
    fn a_void_call_is_a_statement_but_not_a_value() {
        assert!(check_src("fn greet() {\n}\nfn main() {\n  greet();\n}").is_ok());
        let errors = check_src("fn greet() {\n}\nfn main() {\n  int n = greet();\n}").unwrap_err();
        assert!(errors[0].message.contains("returns nothing"), "{}", errors[0].message);
    }

    #[test]
    fn a_returning_call_may_be_used_as_a_statement() {
        // The value is simply discarded, exactly as in C.
        assert!(
            check_src("fn f() -> int {\n  return 1;\n}\nfn main() {\n  f();\n}").is_ok()
        );
    }
}
