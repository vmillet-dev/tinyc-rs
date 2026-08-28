//! Checking array, list and object literals, and `push`.

use super::*;

impl FnChecker<'_, '_> {
    pub(super) fn push_stmt(&mut self, span: Span, target: &Place, value: &Expr) {
        let Some(ty) = self.place_type(target) else {
            self.expr(value);
            return;
        };
        let Ty::List(id) = ty else {
            self.expr(value);
            self.error(
                Diagnostic::new(
                    format!("`push` needs a list, but this is {}", self.ty_article(ty)),
                    span,
                )
                .with_label("only a list can grow")
                .with_note(
                    "an array's length is part of its type and cannot change; `int[]` is the \
                     one that can",
                    None,
                ),
            );
            return;
        };

        // Growing may move the list: the elements are copied into a larger
        // block, and the old one is left behind. Whoever *owns* the list can be
        // told the new address; a caller cannot, because what it handed over
        // was a copy of the address and not the variable holding it.
        //
        // Left alone this would be the worst kind of bug — the length lives
        // with the elements, so a push that happens to fit would be visible to
        // the caller and a push that had to move would silently not be.
        if let Place::Var { name, name_span } = target
            && self.binding(name).is_some_and(|b| b.parameter)
        {
            self.error(
                Diagnostic::new(format!("cannot push onto `{name}`, which is a parameter"), *name_span)
                    .with_label("a parameter is the caller's list, and growing it may move it")
                    .with_note(
                        "copy it first — `int[] mine = xs;` — and return the copy, or build the \
                         list here and return that",
                        None,
                    ),
            );
        }

        let elem = self.shared.declared.table.element(id);
        let actual = self.value_of_type(value, elem);
        if !self.coerces(actual, elem) {
            self.error(
                Diagnostic::new(
                    format!(
                        "cannot push {} value onto {}",
                        self.ty_article(actual),
                        self.ty_article(ty)
                    ),
                    value.span,
                )
                .with_label(format!(
                    "expected {}, found {}",
                    self.ty_name(elem),
                    self.ty_name(actual)
                )),
            );
        }
    }

    /// Check `[1, 2, 3]` and answer the array type it has.
    ///
    /// Its length is its element count — there is nothing to infer and nothing
    /// to declare. What a declaration does is *agree* with it, and the mismatch
    /// is caught where the two meet.
    pub(super) fn array_literal(&mut self, elements: &[Expr], span: Span) -> Ty {
        if elements.is_empty() {
            self.error(
                Diagnostic::new("this array has no elements", span)
                    .with_label("an array needs at least one")
                    .with_note("its element type is read off them, so there is nothing to go on", None),
            );
            return Ty::Int;
        }

        // The elements settle the type between them rather than the first one
        // deciding for the rest: several classes of one hierarchy join at their
        // common ancestor, which is what makes a mixed array possible at all.
        let mut first = self.expr(&elements[0]);
        for element in &elements[1..] {
            let ty = self.expr(element);
            match self.join(first, ty) {
                Some(joined) => first = joined,
                None => self.error(
                    Diagnostic::new(
                        format!(
                            "this element is {}, but the ones before it are {}",
                            self.ty_article(ty),
                            self.ty_article(first)
                        ),
                        element.span,
                    )
                    .with_label(format!(
                        "expected {}, found {}",
                        self.ty_name(first),
                        self.ty_name(ty)
                    ))
                    .with_note("every element of an array has to agree", Some(elements[0].span)),
                ),
            }
        }

        // An object element is fine — it takes its hierarchy's room, and the
        // index arithmetic scales by that instead of by eight. An array element
        // is not: nothing yet says how long the inner one is.
        if matches!(first, Ty::Array(_) | Ty::List(_)) {
            // Pointed at the whole literal rather than at one element: what
            // follows is a declaration complaining that the type it got is not
            // the type it wanted, and the two sort together with the real
            // mistake first.
            self.error(
                Diagnostic::new(
                    format!("cannot make an array of {}", self.ty_article(first)),
                    span,
                )
                .with_label("an array's elements may not themselves be a run of values")
                .with_note("arrays and lists do not nest yet", None),
            );
            return Ty::Int;
        }

        if elements.len() as i64 > MAX_ARRAY_LEN {
            self.error(
                Diagnostic::new("this array has too many elements", span)
                    .with_label(format!("at most {MAX_ARRAY_LEN} are supported")),
            );
            return Ty::Int;
        }
        self.shared.declared.array_of(first, elements.len() as u32)
    }

    /// The element type of an array, or `None` when it is not one — reported
    /// here, because indexing something without elements is the mistake worth
    /// naming.
    pub(super) fn element_type(&mut self, ty: Ty, declared_span: Span, name: &str, at: Span) -> Option<Ty> {
        match ty {
            Ty::Array(id) => Some(self.shared.declared.table.array(id).elem),
            Ty::List(id) => Some(self.shared.declared.table.element(id)),
            // A string is a run of characters, so indexing one produces a
            // character — never a byte and never a string of length one.
            Ty::Str => Some(Ty::Char),
            other => {
                let mut diagnostic =
                    Diagnostic::new(format!("cannot index {}", self.ty_article(other)), at)
                        .with_label("an array, a list and a string have elements; nothing else does");
                if !name.is_empty() {
                    diagnostic = diagnostic
                        .with_note(format!("`{name}` was declared here"), Some(declared_span));
                }
                self.error(diagnostic);
                None
            }
        }
    }

    /// Check `Circle { r: 5 }` and answer the class's type.
    ///
    /// Every field must be named exactly once, so there is no partial object
    /// and no default: a value of a class type is complete from the moment it
    /// exists, which is what removes the question `null` would answer.
    pub(super) fn object_literal(
        &mut self,
        class: &str,
        class_span: Span,
        fields: &[FieldInit],
        span: Span,
    ) -> Ty {
        let Some(id) = self.shared.declared.class_id(class) else {
            self.error(
                Diagnostic::new(format!("unknown class `{class}`"), class_span)
                    .with_label("no class goes by this name"),
            );
            for field in fields {
                self.expr(&field.value);
            }
            return Ty::Int;
        };

        let declared = self.shared.declared.class(id).fields.clone();
        let mut given: Vec<Option<Span>> = vec![None; declared.len()];
        // A field that named nothing leaves a hole that is not the program's
        // real mistake — `Circle { q: 1 }` is a misspelt `r`, not a forgotten
        // one — and the derived complaint sorts ahead of the true one.
        let mut readable = true;

        for field in fields {
            // Which field this is has to be settled *before* its value is
            // checked, because one value cannot say what it is on its own:
            // `[]` is an empty list of whatever was asked for, and what asked
            // here is the field. See `value_of_type`.
            let Some(at) = declared.iter().position(|f| f.name == field.name) else {
                readable = false;
                self.expr(&field.value);
                let known: Vec<&str> = declared.iter().map(|f| f.name.as_str()).collect();
                let at = field.name_span;
                self.error(no_such_member(class, "field", &field.name, at, &known));
                continue;
            };

            if let Some(previous) = given[at] {
                self.expr(&field.value);
                self.error(
                    Diagnostic::new(format!("`{}` is given twice", field.name), field.name_span)
                        .with_label("a field takes one value")
                        .with_note("given here", Some(previous)),
                );
                continue;
            }
            given[at] = Some(field.name_span);

            let expected = declared[at].ty;
            let actual = self.value_of_type(&field.value, expected);
            if !self.coerces(actual, expected) {
                self.error(
                    Diagnostic::new(
                        format!(
                            "cannot give {} value to {} field",
                            self.ty_article(actual),
                            self.ty_article(expected)
                        ),
                        field.value.span,
                    )
                    .with_label(format!(
                        "expected {}, found {}",
                        self.ty_name(expected),
                        self.ty_name(actual)
                    )),
                );
            }
        }

        let missing: Vec<&str> = declared
            .iter()
            .zip(&given)
            .filter(|(_, seen)| seen.is_none())
            .map(|(field, _)| field.name.as_str())
            .collect();
        if !missing.is_empty() && readable {
            self.error(
                Diagnostic::new(format!("this `{class}` is missing a field"), span)
                    .with_label(format!("{} not given", missing_verb(&missing)))
                    .with_note(
                        "an object is complete or it does not exist; there is no default value",
                        None,
                    ),
            );
        }
        Ty::Class(id)
    }


}
