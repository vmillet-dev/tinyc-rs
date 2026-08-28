//! Checking enums: building a variant, and proving a `match` covers its domain.

use super::*;

/// What a `match` may ask of its scrutinee, and when the arms are complete.
///
/// The two shapes this covers are the whole of what changes between
/// `match (colour)` and `match (n)`, and they meet in one struct because
/// everything *else* about a match — the arms agreeing, a block arm having to
/// diverge, a duplicate arm never running — is the same question either way.
struct Domain {
    /// The values a program could write out, in the order a diagnostic should
    /// name them. Empty where there are too many to enumerate.
    names: Vec<String>,
    /// Whether `_` is allowed. False for exactly one type: a declared enum,
    /// where writing every variant out *is* the point of the check.
    catch_all: bool,
    /// What one of [`Self::names`] is called in a message.
    noun: &'static str,
}

/// What one arm's pattern takes out of the domain.
enum Selected {
    /// One of the values [`Domain::names`] lists, by position.
    One(usize),
    /// One value out of too many to count, identified by the pattern itself.
    Literal,
    /// `_`, and so everything the arms before it did not take.
    Everything,
}


impl FnChecker<'_, '_> {
    /// Check `Color::Red` and answer the type it has, which is the enum's.
    ///
    /// Both halves can be wrong independently, and are reported separately: the
    /// enum name underlines one, the variant the other.
    pub(super) fn variant(
        &mut self,
        name: &str,
        span: Span,
        variant: &str,
        variant_span: Span,
        args: &[Expr],
    ) -> Ty {
        let Some(id) = self.shared.declared.enum_id(name) else {
            self.error(
                Diagnostic::new(format!("unknown enum `{name}`"), span)
                    .with_label("no enum goes by this name")
                    .with_note("a variant is always written `Enum::Variant`", None),
            );
            for arg in args {
                self.expr(arg);
            }
            return Ty::Int;
        };

        let Some(payload) = self.shared.declared.info(id).variant(variant).map(|v| v.payload.clone())
        else {
            let info = self.shared.declared.info(id);
            let known: Vec<&str> = info.names();
            let note = format!("`{name}` has {}", list(&known));
            self.error(
                Diagnostic::new(format!("`{name}` has no variant `{variant}`"), variant_span)
                    .with_label("not one of its variants")
                    .with_note(note, None),
            );
            for arg in args {
                self.expr(arg);
            }
            // Even a misspelt variant has the enum's type: that much was
            // written down, and reporting the same mistake again as a type
            // error would not help anybody.
            return Ty::Enum(id);
        };

        self.payload_args(name, variant, variant_span, &payload, args, span);
        Ty::Enum(id)
    }

    /// Name what a variant carries, for the length of the arm that matched it.
    ///
    /// The names are the *arm's* rather than the declaration's — a variant is
    /// declared positionally, so `Shape::Circle(radius)` and
    /// `Shape::Circle(r)` are the same pattern spelt for two different readers.
    /// Each is an ordinary local from here on, with the type the declaration
    /// gave that position.
    pub(super) fn bind_payload(
        &mut self,
        enum_name: &str,
        variant: &str,
        variant_span: Span,
        id: EnumId,
        tag: i64,
        bindings: &[(String, Span)],
    ) {
        let payload = self.shared.declared.info(id).variants[tag as usize].payload.clone();
        if bindings.len() != payload.len() {
            let carries = match payload.is_empty() {
                true => format!("`{enum_name}::{variant}` carries nothing"),
                false => format!(
                    "`{enum_name}::{variant}` carries {}",
                    count(payload.len(), "value", "values")
                ),
            };
            self.error(
                Diagnostic::new(
                    format!(
                        "this arm names {} of what `{enum_name}::{variant}` carries",
                        count(bindings.len(), "value", "values")
                    ),
                    variant_span,
                )
                .with_label(carries)
                .with_note(
                    "a pattern names every value a variant carries, in order — there is no \
                     way to leave one out",
                    None,
                ),
            );
        }
        for (index, (name, span)) in bindings.iter().enumerate() {
            // A name with nothing behind it still goes in scope, as an `int`,
            // so the arm's body does not then report it undeclared as well.
            let ty = payload.get(index).copied().unwrap_or(Ty::Int);
            self.declare(name, ty, *span, "a name for what this variant carries");
        }
    }

    /// What a variant is given, checked against what it carries.
    ///
    /// The same shape as a call's arguments, and deliberately as strict: a
    /// variant that carries an `int` takes an `int`, and nothing converts on
    /// the way in.
    pub(super) fn payload_args(
        &mut self,
        name: &str,
        variant: &str,
        variant_span: Span,
        payload: &[Ty],
        args: &[Expr],
        span: Span,
    ) {
        if args.len() != payload.len() {
            // Point at the first argument with nothing to be, or at the
            // variant itself when there are too few.
            let at = args.get(payload.len()).map_or(variant_span, |arg| arg.span);
            let carries = match payload.is_empty() {
                true => format!("`{name}::{variant}` carries nothing"),
                false => format!(
                    "`{name}::{variant}` carries {}",
                    count(payload.len(), "value", "values")
                ),
            };
            self.error(
                Diagnostic::new(
                    format!(
                        "`{name}::{variant}` was given {}",
                        count(args.len(), "value", "values")
                    ),
                    span,
                )
                .with_label(carries)
                .with_note("a variant takes exactly what it declares, in order", Some(at)),
            );
        }

        for (index, arg) in args.iter().enumerate() {
            let Some(&expected) = payload.get(index) else {
                self.expr(arg);
                continue;
            };
            let actual = self.value_of_type(arg, expected);
            if !self.coerces(actual, expected) {
                self.error(
                    Diagnostic::new(
                        format!(
                            "`{name}::{variant}` carries {} here, but this is {}",
                            self.ty_article(expected),
                            self.ty_article(actual)
                        ),
                        arg.span,
                    )
                    .with_label(format!(
                        "expected {}, found {}",
                        self.ty_name(expected),
                        self.ty_name(actual)
                    )),
                );
            }
        }
    }

    /// Check a `match` and answer the type it produces, or `None` when it
    /// produces nothing.
    ///
    /// Exhaustiveness is the whole point of the construct: a `match` that has
    /// forgotten a variant does not compile, so adding a variant to an enum
    /// turns every place that has to think again into an error rather than into
    /// a silent fall-through — which is also why there is no catch-all pattern.
    ///
    /// `as_statement` says whether producing nothing is acceptable here, the
    /// same question [`Self::call`] asks about a `void` callee.
    pub(super) fn match_expr(&mut self, expr: &Expr, as_statement: bool) -> Option<Ty> {
        let ExprKind::Match { keyword, scrutinee, arms } = &expr.kind else {
            unreachable!("the parser only builds `Stmt::Match` around a match");
        };
        let span = *keyword;

        let ty = self.expr(scrutinee);
        let Some(domain) = self.domain_of(ty) else {
            // A float is refused for a different reason from the rest, and says
            // so: the others cannot be compared at all, while a float can be
            // compared and should not be — a pattern is an equality test, and
            // an equality test is the one question about a float that almost
            // never means what it looks like.
            let (label, note) = match ty {
                Ty::Float => (
                    "a pattern asks whether two floats are exactly equal",
                    "`0.1 + 0.2` is not `0.3`, and a NaN is equal to nothing at all; \
                     compare with `<` and `>` instead",
                ),
                _ => (
                    "a match needs a value that can be compared to a pattern",
                    "an enum, an `int`, a `char`, a `string` or a `bool`; a run of values \
                     and an object are neither",
                ),
            };
            self.error(
                Diagnostic::new(
                    format!("cannot match on {}", self.ty_article(ty)),
                    scrutinee.span,
                )
                .with_label(label)
                .with_note(note, None),
            );
            // The arms are still checked: their own mistakes are worth
            // reporting whatever the scrutinee turned out to be.
            for arm in arms {
                self.arm_body(&arm.body);
            }
            return None;
        };

        // An arm whose pattern said nothing usable leaves a hole that is not
        // the program's real mistake. Reporting what it failed to cover on top
        // of that would be saying the same thing twice, and the derived
        // complaint sorts ahead of the true one.
        let mut readable = true;
        // Where the first arm for each value of a countable domain was, in
        // declaration order — which is the order the diagnostics below want to
        // talk about them in.
        let mut covered: Vec<Option<Span>> = vec![None; domain.names.len()];
        // Where `_` was, and what it makes of every arm after it.
        let mut catch_all: Option<Span> = None;
        // The literal patterns seen so far, for the domains with nothing to
        // count: two arms saying `3` is the same mistake as two saying `Red`.
        let mut literals: Vec<(&Pattern, Span)> = Vec::new();

        for arm in arms {
            if let Some(previous) = catch_all {
                self.error(
                    Diagnostic::new("this arm can never run", arm.span)
                        .with_label("`_` before it already took everything")
                        .with_note("the catch-all has to come last", Some(previous)),
                );
            }
            readable &= self.match_arm(
                ty,
                &domain,
                arm,
                &mut covered,
                &mut catch_all,
                &mut literals,
            );
        }

        if catch_all.is_none() && readable {
            self.report_gaps(ty, &domain, span, &covered);
        }

        self.match_value(span, arms, as_statement)
    }


    /// Check an arm's body, whichever of the two shapes it has.
    pub(super) fn arm_body(&mut self, body: &ArmBody) {
        match body {
            // The type it produced lands in the table; `match_value` reads it
            // back once every arm has been seen.
            ArmBody::Value(value) => {
                self.expr(value);
            }
            ArmBody::Block(block) => self.block(block),
        }
    }

    /// What the arms produce between them, and whether that is allowed here.
    ///
    /// A value arm hands its expression back; a block arm hands back nothing,
    /// because `return` inside one keeps its single meaning of leaving the
    /// function. So a block arm is only admissible where a value is wanted if
    /// control cannot reach its end at all — which is what [`diverges`] asks.
    pub(super) fn match_value(&mut self, span: Span, arms: &[MatchArm], as_statement: bool) -> Option<Ty> {
        // The first arm to produce a value sets the type the rest must agree
        // with, and is where "expected ..., found ..." points back to.
        let mut produced: Option<(Ty, Span)> = None;
        for arm in arms {
            let ArmBody::Value(value) = &arm.body else { continue };
            let ty = self.shared.types.of(value.id);
            match produced {
                None => produced = Some((ty, value.span)),
                Some((expected, first)) if ty != expected => self.error(
                    Diagnostic::new(
                        format!(
                            "this arm produces {}, but an earlier one produces {}",
                            self.ty_article(ty),
                            self.ty_article(expected)
                        ),
                        value.span,
                    )
                    .with_label(format!(
                        "expected {}, found {}",
                        self.ty_name(expected),
                        self.ty_name(ty)
                    ))
                    .with_note("every arm of a match has to agree", Some(first)),
                ),
                Some(_) => {}
            }
        }

        if as_statement {
            // Nothing here would read a value, and TinyC discards none.
            if let Some((_, first)) = produced {
                self.error(
                    Diagnostic::new("this arm produces a value, but nothing uses it", first)
                        .with_label("a match written as a statement runs its arms for effect")
                        .with_note(
                            "wrap it in a block to discard it, or use the match as a value",
                            None,
                        ),
                );
            }
            return None;
        }

        // In value position, a block arm has to be one control never leaves the
        // end of, or there is a path with no value to hand back.
        for arm in arms {
            let ArmBody::Block(block) = &arm.body else { continue };
            if !diverges(block) {
                self.error(
                    Diagnostic::new("this arm produces no value", arm.span)
                        .with_label("but the match it belongs to is used as one")
                        .with_note(
                            "an arm is either an expression, or a block that returns, \
                             breaks or continues",
                            None,
                        ),
                );
            }
        }

        if produced.is_none() {
            self.error(
                Diagnostic::new("this match produces no value", span)
                    .with_label("every arm leaves without one")
                    .with_note("give at least one arm an expression to hand back", None),
            );
        }
        // `Int` is the recovery type, as everywhere else: a match that produced
        // nothing has already been reported, and pretending it has no type at
        // all would make its caller report the same mistake again.
        Some(produced.map_or(Ty::Int, |(ty, _)| ty))
    }

    /// What a value of `ty` can be matched against, or `None` when nothing can.
    ///
    /// Two shapes of answer, and the difference is the whole of what changes
    /// between `match (colour)` and `match (n)`. An enum and a `bool` have a
    /// list of values a program could write out, so a match on one is complete
    /// when it has written them all out. An `int`, a `char` and a `string` do
    /// not, so a match on one is complete only with a `_`.
    fn domain_of(&self, ty: Ty) -> Option<Domain> {
        match ty {
            Ty::Enum(id) => Some(Domain {
                names: self.shared.declared.info(id).names().iter().map(|n| n.to_string()).collect(),
                // Writing out every variant is exactly what the check is for.
                catch_all: false,
                noun: "variant",
            }),
            // `false` is 0 and `true` is 1, which is also how they are stored.
            Ty::Bool => Some(Domain {
                names: vec!["false".to_string(), "true".to_string()],
                catch_all: true,
                noun: "value",
            }),
            Ty::Int | Ty::Char | Ty::Str => {
                Some(Domain { names: Vec::new(), catch_all: true, noun: "value" })
            }
            // A run of values and an object cannot even be compared for
            // equality, so there is nothing a pattern could ask of one.
            Ty::Array(_) | Ty::List(_) | Ty::Class(_) => None,
            // A float can be compared, and that is the problem: a pattern is
            // equality, and equality on floats does not mean what writing one
            // down suggests. `0.1 + 0.2` is not `0.3`, `-0.0` and `0.0` are one
            // number spelled two ways, and a NaN selects no arm at all — not
            // even `NaN`. So none of them is offered.
            Ty::Float => None,
        }
    }

    /// One arm: its pattern has to be about the scrutinee's type, and has to be
    /// the first arm to select what it selects.
    ///
    /// Answers whether the pattern said something usable about coverage — a
    /// duplicate still did, a mistyped one did not.
    fn match_arm<'p>(
        &mut self,
        ty: Ty,
        domain: &Domain,
        arm: &'p MatchArm,
        covered: &mut [Option<Span>],
        catch_all: &mut Option<Span>,
        literals: &mut Vec<(&'p Pattern, Span)>,
    ) -> bool {
        // Always check the body, whatever the pattern turns out to be. What a
        // pattern binds is in scope for exactly that body, so the arm gets a
        // scope of its own — which is also what lets two arms use one name for
        // quite different things.
        self.scopes.push(HashMap::new());
        let selected = self.arm_pattern(ty, domain, arm);
        self.arm_body(&arm.body);
        self.scopes.pop();

        let Some(selected) = selected else { return false };
        match selected {
            // One of a countable set: recorded by position, so what is left
            // over can be named in declaration order.
            Selected::One(at) => {
                if let Some(previous) = covered[at] {
                    self.error(
                        Diagnostic::new(
                            format!("`{}` is already covered", domain.names[at]),
                            arm.span,
                        )
                        .with_label("this arm can never run")
                        .with_note("covered here", Some(previous)),
                    );
                    return true;
                }
                covered[at] = Some(arm.span);
            }
            // One of a set nobody could write out, so the only question a
            // second one raises is whether it repeats an earlier arm.
            Selected::Literal => {
                let previous = literals
                    .iter()
                    .find(|(seen, _)| seen.same_as(&arm.pattern))
                    .map(|(_, at)| *at);
                if let Some(previous) = previous {
                    self.error(
                        Diagnostic::new(
                            format!("{} is already covered", arm.pattern.describe()),
                            arm.span,
                        )
                        .with_label("this arm can never run")
                        .with_note("covered here", Some(previous)),
                    );
                    return true;
                }
                literals.push((&arm.pattern, arm.span));
            }
            Selected::Everything => *catch_all = Some(arm.span),
        }
        true
    }

    /// What an arm's pattern selects, or `None` when it is not about this
    /// scrutinee at all — reported here.
    fn arm_pattern(&mut self, ty: Ty, domain: &Domain, arm: &MatchArm) -> Option<Selected> {
        match &arm.pattern {
            Pattern::Wildcard => {
                if !domain.catch_all {
                    self.error(
                        Diagnostic::new("`_` cannot be used here", arm.span)
                            .with_label(format!(
                                "every {} of {} has to be written out",
                                domain.noun,
                                self.ty_article(ty)
                            ))
                            .with_note(
                                "that is what the check is for: adding a variant should stop \
                                 every match that does not handle it from compiling, and a \
                                 catch-all would swallow exactly that",
                                None,
                            ),
                    );
                    return None;
                }
                Some(Selected::Everything)
            }
            Pattern::Variant { enum_name, enum_span, variant, variant_span, bindings } => {
                let Ty::Enum(id) = ty else {
                    self.error(
                        Diagnostic::new(
                            format!(
                                "this arm matches a variant, but the value is {}",
                                self.ty_article(ty)
                            ),
                            arm.span,
                        )
                        .with_label(format!("expected {}", self.ty_article(ty))),
                    );
                    return None;
                };
                let expected = self.shared.declared.info(id).name.clone();
                if *enum_name != expected {
                    self.error(
                        Diagnostic::new(
                            format!(
                                "this arm matches `{enum_name}`, but the value is a `{expected}`"
                            ),
                            *enum_span,
                        )
                        .with_label(format!("expected a variant of `{expected}`")),
                    );
                    return None;
                }
                let Some(tag) = self.shared.declared.info(id).tag(variant) else {
                    let known: Vec<&str> = domain.names.iter().map(String::as_str).collect();
                    self.error(
                        Diagnostic::new(
                            format!("`{expected}` has no variant `{variant}`"),
                            *variant_span,
                        )
                        .with_label("not one of its variants")
                        .with_note(format!("`{expected}` has {}", list(&known)), None),
                    );
                    return None;
                };
                self.bind_payload(&expected, variant, *variant_span, id, tag, bindings);
                Some(Selected::One(tag as usize))
            }
            // A literal is exactly as type-checked as it would be beside an
            // `==`, and for the same reason: nothing in TinyC converts on its
            // own, so `match (n) { 'a' => ... }` is the same mistake as
            // `n == 'a'`.
            literal => {
                let wanted = literal.matches_ty().expect("the two others are handled above");
                if wanted != ty {
                    self.error(
                        Diagnostic::new(
                            format!(
                                "this arm matches {}, but the value is {}",
                                self.ty_article(wanted),
                                self.ty_article(ty)
                            ),
                            arm.span,
                        )
                        .with_label(format!(
                            "expected {}, found {}",
                            self.ty_name(ty),
                            self.ty_name(wanted)
                        )),
                    );
                    return None;
                }
                // `false` is 0 and `true` is 1, which is both how a `bool` is
                // stored and where it goes in the coverage table.
                match literal {
                    Pattern::Bool(v) => Some(Selected::One(usize::from(*v))),
                    _ => Some(Selected::Literal),
                }
            }
        }
    }

    /// What a match with no `_` still leaves unaccounted for.
    fn report_gaps(&mut self, ty: Ty, domain: &Domain, span: Span, covered: &[Option<Span>]) {
        // Nothing to count: the only way to be complete is the catch-all, and
        // there is not one.
        if domain.names.is_empty() {
            let name = self.ty_name(ty);
            self.error(
                Diagnostic::new(format!("this match does not cover every `{name}`"), span)
                    .with_label("add `_ => ...` for the rest")
                    .with_note(
                        format!(
                            "a `{name}` has no list of values to write out, so a match on one \
                             is only complete with a catch-all"
                        ),
                        None,
                    ),
            );
            return;
        }

        let missing: Vec<&str> = domain
            .names
            .iter()
            .zip(covered)
            .filter(|(_, seen)| seen.is_none())
            .map(|(name, _)| name.as_str())
            .collect();
        if missing.is_empty() {
            return;
        }
        let name = self.ty_name(ty);
        let what = match domain.catch_all {
            true => format!("this match does not cover every `{name}`"),
            false => format!("this match does not cover every variant of `{name}`"),
        };
        let note = match domain.catch_all {
            true => "write the rest out, or add `_ => ...`",
            false => "every variant needs an arm; a match on an enum has no catch-all, so that \
                      adding a variant cannot be quietly ignored",
        };
        self.error(
            Diagnostic::new(what, span)
                .with_label(format!("{} not handled", missing_verb(&missing)))
                .with_note(note, None),
        );
    }

}
