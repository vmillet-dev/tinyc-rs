//! Stage 2: tokens -> AST.
//!
//! Straightforward recursive descent. The grammar is:
//!
//! ```text
//! program := stmt*
//! stmt    := decl | print
//! decl    := ("int" | "string") IDENT "=" expr ";"
//! print   := "print" "(" expr ")" ";"
//! expr    := term (("+" | "-") term)*
//! term    := unary (("*" | "/") unary)*
//! unary   := "-" unary | primary
//! primary := INT | STRING | IDENT | "(" expr ")"
//! ```

use crate::ast::{BinOp, Expr, ExprKind, NodeId, Program, Stmt, Ty};
use crate::diag::{Diagnostic, Result, Span};
use crate::token::{Token, TokenKind};

pub fn parse(tokens: &[Token]) -> Result<Program> {
    Parser { tokens, pos: 0, next_id: 0 }.run().map_err(|d| vec![d])
}

type PResult<T> = std::result::Result<T, Diagnostic>;

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    next_id: u32,
}

impl<'a> Parser<'a> {
    fn run(mut self) -> PResult<Program> {
        let mut stmts = Vec::new();
        while !matches!(self.peek().kind, TokenKind::Eof) {
            stmts.push(self.stmt()?);
        }
        Ok(Program { stmts, node_count: self.next_id as usize })
    }

    fn peek(&self) -> &'a Token {
        &self.tokens[self.pos]
    }

    fn bump(&mut self) -> &'a Token {
        let token = &self.tokens[self.pos];
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        token
    }

    fn eat(&mut self, kind: &TokenKind) -> bool {
        if &self.peek().kind == kind {
            self.bump();
            true
        } else {
            false
        }
    }

    /// Consume `kind` or produce "expected X, found Y" at the offending token.
    fn expect(&mut self, kind: TokenKind) -> PResult<&'a Token> {
        if &self.peek().kind == &kind {
            Ok(self.bump())
        } else {
            let found = self.peek();
            Err(Diagnostic::new(
                format!("expected `{}`, found {}", kind.text(), found.kind.describe()),
                found.span,
            )
            .with_label(format!("expected `{}` here", kind.text())))
        }
    }

    fn node_id(&mut self) -> NodeId {
        let id = NodeId(self.next_id);
        self.next_id += 1;
        id
    }

    fn stmt(&mut self) -> PResult<Stmt> {
        match self.peek().kind {
            TokenKind::KwInt => self.decl(Ty::Int),
            TokenKind::KwString => self.decl(Ty::Str),
            TokenKind::KwPrint => self.print_stmt(),
            _ => {
                let found = self.peek();
                Err(Diagnostic::new(
                    format!("expected a statement, found {}", found.kind.describe()),
                    found.span,
                )
                .with_label("statements start with `int`, `string` or `print`"))
            }
        }
    }

    fn decl(&mut self, ty: Ty) -> PResult<Stmt> {
        let ty_span = self.bump().span;

        let name_token = self.peek();
        let TokenKind::Ident(name) = &name_token.kind else {
            return Err(Diagnostic::new(
                format!("expected a variable name, found {}", name_token.kind.describe()),
                name_token.span,
            )
            .with_label("a declaration needs a name here"));
        };
        let (name, name_span) = (name.clone(), name_token.span);
        self.bump();

        self.expect(TokenKind::Eq)?;
        let init = self.expr()?;
        self.expect(TokenKind::Semi)?;
        Ok(Stmt::Decl { ty, ty_span, name, name_span, init })
    }

    fn print_stmt(&mut self) -> PResult<Stmt> {
        let span = self.bump().span;
        let open = self.expect(TokenKind::LParen)?.span;
        let value = self.expr()?;
        self.expect_closing_paren(open)?;
        self.expect(TokenKind::Semi)?;
        Ok(Stmt::Print { span, value })
    }

    fn expr(&mut self) -> PResult<Expr> {
        let mut lhs = self.term()?;
        loop {
            let op = match self.peek().kind {
                TokenKind::Plus => BinOp::Add,
                TokenKind::Minus => BinOp::Sub,
                _ => return Ok(lhs),
            };
            self.bump();
            let rhs = self.term()?;
            lhs = self.binary(op, lhs, rhs);
        }
    }

    fn term(&mut self) -> PResult<Expr> {
        let mut lhs = self.unary()?;
        loop {
            let op = match self.peek().kind {
                TokenKind::Star => BinOp::Mul,
                TokenKind::Slash => BinOp::Div,
                _ => return Ok(lhs),
            };
            self.bump();
            let rhs = self.unary()?;
            lhs = self.binary(op, lhs, rhs);
        }
    }

    fn binary(&mut self, op: BinOp, lhs: Expr, rhs: Expr) -> Expr {
        Expr {
            id: self.node_id(),
            span: lhs.span.to(rhs.span),
            kind: ExprKind::Bin { op, lhs: Box::new(lhs), rhs: Box::new(rhs) },
        }
    }

    fn unary(&mut self) -> PResult<Expr> {
        if let TokenKind::Minus = self.peek().kind {
            let minus = self.bump().span;
            let operand = self.unary()?;
            return Ok(Expr {
                id: self.node_id(),
                span: minus.to(operand.span),
                kind: ExprKind::Neg(Box::new(operand)),
            });
        }
        self.primary()
    }

    fn primary(&mut self) -> PResult<Expr> {
        let token = self.peek();
        let span = token.span;
        let kind = match &token.kind {
            TokenKind::Int(v) => ExprKind::Int(*v),
            TokenKind::Str(bytes) => ExprKind::Str(bytes.clone()),
            TokenKind::Ident(name) => ExprKind::Var(name.clone()),
            TokenKind::LParen => {
                let open = self.bump().span;
                let inner = self.expr()?;
                let close = self.expect_closing_paren(open)?;
                return Ok(Expr {
                    id: self.node_id(),
                    span: open.to(close),
                    kind: inner.kind,
                });
            }
            other => {
                return Err(Diagnostic::new(
                    format!("expected an expression, found {}", other.describe()),
                    span,
                )
                .with_label("expected a number, string, variable or `(`"));
            }
        };
        self.bump();
        Ok(Expr { id: self.node_id(), span, kind })
    }

    fn expect_closing_paren(&mut self, open: Span) -> PResult<Span> {
        if self.eat(&TokenKind::RParen) {
            return Ok(self.tokens[self.pos - 1].span);
        }
        let found = self.peek();
        Err(
            Diagnostic::new(format!("expected `)`, found {}", found.kind.describe()), found.span)
                .with_label("expected `)` here")
                .with_note("to close this `(`", Some(open)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast;
    use crate::lexer::lex;

    fn parse_src(src: &str) -> Result<Program> {
        parse(&lex(src)?)
    }

    #[test]
    fn parses_the_sample_program() {
        let program = parse_src(
            "int x = 10;\nint y = 20;\nstring s = \"hi\";\nprint(x + y);\nprint(s);\n",
        )
        .unwrap();
        assert_eq!(program.stmts.len(), 5);
    }

    #[test]
    fn multiplication_binds_tighter_than_addition() {
        let program = parse_src("print(1 + 2 * 3);").unwrap();
        // The `*` must appear below the `+` in the tree.
        assert_eq!(ast::dump(&program), "print\n  +\n    int 1\n    *\n      int 2\n      int 3\n");
    }

    #[test]
    fn parentheses_override_precedence() {
        let program = parse_src("print((1 + 2) * 3);").unwrap();
        assert_eq!(ast::dump(&program), "print\n  *\n    +\n      int 1\n      int 2\n    int 3\n");
    }

    #[test]
    fn addition_is_left_associative() {
        let program = parse_src("print(1 - 2 - 3);").unwrap();
        assert_eq!(ast::dump(&program), "print\n  -\n    -\n      int 1\n      int 2\n    int 3\n");
    }

    #[test]
    fn reports_a_missing_semicolon_at_the_next_token() {
        let src = "int x = 1\nprint(x);";
        let errors = parse_src(src).unwrap_err();
        assert!(errors[0].message.contains("expected `;`"), "{}", errors[0].message);
        assert_eq!(errors[0].span, Span::new(10, 5)); // the `print` keyword
    }

    #[test]
    fn reports_an_unclosed_paren() {
        let errors = parse_src("print(1 + 2;").unwrap_err();
        assert!(errors[0].message.contains("expected `)`"));
        assert!(errors[0].note.is_some());
    }
}
