//! Stage 3: type checking.
//!
//! Walks the AST with a symbol table and records the type of every expression
//! node in a side table keyed by [`NodeId`], so the tree itself stays immutable.
//! Unlike the lexer and parser, this stage collects *all* errors before giving
//! up: statements are independent enough that later ones are still worth
//! checking.

use std::collections::HashMap;

use crate::ast::{BinOp, Block, Expr, ExprKind, NodeId, Program, Stmt, Ty};
use crate::diag::{Diagnostic, Result, Span};

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

pub fn check(program: &Program) -> Result<Types> {
    let mut checker = Checker {
        // `Int` is the recovery type: an expression that failed to check is
        // treated as an int so a single mistake does not cascade.
        types: Types { expr_ty: vec![Ty::Int; program.node_count] },
        scopes: vec![HashMap::new()],
        errors: Vec::new(),
    };

    for stmt in &program.stmts {
        checker.stmt(stmt);
    }

    if checker.errors.is_empty() {
        Ok(checker.types)
    } else {
        Err(checker.errors)
    }
}

struct Checker {
    types: Types,
    /// One map per open block, innermost last. A name is looked up from the
    /// inside out, and a block's declarations disappear when it closes.
    scopes: Vec<HashMap<String, (Ty, Span)>>,
    errors: Vec<Diagnostic>,
}

impl Checker {
    /// The type and declaration span of a visible variable.
    fn lookup(&self, name: &str) -> Option<(Ty, Span)> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name).copied())
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
            self.errors.push(
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
                    self.errors.push(
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

                // Only the innermost scope is consulted: an inner block may
                // shadow an outer name, but may not declare the same one twice.
                let innermost = self.scopes.last_mut().expect("a scope is always open");
                if let Some((_, previous)) = innermost.get(name) {
                    let previous = *previous;
                    self.errors.push(
                        Diagnostic::new(format!("`{name}` is already declared"), *name_span)
                            .with_label("declared a second time here")
                            .with_note("previous declaration", Some(previous)),
                    );
                } else {
                    innermost.insert(name.clone(), (*ty, *name_span));
                }
            }
            Stmt::Assign { name, name_span, value } => {
                let actual = self.expr(value);
                match self.lookup(name) {
                    // A variable keeps the type it was declared with.
                    Some((declared, declared_span)) => {
                        if actual != declared {
                            self.errors.push(
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
                                .with_note(
                                    format!("`{name}` was declared here"),
                                    Some(declared_span),
                                ),
                            );
                        }
                    }
                    None => self.errors.push(
                        Diagnostic::new(format!("undeclared variable `{name}`"), *name_span)
                            .with_label("assign to it after declaring it")
                            .with_note(
                                format!("a declaration gives it a type, as in `int {name} = 0;`"),
                                None,
                            ),
                    ),
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
                self.block(body);
            }
            Stmt::For { init, cond, step, body } => {
                // The initialiser's variable is visible to the condition, the
                // step and the body, but not after the loop — so the whole
                // `for` gets a scope of its own.
                self.scopes.push(HashMap::new());
                self.stmt(init);
                self.condition(cond, "for");
                self.stmt(step);
                self.block(body);
                self.scopes.pop();
            }
        }
    }

    fn expr(&mut self, expr: &Expr) -> Ty {
        let ty = match &expr.kind {
            ExprKind::Int(_) => Ty::Int,
            ExprKind::Str(_) => Ty::Str,
            ExprKind::Bool(_) => Ty::Bool,
            ExprKind::Var(name) => match self.lookup(name) {
                Some((ty, _)) => ty,
                None => {
                    self.errors.push(
                        Diagnostic::new(format!("undeclared variable `{name}`"), expr.span)
                            .with_label("not declared anywhere above this point"),
                    );
                    Ty::Int
                }
            },
            ExprKind::Neg(operand) => {
                let inner = self.expr(operand);
                if inner != Ty::Int {
                    self.errors.push(
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
                        self.errors.push(
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
                    self.errors.push(
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
                    self.errors.push(
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
                    self.errors.push(
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
        };

        self.types.expr_ty[expr.id.0 as usize] = ty;
        ty
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;
    use crate::parser::parse;

    fn check_src(src: &str) -> Result<Types> {
        check(&parse(&lex(src)?)?)
    }

    #[test]
    fn accepts_the_sample_program() {
        assert!(
            check_src("int x = 10;\nint y = 20;\nstring s = \"hi\";\nprint(x + y);\nprint(s);\n")
                .is_ok()
        );
    }

    #[test]
    fn rejects_arithmetic_on_strings() {
        let src = "string s = \"a\";\nprint(1 + s);\n";
        let errors = check_src(src).unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("cannot apply `+`"));
        assert_eq!(errors[0].span, Span::new(26, 1)); // the `s` operand
    }

    #[test]
    fn rejects_a_mistyped_initializer() {
        let errors = check_src("int x = \"nope\";").unwrap_err();
        assert!(errors[0].message.contains("cannot initialize"));
    }

    #[test]
    fn rejects_undeclared_variables() {
        let errors = check_src("print(nope);").unwrap_err();
        assert!(errors[0].message.contains("undeclared variable `nope`"));
    }

    #[test]
    fn rejects_redeclaration_and_points_at_the_original() {
        let errors = check_src("int x = 1;\nint x = 2;").unwrap_err();
        assert!(errors[0].message.contains("already declared"));
        assert_eq!(errors[0].note.as_ref().unwrap().1, Some(Span::new(4, 1)));
    }

    #[test]
    fn accepts_assignment_of_the_declared_type() {
        assert!(check_src("string s = \"a\";\ns = \"b\";\nprint(s);").is_ok());
        assert!(check_src("int n = 1;\nn = n * 2;\nprint(n);").is_ok());
    }

    #[test]
    fn rejects_assignment_of_the_wrong_type() {
        let errors = check_src("int n = 1;\nn = \"two\";").unwrap_err();
        assert!(errors[0].message.contains("cannot assign"), "{}", errors[0].message);
        // The note points back at the declaration that fixed the type.
        assert_eq!(errors[0].note.as_ref().unwrap().1, Some(Span::new(4, 1)));
    }

    #[test]
    fn rejects_assignment_to_an_undeclared_variable() {
        let errors = check_src("nope = 1;").unwrap_err();
        assert!(errors[0].message.contains("undeclared variable `nope`"));
        assert_eq!(errors[0].span, Span::new(0, 4));
    }

    #[test]
    fn accepts_bool_declarations_assignment_and_printing() {
        assert!(check_src("bool ready = true;\nready = false;\nprint(ready);").is_ok());
        assert!(check_src("print(true);").is_ok());
    }

    #[test]
    fn rejects_an_int_initializer_for_a_bool() {
        let errors = check_src("bool ready = 1;").unwrap_err();
        assert!(errors[0].message.contains("cannot initialize"), "{}", errors[0].message);
        assert_eq!(errors[0].span, Span::new(13, 1));
    }

    #[test]
    fn rejects_a_bool_initializer_for_an_int() {
        let errors = check_src("int n = true;").unwrap_err();
        assert!(errors[0].message.contains("cannot initialize"), "{}", errors[0].message);
    }

    #[test]
    fn rejects_assigning_a_bool_to_a_string() {
        let errors = check_src("string s = \"hi\";\ns = true;").unwrap_err();
        assert!(errors[0].message.contains("cannot assign"), "{}", errors[0].message);
    }

    #[test]
    fn rejects_arithmetic_on_bools() {
        let errors = check_src("bool ready = true;\nprint(ready + 1);").unwrap_err();
        assert!(errors[0].message.contains("cannot apply `+`"), "{}", errors[0].message);
        assert_eq!(errors[0].span, Span::new(25, 5)); // the `ready` operand
    }

    #[test]
    fn rejects_negating_a_bool() {
        let errors = check_src("bool ready = true;\nprint(-ready);").unwrap_err();
        assert!(errors[0].message.contains("cannot apply `-`"), "{}", errors[0].message);
    }

    #[test]
    fn a_comparison_produces_a_bool() {
        assert!(check_src("bool ok = 1 < 2;\nprint(ok);").is_ok());
        assert!(check_src("if (1 == 2) {\n  print(1);\n}").is_ok());
    }

    #[test]
    fn rejects_a_condition_that_is_not_a_bool() {
        for src in ["if (1) {\n}", "while (1) {\n}", "for (int i = 0; i; i = i + 1) {\n}"] {
            let errors = check_src(src).unwrap_err();
            assert!(errors[0].message.contains("must be a `bool`"), "{src}: {}", errors[0].message);
        }
    }

    #[test]
    fn rejects_comparing_different_types() {
        let errors = check_src("string s = \"a\";\nprint(s == 1);").unwrap_err();
        assert!(errors[0].message.contains("cannot compare"), "{}", errors[0].message);
    }

    #[test]
    fn rejects_ordering_comparisons_that_make_no_sense() {
        let bools = check_src("print(true < false);").unwrap_err();
        assert!(bools[0].message.contains("cannot be compared"), "{}", bools[0].message);

        let strings = check_src("print(\"a\" == \"b\");").unwrap_err();
        assert!(strings[0].message.contains("cannot be compared"), "{}", strings[0].message);
    }

    #[test]
    fn a_block_scopes_its_declarations() {
        // `inner` does not survive the block it was declared in.
        let errors = check_src("if (true) {\n  int inner = 1;\n}\nprint(inner);").unwrap_err();
        assert!(errors[0].message.contains("undeclared variable `inner`"));
    }

    #[test]
    fn an_inner_block_may_shadow_an_outer_name() {
        assert!(check_src("int i = 1;\nif (true) {\n  string i = \"x\";\n  print(i);\n}\nprint(i);").is_ok());
    }

    #[test]
    fn a_for_variable_does_not_escape_its_loop() {
        assert!(check_src("for (int i = 0; i < 1; i = i + 1) {\n}\nfor (int i = 0; i < 1; i = i + 1) {\n}").is_ok());
        let errors = check_src("for (int i = 0; i < 1; i = i + 1) {\n}\nprint(i);").unwrap_err();
        assert!(errors[0].message.contains("undeclared variable `i`"));
    }

    #[test]
    fn rejects_redeclaration_in_the_same_block_only() {
        assert!(check_src("int x = 1;\nx = 2;\nprint(x);").is_ok());
        let errors = check_src("int x = 1;\nint x = 2;").unwrap_err();
        assert!(errors[0].message.contains("already declared"));
    }

    #[test]
    fn rejects_division_by_zero() {
        let errors = check_src("print(1 / 0);").unwrap_err();
        assert!(errors[0].message.contains("division by zero"));
    }

    #[test]
    fn collects_several_errors() {
        let errors = check_src("print(a);\nprint(b);\n").unwrap_err();
        assert_eq!(errors.len(), 2);
    }
}
