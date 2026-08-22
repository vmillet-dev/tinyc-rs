//! Every file in `examples/errors/` must fail, and must fail *at the right
//! place*: these assertions are what keep the line/column reporting honest.

use std::path::Path;

use tinyc::codegen::Target;
use tinyc::codegen::x64_win::is_runtime_symbol;
use tinyc::diag::SourceFile;

/// The examples these tests compile.
///
/// `examples/hello.tc` is deliberately absent: it is a scratch file for trying
/// things out by hand, so it is allowed to be broken at any time. Anything that
/// should stay working belongs in its own example listed here.
const EXAMPLES: [&str; 9] = [
    "arith.tc",
    "spill.tc",
    "reassign.tc",
    "bool.tc",
    "control_flow.tc",
    "functions.tc",
    "enums.tc",
    "arrays.tc",
    "classes.tc",
];

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
    assert_error_at("unterminated_string.tc", "2:14");
}

#[test]
fn missing_semicolon_points_at_the_token_that_should_have_followed_it() {
    assert_error_at("missing_semicolon.tc", "3:3");
}

#[test]
fn undeclared_variable_points_at_the_use() {
    assert_error_at("undeclared_variable.tc", "3:13");
}

#[test]
fn type_mismatch_points_at_the_offending_operand() {
    assert_error_at("type_mismatch.tc", "4:13");
}

#[test]
fn bad_initializer_points_at_the_initializer() {
    assert_error_at("bad_initializer.tc", "2:11");
}

#[test]
fn division_by_zero_points_at_the_divisor() {
    assert_error_at("division_by_zero.tc", "3:13");
}

#[test]
fn an_overflow_points_at_the_whole_operation_not_one_operand() {
    // Neither operand is wrong on its own — it is putting them together that
    // has no answer, so the caret covers both.
    assert_error_at("overflow.tc", "4:9");
}

#[test]
fn an_int_initializer_for_a_bool_points_at_the_initializer() {
    assert_error_at("bool_type_mismatch.tc", "2:16");
}

#[test]
fn a_reserved_word_cannot_be_used_as_a_variable_name() {
    // `true` and `false` became keywords with the bool type, so they are no
    // longer available as identifiers.
    assert_error_at("reserved_word_as_name.tc", "2:8");
}

#[test]
fn arithmetic_on_a_bool_points_at_the_bool_operand() {
    assert_error_at("bool_arithmetic.tc", "3:9");
}

#[test]
fn a_non_bool_condition_points_at_the_condition() {
    assert_error_at("non_bool_condition.tc", "3:10");
}

#[test]
fn comparing_different_types_points_at_the_right_operand() {
    assert_error_at("compare_different_types.tc", "4:14");
}

#[test]
fn a_variable_used_outside_its_block_points_at_the_use() {
    assert_error_at("out_of_scope.tc", "5:9");
}

#[test]
fn assigning_the_wrong_type_points_at_the_value() {
    assert_error_at("assign_wrong_type.tc", "3:11");
}

#[test]
fn assigning_to_an_undeclared_variable_points_at_the_name() {
    assert_error_at("assign_undeclared.tc", "3:3");
}

#[test]
fn a_logical_operator_on_the_wrong_type_points_at_the_offending_operand() {
    assert_error_at("logic_on_ints.tc", "3:9");
}

#[test]
fn negating_an_int_points_at_the_operand_not_the_bang() {
    // `if (!n)` is the habit from a language with truthiness, so this is the
    // one worth pointing at precisely.
    assert_error_at("not_on_an_int.tc", "3:8");
}

#[test]
fn a_break_with_no_loop_to_leave_points_at_the_keyword() {
    // An `if` is not a loop, which is exactly the mistake worth catching.
    assert_error_at("break_outside_loop.tc", "3:5");
}

#[test]
fn redeclaration_points_at_both_declarations() {
    assert_error_at("redeclaration.tc", "3:7");

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/errors/redeclaration.tc");
    let source = SourceFile::new("redeclaration.tc", std::fs::read_to_string(path).unwrap());
    let Err(errors) = tinyc::compile(source.text(), Target::X86_64Windows) else {
        panic!("redeclaration.tc was expected to fail");
    };
    let (_, note_span) = errors[0].note.clone().expect("a note pointing at the first declaration");
    assert_eq!(source.line_col(note_span.unwrap().offset), (2, 7));
}

// -- enums ----------------------------------------------------------------

#[test]
fn a_non_exhaustive_match_points_at_the_keyword() {
    // Not at any one arm: the mistake is what the arms do not say between them,
    // so the caret goes on the statement that had to be complete.
    assert_error_at("non_exhaustive_match.tc", "4:3");
}

#[test]
fn an_unknown_variant_points_at_the_variant_not_the_enum() {
    assert_error_at("unknown_variant.tc", "4:22");
}

#[test]
fn ordering_two_enum_values_points_at_the_whole_comparison() {
    assert_error_at("enum_ordering.tc", "4:9");
}

#[test]
fn arms_that_disagree_point_at_the_second_one_and_note_the_first() {
    assert_error_at("match_arms_disagree.tc", "6:22");

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/errors/match_arms_disagree.tc");
    let source = SourceFile::new("match_arms_disagree.tc", std::fs::read_to_string(path).unwrap());
    let Err(errors) = tinyc::compile(source.text(), Target::X86_64Windows) else {
        panic!("match_arms_disagree.tc was expected to fail");
    };
    let (_, note_span) = errors[0].note.clone().expect("a note pointing at the arm that set the type");
    assert_eq!(source.line_col(note_span.unwrap().offset), (5, 20));
}

#[test]
fn an_arm_with_no_value_points_at_its_pattern() {
    // The block itself is not wrong — it is wrong *for this match*, so the
    // caret goes on the arm rather than inside it.
    assert_error_at("match_arm_without_value.tc", "6:13");
}

// -- arrays ---------------------------------------------------------------

#[test]
fn an_index_the_compiler_can_see_points_at_the_index() {
    assert_error_at("index_out_of_bounds.tc", "3:12");
}

#[test]
fn a_length_that_disagrees_points_at_the_literal() {
    assert_error_at("array_length_mismatch.tc", "2:15");
}

// -- classes --------------------------------------------------------------

#[test]
fn a_missing_field_points_at_the_whole_literal() {
    // No one field is wrong — it is what the literal does not say, so the
    // caret covers the object that had to be complete.
    assert_error_at("object_missing_field.tc", "7:14");
}

#[test]
fn a_downcast_points_at_the_argument() {
    // Widening a subclass to its base is free; the other direction is not
    // something the compiler can know.
    assert_error_at("downcast.tc", "16:17");
}

#[test]
fn the_hidden_return_address_points_at_the_parameter_that_no_longer_fits() {
    // Returning an aggregate spends one of the four argument registers on the
    // address the caller hands in, so a fourth parameter is one too many.
    assert_error_at("too_many_params_returning.tc", "5:31");
}

// -- functions ------------------------------------------------------------

#[test]
fn a_program_without_main_is_reported_at_the_start_of_the_file() {
    assert_error_at("no_main.tc", "1:1");
}

#[test]
fn an_unknown_callee_points_at_the_name() {
    assert_error_at("unknown_function.tc", "2:9");
}

#[test]
fn the_wrong_number_of_arguments_points_at_the_whole_call() {
    assert_error_at("wrong_argument_count.tc", "6:9");
}

#[test]
fn an_argument_of_the_wrong_type_points_at_that_argument() {
    assert_error_at("wrong_argument_type.tc", "6:16");
}

#[test]
fn a_body_that_can_fall_off_the_end_points_at_its_closing_brace() {
    assert_error_at("missing_return.tc", "5:1");
}

#[test]
fn too_many_parameters_points_at_the_first_one_that_does_not_fit() {
    assert_error_at("too_many_params.tc", "1:37");
}

#[test]
fn a_void_call_used_as_a_value_points_at_the_call() {
    assert_error_at("void_used_as_value.tc", "6:11");
}

#[test]
fn a_duplicate_function_points_at_the_second_definition() {
    assert_error_at("duplicate_function.tc", "5:4");
}

#[test]
fn a_bare_return_in_a_returning_function_points_at_the_keyword() {
    assert_error_at("return_without_value.tc", "2:3");
}

/// The good examples must keep compiling, and keep producing the same answers
/// the comments in them promise.
#[test]
fn the_working_examples_compile() {
    for file in EXAMPLES {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples").join(file);
        let text = std::fs::read_to_string(&path).unwrap();
        let compiled = tinyc::compile(&text, Target::X86_64Windows)
            .unwrap_or_else(|errors| panic!("{file} failed to compile: {errors:?}"));

        assert!(compiled.asm.contains("global main"));
        assert!(compiled.asm.contains("section .text"));

        // Every path out of a function must undo its prologue exactly once. A
        // function with two `return`s has two epilogues, so the counts balance
        // per exit, not per file.
        let functions = functions_in(&compiled.asm);
        assert!(!functions.is_empty(), "{file}: no function was recognised in the assembly");
        for (name, body) in functions {
            // The runtime helpers are jumped to and never come back, so they
            // have neither a prologue nor an epilogue to balance.
            if is_runtime_symbol(name) {
                continue;
            }
            let pushes = body.matches("push ").count();
            let exits = body.matches("\n    ret\n").count();
            assert!(exits > 0, "{file}: {name} never returns");
            assert_eq!(
                body.matches("pop  ").count(),
                pushes * exits,
                "unbalanced prologue in {file}: {name}"
            );
            // A leaf that spills nothing reserves no frame, and so has nothing
            // to release — but a `sub` without a matching `add` on some path
            // would leave the stack wrong.
            let reserved = body.matches("sub  rsp,").count();
            assert_eq!(
                body.matches("add  rsp,").count(),
                reserved * exits,
                "{file}: {name} does not release its frame on every path"
            );
        }
    }
}

/// Every symbol the assembly defines, other than the entry point, must be one
/// no runtime or literal can also claim.
#[test]
fn generated_symbols_cannot_collide_with_a_tinyc_function() {
    for file in EXAMPLES {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples").join(file);
        let text = std::fs::read_to_string(&path).unwrap();
        let compiled = tinyc::compile(&text, Target::X86_64Windows).unwrap();

        for (name, _) in functions_in(&compiled.asm) {
            assert!(
                name == "main" || name.contains('$'),
                "{file}: `{name}` is a bare symbol a TinyC function could also define"
            );
        }
    }
}

/// Split emitted assembly into `(name, body)` per function.
///
/// A function starts at a label in the first column; the block labels inside it
/// all begin with a `.`, which is exactly how NASM scopes them too.
fn functions_in(asm: &str) -> Vec<(&str, &str)> {
    let starts: Vec<(usize, &str)> = asm
        .match_indices('\n')
        .filter_map(|(offset, _)| {
            let line = asm[offset + 1..].lines().next()?;
            let name = line.strip_suffix(':')?;
            let plain = !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
                && !name.starts_with(|c: char| c.is_ascii_digit());
            plain.then_some((offset + 1, name))
        })
        .collect();

    starts
        .iter()
        .enumerate()
        .map(|(index, &(offset, name))| {
            let end = starts.get(index + 1).map_or(asm.len(), |&(next, _)| next);
            (name, &asm[offset..end])
        })
        .collect()
}

/// The allocation the backend is handed must be internally consistent.
#[test]
fn allocations_are_valid() {
    use tinyc::codegen::{backend_for, regalloc};

    for file in EXAMPLES {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples").join(file);
        let text = std::fs::read_to_string(&path).unwrap();
        let compiled = tinyc::compile(&text, Target::X86_64Windows).unwrap();
        let backend = backend_for(Target::X86_64Windows);

        for allocation in &compiled.allocations {
            if let Err(problem) = regalloc::verify(allocation, backend.register_file()) {
                panic!("{file}: {problem}");
            }
        }
    }
}
