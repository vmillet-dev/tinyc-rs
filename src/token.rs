//! The token vocabulary produced by the lexer.

use crate::diag::Span;

/// The characters of a string literal, and where each one was written.
///
/// The characters are what the literal *means*: the surrounding quotes are
/// gone, every escape is resolved, and the source's UTF-8 has been decoded, so
/// `"h\u{e9}llo"` is five characters here and everywhere after. Nothing
/// downstream ever counts the bytes it took to write one.
///
/// The offsets are what an ordinary literal never needs and a **format string**
/// cannot do without. A diagnostic about the `%d` in `"n = %d"` has to point at
/// those two characters, and by then an escape earlier in the literal may
/// already have turned two characters of source into one — so the character
/// count is no longer the distance from the opening quote. Keeping each
/// character's own offset is what lets [`Self::span`] cut a span for any run of
/// them; the extra entry for the closing quote is what lets it cut an empty one
/// at the end.
#[derive(Clone, Debug, Default)]
pub struct StrLit {
    pub chars: Vec<char>,
    /// One entry per character, then one for the closing quote.
    pub offsets: Vec<u32>,
}

impl StrLit {
    pub fn push(&mut self, c: char, offset: usize) {
        self.chars.push(c);
        self.offsets.push(offset as u32);
    }

    /// Note where the closing quote is, which is what gives a span an end when
    /// it runs to the last character.
    pub fn close(&mut self, offset: usize) {
        self.offsets.push(offset as u32);
    }

    /// The span of the characters `from..to`, in the source they were written
    /// in rather than in the text they decoded to.
    pub fn span(&self, from: usize, to: usize) -> Span {
        let start = self.offsets[from];
        let end = self.offsets[to];
        Span::new(start as usize, (end - start) as usize)
    }
}

/// Two literals are equal when they say the same thing.
///
/// *Where* one was written is not part of what it is, and comparing tokens for
/// equality is something only the lexer's own tests do — so the offsets, which
/// differ for every copy of the same literal, are deliberately left out.
impl PartialEq for StrLit {
    fn eq(&self, other: &StrLit) -> bool {
        self.chars == other.chars
    }
}

impl Eq for StrLit {}

impl From<&str> for StrLit {
    fn from(text: &str) -> StrLit {
        let chars: Vec<char> = text.chars().collect();
        // No source to point into: a literal built from a Rust string was not
        // written in a TinyC file. Every offset is zero, and so every span cut
        // from it is empty — which is what a test that never renders one wants.
        let offsets = vec![0; chars.len() + 1];
        StrLit { chars, offsets }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TokenKind {
    // Literals and names.
    Int(i64),
    /// A string literal: the characters it stands for, and where each was
    /// written. See [`StrLit`].
    Str(StrLit),
    /// A character literal, `'a'` — exactly one character.
    Char(char),
    Float(f64),
    Bool(bool),
    Ident(String),

    // Keywords.
    KwInt,
    KwString,
    KwChar,
    KwBool,
    KwFloat,
    KwPrint,
    KwPrintln,
    KwIf,
    KwElse,
    KwWhile,
    KwFor,
    KwFn,
    KwReturn,
    KwBreak,
    KwContinue,
    KwEnum,
    KwMatch,
    KwLen,
    KwPush,
    KwClass,

    // Punctuation.
    LParen,
    RParen,
    LBrace,
    RBrace,
    Semi,
    Eq,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Comma,
    Arrow,
    Bang,
    /// `=>`, between a `match` arm's pattern and its block.
    FatArrow,
    /// `::`, which qualifies a variant with the enum it belongs to.
    ColonColon,
    /// `:`, which introduces a base class and separates a field from its value.
    Colon,
    /// `.`, which reaches into an object.
    Dot,
    LBracket,
    RBracket,

    // Comparison.
    EqEq,
    BangEq,
    Lt,
    Le,
    Gt,
    Ge,

    // Logic
    AmpAmp,
    PipePipe,

    /// Synthetic token at the end of the file; simplifies the parser.
    Eof,
}

impl TokenKind {
    /// How this token is referred to in diagnostics.
    pub fn describe(&self) -> String {
        match self {
            TokenKind::Int(v) => format!("`{v}`"),
            TokenKind::Str(_) => "string literal".to_string(),
            TokenKind::Char(c) => format!("`'{c}'`"),
            TokenKind::Bool(v) => format!("`{v}`"),
            TokenKind::Ident(name) => format!("`{name}`"),
            TokenKind::Eof => "end of file".to_string(),
            other => format!("`{}`", other.text()),
        }
    }

    /// How this token is spelled.
    ///
    /// A keyword or a punctuation mark has exactly one spelling, which is what
    /// the parser puts in "expected `;`". The variants that carry a value have
    /// none, so they answer with a generic noun; [`Self::describe`] is what
    /// names the actual value instead.
    ///
    /// This is the only place a spelling is written. `crate::vocabulary`, which
    /// tells an editor about them, reads them back out of here rather than
    /// keeping a copy.
    pub fn text(&self) -> &'static str {
        match self {
            // Keywords.
            TokenKind::KwInt => "int",
            TokenKind::KwString => "string",
            TokenKind::KwChar => "char",
            TokenKind::KwBool => "bool",
            TokenKind::KwFloat => "float",
            TokenKind::KwPrint => "print",
            TokenKind::KwPrintln => "println",
            TokenKind::KwIf => "if",
            TokenKind::KwElse => "else",
            TokenKind::KwWhile => "while",
            TokenKind::KwFor => "for",
            TokenKind::KwFn => "fn",
            TokenKind::KwReturn => "return",
            TokenKind::KwBreak => "break",
            TokenKind::KwContinue => "continue",
            TokenKind::KwEnum => "enum",
            TokenKind::KwMatch => "match",
            TokenKind::KwLen => "len",
            TokenKind::KwPush => "push",
            TokenKind::KwClass => "class",

            // Punctuation.
            TokenKind::LParen => "(",
            TokenKind::RParen => ")",
            TokenKind::LBrace => "{",
            TokenKind::RBrace => "}",
            TokenKind::Semi => ";",
            TokenKind::Eq => "=",
            TokenKind::Plus => "+",
            TokenKind::Minus => "-",
            TokenKind::Star => "*",
            TokenKind::Slash => "/",
            TokenKind::Percent => "%",
            TokenKind::Comma => ",",
            TokenKind::Arrow => "->",
            TokenKind::Bang => "!",
            TokenKind::FatArrow => "=>",
            TokenKind::ColonColon => "::",
            TokenKind::Colon => ":",
            TokenKind::Dot => ".",
            TokenKind::LBracket => "[",
            TokenKind::RBracket => "]",

            // Comparison.
            TokenKind::EqEq => "==",
            TokenKind::BangEq => "!=",
            TokenKind::Lt => "<",
            TokenKind::Le => "<=",
            TokenKind::Gt => ">",
            TokenKind::Ge => ">=",

            // Logic.
            TokenKind::AmpAmp => "&&",
            TokenKind::PipePipe => "||",

            // No fixed spelling: a generic noun, not a value.
            TokenKind::Int(_) => "integer literal",
            TokenKind::Str(_) => "string literal",
            TokenKind::Char(_) => "character literal",
            TokenKind::Bool(_) => "boolean literal",
            TokenKind::Float(_) => "float literal",
            TokenKind::Ident(_) => "identifier",
            TokenKind::Eof => "end of file",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

#[cfg(test)]
mod tests {
    use super::*;

    // The tests that need *every* token with a fixed spelling live in
    // `crate::vocabulary`, which is where the list of them is. Keeping a second
    // list here to test against would be the drift this project exists to avoid.

    /// A token that carries a value has no spelling of its own, so it answers
    /// with a generic noun — and [`TokenKind::describe`] is what names the value
    /// instead.
    #[test]
    fn a_token_that_carries_a_value_is_described_by_the_value() {
        assert_eq!(TokenKind::Int(42).describe(), "`42`");
        assert_eq!(TokenKind::Int(42).text(), "integer literal");

        assert_eq!(TokenKind::Char('x').describe(), "`'x'`");
        assert_eq!(TokenKind::Char('x').text(), "character literal");

        assert_eq!(TokenKind::Bool(true).describe(), "`true`");
        assert_eq!(TokenKind::Bool(false).describe(), "`false`");
        assert_eq!(TokenKind::Bool(true).text(), "boolean literal");

        assert_eq!(TokenKind::Ident("total".to_string()).describe(), "`total`");
        assert_eq!(TokenKind::Ident("total".to_string()).text(), "identifier");

        // A string's contents could be any length and are not worth quoting
        // back at the reader, so this is the one that stays generic in both.
        assert_eq!(TokenKind::Str(StrLit::from("hi")).describe(), "string literal");
        assert_eq!(TokenKind::Str(StrLit::from("hi")).text(), "string literal");
    }

    #[test]
    fn the_end_of_the_file_is_described_in_words_rather_than_quoted() {
        // There is nothing to quote, and "expected `;`, found ``" would be a
        // message about nothing.
        assert_eq!(TokenKind::Eof.describe(), "end of file");
        assert_eq!(TokenKind::Eof.text(), "end of file");
    }
}
