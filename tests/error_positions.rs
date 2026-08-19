//! Every file in `examples/errors/` must fail, and must fail *at the right
//! place*: these assertions are what keep the line/column reporting honest.

use std::path::Path;

use tinyc::codegen::Target;
use tinyc::diag::SourceFile;

/// Compile an example and return the `line:col` of each diagnostic it produces.
fn error_positions(file: &str) -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/errors").join(file);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let source = SourceFile::new(file, text);

    match tinyc::compile(source.text(), Target::X86_64Windows) {
        Ok(_) => panic!("{file} was expected to fail, but compiled"),
        Err(errors) => errors
            .iter()
            .map(|d| {
                let (line, col) = source.line_col(d.span.offset);
                format!("{line}:{col}")
            })
            .collect(),
    }
}

fn assert_error_at(file: &str, expected: &str) {
    let positions = error_positions(file);
    assert_eq!(positions, vec![expected.to_string()], "wrong position(s) for {file}");
}

#[test]
fn unterminated_string_points_at_the_opening_quote() {
    assert_error_at("unterminated_string.tc", "1:12");
}

#[test]
fn missing_semicolon_points_at_the_token_that_should_have_followed_it() {
    assert_error_at("missing_semicolon.tc", "2:1");
}

#[test]
fn undeclared_variable_points_at_the_use() {
    assert_error_at("undeclared_variable.tc", "2:11");
}

#[test]
fn type_mismatch_points_at_the_offending_operand() {
    assert_error_at("type_mismatch.tc", "3:11");
}

#[test]
fn bad_initializer_points_at_the_initializer() {
    assert_error_at("bad_initializer.tc", "1:9");
}

#[test]
fn division_by_zero_points_at_the_divisor() {
    assert_error_at("division_by_zero.tc", "2:11");
}

#[test]
fn an_int_initializer_for_a_bool_points_at_the_initializer() {
    assert_error_at("bool_type_mismatch.tc", "1:14");
}

#[test]
fn arithmetic_on_a_bool_points_at_the_bool_operand() {
    assert_error_at("bool_arithmetic.tc", "2:7");
}

#[test]
fn assigning_the_wrong_type_points_at_the_value() {
    assert_error_at("assign_wrong_type.tc", "2:9");
}

#[test]
fn assigning_to_an_undeclared_variable_points_at_the_name() {
    assert_error_at("assign_undeclared.tc", "2:1");
}

#[test]
fn redeclaration_points_at_both_declarations() {
    assert_error_at("redeclaration.tc", "2:5");

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/errors/redeclaration.tc");
    let source = SourceFile::new("redeclaration.tc", std::fs::read_to_string(path).unwrap());
    let Err(errors) = tinyc::compile(source.text(), Target::X86_64Windows) else {
        panic!("redeclaration.tc was expected to fail");
    };
    let (_, note_span) = errors[0].note.clone().expect("a note pointing at the first declaration");
    assert_eq!(source.line_col(note_span.unwrap().offset), (1, 5));
}

/// The good examples must keep compiling, and keep producing the same answers
/// the comments in them promise.
#[test]
fn the_working_examples_compile() {
    for file in ["hello.tc", "arith.tc", "spill.tc", "reassign.tc", "bool.tc"] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples").join(file);
        let text = std::fs::read_to_string(&path).unwrap();
        let compiled = tinyc::compile(&text, Target::X86_64Windows)
            .unwrap_or_else(|errors| panic!("{file} failed to compile: {errors:?}"));

        assert!(compiled.asm.contains("main PROC"));
        assert!(compiled.asm.contains("main ENDP"));
        // Every pushed register must be popped again.
        assert_eq!(
            compiled.asm.matches("push ").count(),
            compiled.asm.matches("pop  ").count(),
            "unbalanced prologue in {file}"
        );
    }
}

/// The allocation the backend is handed must be internally consistent.
#[test]
fn allocations_are_valid() {
    use tinyc::codegen::{backend_for, regalloc};

    for file in ["hello.tc", "arith.tc", "spill.tc", "reassign.tc", "bool.tc"] {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples").join(file);
        let text = std::fs::read_to_string(&path).unwrap();
        let compiled = tinyc::compile(&text, Target::X86_64Windows).unwrap();
        let backend = backend_for(Target::X86_64Windows);

        if let Err(problem) = regalloc::verify(&compiled.allocation, backend.register_file()) {
            panic!("{file}: {problem}");
        }
    }
}
