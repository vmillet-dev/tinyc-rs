//! Stage 2: tokens -> AST.
//!
//! Straightforward recursive descent. The grammar is:
//!
//! ```text
//! program := (enum_decl | fn_decl)*
//! enum    := "enum" IDENT "{" variant ("," variant)* ","? "}"
//! variant := IDENT ("(" type ("," type)* ")")?
//! fn_decl := "fn" IDENT "(" params? ")" ("->" type)? block
//! params  := param ("," param)*
//! param   := type IDENT
//! type    := (PRIM | IDENT) ("[" INT "]")?    -- PRIM is `ast::Prim::ALL`
//! stmt    := decl | assign | print | if | while | for | match | return
//!          | break | continue
//! decl    := type IDENT "=" expr ";"
//! assign  := place "=" expr ";"
//! place   := IDENT ("[" expr "]")?
//! print   := ("print" | "println") "(" args? ")" ";"
//! args    := format ("," expr)* | expr
//! format  := STRING          -- a literal, whose `%`s are checked here
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
//! variant := IDENT "::" IDENT ("(" (expr ("," expr)*)? ")")?
//! array   := "[" (expr ("," expr)*)? "]"
//! index   := IDENT "[" expr "]"
//! len     := "len" "(" expr ")"
//! convert := PRIM "(" expr ")"
//! call    := IDENT "(" (expr ("," expr)*)? ")"
//! match   := "match" "(" expr ")" "{" arm* "}"
//! arm     := pattern "=>" (expr ","? | block ","?)
//! pattern := IDENT "::" IDENT ("(" IDENT ("," IDENT)* ")")?
//!          | "-"? INT | STRING | CHAR | BOOL | FLOAT | "_"
//! ```
//!
//! A `match` is a primary expression, and a statement only in the way a call is
//! one — `stmt` reaches it through the same node.

use crate::ast;
use crate::ast::{
    ArmBody, BinOp, Block, ClassDecl, CmpOp, EnumDecl, Expr, ExprKind, FieldDecl, FieldInit,
    FnDecl, LogicOp, MatchArm, NodeId, Param, Pattern, Place, Prim, PrintPart, Program, Shape,
    Spec, Stmt, TypeRef, Variant, WILDCARD,
};
use crate::diag::{Diagnostic, Result, Span};
use crate::token::{StrLit, Token, TokenKind};

pub fn parse(tokens: &[Token]) -> Result<Program> {
    Parser { tokens, pos: 0, next_id: 0, depth: 0, errors: Vec::new() }.run()
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
    /// Every mistake found so far.
    ///
    /// A recursive-descent parser has no way to *continue* through a mistake —
    /// it does not know what the program meant — so what it does instead is
    /// throw away tokens until it reaches somewhere it can start again. See
    /// [`Parser::recover_to_statement`]. Every error found that way is reported
    /// and the tree is then discarded whole: it exists only to let the parser
    /// keep reading, and nothing downstream is ever shown it.
    errors: Vec<Diagnostic>,
}

/// A binary operator, whichever of the three kinds of node it builds.
///
/// The three are separate in the tree — `&&` does not evaluate both operands,
/// and a comparison produces a `bool` whatever it took — but they are written
/// the same way and parse the same way, which is what lets one loop read all
/// five precedence levels. See [`Parser::chain`].
#[derive(Clone, Copy)]
enum Operator {
    Arith(BinOp),
    Compare(CmpOp),
    Logic(LogicOp),
}

impl<'a> Parser<'a> {
    fn run(mut self) -> Result<Program> {
        let (mut enums, mut classes, mut functions) = (Vec::new(), Vec::new(), Vec::new());
        while !matches!(self.peek().kind, TokenKind::Eof) {
            let (mark, depth) = (self.pos, self.depth);
            let parsed = match self.peek().kind {
                TokenKind::KwEnum => self.enum_decl().map(|d| enums.push(d)),
                TokenKind::KwClass => self.class_decl(&mut functions).map(|d| classes.push(d)),
                _ => self.fn_decl().map(|d| functions.push(d)),
            };
            if let Err(error) = parsed {
                self.errors.push(error);
                self.depth = depth;
                self.recover_to_declaration();
                self.ensure_progress(mark);
            }
        }
        match self.errors.is_empty() {
            true => Ok(Program { enums, classes, functions, node_count: self.next_id as usize }),
            // Sorted, because a mistake inside a declaration is found before the
            // one that ended it — see `crate::sema::check`, which sorts for the
            // same reason.
            false => {
                self.errors.sort_by_key(|d| d.span.offset);
                Err(self.errors)
            }
        }
    }

    // -- carrying on after a mistake ---------------------------------------

    /// Throw tokens away until the next declaration could begin.
    ///
    /// Only `fn`, `class` and `enum` start one, and only outside braces — a
    /// `class` keyword cannot appear inside a body, so a brace counter is all
    /// it takes to tell "the next declaration" from "somewhere in the middle of
    /// this one".
    ///
    /// That counter is also what stops a missing `}` from turning into a
    /// diagnostic per line for the rest of the file: the count never comes back
    /// to zero, so everything after it is skipped and the one real mistake is
    /// the only thing reported.
    fn recover_to_declaration(&mut self) {
        self.skip_until(|kind, braces| {
            braces == 0
                && matches!(kind, TokenKind::KwFn | TokenKind::KwClass | TokenKind::KwEnum)
        });
    }

    /// Throw tokens away until the next statement could begin.
    ///
    /// A `;` ends the statement that went wrong, so it is consumed and the next
    /// one starts clean. A `}` closes the block, and is left for the loop in
    /// [`Self::block`] to read. Anything that could open a statement stops the
    /// skipping where it stands, which is what makes a forgotten `;` cost one
    /// diagnostic rather than one per line after it.
    fn recover_to_statement(&mut self) {
        let stopped = self.skip_until(|kind, braces| {
            braces == 0
                && (Prim::of_keyword(kind).is_some()
                    || matches!(
                        kind,
                        TokenKind::Semi
                            | TokenKind::RBrace
                            | TokenKind::KwPrint
                            | TokenKind::KwPrintln
                            | TokenKind::KwPush
                            | TokenKind::KwIf
                            | TokenKind::KwWhile
                            | TokenKind::KwFor
                            | TokenKind::KwMatch
                            | TokenKind::KwReturn
                            | TokenKind::KwBreak
                            | TokenKind::KwContinue
                            | TokenKind::Ident(_)
                    ))
        });
        // The `;` belonged to the statement that failed; a `}` belongs to the
        // block, and anything else is the next statement's first token.
        if stopped && matches!(self.peek().kind, TokenKind::Semi) {
            self.bump();
        }
    }

    /// Skip tokens until `stop` accepts one, tracking brace depth on the way.
    /// Answers whether it stopped at a token rather than at the end of the file.
    fn skip_until(&mut self, stop: impl Fn(&TokenKind, u32) -> bool) -> bool {
        let mut braces = 0u32;
        loop {
            let kind = &self.peek().kind;
            if matches!(kind, TokenKind::Eof) {
                return false;
            }
            if stop(kind, braces) {
                return true;
            }
            match kind {
                TokenKind::LBrace => braces += 1,
                TokenKind::RBrace => braces = braces.saturating_sub(1),
                _ => {}
            }
            self.bump();
        }
    }

    /// The one thing recovery must guarantee: that the next attempt starts
    /// somewhere new.
    ///
    /// Everything above stops *before* the token it recognised, so a mistake
    /// reported at a token that also begins a statement would be read again,
    /// fail again, and go round for ever. One token is the price of ruling that
    /// out, and it is paid only when nothing else moved.
    fn ensure_progress(&mut self, mark: usize) {
        if self.pos == mark {
            self.bump();
        }
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
                TokenKind::Eof => return Err(self.unclosed("class", open)),
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
    /// What is counted is the depth of the *tree*, not the depth this parser
    /// happens to recurse to: the two part company wherever a loop builds a
    /// left-leaning chain, and it is the tree that every later pass — and the
    /// drop that frees it — walks recursively. See [`Self::deepen`].
    fn nested<T>(&mut self, parse: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        self.deepen()?;
        let parsed = parse(self);
        self.release(1);
        parsed
    }

    /// Charge one level of nesting for a node about to be built.
    ///
    /// [`Self::nested`] is the recursive form, which releases its level when
    /// the call returns. A loop that builds a left-leaning chain has no such
    /// call to hang one on — `a + b + c` is exactly as deep a tree as
    /// `a + (b + c)`, and would otherwise be counted as flat — so it charges a
    /// level per node here and releases them together at the end.
    ///
    /// Nothing releases on the error path, and nothing here needs to: the two
    /// places that carry on after a mistake — [`Parser::run`] and the statement
    /// loop in [`Parser::block`] — put the counter back to what it held before
    /// the attempt. That is exact where releasing level by level would not be,
    /// since a failed parse has no idea how many it charged.
    fn deepen(&mut self) -> PResult<()> {
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
        Ok(())
    }

    fn release(&mut self, levels: u32) {
        self.depth -= levels;
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
        // A keyword names a type of the language's, an identifier one of the
        // program's, and the note lists the first set rather than repeating it:
        // a type added to `Prim` and forgotten here would otherwise be a type
        // the compiler accepts and the diagnostic denies.
        let name = match &token.kind {
            kind if Prim::of_keyword(kind).is_some() => kind.text().to_string(),
            TokenKind::Ident(name) => name.clone(),
            _ => {
                return Err(Diagnostic::new(
                    format!("expected a type, found {}", token.kind.describe()),
                    token.span,
                )
                .with_label(format!("{what} needs a type here"))
                .with_note(
                    format!(
                        "the types are {} and any declared enum or class",
                        Prim::all_quoted()
                    ),
                    None,
                ));
            }
        };
        let span = self.bump().span;

        // `int[3]`. The length is a literal rather than an expression: it is
        // part of the *type*, and a type is not something the program computes.
        // `int[]` says the opposite — that the length is not knowable here —
        // and the brackets are empty for exactly that reason.
        if !self.eat(&TokenKind::LBracket) {
            return Ok(TypeRef { name, shape: Shape::One, span });
        }
        if let TokenKind::RBracket = self.peek().kind {
            let close = self.bump().span;
            return Ok(TypeRef { name, shape: Shape::List, span: span.to(close) });
        }
        let token = self.peek();
        let TokenKind::Int(len) = token.kind else {
            return Err(Diagnostic::new(
                format!("expected an array length, found {}", token.kind.describe()),
                token.span,
            )
            .with_label("a length has to be written out here")
            .with_note("`int[3]` is an array of three ints, and `int[]` a list of them", None));
        };
        let len_span = self.bump().span;
        let close = self.expect(TokenKind::RBracket)?.span;
        Ok(TypeRef { name, shape: Shape::Array(len, len_span), span: span.to(close) })
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
        if Prim::of_keyword(&self.peek().kind).is_some() {
            return true;
        }
        match self.peek().kind {
            TokenKind::Ident(_) => match self.peek_at(1).kind {
                TokenKind::Ident(_) => true,
                TokenKind::LBracket => match self.peek_at(2).kind {
                    // `Colour[] cs` — nothing between the brackets, so the
                    // third token settles it: an index always has something.
                    TokenKind::RBracket => matches!(self.peek_at(3).kind, TokenKind::Ident(_)),
                    TokenKind::Int(_) => {
                        matches!(self.peek_at(3).kind, TokenKind::RBracket)
                            && matches!(self.peek_at(4).kind, TokenKind::Ident(_))
                    }
                    _ => false,
                },
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
                // `Circle(int)` — what this variant carries, if anything. A
                // variant without parentheses carries nothing, which is every
                // variant TinyC had before.
                let mut payload = Vec::new();
                if self.eat(&TokenKind::LParen) {
                    loop {
                        payload.push(self.expect_type("a variant's payload")?);
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                    self.expect_closing(TokenKind::RParen, name_span, "expected `)` here")?;
                }
                variants.push(Variant { name, name_span, payload });
                if !self.eat(&TokenKind::Comma) {
                    break;
                }
                // A trailing comma is allowed here and nowhere else in the
                // language, because this is the one list that is normally
                // written down the page rather than across it — a variant with
                // a payload is long enough that a per-line enum is the usual
                // shape, and then the last line should look like the others.
                if matches!(self.peek().kind, TokenKind::RBrace) {
                    break;
                }
            }
        }

        self.expect_closing(TokenKind::RBrace, open, "a variant list ends here")?;
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
            TokenKind::KwPrint | TokenKind::KwPrintln => self.print_stmt(),
            TokenKind::KwPush => self.push_stmt(),
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
                TokenKind::Eof => return Err(self.unclosed("match", open)),
                _ => arms.push(self.match_arm()?),
            }
        }
    }

    /// `Color::Red => "warm",` or `3 => "three",` or `_ => { ... }`
    ///
    /// The token after `=>` decides between a value and a block, with no
    /// lookahead beyond it: a `{` can only open a block, because TinyC has no
    /// other use for one in expression position.
    fn match_arm(&mut self) -> PResult<MatchArm> {
        let (pattern, span) = self.pattern()?;
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
        Ok(MatchArm { pattern, span, body })
    }

    /// What an arm matches, and where it was written.
    ///
    /// Every shape here is settled by one token, or by two for a qualified
    /// variant. There is nothing to disambiguate: a pattern is a value, so it
    /// is spelled exactly as the value would be, and `_` is the one spelling
    /// that is not a value at all.
    ///
    /// Which of these the scrutinee *admits* is not a syntactic question — an
    /// `int` takes `3` and not `Color::Red` — so the parser accepts them all
    /// and [`crate::sema`] holds each to the type it is matching.
    fn pattern(&mut self) -> PResult<(Pattern, Span)> {
        let token = self.peek();
        let span = token.span;
        let pattern = match &token.kind {
            TokenKind::Int(v) => Pattern::Int(*v),
            TokenKind::Float(v) => Pattern::Float(*v),
            TokenKind::Char(c) => Pattern::Char(*c),
            TokenKind::Str(lit) => Pattern::Str(lit.chars.clone()),
            TokenKind::Bool(v) => Pattern::Bool(*v),
            // A negative number is two tokens everywhere else in the grammar
            // too; here there is no expression to fold it into, so it is read
            // as part of the literal.
            TokenKind::Minus => {
                self.bump();
                let token = self.peek();
                let TokenKind::Int(v) = token.kind else {
                    return Err(Diagnostic::new(
                        format!("expected a number, found {}", token.kind.describe()),
                        token.span,
                    )
                    .with_label("a `-` in a pattern has to be part of a number"));
                };
                let end = self.bump().span;
                // `-9223372036854775808` is the one value whose positive half
                // does not fit, and the lexer has already refused anything
                // bigger — so this cannot overflow.
                return Ok((Pattern::Int(-v), span.to(end)));
            }
            TokenKind::Ident(name) if name == WILDCARD => Pattern::Wildcard,
            TokenKind::Ident(name) => {
                let name = name.clone();
                let enum_span = self.bump().span;
                self.expect(TokenKind::ColonColon)?;
                let (variant, variant_span) = self.expect_ident("a variant name")?;
                // `Shape::Circle(r)` — the names this arm gives what the
                // variant carries. Names, not patterns: TinyC does not nest
                // one pattern inside another, so what is between the
                // parentheses is exactly a list of new variables.
                let mut bindings = Vec::new();
                let mut end = variant_span;
                if self.eat(&TokenKind::LParen) {
                    loop {
                        bindings.push(self.expect_ident("a name for what it carries")?);
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                    end = self.expect_closing(TokenKind::RParen, enum_span, "expected `)` here")?;
                }
                return Ok((
                    Pattern::Variant {
                        enum_name: name,
                        enum_span,
                        variant,
                        variant_span,
                        bindings,
                    },
                    enum_span.to(end),
                ));
            }
            other => {
                return Err(Diagnostic::new(
                    format!("expected a pattern, found {}", other.describe()),
                    span,
                )
                .with_label("expected a variant, a literal or `_`"));
            }
        };
        self.bump();
        Ok((pattern, span))
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
                    TokenKind::Eof => return Err(p.unclosed("block", open)),
                    _ => {
                        let (mark, depth) = (p.pos, p.depth);
                        match p.stmt() {
                            Ok(stmt) => stmts.push(stmt),
                            Err(error) => {
                                p.errors.push(error);
                                // The levels the failed statement charged are
                                // released by giving the counter back what it
                                // held before, since nothing on that path will
                                // release them itself.
                                p.depth = depth;
                                p.recover_to_statement();
                                p.ensure_progress(mark);
                            }
                        }
                    }
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
            // so the AST needs no separate "else if" shape. That synthetic
            // block is real nesting, and is counted: a chain of a thousand
            // `else if`s is a tree a thousand deep, whatever it looks like on
            // the page.
            if matches!(self.peek().kind, TokenKind::KwIf) {
                let nested = self.nested(Self::if_stmt)?;
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

    /// `print(...)` and `println(...)`, split into what they write.
    ///
    /// The first argument decides how the rest are read. A **string literal**
    /// there is a *format*: its `%`s are specifiers, and exactly one value must
    /// follow for each. Anything else is a single value to write, which is the
    /// whole of the one-argument form this statement used to have.
    ///
    /// Splitting it here, once, is the design. Nothing at run time ever reads a
    /// `%`, so every mistake in a format — an unknown letter, a missing value,
    /// a value of the wrong type — is a compile error with a column; and the C
    /// `printf` underneath is never handed text the program wrote, which is the
    /// other half of the same decision.
    fn print_stmt(&mut self) -> PResult<Stmt> {
        let keyword = self.bump();
        let span = keyword.span;
        let newline = keyword.kind == TokenKind::KwPrintln;
        let open = self.expect(TokenKind::LParen)?.span;

        // Asked before the literal is reached, not as a guard after it: an
        // empty argument list is `print()`, and a format that happens to be the
        // only argument is not.
        let parts = if self.peek().kind == TokenKind::RParen {
            // `println()` writes nothing but the line ending, and `print()`
            // writes nothing at all. One rule rather than two, and a blank line
            // is what the first is for.
            Vec::new()
        } else if let Some((format, at)) = self.format_literal() {
            self.formatted(format, at)?
        } else {
            let value = self.expr()?;
            if self.peek().kind == TokenKind::Comma {
                return Err(
                    Diagnostic::new("a format has to be written out here", value.span)
                        .with_label("this is not a string literal")
                        .with_note(
                            "only a literal can be checked against the values after it, \
                             so only a literal may be a format",
                            None,
                        ),
                );
            }
            vec![PrintPart::Value(value)]
        };

        self.expect_closing_paren(open)?;
        self.expect(TokenKind::Semi)?;
        Ok(Stmt::Print { span, newline, parts })
    }

    /// The string literal in first position, if that is what is there.
    ///
    /// A literal *followed by an operator* is not one: `print("a" + s)` joins
    /// two strings and writes the answer, and only a literal that is the whole
    /// argument can be a format. So the token after it has to end the argument.
    fn format_literal(&mut self) -> Option<(StrLit, Span)> {
        let TokenKind::Str(lit) = &self.peek().kind else { return None };
        if !matches!(self.peek_at(1).kind, TokenKind::Comma | TokenKind::RParen) {
            return None;
        }
        let lit = lit.clone();
        // The token span, quotes included: a note about the format as a whole
        // should underline the whole of it, and the offsets inside a literal
        // start after the opening quote.
        Some((lit, self.bump().span))
    }

    /// A format string and the values that fill it, paired up.
    fn formatted(&mut self, format: StrLit, at: Span) -> PResult<Vec<PrintPart>> {
        let pieces = split_format(&format)?;

        let mut values = Vec::new();
        while self.eat(&TokenKind::Comma) {
            values.push(self.expr()?);
        }

        let wanted = pieces.iter().filter(|piece| matches!(piece, Piece::Spec(..))).count();
        if values.len() > wanted {
            // The first value with nothing to write it is the one to point at:
            // the ones before it are accounted for, whatever went wrong.
            return Err(Diagnostic::new(
                "too many values for this format",
                values[wanted].span,
            )
            .with_label("no specifier is left for this one")
            .with_note(
                format!("the format has {}", count(wanted, "specifier", "specifiers")),
                Some(at),
            ));
        }

        let mut values = values.into_iter();
        let mut parts = Vec::new();
        for piece in pieces {
            match piece {
                Piece::Text(text) => parts.push(PrintPart::Text(text)),
                Piece::Spec(spec, at) => match values.next() {
                    Some(expr) => parts.push(PrintPart::Spec { spec, span: at, expr }),
                    None => {
                        return Err(Diagnostic::new("too few values for this format", at)
                            .with_label("nothing was given for this one")
                            .with_note(
                                format!(
                                    "the format asks for {}",
                                    count(wanted, "value", "values")
                                ),
                                None,
                            ));
                    }
                },
            }
        }
        Ok(parts)
    }

    /// `push(xs, value);`
    ///
    /// The first argument is parsed as an expression and then required to be a
    /// place, which is how the assignment statement does it too — the two are
    /// indistinguishable until the shape of the whole statement is known, and
    /// `into_place` is where the demand is made.
    fn push_stmt(&mut self) -> PResult<Stmt> {
        let span = self.bump().span;
        let open = self.expect(TokenKind::LParen)?.span;
        let target = Self::into_place(self.primary()?)?;
        self.expect(TokenKind::Comma)?;
        let value = self.expr()?;
        self.expect_closing_paren(open)?;
        self.expect(TokenKind::Semi)?;
        Ok(Stmt::Push { span, target, value })
    }

    /// `||` binds loosest of all, so `a < 1 || b < 2` is one disjunction of two
    /// comparisons rather than a comparison against a disjunction.
    fn expr(&mut self) -> PResult<Expr> {
        self.chain(Self::and, &[(TokenKind::PipePipe, Operator::Logic(LogicOp::Or))])
    }

    /// `&&` binds tighter than `||`, so `a || b && c` is `a || (b && c)`.
    fn and(&mut self) -> PResult<Expr> {
        self.chain(Self::comparison, &[(TokenKind::AmpAmp, Operator::Logic(LogicOp::And))])
    }

    /// Comparisons bind looser than arithmetic, so `a + 1 < b * 2` compares the
    /// two sums.
    fn comparison(&mut self) -> PResult<Expr> {
        self.chain(
            Self::sum,
            &[
                (TokenKind::EqEq, Operator::Compare(CmpOp::Eq)),
                (TokenKind::BangEq, Operator::Compare(CmpOp::Ne)),
                (TokenKind::Lt, Operator::Compare(CmpOp::Lt)),
                (TokenKind::Le, Operator::Compare(CmpOp::Le)),
                (TokenKind::Gt, Operator::Compare(CmpOp::Gt)),
                (TokenKind::Ge, Operator::Compare(CmpOp::Ge)),
            ],
        )
    }

    fn sum(&mut self) -> PResult<Expr> {
        self.chain(
            Self::term,
            &[
                (TokenKind::Plus, Operator::Arith(BinOp::Add)),
                (TokenKind::Minus, Operator::Arith(BinOp::Sub)),
            ],
        )
    }

    fn term(&mut self) -> PResult<Expr> {
        self.chain(
            Self::unary,
            &[
                (TokenKind::Star, Operator::Arith(BinOp::Mul)),
                (TokenKind::Slash, Operator::Arith(BinOp::Div)),
                (TokenKind::Percent, Operator::Arith(BinOp::Rem)),
            ],
        )
    }

    /// `operand (op operand)*` — one precedence level, associating to the left.
    ///
    /// All five levels have this shape and differ only in what they take for an
    /// operand and which operators they accept, so they are one loop written
    /// once. The nesting is charged here rather than by [`Self::nested`],
    /// because the chain leans left: every operator puts one more node *on top*
    /// of everything parsed so far, exactly as a recursive call would.
    fn chain(
        &mut self,
        operand: fn(&mut Self) -> PResult<Expr>,
        operators: &[(TokenKind, Operator)],
    ) -> PResult<Expr> {
        let mut lhs = operand(self)?;
        let mut levels = 0;
        loop {
            let found = operators.iter().find(|(token, _)| *token == self.peek().kind);
            let Some(&(_, op)) = found else {
                self.release(levels);
                return Ok(lhs);
            };
            self.bump();
            self.deepen()?;
            levels += 1;
            let rhs = operand(self)?;
            lhs = self.binary(op, lhs, rhs);
        }
    }

    /// One node of a chain, whichever of the three shapes its operator builds.
    fn binary(&mut self, op: Operator, lhs: Expr, rhs: Expr) -> Expr {
        let span = lhs.span.to(rhs.span);
        let (lhs, rhs) = (Box::new(lhs), Box::new(rhs));
        let kind = match op {
            Operator::Arith(op) => ExprKind::Bin { op, lhs, rhs },
            Operator::Compare(op) => ExprKind::Cmp { op, lhs, rhs },
            Operator::Logic(op) => ExprKind::Logic { op, lhs, rhs },
        };
        Expr { id: self.node_id(), span, kind }
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
    ///
    /// The chain leans left exactly as an operator chain does, so its nesting
    /// is charged the same way — see [`Self::chain`].
    fn primary(&mut self) -> PResult<Expr> {
        let mut expr = self.atom()?;
        let mut levels = 0;
        loop {
            if !matches!(self.peek().kind, TokenKind::Dot | TokenKind::LBracket) {
                self.release(levels);
                return Ok(expr);
            }
            self.deepen()?;
            levels += 1;
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
            } else {
                self.bump(); // `[`
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
            }
        }
    }

    fn atom(&mut self) -> PResult<Expr> {
        let token = self.peek();
        let span = token.span;
        let kind = match &token.kind {
            TokenKind::Int(v) => ExprKind::Int(*v),
            TokenKind::Float(v) => ExprKind::Float(*v),
            TokenKind::Str(lit) => ExprKind::Str(lit.chars.clone()),
            TokenKind::Char(c) => ExprKind::Char(*c),
            TokenKind::Bool(v) => ExprKind::Bool(*v),
            // `int(c)` — a conversion, written as the type it produces. A type
            // keyword in expression position can be nothing else: a declaration
            // is a *statement*, and statements are told apart before this point.
            kind if Prim::of_keyword(kind).is_some() => {
                let to = Prim::of_keyword(kind).expect("just matched");
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
                    // `Shape::Circle(5)` — what the variant is given. Written
                    // exactly like a call, and told apart from one by the `::`
                    // that had to come first.
                    let mut args = Vec::new();
                    let mut end = variant_span;
                    if self.eat(&TokenKind::LParen) {
                        args = self.call_args()?;
                        end = self.expect_closing_paren(span)?;
                    }
                    return Ok(Expr {
                        id: self.node_id(),
                        span: span.to(end),
                        kind: ExprKind::Variant {
                            enum_name: name,
                            enum_span: span,
                            variant,
                            variant_span,
                            args,
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
        let close = self.expect_closing(TokenKind::RBrace, open, "an object literal ends here")?;
        let span = class_span.to(close);
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
        let close = self.expect_closing(TokenKind::RBracket, open, "an array literal ends here")?;
        let span = open.to(close);
        Ok(Expr { id: self.node_id(), span, kind: ExprKind::Array { elements, span } })
    }

    fn expect_closing_paren(&mut self, open: Span) -> PResult<Span> {
        self.expect_closing(TokenKind::RParen, open, "expected `)` here")
    }

    /// Consume the bracket that ends a construct, or say what it was meant to
    /// close.
    ///
    /// Four constructs end in a bracket and all four report the same way, so
    /// the shape is written once: what differs is which bracket and how the
    /// construct is named, and `label` is the only part a reader of the message
    /// would notice.
    fn expect_closing(&mut self, kind: TokenKind, open: Span, label: &str) -> PResult<Span> {
        if self.eat(&kind) {
            return Ok(self.tokens[self.pos - 1].span);
        }
        let found = self.peek();
        Err(Diagnostic::new(
            format!("expected `{}`, found {}", kind.text(), found.kind.describe()),
            found.span,
        )
        .with_label(label)
        .with_note(format!("to close this `{}`", opening_of(&kind)), Some(open)))
    }

    /// The end of the file arrived before the `}` that should have ended
    /// `what` — the other way a braced construct can run on, and the one where
    /// naming a token found instead would say nothing.
    fn unclosed(&self, what: &str, open: Span) -> Diagnostic {
        Diagnostic::new(format!("unclosed {what}"), self.peek().span)
            .with_label("expected `}` before the end of the file")
            .with_note("to close this `{`", Some(open))
    }
}

/// The bracket a closing one matches, for "to close this `{`".
fn opening_of(closing: &TokenKind) -> &'static str {
    match closing {
        TokenKind::RParen => "(",
        TokenKind::RBrace => "{",
        TokenKind::RBracket => "[",
        other => unreachable!("`{}` closes nothing", other.text()),
    }
}

/// A format string's pieces, before the values are attached to them.
enum Piece {
    Text(Vec<char>),
    Spec(Spec, Span),
}

/// Split a format string into the text and the specifiers it is made of.
///
/// Every span cut here points into the *source*, not into the decoded
/// characters — which is why a literal keeps the offset of each of its
/// characters. An escape earlier in the string has already turned two
/// characters of source into one by the time a `%d` later in it is reached, so
/// counting from the opening quote would land in the wrong column.
fn split_format(lit: &StrLit) -> PResult<Vec<Piece>> {
    let mut pieces = Vec::new();
    let mut text: Vec<char> = Vec::new();
    let mut at = 0;
    while at < lit.chars.len() {
        if lit.chars[at] != '%' {
            text.push(lit.chars[at]);
            at += 1;
            continue;
        }
        let Some(&letter) = lit.chars.get(at + 1) else {
            return Err(Diagnostic::new("unfinished specifier", lit.span(at, at + 1))
                .with_label("a `%` at the end of a format writes nothing")
                .with_note("write `%%` for a percent sign", None));
        };
        let span = lit.span(at, at + 2);
        at += 2;
        // `%%` is how a format says a percent sign, and it is the reason a `%`
        // on its own has to be refused rather than written out: a format that
        // quietly printed `%d` when the value was forgotten would be exactly
        // the kind of silence this language does not keep.
        if letter == '%' {
            text.push('%');
            continue;
        }
        let Some(spec) = Spec::from_letter(letter) else {
            return Err(Diagnostic::new(
                format!("unknown specifier `%{}`", letter.escape_debug()),
                span,
            )
            .with_label("this writes nothing")
            .with_note(spec_list(), None));
        };
        if !text.is_empty() {
            pieces.push(Piece::Text(std::mem::take(&mut text)));
        }
        pieces.push(Piece::Spec(spec, span));
    }
    if !text.is_empty() {
        pieces.push(Piece::Text(text));
    }
    Ok(pieces)
}

/// The specifiers a format may use, built from the list itself so that adding
/// one cannot leave this note behind.
fn spec_list() -> String {
    let each: Vec<String> =
        ast::SPECS.iter().map(|s| format!("`%{}` for {}", s.letter(), s.writes())).collect();
    format!("the specifiers are {}, and `%%` for a percent sign", each.join(", "))
}

/// `1 specifier` but `2 specifiers` — a count a message can read out loud.
fn count(n: usize, one: &str, many: &str) -> String {
    match n {
        1 => format!("1 {one}"),
        _ => format!("{n} {many}"),
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

    // -- carrying on after a mistake ---------------------------------------

    /// Every mistake in one pass, rather than one recompile each.
    ///
    /// The parser cannot *continue* through a mistake — it has no idea what the
    /// program meant — so it throws tokens away until it reaches somewhere a
    /// statement could start. What that buys is the difference between fixing
    /// four things and fixing one thing four times.
    #[test]
    fn a_forgotten_semicolon_does_not_hide_the_mistakes_after_it() {
        let found = errors_in_main("int x = 1\n  int y = 2\n  int z = 3\n  println(x);");
        assert_eq!(found.len(), 3, "{found:#?}");
        assert!(found.iter().all(|d| d.message.contains("expected `;`")), "{found:#?}");
        assert!(found.windows(2).all(|p| p[0].span.offset < p[1].span.offset), "{found:#?}");
    }

    #[test]
    fn a_mistake_in_one_function_does_not_hide_one_in_the_next() {
        let src = "fn a() {\n  int x = 1\n}\nfn b() {\n  int y = 2\n}\nfn main() {\n  a();\n}\n";
        let found = parse_src(src).unwrap_err();
        assert_eq!(found.len(), 2, "{found:#?}");
    }

    /// The guard that keeps one mistake from becoming a message per line.
    #[test]
    fn a_missing_brace_is_not_reported_once_per_line_after_it() {
        // Everything after the unclosed block is swallowed, because the brace
        // count never comes back to zero and so no `fn` is ever reached.
        let src = "fn a() {\n  println(1);\n\nfn b() {\n  println(2);\n}\n";
        let found = parse_src(src).unwrap_err();
        assert!(found.len() <= 3, "one mistake should not cascade: {found:#?}");
        assert!(found.iter().any(|d| d.message.contains("unclosed")), "{found:#?}");
    }

    /// Recovery must always leave the parser somewhere new, or it would read
    /// the same token, fail the same way, and never finish.
    #[test]
    fn nothing_a_program_can_be_makes_the_parser_loop() {
        for src in [
            "}",
            "}}}}",
            "fn",
            "fn main( {",
            "fn main() { ) ) ) }",
            "fn main() { int = ; }",
            "fn main() { if }",
            ";;;;;;",
            "= = = =",
            "fn main() { x = ; y = ; z = ; }",
        ] {
            let found = parse_src(src).unwrap_err();
            assert!(!found.is_empty(), "`{src}` should be refused");
        }
    }

    /// A statement that failed part way through charged levels of nesting that
    /// nothing on its path releases. Recovery puts the counter back, or a file
    /// with a few mistakes near the top would be refused for nesting too deeply
    /// somewhere near the bottom.
    #[test]
    fn recovery_gives_back_the_nesting_a_failed_statement_charged() {
        let mistake = "int x = ((((1 + 2\n";
        let src = format!("fn main() {{\n{}  println(1);\n}}\n", mistake.repeat(200));
        let found = crate::with_compiler_stack(|| parse_src(&src)).unwrap_err();
        assert!(
            found.iter().all(|d| !d.message.contains("nests too deeply")),
            "the levels were not given back: {:#?}",
            &found[..found.len().min(3)]
        );
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

    /// A left-leaning chain looks flat and is not: `1 + 1 + 1` is a tree three
    /// deep, and every later pass walks it recursively. Counting only the
    /// parser's own recursion missed these entirely, and a long enough one was
    /// a stack overflow rather than a diagnostic.
    #[test]
    fn the_limit_counts_a_chain_of_operators() {
        let levels = MAX_NESTING as usize + 8;
        for chain in ["1", "x", "x == x", "x && x"].map(|term| format!(" + {term}")) {
            let source =
                format!("fn main() {{ int x = 1; print(1{}); }}", chain.repeat(levels));
            let errors = parse_deep(&source).unwrap_err();
            assert!(
                errors[0].message.contains("nests too deeply"),
                "{chain}: {:?}",
                errors[0]
            );
        }
    }

    #[test]
    fn the_limit_counts_a_chain_of_field_accesses() {
        // `.a.a.a…` leans left exactly as `+` does, through the postfix loop.
        let source =
            format!("fn main() {{ print(x{}); }}", ".a".repeat(MAX_NESTING as usize + 8));
        let errors = parse_deep(&source).unwrap_err();
        assert!(errors[0].message.contains("nests too deeply"), "{:?}", errors[0]);
    }

    /// An `else if` chain reads as flat and nests as deeply as it is long: each
    /// one goes in a synthetic block inside the previous `else`.
    #[test]
    fn the_limit_counts_an_else_if_chain() {
        let levels = MAX_NESTING as usize + 8;
        let source = format!(
            "fn main() {{ if (true) {{ }} {} }}",
            "else if (true) { } ".repeat(levels)
        );
        let errors = parse_deep(&source).unwrap_err();
        assert!(errors[0].message.contains("nests too deeply"), "{:?}", errors[0]);
    }

    #[test]
    fn a_chain_that_stays_under_the_limit_still_parses() {
        // The limit is on depth, not on length: an array literal is as wide as
        // it likes, because its elements are siblings rather than a chain.
        let terms = vec!["1"; MAX_NESTING as usize - 8].join(" + ");
        assert!(parse_deep(&format!("fn main() {{ print({terms}); }}")).is_ok());

        let elements = vec!["1"; 1000].join(", ");
        assert!(parse_deep(&format!("fn main() {{ int[1000] xs = [{elements}]; }}")).is_ok());
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

    /// The one list in the language that takes a trailing comma.
    ///
    /// It earns it by being the one normally written down the page rather than
    /// across it: a variant with a payload is long enough that a line each is
    /// the usual shape, and then the last line should look like the others.
    #[test]
    fn a_variant_list_may_end_with_a_comma() {
        let program = parse_src("enum Color { Red, Green, }\nfn main() {\n}\n").unwrap();
        assert_eq!(program.enums[0].variants.len(), 2);
        // Two in a row is still a variant that is not there.
        let errors = parse_src("enum Color { Red,, }\nfn main() {\n}").unwrap_err();
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

    // -- format strings ----------------------------------------------------

    /// The one message per mistake, and the column each lands in.
    fn format_error(body: &str) -> (String, usize) {
        let errors = errors_in_main(body);
        assert_eq!(errors.len(), 1, "one message per mistake: {errors:?}");
        // The body starts on line 2 at column 1, so the offset into the whole
        // source is the column, less the newline that opened the body.
        (errors[0].message.clone(), errors[0].span.offset as usize - "fn main() {\n".len())
    }

    #[test]
    fn a_format_splits_into_the_text_and_the_specifiers_around_it() {
        assert_eq!(
            dump_main("int n = 1;\nprintln(\"a %d b\", n);"),
            "decl int n\n  int 1\nprintln\n  text \"a \"\n  %d\n    var n\n  text \" b\"\n"
        );
    }

    /// A format needs no text around its specifiers, and no specifiers in its
    /// text. Both ends of the range parse.
    #[test]
    fn a_format_may_be_all_text_or_all_specifier() {
        assert_eq!(dump_main("println(\"hi\");"), "println\n  text \"hi\"\n");
        assert_eq!(
            dump_main("int n = 1;\nprintln(\"%d\", n);"),
            "decl int n\n  int 1\nprintln\n  %d\n    var n\n"
        );
    }

    /// `%%` is one `%` in the text, and the text around it is not split in two:
    /// what a piece of text *is* is a run with no specifier in it.
    #[test]
    fn a_doubled_percent_is_one_character_of_text() {
        assert_eq!(dump_main("println(\"100%% sure\");"), "println\n  text \"100% sure\"\n");
    }

    /// Every specifier the language has, checked here rather than one test per
    /// letter: a new one that the splitter forgot would show up as a parse
    /// error rather than as a wrong tree.
    #[test]
    fn every_specifier_is_recognised() {
        for spec in ast::SPECS {
            let body = format!("println(\"%{}\", 1);", spec.letter());
            let parsed = parse_src(&format!("fn main() {{\n{body}\n}}\n"));
            assert!(parsed.is_ok(), "`%{}` did not parse", spec.letter());
        }
    }

    #[test]
    fn a_letter_that_is_not_a_specifier_is_refused() {
        let (message, at) = format_error("println(\"a %q b\", 1);");
        assert_eq!(message, "unknown specifier `%q`");
        assert_eq!(at, "println(\"a ".len(), "the caret is on the `%q`");
    }

    #[test]
    fn a_percent_with_nothing_after_it_is_refused() {
        let (message, _) = format_error("println(\"100%\");");
        assert_eq!(message, "unfinished specifier");
    }

    /// The point of keeping a byte offset per character rather than counting
    /// them: `\n` is two characters of source and one of text, so by the `%d`
    /// the two have drifted apart. Counting would land the caret one column
    /// early.
    #[test]
    fn an_escape_before_a_specifier_does_not_shift_the_caret() {
        let (_, at) = format_error("println(\"a\\nb %q\", 1);");
        assert_eq!(at, "println(\"a\\nb ".len());
    }

    #[test]
    fn a_format_with_more_specifiers_than_values_is_refused() {
        let (message, at) = format_error("println(\"%d and %d\", 1);");
        assert_eq!(message, "too few values for this format");
        assert_eq!(at, "println(\"%d and ".len(), "the caret is on the one left over");
    }

    #[test]
    fn a_format_with_more_values_than_specifiers_is_refused() {
        let (message, at) = format_error("println(\"%d\", 1, 2);");
        assert_eq!(message, "too many values for this format");
        assert_eq!(at, "println(\"%d\", 1, ".len(), "the caret is on the spare value");
    }

    /// A literal is the only thing that can be a format, because it is the only
    /// thing whose `%`s are known while the program is being compiled.
    #[test]
    fn only_a_literal_may_be_a_format() {
        let (message, _) = format_error("string f = \"%d\";\nprintln(f, 1);");
        assert_eq!(message, "a format has to be written out here");
    }

    /// A literal *joined to something* is an expression, not a format — so it
    /// is written as one value and its `%` is just a character.
    #[test]
    fn a_literal_that_is_not_the_whole_argument_is_an_expression() {
        assert_eq!(
            dump_main("string s = \"b\";\nprintln(\"a%\" + s);"),
            "decl string s\n  string \"b\"\nprintln\n  +\n    string \"a%\"\n    var s\n"
        );
    }

    /// `print` and `println` are one statement with one difference, and the
    /// tree says which was written. Nothing else about them differs here.
    #[test]
    fn the_two_spellings_parse_to_the_same_shape() {
        assert_eq!(dump_main("print(1);"), "print\n  int 1\n");
        assert_eq!(dump_main("println(1);"), "println\n  int 1\n");
    }

    /// Writing nothing is allowed, and is how a blank line is written. `print()`
    /// is the same rule reaching its uninteresting end rather than a second one.
    #[test]
    fn writing_nothing_is_a_statement() {
        assert_eq!(dump_main("println();"), "println\n");
        assert_eq!(dump_main("print();"), "print\n");
    }
}
