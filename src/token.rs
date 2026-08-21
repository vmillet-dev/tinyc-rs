//! The token vocabulary produced by the lexer.

use crate::diag::Span;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenKind {
    // Literals and names.
    Int(i64),
    /// Decoded bytes of a string literal, without the surrounding quotes and
    /// without a NUL terminator.
    Str(Vec<u8>),
    Bool(bool),
    Ident(String),

    // Keywords.
    KwInt,
    KwString,
    KwBool,
    KwPrint,
    KwIf,
    KwElse,
    KwWhile,
    KwFor,
    KwFn,
    KwReturn,
    KwBreak,
    KwContinue,

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
    pub fn text(&self) -> &'static str {
        match self {
            // Keywords.
            TokenKind::KwInt => "int",
            TokenKind::KwString => "string",
            TokenKind::KwBool => "bool",
            TokenKind::KwPrint => "print",
            TokenKind::KwIf => "if",
            TokenKind::KwElse => "else",
            TokenKind::KwWhile => "while",
            TokenKind::KwFor => "for",
            TokenKind::KwFn => "fn",
            TokenKind::KwReturn => "return",
            TokenKind::KwBreak => "break",
            TokenKind::KwContinue => "continue",

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
            TokenKind::Bool(_) => "boolean literal",
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
