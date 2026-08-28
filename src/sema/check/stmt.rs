//! Checking statements: declarations, control flow, `print`, `return`.

use super::*;

impl FnChecker<'_, '_> {
    pub(super) fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Decl { id, ty, name, name_span, init } => {
                let declared = self.resolve(ty);
                self.record(*id, declared);
                let actual = self.value_of_type(init, declared);
                if !self.coerces(actual, declared) {
                    self.error(
                        Diagnostic::new(
                            format!(
                                "cannot initialize {} variable with {} value",
                                self.ty_article(declared),
                                self.ty_article(actual)
                            ),
                            init.span,
                        )
                        .with_label(format!(
                            "expected {}, found {}",
                            self.ty_name(declared),
                            self.ty_name(actual)
                        )),
                    );
                }
                self.declare(name, declared, *name_span, "a variable");
            }
            Stmt::Assign { target, value } => self.assign(target, value),
            Stmt::Push { span, target, value } => self.push_stmt(*span, target, value),
            Stmt::Print { parts, .. } => {
                for part in parts {
                    match part {
                        // Fixed at compile time: there is nothing to check.
                        PrintPart::Text(_) => {}
                        PrintPart::Value(value) => self.print_value(value),
                        PrintPart::Spec { spec, span, expr } => {
                            self.print_spec(*spec, *span, expr)
                        }
                    }
                }
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
            // The value is discarded, exactly as a call statement's is — and
            // like one, the type still goes in the table.
            Stmt::Match(expr) => {
                let ty = self.match_expr(expr, true).unwrap_or(Ty::Int);
                self.record(expr.id, ty);
            }
            Stmt::Return { span, value } => self.return_stmt(*span, value.as_ref()),
            Stmt::Break { span } => self.loop_jump(*span, "break", "leaves"),
            Stmt::Continue { span } => self.loop_jump(*span, "continue", "restarts"),
            // The result of a call statement is discarded, so a function that
            // returns nothing is exactly as welcome as one that returns a value.
            // The type still goes in the table: every expression node has an
            // entry, and a hole here would be a trap for whoever reads one next.
            // Either kind of call may be written for its effect, and both
            // accept a callee that produces nothing.
            Stmt::Call(call) => {
                let ty = match call.kind {
                    ExprKind::MethodCall { .. } => self.method_call(call, true),
                    _ => self.call(call, true),
                };
                let ty = ty.unwrap_or(Ty::Int);
                self.record(call.id, ty);
            }
        }
    }


    /// Check `push(xs, value);`.
    /// A value written on its own: the `x` in `print(x)`.
    ///
    /// Nothing claimed what it would be, so the only question is whether it has
    /// a rendering at all.
    pub(super) fn print_value(&mut self, value: &Expr) {
        let ty = self.expr(value);
        // A run of values has no rendering, and printing its address would
        // answer a question nobody asked.
        if ty.is_printable() {
            return;
        }
        // A run of values and an object are refused for the same reason but
        // answered differently: one has elements to loop over, the other has
        // fields to name.
        let (label, note) = match ty {
            Ty::Class(_) => (
                "`print` writes one value, and an object is several",
                "write the fields you meant, one at a time",
            ),
            _ => (
                "`print` writes one value, and this is many",
                "write the elements in a loop instead",
            ),
        };
        self.error(
            Diagnostic::new(format!("cannot print {}", self.ty_article(ty)), value.span)
                .with_label(label)
                .with_note(note, None),
        );
    }

    /// A value a specifier claimed the type of: the `%d` and the `x` in
    /// `print("n = %d", x)`.
    ///
    /// The claim is checked exactly, and no conversion is offered to rescue it:
    /// `%d` on a `string` is the same mistake as `int n = s;` and gets the same
    /// answer, because a format string is the one place a number most wants to
    /// become text by itself. `string(n)` is still written out.
    ///
    /// The caret goes on the *value*, since that is the half a reader would
    /// change; the note points back at the specifier that asked for something
    /// else.
    pub(super) fn print_spec(&mut self, spec: Spec, at: Span, value: &Expr) {
        let ty = self.expr(value);
        if spec.accepts(ty) {
            return;
        }
        self.error(
            Diagnostic::new(
                format!("cannot write {} with `%{}`", self.ty_article(ty), spec.letter()),
                value.span,
            )
            .with_label(format!("`%{}` writes {}", spec.letter(), spec.writes()))
            .with_note("this is the specifier it has to match", Some(at)),
        );
    }


    /// A loop's body, checked with one more loop open around it.
    pub(super) fn loop_body(&mut self, body: &Block) {
        self.loop_depth += 1;
        self.block(body);
        self.loop_depth -= 1;
    }

    /// `break` and `continue` need a loop to talk about. `verb` completes
    /// "there is no loop this ... " for the one being checked.
    pub(super) fn loop_jump(&mut self, span: Span, keyword: &str, verb: &str) {
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

    pub(super) fn return_stmt(&mut self, span: Span, value: Option<&Expr>) {
        match (self.ret, value) {
            (Some(expected), Some(expr)) => {
                let actual = self.value_of_type(expr, expected);
                if !self.coerces(actual, expected) {
                    self.error(
                        Diagnostic::new(
                            format!(
                                "cannot return {} value from a function returning {}",
                                self.ty_article(actual),
                                self.ty_article(expected)
                            ),
                            expr.span,
                        )
                        .with_label(format!(
                            "expected {}, found {}",
                            self.ty_name(expected),
                            self.ty_name(actual)
                        ))
                        .with_note("declared here", Some(self.ret_span)),
                    );
                }
            }
            (Some(expected), None) => self.error(
                Diagnostic::new("this `return` needs a value", span)
                    .with_label(format!("expected {}", self.ty_article(expected)))
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


}
