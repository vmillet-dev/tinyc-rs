//! Pass 1: the classes and array types a signature may mention, and their layout.

use std::collections::HashMap;

use crate::ast::{
    ClassId, ClassInfo, FieldInfo, Program,
    MethodInfo, Prim, Shape, Ty, TypeRef, TypeTable,
};
use crate::diag::{Diagnostic, Span};

use super::{Declared, MAX_ARRAY_LEN, Signature, already_declared};

/// Bytes an object spends on its vtable pointer, which sits at offset 0.
pub(super) const VPTR_SIZE: u32 = 8;

/// How much room one object may take.
///
/// Not a limit of the representation but of the *stack*: an object lives in the
/// frame, and containment multiplies — a class holding a thousand of something
/// that itself holds a thousand `int`s is eight megabytes, which no thread has.
/// The limit is what turns that into a diagnostic rather than a crash in a
/// program that compiled. A list is what holds a quantity the frame cannot.
pub const MAX_OBJECT_BYTES: u32 = 64 * 1024;


/// A field whose type is settled but whose place in the object is not yet.
///
/// Resolving every field before measuring anything is what makes the layout
/// *orderable*: a field holding an object reserves that object's room, so what
/// a class contains has to be known before it can be given a size.
#[derive(Clone)]
pub(super) struct ResolvedField {
    name: String,
    name_span: Span,
    ty: Ty,
    /// The type as written, which is what a diagnostic about it underlines.
    ty_span: Span,
}

/// One class holding another, as the graph [`containment_order`] walks sees it.
///
/// The field travels with the edge because it is what a diagnostic points at: a
/// containment that cannot be laid out is reported where it was *written*,
/// rather than on the class that suffers from it.
#[derive(Clone, Copy)]
pub(super) struct Contains {
    /// The hierarchy reached, which is what has to be measured first.
    to: ClassId,
    /// The class whose field this is, and which of its fields.
    owner: ClassId,
    field: usize,
}

/// Where a hierarchy is in the walk: unvisited, on the path being followed, or
/// finished. Reaching one that is still on the path *is* the cycle.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Mark {
    Unseen,
    OnThePath,
    Measured,
}

/// Pass 0b: resolve base classes, lay every object out, and settle the vtables.
///
/// Names are registered first so that a field may mention any class and a base
/// may be declared after its subclass. What has to happen in order is *layout*,
/// and two rules decide that order — see [`containment_order`].
pub(super) fn collect_classes(program: &Program, declared: &mut Declared, errors: &mut Vec<Diagnostic>) {
    // Every name first, with a placeholder layout, so the rest can refer to it.
    for class in &program.classes {
        if let Some(previous) = declared.type_named(&class.name) {
            errors.push(already_declared(&class.name, class.name_span, previous));
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

    // What every class holds, before any of it is measured, and then the order
    // the measuring has to happen in.
    let fields = resolve_fields(program, declared, errors);
    let order = containment_order(declared, &fields, errors);
    lay_out(declared, &fields, &order, errors);
}

/// Turn every field's written type into the type it names.
///
/// Every type is allowed here, a list included — and a list is the one that
/// costs something. An array or an object lives *inside* the object that holds
/// it, so copying the outer one copies it whole; a list's elements live in the
/// arena and the field holds their address, so a byte copy would leave two
/// objects naming one list. What pays for it is a second step after the bytes:
/// see [`TypeTable::holds_a_list`] and the fix-up routine the backend emits
/// from it.
///
/// Allowing it is also what makes a class **recursive**. `Node next` is still
/// refused — a field lives inside the object, so the object would have to be
/// bigger than itself — but `Node[] kids` does not, because the elements are
/// not in the object at all. That is the whole of how TinyC gets trees without
/// a reference type and without `null`.
pub(super) fn resolve_fields(
    program: &Program,
    declared: &mut Declared,
    errors: &mut Vec<Diagnostic>,
) -> Vec<Vec<ResolvedField>> {
    let mut resolved = vec![Vec::new(); declared.table.classes.len()];
    for (id, class) in order_of_declarations(program, declared) {
        for field in &class.fields {
            let Some(ty) = resolve_type(declared, &field.ty, errors) else { continue };
            resolved[id.0 as usize].push(ResolvedField {
                name: field.name.clone(),
                name_span: field.name_span,
                ty,
                ty_span: field.ty.span,
            });
        }
    }
    resolved
}

/// The class a field of this type puts *inside* the object, if any.
///
/// An array holds its elements where it is, so an array of objects contains its
/// element class as surely as a bare field of it does. Everything else is one
/// register wide and contains nothing.
pub(super) fn contained_class(table: &TypeTable, ty: Ty) -> Option<ClassId> {
    match ty {
        Ty::Class(id) => Some(id),
        Ty::Array(id) => contained_class(table, table.array(id).elem),
        _ => None,
    }
}

/// The order the classes have to be measured in, hierarchy by hierarchy.
///
/// Two rules meet here, and both are about what has to have a size first. A
/// class's fields follow its base's, so a base comes first — the prefix rule
/// that makes an upcast free. And a field holding an object reserves that
/// object's room, so whatever it names has to have been measured already.
///
/// The second rule is about a whole *hierarchy* rather than one class, because
/// every class in one reserves the same [`ClassInfo::storage`]: the biggest of
/// them. So the graph walked here has one node per hierarchy, and a cycle in it
/// is a class that would have to be bigger than itself. TinyC has no reference
/// type to break one with, so a cycle is refused rather than represented — what
/// comes back is the hierarchy *roots*, each after everything it contains.
pub(super) fn containment_order(
    declared: &Declared,
    fields: &[Vec<ResolvedField>],
    errors: &mut Vec<Diagnostic>,
) -> Vec<ClassId> {
    let ids: Vec<ClassId> = (0..declared.table.classes.len() as u32).map(ClassId).collect();

    let mut edges: HashMap<ClassId, Vec<Contains>> = HashMap::new();
    for &id in &ids {
        let from = declared.table.root_of(id);
        for (at, field) in fields[id.0 as usize].iter().enumerate() {
            if let Some(held) = contained_class(&declared.table, field.ty) {
                let to = declared.table.root_of(held);
                edges.entry(from).or_default().push(Contains { to, owner: id, field: at });
            }
        }
    }

    let mut walk = Walk {
        edges,
        marks: vec![Mark::Unseen; ids.len()],
        path: Vec::new(),
        order: Vec::new(),
        declared,
        fields,
    };
    for &id in &ids {
        if declared.table.root_of(id) == id && walk.marks[id.0 as usize] == Mark::Unseen {
            walk.measure_after(id, errors);
        }
    }
    walk.order
}

/// One walk over the containment graph, held together so that following an edge
/// is one call rather than eight arguments.
pub(super) struct Walk<'a> {
    edges: HashMap<ClassId, Vec<Contains>>,
    marks: Vec<Mark>,
    /// The chain of containments being followed, which is what names a ring.
    path: Vec<Contains>,
    /// Roots in the order they were finished: each after everything it holds.
    order: Vec<ClassId>,
    declared: &'a Declared,
    fields: &'a [Vec<ResolvedField>],
}

impl Walk<'_> {
    /// Visit `root` after everything it contains, and add it once they are in.
    fn measure_after(&mut self, root: ClassId, errors: &mut Vec<Diagnostic>) {
        self.marks[root.0 as usize] = Mark::OnThePath;
        for at in 0..self.edges.get(&root).map_or(0, Vec::len) {
            let edge = self.edges[&root][at];
            match self.marks[edge.to.0 as usize] {
                Mark::Measured => {}
                // The edge closes a ring. Dropping it is what keeps the rest of
                // the walk finite; the program is refused either way.
                Mark::OnThePath => {
                    errors.push(contains_itself(self.declared, self.fields, &self.path, edge))
                }
                Mark::Unseen => {
                    self.path.push(edge);
                    self.measure_after(edge.to, errors);
                    self.path.pop();
                }
            }
        }
        self.marks[root.0 as usize] = Mark::Measured;
        self.order.push(root);
    }
}

/// The diagnostic for a class that would have to contain itself.
///
/// Reported on the field that closes the ring, which is the one that could be
/// deleted to break it. When the ring passes through other classes the note
/// names them all, because the field on its own does not look wrong.
pub(super) fn contains_itself(
    declared: &Declared,
    fields: &[Vec<ResolvedField>],
    path: &[Contains],
    closing: Contains,
) -> Diagnostic {
    let hop = |edge: &Contains| {
        let field = &fields[edge.owner.0 as usize][edge.field];
        format!(
            "`{}` holds {} in `{}`",
            declared.class(edge.owner).name,
            field.ty.with_article(&declared.table),
            field.name
        )
    };

    let field = &fields[closing.owner.0 as usize][closing.field];
    let diagnostic = Diagnostic::new(
        format!(
            "`{}` cannot contain {}",
            declared.class(closing.owner).name,
            field.ty.with_article(&declared.table)
        ),
        field.ty_span,
    )
    .with_label("a field lives inside the object, so its room is part of this one's");

    // The ring is the stretch of the path that leads back to what this edge
    // reached, and then this edge itself.
    let from = path.iter().position(|edge| edge.to == closing.to).unwrap_or(0);
    let ring: Vec<String> = path[from..].iter().chain(std::iter::once(&closing)).map(hop).collect();
    match ring.len() {
        1 => diagnostic.with_note(
            "TinyC has no reference type, so what an object holds it holds outright",
            None,
        ),
        _ => diagnostic.with_note(format!("{}, and round again", ring.join(", ")), None),
    }
}

/// Give every field its place, hierarchy by hierarchy, in the order that makes
/// every size knowable by the time it is needed.
pub(super) fn lay_out(
    declared: &mut Declared,
    fields: &[Vec<ResolvedField>],
    order: &[ClassId],
    errors: &mut Vec<Diagnostic>,
) {
    let depth_first = layout_order(declared);
    for &root in order {
        let hierarchy: Vec<ClassId> =
            depth_first.iter().copied().filter(|&id| declared.table.root_of(id) == root).collect();
        for &id in &hierarchy {
            lay_out_class(declared, &fields[id.0 as usize], id, errors);
        }

        // Storage is the biggest size in the *hierarchy*, and the same for
        // every class in it. That is what lets a value of a base class hold any
        // of its subclasses without being sliced — and, just as importantly,
        // what keeps copying `storage` bytes out of the smallest of them in
        // bounds. It is settled here, before anything containing one of these
        // classes is measured.
        let storage =
            hierarchy.iter().map(|&id| declared.class(id).size).max().unwrap_or(VPTR_SIZE);
        for &id in &hierarchy {
            declared.table.classes[id.0 as usize].storage = storage;
        }
    }
}

/// Lay one class out: its base's fields, then its own, each where the one
/// before it stops.
pub(super) fn lay_out_class(
    declared: &mut Declared,
    fields: &[ResolvedField],
    id: ClassId,
    errors: &mut Vec<Diagnostic>,
) {
    let base = declared.class(id).base;
    let mut laid = match base {
        Some(base) => declared.class(base).fields.clone(),
        None => Vec::new(),
    };
    // A subclass's own fields start where its base's stop, which is what makes
    // the base a *prefix* of it: one address serves as both.
    let mut offset = base.map_or(VPTR_SIZE, |base| declared.class(base).size);

    let name = declared.class(id).name.clone();
    for field in fields {
        if laid.iter().any(|f| f.name == field.name) {
            errors.push(
                Diagnostic::new(
                    format!("`{name}` already has a field `{}`", field.name),
                    field.name_span,
                )
                .with_label("a field may only be named once, base classes included"),
            );
            continue;
        }
        laid.push(FieldInfo { name: field.name.clone(), ty: field.ty, offset });
        // Saturating, because this is arithmetic on a program that may already
        // be nonsense: a class of a thousand of a thousand of a thousand does
        // not fit in the number, and the answer to that is the diagnostic
        // below rather than a compiler that panics.
        offset = offset.saturating_add(declared.table.size_of(field.ty));
    }

    // An object lives in the frame, so what one costs is the stack's business.
    if offset > MAX_OBJECT_BYTES {
        let size = match offset == u32::MAX {
            true => "more than four gigabytes".to_string(),
            false => format!("{offset} bytes"),
        };
        errors.push(
            Diagnostic::new(format!("`{name}` is too big"), declared.class_spans[id.0 as usize])
                .with_label(format!("{size}, and at most {MAX_OBJECT_BYTES} are supported"))
                .with_note(
                    "an object lives in the frame, and containment multiplies; `int[]` is what \
                     holds a quantity the frame cannot",
                    None,
                ),
        );
        // Recorded at the limit rather than at what it asked for, so that a
        // class holding *this* one multiplies a number that still means
        // something. Nothing is emitted for a program with an error in it; what
        // this protects is the rest of the checking.
        offset = MAX_OBJECT_BYTES;
    }

    let info = &mut declared.table.classes[id.0 as usize];
    info.fields = laid;
    info.size = offset;
}

/// Pass 1b: settle every class's vtable, now that signatures exist.
///
/// A method inherited from the base keeps its slot and its implementation; one
/// that overrides keeps the slot and replaces the implementation; a new one
/// takes the next slot. That is the whole of single inheritance — and the
/// reason a subclass's table can be read as its base's.
pub(super) fn collect_methods(
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
pub(super) fn inherits_through(declared: &Declared, from: ClassId, target: ClassId) -> bool {
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
pub(super) fn order_of_declarations<'a>(
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

pub(super) fn declarations_index(program: &Program, declared: &Declared, id: ClassId) -> usize {
    program
        .classes
        .iter()
        .position(|class| declared.class_id(&class.name) == Some(id))
        .expect("every id came from a declaration")
}

/// Class ids with every base before anything that extends it.
pub(super) fn layout_order(declared: &Declared) -> Vec<ClassId> {
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
pub(super) fn resolve_type(declared: &mut Declared, ty: &TypeRef, errors: &mut Vec<Diagnostic>) -> Option<Ty> {
    // A name the *language* spells first, then one the program declared. The
    // list is `Prim`'s rather than written out here, and so is the note: this
    // one used to say the built-in types were `int`, `string`, `char` and
    // `bool` long after there were five of them.
    let element = match Prim::of_name(&ty.name) {
        Some(prim) => Some(prim.ty()),
        None => declared
            .enum_id(&ty.name)
            .map(Ty::Enum)
            .or_else(|| declared.class_id(&ty.name).map(Ty::Class))
            .or_else(|| {
                errors.push(
                    Diagnostic::new(format!("unknown type `{}`", ty.name), ty.span)
                        .with_label("no built-in type, enum or class goes by this name")
                        .with_note(
                            format!("the built-in types are {}", Prim::all_quoted()),
                            None,
                        ),
                );
                None
            }),
    }?;

    let (len, len_span) = match ty.shape {
        Shape::One => return Some(element),
        Shape::Array(len, len_span) => (len, len_span),
        // A list holds its elements *where it is*, exactly as an array does, so
        // an object may be one: growing copies whole elements rather than
        // handles, and there is nothing for two names to share. A list of lists
        // or of arrays is not refused here because it cannot be written at all
        // — a type carries at most one pair of brackets.
        Shape::List => return Some(declared.list_of(element)),
    };

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
                })
                .with_note("`int[]` is a list, whose length the program decides", None),
        );
        return None;
    }
    Some(declared.array_of(element, len as u32))
}
