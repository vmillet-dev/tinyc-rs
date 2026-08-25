//! The vocabulary of the language, described for something that is not this
//! compiler.
//!
//! An editor cannot run `tinyc` on every keystroke, so it has to know the words
//! and the punctuation for itself — and a second copy of them is a second place
//! to forget. Everything an editor needs is therefore said *here*, once, and
//! written out to `grammar/vocabulary.txt` for the IntelliJ plugin's token
//! table to be generated from.
//!
//! # What this module owns, and what it does not
//!
//! [`SPELLED`] lists every token that is written one way, and [`Role`] says
//! what each is *for* — which is the question an editor colours by and one the
//! compiler never asks. Both belong here rather than in [`crate::token`]: a
//! parser matches on a [`TokenKind`] and does not care that `if` is a keyword
//! and `int` a type name, and `"IF_KW"` is a Kotlin field name that has no
//! business in the compiler's own types.
//!
//! **No spelling is written here.** A row names a [`TokenKind`], and how it is
//! spelled comes from [`TokenKind::text`], so the two cannot drift: the table
//! says `if` is Control, and `token.rs` says Control-what-exactly is spelled
//! `if`. The same holds for the built-ins ([`ast::Builtin::ALL`]) and the
//! format specifiers ([`ast::SPECS`]), which are read where they are declared.
//!
//! What a table cannot promise is that nothing is *missing* from it, since Rust
//! cannot be asked what an enum's variants are. That is a test's job —
//! [`tests::every_spelled_token_lexes_back_to_itself`] — and it is the same
//! bargain `token.rs` already made when its tests kept a written-out list of
//! every token with a spelling. This is that list, given roles and a use.
//!
//! # The format
//!
//! One record per line, fields separated by tabs, `#` starting a comment.
//! Deliberately not JSON: this compiler has one dependency and the plugin's
//! build should need none, and a reader for this is five lines on either side.
//!
//! ```text
//! format   1
//! role     <name>    word | symbol
//! token    <name>    <spelling>              <role>
//! valued   <name>    <what a diagnostic calls it>
//! builtin  <name>    <parameters, by comma>  <return type>
//! spec     <letter>  <what it writes>
//! ```

use crate::ast::{Builtin, Prim, SPECS};
use crate::token::{StrLit, TokenKind};

/// Where the export lives, relative to the repository root.
pub const PATH: &str = "grammar/vocabulary.txt";

/// How to produce it again, quoted in the message when it is out of date.
pub const REGENERATE: &str = "cargo run --bin export-vocabulary";

/// The version of the layout above, so that a plugin built against an older one
/// says so rather than reading the columns in the wrong order.
const FORMAT: u32 = 1;

/// What a token is *for*, which is what an editor colours and completes by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Names a type, which in TinyC is also what spells a conversion.
    Type,
    /// Shapes the program: `fn`, `if`, `match`, `class`.
    Control,
    /// A construct rather than a function, because no signature describes it —
    /// `len` takes several unrelated types, `push` takes a place.
    Construct,
    /// A word that is a value: `true` and `false`.
    Literal,
    /// Separates rather than computes: `(`, `;`, `,`, `::`.
    Punctuation,
    /// Computes, or points: `+`, `==`, `&&`, `->`, `=>`.
    Operator,
}

impl Role {
    /// Every role, since Rust cannot be asked what an enum's variants are.
    /// Leaving one out is caught by [`tests::every_role_is_in_role_all`].
    pub const ALL: [Role; 6] = [
        Role::Type,
        Role::Control,
        Role::Construct,
        Role::Literal,
        Role::Punctuation,
        Role::Operator,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Role::Type => "type",
            Role::Control => "control",
            Role::Construct => "construct",
            Role::Literal => "literal",
            Role::Punctuation => "punctuation",
            Role::Operator => "operator",
        }
    }

    /// Whether tokens in this role are spelled as words rather than symbols,
    /// which is the same question as "is this a name no program may take".
    ///
    /// A `match` and not a `matches!`, so that adding a [`Role`] does not
    /// compile until it is decided: a role missing from a `matches!` would be
    /// silently false, and a new keyword would lex as an identifier.
    pub fn is_word(self) -> bool {
        match self {
            Role::Type | Role::Control | Role::Construct | Role::Literal => true,
            Role::Punctuation | Role::Operator => false,
        }
    }
}

/// A token that is written exactly one way, and what an editor should make of it.
pub struct Spelled {
    /// What the editor plugin calls this token. It is a Kotlin field name — the
    /// plugin's table is generated from it — so renaming one renames both.
    pub name: &'static str,
    pub kind: TokenKind,
    pub role: Role,
    /// How it is written, for the tokens whose kind cannot say. See
    /// [`Spelled::literal`]; everything else leaves this `None`.
    written: Option<&'static str>,
}

impl Spelled {
    /// A token whose kind knows how it is written.
    const fn new(name: &'static str, kind: TokenKind, role: Role) -> Spelled {
        Spelled { name, kind, role, written: None }
    }

    /// A token that carries a value and is *still* written exactly one way.
    ///
    /// `true` and `false` are the whole of this, and they need it because
    /// [`TokenKind::text`] answers "boolean literal" for a [`TokenKind::Bool`]
    /// — which is right for "expected a boolean literal" and useless for
    /// lexing. So the two spellings are written here, and nowhere else in the
    /// compiler.
    const fn literal(name: &'static str, kind: TokenKind, written: &'static str) -> Spelled {
        Spelled { name, kind, role: Role::Literal, written: Some(written) }
    }

    /// How it is written: the kind's own spelling unless it has none.
    pub fn text(&self) -> &'static str {
        match self.written {
            Some(written) => written,
            None => self.kind.text(),
        }
    }
}

/// Every token with a fixed spelling, and what it is for.
///
/// The spelling is deliberately absent: it is [`TokenKind::text`]'s to give.
/// `true` and `false` are here despite being [`TokenKind::Bool`], because the
/// question this answers is which *words* a program may not take as a name.
pub static SPELLED: &[Spelled] = &[
    Spelled::new("INT_KW", TokenKind::KwInt, Role::Type),
    Spelled::new("STRING_KW", TokenKind::KwString, Role::Type),
    Spelled::new("CHAR_KW", TokenKind::KwChar, Role::Type),
    Spelled::new("BOOL_KW", TokenKind::KwBool, Role::Type),
    Spelled::new("IF_KW", TokenKind::KwIf, Role::Control),
    Spelled::new("ELSE_KW", TokenKind::KwElse, Role::Control),
    Spelled::new("WHILE_KW", TokenKind::KwWhile, Role::Control),
    Spelled::new("FOR_KW", TokenKind::KwFor, Role::Control),
    Spelled::new("FN_KW", TokenKind::KwFn, Role::Control),
    Spelled::new("RETURN_KW", TokenKind::KwReturn, Role::Control),
    Spelled::new("BREAK_KW", TokenKind::KwBreak, Role::Control),
    Spelled::new("CONTINUE_KW", TokenKind::KwContinue, Role::Control),
    Spelled::new("ENUM_KW", TokenKind::KwEnum, Role::Control),
    Spelled::new("MATCH_KW", TokenKind::KwMatch, Role::Control),
    Spelled::new("CLASS_KW", TokenKind::KwClass, Role::Control),
    Spelled::new("PRINT_KW", TokenKind::KwPrint, Role::Construct),
    Spelled::new("PRINTLN_KW", TokenKind::KwPrintln, Role::Construct),
    Spelled::new("LEN_KW", TokenKind::KwLen, Role::Construct),
    Spelled::new("PUSH_KW", TokenKind::KwPush, Role::Construct),
    Spelled::literal("TRUE_KW", TokenKind::Bool(true), "true"),
    Spelled::literal("FALSE_KW", TokenKind::Bool(false), "false"),
    Spelled::new("LPAREN", TokenKind::LParen, Role::Punctuation),
    Spelled::new("RPAREN", TokenKind::RParen, Role::Punctuation),
    Spelled::new("LBRACE", TokenKind::LBrace, Role::Punctuation),
    Spelled::new("RBRACE", TokenKind::RBrace, Role::Punctuation),
    Spelled::new("LBRACKET", TokenKind::LBracket, Role::Punctuation),
    Spelled::new("RBRACKET", TokenKind::RBracket, Role::Punctuation),
    Spelled::new("SEMI", TokenKind::Semi, Role::Punctuation),
    Spelled::new("COMMA", TokenKind::Comma, Role::Punctuation),
    Spelled::new("DOT", TokenKind::Dot, Role::Punctuation),
    Spelled::new("COLON", TokenKind::Colon, Role::Punctuation),
    Spelled::new("COLON_COLON", TokenKind::ColonColon, Role::Punctuation),
    Spelled::new("ARROW", TokenKind::Arrow, Role::Operator),
    Spelled::new("FAT_ARROW", TokenKind::FatArrow, Role::Operator),
    Spelled::new("EQ", TokenKind::Eq, Role::Operator),
    Spelled::new("PLUS", TokenKind::Plus, Role::Operator),
    Spelled::new("MINUS", TokenKind::Minus, Role::Operator),
    Spelled::new("STAR", TokenKind::Star, Role::Operator),
    Spelled::new("SLASH", TokenKind::Slash, Role::Operator),
    Spelled::new("PERCENT", TokenKind::Percent, Role::Operator),
    Spelled::new("BANG", TokenKind::Bang, Role::Operator),
    Spelled::new("EQ_EQ", TokenKind::EqEq, Role::Operator),
    Spelled::new("BANG_EQ", TokenKind::BangEq, Role::Operator),
    Spelled::new("LT", TokenKind::Lt, Role::Operator),
    Spelled::new("LE", TokenKind::Le, Role::Operator),
    Spelled::new("GT", TokenKind::Gt, Role::Operator),
    Spelled::new("GE", TokenKind::Ge, Role::Operator),
    Spelled::new("AND_AND", TokenKind::AmpAmp, Role::Operator),
    Spelled::new("OR_OR", TokenKind::PipePipe, Role::Operator),
];

/// The token a word spells, if it spells one rather than being a name.
///
/// The lexer asks this instead of matching on the word itself, which is what
/// makes [`SPELLED`] the definition of the keywords rather than a description
/// of them kept alongside — a word left out is not a keyword the editor forgot,
/// it is not a keyword at all.
pub fn keyword(word: &str) -> Option<TokenKind> {
    SPELLED
        .iter()
        .find(|spelled| spelled.role.is_word() && spelled.text() == word)
        .map(|spelled| spelled.kind.clone())
}

/// The tokens spelled every way rather than one, and what the editor calls them.
///
/// The value each carries is a placeholder: what is read off it is *which*
/// variant it is, never what is in it. The noun comes from [`TokenKind::text`]
/// like every other piece of text here.
///
/// A comment is deliberately absent: the lexer skips one as trivia and makes no
/// token, so it is the editor's business and not the language's.
fn valued() -> [(&'static str, TokenKind); 4] {
    [
        ("INT_LITERAL", TokenKind::Int(0)),
        ("STRING_LITERAL", TokenKind::Str(StrLit::default())),
        ("CHAR_LITERAL", TokenKind::Char(' ')),
        ("IDENTIFIER", TokenKind::Ident(String::new())),
    ]
}

/// The whole vocabulary, as the text of `grammar/vocabulary.txt`.
///
/// Every loop here walks a table that lives beside the thing it describes, so
/// growing the language means adding a row where the language is defined and
/// never opening this function.
pub fn export() -> String {
    let mut out = String::new();

    out.push_str("# The vocabulary of TinyC, exported from the compiler that defines it.\n");
    out.push_str("#\n");
    out.push_str(&format!("# Generated by `{REGENERATE}` — do not edit. What this is\n"));
    out.push_str("# a copy *of* lives in `src/vocabulary.rs` and `src/ast.rs`; what it is a\n");
    out.push_str("# copy *for* is `ide/intellij`, whose token table is generated from it.\n");
    out.push_str("#\n");
    out.push_str("#   role     <name>    word | symbol\n");
    out.push_str("#   token    <name>    <spelling>              <role>\n");
    out.push_str("#   valued   <name>    <what a diagnostic calls it>\n");
    out.push_str("#   builtin  <name>    <parameters, by comma>  <return type>\n");
    out.push_str("#   spec     <letter>  <what it writes>\n");
    out.push_str(&format!("\nformat\t{FORMAT}\n"));

    out.push_str("\n# What a token is for. A `word` is a name no program may take; a\n");
    out.push_str("# `symbol` is punctuation. Whoever reads this makes a set per role\n");
    out.push_str("# rather than knowing the roles, so a new one needs nothing said.\n");
    for role in Role::ALL {
        let shape = if role.is_word() { "word" } else { "symbol" };
        out.push_str(&format!("role\t{}\t{shape}\n", role.name()));
    }

    out.push_str("\n# Every token spelled exactly one way.\n");
    for spelled in SPELLED {
        out.push_str(&format!(
            "token\t{}\t{}\t{}\n",
            spelled.name,
            spelled.text(),
            spelled.role.name()
        ));
    }

    out.push_str("\n# The tokens that carry a value, and so are spelled every way rather\n");
    out.push_str("# than one. An editor still needs to tell them apart to colour them.\n");
    for (name, kind) in valued() {
        out.push_str(&format!("valued\t{name}\t{}\n", kind.text()));
    }

    out.push_str("\n# Names already in the signature table when the first line is checked.\n");
    // A built-in states its signature in `Prim`s, so there is no type table to
    // hand in and no shape this can fail to write. See `ast::Builtin::params`.
    for builtin in Builtin::ALL {
        let params: Vec<&str> = builtin.params().iter().map(|prim| prim.name()).collect();
        let ret = builtin.ret().map(Prim::name).unwrap_or_default();
        out.push_str(&format!("builtin\t{}\t{}\t{}\n", builtin.name(), params.join(","), ret));
    }

    out.push_str("\n# What a `%` in a format string may be followed by.\n");
    for spec in SPECS {
        out.push_str(&format!("spec\t{}\t{}\n", spec.letter(), spec.writes()));
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The checked-in file is what this run of the compiler would write.
    ///
    /// The plugin reads the file rather than running `tinyc`, so that its build
    /// needs neither Rust nor this repository — which leaves exactly one way
    /// for the two to drift, and this is it.
    #[test]
    fn the_checked_in_export_is_what_the_compiler_would_write() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/grammar/vocabulary.txt");
        let on_disk = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("{PATH} cannot be read ({e}); run `{REGENERATE}`"));

        // Compared line by line, and without the carriage returns a Windows
        // checkout may have added: what is being checked is the vocabulary, not
        // how git wrote it down.
        let written = export();
        let mut expected = written.lines();
        for (number, actual) in on_disk.lines().enumerate() {
            assert_eq!(
                Some(actual.trim_end_matches('\r')),
                expected.next(),
                "{PATH} line {} is stale; run `{REGENERATE}`",
                number + 1
            );
        }
        assert_eq!(expected.next(), None, "{PATH} is missing lines; run `{REGENERATE}`");
    }

    /// Every token in [`SPELLED`] lexes back to the token it names.
    ///
    /// This is what holds the table to the language rather than to an opinion
    /// about it: the spelling is `TokenKind::text`'s, and the lexer has to agree
    /// that writing it produces that very token. A row naming the wrong variant
    /// fails here, not in an editor three weeks later.
    #[test]
    fn every_spelled_token_lexes_back_to_itself() {
        for spelled in SPELLED {
            let text = spelled.text();
            let lexed = crate::lexer::lex(text)
                .unwrap_or_else(|e| panic!("`{text}` should lex: {e:?}"));
            assert_eq!(
                lexed.first().map(|t| &t.kind),
                Some(&spelled.kind),
                "`{text}` does not lex back to {:?}",
                spelled.kind
            );
            assert_eq!(lexed.len(), 2, "`{text}` should be one token and the end of the file");
        }
    }

    /// No token is left out of [`SPELLED`].
    ///
    /// Rust cannot be asked what an enum's variants are, so this asks the
    /// question the other way round: `TokenKind::text` answers with a spelling
    /// for exactly the tokens that have one, and with a generic noun for the
    /// rest — so lexing every spelling in the table must account for every
    /// token the lexer can produce from punctuation and words. What that cannot
    /// see, a count can: the table is as long as the language is wide.
    #[test]
    fn the_table_covers_every_token_that_has_a_spelling() {
        // Adding a `TokenKind` with a fixed spelling means adding a row here
        // *and* bumping this number, which is the moment to notice the row.
        assert_eq!(
            SPELLED.len(),
            49,
            "a token was added or removed; the table and this count both move"
        );
        for (_, kind) in valued() {
            assert!(
                SPELLED.iter().all(|spelled| spelled.kind != kind),
                "{kind:?} carries a value and does not belong in SPELLED"
            );
        }
    }

    /// A written-out spelling is only ever used where the kind has none.
    ///
    /// Otherwise `Spelled::literal` would be a way to quietly disagree with
    /// [`TokenKind::text`] — the one thing this table exists not to do. A kind
    /// that really has no spelling of its own does not lex back to itself from
    /// the noun it answers with, and that is what is checked.
    #[test]
    fn a_spelling_is_written_out_only_where_the_kind_has_none() {
        for spelled in SPELLED.iter().filter(|s| s.written.is_some()) {
            let noun = spelled.kind.text();
            let lexed = crate::lexer::lex(noun).ok().and_then(|t| t.first().map(|t| t.kind.clone()));
            assert_ne!(
                lexed.as_ref(),
                Some(&spelled.kind),
                "{} spells itself; it does not need writing out",
                spelled.name
            );
        }
    }

    #[test]
    fn no_two_tokens_are_spelled_the_same_way() {
        // Two sharing a spelling would make "expected `X`" ambiguous, and would
        // mean the lexer has to be guessing somewhere.
        for (at, spelled) in SPELLED.iter().enumerate() {
            let clash = SPELLED[at + 1..].iter().find(|other| other.text() == spelled.text());
            assert!(
                clash.is_none(),
                "{} and {} are both `{}`",
                spelled.name,
                clash.map_or("", |other| other.name),
                spelled.text()
            );
        }
    }

    /// The names are what the plugin's token table is generated from, so two
    /// the same would be one Kotlin field standing for two tokens.
    #[test]
    fn no_two_tokens_share_a_name() {
        let all: Vec<&str> = SPELLED
            .iter()
            .map(|spelled| spelled.name)
            .chain(valued().iter().map(|(name, _)| *name))
            .collect();
        for (at, name) in all.iter().enumerate() {
            assert!(!all[at + 1..].contains(name), "two tokens are called `{name}`");
        }
    }

    /// [`Role::ALL`] really is all of them: the match below is exhaustive, so
    /// adding a [`Role`] stops this file compiling until the new one is named.
    #[test]
    fn every_role_is_in_role_all() {
        for role in Role::ALL {
            // Nothing is asserted about the arms; rustc refusing this until
            // every variant has one is the whole of it.
            match role {
                Role::Type
                | Role::Control
                | Role::Construct
                | Role::Literal
                | Role::Punctuation
                | Role::Operator => {}
            }
        }
        // No count is asserted: `[Role; N]` already refuses a variant that was
        // not listed. What it cannot catch is one listed twice.
        let names: Vec<&str> = Role::ALL.iter().map(|role| role.name()).collect();
        for (at, name) in names.iter().enumerate() {
            assert!(!names[at + 1..].contains(name), "two roles are called `{name}`");
        }
    }

    /// A role nothing is written in reaches the editor as an empty set. That is
    /// harmless, and still worth knowing: it almost always means a token was
    /// meant to have it and does not.
    #[test]
    fn no_role_is_declared_that_nothing_uses() {
        for role in Role::ALL {
            assert!(
                SPELLED.iter().any(|spelled| spelled.role == role),
                "no token has the role `{}`",
                role.name()
            );
        }
    }

    /// A word in the table is a word a program cannot use as a name, which is
    /// the property the plugin colours by.
    #[test]
    fn every_word_in_the_table_is_a_word_no_program_can_name() {
        for spelled in SPELLED.iter().filter(|s| s.role.is_word()) {
            assert_eq!(
                keyword(spelled.text()),
                Some(spelled.kind.clone()),
                "`{}` is in the table but does not lex as itself",
                spelled.text()
            );
        }
        assert_eq!(keyword("total"), None);
        // Punctuation is in the same table and is not a word.
        assert_eq!(keyword("::"), None);
    }

    #[test]
    fn a_token_with_a_spelling_is_described_by_quoting_it() {
        for spelled in SPELLED {
            assert_eq!(
                spelled.kind.describe(),
                format!("`{}`", spelled.text()),
                "{}",
                spelled.name
            );
        }
    }

    /// Nothing in the export may contain a tab or a newline, since those are
    /// what separate one field and one record from the next.
    #[test]
    fn no_field_can_be_mistaken_for_the_separator() {
        for spelled in SPELLED {
            assert!(!spelled.text().contains(['\t', '\n']), "{} is unwritable", spelled.name);
            assert!(!spelled.name.contains(['\t', '\n']), "{} is unwritable", spelled.name);
        }
    }
}
