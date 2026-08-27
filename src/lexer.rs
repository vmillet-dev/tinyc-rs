//! Stage 1: source text -> tokens.
//!
//! The lexer walks `char_indices()` so byte offsets stay valid for UTF-8 input,
//! and reports every malformed token it can still find its footing after.

use crate::diag::{Diagnostic, Result, Span};
use crate::token::{StrLit, Token, TokenKind};

pub fn lex(src: &str) -> Result<Vec<Token>> {
    Lexer::new(src).run()
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

    fn run(mut self) -> Result<Vec<Token>> {
        let mut tokens = Vec::new();
        let mut errors: Vec<Diagnostic> = Vec::new();
        loop {
            self.skip_trivia();
            let start = self.offset();
            let Some(_) = self.peek() else { break };

            match self.token(start) {
                Ok(kind) => {
                    tokens.push(Token { kind, span: Span::new(start, self.offset() - start) })
                }
                Err(error) => {
                    errors.push(error);
                    // How far it got says where it is safe to start again. One
                    // character means a stray one — `@`, or a `&` with nothing
                    // beside it — and the very next character is a token like
                    // any other. More than one means it stopped somewhere
                    // *inside* something: an unclosed quote, an escape that
                    // names nothing. There is no telling what the rest of that
                    // was meant to be, so the rest of the line goes with it
                    // rather than being read as tokens nobody wrote.
                    match self.offset() - start {
                        0 => self.bump().map(|_| ()).unwrap_or_default(),
                        1 => {}
                        _ => self.skip_line(),
                    }
                }
            }
        }

        tokens.push(Token { kind: TokenKind::Eof, span: Span::new(self.src.len(), 0) });
        match errors.is_empty() {
            true => Ok(tokens),
            false => Err(errors),
        }
    }

    /// Read one token, having already skipped whatever came before it.
    fn token(&mut self, start: usize) -> std::result::Result<TokenKind, Diagnostic> {
        let c = self.peek().expect("the caller checked there is a character here");
        let kind = match c {
            '(' => self.single(TokenKind::LParen),
            ')' => self.single(TokenKind::RParen),
            '{' => self.single(TokenKind::LBrace),
            '}' => self.single(TokenKind::RBrace),
            ';' => self.single(TokenKind::Semi),
            '[' => self.single(TokenKind::LBracket),
            ']' => self.single(TokenKind::RBracket),
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
            ':' => self.one_or_two(':', TokenKind::ColonColon, TokenKind::Colon),
            '.' => self.single(TokenKind::Dot),
            '<' => self.one_or_two('=', TokenKind::Le, TokenKind::Lt),
            '>' => self.one_or_two('=', TokenKind::Ge, TokenKind::Gt),
            '!' => self.one_or_two('=', TokenKind::BangEq, TokenKind::Bang),
            // These two exist only doubled: TinyC has no bitwise `&` or `|`
            // for the lone character to mean instead.
            '&' => self.only_doubled('&', TokenKind::AmpAmp, start)?,
            '|' => self.only_doubled('|', TokenKind::PipePipe, start)?,
            '+' => self.single(TokenKind::Plus),
            '-' => self.one_or_two('>', TokenKind::Arrow, TokenKind::Minus),
            '*' => self.single(TokenKind::Star),
            '/' => self.single(TokenKind::Slash),
            '%' => self.single(TokenKind::Percent),
            ',' => self.single(TokenKind::Comma),
            '"' => self.string()?,
            '\'' => self.character()?,
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
        Ok(kind)
    }

    /// Throw away what is left of the line, which is where a token that went
    /// wrong part way through stops being worth reading.
    fn skip_line(&mut self) {
        while !matches!(self.peek(), Some('\n') | None) {
            self.bump();
        }
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
        // The words are not written out here: `vocabulary::SPELLED` is the one
        // place they are, so that the editor plugin can be generated from the
        // same list the lexer reads rather than repeating it.
        let word = &self.src[start..self.offset()];
        crate::vocabulary::keyword(word).unwrap_or_else(|| TokenKind::Ident(word.to_string()))
    }

    /// A number, which is an `int` unless a `.` with a digit behind it makes it
    /// a `float`.
    ///
    /// The digits are not accumulated here — they are handed to `str::parse`,
    /// which is the one thing in reach that rounds a decimal literal to the
    /// nearest `f64` **once**. Building the value out of an integer part and a
    /// fraction goes wrong twice over: the fraction cannot hold more digits
    /// than a `u64` has room for, so π written to thirty places is refused for
    /// being *too large* when it is merely precise; and the two halves are
    /// rounded separately and then again when they are added, which lands about
    /// one literal in a thousand on the `f64` next to the right one.
    fn number(&mut self) -> std::result::Result<TokenKind, Diagnostic> {
        let start = self.offset();
        self.digits();

        // A `.` starts a fraction, and TinyC puts a `.` after a number for no
        // other reason — so a `.` with no digit behind it is a mistake with a
        // name rather than an `int` followed by a field access naming no object.
        let is_float = self.peek() == Some('.');
        if is_float {
            self.bump();
            if !self.peek().is_some_and(|c| c.is_ascii_digit()) {
                return Err(Diagnostic::new(
                    "a floating-point literal needs a digit after the `.`",
                    Span::new(start, self.offset() - start),
                )
                .with_label("`1.0` rather than `1.`"));
            }
            self.digits();
        }

        let (noun, contents) = match is_float {
            true => ("floating-point", "digits and one `.`"),
            false => ("integer", "digits"),
        };

        // `123abc` and `1.5f` are malformed literals, not a literal followed by
        // a name. An exponent lands here too: TinyC has no `1e9`.
        if self.peek().is_some_and(is_ident_continue) {
            while self.peek().is_some_and(is_ident_continue) {
                self.bump();
            }
            return Err(Diagnostic::new(
                format!("invalid suffix on {noun} literal"),
                Span::new(start, self.offset() - start),
            )
            .with_label(format!("{noun} literals may only contain {contents}")));
        }

        let span = Span::new(start, self.offset() - start);
        let text = &self.src[start..self.offset()];
        if !is_float {
            // Nothing here carries a sign, so `i64::MIN` is not writable as a
            // literal: it is `-` applied to a value one past the largest one.
            return text.parse::<i64>().map(TokenKind::Int).map_err(|_| {
                Diagnostic::new("integer literal is too large", span).with_label(format!(
                    "`int` values must fit in {}..={}",
                    i64::MIN,
                    i64::MAX
                ))
            });
        }

        let value: f64 = text.parse().expect("digits, one `.` and digits parse as a float");
        // `parse` answers an infinity for a literal past the largest `float`
        // there is. Nothing in the language can *write* an infinity, so letting
        // one in this way would make a row of zeroes mean a value the program
        // never named and cannot have meant.
        if !value.is_finite() {
            return Err(Diagnostic::new("floating-point literal is too large", span)
                .with_label(format!("`float` values must fit in ±{:e}", f64::MAX)));
        }
        Ok(TokenKind::Float(value))
    }

    /// Run past a stretch of decimal digits, however many there are.
    fn digits(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.bump();
        }
    }

    /// A string literal, decoded into the characters it stands for.
    ///
    /// The source is UTF-8 and `chars` has already decoded it, so a literal
    /// containing `é` yields one character here and everywhere after. Nothing
    /// downstream ever sees the two bytes it took to write.
    fn string(&mut self) -> std::result::Result<TokenKind, Diagnostic> {
        let open = self.offset();
        self.bump(); // opening quote
        let mut lit = StrLit::default();
        loop {
            // Read before consuming: this is where the character about to be
            // pushed was written, whether it takes one character of source or
            // the two an escape does.
            let at = self.offset();
            match self.peek() {
                Some('"') => {
                    lit.close(at);
                    self.bump();
                    return Ok(TokenKind::Str(lit));
                }
                Some('\\') => match self.escape()? {
                    Some(c) => lit.push(c, at),
                    None => break,
                },
                // A string literal may not span lines.
                Some('\n') | None => break,
                Some(c) => {
                    self.bump();
                    lit.push(c, at);
                }
            }
        }
        Err(Diagnostic::new("unterminated string literal", Span::new(open, 1))
            .with_label("this quote has no match on the same line"))
    }

    /// A character literal: exactly one character between single quotes.
    ///
    /// "Exactly one" is the whole check, and it is worth making — `'ab'` is a
    /// string somebody quoted the wrong way, and saying so is more useful than
    /// silently keeping the first character.
    fn character(&mut self) -> std::result::Result<TokenKind, Diagnostic> {
        let open = self.offset();
        self.bump(); // opening quote

        let unterminated = || {
            Diagnostic::new("unterminated character literal", Span::new(open, 1))
                .with_label("this quote has no match on the same line")
                .with_note("a character literal is written `'a'`", None)
        };

        let value = match self.peek() {
            Some('\'') => {
                self.bump();
                return Err(Diagnostic::new(
                    "empty character literal",
                    Span::new(open, self.offset() - open),
                )
                .with_label("a character literal holds exactly one character")
                .with_note("the empty *string* is written `\"\"`", None));
            }
            Some('\\') => match self.escape()? {
                Some(c) => c,
                None => return Err(unterminated()),
            },
            Some('\n') | None => return Err(unterminated()),
            Some(c) => {
                self.bump();
                c
            }
        };

        match self.peek() {
            Some('\'') => {
                self.bump();
                Ok(TokenKind::Char(value))
            }
            // Run to the closing quote so the whole literal can be underlined
            // rather than the one character that was allowed.
            Some(_) => {
                while !matches!(self.peek(), Some('\'') | Some('\n') | None) {
                    self.bump();
                }
                let closed = self.peek() == Some('\'');
                if closed {
                    self.bump();
                }
                match closed {
                    true => Err(Diagnostic::new(
                        "character literal holds more than one character",
                        Span::new(open, self.offset() - open),
                    )
                    .with_label("a character literal holds exactly one character")
                    .with_note("write it with double quotes for a string", None)),
                    false => Err(unterminated()),
                }
            }
            None => Err(unterminated()),
        }
    }

    /// One escape sequence, starting at the backslash.
    ///
    /// `None` means the file ended inside it, which is the caller's business:
    /// what is unterminated is the literal, not the escape.
    fn escape(&mut self) -> std::result::Result<Option<char>, Diagnostic> {
        let start = self.offset();
        self.bump(); // the backslash
        let escaped = match self.bump() {
            Some('n') => '\n',
            Some('t') => '\t',
            Some('r') => '\r',
            Some('0') => '\0',
            Some('\\') => '\\',
            Some('"') => '"',
            Some('\'') => '\'',
            Some(c) => {
                return Err(Diagnostic::new(
                    format!("unknown escape sequence `\\{c}`"),
                    Span::new(start, self.offset() - start),
                )
                .with_note(SUPPORTED_ESCAPES, None));
            }
            None => return Ok(None),
        };
        Ok(Some(escaped))
    }
}

const SUPPORTED_ESCAPES: &str = r#"supported escapes are \n \t \r \0 \\ \" \'"#;

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

    /// The characters a literal is expected to have produced.
    fn chars(text: &str) -> StrLit {
        StrLit::from(text)
    }

    /// Every diagnostic a malformed source produces, in order.
    fn errors(src: &str) -> Vec<Diagnostic> {
        lex(src).unwrap_err()
    }

    /// The single diagnostic a malformed source produces.
    ///
    /// Most of these sources hold one mistake, and asserting that they produce
    /// one diagnostic is half of what the test is saying: the lexer carries on
    /// after a bad token, so a second message would mean it had invented a
    /// second mistake out of the first.
    fn error(src: &str) -> Diagnostic {
        let errors = errors(src);
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
    fn lexes_a_float_declaration() {
        assert_eq!(
            kinds("float x = 4.5;"),
            vec![
                TokenKind::KwFloat,
                TokenKind::Ident("x".into()),
                TokenKind::Eq,
                TokenKind::Float(4.5),
                TokenKind::Semi,
                TokenKind::Eof,
            ]
        );
    }

    /// The literal is rounded to the nearest `float` **once**, and however many
    /// digits it takes to say which one that is.
    ///
    /// The digits past what an `f64` can hold are the whole point here, so the
    /// lint that would trim them is off: `rustc` rounds the Rust literal by the
    /// same rule the lexer must, and the test is that the two agree.
    #[test]
    #[allow(clippy::excessive_precision)]
    fn a_float_literal_is_rounded_once() {
        assert_eq!(kinds("0.1")[0], TokenKind::Float(0.1));
        assert_eq!(kinds("10564161.55614565165161")[0], TokenKind::Float(10564161.55614565165161));
        // Rounding an integer part and a fraction apart and adding them rounds
        // three times, and lands one step below the right answer here.
        assert_eq!(kinds("3.691709654685621")[0], TokenKind::Float(3.691709654685621));
        assert_ne!(kinds("3.691709654685621")[0], TokenKind::Float(3.6917096546856207));
        // And more fraction digits than a `u64` could hold is precision, not
        // size: π to thirty places is a number, not a mistake.
        assert_eq!(
            kinds("3.141592653589793238462643383279")[0],
            TokenKind::Float(std::f64::consts::PI)
        );
    }

    /// The two ways a number can be malformed, and the one way it can be too
    /// large to name. Each says which kind of literal it was reading.
    #[test]
    fn a_malformed_number_says_what_was_wrong() {
        assert_eq!(error("1.").message, "a floating-point literal needs a digit after the `.`");
        assert_eq!(error("1.5f").message, "invalid suffix on floating-point literal");
        assert_eq!(error("1e9").message, "invalid suffix on integer literal");
        assert_eq!(error("99999999999999999999").message, "integer literal is too large");
        let huge = format!("{}.0", "9".repeat(400));
        assert_eq!(error(&huge).message, "floating-point literal is too large");
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
        assert_eq!(kinds(r#""a\n\"b""#), vec![TokenKind::Str(chars("a\n\"b")), TokenKind::Eof]);
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
    fn a_colon_is_doubled_only_when_it_really_is() {
        // A single `:` introduces a base class and separates a field from its
        // value, so the qualifier has to win only when the second one is there.
        assert_eq!(
            kinds("Circle : Shape"),
            vec![
                TokenKind::Ident("Circle".into()),
                TokenKind::Colon,
                TokenKind::Ident("Shape".into()),
                TokenKind::Eof,
            ]
        );
        assert_eq!(
            kinds("a::b"),
            vec![
                TokenKind::Ident("a".into()),
                TokenKind::ColonColon,
                TokenKind::Ident("b".into()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn lexes_a_field_access() {
        assert_eq!(
            kinds("self.r"),
            vec![
                TokenKind::Ident("self".into()),
                TokenKind::Dot,
                TokenKind::Ident("r".into()),
                TokenKind::Eof,
            ]
        );
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
            kinds("int string bool float print if else while for fn return break continue enum match"),
            vec![
                TokenKind::KwInt,
                TokenKind::KwString,
                TokenKind::KwBool,
                TokenKind::KwFloat,
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
            vec![TokenKind::Str(chars("\n\t\r\0\\\"")), TokenKind::Eof]
        );
    }

    #[test]
    fn an_empty_string_is_valid() {
        assert_eq!(kinds(r#""""#), vec![TokenKind::Str(StrLit::default()), TokenKind::Eof]);
    }

    #[test]
    fn a_string_holds_characters_not_bytes() {
        // The source's UTF-8 is decoded here and nowhere else, so `é` is one
        // character from this point on however many bytes it took to write.
        let token = kinds(r#""héllo""#);
        assert_eq!(token, vec![TokenKind::Str(chars("héllo")), TokenKind::Eof]);
        let TokenKind::Str(decoded) = &token[0] else { panic!("a string literal") };
        assert_eq!(decoded.chars.len(), 5, "five characters, six bytes");
    }

    // -- character literals ------------------------------------------------

    #[test]
    fn lexes_a_character_literal() {
        assert_eq!(kinds("'a'"), vec![TokenKind::Char('a'), TokenKind::Eof]);
        assert_eq!(kinds("'é'"), vec![TokenKind::Char('é'), TokenKind::Eof]);
        assert_eq!(kinds(r"'\n'"), vec![TokenKind::Char('\n'), TokenKind::Eof]);
        assert_eq!(kinds(r"'\''"), vec![TokenKind::Char('\''), TokenKind::Eof]);
    }

    #[test]
    fn rejects_a_character_literal_holding_more_than_one() {
        // `'ab'` is a string somebody quoted the wrong way, and saying so is
        // more useful than keeping the first character.
        let error = error("'ab'");
        assert!(error.message.contains("more than one character"), "{}", error.message);
        assert_eq!(error.span, Span::new(0, 4), "the whole literal is underlined");
        assert!(error.note.is_some(), "the note points at double quotes");
    }

    #[test]
    fn rejects_an_empty_character_literal() {
        let error = error("''");
        assert!(error.message.contains("empty character literal"), "{}", error.message);
    }

    #[test]
    fn rejects_an_unterminated_character_literal() {
        let error = error("'a\n");
        assert!(error.message.contains("unterminated character literal"), "{}", error.message);
        assert_eq!(error.span, Span::new(0, 1), "the caret goes on the quote");
    }

    #[test]
    fn char_is_a_keyword() {
        assert_eq!(
            kinds("char c"),
            vec![TokenKind::KwChar, TokenKind::Ident("c".to_string()), TokenKind::Eof]
        );
    }

    #[test]
    fn a_comment_marker_inside_a_string_is_just_text() {
        assert_eq!(
            kinds(r#""// not a comment""#),
            vec![TokenKind::Str(chars("// not a comment")), TokenKind::Eof]
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


    // -- running out mid-token ---------------------------------------------

    #[test]
    fn a_character_literal_that_runs_out_at_the_end_of_the_file() {
        // The newline case is covered above; this is the other way a literal
        // can stop, and it reaches a different arm — there is no character
        // after the one that was allowed, not even a wrong one.
        for src in ["'a", "'ab", "'"] {
            let error = error(src);
            assert!(
                error.message.contains("unterminated character literal"),
                "`{src}`: {}",
                error.message
            );
        }
    }

    #[test]
    fn a_backslash_at_the_end_of_the_file_leaves_the_literal_unterminated() {
        // `escape` answers `None` when the file ends inside it, and what is
        // unterminated is the *literal* rather than the escape — so this is the
        // message either kind of literal reports.
        assert!(error(r#""abc\"#).message.contains("unterminated string literal"));
        assert!(error(r"'\").message.contains("unterminated character literal"));
    }

    #[test]
    fn a_string_may_hold_a_quote_of_the_other_kind_escaped_or_not() {
        // `\'` is accepted in a string even though nothing there needs it, so
        // that the two literals take the same escapes.
        assert_eq!(kinds(r#""it\'s""#), vec![TokenKind::Str(chars("it's")), TokenKind::Eof]);
        assert_eq!(kinds(r#""it's""#), vec![TokenKind::Str(chars("it's")), TokenKind::Eof]);
        // ... and a character literal takes `\"` for the same reason.
        assert_eq!(kinds(r#"'\"'"#), vec![TokenKind::Char('"'), TokenKind::Eof]);
    }

    #[test]
    fn the_nul_character_can_be_written_in_both_literals() {
        assert_eq!(kinds(r"'\0'"), vec![TokenKind::Char('\0'), TokenKind::Eof]);
        assert_eq!(kinds(r#""a\0b""#), vec![TokenKind::Str(chars("a\0b")), TokenKind::Eof]);
    }

    #[test]
    fn a_string_literal_may_not_run_past_the_end_of_its_line() {
        // A real newline ends it; `\n` is how one is put *in* it.
        let found = errors("\"a\nb\"");
        assert!(found[0].message.contains("unterminated string literal"), "{found:#?}");
        // Two, and both are real: the quote on the second line opens a literal
        // that is not closed either. That the lexer reads the second line at all
        // is the recovery working.
        assert_eq!(found.len(), 2, "{found:#?}");
        assert_eq!(kinds(r#""a\nb""#), vec![TokenKind::Str(chars("a\nb")), TokenKind::Eof]);
    }

    // -- carrying on after a bad token -------------------------------------

    /// Every mistake in one pass, rather than one recompile each.
    #[test]
    fn a_bad_token_does_not_hide_the_ones_after_it() {
        let found = errors("int a = 1 @ 2;\nbool b = true & false;\nint c = 99999999999999999999;");
        let messages: Vec<&str> = found.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(
            messages,
            vec![
                "unexpected character `@`",
                "unexpected character `&`",
                "integer literal is too large",
            ],
            "{found:#?}"
        );
        // In source order, which is the order they were made in.
        assert!(found.windows(2).all(|p| p[0].span.offset < p[1].span.offset), "{found:#?}");
    }

    /// A stray character costs the character, and a broken token costs its line.
    ///
    /// The difference is the whole of the recovery rule: one character consumed
    /// means the lexer never got *into* anything, so the next character is a
    /// token like any other. More than one means it stopped somewhere inside,
    /// and what follows is not worth reading as tokens nobody wrote.
    #[test]
    fn what_a_mistake_costs_depends_on_how_far_it_got() {
        // The `2` after the `@` is still lexed, on the same line.
        assert_eq!(errors("@ 2\n@ 3").len(), 2);
        // Where an unclosed literal takes the rest of its line with it, so the
        // `@` hiding behind the quote is never reported.
        let swallowed = errors("\"unclosed @ @ @\nint x = 1;");
        assert_eq!(swallowed.len(), 1, "{swallowed:#?}");
        assert!(swallowed[0].message.contains("unterminated"), "{swallowed:#?}");
    }

    #[test]
    fn a_file_of_nothing_but_mistakes_still_ends() {
        // Every one is a character the lexer consumed, so this cannot fail to
        // make progress — but it is the shape that would loop for ever if it
        // ever stopped consuming, so it is worth a test of its own.
        let found = errors("@#$^~`?@#$^~`?");
        assert_eq!(found.len(), 14, "{found:#?}");
    }

    // -- trivia the lexer does not have ------------------------------------

    #[test]
    fn there_are_no_block_comments() {
        // `/*` is a division followed by a multiplication, and saying so here
        // is what keeps a program that expects otherwise from being read wrong.
        assert_eq!(
            kinds("1 /* 2"),
            vec![TokenKind::Int(1), TokenKind::Slash, TokenKind::Star, TokenKind::Int(2),
                 TokenKind::Eof]
        );
    }

    #[test]
    fn a_carriage_return_is_whitespace_like_any_other() {
        // A file with Windows line endings has one before every newline, so a
        // lexer that did not skip it would fail on half the world's files.
        assert_eq!(
            kinds("int\r\nx\r\n"),
            vec![TokenKind::KwInt, TokenKind::Ident("x".into()), TokenKind::Eof]
        );
        // Including at the end of a comment, which stops at the newline.
        assert_eq!(kinds("// hi\r\n1"), vec![TokenKind::Int(1), TokenKind::Eof]);
    }
    // -- malformed input ---------------------------------------------------

    #[test]
    fn rejects_an_unexpected_character() {
        let error = error("int x = @;");
        assert!(error.message.contains("unexpected character `@`"), "{}", error.message);
        assert_eq!(error.span, Span::new(8, 1));
    }
}
