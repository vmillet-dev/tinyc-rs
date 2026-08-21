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

use codegen::{Allocation, RegisterFile, Target};

/// Stack the compiler needs to run on.
///
/// The parser, the type checker and the lowering all walk the tree with plain
/// recursion, so source nesting becomes call frames — several kilobytes of them
/// per level in a debug build. Windows hands a thread one megabyte by default,
/// which is not enough for [`parser::MAX_NESTING`] levels, so anything that runs
/// the pipeline should go through [`with_compiler_stack`].
pub const STACK_SIZE: usize = 32 * 1024 * 1024;

/// Run `job` on a thread with a stack sized for [`STACK_SIZE`].
///
/// Scoped, so `job` may borrow from its caller exactly as if it had been called
/// directly; a panic inside it is resumed here rather than swallowed.
pub fn with_compiler_stack<T: Send>(job: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .stack_size(STACK_SIZE)
            .spawn_scoped(scope, job)
            .expect("the compiler thread could not be started")
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

/// Everything the pipeline produced, for the CLI to print or write out.
pub struct Compiled {
    pub ir: ir::Program,
    pub allocations: Vec<Allocation>,
    pub asm: String,
    /// Name of the backend that produced `asm`.
    pub backend: &'static str,
    /// The register file that backend described, so `--dump-regalloc` can name
    /// the registers in `allocations` without building the backend again.
    pub registers: RegisterFile,
}

/// An intermediate result, on its way past [`compile_with`]'s observer.
pub enum Stage<'a> {
    Tokens(&'a [token::Token]),
    Ast(&'a ast::Program),
    Ir(&'a ir::Program),
}

/// Run the whole pipeline. Errors carry spans into `source`.
pub fn compile(source: &str, target: Target) -> diag::Result<Compiled> {
    let compiled = compile_with(source, target, |_| true)?;
    Ok(compiled.expect("nothing asked this pipeline to stop early"))
}

/// Run the pipeline, showing each intermediate result to `observe` as it is
/// produced. `observe` answers whether to carry on, which is what the CLI's
/// `--emit` uses to stop after a stage — and why there is only one copy of the
/// stage order in the whole compiler.
///
/// The answer is `None` exactly when `observe` stopped the run.
pub fn compile_with(
    source: &str,
    target: Target,
    mut observe: impl FnMut(Stage<'_>) -> bool,
) -> diag::Result<Option<Compiled>> {
    let backend = codegen::backend_for(target);

    let tokens = lexer::lex(source)?;
    if !observe(Stage::Tokens(&tokens)) {
        return Ok(None);
    }

    let ast = parser::parse(&tokens)?;
    if !observe(Stage::Ast(&ast)) {
        return Ok(None);
    }

    // How many arguments fit in registers is the target's business, not the
    // type checker's; the front end only asks.
    let types = sema::check(&ast, backend.register_file().max_args)?;
    let ir = ir::lower(&ast, &types);
    if !observe(Stage::Ir(&ir)) {
        return Ok(None);
    }

    let (allocations, asm) = codegen::compile(&ir, backend.as_ref());
    Ok(Some(Compiled {
        ir,
        allocations,
        asm,
        backend: backend.name(),
        registers: backend.register_file().clone(),
    }))
}
