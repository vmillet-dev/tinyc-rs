//! Stage 3: type checking.
//!
//! Walks the AST with a symbol table and records the type of every expression
//! node in a side table keyed by [`NodeId`], so the tree itself stays immutable.
//! Unlike the lexer and parser, this stage collects *all* errors before giving
//! up: statements are independent enough that later ones are still worth
//! checking.
//!
//! ## Three passes, and why
//!
//! Each one exists because the next needs a table it could not have built for
//! itself, and a single pass could only ever look backwards.
//!
//! 0. **Enums**, because a signature may mention one and a declaration may.
//!    Nothing in an enum refers to anything else, so one sweep settles them.
//!    Array types join the same table later, as the program writes them.
//! 1. **Signatures**, before any body. That is what lets a function call
//!    another declared further down the file, and what lets a function call
//!    *itself* — by the time `fib`'s body is checked, `fib` is in the table.
//! 2. **Bodies**, each with the whole of both tables visible.

mod check;
mod declare;
mod diagnostics;
mod signature;

#[cfg(test)]
mod tests;

pub use declare::MAX_OBJECT_BYTES;
pub use diagnostics::MAX_ARRAY_LEN;

use check::{Checker, FnChecker};
use declare::{collect_classes, collect_methods, resolve_type};
use diagnostics::{count, list};
use signature::{check_entry_point, collect_signatures};

use std::collections::HashMap;

use crate::ast::{
    ArrayId, ArrayInfo, ClassId, ClassInfo, EnumId, EnumInfo,
    ListId, NodeId,
    VariantInfo,
    Program, Ty, TypeTable,
};
use crate::diag::{Diagnostic, Result, Span};

/// The name of the entry point.
pub const ENTRY_POINT: &str = "main";

/// Everything this stage worked out, for the ones after it.
///
/// Chiefly the type of every expression, but also the two answers that took a
/// name lookup to reach and that no later stage can repeat, because only this
/// one ever holds the table of declared types.
#[derive(Debug)]
pub struct Types {
    expr_ty: Vec<Ty>,
    /// Resolved return type per function, in declaration order.
    fn_ret: Vec<Option<Ty>>,
    /// Resolved parameter types per function, likewise.
    fn_params: Vec<Vec<Ty>>,
    table: TypeTable,
}

impl Types {
    pub fn of(&self, id: NodeId) -> Ty {
        self.expr_ty[id.0 as usize]
    }

    /// The return type of a function, by its position in the program.
    pub fn ret_of(&self, function: usize) -> Option<Ty> {
        self.fn_ret[function]
    }

    /// The parameter types of a function, by its position in the program.
    pub fn params_of(&self, function: usize) -> &[Ty] {
        &self.fn_params[function]
    }

    /// Every type the program has, which is what naming one takes.
    pub fn table(&self) -> &TypeTable {
        &self.table
    }
}

/// Every type the program has, and everything asked about one.
///
/// Enums arrive whole from pass 0. Array types instead appear as they are
/// *written*, and are interned so that two `int[3]`s written apart get the same
/// [`ArrayId`] — without which `Ty`'s equality, an integer comparison, would say
/// two identical types differ.
struct Declared {
    /// Shared onward with [`crate::ir`].
    table: TypeTable,
    /// Where each enum was declared, for "declared here" notes.
    enum_spans: Vec<Span>,
    enums_by_name: HashMap<String, EnumId>,
    arrays_by_shape: HashMap<(Ty, u32), ArrayId>,
    lists_by_element: HashMap<Ty, ListId>,
    classes_by_name: HashMap<String, ClassId>,
    /// Where each class was declared.
    class_spans: Vec<Span>,
    /// Which class each of the program's functions is a method of, so a method
    /// can be told from a free function and given its receiver's type.
    method_of: HashMap<usize, ClassId>,
}

impl Declared {
    fn enum_id(&self, name: &str) -> Option<EnumId> {
        self.enums_by_name.get(name).copied()
    }

    fn info(&self, id: EnumId) -> &EnumInfo {
        self.table.enum_info(id)
    }

    fn class_id(&self, name: &str) -> Option<ClassId> {
        self.classes_by_name.get(name).copied()
    }

    fn class(&self, id: ClassId) -> &ClassInfo {
        self.table.class(id)
    }

    /// Where this type name was already claimed, if it was.
    ///
    /// Enums and classes share one namespace, because [`resolve_type`] answers
    /// a name with one type and there is nowhere for a second to go. Asking
    /// both tables in one question is what keeps a class named after an enum
    /// from being quietly unreachable instead of reported.
    fn type_named(&self, name: &str) -> Option<Span> {
        self.enum_id(name)
            .map(|id| self.enum_spans[id.0 as usize])
            .or_else(|| self.class_id(name).map(|id| self.class_spans[id.0 as usize]))
    }

    /// The type of an array of `len` `elem`s, made if this is the first time
    /// the program has asked for one.
    fn array_of(&mut self, elem: Ty, len: u32) -> Ty {
        let id = *self.arrays_by_shape.entry((elem, len)).or_insert_with(|| {
            let id = ArrayId(self.table.arrays.len() as u32);
            self.table.arrays.push(ArrayInfo { elem, len });
            id
        });
        Ty::Array(id)
    }

    /// Interned like an array type, and for the same reason: `Ty` compares as
    /// an integer, so two `int[]`s written apart have to reach the same id or
    /// the type checker would say they differ.
    fn list_of(&mut self, elem: Ty) -> Ty {
        let id = *self.lists_by_element.entry(elem).or_insert_with(|| {
            let id = ListId(self.table.lists.len() as u32);
            self.table.lists.push(elem);
            id
        });
        Ty::List(id)
    }
}

/// Pass 0: one entry per enum name, with the first declaration winning.
///
/// Enums come before signatures because a signature may mention one, and before
/// bodies because a declaration may. Nothing in an enum can refer to anything
/// else, so one pass over them is enough.
fn collect_enums(program: &Program, errors: &mut Vec<Diagnostic>) -> Declared {
    let mut enums = Declared {
        table: TypeTable::default(),
        enum_spans: Vec::new(),
        enums_by_name: HashMap::new(),
        arrays_by_shape: HashMap::new(),
        lists_by_element: HashMap::new(),
        classes_by_name: HashMap::new(),
        class_spans: Vec::new(),
        method_of: HashMap::new(),
    };

    for declaration in &program.enums {
        if declaration.variants.is_empty() {
            errors.push(
                Diagnostic::new(
                    format!("`{}` has no variants", declaration.name),
                    declaration.name_span,
                )
                .with_label("an enum needs at least one")
                .with_note("no value could ever have this type otherwise", None),
            );
        }

        // Variants are named within their own enum, so two enums may both have
        // a `Red` — but one enum may not have two.
        //
        // What a variant *carries* is left until later: a payload may name a
        // class, and no class has been registered yet. Nothing in this pass
        // depends on it — an enum is one pointer or one tag whatever it
        // carries — so the two questions come apart cleanly. See
        // [`resolve_payloads`].
        let mut variants: Vec<VariantInfo> = Vec::new();
        for variant in &declaration.variants {
            if variants.iter().any(|v| v.name == variant.name) {
                errors.push(
                    Diagnostic::new(
                        format!("`{}` is declared twice in `{}`", variant.name, declaration.name),
                        variant.name_span,
                    )
                    .with_label("a variant may only be named once"),
                );
                continue;
            }
            variants.push(VariantInfo { name: variant.name.clone(), payload: Vec::new() });
        }

        if let Some(previous) = enums.type_named(&declaration.name) {
            errors.push(already_declared(&declaration.name, declaration.name_span, previous));
            continue;
        }

        let id = EnumId(enums.table.enums.len() as u32);
        enums.enums_by_name.insert(declaration.name.clone(), id);
        enums.table.enums.push(EnumInfo { name: declaration.name.clone(), variants });
        enums.enum_spans.push(declaration.name_span);
    }

    enums
}

/// Pass 0c: what each variant carries, now that every type has a name.
///
/// Split from [`collect_enums`] because a payload may name a class, and
/// classes are registered after enums are. Nothing in between needs the answer:
/// a value of an enum is one pointer when the enum carries anything and one tag
/// when it does not, and neither number depends on *what* is carried.
///
/// Every payload type has to fit in a register. An object or an array would
/// have to live inside the value, which would give an enum a size — the biggest
/// of its variants — and pull it into the containment ordering that classes
/// already need. A list is allowed and costs nothing extra, because an enum is
/// read-only: what goes in is copied in, what comes out of a pattern is copied
/// out, and there is no third way to reach it.
fn resolve_payloads(program: &Program, declared: &mut Declared, errors: &mut Vec<Diagnostic>) {
    for declaration in &program.enums {
        let Some(id) = declared.enum_id(&declaration.name) else { continue };
        // A name declared twice kept only its first enum, whose variants are
        // the ones in the table. Line the two up by name rather than by
        // position, so a duplicate variant cannot shift everything after it.
        for variant in &declaration.variants {
            let mut payload = Vec::new();
            for written in &variant.payload {
                let Some(ty) = resolve_type(declared, written, errors) else { continue };
                if !ty.fits_in_a_register() {
                    errors.push(
                        Diagnostic::new(
                            format!("a variant cannot carry {}", ty.with_article(&declared.table)),
                            written.span,
                        )
                        .with_label("a payload has to fit in a register")
                        .with_note(
                            "an object or an array lives inside the value that holds it, which \
                             would give this enum a size of its own; a class is what holds one \
                             of those",
                            None,
                        ),
                    );
                    continue;
                }
                payload.push(ty);
            }
            let info = &mut declared.table.enums[id.0 as usize];
            if let Some(at) = info.variants.iter().position(|v| v.name == variant.name) {
                info.variants[at].payload = payload;
            }
        }
    }
}

/// "`X` is already declared", underlining both places.
///
/// Enums are collected before classes, so the two spans may arrive here in the
/// opposite order from the source. What a reader wants underlined is the
/// *second* declaration, whichever kind it turned out to be, so the spans are
/// put back in the order they were written.
fn already_declared(name: &str, at: Span, previous: Span) -> Diagnostic {
    let (at, previous) = match at.offset < previous.offset {
        true => (previous, at),
        false => (at, previous),
    };
    Diagnostic::new(format!("`{name}` is already declared"), at)
        .with_label("declared a second time here")
        .with_note("previous declaration", Some(previous))
}


/// Everything a call site needs to know about its callee.
#[derive(Clone, Debug)]
struct Signature {
    params: Vec<Ty>,
    /// `None` for a function that returns nothing.
    ret: Option<Ty>,
    /// Where the function was declared, for "defined here" notes — and `None`
    /// for a built-in, which was declared nowhere the program can be shown.
    name_span: Option<Span>,
}

/// The note that says where a callee came from, which is a different sentence
/// for a function the program wrote and one it was given.
fn came_from(name: &str, at: Option<Span>) -> (String, Option<Span>) {
    match at {
        Some(at) => (format!("`{name}` is defined here"), Some(at)),
        None => (format!("`{name}` is built in"), None),
    }
}


/// Type check a program.
///
/// `max_params` is how many arguments the target can pass in registers; the
/// front end has no opinion of its own about that, it just enforces what
/// [`crate::codegen::RegisterFile::max_args`] reports.
pub fn check(program: &Program, max_params: usize) -> Result<Types> {
    let mut errors = Vec::new();

    // Pass 0: the declared types, before anything can mention one.
    let mut declared = collect_enums(program, &mut errors);
    collect_classes(program, &mut declared, &mut errors);
    // Last of the three, because a payload may name either of the others.
    resolve_payloads(program, &mut declared, &mut errors);

    // Pass 1: every signature, before any body. A method's goes in its class
    // rather than in the program's namespace.
    let mut method_signatures: HashMap<usize, Signature> = HashMap::new();
    let signatures =
        collect_signatures(program, &mut declared, max_params, &mut method_signatures, &mut errors);
    collect_methods(program, &mut declared, &method_signatures, &mut errors);
    check_entry_point(program, &signatures, &declared, &mut errors);

    // Pass 2: the bodies, each with the whole table visible.
    let signature_of = |at: usize, name: &String| -> Signature {
        method_signatures.get(&at).cloned().unwrap_or_else(|| signatures[name].clone())
    };
    let fn_ret =
        program.functions.iter().enumerate().map(|(at, f)| signature_of(at, &f.name).ret).collect();
    let fn_params = program
        .functions
        .iter()
        .enumerate()
        .map(|(at, f)| signature_of(at, &f.name).params)
        .collect();
    let mut checker = Checker {
        // `Int` is the recovery type: an expression that failed to check is
        // treated as an int so a single mistake does not cascade.
        types: Types {
            expr_ty: vec![Ty::Int; program.node_count],
            fn_ret,
            fn_params,
            table: TypeTable::default(),
        },
        signatures: &signatures,
        declared,
        errors,
    };
    for (at, function) in program.functions.iter().enumerate() {
        FnChecker::new(&mut checker, at, function).run(function);
    }

    // The table is finished only now: a body may have written an array type
    // nothing before it mentioned.
    checker.types.table = checker.declared.table;

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

