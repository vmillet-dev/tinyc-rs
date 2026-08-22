//! Stage 2: tokens -> AST.
//!
//! Straightforward recursive descent. The grammar is:
//!
//! ```text
//! program := (enum_decl | fn_decl)*
//! enum    := "enum" IDENT "{" IDENT ("," IDENT)* "}"
//! fn_decl := "fn" IDENT "(" params? ")" ("->" type)? block
//! params  := param ("," param)*
//! param   := type IDENT
//! type    := ("int" | "string" | "char" | "bool" | IDENT) ("[" INT "]")?
//! stmt    := decl | assign | print | if | while | for | match | return
//!          | break | continue
//! decl    := type IDENT "=" expr ";"
//! assign  := place "=" expr ";"
//! place   := IDENT ("[" expr "]")?
//! print   := "print" "(" expr ")" ";"
//! if      := "if" "(" expr ")" block ("else" (block | if))?
//! while   := "while" "(" expr ")" block
//! for     := "for" "(" (decl | assign) expr ";" assign-no-semi ")" block
//! return  := "return" expr? ";"
//! break   := "break" ";"
//! cont    := "continue" ";"
//! block   := "{" stmt* "}"
//! expr    := and ("||" and)*
//! and     := cmp ("&&" cmp)*
//! cmp     := sum (("==" | "!=" | "<" | "<=" | ">" | ">=") sum)*
//! sum     := term (("+" | "-") term)*
//! term    := unary (("*" | "/" | "%") unary)*
//! unary   := ("-" | "!") unary | primary
//! primary := INT | STRING | CHAR | BOOL | IDENT | variant | call | match
//!          | array | index | len | convert | "(" expr ")"
//! variant := IDENT "::" IDENT
//! array   := "[" (expr ("," expr)*)? "]"
//! index   := IDENT "[" expr "]"
//! len     := "len" "(" expr ")"
//! convert := ("int" | "char" | "string" | "bool") "(" expr ")"
//! call    := IDENT "(" (expr ("," expr)*)? ")"
//! match   := "match" "(" expr ")" "{" arm* "}"
//! arm     := IDENT "::" IDENT "=>" (expr ","? | block ","?)
//! ```
//!
//! A `match` is a primary expression, and a statement only in the way a call is
//! one — `stmt` reaches it through the same node.

use crate::ast;
use crate::ast::{
    ArmBody, BinOp, Block, ClassDecl, CmpOp, EnumDecl, Expr, ExprKind, FieldDecl, FieldInit,
    FnDecl, LogicOp, MatchArm, NodeId, Param, Place, Prim, Program, Stmt, TypeRef, Variant,
};
use crate::diag::{Diagnostic, Result, Span};
use crate::token::{Token, TokenKind};

pub fn parse(tokens: &[Token]) -> Result<Program> {
    Parser { tokens, pos: 0, next_id: 0, depth: 0 }.run().map_err(|d| vec![d])
}

type PResult<T> = std::result::Result<T, Diagnostic>;

/// How deeply constructs may nest before the parser gives up.
///
/// Recursive descent turns nesting in the source into nesting on the *call
/// stack*, and so do [`crate::sema`] and [`crate::ir`] afterwards — dropping the
/// tree does too. Without a limit, `((((...))))` is not a parse error but a
/// stack overflow, which is a crash rather than a diagnostic.
///
/// The limit alone is not enough: a debug build spends several kilobytes of
/// stack per level, far more than the megabyte Windows gives a thread by
/// default. [`crate::STACK_SIZE`] is the other half of the bargain, and the two
/// constants must be read together.
pub const MAX_NESTING: u32 = 256;

struct Parser<'a> {
    tokens: &'a [Token],
    pos: usize,
    next_id: u32,
    /// How many nested expressions and blocks are currently open.
    depth: u32,
}

impl<'a> Parser<'a> {
    fn run(mut self) -> PResult<Program> {
        let (mut enums, mut classes, mut functions) = (Vec::new(), Vec::new(), Vec::new());
        while !matches!(self.peek().kind, TokenKind::Eof) {
            match self.peek().kind {
                TokenKind::KwEnum => enums.push(self.enum_decl()?),
                TokenKind::KwClass => classes.push(self.class_decl(&mut functions)?),
                _ => functions.push(self.fn_decl()?),
            }
        }
        Ok(Program { enums, classes, functions, node_count: self.next_id as usize })
    }

    /// `class Circle : Shape { int r; fn area(self) -> int { ... } }`
    ///
    /// Methods are appended to the program's flat list of functions and
    /// referred to by index, so that everything downstream sees one kind of
    /// callable: a method is a function whose first parameter is a receiver.
    fn class_decl(&mut self, functions: &mut Vec<FnDecl>) -> PResult<ClassDecl> {
        self.bump(); // `class`
        let (name, name_span) = self.expect_ident("a class name")?;

        let base = if self.eat(&TokenKind::Colon) {
            Some(self.expect_ident("a base class name")?)
        } else {
            None
        };

        let open = self.expect(TokenKind::LBrace)?.span;
        let (mut fields, mut methods) = (Vec::new(), Vec::new());
        loop {
            match self.peek().kind {
                TokenKind::RBrace => {
                    self.bump();
                    return Ok(ClassDecl { name, name_span, base, fields, methods });
                }
                TokenKind::Eof => {
                    let found = self.peek();
                    return Err(Diagnostic::new("unclosed class", found.span)
                        .with_label("expected `}` before the end of the file")
                        .with_note("to close this `{`", Some(open)));
                }
                TokenKind::KwFn => {
                    methods.push(functions.len());
                    functions.push(self.fn_decl()?);
                }
                // Anything else has to be a field, which starts with a type.
                _ => {
                    let ty = self.expect_type("a field")?;
                    let (name, name_span) = self.expect_ident("a field name")?;
                    self.expect(TokenKind::Semi)?;
                    fields.push(FieldDecl { ty, name, name_span });
                }
            }
        }
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
        if self.peek().kind == kind {
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

    /// Run `parse` one level deeper, refusing to go past [`MAX_NESTING`].
    ///
    /// Every recursive path through the grammar passes through [`Self::unary`]
    /// or [`Self::block`], so counting those two is enough to bound the depth of
    /// the whole tree — and therefore the stack every later pass will use.
    fn nested<T>(&mut self, parse: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        self.depth += 1;
        if self.depth > MAX_NESTING {
            self.depth -= 1;
            let found = self.peek();
            return Err(Diagnostic::new("this program nests too deeply", found.span)
                .with_label("giving up here")
                .with_note(
                    format!("at most {MAX_NESTING} levels of nested expressions and blocks"),
                    None,
                ));
        }
        let parsed = parse(self);
        self.depth -= 1;
        parsed
    }

    /// Consume a type. `what` names the thing that needed it, so one helper can
    /// produce "a parameter needs a type here" and "a return type needs a type
    /// here".
    ///
    /// A bare identifier is accepted as a type: it may name an enum, and the
    /// parser has no way of knowing. What it produces is therefore a
    /// [`TypeRef`] — a name — and [`crate::sema`] is what decides whether such a
    /// type exists.
    fn expect_type(&mut self, what: &str) -> PResult<TypeRef> {
        let token = self.peek();
        let name = match &token.kind {
            TokenKind::KwInt | TokenKind::KwString | TokenKind::KwChar | TokenKind::KwBool => {
                token.kind.text().to_string()
            }
            TokenKind::Ident(name) => name.clone(),
            _ => {
                return Err(Diagnostic::new(
                    format!("expected a type, found {}", token.kind.describe()),
                    token.span,
                )
                .with_label(format!("{what} needs a type here"))
                .with_note("the types are `int`, `string`, `char`, `bool` and any declared enum or class", None));
            }
        };
        let span = self.bump().span;

        // `int[3]`. The length is a literal rather than an expression: it is
        // part of the *type*, and a type is not something the program computes.
        if !self.eat(&TokenKind::LBracket) {
            return Ok(TypeRef { name, array_len: None, span });
        }
        let token = self.peek();
        let TokenKind::Int(len) = token.kind else {
            return Err(Diagnostic::new(
                format!("expected an array length, found {}", token.kind.describe()),
                token.span,
            )
            .with_label("a length has to be written out here")
            .with_note("`int[3]` is an array of three ints", None));
        };
        let len_span = self.bump().span;
        let close = self.expect(TokenKind::RBracket)?.span;
        Ok(TypeRef { name, array_len: Some((len, len_span)), span: span.to(close) })
    }

    /// Whether a declaration starts at the current token.
    ///
    /// A type keyword settles it outright. An identifier does not: `Colour c`
    /// is a declaration, but `c = 1` and `c(1)` are not — so the second token
    /// decides, and only a name can follow a type.
    ///
    /// Arrays make the identifier case reach further, because `Colour[3] cs`
    /// and `cs[3] = 1` agree for two more tokens. They part at the fourth: a
    /// type's length is a literal, and what follows the `]` is a name in a
    /// declaration and an `=` in an assignment.
    fn starts_declaration(&self) -> bool {
        match self.peek().kind {
            TokenKind::KwInt | TokenKind::KwString | TokenKind::KwChar | TokenKind::KwBool => true,
            TokenKind::Ident(_) => match self.peek_at(1).kind {
                TokenKind::Ident(_) => true,
                TokenKind::LBracket => {
                    matches!(self.peek_at(2).kind, TokenKind::Int(_))
                        && matches!(self.peek_at(3).kind, TokenKind::RBracket)
                        && matches!(self.peek_at(4).kind, TokenKind::Ident(_))
                }
                _ => false,
            },
            _ => false,
        }
    }

    /// `enum Color { Red, Green, Blue }`
    fn enum_decl(&mut self) -> PResult<EnumDecl> {
        self.bump(); // `enum`
        let (name, name_span) = self.expect_ident("an enum name")?;
        let open = self.expect(TokenKind::LBrace)?.span;

        let mut variants = Vec::new();
        if !matches!(self.peek().kind, TokenKind::RBrace) {
            loop {
                let (name, name_span) = self.expect_ident("a variant name")?;
                variants.push(Variant { name, name_span });
                // As in a parameter list, a comma promises another one.
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }

        if !self.eat(&TokenKind::RBrace) {
            let found = self.peek();
            return Err(Diagnostic::new(
                format!("expected `}}`, found {}", found.kind.describe()),
                found.span,
            )
            .with_label("a variant list ends here")
            .with_note("to close this `{`", Some(open)));
        }
        Ok(EnumDecl { name, name_span, variants })
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
            .with_label("only enums and functions may appear at the top level")
            .with_note("a program is a list of declarations, and starts at `main`", None));
        }
        self.bump(); // `fn`

        let (name, name_span) = self.expect_ident("a function name")?;
        let open = self.expect(TokenKind::LParen)?.span;
        let params = self.params()?;
        let close = self.expect_closing_paren(open)?;

        // No `->` means the function returns nothing. Diagnostics about that
        // point at the `)`, since there is no return type to underline.
        let (ret, ret_span) = if self.eat(&TokenKind::Arrow) {
            let ty = self.expect_type("a return type")?;
            let span = ty.span;
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
            // `self` is written alone: its type is the class the method is in,
            // and the parser does not know which that is.
            if matches!(&self.peek().kind, TokenKind::Ident(name) if name == ast::SELF) {
                let name_span = self.bump().span;
                params.push(Param { ty: None, name: ast::SELF.to_string(), name_span });
                if !self.eat(&TokenKind::Comma) {
                    return Ok(params);
                }
                continue;
            }
            let ty = self.expect_type("a parameter")?;
            let (name, name_span) = self.expect_ident("a parameter name")?;
            params.push(Param { ty: Some(ty), name, name_span });

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

    /// `break;` or `continue;` — a keyword and a semicolon, nothing else.
    ///
    /// Which loop they belong to is not a syntactic question: the parser has no
    /// idea whether there is one, and says so by accepting them anywhere.
    fn loop_jump(&mut self, build: impl FnOnce(Span) -> Stmt) -> PResult<Stmt> {
        let span = self.bump().span;
        self.expect(TokenKind::Semi)?;
        Ok(build(span))
    }

    /// A call used as a statement, `greet("hi");` — the only expression
    /// statement in the language, and the only way to call a `void` function.
    fn stmt(&mut self) -> PResult<Stmt> {
        // A declaration may begin with a plain identifier now that a type can
        // be an enum's name, so this is asked before the keywords are matched.
        if self.starts_declaration() {
            return self.decl();
        }
        match self.peek().kind {
            TokenKind::KwPrint => self.print_stmt(),
            TokenKind::KwIf => self.if_stmt(),
            TokenKind::KwWhile => self.while_stmt(),
            TokenKind::KwFor => self.for_stmt(),
            // A match written for its effect. Like a call statement, it is the
            // same node as the expression — only what surrounds it differs.
            TokenKind::KwMatch => Ok(Stmt::Match(self.match_expr()?)),
            TokenKind::KwReturn => self.return_stmt(),
            TokenKind::KwBreak => self.loop_jump(|span| Stmt::Break { span }),
            TokenKind::KwContinue => self.loop_jump(|span| Stmt::Continue { span }),
            // `f(...)`, `p.m()` and `x = ...` all start with a name, and stay
            // indistinguishable to the end of the chain — so one parse covers
            // them and what follows decides. See `assign_without_semi`.
            TokenKind::Ident(_) => self.assign(),
            _ => {
                let found = self.peek();
                Err(Diagnostic::new(
                    format!("expected a statement, found {}", found.kind.describe()),
                    found.span,
                )
                .with_label(
                    "statements start with a type, `print`, `if`, `while`, `for`, `match` or a variable name",
                ))
            }
        }
    }

    /// `match (value) { Color::Red => "warm", Color::Green => { ... } }`
    ///
    /// The arms are not judged here beyond their shape: whether they cover the
    /// scrutinee's variants, whether the scrutinee even has any, and whether
    /// what the arms produce agrees, are all questions for the stage that knows
    /// what the names mean.
    fn match_expr(&mut self) -> PResult<Expr> {
        let keyword = self.bump().span; // `match`
        let scrutinee = self.condition()?;
        let open = self.expect(TokenKind::LBrace)?.span;

        let mut arms = Vec::new();
        loop {
            match self.peek().kind {
                TokenKind::RBrace => {
                    let close = self.bump().span;
                    return Ok(Expr {
                        id: self.node_id(),
                        span: keyword.to(close),
                        kind: ExprKind::Match {
                            keyword,
                            scrutinee: Box::new(scrutinee),
                            arms,
                        },
                    });
                }
                TokenKind::Eof => {
                    let found = self.peek();
                    return Err(Diagnostic::new("unclosed match", found.span)
                        .with_label("expected `}` before the end of the file")
                        .with_note("to close this `{`", Some(open)));
                }
                _ => arms.push(self.match_arm()?),
            }
        }
    }

    /// `Color::Red => "warm",` or `Color::Red => { ... }`
    ///
    /// The token after `=>` decides between the two, with no lookahead beyond
    /// it: a `{` can only open a block, because TinyC has no other use for one
    /// in expression position.
    fn match_arm(&mut self) -> PResult<MatchArm> {
        let (enum_name, enum_span) = self.expect_ident("an enum name")?;
        self.expect(TokenKind::ColonColon)?;
        let (variant, variant_span) = self.expect_ident("a variant name")?;
        self.expect(TokenKind::FatArrow)?;

        let body = if matches!(self.peek().kind, TokenKind::LBrace) {
            let block = self.block()?;
            // A block ends itself, so a comma after one is optional — as it is
            // after an `if`'s block.
            self.eat(&TokenKind::Comma);
            ArmBody::Block(block)
        } else {
            let value = self.expr()?;
            // An expression does not, so the next arm needs announcing unless
            // this was the last.
            if !matches!(self.peek().kind, TokenKind::RBrace) {
                self.expect(TokenKind::Comma)?;
            }
            ArmBody::Value(value)
        };
        Ok(MatchArm { enum_name, enum_span, variant, variant_span, body })
    }

    /// `{ stmt* }`
    fn block(&mut self) -> PResult<Block> {
        let open = self.expect(TokenKind::LBrace)?.span;
        self.nested(|p| {
            let mut stmts = Vec::new();
            loop {
                match p.peek().kind {
                    TokenKind::RBrace => {
                        let close = p.bump().span;
                        return Ok(Block { stmts, span: open.to(close) });
                    }
                    TokenKind::Eof => {
                        let found = p.peek();
                        return Err(Diagnostic::new("unclosed block", found.span)
                            .with_label("expected `}` before the end of the file")
                            .with_note("to close this `{`", Some(open)));
                    }
                    _ => stmts.push(p.stmt()?),
                }
            }
        })
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
        let init = if self.starts_declaration() {
            self.decl()?
        } else if matches!(self.peek().kind, TokenKind::Ident(_)) {
            self.assign()?
        } else {
            let found = self.peek();
            return Err(Diagnostic::new(
                format!("expected a declaration or assignment, found {}", found.kind.describe()),
                found.span,
            )
            .with_label("the first part of a `for` initialises a variable"));
        };

        let cond = self.expr()?;
        self.expect(TokenKind::Semi)?;

        // The step has no trailing `;` — the closing paren ends it.
        let step = self.assign_without_semi()?;
        self.expect_closing_paren(open)?;

        let body = self.block()?;
        Ok(Stmt::For { init: Box::new(init), cond, step: Box::new(step), body })
    }

    fn decl(&mut self) -> PResult<Stmt> {
        let ty = self.expect_type("a declaration")?;

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
        Ok(Stmt::Decl { id: self.node_id(), ty, name, name_span, init })
    }

    fn assign(&mut self) -> PResult<Stmt> {
        let stmt = self.assign_without_semi()?;
        self.expect(TokenKind::Semi)?;
        Ok(stmt)
    }

    /// An assignment or a call, without its terminator: a `for` step ends at `)`.
    ///
    /// The two are told apart *after* the fact. Both begin with a postfix chain
    /// — `p.items[i].count` and `p.items[i].push()` agree until the very end —
    /// so the chain is parsed once as an expression, and what follows decides
    /// whether it named a place or did something.
    fn assign_without_semi(&mut self) -> PResult<Stmt> {
        if !matches!(self.peek().kind, TokenKind::Ident(_)) {
            let found = self.peek();
            return Err(Diagnostic::new(
                format!("expected a variable name, found {}", found.kind.describe()),
                found.span,
            )
            .with_label("an assignment starts with a variable name"));
        }

        let expr = self.primary()?;
        if self.eat(&TokenKind::Eq) {
            let target = Self::into_place(expr)?;
            let value = self.expr()?;
            return Ok(Stmt::Assign { target, value });
        }

        // Not an assignment, so it has to be worth writing on its own.
        match expr.kind {
            ExprKind::Call { .. } | ExprKind::MethodCall { .. } => Ok(Stmt::Call(expr)),
            _ => Err(Diagnostic::new("this statement does nothing", expr.span)
                .with_label("expected an assignment or a call")
                .with_note("a value on its own has no effect, so TinyC does not accept one", None)),
        }
    }

    /// Read an expression back as the place it names, or say it names none.
    ///
    /// Every shape that survives here is one the postfix loop built out of a
    /// variable, which is the only thing in TinyC that names storage.
    fn into_place(expr: Expr) -> PResult<Place> {
        let span = expr.span;
        match expr.kind {
            ExprKind::Var(name) => Ok(Place::Var { name, name_span: span }),
            ExprKind::Index { array, index } => Ok(Place::Element {
                base: Box::new(Self::into_place(*array)?),
                index: *index,
                span,
            }),
            ExprKind::Field { object, name, name_span } => Ok(Place::Field {
                base: Box::new(Self::into_place(*object)?),
                name,
                name_span,
            }),
            _ => Err(Diagnostic::new("cannot assign to this", span)
                .with_label("only a variable, an element or a field names a place")),
        }
    }

    fn print_stmt(&mut self) -> PResult<Stmt> {
        let span = self.bump().span;
        let open = self.expect(TokenKind::LParen)?.span;
        let value = self.expr()?;
        self.expect_closing_paren(open)?;
        self.expect(TokenKind::Semi)?;
        Ok(Stmt::Print { span, value })
    }

    /// `||` binds loosest of all, so `a < 1 || b < 2` is one disjunction of two
    /// comparisons rather than a comparison against a disjunction.
    fn expr(&mut self) -> PResult<Expr> {
        let mut lhs = self.and()?;
        while self.eat(&TokenKind::PipePipe) {
            let rhs = self.and()?;
            lhs = self.logic(LogicOp::Or, lhs, rhs);
        }
        Ok(lhs)
    }

    /// `&&` binds tighter than `||`, so `a || b && c` is `a || (b && c)`.
    fn and(&mut self) -> PResult<Expr> {
        let mut lhs = self.comparison()?;
        while self.eat(&TokenKind::AmpAmp) {
            let rhs = self.comparison()?;
            lhs = self.logic(LogicOp::And, lhs, rhs);
        }
        Ok(lhs)
    }

    /// Comparisons bind looser than arithmetic, so `a + 1 < b * 2` compares the
    /// two sums.
    fn comparison(&mut self) -> PResult<Expr> {
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

    fn logic(&mut self, op: LogicOp, lhs: Expr, rhs: Expr) -> Expr {
        Expr {
            id: self.node_id(),
            span: lhs.span.to(rhs.span),
            kind: ExprKind::Logic { op, lhs: Box::new(lhs), rhs: Box::new(rhs) },
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
                TokenKind::Percent => BinOp::Rem,
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

    /// Every recursive path through the expression grammar comes through here,
    /// which is why this is where the nesting limit is counted.
    ///
    /// Both prefix operators bind tighter than any binary one, so `!a == b`
    /// compares `!a` with `b` — the same way `-a * b` multiplies `-a`.
    fn unary(&mut self) -> PResult<Expr> {
        self.nested(|p| {
            let build: fn(Box<Expr>) -> ExprKind = match p.peek().kind {
                TokenKind::Minus => ExprKind::Neg,
                TokenKind::Bang => ExprKind::Not,
                _ => return p.primary(),
            };
            let operator = p.bump().span;
            let operand = p.unary()?;
            Ok(Expr {
                id: p.node_id(),
                span: operator.to(operand.span),
                kind: build(Box::new(operand)),
            })
        })
    }

    /// An atom, then everything written *after* it: `.field`, `.method(...)`
    /// and `[index]`, chained as far as they go.
    ///
    /// One loop rather than a case per shape, which is what lets `a.b[i].c`
    /// parse without the grammar knowing that combination exists.
    fn primary(&mut self) -> PResult<Expr> {
        let mut expr = self.atom()?;
        loop {
            if self.eat(&TokenKind::Dot) {
                let (name, name_span) = self.expect_ident("a field or method name")?;
                // A `(` is what tells a method call from a field.
                if matches!(self.peek().kind, TokenKind::LParen) {
                    let open = self.bump().span;
                    let args = self.call_args()?;
                    let close = self.expect_closing_paren(open)?;
                    expr = Expr {
                        id: self.node_id(),
                        span: expr.span.to(close),
                        kind: ExprKind::MethodCall {
                            receiver: Box::new(expr),
                            name,
                            name_span,
                            args,
                        },
                    };
                } else {
                    expr = Expr {
                        id: self.node_id(),
                        span: expr.span.to(name_span),
                        kind: ExprKind::Field { object: Box::new(expr), name, name_span },
                    };
                }
            } else if self.eat(&TokenKind::LBracket) {
                let index = self.expr()?;
                let close = self.expect(TokenKind::RBracket)?.span;
                expr = Expr {
                    id: self.node_id(),
                    span: expr.span.to(close),
                    kind: ExprKind::Index {
                        array: Box::new(expr),
                        index: Box::new(index),
                    },
                };
            } else {
                return Ok(expr);
            }
        }
    }

    fn atom(&mut self) -> PResult<Expr> {
        let token = self.peek();
        let span = token.span;
        let kind = match &token.kind {
            TokenKind::Int(v) => ExprKind::Int(*v),
            TokenKind::Str(chars) => ExprKind::Str(chars.clone()),
            TokenKind::Char(c) => ExprKind::Char(*c),
            TokenKind::Bool(v) => ExprKind::Bool(*v),
            // `int(c)` — a conversion, written as the type it produces. A type
            // keyword in expression position can be nothing else: a declaration
            // is a *statement*, and statements are told apart before this point.
            TokenKind::KwInt | TokenKind::KwChar | TokenKind::KwString | TokenKind::KwBool => {
                let to = match token.kind {
                    TokenKind::KwInt => Prim::Int,
                    TokenKind::KwChar => Prim::Char,
                    TokenKind::KwString => Prim::Str,
                    _ => Prim::Bool,
                };
                let keyword = self.bump().span;
                let open = self.expect(TokenKind::LParen)?.span;
                let value = self.expr()?;
                let close = self.expect_closing_paren(open)?;
                let span = keyword.to(close);
                return Ok(Expr {
                    id: self.node_id(),
                    span,
                    kind: ExprKind::Convert { to, value: Box::new(value), span },
                });
            }
            // A name is a variable unless a `(` follows, which makes it a call.
            // Both need the identifier consumed before the decision, so this
            // arm leaves through its own `return` rather than the shared
            // `bump` at the bottom.
            TokenKind::Ident(name) => {
                let name = name.clone();
                self.bump();
                // `Color::Red`. A variant is always written qualified, so no
                // lookahead beyond the `::` is needed to tell one from a
                // variable — and no enum may quietly shadow a name.
                if self.eat(&TokenKind::ColonColon) {
                    let (variant, variant_span) = self.expect_ident("a variant name")?;
                    return Ok(Expr {
                        id: self.node_id(),
                        span: span.to(variant_span),
                        kind: ExprKind::Variant {
                            enum_name: name,
                            enum_span: span,
                            variant,
                            variant_span,
                        },
                    });
                }
                // `Circle { r: 5 }`. There is no ambiguity with a block: every
                // construct that takes an expression before one — `if`, `while`,
                // `for`, `match` — puts the expression in parentheses, so a `{`
                // in expression position can only open a literal.
                if matches!(self.peek().kind, TokenKind::LBrace) {
                    return self.object_literal(name, span);
                }
                if !matches!(self.peek().kind, TokenKind::LParen) {
                    let kind = ExprKind::Var(name);
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
            // A match is an ordinary primary expression: it binds as tightly as
            // a literal, so `match (c) { ... } == x` compares the two.
            TokenKind::KwMatch => return self.match_expr(),
            TokenKind::LBracket => return self.array_literal(),
            TokenKind::KwLen => {
                let keyword = self.bump().span;
                let open = self.expect(TokenKind::LParen)?.span;
                let array = self.expr()?;
                let close = self.expect_closing_paren(open)?;
                let span = keyword.to(close);
                return Ok(Expr {
                    id: self.node_id(),
                    span,
                    kind: ExprKind::Len { array: Box::new(array), span },
                });
            }
            // Parentheses only group: the expression between them keeps its own
            // node id, so [`crate::sema`]'s table gains no unused entry, and
            // only its span widens to cover the brackets a diagnostic should
            // underline.
            TokenKind::LParen => {
                let open = self.bump().span;
                let mut inner = self.expr()?;
                let close = self.expect_closing_paren(open)?;
                inner.span = open.to(close);
                return Ok(inner);
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

    /// `Circle { r: 5 }` — the only way to make an object.
    ///
    /// Every field is named, in any order: which fields the class has and
    /// whether they are all here is not a syntactic question.
    fn object_literal(&mut self, class: String, class_span: Span) -> PResult<Expr> {
        let open = self.expect(TokenKind::LBrace)?.span;
        let mut fields = Vec::new();
        if !matches!(self.peek().kind, TokenKind::RBrace) {
            loop {
                let (name, name_span) = self.expect_ident("a field name")?;
                self.expect(TokenKind::Colon)?;
                let value = self.expr()?;
                fields.push(FieldInit { name, name_span, value });
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        if !self.eat(&TokenKind::RBrace) {
            let found = self.peek();
            return Err(Diagnostic::new(
                format!("expected `}}`, found {}", found.kind.describe()),
                found.span,
            )
            .with_label("an object literal ends here")
            .with_note("to close this `{`", Some(open)));
        }
        let span = class_span.to(self.tokens[self.pos - 1].span);
        Ok(Expr { id: self.node_id(), span, kind: ExprKind::New { class, class_span, fields, span } })
    }

    /// `[1, 2, 3]` — the only way to make an array.
    ///
    /// Empty is allowed by the grammar and rejected by `sema`, which is where
    /// "an array needs at least one element" belongs alongside the same rule
    /// for an enum.
    fn array_literal(&mut self) -> PResult<Expr> {
        let open = self.expect(TokenKind::LBracket)?.span;
        let mut elements = Vec::new();
        if !matches!(self.peek().kind, TokenKind::RBracket) {
            loop {
                elements.push(self.expr()?);
                // A comma promises another element, as everywhere else.
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
            }
        }
        if !self.eat(&TokenKind::RBracket) {
            let found = self.peek();
            return Err(Diagnostic::new(
                format!("expected `]`, found {}", found.kind.describe()),
                found.span,
            )
            .with_label("an array literal ends here")
            .with_note("to close this `[`", Some(open)));
        }
        let span = open.to(self.tokens[self.pos - 1].span);
        Ok(Expr { id: self.node_id(), span, kind: ExprKind::Array { elements, span } })
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

    // -- the nesting limit -------------------------------------------------

    /// Parse on a stack big enough for [`MAX_NESTING`] levels — the same one
    /// the CLI runs the compiler on.
    fn parse_deep(src: &str) -> Result<Program> {
        crate::with_compiler_stack(|| parse_src(src))
    }

    fn nested_parens(levels: usize) -> String {
        format!("fn main() {{ print({}1{}); }}", "(".repeat(levels), ")".repeat(levels))
    }

    #[test]
    fn nesting_just_under_the_limit_is_accepted() {
        assert!(parse_deep(&nested_parens(MAX_NESTING as usize - 8)).is_ok());
    }

    #[test]
    fn nesting_past_the_limit_is_a_diagnostic_rather_than_a_crash() {
        // Recursive descent turns nesting into stack frames, so without a limit
        // this is not a parse error but a stack overflow.
        let errors = parse_deep(&nested_parens(MAX_NESTING as usize + 8)).unwrap_err();
        assert!(errors[0].message.contains("nests too deeply"), "{:?}", errors[0]);
    }

    #[test]
    fn the_limit_counts_unary_operators_too() {
        // `-----1` recurses through `unary` without ever passing a `(`.
        let source = format!("fn main() {{ print({}1); }}", "-".repeat(MAX_NESTING as usize + 8));
        let errors = parse_deep(&source).unwrap_err();
        assert!(errors[0].message.contains("nests too deeply"), "{:?}", errors[0]);
    }

    #[test]
    fn the_limit_counts_nested_blocks_too() {
        let levels = MAX_NESTING as usize + 8;
        let source = format!(
            "fn main() {{ {}{} }}",
            "if (true) { ".repeat(levels),
            "}".repeat(levels)
        );
        let errors = parse_deep(&source).unwrap_err();
        assert!(errors[0].message.contains("nests too deeply"), "{:?}", errors[0]);
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
    fn remainder_binds_as_tightly_as_multiplication() {
        assert_eq!(
            dump_main("print(1 + 2 % 3);"),
            "print\n  +\n    int 1\n    %\n      int 2\n      int 3\n"
        );
        // And is left-associative alongside it: `(4 % 3) * 2`.
        assert_eq!(
            dump_main("print(4 % 3 * 2);"),
            "print\n  *\n    %\n      int 4\n      int 3\n    int 2\n"
        );
    }

    // -- logical operators -------------------------------------------------

    #[test]
    fn negation_binds_tighter_than_any_binary_operator() {
        // `!a && b` is `(!a) && b`, and `!a == b` compares `!a` with `b`.
        assert_eq!(
            dump_main("print(!a && b);"),
            "print\n  &&\n    not\n      var a\n    var b\n"
        );
        assert_eq!(
            dump_main("print(!a == b);"),
            "print\n  ==\n    not\n      var a\n    var b\n"
        );
    }

    #[test]
    fn negation_nests() {
        assert_eq!(dump_main("print(!!a);"), "print\n  not\n    not\n      var a\n");
    }

    #[test]
    fn a_bang_is_not_equal_only_when_an_equals_follows_it() {
        // The two spellings are distinguished in the lexer, but this is where
        // getting it wrong would show: `a != b` must not parse as `a ! (= b)`.
        assert_eq!(
            dump_main("print(a != b);"),
            "print\n  !=\n    var a\n    var b\n"
        );
    }


    #[test]
    fn and_binds_tighter_than_or() {
        // The `&&` must appear below the `||` in the tree.
        assert_eq!(
            dump_main("print(true || false && true);"),
            "print\n  ||\n    bool true\n    &&\n      bool false\n      bool true\n"
        );
    }

    #[test]
    fn logic_binds_looser_than_comparison() {
        // Without this, `1 < 2 && 3 < 4` would compare `2 && 3`.
        assert_eq!(
            dump_main("print(1 < 2 && 3 < 4);"),
            concat!(
                "print\n",
                "  &&\n",
                "    <\n",
                "      int 1\n",
                "      int 2\n",
                "    <\n",
                "      int 3\n",
                "      int 4\n",
            )
        );
    }

    #[test]
    fn logical_operators_are_left_associative() {
        assert_eq!(
            dump_main("print(true && false && true);"),
            "print\n  &&\n    &&\n      bool true\n      bool false\n    bool true\n"
        );
    }

    #[test]
    fn parentheses_override_logical_precedence() {
        assert_eq!(
            dump_main("print((true || false) && true);"),
            "print\n  &&\n    ||\n      bool true\n      bool false\n    bool true\n"
        );
    }

    // -- break and continue ------------------------------------------------

    #[test]
    fn parses_break_and_continue() {
        assert_eq!(
            dump_main("while (true) {\n  break;\n  continue;\n}"),
            "while\n  bool true\n  break\n  continue\n"
        );
    }

    #[test]
    fn break_and_continue_need_a_semicolon() {
        for body in ["while (true) {\n  break\n}", "while (true) {\n  continue\n}"] {
            let errors = errors_in_main(body);
            assert!(errors[0].message.contains("expected `;`"), "{body}: {}", errors[0].message);
        }
    }

    #[test]
    fn the_parser_accepts_a_loop_jump_outside_a_loop() {
        // Nothing syntactic is wrong with it; `sema` is the stage that knows
        // there is no loop to leave.
        assert_eq!(dump_main("break;"), "break\n");
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

    // -- classes -----------------------------------------------------------

    #[test]
    fn parses_a_class_with_a_base_a_field_and_a_method() {
        let program =
            parse_src("class Circle : Shape {\n  int r;\n  fn area(self) -> int {\n    return 1;\n  }\n}\nfn main() {\n}\n")
                .unwrap();
        assert_eq!(
            ast::dump(&program),
            concat!(
                "class Circle : Shape\n",
                "  field int r\n",
                "  fn area(self) -> int\n",
                "    return\n",
                "      int 1\n",
                "fn main()\n",
            )
        );
    }

    #[test]
    fn a_method_is_lifted_into_the_flat_list_of_functions() {
        // Everything after the parser sees one kind of callable; a class only
        // says which of them are its own.
        let program =
            parse_src("class A {\n  fn f(self) {\n  }\n}\nfn main() {\n}\n").unwrap();
        assert_eq!(program.functions.len(), 2);
        assert_eq!(program.classes[0].methods, vec![0]);
    }

    #[test]
    fn parses_an_object_literal() {
        assert_eq!(
            dump_main("Circle c = Circle { r: 5 };"),
            "decl Circle c\n  new Circle\n    r\n      int 5\n"
        );
    }

    #[test]
    fn an_object_literal_is_told_from_a_block_by_where_it_appears() {
        // Every construct that takes an expression before a block puts the
        // expression in parentheses, so a `{` in expression position can only
        // open a literal — there is nothing to disambiguate.
        assert!(parse_src("fn main() {\n  if (c) {\n    print(1);\n  }\n}").is_ok());
        assert_eq!(
            dump_main("f(Circle { r: 1 });"),
            "call f\n  new Circle\n    r\n      int 1\n"
        );
    }

    #[test]
    fn parses_a_field_access_and_a_method_call() {
        assert_eq!(dump_main("print(c.r);"), "print\n  field r\n    var c\n");
        assert_eq!(
            dump_main("print(s.area(1));"),
            "print\n  method area\n    var s\n    int 1\n"
        );
    }

    #[test]
    fn postfix_chains_as_far_as_it_goes() {
        // One loop rather than a case per shape, so a combination the grammar
        // never names still parses.
        assert_eq!(
            dump_main("print(a.b[0].c);"),
            "print\n  field c\n    index\n      field b\n        var a\n      int 0\n"
        );
    }

    #[test]
    fn a_field_and_a_method_call_are_told_apart_by_the_parenthesis() {
        assert_eq!(dump_main("c.f = 1;"), "assign c.f\n  int 1\n");
        assert_eq!(dump_main("c.f();"), "method f\n  var c\n");
    }

    #[test]
    fn rejects_assigning_to_something_that_is_not_a_place() {
        let errors = errors_in_main("f() = 1;");
        assert!(errors[0].message.contains("cannot assign to this"), "{}", errors[0].message);
    }

    #[test]
    fn rejects_a_statement_that_does_nothing() {
        let errors = errors_in_main("c.r;");
        assert!(errors[0].message.contains("does nothing"), "{}", errors[0].message);
    }

    #[test]
    fn reports_an_unclosed_class() {
        let errors = parse_src("class A {\n  int x;\n").unwrap_err();
        assert!(errors[0].message.contains("unclosed class"), "{}", errors[0].message);
        assert!(errors[0].note.is_some());
    }

    // -- arrays ------------------------------------------------------------

    #[test]
    fn parses_an_array_declaration() {
        assert_eq!(
            dump_main("int[3] xs = [1, 2, 3];"),
            "decl int[3] xs\n  array\n    int 1\n    int 2\n    int 3\n"
        );
    }

    #[test]
    fn a_declaration_and_an_indexed_assignment_agree_for_three_tokens() {
        // `Colour[3] cs` and `cs[3] = 1` part at the fourth: a name after the
        // `]` in one, an `=` in the other.
        assert_eq!(
            dump_main("Colour[3] cs = [1];"),
            "decl Colour[3] cs\n  array\n    int 1\n"
        );
        assert_eq!(dump_main("cs[3] = 1;"), "assign cs[]\n  int 3\n  int 1\n");
    }

    #[test]
    fn parses_indexing_and_len() {
        assert_eq!(dump_main("print(xs[0]);"), "print\n  index\n    var xs\n    int 0\n");
        assert_eq!(dump_main("print(len(xs));"), "print\n  len\n    var xs\n");
    }

    #[test]
    fn an_index_may_be_any_expression() {
        assert_eq!(
            dump_main("print(xs[i + 1]);"),
            "print\n  index\n    var xs\n    +\n      var i\n      int 1\n"
        );
    }

    #[test]
    fn an_array_type_needs_a_written_length() {
        let errors = errors_in_main("int[n] xs = [1];");
        assert!(errors[0].message.contains("expected an array length"), "{}", errors[0].message);
    }

    #[test]
    fn rejects_a_trailing_comma_and_an_unclosed_literal() {
        assert!(errors_in_main("int[1] xs = [1,];")[0].message.contains("expected an expression"));
        let errors = errors_in_main("int[2] xs = [1, 2;");
        assert!(errors[0].message.contains("expected `]`"), "{}", errors[0].message);
        assert!(errors[0].note.is_some());
    }

    #[test]
    fn an_empty_literal_is_left_for_sema_to_judge() {
        // Its element type is the question, and that is not a syntactic one.
        assert_eq!(dump_main("int[1] xs = [];"), "decl int[1] xs\n  array\n");
    }

    // -- enums and match ---------------------------------------------------

    #[test]
    fn parses_an_enum_declaration() {
        let program = parse_src("enum Color { Red, Green, Blue }\nfn main() {\n}\n").unwrap();
        assert_eq!(ast::dump(&program), "enum Color { Red, Green, Blue }\nfn main()\n");
    }

    #[test]
    fn an_enum_may_have_one_variant() {
        let program = parse_src("enum Unit { Only }\nfn main() {\n}\n").unwrap();
        assert_eq!(program.enums[0].variants.len(), 1);
    }

    #[test]
    fn rejects_a_trailing_comma_in_a_variant_list() {
        // As in a parameter list, a comma promises another one.
        let errors = parse_src("enum Color { Red, }\nfn main() {\n}").unwrap_err();
        assert!(errors[0].message.contains("expected a variant name"), "{}", errors[0].message);
    }

    #[test]
    fn reports_an_unclosed_variant_list_against_its_brace() {
        let errors = parse_src("enum Color { Red Green }\nfn main() {\n}").unwrap_err();
        assert!(errors[0].message.contains("expected `}`"), "{}", errors[0].message);
        assert!(errors[0].note.is_some());
    }

    #[test]
    fn parses_a_variant_expression() {
        assert_eq!(dump_main("print(Color::Red);"), "print\n  variant Color::Red\n");
    }

    #[test]
    fn a_name_is_a_variable_a_call_or_a_variant_by_what_follows_it() {
        // The three shapes that start with an identifier, told apart by the
        // very next token and nothing else.
        assert_eq!(dump_main("print(c);"), "print\n  var c\n");
        assert_eq!(dump_main("print(c());"), "print\n  call c\n");
        assert_eq!(dump_main("print(c::d);"), "print\n  variant c::d\n");
    }

    #[test]
    fn parses_a_declaration_whose_type_is_a_name() {
        // `Color c = ...` and `c = ...` both start with an identifier, so the
        // *second* token is what decides.
        assert_eq!(
            dump_main("Color c = Color::Red;"),
            "decl Color c\n  variant Color::Red\n"
        );
        assert_eq!(dump_main("c = 1;"), "assign c\n  int 1\n");
    }

    #[test]
    fn parses_a_match_of_blocks() {
        assert_eq!(
            dump_main("match (c) {\n  Color::Red => { print(1); }\n  Color::Blue => { print(2); }\n}"),
            concat!(
                "match\n",
                "  var c\n",
                "  Color::Red\n",
                "    print\n",
                "      int 1\n",
                "  Color::Blue\n",
                "    print\n",
                "      int 2\n",
            )
        );
    }

    #[test]
    fn the_parser_accepts_a_match_it_cannot_judge() {
        // Whether the arms cover the enum, or whether these names mean
        // anything, is not a syntactic question.
        assert!(parse_src("fn main() {\n  match (c) {\n  }\n}").is_ok());
        assert!(
            parse_src("fn main() {\n  match (c) {\n    A::B => {}\n    A::B => {}\n  }\n}").is_ok()
        );
    }

    #[test]
    fn a_match_arm_needs_a_qualified_pattern() {
        for (body, expected) in [
            ("match (c) {\n  Red => 1\n}", "expected `::`"),
            ("match (c) {\n  Color::Red 1\n}", "expected `=>`"),
        ] {
            let errors = errors_in_main(body);
            assert!(errors[0].message.contains(expected), "{body}: {}", errors[0].message);
        }
    }

    #[test]
    fn a_value_arm_announces_the_next_one_with_a_comma() {
        // An expression does not end itself, so without the comma the parser
        // would keep reading `1 Color::Blue` as one expression.
        let errors = errors_in_main("match (c) {\n  Color::Red => 1\n  Color::Blue => 2,\n}");
        assert!(errors[0].message.contains("expected `,`"), "{}", errors[0].message);
        // The last arm needs none, since `}` ends it.
        assert!(parse_src("fn main() {\n  match (c) {\n    A::B => 1\n  }\n}").is_ok());
    }

    #[test]
    fn a_block_arm_needs_no_comma_but_may_have_one() {
        for body in [
            "match (c) {\n  A::X => { }\n  A::Y => { }\n}",
            "match (c) {\n  A::X => { },\n  A::Y => { },\n}",
        ] {
            assert!(parse_src(&format!("fn main() {{\n{body}\n}}")).is_ok(), "{body}");
        }
    }

    #[test]
    fn the_token_after_the_arrow_picks_the_arm_shape() {
        // A `{` can only open a block; anything else begins an expression.
        assert_eq!(
            dump_main("match (c) {\n  A::X => 1,\n  A::Y => { print(2); }\n}"),
            concat!(
                "match\n",
                "  var c\n",
                "  A::X\n",
                "    int 1\n",
                "  A::Y\n",
                "    print\n",
                "      int 2\n",
            )
        );
    }

    #[test]
    fn a_match_is_an_expression_anywhere_one_fits() {
        assert_eq!(
            dump_main("int n = match (c) {\n  A::X => 1,\n};"),
            "decl int n\n  match\n    var c\n    A::X\n      int 1\n"
        );
        // And binds as tightly as a literal, so this compares the two.
        assert!(parse_src("fn main() {\n  print(match (c) { A::X => 1 } == 2);\n}").is_ok());
    }

    #[test]
    fn reports_an_unclosed_match() {
        let errors = parse_src("fn main() {\n  match (c) {\n    A::B => { }\n}\n").unwrap_err();
        assert!(errors[0].message.contains("unclosed"), "{}", errors[0].message);
    }

    #[test]
    fn an_enum_may_be_declared_between_functions() {
        let program =
            parse_src("fn a() {\n}\nenum E { X }\nfn main() {\n}\n").unwrap();
        assert_eq!(program.enums.len(), 1);
        assert_eq!(program.functions.len(), 2);
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
        assert!(main.ret.is_none());
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
