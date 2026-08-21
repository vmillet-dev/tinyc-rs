//! Command line driver: read a source file, run the pipeline, write assembly.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};

use tinyc::codegen::{self, Target};
use tinyc::diag::SourceFile;
use tinyc::{ast, ir, lexer, parser, sema};

#[derive(Parser)]
#[command(
    name = "tinyc",
    version,
    about = "Compile a TinyC source file to assembly",
    after_help = "Example:\n  tinyc examples/hello.tc -o out/hello.asm"
)]
struct Cli {
    /// TinyC source file to compile.
    input: PathBuf,

    /// Where to write the assembly (default: the input file with a .asm suffix).
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,

    /// Stop after a stage and print its result instead of writing assembly.
    #[arg(long, value_enum, default_value_t = Emit::Asm)]
    emit: Emit,

    /// Target to generate code for.
    #[arg(long, default_value = "x86_64-windows")]
    target: String,

    /// Print live intervals and register assignments.
    #[arg(long)]
    dump_regalloc: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Emit {
    Tokens,
    Ast,
    Ir,
    Asm,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprint!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: &Cli) -> Result<(), String> {
    let Some(target) = Target::from_name(&cli.target) else {
        return Err(format!(
            "error: unknown target `{}`\n  known targets: {}\n",
            cli.target,
            Target::names().join(", ")
        ));
    };

    let text = std::fs::read_to_string(&cli.input)
        .map_err(|e| format!("error: cannot read {}: {e}\n", cli.input.display()))?;
    let source = SourceFile::new(&cli.input, text);

    // Each stage is run explicitly here so that `--emit` can stop between any
    // two of them; `tinyc::compile` chains the same calls in one go.
    let render = |errors: Vec<tinyc::diag::Diagnostic>| -> String {
        errors.iter().map(|d| source.render(d)).collect::<Vec<_>>().join("\n")
    };

    let tokens = lexer::lex(source.text()).map_err(render)?;
    if cli.emit == Emit::Tokens {
        for token in &tokens {
            let (line, col) = source.line_col(token.span.offset);
            println!("{line:>3}:{col:<3} {:?}", token.kind);
        }
        return Ok(());
    }

    let ast = parser::parse(&tokens).map_err(render)?;
    if cli.emit == Emit::Ast {
        print!("{}", ast::dump(&ast));
        return Ok(());
    }

    let types = sema::check(&ast).map_err(render)?;
    let ir = ir::lower(&ast, &types);
    if cli.emit == Emit::Ir {
        print!("{}", ir.dump());
        return Ok(());
    }

    let (backend, allocations, asm) = codegen::compile(&ir, target);
    if cli.dump_regalloc {
        for (function, allocation) in ir.functions.iter().zip(&allocations) {
            println!("{}:", function.signature());
            print!("{}", allocation.dump(function, backend.register_file()));
        }
    }

    let output = cli.output.clone().unwrap_or_else(|| cli.input.with_extension("asm"));
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("error: cannot create {}: {e}\n", parent.display()))?;
    }
    std::fs::write(&output, asm)
        .map_err(|e| format!("error: cannot write {}: {e}\n", output.display()))?;

    println!("wrote {} ({})", output.display(), backend.name());
    Ok(())
}
