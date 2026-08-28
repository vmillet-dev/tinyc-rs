//! Checking expressions: operators, calls, conversions, indexing.

use super::*;

impl FnChecker<'_, '_> {
    /// Reject arithmetic the machine could not perform, when the operands are
    /// known here and now.
    ///
    /// This is the compile-time half of a pair. What it catches, it catches
    /// before the program is ever built; what it cannot see — an operand that
    /// is a variable, a parameter or a call — becomes a guard in the emitted
    /// code instead, and reports at the same moment with the same wording. The
    /// language has one rule, checked in whichever of the two places can.
    pub(super) fn check_arithmetic(&mut self, expr: &Expr, op: BinOp, lhs: &Expr, rhs: &Expr) {
        // A zero divisor is knowable from the right operand on its own —
        // whatever is being divided, there is no answer — and it is a different
        // mistake from a result that came out too large, so it is said
        // differently and pointed at the divisor rather than the whole
        // operation.
        if op.divides() && const_int(rhs) == Some(0) {
            self.error(
                Diagnostic::new("division by zero", rhs.span)
                    .with_label("this divisor is always zero"),
            );
            return;
        }

        // Anything else needs both operands. An operand that is itself a
        // mistake was reported where it was made, and answers `None` here, so
        // nothing is said twice.
        let (Some(a), Some(b)) = (const_int(lhs), const_int(rhs)) else { return };
        if op.apply(a, b).is_some() {
            return;
        }

        let label = match op.apply_exact(a, b) {
            Some(exact) if i64::try_from(exact).is_err() => {
                format!("`{exact}` does not fit in an `int`")
            }
            // `MIN % -1` is 0 on paper, and lands here anyway: the machine
            // reaches that 0 through the division whose quotient does not fit.
            _ => "the machine cannot perform this operation".to_string(),
        };
        self.error(
            Diagnostic::new(format!("this {} overflows an `int`", op.noun()), expr.span)
                .with_label(label)
                .with_note(
                    format!("`int` values must fit in {}..={}", i64::MIN, i64::MAX),
                    None,
                ),
        );
    }


    /// Check `receiver.method(args)` and answer the type it produces.
    ///
    /// `as_statement` says whether producing nothing is acceptable here, the
    /// same question [`Self::call`] asks of a `void` function.
    pub(super) fn method_call(&mut self, expr: &Expr, as_statement: bool) -> Option<Ty> {
        let ExprKind::MethodCall { receiver, name, name_span, args } = &expr.kind else {
            unreachable!("the caller matched a method call");
        };

        let of = self.expr(receiver);
        // Arguments are checked whatever the receiver turns out to be, so their
        // own mistakes are reported either way.
        let actual: Vec<Ty> = args.iter().map(|arg| self.expr(arg)).collect();

        let Ty::Class(id) = of else {
            self.error(
                Diagnostic::new(
                    format!("cannot call a method on {}", self.ty_article(of)),
                    *name_span,
                )
                .with_label("only an object has methods"),
            );
            return Some(Ty::Int);
        };

        let class = self.shared.declared.class(id);
        let Some(method) = class.method(name) else {
            let known: Vec<&str> = class.methods.iter().map(|m| m.name.as_str()).collect();
            let diagnostic = no_such_member(&class.name, "method", name, *name_span, &known);
            self.error(diagnostic);
            return Some(Ty::Int);
        };

        // The receiver is parameter zero, so the written arguments line up with
        // everything after it.
        let (expected, ret) = (method.params[1..].to_vec(), method.ret);
        if expected.len() != args.len() {
            self.error(
                Diagnostic::new(
                    format!(
                        "`{name}` takes {}, but {} supplied",
                        plural(expected.len(), "argument"),
                        plural(args.len(), "was")
                    ),
                    expr.span,
                )
                .with_label(format!("expected {} here", plural(expected.len(), "argument"))),
            );
        } else {
            for ((want, found), arg) in expected.iter().zip(&actual).zip(args) {
                if !self.coerces(*found, *want) {
                    self.error(
                        Diagnostic::new(
                            format!(
                                "cannot pass {} value where {} is expected",
                                self.ty_article(*found),
                                self.ty_article(*want)
                            ),
                            arg.span,
                        )
                        .with_label(format!(
                            "expected {}, found {}",
                            self.ty_name(*want),
                            self.ty_name(*found)
                        )),
                    );
                }
            }
        }

        if ret.is_none() && !as_statement {
            self.error(
                Diagnostic::new(format!("`{name}` returns nothing"), expr.span)
                    .with_label("so this call produces no value to use"),
            );
        }
        ret
    }

    /// Check `int(c)` and its relatives, and answer the type produced.
    ///
    /// The whole point of the form is that these are the *only* conversions in
    /// the language: nothing widens, narrows or stringifies on its own, so this
    /// list is exhaustive and the refusal below can say so.
    pub(super) fn convert(&mut self, to: Prim, value: &Expr, span: Span) -> Ty {
        let from = self.expr(value);
        let target = to.ty();

        match (from, target) {
            // A character's code point, and the character with that code point.
            // Two directions, spelled apart, so neither can happen by accident.
            (Ty::Char, Ty::Int) => {}
            // Text back into a number, which is what a line of input is for.
            // Nothing is guessed: an optional `-`, then digits, and the program
            // stops on anything else rather than settling for a zero.
            (Ty::Str, Ty::Int) => {}
            // The two numbers, in both directions, and neither happens on its
            // own. `float(n)` is exact for every `int` up to 2^53 and rounds
            // above it; `int(f)` throws the fraction away rather than rounding,
            // and stops the program where the float has no `int` at all.
            (Ty::Int, Ty::Float) => {}
            (Ty::Float, Ty::Int) => {
                // A constant is settled here, exactly as a constant character
                // is: what reaches the emitted code is only ever a value the
                // running program alone knows.
                if let Some(at) = const_float(value)
                    && !fits_in_an_int(at)
                {
                    self.error(
                        Diagnostic::new(format!("`{at}` has no `int`"), value.span).with_label(
                            format!("`int` values must fit in {}..={}", i64::MIN, i64::MAX),
                        ),
                    );
                }
            }
            // Into a string: a character on its own, or a number written out in
            // decimal. Both exist because `+` converts nothing, so a message
            // with a value in it has to say where the value became text.
            (Ty::Char | Ty::Int, Ty::Str) => {}
            // And a whole list of characters at once, which is how a string is
            // built a character at a time without paying for a new string on
            // every one — see the note on `+` in `docs/architecture.md`.
            (Ty::List(id), Ty::Str)
                if self.shared.declared.table.element(id) == Ty::Char => {}
            (Ty::Int, Ty::Char) => {
                // A constant that names no character is settled here, exactly
                // as a constant index out of range is: what reaches the emitted
                // code is only ever a value the running program alone knows.
                if let Some(at) = const_int(value)
                    && !is_scalar_value(at)
                {
                    self.error(
                        Diagnostic::new(format!("`{at}` is not a character"), value.span)
                            .with_label(scalar_range_label(at))
                            .with_note(
                                "a character is a Unicode scalar value, and only some numbers name one",
                                None,
                            ),
                    );
                }
            }
            // An identity conversion is not wrong so much as confused: it
            // reads as if something happened.
            (from, target) if from == target => {
                self.error(
                    Diagnostic::new(
                        format!("this is already {}", self.ty_article(target)),
                        span,
                    )
                    .with_label("a conversion to its own type does nothing"),
                );
            }
            _ => {
                // Writing a float out is `print`'s job, and the note says so
                // rather than leaving a reader to conclude from a list that a
                // float cannot be written at all. A conversion would have to
                // settle how many digits a float turns into, once and for the
                // whole language, and there is no good answer to that.
                let note = match (from, target) {
                    (Ty::Float, Ty::Str) => "a float is written by `print` — `println(f)`, or \
                                             `%f` inside a format",
                    _ => CONVERSIONS,
                };
                self.error(
                    Diagnostic::new(
                        format!(
                            "there is no conversion from `{}` to `{}`",
                            self.ty_name(from),
                            self.ty_name(target)
                        ),
                        span,
                    )
                    .with_label(format!("this is {}", self.ty_article(from)))
                    .with_note(note, None),
                );
            }
        }
        target
    }

    /// Check an index expression: it must be an `int`, and when it is a
    /// constant it must be one the array has.
    ///
    /// This is where the safety is bought. A literal index is settled now and
    /// costs nothing at run time; anything else becomes a check in the emitted
    /// code. Neither can reach past the end.
    pub(super) fn index(&mut self, array: Option<Ty>, index: &Expr) {
        let ty = self.expr(index);
        if ty != Ty::Int {
            self.error(
                Diagnostic::new(
                    format!("cannot index with {}", self.ty_article(ty)),
                    index.span,
                )
                .with_label(format!("expected int, found {}", self.ty_name(ty))),
            );
            return;
        }

        let Some(Ty::Array(id)) = array else { return };
        let len = self.shared.declared.table.array(id).len;
        let Some(at) = const_int(index) else { return };
        if at < 0 || at >= i64::from(len) {
            self.error(
                Diagnostic::new(format!("index `{at}` is out of bounds"), index.span)
                    .with_label(format!(
                        "this array holds {len}, so the last index is {}",
                        len - 1
                    ))
                    .with_note("an index the compiler can see is checked here rather than guarded at run time", None),
            );
        }
    }


    /// Check a call and answer the type it produces, or `None` when the callee
    /// returns nothing.
    ///
    /// `as_statement` says whether "nothing" is acceptable here: it is for
    /// `greet("hi");`, it is not for `int n = greet("hi");`.
    pub(super) fn call(&mut self, expr: &Expr, as_statement: bool) -> Option<Ty> {
        let ExprKind::Call { name, name_span, args } = &expr.kind else {
            unreachable!("the parser only builds `Stmt::Call` around a call");
        };

        // Arguments are checked first, so their own mistakes are reported even
        // when the callee turns out not to exist — but each against the
        // parameter it is going to, so `f([1, 2])` builds a list when `f` wants
        // one. An unknown callee has no parameters to check against, and the
        // arguments are still worth checking on their own.
        let wanted: Vec<Option<Ty>> = match self.signature(name) {
            Some(signature) if signature.params.len() == args.len() => {
                signature.params.iter().copied().map(Some).collect()
            }
            _ => vec![None; args.len()],
        };
        let actual: Vec<Ty> = args
            .iter()
            .zip(&wanted)
            .map(|(arg, wanted)| match wanted {
                Some(wanted) => self.value_of_type(arg, *wanted),
                None => self.expr(arg),
            })
            .collect();

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
                .with_note(came_from(name, declared_at).0, declared_at),
            );
        } else {
            for ((expected, found), arg) in params.iter().zip(&actual).zip(args) {
                if !self.coerces(*found, *expected) {
                    self.error(
                        Diagnostic::new(
                            format!(
                                "cannot pass {} value where {} is expected",
                                self.ty_article(*found),
                                self.ty_article(*expected)
                            ),
                            arg.span,
                        )
                        .with_label(format!(
                            "expected {}, found {}",
                            self.ty_name(*expected),
                            self.ty_name(*found)
                        ))
                        .with_note(came_from(name, declared_at).0, declared_at),
                    );
                }
            }
        }

        if ret.is_none() && !as_statement {
            self.error(
                Diagnostic::new(format!("`{name}` returns nothing"), expr.span)
                    .with_label("so this call produces no value to use")
                    .with_note(came_from(name, declared_at).0, declared_at),
            );
        }
        ret
    }

    pub(super) fn expr(&mut self, expr: &Expr) -> Ty {
        let ty = match &expr.kind {
            ExprKind::Int(_) => Ty::Int,
            ExprKind::Str(_) => Ty::Str,
            ExprKind::Char(_) => Ty::Char,
            ExprKind::Bool(_) => Ty::Bool,
            ExprKind::Float(_) => Ty::Float,
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
            // Unary minus keeps the type it was given, which is what makes
            // `-1.5` a float literal in every way that matters.
            ExprKind::Neg(operand) => {
                let inner = self.expr(operand);
                if !matches!(inner, Ty::Int | Ty::Float) {
                    self.error(
                        Diagnostic::new(
                            format!("cannot apply `-` to {} value", self.ty_article(inner)),
                            operand.span,
                        )
                        .with_label(format!(
                            "expected int or float, found {}",
                            self.ty_name(inner)
                        )),
                    );
                    return self.record(expr.id, Ty::Int);
                }
                inner
            }
            ExprKind::Not(operand) => {
                let inner = self.expr(operand);
                if inner != Ty::Bool {
                    self.error(
                        Diagnostic::new(
                            format!("cannot apply `!` to {} value", self.ty_article(inner)),
                            operand.span,
                        )
                        .with_label(format!("expected bool, found {}", self.ty_name(inner)))
                        // `!n` on an int is the mistake this catches, and it is
                        // almost always a habit from a language with truthiness.
                        .with_note("`!` negates a bool; there is no implicit truth test", None),
                    );
                }
                Ty::Bool
            }
            ExprKind::Bin { op, lhs, rhs } => {
                let lhs_ty = self.expr(lhs);
                let rhs_ty = self.expr(rhs);

                // `+` joins two strings. It is the one operator with a second
                // meaning, and it gets one because there is no other operator a
                // reader would look for: joining is not "adding" anything else.
                if *op == BinOp::Add && (lhs_ty == Ty::Str || rhs_ty == Ty::Str) {
                    if lhs_ty != Ty::Str || rhs_ty != Ty::Str {
                        let (ty, operand) =
                            if lhs_ty == Ty::Str { (rhs_ty, rhs) } else { (lhs_ty, lhs) };
                        self.error(
                            Diagnostic::new(
                                format!(
                                    "cannot apply `+` to `{}` and `{}`",
                                    self.ty_name(lhs_ty),
                                    self.ty_name(rhs_ty)
                                ),
                                operand.span,
                            )
                            .with_label(format!("expected string, found {}", self.ty_name(ty)))
                            // The mistake this catches is `"n = " + n`, which
                            // every language with a looser `+` would accept —
                            // so the note says how to write what was meant.
                            .with_note(
                                "`+` joins two strings; `string(n)` makes one out of a number, \
                                 and `string(c)` out of a character",
                                None,
                            ),
                        );
                    }
                    return self.record(expr.id, Ty::Str);
                }

                // Everything else is arithmetic on two numbers of the *same*
                // kind. Nothing widens on its own, so the left operand settles
                // which kind this is and the right one has to agree; report
                // whichever does not.
                let expected = match lhs_ty {
                    Ty::Int | Ty::Float => lhs_ty,
                    _ => Ty::Int,
                };
                for (ty, operand) in [(lhs_ty, lhs), (rhs_ty, rhs)] {
                    if ty == expected {
                        continue;
                    }
                    let mut diagnostic = Diagnostic::new(
                        format!(
                            "cannot apply `{}` to `{}` and `{}`",
                            op.symbol(),
                            self.ty_name(lhs_ty),
                            self.ty_name(rhs_ty)
                        ),
                        operand.span,
                    )
                    .with_label(format!(
                        "expected {}, found {}",
                        self.ty_name(expected),
                        self.ty_name(ty)
                    ));
                    // A character is a character, not a small number. The
                    // way to do arithmetic on one is to say so.
                    if ty == Ty::Char {
                        diagnostic = diagnostic.with_note(
                            "`int(c)` is a character's code point, and `char(n)` goes back",
                            None,
                        );
                    }
                    // The same bargain one type along: an `int` and a `float`
                    // are both numbers and still do not mix, because a
                    // conversion that happened on its own would be the one
                    // place in the language where precision went quietly.
                    if matches!(ty, Ty::Int | Ty::Float) {
                        diagnostic = diagnostic.with_note(
                            "`float(n)` makes a float out of an int, and `int(f)` goes back",
                            None,
                        );
                    }
                    self.error(diagnostic);
                }

                // `%` is what the *other* half of an `idiv` answers, and a
                // float division has no other half. C spells the operation
                // `fmod` and calls it a function rather than an operator, which
                // is the honest name for it: TinyC says it has none instead of
                // quietly meaning something else by `%`.
                if expected == Ty::Float && *op == BinOp::Rem {
                    self.error(
                        Diagnostic::new("`%` has no meaning on `float`", expr.span)
                            .with_label("a remainder counts whole divisors")
                            .with_note(
                                "`%` is an `int` operator; there is no floating-point \
                                 remainder in TinyC",
                                None,
                            ),
                    );
                }

                // Only an `int` can overflow, and only an `int` has a division
                // with no answer at all. Float arithmetic answers an infinity
                // or a NaN rather than refusing, so there is nothing here to
                // reject — and nothing for the backend to guard.
                if expected == Ty::Int {
                    self.check_arithmetic(expr, *op, lhs, rhs);
                }

                // Where the operands agreed, this is their type. Where they did
                // not, it is a guess made only so that the rest of the checking
                // stays about the rest of the program: a mixed pair was meant
                // to be the float one, because that is the operand a `float(n)`
                // would have been written around. Guessing the other way would
                // report the same mistake twice — once here and once at the
                // declaration it was written into.
                match lhs_ty == Ty::Float || rhs_ty == Ty::Float {
                    true => Ty::Float,
                    false => expected,
                }
            }
            ExprKind::Cmp { op, lhs, rhs } => {
                let lhs_ty = self.expr(lhs);
                let rhs_ty = self.expr(rhs);

                if lhs_ty != rhs_ty {
                    self.error(
                        Diagnostic::new(
                            format!(
                                "cannot compare `{}` with `{}`",
                                self.ty_name(lhs_ty),
                                self.ty_name(rhs_ty)
                            ),
                            rhs.span,
                        )
                        .with_label(format!("expected {}, found {}", self.ty_name(lhs_ty), self.ty_name(rhs_ty))),
                    );
                } else if !lhs_ty.has_equality(&self.shared.declared.table) || (op.is_ordering() && !lhs_ty.is_ordered()) {
                    // Ints and characters are ordered. Everything else that can
                    // be compared at all answers only `==`, since that is a
                    // question about *which value it is*; an array or an object
                    // answers nothing, because comparing addresses would quietly
                    // answer something else again.
                    let label = if !lhs_ty.has_equality(&self.shared.declared.table) {
                        format!(
                            "comparing two `{}` values would compare addresses, not contents",
                            self.ty_name(lhs_ty)
                        )
                    } else {
                        format!("`{}` values only support `==` and `!=`", self.ty_name(lhs_ty))
                    };
                    let mut diagnostic = Diagnostic::new(
                        format!("`{}` values cannot be compared with `{}`", self.ty_name(lhs_ty), op.symbol()),
                        expr.span,
                    )
                    .with_label(label);
                    // Ordering strings is the one refusal a reader is likely to
                    // argue with, so it says why rather than only that.
                    if lhs_ty == Ty::Str && op.is_ordering() {
                        diagnostic = diagnostic.with_note(
                            "where a string sorts is a question about a language, not about the encoding; characters are ordered, strings are not",
                            None,
                        );
                    }
                    self.error(diagnostic);
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
                                    self.ty_name(lhs_ty),
                                    self.ty_name(rhs_ty)
                                ),
                                operand.span,
                            )
                            .with_label(format!("expected bool, found {}", self.ty_name(ty)))
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
            ExprKind::Variant { enum_name, enum_span, variant, variant_span, args } => {
                self.variant(enum_name, *enum_span, variant, *variant_span, args)
            }
            ExprKind::Array { elements, span } => self.array_literal(elements, *span),
            ExprKind::Index { array, index } => {
                let of = self.expr(array);
                let element = self.element_type(of, array.span, "", array.span);
                self.index(Some(of), index);
                element.unwrap_or(Ty::Int)
            }
            ExprKind::Len { array, span } => {
                // For an array the answer is a fact about the *type* and folds
                // to a literal; for a string it is a load. Both are `len`,
                // because from the source's point of view they are the same
                // question.
                let of = self.expr(array);
                if !matches!(of, Ty::Array(_) | Ty::List(_) | Ty::Str) {
                    self.error(
                        Diagnostic::new(
                            format!("`len` needs something with a length, but this is {}", self.ty_article(of)),
                            *span,
                        )
                        .with_label("an array, a list and a string have one; nothing else does"),
                    );
                }
                Ty::Int
            }
            ExprKind::Convert { to, value, span } => self.convert(*to, value, *span),
            ExprKind::New { class, class_span, fields, span } => {
                self.object_literal(class, *class_span, fields, *span)
            }
            ExprKind::Field { object, name, name_span } => {
                let of = self.expr(object);
                self.field_type(of, name, *name_span).unwrap_or(Ty::Int)
            }
            ExprKind::MethodCall { .. } => self.method_call(expr, false).unwrap_or(Ty::Int),
            // A call in expression position must produce a value; `Int` is the
            // recovery type when it does not.
            ExprKind::Call { .. } => self.call(expr, false).unwrap_or(Ty::Int),
            ExprKind::Match { .. } => self.match_expr(expr, false).unwrap_or(Ty::Int),
        };

        self.record(expr.id, ty)
    }

}
