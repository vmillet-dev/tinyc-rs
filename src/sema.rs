//! Stage 3: type checking.
//!
//! Walks the AST with a symbol table and records the type of every expression
//! node in a side table keyed by [`NodeId`], so the tree itself stays immutable.
//! Unlike the lexer and parser, this stage collects *all* errors before giving
//! up: statements are independent enough that later ones are still worth
//! checking.

use std::collections::HashMap;

use crate::ast::{BinOp, Expr, ExprKind, NodeId, Program, Stmt, Ty};
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
        scope: HashMap::new(),
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
    /// Variable name -> (type, span of its declaration).
    scope: HashMap<String, (Ty, Span)>,
    errors: Vec<Diagnostic>,
}

impl Checker {
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

                if let Some((_, previous)) = self.scope.get(name) {
                    self.errors.push(
                        Diagnostic::new(format!("`{name}` is already declared"), *name_span)
                            .with_label("declared a second time here")
                            .with_note("previous declaration", Some(*previous)),
                    );
                } else {
                    self.scope.insert(name.clone(), (*ty, *name_span));
                }
            }
            Stmt::Print { value, .. } => {
                self.expr(value);
            }
        }
    }

    fn expr(&mut self, expr: &Expr) -> Ty {
        let ty = match &expr.kind {
            ExprKind::Int(_) => Ty::Int,
            ExprKind::Str(_) => Ty::Str,
            ExprKind::Var(name) => match self.scope.get(name) {
                Some((ty, _)) => *ty,
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
