//! **The one place a primitive type is declared.**
//!
//! Adding `float` to this compiler once meant a row in five tables — a token
//! variant and its spelling, a `Ty` variant, a `Prim` variant, a row for the
//! editor's vocabulary, a format specifier — and four *copies of the list* in
//! the parser and the type checker besides. Every one of them said the same
//! thing, and any one of them could be forgotten.
//!
//! They are now one row of [`primitives!`], and everything else is generated
//! from it or looked up in it.
//!
//! # How it works
//!
//! [`primitives!`] is an *X-macro*: it does not expand to anything itself, it
//! hands the table to a macro named by the caller. Each consumer defines what a
//! row means to it and asks for the table:
//!
//! ```ignore
//! macro_rules! declare_something { ($($v:ident $spelling:literal …;)*) => { … }; }
//! primitives!(declare_something);
//! ```
//!
//! That is the same trick C compilers use, spelled in Rust rather than in the
//! preprocessor: Clang's `TokenKinds.def` and `BuiltinTypes.def` are `#include`d
//! a dozen times each with a different `#define KEYWORD(…)` in front, and GCC's
//! `tree.def` is read the same way. One table, many readings.
//!
//! # What a row cannot say
//!
//! Everything in a row is a *fact about the notation*: how the type is written,
//! which letter prints it, what the editor calls its token. None of it is a
//! **decision** — what `+` means on two of them, whether a `match` may ask about
//! one, which machine instruction compares two. Those live where they are
//! decided, and adding a row makes the compiler refuse to build until each has
//! been answered. That refusal is the point; it is not something to generate
//! away.
//!
//! This is also where the real compilers stop. `BuiltinTypes.def` gives Clang
//! the variant, the name and the printer; what `float` *means* is thousands of
//! hand-written lines elsewhere.

/// The table, and the only place to add a type.
///
/// Each row is: an identifier — used for the [`Prim`] and [`crate::ast::Ty`]
/// variants alike, so the two cannot come apart — how it is spelled, what the
/// editor plugin calls its token, and the letter that writes it in a format
/// string. The doc comment above a row is attached to both variants it
/// generates, which is what makes this the place to describe the type as well
/// as to declare it.
///
/// Adding a row is the whole of adding a type's *notation*. What it then costs
/// is three `match`es that stop compiling — see the module docs.
macro_rules! primitives {
    ($callback:ident) => {
        $callback! {
            /// A 64-bit signed integer. Arithmetic on one stops the program
            /// rather than wrapping — see [`crate::ast::BinOp::apply`].
            Int, "int", "INT_KW", 'd';

            /// An IEEE-754 double.
            ///
            /// It is a machine word like everything else here, and that is the
            /// whole of how a float is carried: what the word *holds* is the
            /// double's bits, and only the instructions that do arithmetic on
            /// it or write it out ever have to know. See [`crate::ir::Num`],
            /// where that is said once.
            ///
            /// No arithmetic mixes it with an `int` — `float(n)` and `int(f)`
            /// are written out, for the same reason `int(c)` is.
            Float, "float", "FLOAT_KW", 'f';

            /// One Unicode scalar value: what a string is made of, and what
            /// indexing one produces.
            ///
            /// A separate type rather than a small `int`, so that `+` cannot be
            /// applied to it by accident and `print` knows to write a character
            /// rather than a number. Going between the two is spelled out.
            Char, "char", "CHAR_KW", 'c';

            /// A run of characters, held as the address of the first one.
            ///
            /// The characters are four bytes each and the count sits in the
            /// eight bytes *before* them, so a string knows its own length and
            /// `len` is a load. A literal is laid out the same way in `.data`
            /// as a built one is in the arena, which is why nothing anywhere
            /// has to ask which kind it holds.
            Str, "string", "STRING_KW", 's';

            /// `true` or `false`.
            Bool, "bool", "BOOL_KW", 'b';
        }
    };
}

/// Declare [`Prim`] itself from the table.
macro_rules! declare_prim {
    ($($(#[$meta:meta])* $variant:ident, $spelling:literal, $editor:literal, $letter:literal;)*) => {
        /// **The types the language spells with a word of its own.**
        ///
        /// One enum, because it is one fact asked in five places: which words
        /// may start a declaration, which name a type after a `:` or a `->`,
        /// which may be written where a value is expected — `int(c)` is the
        /// code point of a character — which names [`crate::sema`] resolves
        /// without looking anything up, and which letters a format string
        /// takes. A word that is not here is an identifier, and an identifier
        /// followed by `(` is a call.
        ///
        /// Generated from [`primitives!`]; see that macro to add one.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Prim {
            $($(#[$meta])* $variant,)*
        }

        impl Prim {
            /// Every one of them, in the order a diagnostic reads them out.
            ///
            /// Generated with the enum, so it cannot fall behind it — which is
            /// what a hand-written `ALL` beside a hand-written enum can always
            /// do, and what a test used to have to watch for.
            pub const ALL: [Prim; [$(Prim::$variant),*].len()] = [$(Prim::$variant),*];

            /// How it is spelled, which is its name as a type, its name as a
            /// conversion, and the keyword that writes both.
            pub fn name(self) -> &'static str {
                match self {
                    $(Prim::$variant => $spelling,)*
                }
            }

            /// What the editor plugin calls its token. A Kotlin field name, so
            /// it is written out rather than derived from the spelling.
            pub fn token_name(self) -> &'static str {
                match self {
                    $(Prim::$variant => $editor,)*
                }
            }

            /// The letter that writes it in a format string.
            ///
            /// The one thing about a type that is not derivable from its
            /// spelling: `%d` writes an `int` and `%s` a `string`, because
            /// those are the letters C taught everyone.
            pub fn letter(self) -> char {
                match self {
                    $(Prim::$variant => $letter,)*
                }
            }
        }
    };
}

primitives!(declare_prim);

// Scoped to the crate rather than exported: this is how the compiler is put
// together, not part of what it offers.
pub(crate) use primitives;

impl Prim {
    /// The same question asked of a name rather than a token, which is what
    /// resolving a written type comes down to once enums and classes have been
    /// ruled out.
    pub fn of_name(name: &str) -> Option<Prim> {
        Prim::ALL.into_iter().find(|prim| prim.name() == name)
    }

    /// The name with its indefinite article, for prose in a diagnostic.
    pub fn with_article(self) -> String {
        let name = self.name();
        let article = match name.starts_with(['a', 'e', 'i', 'o', 'u']) {
            true => "an",
            false => "a",
        };
        format!("{article} {name}")
    }

    /// Them all, quoted and joined, for a diagnostic that has to say what a
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The table is a set, not a list: two rows sharing a spelling would make
    /// "expected a type" ambiguous, and two sharing a letter would make one of
    /// them unreachable from any format string.
    #[test]
    fn no_two_primitives_share_a_spelling_or_a_letter() {
        for (at, prim) in Prim::ALL.iter().enumerate() {
            for other in &Prim::ALL[at + 1..] {
                assert_ne!(prim.name(), other.name(), "{prim:?} and {other:?}");
                assert_ne!(prim.letter(), other.letter(), "{prim:?} and {other:?}");
                assert_ne!(prim.token_name(), other.token_name(), "{prim:?} and {other:?}");
            }
        }
    }

    /// A name reaches its type and back. This is what lets `sema` resolve a
    /// written type with a lookup instead of a list of its own.
    #[test]
    fn a_name_reaches_its_primitive() {
        for prim in Prim::ALL {
            assert_eq!(Prim::of_name(prim.name()), Some(prim));
        }
        assert_eq!(Prim::of_name("Colour"), None);
        assert_eq!(Prim::of_name(""), None);
    }

    /// The prose a diagnostic reads out is the table, so it cannot come to name
    /// four types out of five.
    #[test]
    fn the_list_a_diagnostic_reads_out_is_the_table() {
        let quoted = Prim::all_quoted();
        for prim in Prim::ALL {
            assert!(quoted.contains(&format!("`{}`", prim.name())), "{quoted} omits {prim:?}");
        }
        assert_eq!(quoted.matches('`').count(), Prim::ALL.len() * 2);
    }

    #[test]
    fn an_article_follows_the_spelling() {
        assert_eq!(Prim::Int.with_article(), "an int");
        assert_eq!(Prim::Float.with_article(), "a float");
    }
}
