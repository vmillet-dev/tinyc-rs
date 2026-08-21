//! Stage 2: tokens -> AST.
//!
//! Straightforward recursive descent. The grammar is:
//!
//! ```text
//! program := stmt*
//! stmt    := decl | assign | print | if | while | for
//! decl    := ("int" | "string" | "bool") IDENT "=" expr ";"
//! assign  := IDENT "=" expr ";"
//! print   := "print" "(" expr ")" ";"
//! if      := "if" "(" expr ")" block ("else" (block | if))?
//! while   := "while" "(" expr ")" block
//! for     := "for" "(" (decl | assign) expr ";" assign-no-semi ")" block
//! block   := "{" stmt* "}"
//! expr    := sum (("==" | "!=" | "<" | "<=" | ">" | ">=") sum)*
//! sum     := term (("+" | "-") term)*
//! term    := unary (("*" | "/") unary)*
//! unary   := "-" unary | primary
//! primary := INT | STRING | BOOL | IDENT | "(" expr ")"
//! ```

use crate::ast::{Block, BinOp, CmpOp, Expr, ExprKind, NodeId, Program, Stmt, Ty};
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
            TokenKind::KwBool => self.decl(Ty::Bool),
            TokenKind::KwPrint => self.print_stmt(),
            TokenKind::KwIf => self.if_stmt(),
            TokenKind::KwWhile => self.while_stmt(),
            TokenKind::KwFor => self.for_stmt(),
            TokenKind::Ident(_) => self.assign(),
            _ => {
                let found = self.peek();
                Err(Diagnostic::new(
                    format!("expected a statement, found {}", found.kind.describe()),
                    found.span,
                )
                .with_label(
                    "statements start with a type, `print`, `if`, `while`, `for` or a variable name",
                ))
            }
        }
    }

    /// `{ stmt* }`
    fn block(&mut self) -> PResult<Block> {
        let open = self.expect(TokenKind::LBrace)?.span;
        let mut stmts = Vec::new();
        loop {
            match self.peek().kind {
                TokenKind::RBrace => {
                    let close = self.bump().span;
                    return Ok(Block { stmts, span: open.to(close) });
                }
                TokenKind::Eof => {
                    let found = self.peek();
                    return Err(Diagnostic::new("unclosed block", found.span)
                        .with_label("expected `}` before the end of the file")
                        .with_note("to close this `{`", Some(open)));
                }
                _ => stmts.push(self.stmt()?),
            }
        }
    }

    /// The condition of `if` and `while`, including its parentheses.
    fn condition(&mut self) -> PResult<Expr> {
        let open = self.expect(TokenKind::LParen)?.span;
        let cond = self.expr()?;
        self.expect_closing_paren(open)?;
        Ok(cond)
    }

    fn if_stmt(&mut self) -> PResult<Stmt> {
        self.bump(); // `if`
        let cond = self.condition()?;
        let then_block = self.block()?;

        let else_block = if self.eat(&TokenKind::KwElse) {
            // `else if` chains by nesting the next `if` in a synthetic block,
            // so the AST needs no separate "else if" shape.
            if matches!(self.peek().kind, TokenKind::KwIf) {
                let nested = self.if_stmt()?;
                Some(Block { stmts: vec![nested], span: self.tokens[self.pos - 1].span })
            } else {
                Some(self.block()?)
            }
        } else {
            None
        };

        Ok(Stmt::If { cond, then_block, else_block })
    }

    fn while_stmt(&mut self) -> PResult<Stmt> {
        self.bump(); // `while`
        let cond = self.condition()?;
        let body = self.block()?;
        Ok(Stmt::While { cond, body })
    }

    fn for_stmt(&mut self) -> PResult<Stmt> {
        self.bump(); // `for`
        let open = self.expect(TokenKind::LParen)?.span;

        // The initialiser is a full statement, so it consumes its own `;`.
        let init = match self.peek().kind {
            TokenKind::KwInt => self.decl(Ty::Int)?,
            TokenKind::KwString => self.decl(Ty::Str)?,
            TokenKind::KwBool => self.decl(Ty::Bool)?,
            TokenKind::Ident(_) => self.assign()?,
            _ => {
                let found = self.peek();
                return Err(Diagnostic::new(
                    format!("expected a declaration or assignment, found {}", found.kind.describe()),
                    found.span,
                )
                .with_label("the first part of a `for` initialises a variable"));
            }
        };

        let cond = self.expr()?;
        self.expect(TokenKind::Semi)?;

        // The step has no trailing `;` — the closing paren ends it.
        let step = self.assign_without_semi()?;
        self.expect_closing_paren(open)?;

        let body = self.block()?;
        Ok(Stmt::For { init: Box::new(init), cond, step: Box::new(step), body })
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

    fn assign(&mut self) -> PResult<Stmt> {
        let stmt = self.assign_without_semi()?;
        self.expect(TokenKind::Semi)?;
        Ok(stmt)
    }

    /// The assignment itself, without its terminator: a `for` step ends at `)`.
    fn assign_without_semi(&mut self) -> PResult<Stmt> {
        let token = self.peek();
        let TokenKind::Ident(name) = &token.kind else {
            return Err(Diagnostic::new(
                format!("expected a variable name, found {}", token.kind.describe()),
                token.span,
            )
            .with_label("an assignment starts with a variable name"));
        };
        let (name, name_span) = (name.clone(), token.span);
        self.bump();

        self.expect(TokenKind::Eq)?;
        let value = self.expr()?;
        Ok(Stmt::Assign { name, name_span, value })
    }

    fn print_stmt(&mut self) -> PResult<Stmt> {
        let span = self.bump().span;
        let open = self.expect(TokenKind::LParen)?.span;
        let value = self.expr()?;
        self.expect_closing_paren(open)?;
        self.expect(TokenKind::Semi)?;
        Ok(Stmt::Print { span, value })
    }

    /// Comparisons bind loosest, so `a + 1 < b * 2` compares the two sums.
    fn expr(&mut self) -> PResult<Expr> {
        let mut lhs = self.sum()?;
        loop {
            let op = match self.peek().kind {
                TokenKind::EqEq => CmpOp::Eq,
                TokenKind::BangEq => CmpOp::Ne,
                TokenKind::Lt => CmpOp::Lt,
                TokenKind::Le => CmpOp::Le,
                TokenKind::Gt => CmpOp::Gt,
                TokenKind::Ge => CmpOp::Ge,
                _ => return Ok(lhs),
            };
            self.bump();
            let rhs = self.sum()?;
            lhs = Expr {
                id: self.node_id(),
                span: lhs.span.to(rhs.span),
                kind: ExprKind::Cmp { op, lhs: Box::new(lhs), rhs: Box::new(rhs) },
            };
        }
    }

    fn sum(&mut self) -> PResult<Expr> {
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
            TokenKind::Bool(v) => ExprKind::Bool(*v),
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
    fn parses_an_assignment() {
        let program = parse_src("int x = 1;\nx = x + 2;").unwrap();
        assert_eq!(
            ast::dump(&program),
            "decl int x\n  int 1\nassign x\n  +\n    var x\n    int 2\n"
        );
    }

    #[test]
    fn parses_a_bool_declaration_and_assignment() {
        let program = parse_src("bool ready = true;\nready = false;").unwrap();
        assert_eq!(
            ast::dump(&program),
            "decl bool ready\n  bool true\nassign ready\n  bool false\n"
        );
    }

    #[test]
    fn comparisons_bind_looser_than_arithmetic() {
        let program = parse_src("print(1 + 2 < 4);").unwrap();
        assert_eq!(
            ast::dump(&program),
            "print\n  <\n    +\n      int 1\n      int 2\n    int 4\n"
        );
    }

    #[test]
    fn parses_if_else() {
        let program = parse_src("if (true) {\n  print(1);\n} else {\n  print(2);\n}").unwrap();
        assert_eq!(
            ast::dump(&program),
            "if\n  bool true\nthen\n  print\n    int 1\nelse\n  print\n    int 2\n"
        );
    }

    #[test]
    fn else_if_nests_inside_the_else_block() {
        let program =
            parse_src("if (true) {\n} else if (false) {\n}").unwrap();
        let dumped = ast::dump(&program);
        // The second `if` appears one level deeper, inside the `else`.
        assert!(dumped.contains("else\n  if\n"), "{dumped}");
    }

    #[test]
    fn parses_a_while_loop() {
        let program = parse_src("while (1 < 2) {\n  print(1);\n}").unwrap();
        assert_eq!(ast::dump(&program), "while\n  <\n    int 1\n    int 2\n  print\n    int 1\n");
    }

    #[test]
    fn parses_a_for_loop() {
        let program = parse_src("for (int i = 0; i < 3; i = i + 1) {\n  print(i);\n}").unwrap();
        assert_eq!(
            ast::dump(&program),
            concat!(
                "for\n",
                "  decl int i\n",
                "    int 0\n",
                "  <\n",
                "    var i\n",
                "    int 3\n",
                "  assign i\n",
                "    +\n",
                "      var i\n",
                "      int 1\n",
                "  print\n",
                "    var i\n",
            )
        );
    }

    #[test]
    fn reports_an_unclosed_block() {
        let errors = parse_src("if (true) {\n  print(1);\n").unwrap_err();
        assert!(errors[0].message.contains("unclosed block"), "{}", errors[0].message);
        assert!(errors[0].note.is_some());
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
