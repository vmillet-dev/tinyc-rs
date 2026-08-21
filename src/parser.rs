//! Stage 2: tokens -> AST.
//!
//! Straightforward recursive descent. The grammar is:
//!
//! ```text
//! program := fn_decl*
//! fn_decl := "fn" IDENT "(" params? ")" ("->" type)? block
//! params  := param ("," param)*
//! param   := type IDENT
//! type    := "int" | "string" | "bool"
//! stmt    := decl | assign | print | if | while | for | return
//! decl    := type IDENT "=" expr ";"
//! assign  := IDENT "=" expr ";"
//! print   := "print" "(" expr ")" ";"
//! if      := "if" "(" expr ")" block ("else" (block | if))?
//! while   := "while" "(" expr ")" block
//! for     := "for" "(" (decl | assign) expr ";" assign-no-semi ")" block
//! return  := "return" expr? ";"
//! block   := "{" stmt* "}"
//! expr    := sum (("==" | "!=" | "<" | "<=" | ">" | ">=") sum)*
//! sum     := term (("+" | "-") term)*
//! term    := unary (("*" | "/") unary)*
//! unary   := "-" unary | primary
//! primary := INT | STRING | BOOL | IDENT | call | "(" expr ")"
//! call    := IDENT "(" (expr ("," expr)*)? ")"
//! ```

use crate::ast::{
    BinOp, Block, CmpOp, Expr, ExprKind, FnDecl, NodeId, Param, Program, Stmt, Ty,
};
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
        let mut functions = Vec::new();
        while !matches!(self.peek().kind, TokenKind::Eof) {
            functions.push(self.fn_decl()?);
        }
        Ok(Program { functions, node_count: self.next_id as usize })
    }

    fn peek(&self) -> &'a Token {
        &self.tokens[self.pos]
    }

    /// The token `offset` positions ahead, clamped to the end-of-file token so
    /// looking past the end is never out of bounds.
    fn peek_at(&self, offset: usize) -> &'a Token {
        &self.tokens[(self.pos + offset).min(self.tokens.len() - 1)]
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

    /// The type a type keyword names, without consuming it. Answering `None`
    /// rather than erroring is what lets a caller use this to *decide*: `stmt`
    /// asks "does a declaration start here?", `fn_decl` asks "is there a return
    /// type?".
    fn type_token(&self) -> Option<Ty> {
        match self.peek().kind {
            TokenKind::KwInt => Some(Ty::Int),
            TokenKind::KwString => Some(Ty::Str),
            TokenKind::KwBool => Some(Ty::Bool),
            _ => None,
        }
    }

    /// Consume a type keyword. `what` names the thing that needed it, so one
    /// helper can produce "a parameter needs a type here" and "a return type
    /// needs a type here".
    fn expect_type(&mut self, what: &str) -> PResult<(Ty, Span)> {
        match self.type_token() {
            Some(ty) => Ok((ty, self.bump().span)),
            None => {
                let found = self.peek();
                Err(Diagnostic::new(
                    format!("expected a type, found {}", found.kind.describe()),
                    found.span,
                )
                .with_label(format!("{what} needs a type here"))
                .with_note("the types are `int`, `string` and `bool`", None))
            }
        }
    }

    /// Consume an identifier and hand back its text and span.
    fn expect_ident(&mut self, what: &str) -> PResult<(String, Span)> {
        let token = self.peek();
        let TokenKind::Ident(name) = &token.kind else {
            return Err(Diagnostic::new(
                format!("expected {what}, found {}", token.kind.describe()),
                token.span,
            )
            .with_label(format!("expected {what} here")));
        };
        // Clone before bumping: `name` borrows the token, and `bump` needs
        // `&mut self`, so the borrow has to end first.
        let ident = (name.clone(), token.span);
        self.bump();
        Ok(ident)
    }

    /// `fn name(params) -> ty { ... }`
    fn fn_decl(&mut self) -> PResult<FnDecl> {
        if !matches!(self.peek().kind, TokenKind::KwFn) {
            let found = self.peek();
            return Err(Diagnostic::new(
                format!("expected a function, found {}", found.kind.describe()),
                found.span,
            )
            .with_label("only functions may appear at the top level")
            .with_note("a program is a list of functions, and starts at `main`", None));
        }
        self.bump(); // `fn`

        let (name, name_span) = self.expect_ident("a function name")?;
        let open = self.expect(TokenKind::LParen)?.span;
        let params = self.params()?;
        let close = self.expect_closing_paren(open)?;

        // No `->` means the function returns nothing. Diagnostics about that
        // point at the `)`, since there is no return type to underline.
        let (ret, ret_span) = if self.eat(&TokenKind::Arrow) {
            let (ty, span) = self.expect_type("a return type")?;
            (Some(ty), span)
        } else {
            (None, close)
        };

        let body = self.block()?;
        Ok(FnDecl { name, name_span, params, ret, ret_span, body })
    }

    /// The parameters between the parentheses: nothing, `int a`, or
    /// `int a, int b`.
    fn params(&mut self) -> PResult<Vec<Param>> {
        let mut params = Vec::new();

        // The empty list is the one case with no parameter to parse at all;
        // checking it first is what keeps the loop below from having to.
        if matches!(self.peek().kind, TokenKind::RParen) {
            return Ok(params);
        }

        loop {
            let (ty, ty_span) = self.expect_type("a parameter")?;
            let (name, name_span) = self.expect_ident("a parameter name")?;
            params.push(Param { ty, ty_span, name, name_span });

            // A comma promises another parameter, so the loop goes round again
            // and `(int a,)` fails on the `)` instead of being accepted.
            if !self.eat(&TokenKind::Comma) {
                return Ok(params);
            }
        }
    }

    /// The arguments of a call: the same list shape as [`Self::params`], with
    /// expressions in place of the type-and-name pairs.
    fn call_args(&mut self) -> PResult<Vec<Expr>> {
        let mut args = Vec::new();
        if matches!(self.peek().kind, TokenKind::RParen) {
            return Ok(args);
        }
        loop {
            args.push(self.expr()?);
            if !self.eat(&TokenKind::Comma) {
                return Ok(args);
            }
        }
    }

    /// `return;` or `return expr;`
    fn return_stmt(&mut self) -> PResult<Stmt> {
        let span = self.bump().span; // `return`

        // A `;` straight after the keyword means there is no value to parse;
        // one token of lookahead is all this decision needs.
        if self.eat(&TokenKind::Semi) {
            return Ok(Stmt::Return { span, value: None });
        }

        let value = self.expr()?;
        self.expect(TokenKind::Semi)?;
        Ok(Stmt::Return { span, value: Some(value) })
    }

    /// A call used as a statement, `greet("hi");` — the only expression
    /// statement in the language, and the only way to call a `void` function.
    fn call_stmt(&mut self) -> PResult<Stmt> {
        let call = self.primary()?;
        self.expect(TokenKind::Semi)?;
        Ok(Stmt::Call(call))
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
            TokenKind::KwReturn => self.return_stmt(),
            // `f(...)` and `x = ...` both start with a name, so this is the one
            // place the parser needs to look one token further ahead.
            TokenKind::Ident(_) if matches!(self.peek_at(1).kind, TokenKind::LParen) => {
                self.call_stmt()
            }
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
            // A name is a variable unless a `(` follows, which makes it a call.
            // Both need the identifier consumed before the decision, so this
            // arm leaves through its own `return` rather than the shared
            // `bump` at the bottom.
            TokenKind::Ident(name) => {
                self.bump();
                if !matches!(self.peek().kind, TokenKind::LParen) {
                    let kind = ExprKind::Var(name.clone());
                    return Ok(Expr { id: self.node_id(), span, kind });
                }
                let open = self.bump().span;
                let args = self.call_args()?;
                let close = self.expect_closing_paren(open)?;
                return Ok(Expr {
                    id: self.node_id(),
                    span: span.to(close),
                    kind: ExprKind::Call { name: name.clone(), name_span: span, args },
                });
            }
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

    /// Parse a `main` body and dump it without the signature line, so tests
    /// about statements stay about statements.
    fn dump_main(body: &str) -> String {
        let program = parse_src(&format!("fn main() {{\n{body}\n}}\n")).unwrap();
        let dumped = ast::dump(&program);
        // Drop the `fn main()` line, and one level of indentation from the rest.
        dumped
            .lines()
            .skip(1)
            .map(|line| line.strip_prefix("  ").unwrap_or(line))
            .fold(String::new(), |mut out, line| {
                out.push_str(line);
                out.push('\n');
                out
            })
    }

    fn errors_in_main(body: &str) -> Vec<Diagnostic> {
        parse_src(&format!("fn main() {{\n{body}\n}}\n")).unwrap_err()
    }

    #[test]
    fn parses_the_sample_program() {
        let program = parse_src(
            "fn main() {\nint x = 10;\nint y = 20;\nstring s = \"hi\";\nprint(x + y);\nprint(s);\n}\n",
        )
        .unwrap();
        assert_eq!(program.functions.len(), 1);
        assert_eq!(program.functions[0].body.stmts.len(), 5);
    }

    #[test]
    fn multiplication_binds_tighter_than_addition() {
        // The `*` must appear below the `+` in the tree.
        assert_eq!(dump_main("print(1 + 2 * 3);"), "print\n  +\n    int 1\n    *\n      int 2\n      int 3\n");
    }

    #[test]
    fn parentheses_override_precedence() {
        assert_eq!(dump_main("print((1 + 2) * 3);"), "print\n  *\n    +\n      int 1\n      int 2\n    int 3\n");
    }

    #[test]
    fn addition_is_left_associative() {
        assert_eq!(dump_main("print(1 - 2 - 3);"), "print\n  -\n    -\n      int 1\n      int 2\n    int 3\n");
    }

    #[test]
    fn parses_an_assignment() {
        assert_eq!(
            dump_main("int x = 1;\nx = x + 2;"),
            "decl int x\n  int 1\nassign x\n  +\n    var x\n    int 2\n"
        );
    }

    #[test]
    fn parses_a_bool_declaration_and_assignment() {
        assert_eq!(
            dump_main("bool ready = true;\nready = false;"),
            "decl bool ready\n  bool true\nassign ready\n  bool false\n"
        );
    }

    #[test]
    fn comparisons_bind_looser_than_arithmetic() {
        assert_eq!(
            dump_main("print(1 + 2 < 4);"),
            "print\n  <\n    +\n      int 1\n      int 2\n    int 4\n"
        );
    }

    #[test]
    fn parses_if_else() {
        assert_eq!(
            dump_main("if (true) {\n  print(1);\n} else {\n  print(2);\n}"),
            "if\n  bool true\nthen\n  print\n    int 1\nelse\n  print\n    int 2\n"
        );
    }

    #[test]
    fn else_if_nests_inside_the_else_block() {
        let dumped = dump_main("if (true) {\n} else if (false) {\n}");
        // The second `if` appears one level deeper, inside the `else`.
        assert!(dumped.contains("else\n  if\n"), "{dumped}");
    }

    #[test]
    fn parses_a_while_loop() {
        assert_eq!(
            dump_main("while (1 < 2) {\n  print(1);\n}"),
            "while\n  <\n    int 1\n    int 2\n  print\n    int 1\n"
        );
    }

    #[test]
    fn parses_a_for_loop() {
        assert_eq!(
            dump_main("for (int i = 0; i < 3; i = i + 1) {\n  print(i);\n}"),
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
        let errors = parse_src("fn main() {\n  if (true) {\n    print(1);\n}\n").unwrap_err();
        assert!(errors[0].message.contains("unclosed block"), "{}", errors[0].message);
        assert!(errors[0].note.is_some());
    }

    #[test]
    fn reports_a_missing_semicolon_at_the_next_token() {
        let errors = errors_in_main("int x = 1\nprint(x);");
        assert!(errors[0].message.contains("expected `;`"), "{}", errors[0].message);
    }

    #[test]
    fn reports_an_unclosed_paren() {
        let errors = errors_in_main("print(1 + 2;");
        assert!(errors[0].message.contains("expected `)`"));
        assert!(errors[0].note.is_some());
    }

    // -- functions ---------------------------------------------------------

    #[test]
    fn parses_a_signature_with_parameters_and_a_return_type() {
        let program = parse_src("fn add(int a, int b) -> int {\n  return a + b;\n}\n").unwrap();
        assert_eq!(
            ast::dump(&program),
            "fn add(int a, int b) -> int\n  return\n    +\n      var a\n      var b\n"
        );
    }

    #[test]
    fn parses_an_empty_parameter_list_and_no_return_type() {
        let program = parse_src("fn main() {\n}\n").unwrap();
        let main = &program.functions[0];
        assert!(main.params.is_empty());
        assert_eq!(main.ret, None);
        assert_eq!(ast::dump(&program), "fn main()\n");
    }

    #[test]
    fn a_missing_return_type_points_diagnostics_at_the_closing_paren() {
        // `ret_span` is never absent, so "this function returns nothing" always
        // has something to underline.
        let program = parse_src("fn f() {\n}\n").unwrap();
        let f = &program.functions[0];
        assert_eq!(f.ret_span, Span::new(5, 1)); // the `)`
    }

    #[test]
    fn parses_every_parameter_count_up_to_four() {
        for (params, expected) in [
            ("", 0),
            ("int a", 1),
            ("int a, int b", 2),
            ("int a, int b, int c", 3),
            ("int a, int b, string d", 3),
        ] {
            let program = parse_src(&format!("fn f({params}) {{\n}}\n")).unwrap();
            assert_eq!(program.functions[0].params.len(), expected, "`{params}`");
        }
    }

    #[test]
    fn rejects_a_trailing_or_lone_comma_in_a_parameter_list() {
        for src in ["fn f(int a,) {\n}", "fn f(,) {\n}", "fn f(int a, ) {\n}"] {
            let errors = parse_src(src).unwrap_err();
            assert!(errors[0].message.contains("expected a type"), "{src}: {}", errors[0].message);
        }
    }

    #[test]
    fn rejects_a_parameter_without_a_name() {
        let errors = parse_src("fn f(int) {\n}").unwrap_err();
        assert!(errors[0].message.contains("expected a parameter name"), "{}", errors[0].message);
    }

    #[test]
    fn rejects_an_arrow_without_a_type() {
        let errors = parse_src("fn f() -> {\n}").unwrap_err();
        assert!(errors[0].message.contains("expected a type"), "{}", errors[0].message);
    }

    #[test]
    fn rejects_a_statement_at_the_top_level() {
        let errors = parse_src("int x = 1;\n").unwrap_err();
        assert!(errors[0].message.contains("expected a function"), "{}", errors[0].message);
        assert_eq!(errors[0].span, Span::new(0, 3));
    }

    #[test]
    fn parses_several_functions() {
        let program = parse_src("fn a() {\n}\nfn b() {\n}\nfn main() {\n}\n").unwrap();
        let names: Vec<&str> = program.functions.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "main"]);
    }

    // -- calls and returns -------------------------------------------------

    #[test]
    fn parses_a_call_in_an_expression() {
        assert_eq!(
            dump_main("print(add(1, 2));"),
            "print\n  call add\n    int 1\n    int 2\n"
        );
    }

    #[test]
    fn parses_a_call_with_no_arguments() {
        assert_eq!(dump_main("print(now());"), "print\n  call now\n");
    }

    #[test]
    fn parses_a_call_as_a_statement() {
        assert_eq!(dump_main("greet(\"hi\");"), "call greet\n  string \"hi\"\n");
    }

    #[test]
    fn a_name_without_parentheses_is_still_a_variable() {
        assert_eq!(dump_main("print(x);"), "print\n  var x\n");
    }

    #[test]
    fn parses_nested_calls() {
        assert_eq!(
            dump_main("print(f(g(1)));"),
            "print\n  call f\n    call g\n      int 1\n"
        );
    }

    #[test]
    fn a_call_may_appear_inside_arithmetic() {
        assert_eq!(
            dump_main("print(f(1) + 2);"),
            "print\n  +\n    call f\n      int 1\n    int 2\n"
        );
    }

    #[test]
    fn parses_both_spellings_of_return() {
        assert_eq!(dump_main("return;"), "return\n");
        assert_eq!(dump_main("return 1 + 2;"), "return\n  +\n    int 1\n    int 2\n");
    }

    #[test]
    fn rejects_a_return_without_a_semicolon() {
        let errors = errors_in_main("return 1");
        assert!(errors[0].message.contains("expected `;`"), "{}", errors[0].message);
    }

    #[test]
    fn rejects_a_trailing_comma_in_an_argument_list() {
        let errors = errors_in_main("f(1,);");
        assert!(errors[0].message.contains("expected an expression"), "{}", errors[0].message);
    }
}
