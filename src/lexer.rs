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
                '{' => self.single(TokenKind::LBrace),
                '}' => self.single(TokenKind::RBrace),
                ';' => self.single(TokenKind::Semi),
                // Two-character operators are recognised before their prefixes.
                // `=` is the one with two of them, so it does not fit
                // [`Self::one_or_two`].
                '=' => {
                    self.bump();
                    match self.peek() {
                        Some('=') => self.single(TokenKind::EqEq),
                        Some('>') => self.single(TokenKind::FatArrow),
                        _ => TokenKind::Eq,
                    }
                }
                ':' => self.only_doubled(':', TokenKind::ColonColon, start)?,
                '<' => self.one_or_two('=', TokenKind::Le, TokenKind::Lt),
                '>' => self.one_or_two('=', TokenKind::Ge, TokenKind::Gt),
                '!' => self.one_or_two('=', TokenKind::BangEq, TokenKind::Bang),
                // These exist only doubled: TinyC has no bitwise `&` or `|`, and
                // nothing at all for a single `:`, for the lone character to
                // mean instead.
                '&' => self.only_doubled('&', TokenKind::AmpAmp, start)?,
                '|' => self.only_doubled('|', TokenKind::PipePipe, start)?,
                '+' => self.single(TokenKind::Plus),
                '-' => self.one_or_two('>', TokenKind::Arrow, TokenKind::Minus),
                '*' => self.single(TokenKind::Star),
                '/' => self.single(TokenKind::Slash),
                '%' => self.single(TokenKind::Percent),
                ',' => self.single(TokenKind::Comma),
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

    /// Consume a one-character token, or a two-character one when `second`
    /// follows: this is what keeps `<=` from lexing as `<` then `=`.
    fn one_or_two(&mut self, second: char, both: TokenKind, first: TokenKind) -> TokenKind {
        self.bump();
        if self.peek() == Some(second) {
            self.bump();
            both
        } else {
            first
        }
    }

    /// Consume a token that has no one-character form: `&&` and `||`.
    ///
    /// Unlike [`Self::one_or_two`] there is nothing to fall back to, so the
    /// character on its own is an error — and the only thing it can plausibly
    /// have meant is the doubled spelling, which the message says.
    fn only_doubled(
        &mut self,
        second: char,
        both: TokenKind,
        start: usize,
    ) -> std::result::Result<TokenKind, Diagnostic> {
        let first = self.bump().expect("the caller peeked this character");
        if self.peek() == Some(second) {
            self.bump();
            return Ok(both);
        }
        Err(Diagnostic::new(
            format!("unexpected character `{first}`"),
            Span::new(start, self.offset() - start),
        )
        .with_label(format!("did you mean `{}`?", both.text())))
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
            "bool" => TokenKind::KwBool,
            "print" => TokenKind::KwPrint,
            "if" => TokenKind::KwIf,
            "else" => TokenKind::KwElse,
            "while" => TokenKind::KwWhile,
            "for" => TokenKind::KwFor,
            "true" => TokenKind::Bool(true),
            "false" => TokenKind::Bool(false),
            "fn" => TokenKind::KwFn,
            "return" => TokenKind::KwReturn,
            "break" => TokenKind::KwBreak,
            "continue" => TokenKind::KwContinue,
            "enum" => TokenKind::KwEnum,
            "match" => TokenKind::KwMatch,
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

    /// The single diagnostic a malformed source produces.
    fn error(src: &str) -> Diagnostic {
        let errors = lex(src).unwrap_err();
        assert_eq!(errors.len(), 1, "expected exactly one error, got {errors:#?}");
        errors.into_iter().next().expect("just asserted there is one")
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
    fn lexes_the_bool_keyword_and_its_literals() {
        assert_eq!(
            kinds("bool b = true;"),
            vec![
                TokenKind::KwBool,
                TokenKind::Ident("b".into()),
                TokenKind::Eq,
                TokenKind::Bool(true),
                TokenKind::Semi,
                TokenKind::Eof,
            ]
        );
        assert_eq!(kinds("false"), vec![TokenKind::Bool(false), TokenKind::Eof]);
    }

    #[test]
    fn two_character_operators_win_over_their_prefixes() {
        assert_eq!(
            kinds("<= < == = >= > != ! -> -"),
            vec![
                TokenKind::Le,
                TokenKind::Lt,
                TokenKind::EqEq,
                TokenKind::Eq,
                TokenKind::Ge,
                TokenKind::Gt,
                TokenKind::BangEq,
                TokenKind::Bang,
                TokenKind::Arrow,
                TokenKind::Minus,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn a_lone_bang_is_the_negation_operator() {
        // `!` and `!=` share a first character, so the two-character form has to
        // win when it is really there and lose when it is not.
        assert_eq!(
            kinds("!x != !y"),
            vec![
                TokenKind::Bang,
                TokenKind::Ident("x".into()),
                TokenKind::BangEq,
                TokenKind::Bang,
                TokenKind::Ident("y".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn lexes_the_logical_operators() {
        assert_eq!(
            kinds("a && b || c"),
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::AmpAmp,
                TokenKind::Ident("b".into()),
                TokenKind::PipePipe,
                TokenKind::Ident("c".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn a_single_ampersand_or_pipe_is_an_error() {
        // Neither is an operator on its own, so the doubled spelling is the only
        // thing they can have been meant as.
        for (src, doubled) in [("a & b", "&&"), ("a | b", "||")] {
            let error = error(src);
            assert!(error.message.contains("unexpected character"), "{src}: {}", error.message);
            assert!(
                error.label.as_deref().unwrap().contains(doubled),
                "{src}: {:?}",
                error.label
            );
            assert_eq!(error.span, Span::new(2, 1), "{src}");
        }
    }

    #[test]
    fn lexes_the_enum_and_match_punctuation() {
        assert_eq!(
            kinds("Color::Red => "),
            vec![
                TokenKind::Ident("Color".into()),
                TokenKind::ColonColon,
                TokenKind::Ident("Red".into()),
                TokenKind::FatArrow,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn an_equals_has_three_spellings_and_the_longest_wins() {
        assert_eq!(
            kinds("== => ="),
            vec![TokenKind::EqEq, TokenKind::FatArrow, TokenKind::Eq, TokenKind::Eof]
        );
    }

    #[test]
    fn a_single_colon_is_an_error() {
        // TinyC has no `:` of its own, so the only thing it can have meant is
        // the qualifier.
        let error = error("Color:Red");
        assert!(error.message.contains("unexpected character `:`"), "{}", error.message);
        assert!(error.label.as_deref().unwrap().contains("::"), "{:?}", error.label);
    }

    #[test]
    fn a_doubled_operator_is_one_token_not_two() {
        // `&&&` is `&&` followed by a lone `&`, which stops the lexer.
        assert_eq!(kinds("&&"), vec![TokenKind::AmpAmp, TokenKind::Eof]);
        assert!(error("&&&").message.contains("unexpected character `&`"));
    }

    #[test]
    fn lexes_control_flow_keywords_and_braces() {
        assert_eq!(
            kinds("if else while for { }"),
            vec![
                TokenKind::KwIf,
                TokenKind::KwElse,
                TokenKind::KwWhile,
                TokenKind::KwFor,
                TokenKind::LBrace,
                TokenKind::RBrace,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn only_the_exact_spellings_are_reserved() {
        // The keyword match runs on the whole identifier, not on a prefix.
        assert_eq!(kinds("trueish"), vec![TokenKind::Ident("trueish".into()), TokenKind::Eof]);
        assert_eq!(kinds("_true"), vec![TokenKind::Ident("_true".into()), TokenKind::Eof]);
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

    #[test]
    fn lexes_a_function_declaration() {
        assert_eq!(
            kinds("fn add(int a, int b) -> int { return a + b; }"),
            vec![
                TokenKind::KwFn,
                TokenKind::Ident("add".into()),
                TokenKind::LParen,
                TokenKind::KwInt,
                TokenKind::Ident("a".into()),
                TokenKind::Comma,
                TokenKind::KwInt,
                TokenKind::Ident("b".into()),
                TokenKind::RParen,
                TokenKind::Arrow,
                TokenKind::KwInt,
                TokenKind::LBrace,
                TokenKind::KwReturn,
                TokenKind::Ident("a".into()),
                TokenKind::Plus,
                TokenKind::Ident("b".into()),
                TokenKind::Semi,
                TokenKind::RBrace,
                TokenKind::Eof
            ]
        )
    }

    // -- token vocabulary --------------------------------------------------

    #[test]
    fn lexes_every_punctuation_token() {
        assert_eq!(
            kinds("( ) { } ; = + - * / % , !"),
            vec![
                TokenKind::LParen,
                TokenKind::RParen,
                TokenKind::LBrace,
                TokenKind::RBrace,
                TokenKind::Semi,
                TokenKind::Eq,
                TokenKind::Plus,
                TokenKind::Minus,
                TokenKind::Star,
                TokenKind::Slash,
                TokenKind::Percent,
                TokenKind::Comma,
                TokenKind::Bang,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn lexes_every_keyword() {
        assert_eq!(
            kinds("int string bool print if else while for fn return break continue enum match"),
            vec![
                TokenKind::KwInt,
                TokenKind::KwString,
                TokenKind::KwBool,
                TokenKind::KwPrint,
                TokenKind::KwIf,
                TokenKind::KwElse,
                TokenKind::KwWhile,
                TokenKind::KwFor,
                TokenKind::KwFn,
                TokenKind::KwReturn,
                TokenKind::KwBreak,
                TokenKind::KwContinue,
                TokenKind::KwEnum,
                TokenKind::KwMatch,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn a_minus_is_an_arrow_only_when_a_greater_than_follows_it() {
        // The cases the sweep above cannot express: a `-` followed by something
        // other than `>`, and a `>` that is not adjacent to it.
        assert_eq!(kinds("-"), vec![TokenKind::Minus, TokenKind::Eof]);
        assert_eq!(kinds("-->"), vec![TokenKind::Minus, TokenKind::Arrow, TokenKind::Eof]);
        assert_eq!(kinds("- >"), vec![TokenKind::Minus, TokenKind::Gt, TokenKind::Eof]);
    }

    #[test]
    fn a_negative_number_is_a_minus_and_a_literal() {
        // The lexer never produces a negative `Int`; unary minus is the parser's
        // job, which is also why `i64::MIN` cannot be written literally.
        assert_eq!(kinds("-1"), vec![TokenKind::Minus, TokenKind::Int(1), TokenKind::Eof]);
    }

    // -- identifiers -------------------------------------------------------

    #[test]
    fn a_keyword_is_never_recognised_as_a_prefix() {
        // Each of these starts with a keyword's spelling but is longer, so the
        // match on the whole identifier has to fall through to `Ident`.
        for name in [
            "fnx", "returns", "printer", "intx", "forth", "elsewhere", "boolean", "breaks",
            "continued", "enums", "matches",
        ] {
            assert_eq!(
                kinds(name),
                vec![TokenKind::Ident(name.into()), TokenKind::Eof],
                "`{name}` should be an identifier"
            );
        }
    }

    #[test]
    fn identifiers_may_hold_underscores_and_digits() {
        // A digit may continue an identifier but may not start one, or `1a`
        // would lex as a name instead of a malformed literal.
        assert_eq!(
            kinds("_ _x1 a_b_2"),
            vec![
                TokenKind::Ident("_".into()),
                TokenKind::Ident("_x1".into()),
                TokenKind::Ident("a_b_2".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn identifiers_may_be_non_ascii() {
        // `is_ident_start` asks for `is_alphabetic`, not `is_ascii_alphabetic`.
        assert_eq!(kinds("café"), vec![TokenKind::Ident("café".into()), TokenKind::Eof]);
    }

    // -- integer literals --------------------------------------------------

    #[test]
    fn lexes_the_largest_int() {
        assert_eq!(kinds("9223372036854775807"), vec![TokenKind::Int(i64::MAX), TokenKind::Eof]);
    }

    #[test]
    fn the_smallest_int_cannot_be_written_as_a_literal() {
        // `-9223372036854775808` is two tokens, and the literal half overflows
        // on its own. A known limitation, pinned here so it changes on purpose.
        assert!(error("9223372036854775808").message.contains("too large"));
    }

    #[test]
    fn leading_zeros_are_accepted() {
        assert_eq!(kinds("007"), vec![TokenKind::Int(7), TokenKind::Eof]);
    }

    #[test]
    fn rejects_a_suffix_on_an_integer_literal() {
        // The whole malformed token is consumed, so the caret covers `123abc`
        // instead of stopping at the first letter.
        let error = error("123abc");
        assert!(error.message.contains("invalid suffix"), "{}", error.message);
        assert_eq!(error.span, Span::new(0, 6));
    }

    // -- string literals ---------------------------------------------------

    #[test]
    fn decodes_every_escape() {
        assert_eq!(
            kinds(r#""\n\t\r\0\\\"""#),
            vec![TokenKind::Str(b"\n\t\r\0\\\"".to_vec()), TokenKind::Eof]
        );
    }

    #[test]
    fn an_empty_string_is_valid() {
        assert_eq!(kinds(r#""""#), vec![TokenKind::Str(Vec::new()), TokenKind::Eof]);
    }

    #[test]
    fn a_string_holds_utf8_bytes() {
        // Characters are re-encoded on the way in, so the token carries bytes
        // and not chars.
        assert_eq!(
            kinds(r#""héllo""#),
            vec![TokenKind::Str("héllo".as_bytes().to_vec()), TokenKind::Eof]
        );
    }

    #[test]
    fn a_comment_marker_inside_a_string_is_just_text() {
        assert_eq!(
            kinds(r#""// not a comment""#),
            vec![TokenKind::Str(b"// not a comment".to_vec()), TokenKind::Eof]
        );
    }

    #[test]
    fn rejects_an_unknown_escape_and_lists_the_supported_ones() {
        let error = error(r#""a\q""#);
        assert!(error.message.contains(r"unknown escape sequence `\q`"), "{}", error.message);
        assert_eq!(error.span, Span::new(2, 2)); // the `\q`, not the whole string
        assert!(error.note.is_some(), "the note lists the escapes that do work");
    }

    #[test]
    fn reports_an_unterminated_string_at_end_of_file() {
        // The other way a string can run out: not a newline, just no more input.
        assert_eq!(error(r#""abc"#).span, Span::new(0, 1));
    }

    // -- trivia ------------------------------------------------------------

    #[test]
    fn a_comment_may_run_to_the_end_of_the_file() {
        // `skip_trivia` stops on `None` as well as on a newline.
        assert_eq!(kinds("1 // hi"), vec![TokenKind::Int(1), TokenKind::Eof]);
    }

    #[test]
    fn a_single_slash_is_division() {
        assert_eq!(
            kinds("1 / 2"),
            vec![TokenKind::Int(1), TokenKind::Slash, TokenKind::Int(2), TokenKind::Eof]
        );
    }

    #[test]
    fn a_percent_is_the_remainder_operator() {
        // Nothing else starts with `%`, so it needs no lookahead at all.
        assert_eq!(
            kinds("7 % 2"),
            vec![TokenKind::Int(7), TokenKind::Percent, TokenKind::Int(2), TokenKind::Eof]
        );
    }

    #[test]
    fn whitespace_between_tokens_is_optional() {
        assert_eq!(
            kinds("fn f(){return 1;}"),
            vec![
                TokenKind::KwFn,
                TokenKind::Ident("f".into()),
                TokenKind::LParen,
                TokenKind::RParen,
                TokenKind::LBrace,
                TokenKind::KwReturn,
                TokenKind::Int(1),
                TokenKind::Semi,
                TokenKind::RBrace,
                TokenKind::Eof,
            ]
        );
    }

    // -- spans and end of file ---------------------------------------------

    #[test]
    fn an_empty_source_is_just_end_of_file() {
        let tokens = lex("").unwrap();
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].kind, TokenKind::Eof);
        assert_eq!(tokens[0].span, Span::new(0, 0));
    }

    #[test]
    fn end_of_file_is_an_empty_span_after_the_last_byte() {
        // Parser errors are reported at this span, so it has to stay inside the
        // file for `line_col` to find a line for it.
        let tokens = lex("x").unwrap();
        assert_eq!(tokens.last().unwrap().span, Span::new(1, 0));
    }

    #[test]
    fn spans_stay_byte_offsets_after_non_ascii_text() {
        // `é` is two bytes but one character. Spans count bytes; only the
        // rendering in `diag` converts them to a character column.
        let tokens = lex("int café = 1;").unwrap();
        assert_eq!(tokens[1].span, Span::new(4, 5)); // café: 4 chars, 5 bytes
        assert_eq!(tokens[2].span, Span::new(10, 1)); // =
    }

    // -- malformed input ---------------------------------------------------

    #[test]
    fn rejects_an_unexpected_character() {
        let error = error("int x = @;");
        assert!(error.message.contains("unexpected character `@`"), "{}", error.message);
        assert_eq!(error.span, Span::new(8, 1));
    }
}
