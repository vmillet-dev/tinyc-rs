//! What the program actually needs out of the runtime.
//!
//! Answered in one sweep over the IR rather than one per question, so nothing
//! unused is declared, emitted or linked. A program that touches no string
//! carries no arena; one that only writes literal text carries no encoder.
//!
//! Nothing here is platform-specific: these are questions about the *program*.
//! Which symbols the answers turn into is [`super::Platform::externs`]'s
//! business.

use crate::ast::{ClassId, EnumId, Ty, TypeTable};
use crate::ir::{DivGuards, Instr, Program, Runtime};

use super::{ENTRY_POINT, format_index};

pub struct Used {
    /// Which `printf` format each shape of value goes out through, indexed by
    /// [`super::format_index`]. The last slot is a string or a character,
    /// which go out through no format at all — they are written by length.
    formats: [bool; 4],
    /// The same four, for a value that *ends* a line. A program that prints
    /// without ever calling `println` carries no format that ends one.
    lines: [bool; 4],
    /// Which enums have a value printed, and so need their table of variant
    /// names emitted. An enum used only in a `match` needs none: matching is
    /// arithmetic on the tag, and never asks what the tag is called.
    pub enums: Vec<bool>,
    /// Which classes are ever instantiated, and so need their method table
    /// emitted. A class nothing builds has no objects to dispatch on.
    pub vtables: Vec<bool>,
    /// Whether anything at all can stop this program, and so whether the report
    /// that says what happened is needed. Derived from the four below and from
    /// everything the runtime can fail at.
    pub aborts: bool,
    /// Which ways of failing the program's own instructions reach. Each answers
    /// one row of [`super::runtime::ABORTS`], which is what keeps a program that
    /// never divides from carrying a message about division.
    pub div_zero: bool,
    pub div_overflow: bool,
    pub overflow: bool,
    pub bounds: bool,
    /// Whether any prologue guards against running out of stack, and so whether
    /// the entry point has to find out where the stack ends.
    ///
    /// A program of one function cannot go deeper than it already is: `main` is
    /// entered once, and only a *call* can nest. So a program that makes none
    /// carries no limit, no check and neither platform's way of asking for one.
    pub checks_stack: bool,
    /// Which of the compiler's own routines the program reaches, directly or
    /// through another one. A program that never touches a string pays for
    /// none of them — not even the arena.
    pub concat: bool,
    /// `s = s + e` where nothing else can be holding `s`. It falls back to
    /// [`Runtime::Concat`] whenever it cannot grow in place, so a program with
    /// one carries both.
    pub append: bool,
    pub str_eq: bool,
    pub check_char: bool,
    pub char_str: bool,
    pub int_str: bool,
    pub str_int: bool,
    pub is_int: bool,
    pub list_new: bool,
    pub list_push: bool,
    pub list_push_big: bool,
    pub list_clone: bool,
    pub chars_str: bool,
    pub read_line: bool,
    pub eof: bool,
    pub print_str: bool,
    /// Whether any run of literal text is written out. Its bytes are in the
    /// file already, so it needs no arena and no encoder — but it does still
    /// put characters on the console, which is a question of its own.
    pub print_text: bool,
    pub print_char: bool,
    /// Whether anything is given its own elements after a copy — the price of
    /// a list field. See [`crate::ir::Instr::Fixup`].
    pub fixup: bool,
    /// Whether any variant that carries something is built, and so whether
    /// the arena is needed for one.
    pub variant_new: bool,
    /// Which enums need the one value each of their empty variants has written
    /// down in `.data`. Only a boxed enum has any.
    pub variant_values: Vec<bool>,
}

impl Used {
    pub fn of(program: &Program) -> Used {
        let mut used = Used {
            formats: [false; 4],
            lines: [false; 4],
            enums: vec![false; program.table.enums.len()],
            vtables: vec![false; program.vtables.len()],
            aborts: false,
            div_zero: false,
            div_overflow: false,
            overflow: false,
            bounds: false,
            // Dead functions are gone by now, so anything left beside the entry
            // point is something the program really calls.
            checks_stack: program.functions.iter().any(|f| f.name != ENTRY_POINT),
            concat: false,
            append: false,
            str_eq: false,
            check_char: false,
            char_str: false,
            int_str: false,
            str_int: false,
            is_int: false,
            list_new: false,
            list_push: false,
            list_push_big: false,
            list_clone: false,
            chars_str: false,
            read_line: false,
            eof: false,
            print_str: false,
            print_char: false,
            print_text: false,
            fixup: false,
            variant_new: false,
            variant_values: vec![false; program.table.enums.len()],
        };
        for instr in program.functions.iter().flat_map(|f| &f.blocks).flat_map(|b| &b.instrs) {
            match instr {
                Instr::Print { ty, newline, .. } => {
                    // One or the other, never both: a program that only ever
                    // writes whole lines carries only the format that ends one.
                    used.formats[format_index(*ty)] |= !*newline;
                    used.lines[format_index(*ty)] |= *newline;
                    match ty {
                        Ty::Enum(id) => used.enums[id.0 as usize] = true,
                        Ty::Str => used.print_str = true,
                        Ty::Char => used.print_char = true,
                        _ => {}
                    }
                }
                // Text is bytes that are already in the file, written by the
                // count the compiler took of them: no format, and no encoder.
                Instr::PrintText { .. } => used.print_text = true,
                Instr::VTable { class, .. } => used.vtables[class.0 as usize] = true,
                Instr::Fixup { .. } => used.fixup = true,
                Instr::VariantAddr { id, .. } => used.variant_values[id.0 as usize] = true,
                other => {
                    // Which way it can fail, not merely that it can: the two
                    // questions used to be one, and the answer to the narrow
                    // one is what decides which messages the file carries.
                    match other {
                        Instr::Bin { op, lhs, rhs, .. } if op.divides() => {
                            let guards = DivGuards::of(lhs, rhs);
                            used.div_zero |= guards.zero;
                            used.div_overflow |= guards.overflow;
                        }
                        Instr::Bin { .. } => used.overflow = true,
                        Instr::Elem { .. } => used.bounds |= other.can_fail(),
                        _ => {}
                    }
                    if let Instr::RtCall { callee, .. } = other {
                        match callee {
                            Runtime::Concat => used.concat = true,
                            Runtime::Append => used.append = true,
                            Runtime::StrEq => used.str_eq = true,
                            Runtime::CheckChar => used.check_char = true,
                            Runtime::CharToStr => used.char_str = true,
                            Runtime::IntToStr => used.int_str = true,
                            Runtime::StrToInt => used.str_int = true,
                            Runtime::IsInt => used.is_int = true,
                            Runtime::ListNew => used.list_new = true,
                            Runtime::ListPush => used.list_push = true,
                            Runtime::ListPushBig => used.list_push_big = true,
                            Runtime::ListClone => used.list_clone = true,
                            // Room for a variant that carries something. It
                            // names the arena directly rather than through a
                            // routine, so this is the flag that pulls the
                            // arena in.
                            Runtime::Alloc => used.variant_new = true,
                            Runtime::CharsToStr => used.chars_str = true,
                            Runtime::ReadLine => used.read_line = true,
                            Runtime::Eof => used.eof = true,
                        }
                    }
                }
            }
        }
        // Some routines are reached without any instruction naming them: a
        // `print` encodes into a buffer cut from the arena, cloning a list
        // builds the new one with `list_new`, and reading a line accumulates
        // its characters in a list before sealing it into a string.
        // A class that is built at all and whose fields reach a list needs the
        // routine that gives a copy of it its own — and its vtable names that
        // routine whether or not anything in the program ever copies one.
        used.fixup |= (0..program.table.classes.len() as u32)
            .map(ClassId)
            .any(|id| used.vtables[id.0 as usize] && program.table.class_holds_a_list(id));
        // Giving a list field its own elements *is* a clone.
        used.list_clone |= used.fixup;
        // Appending in place is the same routine when it cannot: it falls back.
        used.concat |= used.append;
        used.list_new |= used.list_clone | used.read_line;
        used.list_push |= used.read_line;
        used.chars_str |= used.read_line;
        // Whether *anything* can stop the program is now the same question as
        // whether any row of the abort table is reached — asked in one place so
        // the report and the messages it reads can never disagree about it.
        used.aborts = super::runtime::ABORTS.iter().any(|abort| abort.reached_by(&used));
        used
    }

    pub fn prints(&self, ty: Ty) -> bool {
        self.formats[format_index(ty)]
    }

    /// Whether this class needs a routine of its own: it is built somewhere,
    /// and what it holds reaches a list.
    ///
    /// A class whose *base* holds one still needs its own, because the routine
    /// walks every field the object has — inherited ones included — and it is
    /// reached through the object's own vtable.
    pub fn owns_elements(&self, table: &TypeTable, id: ClassId) -> bool {
        self.vtables[id.0 as usize] && table.class_holds_a_list(id)
    }

    /// Whether a value of this type ever ends a line, and so whether the format
    /// that ends one is needed.
    pub fn ends_a_line(&self, ty: Ty) -> bool {
        self.lines[format_index(ty)]
    }

    /// Whether a variant name is ever written, and so whether the plain `%s`
    /// is needed.
    ///
    /// An enum is the last thing that goes out through `printf`'s `%s`, and it
    /// may because its variants' names are bytes the *compiler* wrote: no NUL
    /// is among them, so "up to the first NUL" and "all of them" are the same
    /// answer. A string cannot promise that, which is why it is written by
    /// length instead.
    pub fn prints_enum(&self) -> bool {
        self.formats[super::enum_slot()]
    }

    /// Whether `printf` is called at all.
    ///
    /// Only the shapes with a format of their own reach it now. A program that
    /// writes nothing but strings, characters and literal text needs `fwrite`
    /// and nothing else out of the C library's output half.
    pub fn needs_printf(&self) -> bool {
        self.writes(Ty::Int)
            || self.writes(Ty::Bool)
            || self.prints_enum()
            || self.lines[super::enum_slot()]
    }

    /// Whether a value of this type is ever written at all, whichever of the
    /// two formats it goes out through.
    ///
    /// The two questions came apart when a `println` stopped needing a second
    /// call, and they are not the same one: a `bool` needs the words `true` and
    /// `false` either way, and only the *format* depends on which.
    pub fn writes(&self, ty: Ty) -> bool {
        self.prints(ty) || self.ends_a_line(ty)
    }

    /// Whether a newline is ever written on its own.
    ///
    /// A string and a character go out through a routine rather than a format
    /// of their own, so a `println` of one still ends its line separately —
    /// one byte, written by the same count-taking call as everything else.
    pub fn writes_a_bare_newline(&self) -> bool {
        self.lines[super::text_slot()]
    }

    /// Whether the routine `int(s)` and `is_int(s)` are both wrappers around is
    /// needed. Nothing in the IR names it: it is reached only through them.
    pub fn parse_int(&self) -> bool {
        self.str_int || self.is_int
    }

    /// The same for the routine both pushes are wrappers around: it makes the
    /// room, and only what goes into it differs.
    pub fn list_room(&self) -> bool {
        self.list_push || self.list_push_big
    }

    /// Whether anything cuts memory out of the arena — which is what decides
    /// whether the program calls `malloc` at all.
    pub fn allocates(&self) -> bool {
        self.concat
            || self.char_str
            || self.int_str
            || self.list_new
            || self.list_room()
            || self.list_clone
            || self.chars_str
            || self.print_str
            || self.variant_new
    }

    /// Whether any character reaches the console, and so whether it may need
    /// telling what encoding is coming. Literal text counts: its bytes are
    /// UTF-8 too.
    pub fn writes_text(&self) -> bool {
        self.print_str || self.print_char || self.print_text
    }

    /// Whether anything has to be *turned into* UTF-8 first, and so whether the
    /// encoder is needed at all.
    ///
    /// A different question from [`Self::writes_text`], and the text table is
    /// what pulled them apart: a run of literal text is already the bytes it
    /// will be written as, so a program that only writes literals carries no
    /// encoder.
    pub fn encodes_text(&self) -> bool {
        self.print_str || self.print_char
    }

    /// Whether anything is read from the input, and so whether the buffer and
    /// the decoder are needed.
    pub fn reads_text(&self) -> bool {
        self.read_line || self.eof
    }
}

/// The label of one run of literal text.
pub fn text_label(index: usize) -> String {
    format!("text{index}")
}

/// The label of a class's method table — the *slots*, which is what an object's
/// vtable pointer points at.
pub fn vtable_label(class: ClassId) -> String {
    format!("vtable{}", class.0)
}

/// The word laid down immediately in front of those slots, holding the routine
/// that gives a fresh copy of this class its own elements — or zero when a copy
/// of it shares nothing.
///
/// In front rather than in a slot of its own, so that method numbering stays
/// the hierarchy's business alone and adding this changed no dispatch. A string
/// and a list carry their length the same way, for the same reason.
pub fn vtable_header(class: ClassId) -> String {
    format!("vtable{}_owns", class.0)
}

/// The label of that routine.
pub fn fixup_label(class: ClassId) -> String {
    format!("tc$rt$fixup${}", class.0)
}

/// The label of the table that maps an enum's tags to its variants' names.
pub fn enum_table(id: EnumId) -> String {
    format!("enum{}_names", id.0)
}

/// The label of the one value a variant that carries nothing has.
///
/// Only an enum some other variant of which carries something needs these: its
/// values are pointers, and a variant with no payload still has to be one.
pub fn variant_value(id: EnumId, tag: usize) -> String {
    format!("enum{}_v{tag}_value", id.0)
}

/// The label of one variant's name within that table.
pub fn enum_variant_text(id: EnumId, tag: usize) -> String {
    format!("enum{}_v{tag}", id.0)
}
