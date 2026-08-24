//! The contracts that belong to the pipeline as a whole rather than to any one
//! stage: how far it will go before giving up, and how it can be stopped early.
//!
//! Everything here goes through `tinyc::compile` or `tinyc::compile_with`, so a
//! stage that started recursing further, or one quietly reordered, is caught
//! here and not by reading any single module.

use tinyc::codegen::Target;
use tinyc::parser::MAX_NESTING;
use tinyc::{Options, Stage, compile, compile_with, with_compiler_stack};

/// Run the whole pipeline on the stack the CLI gives it.
///
/// [`tinyc::STACK_SIZE`] and [`MAX_NESTING`] are one bargain: the limit is
/// what keeps the recursion finite, and the stack is what makes the permitted
/// depth affordable. Neither is worth much without the other, which is why
/// these tests run on the same thread the CLI would.
fn compiles(src: &str) -> bool {
    with_compiler_stack(|| compile(src, Target::X86_64Windows)).is_ok()
}

fn refused_for_depth(src: &str) -> bool {
    match with_compiler_stack(|| compile(src, Target::X86_64Windows)) {
        Ok(_) => false,
        Err(errors) => errors[0].message.contains("nests too deeply"),
    }
}

/// The shapes that nest, each written as deep as `levels` asks.
///
/// The last three lean *left*, and look flat on the page: they are the ones a
/// parser counting only its own recursion misses, and each was a stack overflow
/// rather than a diagnostic until the count was made about the tree instead.
fn nested(levels: usize) -> Vec<(&'static str, String)> {
    vec![
        ("parentheses", format!("fn main() {{ println({}1{}); }}", "(".repeat(levels), ")".repeat(levels))),
        ("unary operators", format!("fn main() {{ println({}1); }}", "-".repeat(levels))),
        (
            "blocks",
            format!("fn main() {{ {}{} }}", "if (true) { ".repeat(levels), "}".repeat(levels)),
        ),
        ("a chain of operators", format!("fn main() {{ println(1{}); }}", " + 1".repeat(levels))),
        (
            "a chain of short circuits",
            format!("fn main() {{ println(true{}); }}", " && true".repeat(levels)),
        ),
        (
            "an else-if chain",
            format!(
                "fn main() {{ if (true) {{ }} {} }}",
                "else if (true) { } ".repeat(levels)
            ),
        ),
    ]
}

/// Nesting the compiler accepts, it has to survive — every stage of it.
///
/// Parsing a deep program is the easy half. `sema` walks the same tree, `ir`
/// lowers it, and dropping it recurses too, so the limit only means anything
/// if the whole pipeline fits in the stack the limit was chosen for.
#[test]
fn nesting_up_to_the_limit_survives_every_stage() {
    // Just under: a few of these charge a level for the operand as well as for
    // the operator, so the margin is what keeps the test about the stack rather
    // than about the exact accounting.
    let levels = MAX_NESTING as usize - 8;
    for (what, source) in nested(levels) {
        assert!(compiles(&source), "{what}, {levels} deep, should compile");
    }
}

/// Past the limit, every shape is a diagnostic — never a crash.
///
/// The distinction is the whole point: a compiler that overflows its stack
/// tells the reader nothing about their program, and cannot be caught.
#[test]
fn nesting_past_the_limit_is_refused_in_words() {
    for (what, source) in nested(MAX_NESTING as usize + 8) {
        assert!(refused_for_depth(&source), "{what} past the limit should be refused");
    }
}

/// A chain of *thousands*, which is what a generated program looks like.
///
/// Deep enough that no stack this compiler could reasonably ask for would hold
/// it, so the limit has to catch it long before the recursion does.
#[test]
fn a_chain_far_past_the_limit_is_still_a_diagnostic() {
    for (what, source) in nested(20_000) {
        assert!(refused_for_depth(&source), "{what}, twenty thousand deep, should be refused");
    }
}

/// Width is not depth, and only depth is limited.
///
/// An array literal's elements are siblings: they make the tree wide, and no
/// pass recurses once per element. Refusing them would be the limit answering a
/// question nobody asked.
#[test]
fn a_wide_program_is_not_a_deep_one() {
    let elements = vec!["1"; 1000].join(", ");
    let source = format!("fn main() {{ int[1000] xs = [{elements}];\n  println(xs[999]); }}");
    assert!(compiles(&source), "a thousand elements is width, not depth");

    let statements = "println(1);\n".repeat(2000);
    assert!(compiles(&format!("fn main() {{ {statements} }}")), "statements are siblings too");

    let functions: String =
        (0..500).map(|n| format!("fn f{n}() -> int {{ return {n}; }}\n")).collect();
    assert!(compiles(&format!("{functions}fn main() {{ println(f499()); }}")));
}

// -- stopping early --------------------------------------------------------

/// `--emit` stops the pipeline after a stage, and the stage order lives in
/// `compile_with` rather than in the CLI.
///
/// Answering `false` has to stop it *there*: a stage that ran anyway would make
/// `--emit tokens` on a program with a type error print the tokens and then
/// fail, which is the opposite of what asking for tokens means.
#[test]
fn an_observer_that_says_no_stops_the_pipeline_where_it_said_so() {
    // Lexes, parses, and does not type-check: `main` must take nothing.
    let source = "fn main(int a) {\n  println(a);\n}\n";

    let mut seen = Vec::new();
    let stopped = compile_with(source, Target::X86_64Windows, Options::default(), |stage| {
        seen.push(match stage {
            Stage::Tokens(_) => "tokens",
            Stage::Ast(_) => "ast",
            Stage::Ir(_) => "ir",
        });
        false
    });
    assert_eq!(seen, vec!["tokens"], "nothing after the first stage should have run");
    assert!(matches!(stopped, Ok(None)), "stopping early is not a failure");
}

#[test]
fn the_stages_arrive_in_the_order_the_pipeline_runs_them() {
    let source = "fn main() {\n  println(1 + 2);\n}\n";

    let mut seen = Vec::new();
    let compiled = compile_with(source, Target::X86_64Windows, Options::default(), |stage| {
        match stage {
            Stage::Tokens(tokens) => {
                seen.push("tokens");
                assert!(!tokens.is_empty());
            }
            Stage::Ast(ast) => {
                seen.push("ast");
                assert_eq!(ast.functions.len(), 1);
            }
            Stage::Ir(ir) => {
                seen.push("ir");
                assert_eq!(ir.functions.len(), 1);
            }
        }
        true
    });

    assert_eq!(seen, vec!["tokens", "ast", "ir"]);
    let compiled = compiled.expect("it should compile").expect("nothing stopped it");
    assert!(!compiled.asm.is_empty());
    assert_eq!(compiled.allocations.len(), compiled.ir.functions.len());
    assert_eq!(compiled.backend, "x86_64-windows");
}

/// A stage that fails is reported before any later one is shown.
///
/// The observer is not a way to see past an error: `--emit ir` on a program
/// that does not type-check has no IR to print, and has to say why rather than
/// print something.
#[test]
fn a_stage_that_fails_shows_nothing_after_it() {
    let source = "fn main() {\n  int x = \"not a number\";\n}\n";

    let mut seen = Vec::new();
    let result = compile_with(source, Target::X86_64Windows, Options::default(), |stage| {
        seen.push(match stage {
            Stage::Tokens(_) => "tokens",
            Stage::Ast(_) => "ast",
            Stage::Ir(_) => "ir",
        });
        true
    });

    assert_eq!(seen, vec!["tokens", "ast"], "the type checker is what refused");
    let Err(errors) = result else { panic!("a mistyped initialiser should be refused") };
    assert!(!errors.is_empty());
}

/// Every stage reports its failures the same way: a span into the source, so
/// the CLI can render any of them without knowing which stage it came from.
#[test]
fn a_failure_from_any_stage_carries_a_span_into_the_source() {
    let cases = [
        ("the lexer", "fn main() {\n  println(@);\n}\n"),
        ("the parser", "fn main() {\n  println(1\n}\n"),
        ("the type checker", "fn main() {\n  println(nope);\n}\n"),
    ];

    for (stage, source) in cases {
        let Err(errors) = compile(source, Target::X86_64Windows) else {
            panic!("{stage} should have refused this")
        };
        for error in &errors {
            let end = (error.span.offset + error.span.len) as usize;
            assert!(end <= source.len(), "{stage}: a span past the end of the file");
            assert!(!error.message.is_empty(), "{stage}: a diagnostic with no message");
        }
    }
}
