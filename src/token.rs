//! The token vocabulary produced by the lexer.

use crate::diag::Span;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenKind {
    // Literals and names.
    Int(i64),
    /// Decoded bytes of a string literal, without the surrounding quotes and
    /// without a NUL terminator.
    Str(Vec<u8>),
    Ident(String),

    // Keywords.
    KwInt,
    KwString,
    KwPrint,

    // Punctuation.
    LParen,
    RParen,
    Semi,
    Eq,
    Plus,
    Minus,
    Star,
    Slash,

    /// Synthetic token at the end of the file; simplifies the parser.
    Eof,
}

impl TokenKind {
    /// How this token is referred to in diagnostics.
    pub fn describe(&self) -> String {
        match self {
            TokenKind::Int(v) => format!("`{v}`"),
            TokenKind::Str(_) => "string literal".to_string(),
            TokenKind::Ident(name) => format!("`{name}`"),
            TokenKind::Eof => "end of file".to_string(),
            other => format!("`{}`", other.text()),
        }
    }

    /// The spelling of a keyword or punctuation token (a description for the
    /// variants that carry a value).
    pub fn text(&self) -> &'static str {
        match self {
            TokenKind::KwInt => "int",
            TokenKind::KwString => "string",
            TokenKind::KwPrint => "print",
            TokenKind::LParen => "(",
            TokenKind::RParen => ")",
            TokenKind::Semi => ";",
            TokenKind::Eq => "=",
            TokenKind::Plus => "+",
            TokenKind::Minus => "-",
            TokenKind::Star => "*",
            TokenKind::Slash => "/",
            TokenKind::Int(_) => "integer literal",
            TokenKind::Str(_) => "string literal",
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
