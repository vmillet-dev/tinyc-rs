//! Pass 2: every function's signature, and the entry point's shape.

use std::collections::HashMap;

use crate::ast::{
    ArmBody, BinOp, Block, Builtin, Expr,
    ExprKind, MatchArm,
    Prim, Program, Stmt, Ty,
};
use crate::diag::{Diagnostic, Span};

use super::{
    Declared, ENTRY_POINT, Signature, list, resolve_type,
};

/// Pass 1: one entry per function name, with the first declaration winning.
pub(super) fn collect_signatures(
    program: &Program,
    declared: &mut Declared,
    max_params: usize,
    method_signatures: &mut HashMap<usize, Signature>,
    errors: &mut Vec<Diagnostic>,
) -> HashMap<String, Signature> {
    // The built-ins are in the table before the program's own functions are,
    // so a call reaches them through exactly the machinery a declared function
    // uses — and so a program that declares one of their names collides with
    // something already there rather than quietly winning.
    let mut signatures: HashMap<String, Signature> = Builtin::ALL
        .into_iter()
        .map(|builtin| {
            let signature = Signature {
                // A built-in states its signature in primitives, since it is a
                // name in this table before any program exists; the widening to
                // `Ty` happens here, where the program's types begin.
                params: builtin.params().iter().map(|prim| prim.ty()).collect(),
                ret: builtin.ret().map(Prim::ty),
                name_span: None,
            };
            (builtin.name().to_string(), signature)
        })
        .collect();

    for (at, function) in program.functions.iter().enumerate() {
        if !declared.method_of.contains_key(&at)
            && let Some(builtin) = Builtin::from_name(&function.name)
        {
            errors.push(
                Diagnostic::new(
                    format!("`{}` is built in and cannot be redefined", function.name),
                    function.name_span,
                )
                .with_label("this name already belongs to the language")
                .with_note(
                    format!(
                        "the built-in functions are {}",
                        list(&Builtin::ALL.map(|b| b.name()))
                    ),
                    None,
                ),
            );
            let _ = builtin;
            continue;
        }

        // A function returning an aggregate spends one register on the hidden
        // address the caller hands in, so it has one fewer to give parameters.
        let returns_aggregate = function.ret.as_ref().is_some_and(|ty| {
            resolve_type(declared, ty, &mut Vec::new()).is_some_and(|ty| !ty.fits_in_a_register())
        });
        let room = max_params - usize::from(returns_aggregate);
        if function.params.len() > room {
            // Point at the first parameter that does not fit.
            let offending = &function.params[room];
            let from = offending.ty.as_ref().map_or(offending.name_span, |ty| ty.span);
            errors.push(
                Diagnostic::new(
                    format!(
                        "`{}` takes {} parameters, but at most {room} are supported",
                        function.name,
                        function.params.len()
                    ),
                    from.to(offending.name_span),
                )
                .with_label(format!("parameter {} is one too many", room + 1))
                .with_note(
                    match returns_aggregate {
                        true => format!(
                            "one of the {max_params} argument registers carries the address this \
                             function returns into, leaving {room}"
                        ),
                        false => format!(
                            "the first {max_params} arguments travel in registers; passing more \
                             would need stack arguments"
                        ),
                    },
                    None,
                ),
            );
        }

        // A method's name lives in its class, not in the program, so two
        // classes may both have an `area` and neither collides with a free
        // function. Only free functions go in this table.
        let owner = declared.method_of.get(&at).copied();
        if owner.is_none()
            && let Some(previous) = signatures.get(&function.name)
        {
            let previous = previous.name_span;
            errors.push(
                Diagnostic::new(
                    format!("`{}` is already defined", function.name),
                    function.name_span,
                )
                .with_label("defined a second time here")
                .with_note("previous definition", previous),
            );
            continue;
        }

        // A type that does not resolve was reported by `resolve_type`; `Int`
        // stands in so the rest of the signature still checks.
        let params = function
            .params
            .iter()
            .enumerate()
            .map(|(index, p)| match &p.ty {
                Some(ty) => resolve_type(declared, ty, errors).unwrap_or(Ty::Int),
                // `self` has the class's type, and only where there is one to
                // have — and only first, where a receiver goes.
                None => match owner {
                    Some(id) if index == 0 => Ty::Class(id),
                    Some(_) => {
                        errors.push(
                            Diagnostic::new("`self` must come first", p.name_span)
                                .with_label("a receiver is a method's first parameter"),
                        );
                        Ty::Int
                    }
                    None => {
                        errors.push(
                            Diagnostic::new("`self` outside a class", p.name_span)
                                .with_label("only a method has a receiver")
                                .with_note(
                                    "a function inside a `class` block is a method; this one is not",
                                    None,
                                ),
                        );
                        Ty::Int
                    }
                },
            })
            .collect();
        // An aggregate does not come back in a register: the caller reserves
        // the room and hands its address in, so the callee copies into what the
        // caller already owns. Nothing escapes, and the hidden argument costs
        // one of the registers the count below is about.
        let ret = function
            .ret
            .as_ref()
            .map(|ty| resolve_type(declared, ty, errors).unwrap_or(Ty::Int));

        let signature = Signature { params, ret, name_span: Some(function.name_span) };
        match owner {
            Some(_) => method_signatures.insert(at, signature),
            None => signatures.insert(function.name.clone(), signature),
        };
    }

    signatures
}

/// `main` must exist, take nothing and return nothing: it is what the C runtime
/// calls, and [`crate::codegen`] returns 0 from it.
pub(super) fn check_entry_point(
    program: &Program,
    signatures: &HashMap<String, Signature>,
    declared: &Declared,
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
                first
                    .ty
                    .as_ref()
                    .map_or(first.name_span, |ty| ty.span)
                    .to(decl.params[decl.params.len() - 1].name_span),
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
            .with_label(format!("expected no return type, found {}", ty.name(&declared.table)))
            .with_note("the process exit code is always 0", None),
        );
    }
}

/// The value of an expression the compiler can work out for itself, or `None`
/// when it depends on something only the running program knows.
///
/// Deliberately shallow: no variable is ever looked up, because a variable's
/// value is not this stage's business. What it does cover is arithmetic on
/// literals, at any depth — which is exactly the arithmetic that can be
/// *rejected* rather than merely guarded, and the reason `2 * 2 * BIG` is a
/// compile error rather than a crash.
///
/// It answers `None` for an operation that has no answer too, so a mistake
/// inside a larger expression is reported once, at the operator that made it,
/// instead of again at every operator above.
pub(super) fn const_int(expr: &Expr) -> Option<i64> {
    match &expr.kind {
        ExprKind::Int(v) => Some(*v),
        ExprKind::Neg(operand) => const_int(operand)?.checked_neg(),
        ExprKind::Bin { op, lhs, rhs } => op.apply(const_int(lhs)?, const_int(rhs)?),
        _ => None,
    }
}

/// The same question about a float, and the answer is never `None` for a
/// reason of arithmetic: every float operation has an answer, so what stops
/// this is only ever an operand this stage cannot see.
///
/// It does not look through `%`: `sema` has already refused that program, and
/// the arm below would have to invent an answer for an operation TinyC does not
/// have.
pub(super) fn const_float(expr: &Expr) -> Option<f64> {
    match &expr.kind {
        ExprKind::Float(v) => Some(*v),
        ExprKind::Neg(operand) => Some(-const_float(operand)?),
        ExprKind::Bin { op, lhs, rhs } => {
            let (a, b) = (const_float(lhs)?, const_float(rhs)?);
            match op {
                BinOp::Add => Some(a + b),
                BinOp::Sub => Some(a - b),
                BinOp::Mul => Some(a * b),
                BinOp::Div => Some(a / b),
                BinOp::Rem => None,
            }
        }
        _ => None,
    }
}

/// Whether every path out of a block ends in a `return`.
///
/// Deliberately simple: a loop is never assumed to run, so `while (true)` does
/// not count. That can only reject a program that would in fact be fine, never
/// accept one that would fall off the end.
pub(super) fn always_returns(block: &Block) -> bool {
    block.stmts.iter().any(|stmt| match stmt {
        Stmt::Return { .. } => true,
        // Both arms must return, and an `if` without an `else` has a path that
        // skips it entirely.
        Stmt::If { then_block, else_block: Some(else_block), .. } => {
            always_returns(then_block) && always_returns(else_block)
        }
        // A `match` needs no `else` to be complete: it covers every variant, so
        // if every arm returns then so does the statement. This is the one
        // place exhaustiveness pays for itself in what a program may leave out.
        Stmt::Match(expr) => match_arms(expr).is_some_and(|arms| {
            !arms.is_empty()
                && arms.iter().all(|arm| match &arm.body {
                    ArmBody::Block(block) => always_returns(block),
                    // A value arm hands one back to whoever wanted it; it does
                    // not leave the function.
                    ArmBody::Value(_) => false,
                })
        }),
        _ => false,
    })
}

/// Whether every path out of a block leaves it early, by any means.
///
/// Wider than [`always_returns`], which only counts `return` because only a
/// `return` ends a *function*. A `match` used as an expression asks a different
/// question about its block arms — "can control reach the end of this, where a
/// value would be needed?" — and a `break` or a `continue` answers it just as
/// well as a `return` does.
pub(super) fn diverges(block: &Block) -> bool {
    block.stmts.iter().any(|stmt| match stmt {
        Stmt::Return { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => true,
        Stmt::If { then_block, else_block: Some(else_block), .. } => {
            diverges(then_block) && diverges(else_block)
        }
        Stmt::Match(expr) => match_arms(expr).is_some_and(|arms| {
            !arms.is_empty()
                && arms.iter().all(|arm| match &arm.body {
                    ArmBody::Block(block) => diverges(block),
                    ArmBody::Value(_) => false,
                })
        }),
        _ => false,
    })
}

/// The arms of an expression that is a `match`, for the two walks above.
pub(super) fn match_arms(expr: &Expr) -> Option<&[MatchArm]> {
    match &expr.kind {
        ExprKind::Match { arms, .. } => Some(arms),
        _ => None,
    }
}

