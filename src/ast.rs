//! The abstract syntax tree produced by the parser.

use crate::diag::Span;

/// Index of an enum in [`Program::enums`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EnumId(pub u32);

/// Index of an array type in [`TypeTable::arrays`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArrayId(pub u32);

/// Index of a list type in [`TypeTable::lists`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ListId(pub u32);

/// What every stage after the parser needs to know about a declared enum: what
/// it is called, and what its variants are called *in order*.
///
/// The order is the whole representation — a variant's tag is its position — so
/// this is also the table the runtime prints a value's name from.
#[derive(Clone, Debug)]
pub struct EnumInfo {
    pub name: String,
    pub variants: Vec<String>,
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
    /// Everything that fits in a register is eight, which is what makes a
    /// field's offset its position. An object takes its hierarchy's room, and
    /// an array its length times whatever it holds.
    pub fn size_of(&self, ty: Ty) -> u32 {
        match ty {
            Ty::Class(id) => self.class(id).storage,
            Ty::Array(id) => {
                let info = self.array(id);
                info.len * self.size_of(info.elem)
            }
            _ => 8,
        }
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
        self.variants.iter().position(|v| v == variant).map(|at| at as i64)
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
            Ty::Int => "int".to_string(),
            Ty::Str => "string".to_string(),
            Ty::Char => "char".to_string(),
            Ty::Bool => "bool".to_string(),
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
    pub fn is_ordered(self) -> bool {
        matches!(self, Ty::Int | Ty::Char)
    }

    /// Whether two values of this type can be compared for equality at all.
    ///
    /// Strings can, and it is their *contents* that answer: comparing the
    /// addresses would quietly answer a different question, so this one costs a
    /// call. Arrays and objects cannot — element by element is a loop nobody
    /// asked for, and the addresses are not what anybody meant.
    pub fn has_equality(self) -> bool {
        !matches!(self, Ty::Array(_) | Ty::List(_) | Ty::Class(_))
    }

    /// Whether `print` can render a value of this type.
    ///
    /// Not the same question as whether it fits in a register: a list does, and
    /// printing it would show the address of its elements rather than the
    /// elements — which is the answer to a question nobody asked.
    pub fn is_printable(self) -> bool {
        self.fits_in_a_register() && !matches!(self, Ty::List(_))
    }

    /// Whether a value of this type travels in a register.
    ///
    /// Arrays and objects do not: they live in the frame, and what a register
    /// holds is their address. That is why assigning one copies rather than
    /// aliases, and why returning one is done by filling room the *caller*
    /// reserved — an address that never travels outward cannot dangle.
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

/// A type written where a value is expected, which is how TinyC spells a
/// conversion: `int(c)` is the code point of a character, `char(n)` the
/// character with that code point.
///
/// Only the types with a keyword can be written this way, which is what keeps
/// the form unambiguous — every other type name is an identifier, and an
/// identifier followed by `(` is a call.
///
/// The point of the form is that **there are no implicit conversions at all**.
/// Where another language would quietly widen a character into an integer, this
/// one makes you say which of the two you meant; and because the answer is
/// spelled out, `char(n)` may reject at run time an `n` that names no character
/// rather than inventing one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prim {
    Int,
    Char,
    Str,
    Bool,
}

impl Prim {
    /// The type this converts to.
    pub fn ty(self) -> Ty {
        match self {
            Prim::Int => Ty::Int,
            Prim::Char => Ty::Char,
            Prim::Str => Ty::Str,
            Prim::Bool => Ty::Bool,
        }
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
    Variant { enum_name: String, enum_span: Span, variant: String, variant_span: Span },
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
    Print {
        /// Span of the `print` keyword, for diagnostics about the statement.
        span: Span,
        value: Expr,
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

/// One arm of a `match`: a qualified variant and what it does.
///
/// The pattern is spelled exactly as the expression would be, `Color::Red`, and
/// carries the same two spans for the same two diagnostics.
#[derive(Clone, Debug)]
pub struct MatchArm {
    pub enum_name: String,
    pub enum_span: Span,
    pub variant: String,
    pub variant_span: Span,
    pub body: ArmBody,
}

/// One variant of an enum declaration.
#[derive(Clone, Debug)]
pub struct Variant {
    pub name: String,
    pub name_span: Span,
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
        Stmt::Print { value, .. } => {
            out.push_str(&format!("{pad}print\n"));
            dump_expr(out, value, depth + 1);
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
            out.push_str(&format!("{pad}convert to {}\n", to.ty().name(&TypeTable::default())));
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
                out.push_str(&format!("{pad}  {}::{}\n", arm.enum_name, arm.variant));
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
