//! Command line driver: read a source file, run the pipeline, write assembly.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};

use tinyc::Stage;
use tinyc::codegen::Target;
use tinyc::diag::SourceFile;
use tinyc::ast;

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
    // The pipeline recurses with the nesting of the source, which needs more
    // stack than a thread gets by default.
    match tinyc::with_compiler_stack(|| run(&cli)) {
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

    let render = |errors: Vec<tinyc::diag::Diagnostic>| -> String {
        errors.iter().map(|d| source.render(d)).collect::<Vec<_>>().join("\n")
    };

    // `--emit` stops the pipeline after a stage by answering `false`; the order
    // of the stages themselves lives in `tinyc::compile_with`, not here.
    let compiled = tinyc::compile_with(source.text(), target, |stage| match (stage, cli.emit) {
        (Stage::Tokens(tokens), Emit::Tokens) => {
            for token in tokens {
                let (line, col) = source.line_col(token.span.offset);
                println!("{line:>3}:{col:<3} {:?}", token.kind);
            }
            false
        }
        (Stage::Ast(ast), Emit::Ast) => {
            print!("{}", ast::dump(ast));
            false
        }
        (Stage::Ir(ir), Emit::Ir) => {
            print!("{}", ir.dump());
            false
        }
        _ => true,
    })
    .map_err(render)?;

    let Some(compiled) = compiled else { return Ok(()) };

    if cli.dump_regalloc {
        for (function, allocation) in compiled.ir.functions.iter().zip(&compiled.allocations) {
            println!("{}:", function.signature());
            print!("{}", allocation.dump(function, &compiled.registers));
        }
    }

    let output = cli.output.clone().unwrap_or_else(|| cli.input.with_extension("asm"));
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("error: cannot create {}: {e}\n", parent.display()))?;
    }
    std::fs::write(&output, &compiled.asm)
        .map_err(|e| format!("error: cannot write {}: {e}\n", output.display()))?;

    println!("wrote {} ({})", output.display(), compiled.backend);
    Ok(())
}
