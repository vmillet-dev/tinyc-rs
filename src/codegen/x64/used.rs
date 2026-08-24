//! What the program actually needs out of the runtime.
//!
//! Answered in one sweep over the IR rather than one per question, so nothing
//! unused is declared, emitted or linked. A program that touches no string
//! carries no arena; one that only writes literal text carries no encoder.
//!
//! Nothing here is platform-specific: these are questions about the *program*.
//! Which symbols the answers turn into is [`super::Platform::externs`]'s
//! business.

use crate::ast::{ClassId, EnumId, Ty};
use crate::ir::{Instr, Program, Runtime};

use super::{format_index, may_abort};

pub struct Used {
    formats: [bool; 3],
    /// Which enums have a value printed, and so need their table of variant
    /// names emitted. An enum used only in a `match` needs none: matching is
    /// arithmetic on the tag, and never asks what the tag is called.
    pub enums: Vec<bool>,
    /// Which classes are ever instantiated, and so need their method table
    /// emitted. A class nothing builds has no objects to dispatch on.
    pub vtables: Vec<bool>,
    pub aborts: bool,
    /// Which of the compiler's own routines the program reaches, directly or
    /// through another one. A program that never touches a string pays for
    /// none of them — not even the arena.
    pub concat: bool,
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
}

impl Used {
    pub fn of(program: &Program) -> Used {
        let mut used = Used {
            formats: [false; 3],
            enums: vec![false; program.table.enums.len()],
            vtables: vec![false; program.vtables.len()],
            aborts: false,
            concat: false,
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
        };
        for instr in program.functions.iter().flat_map(|f| &f.blocks).flat_map(|b| &b.instrs) {
            match instr {
                Instr::Print { ty, .. } => {
                    used.formats[format_index(*ty)] = true;
                    match ty {
                        Ty::Enum(id) => used.enums[id.0 as usize] = true,
                        Ty::Str => used.print_str = true,
                        Ty::Char => used.print_char = true,
                        _ => {}
                    }
                }
                // Text is written by handing `printf` bytes that are already in
                // the file, so it needs the `%s` format and nothing else.
                Instr::PrintText { .. } => {
                    used.formats[format_index(Ty::Str)] = true;
                    used.print_text = true;
                }
                Instr::VTable { class, .. } => used.vtables[class.0 as usize] = true,
                other => {
                    used.aborts |= may_abort(other);
                    if let Instr::RtCall { callee, .. } = other {
                        match callee {
                            Runtime::Concat => used.concat = true,
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
        used.list_new |= used.list_clone | used.read_line;
        used.list_push |= used.read_line;
        used.chars_str |= used.read_line;
        // Asking the arena for memory is a way to fail like any other.
        used.aborts |= used.allocates();
        used
    }

    pub fn prints(&self, ty: Ty) -> bool {
        self.formats[format_index(ty)]
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

/// The label of a class's method table.
pub fn vtable_label(class: ClassId) -> String {
    format!("vtable{}", class.0)
}

/// The label of the table that maps an enum's tags to its variants' names.
pub fn enum_table(id: EnumId) -> String {
    format!("enum{}_names", id.0)
}

/// The label of one variant's name within that table.
pub fn enum_variant_text(id: EnumId, tag: usize) -> String {
    format!("enum{}_v{tag}", id.0)
}
