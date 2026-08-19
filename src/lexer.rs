//! Stage 1: source text -> tokens.
//!
//! The lexer walks `char_indices()` so byte offsets stay valid for UTF-8 input,
//! and stops at the first malformed token.

use crate::diag::{Diagnostic, Result, Span};
use crate::token::{Token, TokenKind};

pub fn lex(src: &str) -> Result<Vec<Token>> {
    Lexer::new(src).run().map_err(|d| vec![d])
}

struct Lexer<'a> {
    src: &'a str,
    /// `(byte offset, character)` for every character in the source.
    chars: Vec<(usize, char)>,
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a str) -> Lexer<'a> {
        Lexer { src, chars: src.char_indices().collect(), pos: 0 }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).map(|&(_, c)| c)
    }

    fn peek_next(&self) -> Option<char> {
        self.chars.get(self.pos + 1).map(|&(_, c)| c)
    }

    /// Byte offset of the current character (end of file offset when done).
    fn offset(&self) -> usize {
        self.chars.get(self.pos).map_or(self.src.len(), |&(o, _)| o)
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn run(mut self) -> std::result::Result<Vec<Token>, Diagnostic> {
        let mut tokens = Vec::new();
        loop {
            self.skip_trivia();
            let start = self.offset();
            let Some(c) = self.peek() else { break };

            let kind = match c {
                '(' => self.single(TokenKind::LParen),
                ')' => self.single(TokenKind::RParen),
                ';' => self.single(TokenKind::Semi),
                '=' => self.single(TokenKind::Eq),
                '+' => self.single(TokenKind::Plus),
                '-' => self.single(TokenKind::Minus),
                '*' => self.single(TokenKind::Star),
                '/' => self.single(TokenKind::Slash),
                '"' => self.string()?,
                c if c.is_ascii_digit() => self.number()?,
                c if is_ident_start(c) => self.ident(),
                c => {
                    self.bump();
                    return Err(Diagnostic::new(
                        format!("unexpected character `{c}`"),
                        Span::new(start, self.offset() - start),
                    ));
                }
            };
            tokens.push(Token { kind, span: Span::new(start, self.offset() - start) });
        }

        tokens.push(Token { kind: TokenKind::Eof, span: Span::new(self.src.len(), 0) });
        Ok(tokens)
    }

    fn single(&mut self, kind: TokenKind) -> TokenKind {
        self.bump();
        kind
    }

    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.bump();
                }
                // `//` runs to the end of the line.
                Some('/') if self.peek_next() == Some('/') => {
                    while let Some(c) = self.peek() {
                        if c == '\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                _ => return,
            }
        }
    }

    fn ident(&mut self) -> TokenKind {
        let start = self.offset();
        while self.peek().is_some_and(is_ident_continue) {
            self.bump();
        }
        match &self.src[start..self.offset()] {
            "int" => TokenKind::KwInt,
            "string" => TokenKind::KwString,
            "print" => TokenKind::KwPrint,
            name => TokenKind::Ident(name.to_string()),
        }
    }

    fn number(&mut self) -> std::result::Result<TokenKind, Diagnostic> {
        let start = self.offset();
        let mut value: i64 = 0;
        let mut overflowed = false;
        while let Some(c) = self.peek() {
            let Some(digit) = c.to_digit(10) else { break };
            self.bump();
            value = match value.checked_mul(10).and_then(|v| v.checked_add(digit as i64)) {
                Some(v) => v,
                None => {
                    overflowed = true;
                    0
                }
            };
        }

        // `123abc` is a malformed literal, not a literal followed by a name.
        if self.peek().is_some_and(is_ident_continue) {
            while self.peek().is_some_and(is_ident_continue) {
                self.bump();
            }
            return Err(Diagnostic::new(
                "invalid suffix on integer literal",
                Span::new(start, self.offset() - start),
            )
            .with_label("integer literals may only contain digits"));
        }

        if overflowed {
            return Err(Diagnostic::new(
                "integer literal is too large",
                Span::new(start, self.offset() - start),
            )
            .with_label(format!("`int` values must fit in {}..={}", i64::MIN, i64::MAX)));
        }
        Ok(TokenKind::Int(value))
    }

    fn string(&mut self) -> std::result::Result<TokenKind, Diagnostic> {
        let open = self.offset();
        self.bump(); // opening quote
        let mut bytes = Vec::new();
        loop {
            let escape_start = self.offset();
            match self.bump() {
                Some('"') => return Ok(TokenKind::Str(bytes)),
                Some('\\') => {
                    let escaped = match self.bump() {
                        Some('n') => b'\n',
                        Some('t') => b'\t',
                        Some('r') => b'\r',
                        Some('0') => 0,
                        Some('\\') => b'\\',
                        Some('"') => b'"',
                        Some(c) => {
                            return Err(Diagnostic::new(
                                format!("unknown escape sequence `\\{c}`"),
                                Span::new(escape_start, self.offset() - escape_start),
                            )
                            .with_note(SUPPORTED_ESCAPES, None));
                        }
                        None => break,
                    };
                    bytes.push(escaped);
                }
                // A string literal may not span lines.
                Some('\n') | None => break,
                Some(c) => {
                    let mut buf = [0u8; 4];
                    bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            }
        }
        Err(Diagnostic::new("unterminated string literal", Span::new(open, 1))
            .with_label("this quote has no match on the same line"))
    }
}

const SUPPORTED_ESCAPES: &str = r#"supported escapes are \n \t \r \0 \\ \""#;

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn is_ident_continue(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        lex(src).unwrap().into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn lexes_a_declaration() {
        assert_eq!(
            kinds("int x = 10;"),
            vec![
                TokenKind::KwInt,
                TokenKind::Ident("x".into()),
                TokenKind::Eq,
                TokenKind::Int(10),
                TokenKind::Semi,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn spans_cover_the_token() {
        let tokens = lex("int  x = 10;").unwrap();
        assert_eq!(tokens[1].span, Span::new(5, 1)); // x
        assert_eq!(tokens[3].span, Span::new(9, 2)); // 10
    }

    #[test]
    fn skips_line_comments() {
        assert_eq!(kinds("// hi\n;"), vec![TokenKind::Semi, TokenKind::Eof]);
    }

    #[test]
    fn decodes_escapes() {
        assert_eq!(kinds(r#""a\n\"b""#), vec![TokenKind::Str(b"a\n\"b".to_vec()), TokenKind::Eof]);
    }

    #[test]
    fn reports_unterminated_string_at_the_quote() {
        let errors = lex("string s = \"oops;\n").unwrap_err();
        assert_eq!(errors[0].span, Span::new(11, 1));
    }

    #[test]
    fn reports_integer_overflow() {
        let errors = lex("int x = 99999999999999999999;").unwrap_err();
        assert!(errors[0].message.contains("too large"));
    }
}
