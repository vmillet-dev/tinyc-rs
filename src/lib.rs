//! TinyC — a small compiler for a tiny typed language.
//!
//! The pipeline is one module per stage:
//!
//! ```text
//! source text
//!   -> lexer   -> tokens          (lexer.rs, token.rs)
//!   -> parser  -> AST             (parser.rs, ast.rs)
//!   -> sema    -> types           (sema.rs)
//!   -> ir      -> three-address code (ir.rs)
//!   -> codegen -> assembly        (codegen/)
//! ```
//!
//! Every stage reports failures as [`diag::Diagnostic`]s carrying a source span,
//! which [`diag::SourceFile::render`] turns into a message with a line, a column
//! and a caret.

pub mod ast;
pub mod codegen;
pub mod diag;
pub mod ir;
pub mod lexer;
pub mod parser;
pub mod sema;
pub mod token;

use codegen::{Allocation, Target};

/// Everything the pipeline produced, for the CLI to print or write out.
pub struct Compiled {
    pub ir: ir::Program,
    pub allocation: Allocation,
    pub asm: String,
}

/// Run the whole pipeline. Errors carry spans into `source`.
pub fn compile(source: &str, target: Target) -> diag::Result<Compiled> {
    let tokens = lexer::lex(source)?;
    let ast = parser::parse(&tokens)?;
    let types = sema::check(&ast)?;
    let ir = ir::lower(&ast, &types);
    let (_, allocation, asm) = codegen::compile(&ir, target);
    Ok(Compiled { ir, allocation, asm })
}
