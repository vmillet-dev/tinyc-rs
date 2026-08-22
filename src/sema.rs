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

use std::collections::{HashMap, HashSet};

use crate::ast::{
    ArmBody, ArrayId, ArrayInfo, BinOp, Block, ClassId, ClassInfo, EnumId, EnumInfo, Expr,
    ExprKind, FieldInfo, FieldInit, FnDecl, MatchArm, MethodInfo, NodeId, Place, Prim, Program,
    Stmt, Ty, TypeRef, TypeTable, is_scalar_value,
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
        let mut variants: Vec<String> = Vec::new();
        for variant in &declaration.variants {
            if variants.contains(&variant.name) {
                errors.push(
                    Diagnostic::new(
                        format!("`{}` is declared twice in `{}`", variant.name, declaration.name),
                        variant.name_span,
                    )
                    .with_label("a variant may only be named once"),
                );
                continue;
            }
            variants.push(variant.name.clone());
        }

        if let Some(&previous) = enums.enums_by_name.get(&declaration.name) {
            let previous = enums.enum_spans[previous.0 as usize];
            errors.push(
                Diagnostic::new(
                    format!("`{}` is already declared", declaration.name),
                    declaration.name_span,
                )
                .with_label("declared a second time here")
                .with_note("previous declaration", Some(previous)),
            );
            continue;
        }

        let id = EnumId(enums.table.enums.len() as u32);
        enums.enums_by_name.insert(declaration.name.clone(), id);
        enums.table.enums.push(EnumInfo { name: declaration.name.clone(), variants });
        enums.enum_spans.push(declaration.name_span);
    }

    enums
}

/// Bytes an object spends on its vtable pointer, which sits at offset 0.
const VPTR_SIZE: u32 = 8;

/// Pass 0b: resolve base classes, lay every object out, and settle the vtables.
///
/// Names are registered first so that a field may mention any class and a base
/// may be declared after its subclass. What has to happen in order is *layout*:
/// a class's fields follow its base's, so a base is laid out before anything
/// that extends it.
fn collect_classes(program: &Program, declared: &mut Declared, errors: &mut Vec<Diagnostic>) {
    // Every name first, with a placeholder layout, so the rest can refer to it.
    for class in &program.classes {
        if let Some(&previous) = declared.classes_by_name.get(&class.name) {
            let previous = declared.class_spans[previous.0 as usize];
            errors.push(
                Diagnostic::new(format!("`{}` is already declared", class.name), class.name_span)
                    .with_label("declared a second time here")
                    .with_note("previous declaration", Some(previous)),
            );
            continue;
        }
        let id = ClassId(declared.table.classes.len() as u32);
        declared.classes_by_name.insert(class.name.clone(), id);
        declared.class_spans.push(class.name_span);
        declared.table.classes.push(ClassInfo {
            name: class.name.clone(),
            base: None,
            fields: Vec::new(),
            methods: Vec::new(),
            size: VPTR_SIZE,
            storage: VPTR_SIZE,
        });
        for &at in &class.methods {
            declared.method_of.insert(at, id);
        }
    }

    // Bases, with the one shape that would make everything below loop forever
    // ruled out first.
    let declarations = order_of_declarations(program, declared);
    for &(id, class) in &declarations {
        let Some((name, span)) = &class.base else { continue };
        let Some(base) = declared.class_id(name) else {
            errors.push(
                Diagnostic::new(format!("unknown class `{name}`"), *span)
                    .with_label("a base class has to be declared somewhere in this file"),
            );
            continue;
        };
        if base == id || inherits_through(declared, base, id) {
            errors.push(
                Diagnostic::new(format!("`{}` inherits from itself", class.name), *span)
                    .with_label("a class cannot be its own ancestor, directly or otherwise"),
            );
            continue;
        }
        declared.table.classes[id.0 as usize].base = Some(base);
    }

    // Layout, bases first. Every value in TinyC is eight bytes wide, so a
    // field's place is its position — the same arithmetic an array index does.
    for id in layout_order(declared) {
        let class = &program.classes[declarations_index(program, declared, id)];
        let base = declared.table.classes[id.0 as usize].base;

        let mut fields = match base {
            Some(base) => declared.class(base).fields.clone(),
            None => Vec::new(),
        };
        let mut offset = fields.last().map_or(VPTR_SIZE, |f| f.offset + 8);

        for field in &class.fields {
            let ty = resolve_type(declared, &field.ty, errors).unwrap_or(Ty::Int);
            if !ty.fits_in_a_register() {
                errors.push(
                    Diagnostic::new(
                        format!("a field cannot be {}", ty.with_article(&declared.table)),
                        field.ty.span,
                    )
                    .with_label("every field of an object is one value")
                    .with_note("arrays and objects do not nest yet", None),
                );
                continue;
            }
            if fields.iter().any(|f| f.name == field.name) {
                errors.push(
                    Diagnostic::new(
                        format!("`{}` already has a field `{}`", class.name, field.name),
                        field.name_span,
                    )
                    .with_label("a field may only be named once, base classes included"),
                );
                continue;
            }
            fields.push(FieldInfo { name: field.name.clone(), ty, offset });
            offset += 8;
        }

        let info = &mut declared.table.classes[id.0 as usize];
        info.fields = fields;
        info.size = offset;
    }

    // Storage is the biggest size in the *hierarchy*, and the same for every
    // class in it. That is what lets a value of a base class hold any of its
    // subclasses without being sliced — and, just as importantly, what keeps
    // copying `storage` bytes out of the smallest of them in bounds.
    let ids: Vec<ClassId> = (0..declared.table.classes.len() as u32).map(ClassId).collect();
    for &id in &ids {
        let root = declared.table.root_of(id);
        let storage = ids
            .iter()
            .filter(|&&other| declared.table.root_of(other) == root)
            .map(|&other| declared.class(other).size)
            .max()
            .unwrap_or(VPTR_SIZE);
        declared.table.classes[id.0 as usize].storage = storage;
    }
}

/// Pass 1b: settle every class's vtable, now that signatures exist.
///
/// A method inherited from the base keeps its slot and its implementation; one
/// that overrides keeps the slot and replaces the implementation; a new one
/// takes the next slot. That is the whole of single inheritance — and the
/// reason a subclass's table can be read as its base's.
fn collect_methods(
    program: &Program,
    declared: &mut Declared,
    signatures: &HashMap<usize, Signature>,
    errors: &mut Vec<Diagnostic>,
) {
    for id in layout_order(declared) {
        let class = &program.classes[declarations_index(program, declared, id)];
        let base = declared.class(id).base;

        let mut methods = match base {
            Some(base) => declared.class(base).methods.clone(),
            None => Vec::new(),
        };

        for &at in &class.methods {
            let declaration = &program.functions[at];
            let Some(signature) = signatures.get(&at) else { continue };

            let Some(existing) = methods.iter().position(|m| m.name == declaration.name) else {
                let slot = methods.len();
                methods.push(MethodInfo {
                    name: declaration.name.clone(),
                    function: at,
                    params: signature.params.clone(),
                    ret: signature.ret,
                    slot,
                });
                continue;
            };

            // An override has to be usable wherever the base's method was, so
            // it has to agree with it — otherwise a call through the base
            // would pass the wrong arguments or read the wrong result.
            let inherited = &methods[existing];
            let agrees = inherited.params.len() == signature.params.len()
                && inherited.params[1..] == signature.params[1..]
                && inherited.ret == signature.ret;
            if !agrees {
                errors.push(
                    Diagnostic::new(
                        format!(
                            "`{}` does not match the `{}` it overrides",
                            declaration.name, declaration.name
                        ),
                        declaration.name_span,
                    )
                    .with_label("an override has to take and return what the base's does")
                    .with_note(
                        "a call through the base class cannot know which one it reached",
                        None,
                    ),
                );
            }
            methods[existing].function = at;
        }

        declared.table.classes[id.0 as usize].methods = methods;
    }
}

/// Whether `from` reaches `target` by following base classes, used to catch a
/// cycle before anything walks one.
fn inherits_through(declared: &Declared, from: ClassId, target: ClassId) -> bool {
    let mut at = Some(from);
    // The chain cannot be longer than the number of classes without repeating.
    for _ in 0..=declared.table.classes.len() {
        match at {
            Some(id) if id == target => return true,
            Some(id) => at = declared.class(id).base,
            None => return false,
        }
    }
    true
}

/// Pair each registered class with the declaration it came from.
fn order_of_declarations<'a>(
    program: &'a Program,
    declared: &Declared,
) -> Vec<(ClassId, &'a crate::ast::ClassDecl)> {
    program
        .classes
        .iter()
        .filter_map(|class| declared.class_id(&class.name).map(|id| (id, class)))
        // A duplicate name resolves to the first declaration; laying it out
        // twice would be wrong, so only the first is kept.
        .fold(Vec::new(), |mut kept, entry| {
            if !kept.iter().any(|(id, _)| *id == entry.0) {
                kept.push(entry);
            }
            kept
        })
}

fn declarations_index(program: &Program, declared: &Declared, id: ClassId) -> usize {
    program
        .classes
        .iter()
        .position(|class| declared.class_id(&class.name) == Some(id))
        .expect("every id came from a declaration")
}

/// Class ids with every base before anything that extends it.
fn layout_order(declared: &Declared) -> Vec<ClassId> {
    let mut order: Vec<ClassId> = (0..declared.table.classes.len() as u32).map(ClassId).collect();
    order.sort_by_key(|&id| {
        let mut depth = 0;
        let mut at = declared.class(id).base;
        while let Some(base) = at {
            depth += 1;
            at = declared.class(base).base;
        }
        depth
    });
    order
}

/// Turn a written type into the type it names.
///
/// `None` means the name does not name a type at all — reported here, so a
/// caller can fall back on the recovery type without saying anything further.
fn resolve_type(declared: &mut Declared, ty: &TypeRef, errors: &mut Vec<Diagnostic>) -> Option<Ty> {
    let element = match ty.name.as_str() {
        "int" => Some(Ty::Int),
        "string" => Some(Ty::Str),
        "char" => Some(Ty::Char),
        "bool" => Some(Ty::Bool),
        name => declared
            .enum_id(name)
            .map(Ty::Enum)
            .or_else(|| declared.class_id(name).map(Ty::Class))
            .or_else(|| {
                errors.push(
                    Diagnostic::new(format!("unknown type `{name}`"), ty.span)
                        .with_label("no built-in type, enum or class goes by this name")
                        .with_note("the built-in types are `int`, `string`, `char` and `bool`", None),
                );
                None
            }),
    }?;

    let Some((len, len_span)) = ty.array_len else { return Some(element) };

    // A length is part of the type, so it has to be a length a type could have.
    // `MAX_ARRAY_LEN` is a limit on the *generated code*: an array literal is
    // unrolled into one store per element.
    if len <= 0 || len > MAX_ARRAY_LEN {
        errors.push(
            Diagnostic::new(format!("`{len}` is not a valid array length"), len_span)
                .with_label(if len <= 0 {
                    "an array needs at least one element".to_string()
                } else {
                    format!("at most {MAX_ARRAY_LEN} are supported")
                }),
        );
        return None;
    }
    Some(declared.array_of(element, len as u32))
}

/// How many elements an array may hold.
///
/// Not a limit of the representation but of the *code*: an array is built by
/// storing every element, so a huge one would emit a huge function. A repeat
/// form like `[0; 1000]` lowered to a loop is what would lift it.
pub const MAX_ARRAY_LEN: i64 = 1024;

/// Every conversion the language has, listed wherever one is refused.
///
/// It is a short list on purpose: nothing converts on its own, so this is also
/// the complete answer to "how do I turn this into that".
const CONVERSIONS: &str = "the conversions are `int(c)`, `char(n)`, `string(c)` and `string(n)`";

/// Why a particular number is not a character, which is a different sentence
/// depending on where it lands.
fn scalar_range_label(value: i64) -> String {
    match value {
        0xD800..=0xDFFF => format!(
            "`{value}` is in the surrogate range 55296..=57343, which names no character"
        ),
        _ => "a character's code point is in 0..=1114111".to_string(),
    }
}

/// `` `A` is `` / `` `A` and `B` are ``, so the label agrees with its subject.
fn missing_verb(missing: &[&str]) -> String {
    let verb = if missing.len() == 1 { "is" } else { "are" };
    format!("{} {verb}", list(missing))
}

/// `A`, `A` and `B`, `A`, `B` and `C` — so a list reads as prose.
fn list(items: &[&str]) -> String {
    let quoted: Vec<String> = items.iter().map(|item| format!("`{item}`")).collect();
    match quoted.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
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

    // Pass 0: the declared types, before anything can mention one.
    let mut declared = collect_enums(program, &mut errors);
    collect_classes(program, &mut declared, &mut errors);

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

/// Pass 1: one entry per function name, with the first declaration winning.
fn collect_signatures(
    program: &Program,
    declared: &mut Declared,
    max_params: usize,
    method_signatures: &mut HashMap<usize, Signature>,
    errors: &mut Vec<Diagnostic>,
) -> HashMap<String, Signature> {
    let mut signatures: HashMap<String, Signature> = HashMap::new();

    for (at, function) in program.functions.iter().enumerate() {
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
                .with_note("previous definition", Some(previous)),
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

        let signature = Signature { params, ret, name_span: function.name_span };
        match owner {
            Some(_) => method_signatures.insert(at, signature),
            None => signatures.insert(function.name.clone(), signature),
        };
    }

    signatures
}

/// `main` must exist, take nothing and return nothing: it is what the C runtime
/// calls, and [`crate::codegen`] returns 0 from it.
fn check_entry_point(
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
fn const_int(expr: &Expr) -> Option<i64> {
    match &expr.kind {
        ExprKind::Int(v) => Some(*v),
        ExprKind::Neg(operand) => const_int(operand)?.checked_neg(),
        ExprKind::Bin { op, lhs, rhs } => op.apply(const_int(lhs)?, const_int(rhs)?),
        _ => None,
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
fn diverges(block: &Block) -> bool {
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
fn match_arms(expr: &Expr) -> Option<&[MatchArm]> {
    match &expr.kind {
        ExprKind::Match { arms, .. } => Some(arms),
        _ => None,
    }
}

/// What the whole program shares: one type table, one signature table, one list
/// of diagnostics.
struct Checker<'a> {
    types: Types,
    /// Every signature in the program, immutable once pass 1 is done.
    signatures: &'a HashMap<String, Signature>,
    /// Every declared enum, immutable once pass 0 is done.
    declared: Declared,
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
    /// Where this function sits in the program, which is how its signature is
    /// found whether it is a method or not.
    at: usize,
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
    /// `at` is the function's place in the program, which is how a method is
    /// reached: its signature lives in its class rather than in the program's
    /// namespace, and only the position identifies it either way.
    fn new(shared: &'c mut Checker<'a>, at: usize, function: &FnDecl) -> FnChecker<'a, 'c> {
        // Pass 1 resolved this already; re-resolving would report an unknown
        // return type a second time.
        let ret = shared.types.fn_ret[at];
        FnChecker {
            shared,
            at,
            ret,
            ret_span: function.ret_span,
            scopes: Vec::new(),
            undeclared: HashSet::new(),
            loop_depth: 0,
        }
    }

    fn error(&mut self, diagnostic: Diagnostic) {
        self.shared.errors.push(diagnostic);
    }

    /// Resolve a written type, falling back on the recovery type when it names
    /// nothing. The mistake is reported by [`resolve_type`] itself.
    fn resolve(&mut self, ty: &TypeRef) -> Ty {
        resolve_type(&mut self.shared.declared, ty, &mut self.shared.errors).unwrap_or(Ty::Int)
    }

    /// A type's name. [`Ty`] cannot answer this alone — an enum's name is the
    /// program's, not the compiler's — so every diagnostic asks here.
    fn ty_name(&self, ty: Ty) -> String {
        ty.name(&self.shared.declared.table).to_string()
    }

    /// The same with its indefinite article, for prose.
    fn ty_article(&self, ty: Ty) -> String {
        ty.with_article(&self.shared.declared.table)
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
        // Pass 1 resolved these too, and reported any that named nothing.
        let params = self.shared.types.fn_params[self.at].clone();
        for (param, ty) in function.params.iter().zip(params) {
            self.declare(&param.name, ty, param.name_span, "a parameter");
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
                .with_label(format!("expected bool, found {}", self.ty_name(ty)))
                .with_note("comparisons like `i < 10` produce a bool", None),
            );
        }
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Decl { id, ty, name, name_span, init } => {
                let declared = self.resolve(ty);
                self.record(*id, declared);
                let actual = self.expr(init);
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
            Stmt::Print { value, .. } => {
                let ty = self.expr(value);
                // An array has no rendering, and printing its address would
                // answer a question nobody asked.
                if !ty.fits_in_a_register() {
                    self.error(
                        Diagnostic::new(
                            format!("cannot print {}", self.ty_article(ty)),
                            value.span,
                        )
                        .with_label("`print` takes one value, and an array is many")
                        .with_note("print the elements in a loop instead", None),
                    );
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

    /// Reject arithmetic the machine could not perform, when the operands are
    /// known here and now.
    ///
    /// This is the compile-time half of a pair. What it catches, it catches
    /// before the program is ever built; what it cannot see — an operand that
    /// is a variable, a parameter or a call — becomes a guard in the emitted
    /// code instead, and reports at the same moment with the same wording. The
    /// language has one rule, checked in whichever of the two places can.
    fn check_arithmetic(&mut self, expr: &Expr, op: BinOp, lhs: &Expr, rhs: &Expr) {
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

    /// Check `Color::Red` and answer the type it has, which is the enum's.
    ///
    /// Both halves can be wrong independently, and are reported separately: the
    /// enum name underlines one, the variant the other.
    fn variant(&mut self, name: &str, span: Span, variant: &str, variant_span: Span) -> Ty {
        let Some(id) = self.shared.declared.enum_id(name) else {
            self.error(
                Diagnostic::new(format!("unknown enum `{name}`"), span)
                    .with_label("no enum goes by this name")
                    .with_note("a variant is always written `Enum::Variant`", None),
            );
            return Ty::Int;
        };

        if self.shared.declared.info(id).tag(variant).is_none() {
            let info = self.shared.declared.info(id);
            let known: Vec<&str> = info.variants.iter().map(String::as_str).collect();
            let note = format!("`{name}` has {}", list(&known));
            self.error(
                Diagnostic::new(format!("`{name}` has no variant `{variant}`"), variant_span)
                    .with_label("not one of its variants")
                    .with_note(note, None),
            );
        }
        // Even a misspelt variant has the enum's type: that much was written
        // down, and reporting the same mistake again as a type error would not
        // help anybody.
        Ty::Enum(id)
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
    fn match_expr(&mut self, expr: &Expr, as_statement: bool) -> Option<Ty> {
        let ExprKind::Match { keyword, scrutinee, arms } = &expr.kind else {
            unreachable!("the parser only builds `Stmt::Match` around a match");
        };
        let span = *keyword;

        let ty = self.expr(scrutinee);
        let Ty::Enum(id) = ty else {
            self.error(
                Diagnostic::new(
                    format!("cannot match on {}", self.ty_article(ty)),
                    scrutinee.span,
                )
                .with_label("a match needs an enum, whose variants can be counted")
                .with_note("`if` is what asks a question about an int, string or bool", None),
            );
            // The arms are still checked: their own mistakes are worth
            // reporting whatever the scrutinee turned out to be.
            for arm in arms {
                self.arm_body(&arm.body);
            }
            return None;
        };

        // Where the first arm for each variant was, in declaration order, which
        // is the order both diagnostics below want to talk about them in.
        let variants = self.shared.declared.info(id).variants.clone();
        let mut covered: Vec<Option<Span>> = vec![None; variants.len()];

        // An arm whose pattern named nothing leaves a hole that is not the
        // program's real mistake. Reporting what it failed to cover on top of
        // that would be saying the same thing twice, and the derived complaint
        // sorts ahead of the true one.
        let mut readable = true;
        for arm in arms {
            readable &= self.match_arm(id, arm, &variants, &mut covered);
        }

        let missing: Vec<&str> = variants
            .iter()
            .zip(&covered)
            .filter(|(_, seen)| seen.is_none())
            .map(|(name, _)| name.as_str())
            .collect();
        if !missing.is_empty() && readable {
            let name = self.ty_name(ty);
            self.error(
                Diagnostic::new(
                    format!("this match does not cover every variant of `{name}`"),
                    span,
                )
                .with_label(format!("{} not handled", missing_verb(&missing)))
                .with_note(
                    "every variant needs an arm; TinyC has no catch-all pattern, so that \
                     adding a variant cannot be quietly ignored",
                    None,
                ),
            );
        }

        self.match_value(span, arms, as_statement)
    }

    /// Check `x = value`, `xs[i] = value` and `p.f = value`.
    ///
    /// The three differ only in what type the target has, which is what
    /// [`Self::place_type`] works out. Everything after that — the name exists,
    /// the value agrees — is the same question asked once.
    fn assign(&mut self, target: &Place, value: &Expr) {
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
        let actual = self.expr(value);
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
    fn place_type(&mut self, place: &Place) -> Option<Ty> {
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
    fn coerces(&self, from: Ty, to: Ty) -> bool {
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
    fn join(&self, a: Ty, b: Ty) -> Option<Ty> {
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
    fn field_type(&mut self, object: Ty, name: &str, at: Span) -> Option<Ty> {
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
        let note = match known.is_empty() {
            true => format!("`{}` has no fields", class.name),
            false => format!("`{}` has {}", class.name, list(&known)),
        };
        let class = class.name.clone();
        self.error(
            Diagnostic::new(format!("`{class}` has no field `{name}`"), at)
                .with_label("not one of its fields")
                .with_note(note, None),
        );
        None
    }

    /// Check `[1, 2, 3]` and answer the array type it has.
    ///
    /// Its length is its element count — there is nothing to infer and nothing
    /// to declare. What a declaration does is *agree* with it, and the mismatch
    /// is caught where the two meet.
    fn array_literal(&mut self, elements: &[Expr], span: Span) -> Ty {
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
        if matches!(first, Ty::Array(_)) {
            // Pointed at the whole literal rather than at one element: what
            // follows is a declaration complaining that the type it got is not
            // the type it wanted, and the two sort together with the real
            // mistake first.
            self.error(
                Diagnostic::new(
                    format!("cannot make an array of {}", self.ty_article(first)),
                    span,
                )
                .with_label("an array's elements may not themselves be arrays")
                .with_note("arrays do not nest yet", None),
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
    fn element_type(&mut self, ty: Ty, declared_span: Span, name: &str, at: Span) -> Option<Ty> {
        match ty {
            Ty::Array(id) => Some(self.shared.declared.table.array(id).elem),
            // A string is a run of characters, so indexing one produces a
            // character — never a byte and never a string of length one.
            Ty::Str => Some(Ty::Char),
            other => {
                let mut diagnostic =
                    Diagnostic::new(format!("cannot index {}", self.ty_article(other)), at)
                        .with_label("only an array or a string has elements");
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
    fn object_literal(
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
            let actual = self.expr(&field.value);
            let Some(at) = declared.iter().position(|f| f.name == field.name) else {
                readable = false;
                let known: Vec<&str> = declared.iter().map(|f| f.name.as_str()).collect();
                let note = match known.is_empty() {
                    true => format!("`{class}` has no fields"),
                    false => format!("`{class}` has {}", list(&known)),
                };
                self.error(
                    Diagnostic::new(format!("`{class}` has no field `{}`", field.name), field.name_span)
                        .with_label("not one of its fields")
                        .with_note(note, None),
                );
                continue;
            };

            if let Some(previous) = given[at] {
                self.error(
                    Diagnostic::new(format!("`{}` is given twice", field.name), field.name_span)
                        .with_label("a field takes one value")
                        .with_note("given here", Some(previous)),
                );
                continue;
            }
            given[at] = Some(field.name_span);

            let expected = declared[at].ty;
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

    /// Check `receiver.method(args)` and answer the type it produces.
    ///
    /// `as_statement` says whether producing nothing is acceptable here, the
    /// same question [`Self::call`] asks of a `void` function.
    fn method_call(&mut self, expr: &Expr, as_statement: bool) -> Option<Ty> {
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
            let note = match known.is_empty() {
                true => format!("`{}` has no methods", class.name),
                false => format!("`{}` has {}", class.name, list(&known)),
            };
            let class = class.name.clone();
            self.error(
                Diagnostic::new(format!("`{class}` has no method `{name}`"), *name_span)
                    .with_label("not one of its methods")
                    .with_note(note, None),
            );
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
    fn convert(&mut self, to: Prim, value: &Expr, span: Span) -> Ty {
        let from = self.expr(value);
        let target = to.ty();

        match (from, target) {
            // A character's code point, and the character with that code point.
            // Two directions, spelled apart, so neither can happen by accident.
            (Ty::Char, Ty::Int) => {}
            // Into a string: a character on its own, or a number written out in
            // decimal. Both exist because `+` converts nothing, so a message
            // with a value in it has to say where the value became text.
            (Ty::Char | Ty::Int, Ty::Str) => {}
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
                    .with_note(CONVERSIONS, None),
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
    fn index(&mut self, array: Option<Ty>, index: &Expr) {
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

    /// Check an arm's body, whichever of the two shapes it has.
    fn arm_body(&mut self, body: &ArmBody) {
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
    fn match_value(&mut self, span: Span, arms: &[MatchArm], as_statement: bool) -> Option<Ty> {
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
                    Diagnostic::new("this arm produces no value", arm.variant_span)
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

    /// One arm: it must name the scrutinee's enum, name a variant of it, and be
    /// the first arm to do so.
    ///
    /// Answers whether the pattern named a variant at all — a duplicate still
    /// did, and so still says something about coverage.
    fn match_arm(
        &mut self,
        id: EnumId,
        arm: &MatchArm,
        variants: &[String],
        covered: &mut [Option<Span>],
    ) -> bool {
        // Always check the body, whatever the pattern turns out to be.
        let pattern = self.arm_tag(id, arm, variants);
        self.arm_body(&arm.body);

        let Some(tag) = pattern else { return false };
        let at = tag as usize;
        if let Some(previous) = covered[at] {
            self.error(
                Diagnostic::new(
                    format!("`{}` is already covered", variants[at]),
                    arm.variant_span,
                )
                .with_label("this arm can never run")
                .with_note("covered here", Some(previous)),
            );
            return true;
        }
        covered[at] = Some(arm.variant_span);
        true
    }

    /// The tag an arm's pattern selects, or `None` when the pattern does not
    /// name a variant of this enum — reported here.
    fn arm_tag(&mut self, id: EnumId, arm: &MatchArm, variants: &[String]) -> Option<i64> {
        let expected = self.shared.declared.info(id).name.clone();
        if arm.enum_name != expected {
            self.error(
                Diagnostic::new(
                    format!("this arm matches `{}`, but the value is a `{expected}`", arm.enum_name),
                    arm.enum_span,
                )
                .with_label(format!("expected a variant of `{expected}`")),
            );
            return None;
        }

        let tag = self.shared.declared.info(id).tag(&arm.variant);
        if tag.is_none() {
            let known: Vec<&str> = variants.iter().map(String::as_str).collect();
            self.error(
                Diagnostic::new(
                    format!("`{expected}` has no variant `{}`", arm.variant),
                    arm.variant_span,
                )
                .with_label("not one of its variants")
                .with_note(format!("`{expected}` has {}", list(&known)), None),
            );
        }
        tag
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
            ExprKind::Char(_) => Ty::Char,
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
                            format!("cannot apply `-` to {} value", self.ty_article(inner)),
                            operand.span,
                        )
                        .with_label(format!("expected int, found {}", self.ty_name(inner))),
                    );
                }
                Ty::Int
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

                // Everything else is int-only; report the offending operand.
                for (ty, operand) in [(lhs_ty, lhs), (rhs_ty, rhs)] {
                    if ty != Ty::Int {
                        let mut diagnostic = Diagnostic::new(
                            format!(
                                "cannot apply `{}` to `{}` and `{}`",
                                op.symbol(),
                                self.ty_name(lhs_ty),
                                self.ty_name(rhs_ty)
                            ),
                            operand.span,
                        )
                        .with_label(format!("expected int, found {}", self.ty_name(ty)));
                        // A character is a character, not a small number. The
                        // way to do arithmetic on one is to say so.
                        if ty == Ty::Char {
                            diagnostic = diagnostic.with_note(
                                "`int(c)` is a character's code point, and `char(n)` goes back",
                                None,
                            );
                        }
                        self.error(diagnostic);
                    }
                }

                self.check_arithmetic(expr, *op, lhs, rhs);
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
                                self.ty_name(lhs_ty),
                                self.ty_name(rhs_ty)
                            ),
                            rhs.span,
                        )
                        .with_label(format!("expected {}, found {}", self.ty_name(lhs_ty), self.ty_name(rhs_ty))),
                    );
                } else if !lhs_ty.has_equality() || (op.is_ordering() && !lhs_ty.is_ordered()) {
                    // Ints and characters are ordered. Everything else that can
                    // be compared at all answers only `==`, since that is a
                    // question about *which value it is*; an array or an object
                    // answers nothing, because comparing addresses would quietly
                    // answer something else again.
                    let label = if !lhs_ty.has_equality() {
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
            ExprKind::Variant { enum_name, enum_span, variant, variant_span } => {
                self.variant(enum_name, *enum_span, variant, *variant_span)
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
                if !matches!(of, Ty::Array(_) | Ty::Str) {
                    self.error(
                        Diagnostic::new(
                            format!(
                                "`len` needs an array or a string, but this is {}",
                                self.ty_article(of)
                            ),
                            *span,
                        )
                        .with_label("only an array or a string has a length"),
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
        // Two strings answer `==`, but not `<`: sorting them is a question
        // about a language rather than about the characters.
        let error = &errors_in_main("print(\"a\" < \"b\");")[0];
        assert!(error.message.contains("cannot be compared"), "{}", error.message);
        assert!(error.note.is_some(), "the refusal explains itself");
    }

    #[test]
    fn two_strings_can_be_compared_for_equality() {
        assert!(check_main("print(\"a\" == \"b\");\nprint(\"a\" != \"b\");").is_ok());
    }

    // -- strings and characters --------------------------------------------

    #[test]
    fn joining_two_strings_produces_a_string() {
        assert!(check_main("string s = \"a\" + \"b\";\nprint(s + s);").is_ok());
    }

    #[test]
    fn rejects_joining_a_string_to_anything_else() {
        // The mistake every language with a looser `+` accepts, and the note
        // says how to write what was meant.
        let errors = errors_in_main("print(\"n = \" + 1);");
        assert!(errors[0].message.contains("cannot apply `+`"), "{}", errors[0].message);
        assert!(errors[0].note.as_ref().is_some_and(|(text, _)| text.contains("string(n)")));
    }

    #[test]
    fn a_string_has_a_length_and_so_does_an_array() {
        assert!(check_main("print(len(\"abc\"));").is_ok());
        let errors = errors_in_main("print(len(1));");
        assert!(errors[0].message.contains("array or a string"), "{}", errors[0].message);
    }

    #[test]
    fn indexing_a_string_produces_a_character() {
        assert!(check_main("char c = \"abc\"[0];\nprint(c);").is_ok());
        // Never an int, and never a string of length one: no conversion is
        // implied anywhere, so the declared type has to agree.
        let errors = errors_in_main("int n = \"abc\"[0];");
        assert!(errors[0].message.contains("with a `char` value"), "{}", errors[0].message);
    }

    #[test]
    fn a_string_cannot_be_written_into() {
        // Immutability is what makes sharing a string unobservable, so this is
        // load-bearing rather than a restriction.
        let errors = errors_in_main("string s = \"abc\";\ns[0] = 'x';");
        assert!(errors[0].message.contains("cannot be modified"), "{}", errors[0].message);
    }

    #[test]
    fn characters_compare_but_do_not_do_arithmetic() {
        assert!(check_main("print('a' == 'b');\nprint('a' < 'b');").is_ok());
        let errors = errors_in_main("print('a' + 1);");
        assert!(errors[0].message.contains("cannot apply `+`"), "{}", errors[0].message);
        assert!(errors[0].note.as_ref().is_some_and(|(text, _)| text.contains("int(c)")));
    }

    #[test]
    fn the_four_conversions_are_accepted_and_nothing_else_is() {
        assert!(check_main("print(int('a'));").is_ok());
        assert!(check_main("print(char(65));").is_ok());
        assert!(check_main("print(string('a'));").is_ok());
        assert!(check_main("print(string(65));").is_ok());

        let errors = errors_in_main("print(int(true));");
        assert!(errors[0].message.contains("no conversion from `bool`"), "{}", errors[0].message);
    }

    #[test]
    fn a_conversion_to_its_own_type_is_refused_rather_than_ignored() {
        let errors = errors_in_main("print(int(1));");
        assert!(errors[0].message.contains("already an `int`"), "{}", errors[0].message);
    }

    #[test]
    fn a_constant_that_names_no_character_is_settled_at_compile_time() {
        // The same bargain a constant index strikes: what reaches the emitted
        // code is only ever a value the running program alone knows.
        for bad in ["1114112", "55296", "0 - 1"] {
            let errors = errors_in_main(&format!("print(char({bad}));"));
            assert!(errors[0].message.contains("not a character"), "{bad}: {}", errors[0].message);
        }
        assert!(check_main("print(char(1114111));").is_ok());
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

    #[test]
    fn the_remainder_operator_is_int_only_like_the_rest_of_arithmetic() {
        assert!(check_main("print(17 % 5);").is_ok());
        assert!(check_main("bool even = 4 % 2 == 0;\nprint(even);").is_ok());
        assert!(errors_in_main("print(true % 2);")[0].message.contains("cannot apply `%`"));
    }

    #[test]
    fn a_remainder_by_a_literal_zero_is_rejected_like_a_division() {
        // It is the same instruction underneath, and traps the same way.
        assert!(errors_in_main("print(1 % 0);")[0].message.contains("division by zero"));
    }

    // -- logical operators -------------------------------------------------

    #[test]
    fn negation_takes_a_bool_and_produces_one() {
        assert!(check_main("bool ok = true;\nprint(!ok);").is_ok());
        assert!(check_main("if (!(1 < 2)) {\n}").is_ok());
        assert!(check_main("bool a = !!false;\nprint(a);").is_ok());
    }

    #[test]
    fn rejects_negating_anything_that_is_not_a_bool() {
        // `!n` on an int is the habit from languages with truthiness, so the
        // diagnostic says there is no implicit truth test.
        let errors = errors_in_main("int n = 1;\nprint(!n);");
        assert!(errors[0].message.contains("cannot apply `!`"), "{}", errors[0].message);
        assert!(errors[0].note.is_some(), "{errors:#?}");
        assert!(errors_in_main("print(!\"a\");")[0].message.contains("cannot apply `!`"));
    }

    #[test]
    fn a_negation_is_a_bool_wherever_one_is_wanted() {
        assert!(check_main("bool ok = true;\nwhile (!ok) {\n  ok = true;\n}").is_ok());
        assert!(errors_in_main("bool ok = true;\nint n = !ok;")[0].message.contains("cannot initialize"));
    }


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
        // The divisor alone settles it, however unknown the dividend is.
        assert!(errors_in_main("int n = 1;\nprint(n / 0);")[0].message.contains("division by zero"));
        assert!(errors_in_main("int n = 1;\nprint(n % 0);")[0].message.contains("division by zero"));
    }

    // -- arithmetic that has no answer -------------------------------------

    #[test]
    fn rejects_arithmetic_that_overflows_an_int() {
        for (body, noun) in [
            ("print(9223372036854775807 + 1);", "addition"),
            ("print(0 - 9223372036854775807 - 1 - 1);", "subtraction"),
            ("print(9223372036854775807 * 2);", "multiplication"),
        ] {
            let errors = errors_in_main(body);
            assert!(
                errors[0].message.contains(&format!("this {noun} overflows")),
                "{body}: {}",
                errors[0].message
            );
        }
    }

    #[test]
    fn an_overflow_diagnostic_names_the_value_that_did_not_fit() {
        let errors = errors_in_main("print(9223372036854775807 + 1);");
        assert!(
            errors[0].label.as_deref().unwrap().contains("9223372036854775808"),
            "{errors:#?}"
        );
    }

    #[test]
    fn an_overflow_is_caught_however_deeply_the_constants_are_nested() {
        // A check that only looked at the two literals either side of one
        // operator would miss this: the left operand is itself an expression.
        let errors = errors_in_main("print(2 * 2 * 4611686018427387904);");
        assert!(errors[0].message.contains("overflows"), "{}", errors[0].message);
        // The same reach makes a hidden zero divisor visible too.
        assert!(errors_in_main("print(1 / (3 - 3));")[0].message.contains("division by zero"));
    }

    #[test]
    fn one_overflow_is_reported_once_however_much_is_built_on_it() {
        // The operators above an impossible one cannot evaluate it either, so
        // they say nothing rather than repeating the complaint.
        let errors = errors_in_main("print(9223372036854775807 + 1 + 1 + 1);");
        assert_eq!(errors.len(), 1, "{errors:#?}");
    }

    #[test]
    fn arithmetic_that_fits_is_left_alone() {
        // Including right up to the edge, which a check written with the wrong
        // comparison would reject.
        assert!(check_main("print(9223372036854775806 + 1);").is_ok());
        assert!(check_main("print(0 - 9223372036854775807 - 1);").is_ok());
        assert!(check_main("print(4611686018427387903 * 2);").is_ok());
    }

    #[test]
    fn an_overflow_that_depends_on_a_variable_is_left_to_the_runtime() {
        // `sema` never looks a variable up, so this compiles — and the emitted
        // code carries the guard that catches it instead.
        assert!(check_main("int n = 9223372036854775807;\nprint(n + 1);").is_ok());
    }

    #[test]
    fn collects_several_errors() {
        assert_eq!(errors_in_main("print(a);\nprint(b);").len(), 2);
    }

    // -- classes -----------------------------------------------------------

    /// A `Shape`/`Circle` hierarchy and a `main`, so the tests about classes
    /// stay about classes.
    fn check_shapes(body: &str) -> Result<Types> {
        check_src(&format!(
            "class Shape {{\n  fn area(self) -> int {{ return 0; }}\n}}\n\
             class Circle : Shape {{\n  int r;\n  \
             fn area(self) -> int {{ return 3 * self.r * self.r; }}\n}}\n\
             fn main() {{\n{body}\n}}\n"
        ))
    }

    fn shape_errors(body: &str) -> Vec<Diagnostic> {
        check_shapes(body).unwrap_err()
    }

    #[test]
    fn accepts_a_class_built_read_and_dispatched_on() {
        assert!(
            check_src(
                "class Shape {\n  fn area(self) -> int { return 0; }\n}\n\
                 class Circle : Shape {\n  int r;\n  \
                 fn area(self) -> int { return 3 * self.r * self.r; }\n}\n\
                 fn report(Shape s) {\n  print(s.area());\n}\n\
                 fn main() {\n  Circle c = Circle { r: 5 };\n  report(c);\n}"
            )
            .is_ok()
        );
    }

    #[test]
    fn a_subclass_may_stand_for_its_base_but_not_the_other_way() {
        // The one widening in the language, and it only goes one way: every
        // `Circle` is a `Shape`, and no `Shape` is known to be a `Circle`.
        assert!(check_shapes("Circle c = Circle { r: 1 };\nShape s = c;").is_ok());
        let errors = check_src(
            "class Shape {\n  fn area(self) -> int { return 0; }\n}\n\
             class Circle : Shape {\n  int r;\n  \
             fn area(self) -> int { return self.r; }\n}\n\
             fn take(Circle c) {\n}\n\
             fn main() {\n  Shape s = Circle { r: 1 };\n  take(s);\n}",
        )
        .unwrap_err();
        assert!(errors[0].message.contains("cannot pass"), "{}", errors[0].message);
    }

    #[test]
    fn a_field_is_read_and_written_through_the_object() {
        assert!(
            check_shapes("Circle c = Circle { r: 1 };\nc.r = 2;\nprint(c.r);").is_ok()
        );
        assert!(shape_errors("Circle c = Circle { r: 1 };\nc.r = true;")[0]
            .message
            .contains("cannot assign"));
    }

    #[test]
    fn rejects_an_unknown_field_or_method_and_lists_the_real_ones() {
        let errors = shape_errors("Circle c = Circle { r: 1 };\nprint(c.nope);");
        assert!(errors[0].message.contains("has no field `nope`"), "{}", errors[0].message);
        assert!(errors[0].note.as_ref().unwrap().0.contains("`r`"), "{errors:#?}");

        let errors = shape_errors("Circle c = Circle { r: 1 };\nc.zap();");
        assert!(errors[0].message.contains("has no method `zap`"), "{}", errors[0].message);
        assert!(errors[0].note.as_ref().unwrap().0.contains("`area`"), "{errors:#?}");
    }

    #[test]
    fn an_object_is_complete_or_it_does_not_exist() {
        // No default and no partial object, which is what removes the question
        // `null` would have answered.
        let errors = shape_errors("Circle c = Circle { };");
        assert!(errors[0].message.contains("missing a field"), "{}", errors[0].message);
        assert!(shape_errors("Circle c = Circle { r: 1, r: 2 };")[0]
            .message
            .contains("is given twice"));
        assert!(shape_errors("Circle c = Circle { q: 1 };")[0].message.contains("has no field `q`"));
    }

    #[test]
    fn an_inherited_field_is_named_in_the_literal_too() {
        assert!(
            check_src(
                "class Base {\n  int a;\n}\nclass Derived : Base {\n  int b;\n}\n\
                 fn main() {\n  Derived d = Derived { a: 1, b: 2 };\n  print(d.a + d.b);\n}"
            )
            .is_ok()
        );
        let errors = check_src(
            "class Base {\n  int a;\n}\nclass Derived : Base {\n  int b;\n}\n\
             fn main() {\n  Derived d = Derived { b: 2 };\n}",
        )
        .unwrap_err();
        assert!(errors[0].message.contains("missing a field"), "{}", errors[0].message);
    }

    #[test]
    fn a_base_class_is_laid_out_before_what_extends_it() {
        // Which is what makes the upcast free, and what a subclass's field
        // offsets depend on — so a derived class declared *before* its base
        // still has to come out right.
        assert!(
            check_src(
                "class Derived : Base {\n  int b;\n}\nclass Base {\n  int a;\n}\n\
                 fn main() {\n  Derived d = Derived { a: 1, b: 2 };\n  print(d.a);\n}"
            )
            .is_ok()
        );
    }

    #[test]
    fn rejects_a_class_that_is_its_own_ancestor() {
        for src in [
            "class A : A {\n}\nfn main() {\n}",
            "class A : B {\n}\nclass B : A {\n}\nfn main() {\n}",
        ] {
            let errors = check_src(src).unwrap_err();
            assert!(errors[0].message.contains("inherits from itself"), "{src}: {}", errors[0].message);
        }
    }

    #[test]
    fn rejects_an_unknown_base_and_a_duplicate_class() {
        assert!(
            check_src("class A : Nope {\n}\nfn main() {\n}").unwrap_err()[0]
                .message
                .contains("unknown class `Nope`")
        );
        let errors = check_src("class A {\n}\nclass A {\n}\nfn main() {\n}").unwrap_err();
        assert!(errors[0].message.contains("already declared"), "{}", errors[0].message);
        assert!(errors[0].note.as_ref().unwrap().1.is_some());
    }

    #[test]
    fn a_field_may_not_be_named_twice_base_included() {
        let errors = check_src("class A {\n  int x;\n  int x;\n}\nfn main() {\n}").unwrap_err();
        assert!(errors[0].message.contains("already has a field"), "{}", errors[0].message);
        let errors =
            check_src("class A {\n  int x;\n}\nclass B : A {\n  int x;\n}\nfn main() {\n}")
                .unwrap_err();
        assert!(errors[0].message.contains("already has a field"), "{}", errors[0].message);
    }

    #[test]
    fn an_override_has_to_match_what_it_overrides() {
        let errors = check_src(
            "class A {\n  fn f(self) -> int { return 1; }\n}\n\
             class B : A {\n  fn f(self) -> string { return \"x\"; }\n}\nfn main() {\n}",
        )
        .unwrap_err();
        assert!(errors[0].message.contains("does not match"), "{}", errors[0].message);
    }

    #[test]
    fn two_classes_may_both_have_a_method_of_the_same_name() {
        // A method's name lives in its class, not in the program.
        assert!(
            check_src(
                "class A {\n  fn go(self) -> int { return 1; }\n}\n\
                 class B {\n  fn go(self) -> int { return 2; }\n}\n\
                 fn go() -> int {\n  return 3;\n}\n\
                 fn main() {\n  A a = A { };\n  print(a.go() + go());\n}"
            )
            .is_ok()
        );
    }

    #[test]
    fn rejects_self_where_there_is_no_receiver() {
        let errors = check_src("fn f(self) {\n}\nfn main() {\n}").unwrap_err();
        assert!(errors[0].message.contains("`self` outside a class"), "{}", errors[0].message);
        let errors =
            check_src("class A {\n  fn f(int n, self) {\n  }\n}\nfn main() {\n}").unwrap_err();
        assert!(errors[0].message.contains("`self` must come first"), "{}", errors[0].message);
    }

    // -- what objects are not allowed to do --------------------------------

    #[test]
    fn an_object_may_be_returned_and_may_be_any_of_its_hierarchy() {
        assert!(
            check_src(
                "class Shape {\n  fn area(self) -> int { return 0; }\n}\n\
                 class Circle : Shape {\n  int r;\n  fn area(self) -> int { return self.r; }\n}\n\
                 fn make(int n) -> Shape {\n  return Circle { r: n };\n}\n\
                 fn main() {\n  Shape s = make(3);\n  print(s.area());\n}"
            )
            .is_ok()
        );
    }

    #[test]
    fn rejects_printing_and_comparing_an_object() {
        assert!(shape_errors("Circle c = Circle { r: 1 };\nprint(c);")[0]
            .message
            .contains("cannot print"));
        assert!(shape_errors("Circle c = Circle { r: 1 };\nprint(c == c);")[0]
            .message
            .contains("cannot be compared"));
    }

    #[test]
    fn a_field_cannot_be_an_aggregate_yet() {
        for src in [
            "class A {\n  int[2] xs;\n}\nfn main() {\n}",
            "class B {\n  int x;\n}\nclass A {\n  B b;\n}\nfn main() {\n}",
        ] {
            let errors = check_src(src).unwrap_err();
            assert!(errors[0].message.contains("a field cannot be"), "{src}: {}", errors[0].message);
        }
    }

    #[test]
    fn rejects_a_field_or_method_on_something_that_is_not_an_object() {
        assert!(errors_in_main("int n = 1;\nprint(n.x);")[0]
            .message
            .contains("cannot read a field"));
        assert!(errors_in_main("int n = 1;\nn.f();")[0]
            .message
            .contains("cannot call a method"));
    }

    // -- arrays ------------------------------------------------------------

    #[test]
    fn accepts_an_array_declared_read_written_and_passed() {
        assert!(
            check_src(
                "fn total(int[3] xs) -> int {\n  int sum = 0;\n  \
                 for (int i = 0; i < len(xs); i = i + 1) {\n    sum = sum + xs[i];\n  }\n  \
                 return sum;\n}\n\
                 fn main() {\n  int[3] xs = [1, 2, 3];\n  xs[0] = 9;\n  print(total(xs));\n}"
            )
            .is_ok()
        );
    }

    #[test]
    fn the_length_is_part_of_the_type() {
        // So two arrays of different lengths are different types, and a
        // declaration and its literal have to agree.
        assert!(errors_in_main("int[2] xs = [1, 2, 3];")[0].message.contains("cannot initialize"));
        let errors = check_src(
            "fn f(int[3] xs) {\n}\nfn main() {\n  int[4] xs = [1, 2, 3, 4];\n  f(xs);\n}",
        )
        .unwrap_err();
        assert!(errors[0].message.contains("cannot pass"), "{}", errors[0].message);
    }

    #[test]
    fn every_element_of_a_literal_has_to_agree() {
        let errors = errors_in_main("int[3] xs = [1, true, 3];");
        assert!(errors[0].message.contains("but the ones before it are"), "{}", errors[0].message);
        assert!(errors[0].note.as_ref().unwrap().1.is_some(), "and points at the first");
    }

    #[test]
    fn elements_of_one_hierarchy_settle_on_their_common_ancestor() {
        // The first element does not decide for the rest, which is what makes
        // a mixed collection possible at all.
        assert!(
            check_src(
                "class Shape {\n  fn area(self) -> int { return 0; }\n}\n\
                 class Circle : Shape {\n  int r;\n  fn area(self) -> int { return self.r; }\n}\n\
                 class Rect : Shape {\n  int w;\n  fn area(self) -> int { return self.w; }\n}\n\
                 fn main() {\n  \
                 Shape[2] all = [Circle { r: 1 }, Rect { w: 2 }];\n  print(all[0].area());\n}"
            )
            .is_ok()
        );
    }

    #[test]
    fn arrays_stay_invariant_even_though_their_elements_widen() {
        // A `Circle[2]` is not a `Shape[2]`: writing a `Rect` through the
        // second would put one in the first.
        let errors = check_src(
            "class Shape {\n  fn area(self) -> int { return 0; }\n}\n\
             class Circle : Shape {\n  int r;\n  fn area(self) -> int { return self.r; }\n}\n\
             fn take(Shape[2] s) {\n}\n\
             fn main() {\n  Circle[2] cs = [Circle { r: 1 }, Circle { r: 2 }];\n  take(cs);\n}",
        )
        .unwrap_err();
        assert!(errors[0].message.contains("cannot pass"), "{}", errors[0].message);
    }

    #[test]
    fn arrays_of_every_element_type_work() {
        assert!(check_main("string[2] ws = [\"a\", \"b\"];\nprint(ws[0]);").is_ok());
        assert!(check_main("bool[2] bs = [true, false];\nprint(bs[1]);").is_ok());
        assert!(
            check_src("enum A { X, Y }\nfn main() {\n  A[2] as = [A::X, A::Y];\n  print(as[0]);\n}")
                .is_ok()
        );
    }

    #[test]
    fn arrays_do_not_nest_yet() {
        let errors = errors_in_main("int[2] a = [1, 2];\nint[1] b = [a];");
        assert!(errors[0].message.contains("cannot make an array of"), "{}", errors[0].message);
    }

    // -- what arrays are not allowed to do ---------------------------------

    #[test]
    fn an_aggregate_may_be_returned_because_the_caller_owns_the_room() {
        // Nothing is handed outward: the caller reserves the room and passes
        // its address in, so the callee copies into what already belongs to it.
        assert!(
            check_src(
                "fn make() -> int[2] {\n  int[2] xs = [1, 2];\n  return xs;\n}\n\
                 fn main() {\n  int[2] ys = make();\n  print(ys[0]);\n}"
            )
            .is_ok()
        );
    }

    #[test]
    fn the_hidden_address_costs_one_of_the_argument_registers() {
        // Four parameters is fine for a function that returns a value, and one
        // too many for a function that returns room.
        assert!(
            check_src("fn f(int a, int b, int c, int d) -> int {\n  return a;\n}\nfn main() {\n}")
                .is_ok()
        );
        let errors = check_src(
            "fn f(int a, int b, int c, int d) -> int[2] {\n  int[2] xs = [1, 2];\n  \
             return xs;\n}\nfn main() {\n}",
        )
        .unwrap_err();
        assert!(errors[0].message.contains("at most 3 are supported"), "{}", errors[0].message);
        assert!(
            errors[0].note.as_ref().unwrap().0.contains("carries the address"),
            "{errors:#?}"
        );
    }

    #[test]
    fn rejects_printing_an_array() {
        let errors = errors_in_main("int[2] xs = [1, 2];\nprint(xs);");
        assert!(errors[0].message.contains("cannot print"), "{}", errors[0].message);
    }

    #[test]
    fn arrays_answer_no_comparison_and_no_arithmetic() {
        assert!(errors_in_main("int[2] a = [1, 2];\nprint(a == a);")[0]
            .message
            .contains("cannot be compared"));
        assert!(errors_in_main("int[2] a = [1, 2];\nprint(a + 1);")[0]
            .message
            .contains("cannot apply `+`"));
    }

    #[test]
    fn rejects_indexing_something_that_is_not_an_array() {
        for body in ["int n = 1;\nprint(n[0]);", "int n = 1;\nn[0] = 1;"] {
            let errors = errors_in_main(body);
            assert!(errors[0].message.contains("cannot index"), "{body}: {}", errors[0].message);
        }
        assert!(errors_in_main("int n = 1;\nprint(len(n));")[0].message.contains("`len` needs an array"));
    }

    #[test]
    fn an_index_must_be_an_int() {
        let errors = errors_in_main("int[2] xs = [1, 2];\nprint(xs[true]);");
        assert!(errors[0].message.contains("cannot index with"), "{}", errors[0].message);
    }

    // -- bounds ------------------------------------------------------------

    #[test]
    fn an_index_the_compiler_can_see_is_checked_at_compile_time() {
        for body in [
            "int[3] xs = [1, 2, 3];\nprint(xs[3]);",
            "int[3] xs = [1, 2, 3];\nprint(xs[-1]);",
            "int[3] xs = [1, 2, 3];\nxs[9] = 1;",
            // Reaches through arithmetic, as the overflow check does.
            "int[3] xs = [1, 2, 3];\nprint(xs[1 + 2]);",
        ] {
            let errors = errors_in_main(body);
            assert!(errors[0].message.contains("out of bounds"), "{body}: {}", errors[0].message);
        }
    }

    #[test]
    fn every_index_the_array_really_has_is_accepted() {
        assert!(check_main("int[3] xs = [1, 2, 3];\nprint(xs[0]);\nprint(xs[2]);").is_ok());
    }

    #[test]
    fn an_index_that_depends_on_a_variable_is_left_to_the_runtime() {
        // `sema` never looks a variable up, so this compiles — and the emitted
        // code carries the check that catches it.
        assert!(check_main("int[3] xs = [1, 2, 3];\nint i = 9;\nprint(xs[i]);").is_ok());
    }

    #[test]
    fn rejects_a_length_no_array_could_have() {
        assert!(errors_in_main("int[0] xs = [1];")[0].message.contains("not a valid array length"));
        let errors = errors_in_main("int[99999] xs = [1];");
        assert!(errors[0].message.contains("not a valid array length"), "{}", errors[0].message);
    }

    // -- enums -------------------------------------------------------------

    /// A `Colour` enum, a `main`, and whatever body is given, so the tests
    /// about enums stay about enums.
    fn check_colour(body: &str) -> Result<Types> {
        check_src(&format!("enum Colour {{ Red, Green, Blue }}\nfn main() {{\n{body}\n}}\n"))
    }

    fn colour_errors(body: &str) -> Vec<Diagnostic> {
        check_colour(body).unwrap_err()
    }

    #[test]
    fn accepts_an_enum_declared_used_and_matched() {
        assert!(
            check_colour(
                "Colour c = Colour::Red;\nmatch (c) {\n  Colour::Red => { print(1); }\n  \
                 Colour::Green => { print(2); }\n  Colour::Blue => { print(3); }\n}"
            )
            .is_ok()
        );
    }

    #[test]
    fn an_enum_is_a_type_of_its_own() {
        // Not an int with a different name: nothing converts either way.
        assert!(colour_errors("int n = Colour::Red;")[0].message.contains("cannot initialize"));
        assert!(colour_errors("Colour c = 0;")[0].message.contains("cannot initialize"));
        assert!(colour_errors("Colour c = Colour::Red;\nc = 1;")[0].message.contains("cannot assign"));
    }

    #[test]
    fn arithmetic_and_conditions_reject_an_enum() {
        assert!(colour_errors("print(Colour::Red + 1);")[0].message.contains("cannot apply `+`"));
        assert!(colour_errors("print(-Colour::Red);")[0].message.contains("cannot apply `-`"));
        assert!(colour_errors("print(!Colour::Red);")[0].message.contains("cannot apply `!`"));
        assert!(colour_errors("if (Colour::Red) {\n}")[0].message.contains("must be a `bool`"));
    }

    #[test]
    fn enums_answer_equality_but_not_order() {
        assert!(check_colour("print(Colour::Red == Colour::Blue);").is_ok());
        assert!(check_colour("print(Colour::Red != Colour::Blue);").is_ok());
        // The declaration puts the variants in a sequence, but the program
        // never said that sequence meant anything.
        let errors = colour_errors("print(Colour::Red < Colour::Blue);");
        assert!(errors[0].message.contains("cannot be compared"), "{}", errors[0].message);
        assert!(errors[0].label.as_deref().unwrap().contains("Colour"), "{errors:#?}");
    }

    #[test]
    fn two_enums_are_not_interchangeable() {
        let errors = check_src(
            "enum A { X }\nenum B { X }\nfn main() {\n  A a = A::X;\n  a = B::X;\n}",
        )
        .unwrap_err();
        assert!(errors[0].message.contains("cannot assign"), "{}", errors[0].message);
    }

    #[test]
    fn two_enums_may_share_a_variant_name() {
        // A variant is always written qualified, so there is nothing to
        // disambiguate.
        assert!(
            check_src("enum A { Red }\nenum B { Red }\nfn main() {\n  A a = A::Red;\n  print(a);\n}")
                .is_ok()
        );
    }

    #[test]
    fn rejects_an_unknown_type() {
        for src in [
            "fn main() {\n  Nope n = 1;\n}",
            "fn f(Nope n) {\n}\nfn main() {\n}",
            "fn f() -> Nope {\n}\nfn main() {\n}",
        ] {
            let errors = check_src(src).unwrap_err();
            assert!(errors[0].message.contains("unknown type `Nope`"), "{src}: {}", errors[0].message);
        }
    }

    #[test]
    fn rejects_an_unknown_enum_or_variant() {
        assert!(colour_errors("print(Nope::Red);")[0].message.contains("unknown enum `Nope`"));
        let errors = colour_errors("print(Colour::Purple);");
        assert!(errors[0].message.contains("no variant `Purple`"), "{}", errors[0].message);
        // The note lists the ones it does have, which is the useful half.
        let note = errors[0].note.as_ref().unwrap().0.clone();
        assert!(note.contains("`Red`, `Green` and `Blue`"), "{note}");
    }

    #[test]
    fn rejects_a_duplicate_enum_or_variant() {
        let errors = check_src("enum A { X }\nenum A { Y }\nfn main() {\n}").unwrap_err();
        assert!(errors[0].message.contains("already declared"), "{}", errors[0].message);
        assert!(errors[0].note.as_ref().unwrap().1.is_some());

        let errors = check_src("enum A { X, X }\nfn main() {\n}").unwrap_err();
        assert!(errors[0].message.contains("declared twice"), "{}", errors[0].message);
    }

    #[test]
    fn rejects_an_enum_with_no_variants() {
        // No value could ever have the type, so nothing could be done with it.
        let errors = check_src("enum Void { }\nfn main() {\n}").unwrap_err();
        assert!(errors[0].message.contains("has no variants"), "{}", errors[0].message);
    }

    #[test]
    fn an_enum_may_be_a_parameter_and_a_return_type() {
        assert!(
            check_src(
                "enum A { X, Y }\nfn flip(A a) -> A {\n  match (a) {\n    A::X => { return A::Y; }\n    \
                 A::Y => { return A::X; }\n  }\n}\nfn main() {\n  print(flip(A::X));\n}"
            )
            .is_ok()
        );
    }

    // -- exhaustiveness ----------------------------------------------------

    #[test]
    fn rejects_a_match_that_misses_a_variant() {
        let errors = colour_errors(
            "Colour c = Colour::Red;\nmatch (c) {\n  Colour::Red => { print(1); }\n}",
        );
        assert!(errors[0].message.contains("does not cover every variant"), "{}", errors[0].message);
        // The label names exactly what is missing, in declaration order.
        let label = errors[0].label.as_deref().unwrap();
        assert!(label.contains("`Green` and `Blue` are not handled"), "{label}");
    }

    #[test]
    fn a_single_missing_variant_reads_as_one() {
        let errors = colour_errors(
            "Colour c = Colour::Red;\nmatch (c) {\n  Colour::Red => { }\n  Colour::Green => { }\n}",
        );
        assert!(
            errors[0].label.as_deref().unwrap().contains("`Blue` is not handled"),
            "{errors:#?}"
        );
    }

    #[test]
    fn an_empty_match_is_missing_everything() {
        let errors = colour_errors("Colour c = Colour::Red;\nmatch (c) {\n}");
        assert!(errors[0].message.contains("does not cover"), "{}", errors[0].message);
    }

    #[test]
    fn rejects_a_variant_covered_twice() {
        let errors = colour_errors(
            "Colour c = Colour::Red;\nmatch (c) {\n  Colour::Red => { }\n  Colour::Red => { }\n  \
             Colour::Green => { }\n  Colour::Blue => { }\n}",
        );
        assert!(errors[0].message.contains("already covered"), "{}", errors[0].message);
        assert!(errors[0].note.as_ref().unwrap().1.is_some(), "and points at the first arm");
    }

    #[test]
    fn rejects_an_arm_belonging_to_another_enum() {
        let errors = check_src(
            "enum A { X }\nenum B { Y }\nfn main() {\n  A a = A::X;\n  match (a) {\n    \
             B::Y => { }\n  }\n}",
        )
        .unwrap_err();
        assert!(errors[0].message.contains("but the value is a `A`"), "{}", errors[0].message);
    }

    #[test]
    fn rejects_a_match_on_something_that_is_not_an_enum() {
        for body in ["match (1) {\n}", "match (true) {\n}", "match (\"a\") {\n}"] {
            let errors = errors_in_main(body);
            assert!(errors[0].message.contains("cannot match on"), "{body}: {}", errors[0].message);
        }
    }

    #[test]
    fn an_arm_body_is_checked_whatever_is_wrong_with_its_pattern() {
        // A mistake inside an arm is worth reporting even when the arm itself
        // will never run.
        let errors = colour_errors(
            "Colour c = Colour::Red;\nmatch (c) {\n  Colour::Purple => { print(nope); }\n}",
        );
        assert!(errors.iter().any(|d| d.message.contains("undeclared variable `nope`")), "{errors:#?}");
    }

    // -- match as an expression --------------------------------------------

    #[test]
    fn a_match_of_values_is_an_expression_of_their_type() {
        assert!(
            check_colour(
                "string s = match (Colour::Red) {\n  Colour::Red => \"warm\",\n  \
                 Colour::Green => \"cool\",\n  Colour::Blue => \"cold\",\n};\nprint(s);"
            )
            .is_ok()
        );
        // And is checked against what wanted it.
        let errors = colour_errors(
            "int n = match (Colour::Red) {\n  Colour::Red => \"warm\",\n  \
             Colour::Green => \"cool\",\n  Colour::Blue => \"cold\",\n};",
        );
        assert!(errors[0].message.contains("cannot initialize"), "{}", errors[0].message);
    }

    #[test]
    fn every_arm_of_a_match_has_to_agree() {
        let errors = colour_errors(
            "string s = match (Colour::Red) {\n  Colour::Red => \"warm\",\n  \
             Colour::Green => 1,\n  Colour::Blue => \"cold\",\n};",
        );
        assert!(errors[0].message.contains("but an earlier one produces"), "{}", errors[0].message);
        // The note points back at the arm that set the type.
        assert!(errors[0].note.as_ref().unwrap().1.is_some(), "{errors:#?}");
    }

    #[test]
    fn a_block_arm_is_admissible_in_value_position_only_if_it_diverges() {
        // `return` keeps its one meaning, so a block hands nothing back — it
        // has to be one control never falls out of.
        assert!(
            check_colour(
                "string s = match (Colour::Red) {\n  Colour::Red => \"warm\",\n  \
                 Colour::Green => { print(\"x\"); return; }\n  Colour::Blue => \"cold\",\n};"
            )
            .is_ok()
        );
        let errors = colour_errors(
            "string s = match (Colour::Red) {\n  Colour::Red => \"warm\",\n  \
             Colour::Green => { print(\"x\"); }\n  Colour::Blue => \"cold\",\n};",
        );
        assert!(errors[0].message.contains("produces no value"), "{}", errors[0].message);
    }

    #[test]
    fn a_break_or_a_continue_makes_a_block_arm_diverge_too() {
        // The question is whether control can reach the end of the block, and
        // a loop jump answers it as well as a `return` does.
        assert!(
            check_colour(
                "while (true) {\n  int n = match (Colour::Red) {\n    Colour::Red => 1,\n    \
                 Colour::Green => { break; }\n    Colour::Blue => { continue; }\n  };\n  print(n);\n}"
            )
            .is_ok()
        );
    }

    #[test]
    fn a_match_where_every_arm_leaves_produces_nothing() {
        let errors = colour_errors(
            "int n = match (Colour::Red) {\n  Colour::Red => { return; }\n  \
             Colour::Green => { return; }\n  Colour::Blue => { return; }\n};",
        );
        assert!(errors[0].message.contains("produces no value"), "{}", errors[0].message);
    }

    #[test]
    fn a_value_arm_has_nowhere_to_go_in_statement_position() {
        // TinyC discards no values; a match written as a statement runs its
        // arms for effect.
        let errors = colour_errors(
            "match (Colour::Red) {\n  Colour::Red => 1,\n  Colour::Green => 2,\n  \
             Colour::Blue => 3,\n}",
        );
        assert!(errors[0].message.contains("nothing uses it"), "{}", errors[0].message);
    }

    #[test]
    fn a_match_of_blocks_is_still_a_statement() {
        assert!(
            check_colour(
                "match (Colour::Red) {\n  Colour::Red => { print(1); }\n  \
                 Colour::Green => { print(2); }\n  Colour::Blue => { print(3); }\n}"
            )
            .is_ok()
        );
    }

    #[test]
    fn a_match_expression_is_still_checked_for_exhaustiveness() {
        // The two checks are independent: neither form escapes either.
        let errors = colour_errors("string s = match (Colour::Red) {\n  Colour::Red => \"a\",\n};");
        assert!(errors[0].message.contains("does not cover"), "{}", errors[0].message);
    }

    #[test]
    fn a_match_may_be_the_scrutinee_of_another() {
        assert!(
            check_src(
                "enum A { X, Y }\nfn main() {\n  A a = match (A::X) {\n    A::X => A::Y,\n    \
                 A::Y => A::X,\n  };\n  match (a) {\n    A::X => { print(1); }\n    \
                 A::Y => { print(2); }\n  }\n}"
            )
            .is_ok()
        );
    }

    #[test]
    fn an_exhaustive_match_counts_as_returning() {
        // The payoff of the check: no trailing `return` is needed, and none
        // would be reachable.
        assert!(
            check_src(
                "enum A { X, Y }\nfn f(A a) -> int {\n  match (a) {\n    A::X => { return 1; }\n    \
                 A::Y => { return 2; }\n  }\n}\nfn main() {\n  print(f(A::X));\n}"
            )
            .is_ok()
        );
    }

    #[test]
    fn a_match_with_one_arm_not_returning_does_not_count() {
        let errors = check_src(
            "enum A { X, Y }\nfn f(A a) -> int {\n  match (a) {\n    A::X => { return 1; }\n    \
             A::Y => { print(2); }\n  }\n}\nfn main() {\n}",
        )
        .unwrap_err();
        assert!(errors[0].message.contains("may finish without returning"), "{}", errors[0].message);
    }

    #[test]
    fn break_and_continue_work_inside_an_arm() {
        assert!(
            check_src(
                "enum A { X, Y }\nfn main() {\n  while (true) {\n    A a = A::X;\n    \
                 match (a) {\n      A::X => { break; }\n      A::Y => { continue; }\n    }\n  }\n}"
            )
            .is_ok()
        );
        // And are still rejected when there is no loop around them.
        let errors = check_src(
            "enum A { X }\nfn main() {\n  A a = A::X;\n  match (a) {\n    A::X => { break; }\n  }\n}",
        )
        .unwrap_err();
        assert!(errors[0].message.contains("outside of a loop"), "{}", errors[0].message);
    }

    #[test]
    fn an_arm_is_a_scope_of_its_own() {
        let errors = check_src(
            "enum A { X, Y }\nfn main() {\n  A a = A::X;\n  match (a) {\n    \
             A::X => { int n = 1; print(n); }\n    A::Y => { print(n); }\n  }\n}",
        )
        .unwrap_err();
        assert!(errors[0].message.contains("undeclared variable `n`"), "{}", errors[0].message);
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
