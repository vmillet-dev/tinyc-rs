//! The abstract syntax tree produced by the parser.

use crate::diag::Span;
use crate::token::TokenKind;

/// Index of an enum in [`Program::enums`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EnumId(pub u32);

/// Index of an array type in [`TypeTable::arrays`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArrayId(pub u32);

/// Index of a list type in [`TypeTable::lists`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ListId(pub u32);

/// One variant of a declared enum: what it is called, and what it carries.
#[derive(Clone, Debug)]
pub struct VariantInfo {
    pub name: String,
    /// The types written after the name, in order. Empty for a variant that
    /// carries nothing, which is every variant TinyC had until enums gained
    /// payloads.
    ///
    /// Each has to fit in a register — see `sema`. An object or an array would
    /// have to live *inside* the value, which would make an enum's size the
    /// biggest of its variants and drag the whole layout ordering in with it.
    pub payload: Vec<Ty>,
}

/// What every stage after the parser needs to know about a declared enum: what
/// it is called, and what its variants are called *in order*.
///
/// The order is the whole representation — a variant's tag is its position — so
/// this is also the table the runtime prints a value's name from.
#[derive(Clone, Debug)]
pub struct EnumInfo {
    pub name: String,
    pub variants: Vec<VariantInfo>,
}

impl EnumInfo {
    /// Whether any variant carries something, and so whether a value of this
    /// enum is a **pointer** rather than a bare tag.
    ///
    /// One representation for the whole enum rather than one per variant: a
    /// `Shape` has to be one thing wherever a `Shape` is expected, and which
    /// variant is in it is exactly what the program does not know until it
    /// matches.
    pub fn carries_data(&self) -> bool {
        self.variants.iter().any(|v| !v.payload.is_empty())
    }

    /// How many payload slots a value of this enum reserves: the most any one
    /// variant carries. Zero for an enum that is still a bare tag.
    pub fn slots(&self) -> usize {
        self.variants.iter().map(|v| v.payload.len()).max().unwrap_or(0)
    }

    pub fn names(&self) -> Vec<&str> {
        self.variants.iter().map(|v| v.name.as_str()).collect()
    }
}

/// An array type: what it holds, and how many.
///
/// The length is part of the type, so `int[3]` and `int[4]` are different types
/// and **every length is known at compile time**. That is what lets a constant
/// index be checked outright rather than guarded, and what lets a function
/// receiving an array know how long it is without being told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArrayInfo {
    pub elem: Ty,
    pub len: u32,
}

/// Index of a class in [`TypeTable::classes`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClassId(pub u32);

/// One field of a class, at a settled place in the object.
#[derive(Clone, Debug)]
pub struct FieldInfo {
    pub name: String,
    pub ty: Ty,
    /// Where it sits, in bytes from the start of the object.
    pub offset: u32,
}

/// One method of a class, at a settled place in the vtable.
#[derive(Clone, Debug)]
pub struct MethodInfo {
    pub name: String,
    /// The function that implements it *for this class*, as an index into
    /// [`crate::ast::Program::functions`]. An inherited method names the base's
    /// function; an override names the derived one, in the same slot.
    pub function: usize,
    pub params: Vec<Ty>,
    pub ret: Option<Ty>,
    /// Which entry of the vtable it occupies, fixed by the class that first
    /// declared it so that every subclass agrees.
    pub slot: usize,
}

/// What every stage after the parser needs to know about a class.
///
/// The layout is settled here and nowhere else: a vtable pointer at offset 0,
/// then the base's fields, then this class's own. That prefix rule is what
/// makes an upcast free — a `Circle` *is* a `Shape` at the same address.
#[derive(Clone, Debug)]
pub struct ClassInfo {
    pub name: String,
    pub base: Option<ClassId>,
    /// Every field, inherited ones first, in layout order.
    pub fields: Vec<FieldInfo>,
    /// Every method, in vtable order, with overrides already resolved.
    pub methods: Vec<MethodInfo>,
    /// Bytes this class's own values occupy: the vtable pointer plus its fields.
    pub size: u32,
    /// Bytes any *storage* for this class must reserve: the largest size in
    /// the whole hierarchy, not merely among this class's own descendants.
    ///
    /// Whole-program compilation is what makes this knowable — nothing can
    /// derive from a class after the fact — and it is what lets a polymorphic
    /// value be copied without being sliced: a `Circle` written into a `Shape`
    /// keeps its vtable pointer and all its fields.
    ///
    /// Every class in a hierarchy gets the *same* number, deliberately. A
    /// smaller one would mean copying `storage(Shape)` bytes out of a
    /// `Circle`-sized object could read past the end of it.
    pub storage: u32,
}

impl ClassInfo {
    pub fn field(&self, name: &str) -> Option<&FieldInfo> {
        self.fields.iter().find(|f| f.name == name)
    }

    pub fn method(&self, name: &str) -> Option<&MethodInfo> {
        self.methods.iter().find(|m| m.name == name)
    }
}

/// Every type a program declared or built, indexed by the ids a [`Ty`] holds.
///
/// `Ty` is deliberately small enough to be `Copy` and to compare as an integer,
/// which means it cannot carry a name or an element type. This is where those
/// live, and why naming a type takes a second argument.
#[derive(Clone, Debug, Default)]
pub struct TypeTable {
    pub enums: Vec<EnumInfo>,
    pub arrays: Vec<ArrayInfo>,
    /// What each list type holds. A list has no length in its type — that is
    /// the whole difference from an array — so this is one element type and
    /// nothing else.
    pub lists: Vec<Ty>,
    pub classes: Vec<ClassInfo>,
}

impl TypeTable {
    pub fn array(&self, id: ArrayId) -> ArrayInfo {
        self.arrays[id.0 as usize]
    }

    /// What a list holds.
    pub fn element(&self, id: ListId) -> Ty {
        self.lists[id.0 as usize]
    }

    pub fn enum_info(&self, id: EnumId) -> &EnumInfo {
        &self.enums[id.0 as usize]
    }

    pub fn class(&self, id: ClassId) -> &ClassInfo {
        &self.classes[id.0 as usize]
    }

    /// Whether `sub` is `base` or descends from it, which is what makes an
    /// argument acceptable where a base class is expected.
    pub fn descends_from(&self, sub: ClassId, base: ClassId) -> bool {
        let mut at = Some(sub);
        while let Some(id) = at {
            if id == base {
                return true;
            }
            at = self.class(id).base;
        }
        false
    }

    /// How many bytes a value of this type occupies where it is *stored*.
    ///
    /// Everything that fits in a register is eight. An object takes its
    /// hierarchy's room, and an array its length times whatever it holds — so
    /// a field's offset is the sum of the sizes in front of it rather than its
    /// position, ever since an object could hold either of those.
    ///
    /// Saturating, because this is asked of programs that have already been
    /// refused: `sema` stops a class too big for the frame, and the answer to
    /// one written a thousand times bigger still has to be that diagnostic
    /// rather than a compiler that panics.
    pub fn size_of(&self, ty: Ty) -> u32 {
        match ty {
            Ty::Class(id) => self.class(id).storage,
            Ty::Array(id) => {
                let info = self.array(id);
                info.len.saturating_mul(self.size_of(info.elem))
            }
            _ => 8,
        }
    }

    /// Whether copying the *bytes* of a value of this type would leave two
    /// values naming one run of arena elements.
    ///
    /// An array and an object are copied byte for byte, and for everything they
    /// used to be able to hold that was the whole of the copy: what a field
    /// holds, it held *inside* itself. A list is the exception — a field holds
    /// its address — so an object with one in it needs a second step after the
    /// bytes, and this is the question of whether it does.
    ///
    /// Asked of the whole **hierarchy**, not of one class, because the value in
    /// a `Reading`-shaped hole may be a `Frost`: what has to be fixed up is
    /// decided by the object at run time, and this only decides whether to ask.
    ///
    /// Terminates on a recursive class because the recursion goes *through* a
    /// list, and a list answers `true` without looking at what it holds:
    /// `class Node { Node[] kids; }` is settled at `kids`. Containment by value
    /// cannot cycle at all — `containment_order` refuses it.
    pub fn holds_a_list(&self, ty: Ty) -> bool {
        match ty {
            Ty::List(_) => true,
            Ty::Array(id) => self.holds_a_list(self.array(id).elem),
            Ty::Class(id) => self
                .hierarchy_of(id)
                .into_iter()
                .any(|at| self.class(at).fields.iter().any(|f| self.holds_a_list(f.ty))),
            _ => false,
        }
    }

    /// Whether one class's own fields hold a list, which is what decides
    /// whether *that* class needs a routine of its own — as against
    /// [`Self::holds_a_list`], which asks about everything a hole of this type
    /// could turn out to contain.
    pub fn class_holds_a_list(&self, id: ClassId) -> bool {
        self.class(id).fields.iter().any(|f| self.holds_a_list(f.ty))
    }

    /// `id` and every class that may stand in for it: itself and its
    /// descendants. Its *ancestors* are not among them — a base cannot be put
    /// where a subclass is wanted — and their fields are in `id`'s list anyway.
    pub fn hierarchy_of(&self, id: ClassId) -> Vec<ClassId> {
        (0..self.classes.len() as u32)
            .map(ClassId)
            .filter(|&at| self.descends_from(at, id))
            .collect()
    }

    /// The class at the top of `id`'s hierarchy, which is what decides how much
    /// room every class in it reserves.
    pub fn root_of(&self, id: ClassId) -> ClassId {
        let mut at = id;
        // Cannot loop: `sema` rejects a class that is its own ancestor.
        while let Some(base) = self.class(at).base {
            at = base;
        }
        at
    }

    /// Whether nothing in the program derives from this class.
    ///
    /// A sealed class has exactly one implementation of each of its methods, so
    /// a call on one has nothing to decide at run time. Only whole-program
    /// compilation makes this answerable — a separately compiled language must
    /// assume every class may yet be extended.
    pub fn is_sealed(&self, id: ClassId) -> bool {
        !self.classes.iter().any(|c| c.base == Some(id))
    }

    /// Whether a value of `from` may be used where `to` is expected.
    ///
    /// The only widening in the language, and it is free: a subclass lays its
    /// base's fields out first, so the same address serves as both.
    pub fn coerces(&self, from: Ty, to: Ty) -> bool {
        match (from, to) {
            (Ty::Class(sub), Ty::Class(base)) => self.descends_from(sub, base),
            _ => from == to,
        }
    }
}

impl EnumInfo {
    /// The tag a variant is represented by, which is where it was written.
    pub fn tag(&self, variant: &str) -> Option<i64> {
        self.variants.iter().position(|v| v.name == variant).map(|at| at as i64)
    }

    pub fn variant(&self, name: &str) -> Option<&VariantInfo> {
        self.variants.iter().find(|v| v.name == name)
    }
}

/// The types of TinyC.
///
/// Every variant is a type a *value* can have, which is why there is no `Void`:
/// a function that returns nothing has no return type at all. See [`FnDecl::ret`].
///
/// [`Ty::Enum`] holds an *index* rather than a name, which is what keeps `Ty`
/// `Copy` and its equality an integer comparison. The price is that a `Ty`
/// cannot name itself — see [`Ty::name`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Ty {
    /// 64-bit signed integer.
    Int,
    /// A run of characters, held as the address of the first one.
    ///
    /// The characters are four bytes each and the count sits in the eight bytes
    /// *before* them, so a string knows its own length and `len` is a load. A
    /// literal is laid out the same way in `.data` as a built one is in the
    /// arena, which is why nothing anywhere has to ask which kind it holds.
    Str,
    /// One Unicode scalar value: what a string is made of, and what indexing
    /// one produces.
    ///
    /// A separate type rather than a small `int`, so that `+` cannot be applied
    /// to it by accident and `print` knows to write a character rather than a
    /// number. Going between the two is spelled out — see [`Prim`].
    Char,
    /// An IEEE-754 double.
    ///
    /// It is a machine word like everything else here, and that is the whole of
    /// how a float is carried: what the word *holds* is the double's bits, and
    /// only the instructions that do arithmetic on it or write it out ever have
    /// to know that. See [`crate::ir::Num`], which is where that is said once.
    ///
    /// No arithmetic mixes it with an `int` — `float(n)` and `int(f)` are
    /// written out, for the same reason `int(c)` is.
    Float,
    /// `true` or `false`.
    Bool,
    /// One of the variants of a declared enum, held as its index.
    Enum(EnumId),
    /// A fixed-length run of values, held as an index into the type table.
    ///
    /// The one type that does not fit in a register: a value of one lives in
    /// the frame, and what travels in a register is its address.
    Array(ArrayId),
    /// A run of values whose length is only known while the program runs.
    ///
    /// Where an [`Ty::Array`] lives in the frame and carries its length in its
    /// *type*, a list lives in the arena and carries its length in front of its
    /// elements — exactly as a [`Ty::Str`] carries its own. So a list is one
    /// pointer, it fits in a register, and `len` is a load rather than a
    /// literal.
    ///
    /// It is the one **mutable** thing that is shared by address, which is why
    /// assigning one copies its elements: without that, two names for one list
    /// would be observable the moment either was written to.
    List(ListId),
    /// An instance of a declared class. Like an array, it lives in the frame
    /// and travels as an address.
    Class(ClassId),
}

impl Ty {
    /// This type's name, given the enum names of the program it belongs to,
    /// indexed by [`EnumId`].
    ///
    /// The table has to be handed in because an enum's name comes from the
    /// source rather than from this enum, and [`Ty`] is deliberately too small
    /// to carry one.
    pub fn name(self, types: &TypeTable) -> String {
        match self {
            // Listed rather than left to a wildcard, so a type added to this
            // enum and forgotten in [`Prim`] fails to compile here instead of
            // panicking the first time something tries to name one.
            Ty::Int | Ty::Float | Ty::Str | Ty::Char | Ty::Bool => {
                Prim::of_ty(self).expect("these five are exactly `Prim`").name().to_string()
            }
            Ty::Enum(id) => types.enum_info(id).name.clone(),
            Ty::Class(id) => types.class(id).name.clone(),
            Ty::Array(id) => {
                let info = types.array(id);
                format!("{}[{}]", info.elem.name(types), info.len)
            }
            // The missing length *is* the name: `int[]` says "as many as the
            // program turns out to need".
            Ty::List(id) => format!("{}[]", types.element(id).name(types)),
        }
    }

    /// The quoted type name with its indefinite article, for prose in messages.
    pub fn with_article(self, types: &TypeTable) -> String {
        let name = self.name(types);
        // `int` and `int[3]` both want "an"; an enum's name is the program's, so
        // the article follows the spelling rather than the type.
        let article = match name.starts_with(['a', 'e', 'i', 'o', 'u', 'A', 'E', 'I', 'O', 'U']) {
            true => "an",
            false => "a",
        };
        format!("{article} `{name}`")
    }

    /// Whether values of this type can be ordered, and so compared with `<` and
    /// its relatives rather than only with `==`.
    ///
    /// An enum's variants have an order in the declaration, but it is not one
    /// the program said anything about, so TinyC declines to invent it.
    ///
    /// Characters are ordered by their Unicode scalar value, which *is* a fact
    /// the program can rely on — it is what `'0' <= c && c <= '9'` asks. Two
    /// strings are not, deliberately: that would look like alphabetical order
    /// and would not be it, since where `é` sorts is a question about a
    /// language rather than about the encoding.
    ///
    /// A float is ordered too, with the one gap IEEE-754 puts there: a NaN is
    /// neither less than, equal to, nor greater than anything, itself included.
    /// The backend emits exactly that — see `float_setcc` — so `x < y` and
    /// `!(x >= y)` are *not* the same question about floats, which is the one
    /// thing this ordering does that the others do not.
    pub fn is_ordered(self) -> bool {
        matches!(self, Ty::Int | Ty::Char | Ty::Float)
    }

    /// Whether two values of this type can be compared for equality at all.
    ///
    /// Strings can, and it is their *contents* that answer: comparing the
    /// addresses would quietly answer a different question, so this one costs a
    /// call. Arrays and objects cannot — element by element is a loop nobody
    /// asked for, and the addresses are not what anybody meant.
    ///
    /// An enum that carries a payload joins them, for exactly that reason: two
    /// `Circle`s are the same value only if their radii are, and comparing the
    /// two pointers would answer whether they were built by the same
    /// expression. `match` is what asks about one of those.
    pub fn has_equality(self, types: &TypeTable) -> bool {
        match self {
            Ty::Array(_) | Ty::List(_) | Ty::Class(_) => false,
            Ty::Enum(id) => !types.enum_info(id).carries_data(),
            _ => true,
        }
    }

    /// Whether `print` can render a value of this type.
    ///
    /// Not the same question as whether it fits in a register: a list does, and
    /// printing it would show the address of its elements rather than the
    /// elements — which is the answer to a question nobody asked.
    ///
    /// An enum with a payload *is* printable, and prints what an enum has
    /// always printed: the name of its variant. What it carries has a type of
    /// its own and a way of being written already.
    pub fn is_printable(self) -> bool {
        self.fits_in_a_register() && !matches!(self, Ty::List(_))
    }

    /// Whether a value of this type travels in a register.
    ///
    /// Arrays and objects do not: they live in the frame, and what a register
    /// holds is their address. That is why assigning one copies rather than
    /// aliases, and why returning one is done by filling room the *caller*
    /// reserved — an address that never travels outward cannot dangle.
    ///
    /// An enum does, whether or not it carries anything: one that does is a
    /// pointer to its tag and payload in the arena, exactly as a string is a
    /// pointer to its characters. It can be, because an enum is **read-only** —
    /// there is no syntax that writes into a payload — so two names for one of
    /// them cannot be told apart, which is the same bargain a string strikes.
    pub fn fits_in_a_register(self) -> bool {
        !matches!(self, Ty::Array(_) | Ty::Class(_))
    }
}

/// A type as it was written, before anything knows what it names.
///
/// The parser cannot produce a [`Ty`]: `Color c = ...` and `int c = ...` are the
/// same shape, and only [`crate::sema`] knows whether `Color` was declared. So
/// syntax carries the name and the span, and resolution happens once, in the
/// stage that has the table.
#[derive(Clone, Debug)]
pub struct TypeRef {
    pub name: String,
    /// What the brackets after the name said, if there were any.
    pub shape: Shape,
    /// The whole type as written, brackets included.
    pub span: Span,
}

/// What follows a type's name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// `int` — the type itself.
    One,
    /// `int[3]` — an array, whose length is part of its type. The span is the
    /// length's own, so a nonsensical one can be underlined by itself.
    Array(i64, Span),
    /// `int[]` — a list, whose length is not known until the program runs.
    List,
}

/// **The types the language spells with a word of its own.**
///
/// One enum, because it is one fact asked in four places: which words may start
/// a declaration, which words name a type after a `:` or a `->`, which words
/// may be written where a value is expected — `int(c)` is the code point of a
/// character, `char(n)` the character with that code point — and which names
/// [`crate::sema`] resolves without looking anything up. A word that is not
/// here is an identifier, and an identifier followed by `(` is a call.
///
/// That is what makes adding a type a row in [`Prim::ALL`] rather than a list
/// to find five copies of. What a new type still costs is every place the
/// compiler has a *decision* to make about it — how it is written out, what
/// arithmetic it does, which instruction compares two of them — and those are
/// not copies of each other.
///
/// The point of the conversion form is that **there are no implicit conversions
/// at all**. Where another language would quietly widen a character into an
/// integer, this one makes you say which of the two you meant; and because the
/// answer is spelled out, `char(n)` may reject at run time an `n` that names no
/// character rather than inventing one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prim {
    Int,
    Float,
    Char,
    Str,
    Bool,
}

impl Prim {
    /// Every one of them, in the order a diagnostic reads them out.
    ///
    /// Rust cannot be asked what an enum's variants are, so this is written
    /// down — and [`tests::every_prim_is_in_prim_all`] is what holds it to the
    /// enum, the same bargain [`crate::vocabulary::Role::ALL`] makes.
    pub const ALL: [Prim; 5] = [Prim::Int, Prim::Float, Prim::Char, Prim::Str, Prim::Bool];

    /// The type this converts to.
    pub fn ty(self) -> Ty {
        match self {
            Prim::Int => Ty::Int,
            Prim::Float => Ty::Float,
            Prim::Char => Ty::Char,
            Prim::Str => Ty::Str,
            Prim::Bool => Ty::Bool,
        }
    }

    /// The keyword that writes it.
    ///
    /// **The one place the type vocabulary and the token vocabulary meet**, and
    /// the reason no type is spelled out anywhere in this file:
    /// [`TokenKind::text`] owns every spelling in the language, and everything
    /// here borrows it. Two copies of the word `float` are two places to forget
    /// one.
    pub fn keyword(self) -> TokenKind {
        match self {
            Prim::Int => TokenKind::KwInt,
            Prim::Float => TokenKind::KwFloat,
            Prim::Char => TokenKind::KwChar,
            Prim::Str => TokenKind::KwString,
            Prim::Bool => TokenKind::KwBool,
        }
    }

    /// How the conversion is spelled, which is also its target type's name.
    ///
    /// [`Ty::name`] needs a [`TypeTable`] because a `Ty` may be an enum or a
    /// class, whose names are the program's. None of these five is, so this one
    /// needs nothing handed in.
    pub fn name(self) -> &'static str {
        self.keyword().text()
    }

    /// The type a keyword names, or `None` when the token is not one of them.
    ///
    /// What the parser asks instead of listing the keywords — in four places,
    /// which is how many copies of that list there used to be.
    pub fn of_keyword(kind: &TokenKind) -> Option<Prim> {
        Prim::ALL.into_iter().find(|prim| prim.keyword() == *kind)
    }

    /// The same question asked of a name rather than a token, which is what
    /// resolving a written type comes down to once enums and classes have been
    /// ruled out.
    pub fn of_name(name: &str) -> Option<Prim> {
        Prim::ALL.into_iter().find(|prim| prim.name() == name)
    }

    /// The other direction, for a [`Ty`] that is one of these. `None` for an
    /// enum, a class, an array or a list — the types whose names are the
    /// program's rather than the language's.
    pub fn of_ty(ty: Ty) -> Option<Prim> {
        Prim::ALL.into_iter().find(|prim| prim.ty() == ty)
    }

    /// Them all, quoted and joined, for the diagnostic that has to say what a
    /// type may be. Generated rather than written out, so it cannot come to
    /// list four of five.
    pub fn all_quoted() -> String {
        let names: Vec<String> = Prim::ALL.iter().map(|prim| format!("`{}`", prim.name())).collect();
        match names.split_last() {
            Some((last, rest)) => format!("{}, {last}", rest.join(", ")),
            None => String::new(),
        }
    }
}

/// A function the compiler provides rather than the program declaring.
///
/// Unlike `len` and `push`, which are *constructs* because no signature could
/// describe them — one takes several unrelated types, the other takes a place
/// rather than a value — these two have signatures a TinyC program could have
/// written itself. So they are not syntax: they are names already in the table
/// when the first line is checked, called through the ordinary machinery, and
/// differing from a declared function only in having no body to compile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Builtin {
    /// `read_line() -> string` — one line of input, without its line ending.
    ///
    /// Stops the program when there is no line left, which is why [`Self::Eof`]
    /// exists: asking for something that is not there is a mistake, and there
    /// has to be a way to find out first.
    ReadLine,
    /// `eof() -> bool` — whether the input has run out.
    ///
    /// Answers *before* consuming anything, so `while (!eof())` reads every
    /// line and asks for none that is not there.
    Eof,
    /// `is_int(string) -> bool` — whether `int(s)` would answer.
    ///
    /// The same bargain [`Self::Eof`] strikes with [`Self::ReadLine`], one type
    /// further along: `int(s)` stops the program on text that spells no number,
    /// and text that spells no number is *data* rather than a mistake in the
    /// program — so there has to be a way to find out first. Nothing a program
    /// could write itself would do: the overflow that decides whether a
    /// nineteen-digit number fits can only be found by performing it, and
    /// performing it is what stops the program.
    IsInt,
}

impl Builtin {
    pub const ALL: [Builtin; 3] = [Builtin::ReadLine, Builtin::Eof, Builtin::IsInt];

    pub fn name(self) -> &'static str {
        match self {
            Builtin::ReadLine => "read_line",
            Builtin::Eof => "eof",
            Builtin::IsInt => "is_int",
        }
    }

    /// What it takes.
    ///
    /// In [`Prim`]s rather than [`Ty`]s, and that is a promise rather than a
    /// convenience. A built-in is a name in the signature table *before any
    /// program exists*, so it cannot mention a class or an enum; and an array
    /// or a list is an interned id that means nothing without a [`TypeTable`].
    /// Saying so in the type makes a signature nobody could write down
    /// impossible to write down here — which is what lets [`crate::vocabulary`]
    /// export these with no table to hand and no way to fail.
    pub fn params(self) -> &'static [Prim] {
        match self {
            Builtin::ReadLine | Builtin::Eof => &[],
            Builtin::IsInt => &[Prim::Str],
        }
    }

    pub fn ret(self) -> Option<Prim> {
        match self {
            Builtin::ReadLine => Some(Prim::Str),
            Builtin::Eof | Builtin::IsInt => Some(Prim::Bool),
        }
    }

    pub fn from_name(name: &str) -> Option<Builtin> {
        Builtin::ALL.into_iter().find(|b| b.name() == name)
    }
}

/// Index of an expression node, used by [`crate::sema`] to record its type
/// without mutating the tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeId(pub u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    /// Remainder. Paired with [`BinOp::Div`] everywhere, because on x86 they are
    /// the same instruction read from two different registers.
    Rem,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
        }
    }

    /// Whether this operator divides, and so has a zero divisor to worry about.
    pub fn divides(self) -> bool {
        matches!(self, BinOp::Div | BinOp::Rem)
    }

    /// The English name of the operation, for a diagnostic that talks about it.
    pub fn noun(self) -> &'static str {
        match self {
            BinOp::Add => "addition",
            BinOp::Sub => "subtraction",
            BinOp::Mul => "multiplication",
            BinOp::Div => "division",
            BinOp::Rem => "remainder",
        }
    }

    /// This operator's answer for two known operands, or `None` when the
    /// machine would refuse to produce one.
    ///
    /// **The one place TinyC's arithmetic is defined.** [`crate::sema`] rejects
    /// with it and [`crate::ir`] folds with it, so the two cannot come to
    /// different conclusions about what `MAX + 1` means — which they would
    /// eventually, kept apart.
    ///
    /// `Rem` refuses `MIN % -1` even though the answer is 0, because the machine
    /// reaches that 0 through the `idiv` whose *quotient* does not fit.
    pub fn apply(self, a: i64, b: i64) -> Option<i64> {
        match self {
            BinOp::Add => a.checked_add(b),
            BinOp::Sub => a.checked_sub(b),
            BinOp::Mul => a.checked_mul(b),
            BinOp::Div => a.checked_div(b),
            BinOp::Rem => a.checked_rem(b),
        }
    }

    /// The answer an integer of unlimited width would give.
    ///
    /// Only a diagnostic wants this: it is how "`9223372036854775808` does not
    /// fit in an `int`" gets to name the value that did not fit. `None` is a
    /// division by zero, which has no answer at any width.
    pub fn apply_exact(self, a: i64, b: i64) -> Option<i128> {
        // Two `i64`s cannot overflow an `i128` under any of these, so the plain
        // operators are safe here in a way they are not above.
        let (a, b) = (i128::from(a), i128::from(b));
        match self {
            BinOp::Add => Some(a + b),
            BinOp::Sub => Some(a - b),
            BinOp::Mul => Some(a * b),
            BinOp::Div => a.checked_div(b),
            BinOp::Rem => a.checked_rem(b),
        }
    }

    /// Whether the operands may be exchanged, which lets a backend pick
    /// whichever order needs fewer moves.
    pub fn commutes(self) -> bool {
        matches!(self, BinOp::Add | BinOp::Mul)
    }
}

/// Comparison operators. These take two values of the same type and produce a
/// `bool`, which is what `if` and the loops consume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    pub fn symbol(self) -> &'static str {
        match self {
            CmpOp::Eq => "==",
            CmpOp::Ne => "!=",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
    }

    /// Ordering comparisons need operands that can be ordered; `==` and `!=`
    /// only need equality.
    pub fn is_ordering(self) -> bool {
        !matches!(self, CmpOp::Eq | CmpOp::Ne)
    }

    /// The comparison that is true exactly when this one is false.
    ///
    /// Every comparison has one, which is what lets `!(a < b)` be lowered as
    /// `a >= b` — one instruction where negating the *result* would be two.
    pub fn negate(self) -> CmpOp {
        match self {
            CmpOp::Eq => CmpOp::Ne,
            CmpOp::Ne => CmpOp::Eq,
            CmpOp::Lt => CmpOp::Ge,
            CmpOp::Le => CmpOp::Gt,
            CmpOp::Gt => CmpOp::Le,
            CmpOp::Ge => CmpOp::Lt,
        }
    }
}

/// The short-circuiting operators.
///
/// Deliberately not a [`BinOp`]: `&&` and `||` do not evaluate both operands,
/// so they are not operations on two values at all. [`crate::ir`] lowers them
/// to a branch rather than to an instruction, which is why no `Instr` matches
/// them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogicOp {
    And,
    Or,
}

impl LogicOp {
    pub fn symbol(self) -> &'static str {
        match self {
            LogicOp::And => "&&",
            LogicOp::Or => "||",
        }
    }

    /// The answer the left operand gives when it settles the whole expression
    /// on its own, as the 0 or 1 a `bool` is.
    ///
    /// It doubles as the *condition* for stopping there, because the two happen
    /// to coincide: `false && x` is false, and `true || x` is true.
    pub fn short_circuit(self) -> i64 {
        match self {
            LogicOp::And => 0,
            LogicOp::Or => 1,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Expr {
    pub id: NodeId,
    /// Covers the whole subexpression, so diagnostics can underline it.
    pub span: Span,
    pub kind: ExprKind,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    Int(i64),
    /// The characters of a string literal, already decoded from the source's
    /// UTF-8. What the program manipulates is characters; UTF-8 is a transport
    /// the lexer reads and printing writes, and nothing between them sees it.
    Str(Vec<char>),
    Char(char),
    Bool(bool),
    Float(f64),
    Var(String),
    /// Unary minus, on an `int`.
    Neg(Box<Expr>),
    /// Logical negation, on a `bool`.
    Not(Box<Expr>),
    Bin { op: BinOp, lhs: Box<Expr>, rhs: Box<Expr> },
    Cmp { op: CmpOp, lhs: Box<Expr>, rhs: Box<Expr> },
    /// `lhs && rhs` or `lhs || rhs`. `rhs` is evaluated only when `lhs` did not
    /// already decide the answer, which is why this is not a [`Self::Bin`].
    Logic { op: LogicOp, lhs: Box<Expr>, rhs: Box<Expr> },
    /// `[1, 2, 3]` — the only way to make an array.
    ///
    /// Its length is its element count, and the declaration it initialises has
    /// to agree: `int[3] xs = [1, 2];` is a mistake worth catching, not a length
    /// to infer.
    Array { elements: Vec<Expr>, span: Span },
    /// `xs[i]`.
    Index { array: Box<Expr>, index: Box<Expr> },
    /// `len(xs)` — how many an array holds, or how many characters a string
    /// has.
    ///
    /// A construct rather than a library function because of the array case:
    /// there is nothing a library function could be given, since an array's
    /// length is in its *type* and folds to a literal. A string's is a load,
    /// which is the only reason this ever reaches the emitted code at all.
    Len { array: Box<Expr>, span: Span },
    /// `int(c)`, `char(n)` — a conversion, written as the type it produces.
    Convert { to: Prim, value: Box<Expr>, span: Span },
    /// `Circle { r: 5 }` — the only way to make an object.
    ///
    /// Every field the class has, inherited ones included, must be named
    /// exactly once. There is no default and no partial object, so a value of a
    /// class type is complete from the moment it exists.
    New { class: String, class_span: Span, fields: Vec<FieldInit>, span: Span },
    /// `p.x`
    Field { object: Box<Expr>, name: String, name_span: Span },
    /// `s.area(1, 2)`
    MethodCall { receiver: Box<Expr>, name: String, name_span: Span, args: Vec<Expr> },
    /// A variant of an enum, `Color::Red`.
    ///
    /// Always written qualified, so that two enums may use the same variant
    /// name and so that a variant never has to be told apart from a variable.
    /// Both halves keep a span: the enum name is what "no enum called `Color`"
    /// underlines, the variant what "`Color` has no variant `Purple`" does.
    Variant {
        enum_name: String,
        enum_span: Span,
        variant: String,
        variant_span: Span,
        /// What the variant was given, in order. Empty for one that carries
        /// nothing, which is every variant written without parentheses.
        args: Vec<Expr>,
    },
    /// `match (value) { Colour::Red => "warm", ... }`.
    ///
    /// A match is an expression, and a statement only in the way a call is one:
    /// [`Stmt::Match`] wraps this node when it is written for its effect rather
    /// than for its value.
    ///
    /// Every variant of the scrutinee's enum must have an arm, and no variant
    /// may have two. There is deliberately no catch-all pattern: the whole
    /// value of the check is that adding a variant makes every `match` that
    /// does not handle it stop compiling, and a `_` would silently swallow it.
    Match {
        /// Span of the `match` keyword, which is what "this match does not
        /// cover ..." underlines — the mistake is the arms taken together, not
        /// any one of them.
        keyword: Span,
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
    },
    /// A call, `add(1, 2)`. The enclosing [`Expr::span`] covers the whole call;
    /// `name_span` covers just the callee, which is what "unknown function"
    /// underlines. Each argument keeps its own span, so an argument of the
    /// wrong type is reported on the argument rather than on the call.
    Call { name: String, name_span: Span, args: Vec<Expr> },
}

/// A braced sequence of statements. Declarations inside a block go out of scope
/// at its closing brace.
#[derive(Clone, Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

/// What a `%` in a format string writes, and so what the argument beside it
/// must be.
///
/// One letter per printable type, and deliberately **no letter meaning
/// "whatever this happens to be"**. A specifier is a claim the program makes
/// about its own argument, and the type checker holds it to that claim — the
/// same bargain as `string(n)`, which is written out rather than inferred.
///
/// A run of values has no letter because it has no rendering: printing a list
/// would show where its elements are rather than what they are. See
/// [`Ty::is_printable`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spec {
    Int,
    Float,
    Char,
    Str,
    Bool,
    /// Writes the *name* of the variant, which is the only rendering an enum
    /// has — its value is an index and would say nothing.
    Enum,
}

/// Every specifier, in the order the "unknown specifier" note lists them.
///
/// A specifier that is not in here is a specifier that does not exist:
/// [`Spec::from_letter`] reads this list, so a variant left out of it is
/// unreachable from any source text.
pub const SPECS: [Spec; 6] =
    [Spec::Int, Spec::Float, Spec::Char, Spec::Str, Spec::Bool, Spec::Enum];

impl Spec {
    pub fn from_letter(letter: char) -> Option<Spec> {
        SPECS.into_iter().find(|spec| spec.letter() == letter)
    }

    pub fn letter(self) -> char {
        match self {
            Spec::Int => 'd',
            Spec::Float => 'f',
            Spec::Char => 'c',
            Spec::Str => 's',
            Spec::Bool => 'b',
            Spec::Enum => 'e',
        }
    }

    /// Whether a value of this type is what the specifier promised.
    ///
    /// `Spec::Enum` accepts any enum: which one is a question about the
    /// argument, and a format string that had to name it would have to be
    /// rewritten every time the argument's type did.
    pub fn accepts(self, ty: Ty) -> bool {
        matches!(
            (self, ty),
            (Spec::Int, Ty::Int)
                | (Spec::Char, Ty::Char)
                | (Spec::Str, Ty::Str)
                | (Spec::Bool, Ty::Bool)
                | (Spec::Enum, Ty::Enum(_))
                | (Spec::Float, Ty::Float)
        )
    }

    /// What it writes, named the way a diagnostic wants to say it.
    pub fn writes(self) -> &'static str {
        match self {
            Spec::Int => "an int",
            Spec::Char => "a char",
            Spec::Str => "a string",
            Spec::Bool => "a bool",
            Spec::Enum => "a variant of an enum",
            Spec::Float => "a float",
        }
    }
}

/// One thing a `print` writes, in the order it writes them.
///
/// A format string is split into these once, by the parser, and never looked at
/// again: nothing at run time reads a `%`. That is what lets every mistake in
/// one be a compile error, and it is also why the C `printf` underneath is never
/// handed a format string the program wrote — see [`crate::ir`].
#[derive(Clone, Debug)]
pub enum PrintPart {
    /// Literal text from the format string, with escapes resolved and `%%`
    /// already reduced to one `%`. Fixed at compile time, so the backend writes
    /// out its bytes rather than building the text again on every pass.
    Text(Vec<char>),
    /// A value written on its own: the `x` in `print(x)`. Any printable type
    /// will do, because nothing claimed which it would be.
    Value(Expr),
    /// A value a specifier claimed the type of: the `%d` and the `x` in
    /// `print("n = %d", x)`.
    Spec {
        spec: Spec,
        /// Span of the `%d` itself, so a mismatch can point at the claim as
        /// well as at the argument.
        span: Span,
        expr: Expr,
    },
}

#[derive(Clone, Debug)]
pub enum Stmt {
    Decl {
        /// Where the *declared* type is recorded, which is not the
        /// initialiser's: `Shape s = c;` declares a `Shape` and is given a
        /// `Circle`. Later stages need the one that was written down.
        id: NodeId,
        ty: TypeRef,
        name: String,
        name_span: Span,
        init: Expr,
    },
    /// Assignment to an already declared variable or array element.
    Assign {
        target: Place,
        value: Expr,
    },
    /// `print(...)` or `println(...)`, already split into what it writes.
    Print {
        /// Span of the `print` or `println` keyword, for diagnostics about the
        /// statement.
        span: Span,
        /// Whether the line ends after the last part — the only difference
        /// between the two spellings.
        ///
        /// Kept as written rather than folded into `parts` here, so that
        /// `--emit ast` still shows the program the way it was typed. The
        /// newline becomes an ordinary piece of text one stage later, which is
        /// where `for` becomes a `while` too.
        newline: bool,
        /// Empty for `println()`, which writes nothing but the line ending.
        parts: Vec<PrintPart>,
    },
    /// `push(xs, value);` — one more element on the end of a list.
    ///
    /// A statement rather than an expression, and its target a [`Place`] rather
    /// than an [`Expr`], because growing a list may **move** it: the elements
    /// are copied into a larger block, and whoever names the list has to be
    /// told where it went. Only a place can be told.
    Push {
        /// Span of the `push` keyword, which is what a diagnostic underlines.
        span: Span,
        target: Place,
        value: Expr,
    },
    If {
        cond: Expr,
        then_block: Block,
        else_block: Option<Block>,
    },
    While {
        cond: Expr,
        body: Block,
    },
    /// `for (init; cond; step) body`. Lowering desugars it into a `while`.
    For {
        init: Box<Stmt>,
        cond: Expr,
        step: Box<Stmt>,
        body: Block,
    },
    /// `return;` or `return expr;`.
    ///
    /// The parser accepts both spellings anywhere; whether this one agrees with
    /// the enclosing function's [`FnDecl::ret`] is a question for
    /// [`crate::sema`], which is the first stage that knows which function this
    /// statement is in.
    Return {
        /// Span of the `return` keyword, so "this function returns nothing" can
        /// underline the statement even when it carries no value.
        span: Span,
        value: Option<Expr>,
    },
    /// A `match` evaluated for its effect rather than its value.
    ///
    /// The second expression statement in the language, and it exists for the
    /// same reason the first does: [`Stmt::Call`] is what lets a function
    /// returning nothing be called at all, and this is what lets a match whose
    /// arms only *do* things be written at all. The parser only ever puts an
    /// [`ExprKind::Match`] here.
    Match(Expr),
    /// `break;` — leave the innermost enclosing loop.
    ///
    /// The parser accepts it anywhere, exactly as it does `return`; whether
    /// there *is* a loop to leave is a question for [`crate::sema`], which is
    /// the first stage that tracks how deeply the statement is nested.
    Break { span: Span },
    /// `continue;` — start the innermost enclosing loop's next iteration.
    Continue { span: Span },
    /// A call evaluated for its effect, `greet("hi");`.
    ///
    /// TinyC has no general expression statements — this is one of two, the
    /// other being [`Stmt::Match`], and it exists because a function returning
    /// nothing could not be called otherwise. The parser only ever puts an
    /// [`ExprKind::Call`] here.
    Call(Expr),
}

/// One `name: value` pair of an object literal.
#[derive(Clone, Debug)]
pub struct FieldInit {
    pub name: String,
    pub name_span: Span,
    pub value: Expr,
}

/// Where an assignment puts its value.
///
/// A separate shape from [`Expr`] rather than "an expression that happens to be
/// assignable": what may be written to is a syntactic question, so the parser
/// answers it once instead of every later stage re-deciding. Rooted at a
/// variable, because that is the only thing in TinyC that names storage.
#[derive(Clone, Debug)]
pub enum Place {
    /// `x = ...`
    Var { name: String, name_span: Span },
    /// `xs[i] = ...`
    Element { base: Box<Place>, index: Expr, span: Span },
    /// `p.x = ...`
    Field { base: Box<Place>, name: String, name_span: Span },
}

impl Place {
    /// The variable at the root of the chain, which every place has.
    pub fn root(&self) -> (&str, Span) {
        match self {
            Place::Var { name, name_span } => (name, *name_span),
            Place::Element { base, .. } | Place::Field { base, .. } => base.root(),
        }
    }

    /// Where a diagnostic about this place as a whole points.
    pub fn span(&self) -> Span {
        match self {
            Place::Var { name_span, .. } => *name_span,
            Place::Element { span, .. } => *span,
            Place::Field { base, name_span, .. } => base.span().to(*name_span),
        }
    }
}

/// What an arm does once its pattern matches.
///
/// The two spellings answer two different needs, and the token after `=>`
/// settles which is meant: a `{` opens a block, anything else begins an
/// expression.
#[derive(Clone, Debug)]
pub enum ArmBody {
    /// `Colour::Red => "warm"` — the arm's value, and the match's.
    Value(Expr),
    /// `Colour::Red => { print("hi"); return "warm"; }` — statements run for
    /// their effect.
    ///
    /// A block never *produces* a value: `return` keeps its one meaning of
    /// leaving the function, rather than gaining a second one inside a match.
    /// So where a match is used as an expression, a block arm has to be one
    /// that never reaches the end — see `sema`'s divergence check.
    Block(Block),
}

/// What an arm matches.
///
/// A pattern is a **value**, never a name to bind. TinyC has no binding
/// patterns and no destructuring, so an arm says "the scrutinee is this one"
/// and nothing else — which is what leaves exhaustiveness a question about
/// values rather than about shapes.
#[derive(Clone, Debug)]
pub enum Pattern {
    /// `Color::Red`. Always written qualified, so a variant never has to be
    /// told apart from a variable. Both halves keep a span: the enum name is
    /// what "this arm matches `X`" underlines, the variant what "`X` has no
    /// variant `Y`" does.
    Variant {
        enum_name: String,
        enum_span: Span,
        variant: String,
        variant_span: Span,
        /// The names this arm gives what the variant carries, in order. Each
        /// is a fresh variable for the length of the arm, so two arms may use
        /// the same name for quite different things.
        bindings: Vec<(String, Span)>,
    },
    /// A literal of whatever the scrutinee is: `3`, `-1`, `'a'`, `"done"`,
    /// `true`. Spelled exactly as the expression would be.
    Int(i64),
    Float(f64),
    Char(char),
    Str(Vec<char>),
    Bool(bool),
    /// `_` — everything the arms before it did not take.
    ///
    /// Deliberately **not** available when matching an enum. The whole value of
    /// the exhaustiveness check is that adding a variant stops every `match`
    /// that does not handle it from compiling, and a catch-all would swallow
    /// exactly that. An `int` or a `string` has no finite set of values to
    /// enumerate, so there it is not a catch-all but the only way to be
    /// complete — and it is required rather than optional.
    Wildcard,
}

impl Pattern {
    /// How this pattern is written, for a diagnostic that quotes it back.
    pub fn describe(&self) -> String {
        match self {
            Pattern::Variant { enum_name, variant, .. } => format!("`{enum_name}::{variant}`"),
            Pattern::Int(v) => format!("`{v}`"),
            Pattern::Float(v) => format!("`{v}`"),
            Pattern::Char(c) => format!("`'{c}'`"),
            Pattern::Str(chars) => {
                format!("`\"{}\"`", chars.iter().collect::<String>().escape_debug())
            }
            Pattern::Bool(v) => format!("`{v}`"),
            Pattern::Wildcard => "`_`".to_string(),
        }
    }

    /// The type a value has to be for this pattern to be about it, or `None`
    /// for `_`, which is about every type.
    pub fn matches_ty(&self) -> Option<Ty> {
        match self {
            Pattern::Int(_) => Some(Ty::Int),
            Pattern::Float(_) => Some(Ty::Float),
            Pattern::Char(_) => Some(Ty::Char),
            Pattern::Str(_) => Some(Ty::Str),
            Pattern::Bool(_) => Some(Ty::Bool),
            // An enum's identity is the program's, so this one cannot answer
            // on its own: `sema` resolves the name instead.
            Pattern::Variant { .. } | Pattern::Wildcard => None,
        }
    }

    /// Whether two patterns select the same values, which is what makes the
    /// second of them an arm that can never run.
    pub fn same_as(&self, other: &Pattern) -> bool {
        match (self, other) {
            (Pattern::Variant { variant: a, .. }, Pattern::Variant { variant: b, .. }) => a == b,
            (Pattern::Int(a), Pattern::Int(b)) => a == b,
            (Pattern::Char(a), Pattern::Char(b)) => a == b,
            (Pattern::Str(a), Pattern::Str(b)) => a == b,
            (Pattern::Bool(a), Pattern::Bool(b)) => a == b,
            (Pattern::Wildcard, Pattern::Wildcard) => true,
            _ => false,
        }
    }
}

/// One arm of a `match`: a pattern and what it does.
#[derive(Clone, Debug)]
pub struct MatchArm {
    pub pattern: Pattern,
    /// The whole pattern as written, which is what a diagnostic about this arm
    /// underlines.
    pub span: Span,
    pub body: ArmBody,
}

/// One variant of an enum declaration.
#[derive(Clone, Debug)]
pub struct Variant {
    pub name: String,
    pub name_span: Span,
    /// The types written after the name, if there were any: `Circle(int)`.
    ///
    /// Positional rather than named, unlike a class's fields. A variant is
    /// taken apart by a pattern rather than reached into by name, and the
    /// pattern that takes it apart is where the names get chosen —
    /// `Shape::Circle(radius)` names it whatever the reader of *that* arm
    /// wants it called.
    pub payload: Vec<TypeRef>,
}

/// One field as it was declared: `int r;`.
#[derive(Clone, Debug)]
pub struct FieldDecl {
    pub ty: TypeRef,
    pub name: String,
    pub name_span: Span,
}

/// `class Circle : Shape { int r; fn area(self) -> int { ... } }`.
///
/// Fields and methods may be written in any order; what settles a field's place
/// in the object and a method's place in the vtable is [`crate::sema`], which is
/// the first stage that knows what the base class is.
#[derive(Clone, Debug)]
pub struct ClassDecl {
    pub name: String,
    pub name_span: Span,
    /// The class this one extends, as written.
    pub base: Option<(String, Span)>,
    pub fields: Vec<FieldDecl>,
    /// Methods, as ordinary functions whose first parameter is `self`.
    ///
    /// They are lifted into [`Program::functions`] so that everything after the
    /// parser sees one kind of callable — a method is a function with a
    /// receiver, and nothing downstream needs a second concept.
    pub methods: Vec<usize>,
}

/// `enum Color { Red, Green, Blue }`.
///
/// A variant carries no payload, so a value of the type is its variant's index
/// and nothing more — which is why enums cost the backend nothing but a table
/// of names to print.
#[derive(Clone, Debug)]
pub struct EnumDecl {
    pub name: String,
    pub name_span: Span,
    pub variants: Vec<Variant>,
}

/// One parameter in a signature: `int a`.
///
/// The spans mirror [`Stmt::Decl`], because a parameter *is* a declaration —
/// the type is underlined when an argument does not match it, the name when two
/// parameters collide.
#[derive(Clone, Debug)]
pub struct Param {
    /// `None` for the `self` receiver, whose type is the class it belongs to —
    /// the one type a signature never writes down, because writing it would let
    /// it disagree with the class the method is in.
    pub ty: Option<TypeRef>,
    pub name: String,
    pub name_span: Span,
}

/// The name a method's receiver goes by.
pub const SELF: &str = "self";

/// How a match arm spells "everything the arms before me did not take".
///
/// An ordinary identifier as far as the lexer is concerned, which is the whole
/// reason it needs naming here rather than being a token of its own: nothing
/// stops a program calling a variable `_`, and only a pattern reads it as
/// anything else.
pub const WILDCARD: &str = "_";

/// A function declaration: `fn add(int a, int b) -> int { ... }`.
///
/// This is the only thing that may appear at the top level of a program.
#[derive(Clone, Debug)]
pub struct FnDecl {
    pub name: String,
    pub name_span: Span,
    pub params: Vec<Param>,
    /// The declared return type, or `None` for a function that returns nothing.
    ///
    /// Void is the *absence* of a type rather than a `Ty` variant, so every
    /// `match` on [`Ty`] elsewhere in the compiler stays about types that
    /// values really have.
    pub ret: Option<TypeRef>,
    /// Where a diagnostic about the return type points: the type itself in
    /// `-> int`, or the closing `)` when the signature declares none. Always
    /// present, so "this function returns nothing" has something to underline.
    pub ret_span: Span,
    pub body: Block,
}

#[derive(Clone, Debug)]
pub struct Program {
    /// Declared enums, in source order; an [`EnumId`] indexes this.
    pub enums: Vec<EnumDecl>,
    /// Declared classes, in source order; a [`ClassId`] indexes this.
    pub classes: Vec<ClassDecl>,
    /// Every function, methods included — see [`ClassDecl::methods`].
    pub functions: Vec<FnDecl>,
    /// Number of [`NodeId`]s handed out, i.e. the size of the type table.
    ///
    /// Ids are unique across the whole file rather than per function, so
    /// [`crate::sema`] keeps one flat table for every expression in the program.
    pub node_count: usize,
}

/// Render the tree for `--emit ast`.
pub fn dump(program: &Program) -> String {
    let mut out = String::new();
    for declaration in &program.enums {
        let variants: Vec<&str> = declaration.variants.iter().map(|v| v.name.as_str()).collect();
        out.push_str(&format!("enum {} {{ {} }}\n", declaration.name, variants.join(", ")));
    }
    for declaration in &program.classes {
        let base = match &declaration.base {
            Some((name, _)) => format!(" : {name}"),
            None => String::new(),
        };
        out.push_str(&format!("class {}{base}\n", declaration.name));
        for field in &declaration.fields {
            out.push_str(&format!("  field {} {}\n", written_type(&field.ty), field.name));
        }
        for &at in &declaration.methods {
            let mut method = String::new();
            dump_fn(&mut method, &program.functions[at]);
            for line in method.lines() {
                out.push_str(&format!("  {line}\n"));
            }
        }
    }
    // A method's body is printed under its class, so the flat list skips them.
    let methods: Vec<usize> =
        program.classes.iter().flat_map(|c| c.methods.iter().copied()).collect();
    for (at, function) in program.functions.iter().enumerate() {
        if !methods.contains(&at) {
            dump_fn(&mut out, function);
        }
    }
    out
}

/// Every index expression in a place, outermost name first.
fn dump_place_indices(out: &mut String, place: &Place, depth: usize) {
    match place {
        Place::Var { .. } => {}
        Place::Element { base, index, .. } => {
            dump_place_indices(out, base, depth);
            dump_expr(out, index, depth);
        }
        Place::Field { base, .. } => dump_place_indices(out, base, depth),
    }
}

/// A place as the program spelled it, with the index left out — the dump shows
/// the *shape*, and an index is an expression of its own.
fn place_text(place: &Place) -> String {
    match place {
        Place::Var { name, .. } => name.clone(),
        Place::Element { base, .. } => format!("{}[]", place_text(base)),
        Place::Field { base, name, .. } => format!("{}.{name}", place_text(base)),
    }
}

/// A type as the program spelled it, for the AST dump — which has no table to
/// resolve names against and does not need one.
fn written_type(ty: &TypeRef) -> String {
    match ty.shape {
        Shape::One => ty.name.clone(),
        Shape::Array(len, _) => format!("{}[{len}]", ty.name),
        Shape::List => format!("{}[]", ty.name),
    }
}

fn dump_fn(out: &mut String, function: &FnDecl) {
    let params: Vec<String> =
        function
            .params
            .iter()
            .map(|p| match &p.ty {
                Some(ty) => format!("{} {}", written_type(ty), p.name),
                None => p.name.clone(),
            })
            .collect();
    let ret = match &function.ret {
        Some(ty) => format!(" -> {}", written_type(ty)),
        None => String::new(),
    };
    out.push_str(&format!("fn {}({}){}\n", function.name, params.join(", "), ret));
    dump_block(out, &function.body, 1);
}

fn dump_stmt(out: &mut String, stmt: &Stmt, depth: usize) {
    let pad = "  ".repeat(depth);
    match stmt {
        Stmt::Decl { ty, name, init, .. } => {
            out.push_str(&format!("{pad}decl {} {name}\n", written_type(ty)));
            dump_expr(out, init, depth + 1);
        }
        Stmt::Push { target, value, .. } => {
            out.push_str(&format!("{pad}push {}\n", place_text(target)));
            dump_expr(out, value, depth + 1);
        }
        Stmt::Assign { target, value } => {
            out.push_str(&format!("{pad}assign {}\n", place_text(target)));
            // The shape of the place is on the line above; the expressions
            // inside it are trees of their own, so they go underneath.
            dump_place_indices(out, target, depth + 1);
            dump_expr(out, value, depth + 1);
        }
        Stmt::Print { newline, parts, .. } => {
            out.push_str(&format!("{pad}{}\n", if *newline { "println" } else { "print" }));
            let inner = "  ".repeat(depth + 1);
            for part in parts {
                match part {
                    PrintPart::Text(chars) => out.push_str(&format!(
                        "{inner}text {:?}\n",
                        chars.iter().collect::<String>()
                    )),
                    // A lone value is shown exactly as it was before there were
                    // parts at all: it is the whole of what the statement writes.
                    PrintPart::Value(expr) => dump_expr(out, expr, depth + 1),
                    PrintPart::Spec { spec, expr, .. } => {
                        out.push_str(&format!("{inner}%{}\n", spec.letter()));
                        dump_expr(out, expr, depth + 2);
                    }
                }
            }
        }
        Stmt::If { cond, then_block, else_block } => {
            out.push_str(&format!("{pad}if\n"));
            dump_expr(out, cond, depth + 1);
            out.push_str(&format!("{pad}then\n"));
            dump_block(out, then_block, depth + 1);
            if let Some(block) = else_block {
                out.push_str(&format!("{pad}else\n"));
                dump_block(out, block, depth + 1);
            }
        }
        Stmt::While { cond, body } => {
            out.push_str(&format!("{pad}while\n"));
            dump_expr(out, cond, depth + 1);
            dump_block(out, body, depth + 1);
        }
        Stmt::For { init, cond, step, body } => {
            out.push_str(&format!("{pad}for\n"));
            dump_stmt(out, init, depth + 1);
            dump_expr(out, cond, depth + 1);
            dump_stmt(out, step, depth + 1);
            dump_block(out, body, depth + 1);
        }
        Stmt::Return { value, .. } => {
            out.push_str(&format!("{pad}return\n"));
            if let Some(expr) = value {
                dump_expr(out, expr, depth + 1);
            }
        }
        Stmt::Match(expr) => dump_expr(out, expr, depth),
        Stmt::Break { .. } => out.push_str(&format!("{pad}break\n")),
        Stmt::Continue { .. } => out.push_str(&format!("{pad}continue\n")),
        Stmt::Call(call) => dump_expr(out, call, depth),
    }
}

fn dump_block(out: &mut String, block: &Block, depth: usize) {
    for stmt in &block.stmts {
        dump_stmt(out, stmt, depth);
    }
}

fn dump_expr(out: &mut String, expr: &Expr, depth: usize) {
    let pad = "  ".repeat(depth);
    match &expr.kind {
        ExprKind::Int(v) => out.push_str(&format!("{pad}int {v}\n")),
        ExprKind::Str(chars) => {
            out.push_str(&format!("{pad}string {:?}\n", chars.iter().collect::<String>()))
        }
        ExprKind::Char(c) => out.push_str(&format!("{pad}char {c:?}\n")),
        ExprKind::Bool(v) => out.push_str(&format!("{pad}bool {v}\n")),
        ExprKind::Float(v) => out.push_str(&format!("{pad}float {v}\n")),
        ExprKind::Var(name) => out.push_str(&format!("{pad}var {name}\n")),
        ExprKind::Variant { enum_name, variant, .. } => {
            out.push_str(&format!("{pad}variant {enum_name}::{variant}\n"))
        }
        ExprKind::Array { elements, .. } => {
            out.push_str(&format!("{pad}array\n"));
            for element in elements {
                dump_expr(out, element, depth + 1);
            }
        }
        ExprKind::Index { array, index } => {
            out.push_str(&format!("{pad}index\n"));
            dump_expr(out, array, depth + 1);
            dump_expr(out, index, depth + 1);
        }
        ExprKind::Len { array, .. } => {
            out.push_str(&format!("{pad}len\n"));
            dump_expr(out, array, depth + 1);
        }
        ExprKind::Convert { to, value, .. } => {
            out.push_str(&format!("{pad}convert to {}\n", to.name()));
            dump_expr(out, value, depth + 1);
        }
        ExprKind::New { class, fields, .. } => {
            out.push_str(&format!("{pad}new {class}\n"));
            for field in fields {
                out.push_str(&format!("{pad}  {}\n", field.name));
                dump_expr(out, &field.value, depth + 2);
            }
        }
        ExprKind::Field { object, name, .. } => {
            out.push_str(&format!("{pad}field {name}\n"));
            dump_expr(out, object, depth + 1);
        }
        ExprKind::MethodCall { receiver, name, args, .. } => {
            out.push_str(&format!("{pad}method {name}\n"));
            dump_expr(out, receiver, depth + 1);
            for arg in args {
                dump_expr(out, arg, depth + 1);
            }
        }
        ExprKind::Neg(operand) => {
            out.push_str(&format!("{pad}neg\n"));
            dump_expr(out, operand, depth + 1);
        }
        ExprKind::Not(operand) => {
            out.push_str(&format!("{pad}not\n"));
            dump_expr(out, operand, depth + 1);
        }
        ExprKind::Cmp { op, lhs, rhs } => {
            out.push_str(&format!("{pad}{}\n", op.symbol()));
            dump_expr(out, lhs, depth + 1);
            dump_expr(out, rhs, depth + 1);
        }
        ExprKind::Bin { op, lhs, rhs } => {
            out.push_str(&format!("{pad}{}\n", op.symbol()));
            dump_expr(out, lhs, depth + 1);
            dump_expr(out, rhs, depth + 1);
        }
        ExprKind::Logic { op, lhs, rhs } => {
            out.push_str(&format!("{pad}{}\n", op.symbol()));
            dump_expr(out, lhs, depth + 1);
            dump_expr(out, rhs, depth + 1);
        }
        ExprKind::Match { scrutinee, arms, .. } => {
            out.push_str(&format!("{pad}match\n"));
            dump_expr(out, scrutinee, depth + 1);
            for arm in arms {
                let pattern = arm.pattern.describe();
                out.push_str(&format!("{pad}  {}\n", pattern.trim_matches('`')));
                match &arm.body {
                    ArmBody::Value(value) => dump_expr(out, value, depth + 2),
                    ArmBody::Block(block) => dump_block(out, block, depth + 2),
                }
            }
        }
        ExprKind::Call { name, args, .. } => {
            out.push_str(&format!("{pad}call {name}\n"));
            for arg in args {
                dump_expr(out, arg, depth + 1);
            }
        }
    }
}

/// Whether a number names a character.
///
/// Not every number does, and that is the whole reason `char(n)` is checked at
/// all: the range stops at `0x10FFFF`, and the block reserved for UTF-16
/// surrogates in the middle of it names no character either.
pub fn is_scalar_value(value: i64) -> bool {
    u32::try_from(value).is_ok_and(|v| char::from_u32(v).is_some())
}

/// Whether `int(f)` has an answer, which is the same asymmetry one step along:
/// every `int` has a nearest `float`, and a float too large — or no number at
/// all — has no `int`.
///
/// Truncation is toward zero, so the bound is `-2^63 ..< 2^63` rather than
/// `..= i64::MAX`: `i64::MAX` itself is not a `float`, and the nearest one to it
/// is `2^63`, which is one past the end. Both bounds are exact powers of two
/// and so are exactly representable, which is what lets this be two comparisons
/// and no rounding of its own. A NaN fails both, as it fails every comparison.
pub fn fits_in_an_int(value: f64) -> bool {
    // Half open on purpose, and a NaN answers `false` here as it does to every
    // comparison it is put in.
    (-9223372036854775808.0..9223372036854775808.0).contains(&value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tree a source produces, as [`dump`] renders it.
    ///
    /// Going through the parser rather than building nodes by hand is what
    /// makes these tests about the language: a shape nothing can be written to
    /// produce is not one worth pinning.
    fn dumped(src: &str) -> String {
        let tokens = crate::lexer::lex(src).expect("the source should lex");
        let program = crate::parser::parse(&tokens).expect("the source should parse");
        dump(&program)
    }

    fn dumped_main(body: &str) -> String {
        let whole = dumped(&format!("fn main() {{\n{body}\n}}\n"));
        whole.strip_prefix("fn main()\n").expect("main's own header").to_string()
    }

    /// A table with one hierarchy, `Circle : Shape`, and one enum.
    ///
    /// Built by hand because these are questions about the *table*, not about
    /// any syntax: `sema` is what fills one in from a program, and going
    /// through it would test that instead.
    fn table() -> (TypeTable, ClassId, ClassId, EnumId) {
        let mut table = TypeTable::default();
        table.enums.push(EnumInfo {
            name: "Colour".to_string(),
            variants: ["Red", "Green"]
                .map(|n| VariantInfo { name: n.to_string(), payload: Vec::new() })
                .to_vec(),
        });
        table.classes.push(ClassInfo {
            name: "Shape".to_string(),
            base: None,
            fields: Vec::new(),
            methods: Vec::new(),
            size: 8,
            storage: 24,
        });
        table.classes.push(ClassInfo {
            name: "Circle".to_string(),
            base: Some(ClassId(0)),
            fields: vec![FieldInfo { name: "r".to_string(), ty: Ty::Int, offset: 8 }],
            methods: Vec::new(),
            size: 16,
            // Every class of a hierarchy reserves the hierarchy's maximum.
            storage: 24,
        });
        (table, ClassId(0), ClassId(1), EnumId(0))
    }

    // -- what a type answers -----------------------------------------------

    #[test]
    fn a_type_names_itself_through_the_table_that_holds_its_name() {
        let (mut table, shape, _, colour) = table();
        table.arrays.push(ArrayInfo { elem: Ty::Int, len: 3 });
        table.lists.push(Ty::Str);

        assert_eq!(Ty::Int.name(&table), "int");
        assert_eq!(Ty::Str.name(&table), "string");
        assert_eq!(Ty::Char.name(&table), "char");
        assert_eq!(Ty::Bool.name(&table), "bool");
        assert_eq!(Ty::Enum(colour).name(&table), "Colour");
        assert_eq!(Ty::Class(shape).name(&table), "Shape");
        assert_eq!(Ty::Array(ArrayId(0)).name(&table), "int[3]");
        // The missing length *is* the name: as many as the program needs.
        assert_eq!(Ty::List(ListId(0)).name(&table), "string[]");
    }

    #[test]
    fn the_article_follows_the_spelling_rather_than_the_type() {
        let (mut table, shape, _, colour) = table();
        table.arrays.push(ArrayInfo { elem: Ty::Int, len: 3 });

        assert_eq!(Ty::Int.with_article(&table), "an `int`");
        assert_eq!(Ty::Array(ArrayId(0)).with_article(&table), "an `int[3]`");
        assert_eq!(Ty::Str.with_article(&table), "a `string`");
        assert_eq!(Ty::Class(shape).with_article(&table), "a `Shape`");
        // An enum's name is the program's, so only its spelling can decide.
        assert_eq!(Ty::Enum(colour).with_article(&table), "a `Colour`");
    }

    #[test]
    fn only_numbers_and_characters_are_ordered() {
        // An enum's variants have an order in the declaration, but not one the
        // program said anything about; two strings are not ordered either,
        // because where an accented letter sorts is a question about a language.
        assert!(Ty::Int.is_ordered());
        assert!(Ty::Char.is_ordered());
        for ty in [Ty::Str, Ty::Bool, Ty::Enum(EnumId(0)), Ty::Array(ArrayId(0))] {
            assert!(!ty.is_ordered(), "{ty:?} should not be ordered");
        }
    }

    #[test]
    fn the_aggregates_are_the_types_that_answer_no_equality() {
        // One enum whose variants carry nothing, and one whose first does.
        let mut types = TypeTable::default();
        types.enums.push(EnumInfo {
            name: "Plain".to_string(),
            variants: vec![VariantInfo { name: "A".to_string(), payload: Vec::new() }],
        });
        types.enums.push(EnumInfo {
            name: "Carries".to_string(),
            variants: vec![VariantInfo { name: "A".to_string(), payload: vec![Ty::Int] }],
        });

        for ty in [Ty::Int, Ty::Str, Ty::Char, Ty::Bool, Ty::Enum(EnumId(0))] {
            assert!(ty.has_equality(&types), "{ty:?} should compare");
        }
        // Element by element is a loop nobody asked for, and comparing the
        // addresses would answer a different question.
        for ty in [Ty::Array(ArrayId(0)), Ty::List(ListId(0)), Ty::Class(ClassId(0))] {
            assert!(!ty.has_equality(&types), "{ty:?} should not compare");
        }
        // And an enum that carries something joins them, for exactly that
        // reason: two `Circle`s are the same value only if their radii are.
        assert!(!Ty::Enum(EnumId(1)).has_equality(&types));
    }

    #[test]
    fn a_list_fits_in_a_register_and_still_cannot_be_printed() {
        // The two questions come apart on exactly this type: printing one would
        // show the address of its elements rather than the elements.
        assert!(Ty::List(ListId(0)).fits_in_a_register());
        assert!(!Ty::List(ListId(0)).is_printable());

        for ty in [Ty::Int, Ty::Str, Ty::Char, Ty::Bool, Ty::Enum(EnumId(0))] {
            assert!(ty.is_printable(), "{ty:?} should print");
            assert!(ty.fits_in_a_register(), "{ty:?} should fit in a register");
        }
        // These two live in the frame; what a register holds is their address.
        for ty in [Ty::Array(ArrayId(0)), Ty::Class(ClassId(0))] {
            assert!(!ty.fits_in_a_register(), "{ty:?} should not fit in a register");
            assert!(!ty.is_printable(), "{ty:?} should not print");
        }
    }

    /// Every printable type has exactly one specifier, and every specifier has
    /// a type.
    ///
    /// The two lists are written down apart — [`Ty::is_printable`] decides one,
    /// [`SPECS`] the other — so only a test can keep them in step. A printable
    /// type with no letter would be writable on its own and not inside a
    /// format, which is a gap nobody would notice until they hit it; a letter
    /// that accepted two types would be a claim that checks nothing.
    #[test]
    fn every_printable_type_has_exactly_one_specifier() {
        // The primitives come from `Prim::ALL` rather than being listed, so a
        // type added there and forgotten in `SPECS` fails here — an enum is the
        // one printable type that is not a primitive, and is named.
        let mut printable: Vec<Ty> = Prim::ALL.iter().map(|prim| prim.ty()).collect();
        printable.push(Ty::Enum(EnumId(0)));
        for ty in printable.iter().copied() {
            let accepting = SPECS.iter().filter(|spec| spec.accepts(ty)).count();
            assert_eq!(accepting, 1, "{ty:?} should have exactly one specifier");
        }
        for spec in SPECS {
            assert!(
                printable.iter().any(|&ty| spec.accepts(ty)),
                "`%{}` accepts nothing printable",
                spec.letter()
            );
        }
        // And nothing unprintable slips in under one.
        for ty in [Ty::List(ListId(0)), Ty::Array(ArrayId(0)), Ty::Class(ClassId(0))] {
            assert!(!SPECS.iter().any(|spec| spec.accepts(ty)), "{ty:?} should have none");
        }
    }

    // -- the primitive table ------------------------------------------------

    /// [`Prim::ALL`] really is all of them, the way [`crate::vocabulary::Role`]
    /// makes the same promise: the match below is exhaustive, so adding a
    /// variant stops this file compiling until the new one is named.
    #[test]
    fn every_prim_is_in_prim_all() {
        for prim in Prim::ALL {
            match prim {
                Prim::Int | Prim::Float | Prim::Char | Prim::Str | Prim::Bool => {}
            }
        }
        // `[Prim; N]` already refuses a variant left out. What it cannot catch
        // is one listed twice, which a duplicate name would be.
        let names: Vec<&str> = Prim::ALL.iter().map(|prim| prim.name()).collect();
        for (at, name) in names.iter().enumerate() {
            assert!(!names[at + 1..].contains(name), "two primitives are called `{name}`");
        }
    }

    /// The four ways of naming a primitive all answer each other.
    ///
    /// This is what lets the parser ask `Prim::of_keyword` instead of listing
    /// the keywords, `sema` ask `Prim::of_name` instead of listing the names,
    /// and [`Ty::name`] delegate rather than spelling one out. Each of those
    /// used to be a copy of this table; each is now a lookup *into* it, and a
    /// lookup that does not round-trip would break all three at once.
    #[test]
    fn a_primitive_is_reached_the_same_way_from_all_four_directions() {
        for prim in Prim::ALL {
            assert_eq!(Prim::of_keyword(&prim.keyword()), Some(prim));
            assert_eq!(Prim::of_name(prim.name()), Some(prim), "{}", prim.name());
            assert_eq!(Prim::of_ty(prim.ty()), Some(prim), "{}", prim.name());
            // And the spelling really is the keyword's, not a second copy.
            assert_eq!(prim.name(), prim.keyword().text());
        }
        // The types whose names are the program's, not the language's.
        assert_eq!(Prim::of_ty(Ty::Enum(EnumId(0))), None);
        assert_eq!(Prim::of_ty(Ty::List(ListId(0))), None);
        assert_eq!(Prim::of_name("Colour"), None);
        assert_eq!(Prim::of_keyword(&TokenKind::Ident("Colour".to_string())), None);
        assert_eq!(Prim::of_keyword(&TokenKind::KwIf), None);
    }

    /// The diagnostic's list is generated from the table, so it cannot come to
    /// name four types out of five — which is what `sema`'s "the built-in types
    /// are …" note had quietly done.
    #[test]
    fn the_list_a_diagnostic_reads_out_is_the_table() {
        let quoted = Prim::all_quoted();
        for prim in Prim::ALL {
            assert!(quoted.contains(&format!("`{}`", prim.name())), "{quoted} omits {prim:?}");
        }
        assert_eq!(quoted.matches('`').count(), Prim::ALL.len() * 2);
        // Read as prose rather than as a comma-separated dump.
        assert!(quoted.ends_with(&format!(", `{}`", Prim::ALL[Prim::ALL.len() - 1].name())));
    }

    /// A letter is one specifier or none: the two directions of the mapping
    /// agree, so a diagnostic cannot name a letter the splitter would refuse.
    #[test]
    fn a_letter_and_a_specifier_name_each_other() {
        for spec in SPECS {
            assert_eq!(Spec::from_letter(spec.letter()), Some(spec));
        }
        for letter in ['%', 'q', 'x', ' ', 'D'] {
            assert_eq!(Spec::from_letter(letter), None, "`%{letter}` is not a specifier");
        }
    }

    // -- the type table ----------------------------------------------------

    #[test]
    fn a_class_descends_from_itself_and_from_its_ancestors() {
        let (table, shape, circle, _) = table();
        assert!(table.descends_from(circle, shape));
        assert!(table.descends_from(circle, circle));
        assert!(table.descends_from(shape, shape));
        // ... and not the other way, which is what makes a downcast a mistake.
        assert!(!table.descends_from(shape, circle));
    }

    #[test]
    fn only_a_subclass_coerces_and_only_upwards() {
        let (table, shape, circle, _) = table();
        assert!(table.coerces(Ty::Class(circle), Ty::Class(shape)));
        assert!(!table.coerces(Ty::Class(shape), Ty::Class(circle)));
        // The only widening in the language: everything else is equality.
        assert!(table.coerces(Ty::Int, Ty::Int));
        assert!(!table.coerces(Ty::Int, Ty::Char));
    }

    #[test]
    fn every_class_of_a_hierarchy_reserves_the_same_room() {
        let (mut table, shape, circle, _) = table();
        // Not `size`: a `Circle` written into a `Shape` has to keep its vtable
        // pointer and its fields, so storage is the hierarchy's maximum.
        assert_eq!(table.size_of(Ty::Class(shape)), 24);
        assert_eq!(table.size_of(Ty::Class(circle)), 24);
        assert_eq!(table.class(circle).size, 16);

        // Everything that fits in a register is eight, whichever of them it is.
        for ty in [Ty::Int, Ty::Str, Ty::Char, Ty::Bool, Ty::Enum(EnumId(0))] {
            assert_eq!(table.size_of(ty), 8, "{ty:?}");
        }

        table.arrays.push(ArrayInfo { elem: Ty::Class(circle), len: 3 });
        assert_eq!(
            table.size_of(Ty::Array(ArrayId(0))),
            72,
            "three slots of the hierarchy's room"
        );
    }

    #[test]
    fn the_root_of_a_hierarchy_is_what_settles_its_room() {
        let (table, shape, circle, _) = table();
        assert_eq!(table.root_of(circle), shape);
        assert_eq!(table.root_of(shape), shape);
    }

    #[test]
    fn a_class_is_sealed_when_nothing_extends_it() {
        let (table, shape, circle, _) = table();
        // Only whole-program compilation can answer this, and it is what makes
        // a call on a `Circle` a direct one.
        assert!(table.is_sealed(circle));
        assert!(!table.is_sealed(shape));
    }

    #[test]
    fn a_field_and_a_method_are_found_by_name_or_not_at_all() {
        let (table, _, circle, _) = table();
        assert_eq!(table.class(circle).field("r").map(|f| f.offset), Some(8));
        assert!(table.class(circle).field("nope").is_none());
        assert!(table.class(circle).method("area").is_none());
    }

    #[test]
    fn a_variants_tag_is_where_it_was_written() {
        let (table, _, _, colour) = table();
        assert_eq!(table.enum_info(colour).tag("Red"), Some(0));
        assert_eq!(table.enum_info(colour).tag("Green"), Some(1));
        assert_eq!(table.enum_info(colour).tag("Blue"), None);
    }

    // -- operators ---------------------------------------------------------

    #[test]
    fn arithmetic_answers_nothing_where_the_machine_would_not() {
        assert_eq!(BinOp::Add.apply(2, 3), Some(5));
        assert_eq!(BinOp::Sub.apply(2, 3), Some(-1));
        assert_eq!(BinOp::Mul.apply(6, 7), Some(42));
        assert_eq!(BinOp::Div.apply(7, 2), Some(3), "towards zero");
        assert_eq!(BinOp::Div.apply(-7, 2), Some(-3), "towards zero from below too");
        assert_eq!(BinOp::Rem.apply(-7, 2), Some(-1), "the sign is the dividend's");

        assert_eq!(BinOp::Add.apply(i64::MAX, 1), None);
        assert_eq!(BinOp::Sub.apply(i64::MIN, 1), None);
        assert_eq!(BinOp::Mul.apply(i64::MAX, 2), None);
        assert_eq!(BinOp::Div.apply(1, 0), None);
        assert_eq!(BinOp::Rem.apply(1, 0), None);
        assert_eq!(BinOp::Div.apply(i64::MIN, -1), None);
        // `MIN % -1` is 0 on paper, and the machine reaches that 0 through the
        // `idiv` whose *quotient* does not fit — so it is refused too.
        assert_eq!(BinOp::Rem.apply(i64::MIN, -1), None);
    }

    #[test]
    fn the_exact_answer_is_what_names_the_value_that_did_not_fit() {
        // Only a diagnostic wants this, and it is why an overflow message can
        // say which number an `int` could not hold.
        assert_eq!(BinOp::Add.apply_exact(i64::MAX, 1), Some(i128::from(i64::MAX) + 1));
        assert_eq!(BinOp::Sub.apply_exact(i64::MIN, 1), Some(i128::from(i64::MIN) - 1));
        assert_eq!(
            BinOp::Mul.apply_exact(i64::MIN, i64::MIN),
            Some(i128::from(i64::MIN) * i128::from(i64::MIN))
        );
        // `MIN / -1` overflows an `i64` and does not overflow an `i128`, which
        // is exactly the point of asking at the wider width.
        assert_eq!(BinOp::Div.apply_exact(i64::MIN, -1), Some(-i128::from(i64::MIN)));
        // A division by zero has no answer at any width.
        assert_eq!(BinOp::Div.apply_exact(1, 0), None);
        assert_eq!(BinOp::Rem.apply_exact(1, 0), None);
    }

    #[test]
    fn only_addition_and_multiplication_may_have_their_operands_exchanged() {
        assert!(BinOp::Add.commutes());
        assert!(BinOp::Mul.commutes());
        for op in [BinOp::Sub, BinOp::Div, BinOp::Rem] {
            assert!(!op.commutes(), "{} does not commute", op.symbol());
        }
    }

    #[test]
    fn the_two_operators_with_a_zero_divisor_to_worry_about() {
        assert!(BinOp::Div.divides());
        assert!(BinOp::Rem.divides());
        for op in [BinOp::Add, BinOp::Sub, BinOp::Mul] {
            assert!(!op.divides(), "{} does not divide", op.symbol());
        }
    }

    #[test]
    fn every_arithmetic_operator_has_a_symbol_and_a_noun_of_its_own() {
        let ops = [BinOp::Add, BinOp::Sub, BinOp::Mul, BinOp::Div, BinOp::Rem];
        let symbols: Vec<&str> = ops.iter().map(|op| op.symbol()).collect();
        assert_eq!(symbols, vec!["+", "-", "*", "/", "%"]);
        // The noun is what a diagnostic talks about, so none may be empty or
        // shared with another operator.
        let nouns: Vec<&str> = ops.iter().map(|op| op.noun()).collect();
        for (at, noun) in nouns.iter().enumerate() {
            assert!(!noun.is_empty());
            assert!(!nouns[at + 1..].contains(noun), "two operators share `{noun}`");
        }
    }

    #[test]
    fn negating_a_comparison_twice_gives_it_back() {
        // Every comparison has an opposite, which is what lets `!(a < b)` be
        // lowered as `a >= b` rather than as a negated result.
        for op in [CmpOp::Eq, CmpOp::Ne, CmpOp::Lt, CmpOp::Le, CmpOp::Gt, CmpOp::Ge] {
            assert_ne!(op.negate(), op, "{} is its own opposite", op.symbol());
            assert_eq!(op.negate().negate(), op, "{}", op.symbol());
        }
        assert_eq!(CmpOp::Lt.negate(), CmpOp::Ge);
        assert_eq!(CmpOp::Le.negate(), CmpOp::Gt);
    }

    #[test]
    fn only_the_four_orderings_need_operands_that_can_be_ordered() {
        assert!(!CmpOp::Eq.is_ordering());
        assert!(!CmpOp::Ne.is_ordering());
        for op in [CmpOp::Lt, CmpOp::Le, CmpOp::Gt, CmpOp::Ge] {
            assert!(op.is_ordering(), "{}", op.symbol());
        }
    }

    #[test]
    fn a_short_circuits_answer_is_also_its_condition_for_stopping() {
        // They coincide: `false && x` is false, and `true || x` is true.
        assert_eq!(LogicOp::And.short_circuit(), 0);
        assert_eq!(LogicOp::Or.short_circuit(), 1);
        assert_eq!(LogicOp::And.symbol(), "&&");
        assert_eq!(LogicOp::Or.symbol(), "||");
    }

    // -- conversions and builtins ------------------------------------------

    #[test]
    fn a_conversion_is_spelled_as_the_type_it_produces() {
        for (prim, ty) in [
            (Prim::Int, Ty::Int),
            (Prim::Char, Ty::Char),
            (Prim::Str, Ty::Str),
            (Prim::Bool, Ty::Bool),
        ] {
            assert_eq!(prim.ty(), ty);
            assert_eq!(prim.name(), ty.name(&TypeTable::default()));
        }
    }

    #[test]
    fn a_builtin_is_found_by_the_name_it_answers_to_and_by_no_other() {
        for builtin in Builtin::ALL {
            assert_eq!(Builtin::from_name(builtin.name()), Some(builtin));
            // Every one answers something: a built-in called for its effect
            // would have nothing to distinguish it from a statement.
            assert!(builtin.ret().is_some());
        }
        assert_eq!(Builtin::ReadLine.ret(), Some(Prim::Str));
        assert_eq!(Builtin::Eof.ret(), Some(Prim::Bool));
        assert_eq!(Builtin::IsInt.params(), &[Prim::Str]);
        assert_eq!(Builtin::IsInt.ret(), Some(Prim::Bool));
        // The signature reaches `sema` as types, and this is that widening.
        assert_eq!(Builtin::IsInt.ret().map(Prim::ty), Some(Ty::Bool));
        assert_eq!(Builtin::from_name("print"), None);
        assert_eq!(Builtin::from_name(""), None);
    }

    #[test]
    fn only_some_numbers_name_a_character() {
        assert!(is_scalar_value(0));
        assert!(is_scalar_value(0xD7FF), "the last one below the surrogates");
        assert!(is_scalar_value(0xE000), "the first one above them");
        assert!(is_scalar_value(0x10FFFF), "the last one there is");

        assert!(!is_scalar_value(-1));
        assert!(!is_scalar_value(0xD800), "the surrogate block names nothing");
        assert!(!is_scalar_value(0xDFFF));
        assert!(!is_scalar_value(0x110000), "one past the end");
        assert!(!is_scalar_value(i64::MAX));
    }

    // -- places ------------------------------------------------------------

    #[test]
    fn every_place_is_rooted_at_a_variable() {
        // The only thing in TinyC that names storage, which is what keeps an
        // address from ever having to travel outward.
        let at = Span::new(0, 1);
        let var = Place::Var { name: "xs".to_string(), name_span: at };
        assert_eq!(var.root().0, "xs");

        let element = Place::Element {
            base: Box::new(var.clone()),
            index: Expr { id: NodeId(0), span: at, kind: ExprKind::Int(0) },
            span: Span::new(0, 5),
        };
        assert_eq!(element.root().0, "xs");
        assert_eq!(element.span(), Span::new(0, 5), "an element underlines the whole of it");

        let field = Place::Field {
            base: Box::new(element),
            name: "r".to_string(),
            name_span: Span::new(6, 1),
        };
        assert_eq!(field.root().0, "xs");
        assert_eq!(field.span(), Span::new(0, 7), "a field reaches back to what it is on");
    }

    // -- the dump ----------------------------------------------------------

    #[test]
    fn the_dump_shows_precedence_as_the_shape_of_the_tree() {
        // The whole point of the dump: the tree, not the source it came from.
        assert_eq!(
            dumped_main("print(1 + 2 * 3);"),
            "  print\n    +\n      int 1\n      *\n        int 2\n        int 3\n"
        );
        assert_eq!(
            dumped_main("print((1 + 2) * 3);"),
            "  print\n    *\n      +\n        int 1\n        int 2\n      int 3\n"
        );
    }

    #[test]
    fn the_dump_names_every_kind_of_literal() {
        assert_eq!(dumped_main("print(1);"), "  print\n    int 1\n");
        assert_eq!(dumped_main("print('x');"), "  print\n    char 'x'\n");
        assert_eq!(dumped_main("print(true);"), "  print\n    bool true\n");
        // A string *literal* in that position is a format, and by the time the
        // tree is dumped it is the text it stands for rather than an expression.
        assert_eq!(dumped_main("print(\"a\\nb\");"), "  print\n    text \"a\\nb\"\n");
        // It is still an expression anywhere else, and still dumps as one.
        assert_eq!(
            dumped_main("string s = \"a\\nb\";"),
            "  decl string s\n    string \"a\\nb\"\n"
        );
    }

    /// `println` is not `print`, and the tree says which was written.
    ///
    /// The newline it adds is not here: it becomes a piece of text one stage
    /// later, so that `--emit ast` shows the program rather than the
    /// desugaring — the same bargain `for` gets.
    #[test]
    fn the_dump_tells_the_two_spellings_apart() {
        assert_eq!(dumped_main("println(1);"), "  println\n    int 1\n");
        assert_eq!(dumped_main("println();"), "  println\n");
    }

    /// A format's pieces are shown in the order they are written, each
    /// specifier over the value that fills it.
    #[test]
    fn the_dump_shows_a_formats_pieces_in_order() {
        assert_eq!(
            dumped_main("int n = 1;\nprintln(\"a %d b\", n);"),
            "  decl int n\n    int 1\n  println\n    text \"a \"\n    %d\n      var n\n    text \" b\"\n"
        );
    }

    #[test]
    fn a_declared_type_is_dumped_as_it_was_written() {
        // The dump has no table to resolve names against and needs none: what
        // it shows is the syntax, brackets included.
        assert!(dumped_main("int[3] xs = [1, 2, 3];").starts_with("  decl int[3] xs\n"));
        assert!(dumped_main("int[] ys = [];").starts_with("  decl int[] ys\n"));
        assert!(dumped_main("string s = \"a\";").starts_with("  decl string s\n"));
    }

    #[test]
    fn an_assignment_shows_the_shape_of_its_place_and_the_indices_underneath() {
        // The place is a shape, and an index inside it is an expression of its
        // own — so `xs[i] = 1` shows `xs[]` with `i` and `1` under it.
        let dumped = dumped_main("int[2] xs = [0, 0];\nint i = 0;\nxs[i] = 1;");
        assert!(dumped.ends_with("  assign xs[]\n    var i\n    int 1\n"), "{dumped}");
    }

    #[test]
    fn a_method_body_is_printed_under_its_class_and_not_again_in_the_flat_list() {
        let dumped = dumped(
            "class Shape {\n  int t;\n  fn area(self) -> int { return self.t; }\n}\n\
             fn main() {\n}\n",
        );
        assert_eq!(dumped.matches("fn area(self) -> int").count(), 1, "{dumped}");
        // Indented, because it belongs to the class above it.
        assert!(dumped.contains("\n  fn area(self) -> int\n"), "{dumped}");
        assert!(dumped.contains("class Shape\n  field int t\n"), "{dumped}");
    }

    #[test]
    fn a_base_class_and_an_enums_variants_are_named_on_their_own_line() {
        assert!(dumped("enum C { R, G }\nfn main() {\n}\n").starts_with("enum C { R, G }\n"));
        let hierarchy = dumped("class A {\n}\nclass B : A {\n}\nfn main() {\n}\n");
        assert!(hierarchy.contains("class A\n"), "{hierarchy}");
        assert!(hierarchy.contains("class B : A\n"), "{hierarchy}");
    }

    #[test]
    fn every_statement_reaches_the_dump() {
        // A shape the dump forgot would print nothing at all, and `--emit ast`
        // would quietly lie about the program.
        let body = "int a = 1;\n\
                    a = 2;\n\
                    print(a);\n\
                    int[] ys = [];\n\
                    push(ys, a);\n\
                    if (a == 2) {\n  a = 3;\n} else {\n  a = 4;\n}\n\
                    while (a < 5) {\n  a = a + 1;\n  break;\n}\n\
                    for (int i = 0; i < 2; i = i + 1) {\n  continue;\n}\n\
                    nothing();\n\
                    return;";
        let dumped = dumped(&format!("fn nothing() {{\n}}\nfn main() {{\n{body}\n}}\n"));
        for expected in [
            "decl int a", "assign a", "print", "push ys", "if", "then", "else", "while", "for",
            "break", "continue", "call nothing", "return",
        ] {
            assert!(dumped.contains(expected), "no `{expected}` in the dump:\n{dumped}");
        }
    }

    #[test]
    fn every_expression_reaches_the_dump() {
        let body = "int a = -1;\n\
                    bool b = !(a < 0) && a >= 0 || a == 0;\n\
                    Colour k = Colour::Red;\n\
                    int[2] xs = [1, 2];\n\
                    print(len(xs) + xs[0]);\n\
                    print(int('z'));\n\
                    Circle c = Circle { r: 1 };\n\
                    print(c.r);\n\
                    print(c.get());\n\
                    print(match (k) { Colour::Red => 1, Colour::Green => 2, });";
        let dumped = dumped(&format!(
            "enum Colour {{ Red, Green }}\n\
             class Circle {{\n  int r;\n  fn get(self) -> int {{ return self.r; }}\n}}\n\
             fn main() {{\n{body}\n}}\n"
        ));
        for expected in [
            "neg", "not", "&&", "||", "==", ">=", "<", "variant Colour::Red", "array", "len",
            "index", "convert to int", "new Circle", "field r", "method get", "match",
            "  Colour::Red", "int 1",
        ] {
            assert!(dumped.contains(expected), "no `{expected}` in the dump:\n{dumped}");
        }
    }
}
