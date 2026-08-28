//! Checking what an expression *names*: assignment targets, places and fields.

use super::*;

impl FnChecker<'_, '_> {
    /// Check `x = value`, `xs[i] = value` and `p.f = value`.
    ///
    /// The three differ only in what type the target has, which is what
    /// [`Self::place_type`] works out. Everything after that — the name exists,
    /// the value agrees — is the same question asked once.
    pub(super) fn assign(&mut self, target: &Place, value: &Expr) {
        // The root is looked up *before* the value is checked, so that
        // `y = y + 1` reports the assignment rather than the use inside it.
        let (name, name_span) = target.root();
        let (name, root) = (name.to_string(), self.lookup(name));
        if root.is_none() {
            let reported = name.clone();
            self.report_undeclared(&name, || {
                Diagnostic::new(format!("undeclared variable `{reported}`"), name_span)
                    .with_label("assign to it after declaring it")
                    .with_note(
                        format!("a declaration gives it a type, as in `int {reported} = 0;`"),
                        None,
                    )
            });
        }
        let Some((_, declared_span)) = root else {
            self.expr(value);
            return;
        };

        let expected = self.place_type(target);
        let actual = match expected {
            Some(expected) => self.value_of_type(value, expected),
            None => self.expr(value),
        };
        // A variable keeps the type it was declared with. An undeclared one has
        // no type to disagree with, and was reported above.
        if let Some(expected) = expected
            && !self.coerces(actual, expected)
        {
            let noun = match target {
                Place::Var { .. } => "variable",
                Place::Element { .. } => "element",
                Place::Field { .. } => "field",
            };
            self.error(
                Diagnostic::new(
                    format!(
                        "cannot assign {} value to {} {noun}",
                        self.ty_article(actual),
                        self.ty_article(expected)
                    ),
                    value.span,
                )
                .with_label(format!(
                    "expected {}, found {}",
                    self.ty_name(expected),
                    self.ty_name(actual)
                ))
                .with_note(format!("`{name}` was declared here"), Some(declared_span)),
            );
        }
    }

    /// The type a place names, or `None` when the chain to it does not hold up.
    ///
    /// Reached by walking outward from the variable at the root, so an index or
    /// a field is checked against what actually precedes it rather than against
    /// what the whole chain was hoped to be.
    pub(super) fn place_type(&mut self, place: &Place) -> Option<Ty> {
        match place {
            Place::Var { name, .. } => self.lookup(name).map(|(ty, _)| ty),
            Place::Element { base, index, .. } => {
                let of = self.place_type(base)?;
                // A string is read-only, and that is what makes sharing one
                // safe: two variables may hold the same characters precisely
                // because neither can change them under the other.
                if of == Ty::Str {
                    self.error(
                        Diagnostic::new("a string cannot be modified", place.span())
                            .with_label("strings are read-only")
                            .with_note(
                                "build the string you want instead — `+` joins two of them",
                                None,
                            ),
                    );
                    self.index(Some(of), index);
                    return None;
                }
                let (name, at) = base.root();
                let (name, at) = (name.to_string(), at);
                let element = self.element_type(of, at, &name, base.span());
                self.index(Some(of), index);
                element
            }
            Place::Field { base, name, name_span } => {
                let of = self.place_type(base)?;
                self.field_type(of, name, *name_span)
            }
        }
    }

    /// Whether a value of `from` may be used where `to` is wanted — equality,
    /// widened by the one rule that lets a subclass stand for its base.
    pub(super) fn coerces(&self, from: Ty, to: Ty) -> bool {
        self.shared.declared.table.coerces(from, to)
    }

    /// The one type two values can *both* be seen as, if there is one.
    ///
    /// Equality answers it for everything but classes, where the answer is
    /// their nearest common ancestor: a `Circle` and a `Rect` are both a
    /// `Shape`. This is what lets several things be collected together without
    /// the first one deciding for the rest.
    ///
    /// Deliberately not applied to arrays, which stay invariant — a
    /// `Circle[3]` is not a `Shape[3]`, because writing a `Rect` through the
    /// second would put one in the first.
    pub(super) fn join(&self, a: Ty, b: Ty) -> Option<Ty> {
        if a == b {
            return Some(a);
        }
        let (Ty::Class(x), Ty::Class(y)) = (a, b) else { return None };
        let table = &self.shared.declared.table;
        let mut at = Some(x);
        while let Some(id) = at {
            if table.descends_from(y, id) {
                return Some(Ty::Class(id));
            }
            at = table.class(id).base;
        }
        None
    }

    /// The type of `object.name`, or `None` when there is no such field —
    /// reported here.
    pub(super) fn field_type(&mut self, object: Ty, name: &str, at: Span) -> Option<Ty> {
        let Ty::Class(id) = object else {
            self.error(
                Diagnostic::new(
                    format!("cannot read a field of {}", self.ty_article(object)),
                    at,
                )
                .with_label("only an object has fields"),
            );
            return None;
        };

        let class = self.shared.declared.class(id);
        if let Some(field) = class.field(name) {
            return Some(field.ty);
        }
        let known: Vec<&str> = class.fields.iter().map(|f| f.name.as_str()).collect();
        let diagnostic = no_such_member(&class.name, "field", name, at, &known);
        self.error(diagnostic);
        None
    }

    /// Check a value against the type it is being given to.
    ///
    /// Exactly one thing needs this, and it needs it because nothing in the
    /// literal itself could say: `[1, 2, 3]` is an `int[3]` on its own and an
    /// `int[]` where a list was asked for. So an array literal takes its shape
    /// from the type it is handed to — a declaration, an assignment, a return
    /// or a parameter — and everywhere else an expression's own type is the
    /// whole answer.
    ///
    /// It is also the only way to write an *empty* list, since `[]` has no
    /// element to read a type off.
    pub(super) fn value_of_type(&mut self, expr: &Expr, expected: Ty) -> Ty {
        let (ExprKind::Array { elements, .. }, Ty::List(id)) = (&expr.kind, expected) else {
            return self.expr(expr);
        };
        let elem = self.shared.declared.table.element(id);
        for element in elements {
            let ty = self.expr(element);
            if !self.coerces(ty, elem) {
                self.error(
                    Diagnostic::new(
                        format!(
                            "this element is {}, but the list holds {}",
                            self.ty_article(ty),
                            self.ty_article(elem)
                        ),
                        element.span,
                    )
                    .with_label(format!(
                        "expected {}, found {}",
                        self.ty_name(elem),
                        self.ty_name(ty)
                    )),
                );
            }
        }
        self.record(expr.id, expected)
    }


}
