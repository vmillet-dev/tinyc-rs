//! Every file in `examples/errors/` must fail, and must fail *at the right
//! place*: these assertions are what keep the line/column reporting honest.
//!
//! Only about *where* the caret lands. That a program is refused at all is
//! checked for every target at once in `tests/targets.rs`, and what the emitted
//! assembly looks like belongs to the backend that emits it — neither is here.

use std::path::Path;

use tinyc::codegen::Target;
use tinyc::diag::SourceFile;

/// The target these tests compile for.
///
/// A diagnostic's position is a fact about the source text, so nearly all of
/// these hold whatever the target is — and `tests/targets.rs` checks that every
/// target refuses every one of these files. The exceptions are the two about
/// parameter counts, which are the target's own answer: `too_many_params`
/// points at the *fifth* parameter only on a target that passes four. So one is
/// pinned here deliberately rather than assumed.
const TARGET: Target = Target::X86_64Windows;

/// Compile an example and return the `line:col` of each diagnostic it produces.
fn error_positions(file: &str) -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/errors").join(file);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let source = SourceFile::new(file, text);

    match tinyc::compile(source.text(), TARGET) {
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
    // `x + s` mixes an int with a string. Since `+` joins strings, the operand
    // that is not one is the one at fault — which is also the reading that
    // helps, because the mistake this catches is nearly always `"total: " + n`.
    assert_error_at("type_mismatch.tc", "4:9");
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
    let Err(errors) = tinyc::compile(source.text(), TARGET) else {
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
    let Err(errors) = tinyc::compile(source.text(), TARGET) else {
        panic!("match_arms_disagree.tc was expected to fail");
    };
    let (_, note_span) = errors[0].note.clone().expect("a note pointing at the arm that set the type");
    assert_eq!(source.line_col(note_span.unwrap().offset), (5, 20));
}

#[test]
fn an_arm_with_no_value_points_at_its_pattern() {
    // The block itself is not wrong — it is wrong *for this match*, so the
    // caret goes on the arm rather than inside it, and covers the whole
    // pattern rather than the half of it that names a variant.
    assert_error_at("match_arm_without_value.tc", "6:5");
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

// -- strings and characters -----------------------------------------------

#[test]
fn writing_into_a_string_points_at_the_element() {
    // A string is read-only, which is what makes sharing one unobservable.
    assert_error_at("string_modified.tc", "3:3");
}

#[test]
fn arithmetic_on_a_character_points_at_the_character() {
    assert_error_at("char_arithmetic.tc", "3:9");
}

/// The left operand settles which kind of number the expression is, so what is
/// underlined is the operand that disagreed with it.
#[test]
fn mixing_an_int_and_a_float_points_at_the_second_operand() {
    assert_error_at("mixing_int_and_float.tc", "4:15");
}

/// A `match` on a float is refused at the *scrutinee*: the arms are not the
/// mistake, and one of them would be underlined for the wrong reason.
#[test]
fn matching_on_a_float_points_at_the_value_being_matched() {
    assert_error_at("match_on_a_float.tc", "3:10");
}

#[test]
fn a_conversion_that_does_not_exist_points_at_the_whole_conversion() {
    assert_error_at("no_conversion.tc", "2:9");
}

#[test]
fn ordering_two_strings_points_at_the_whole_comparison() {
    // Equality is fine; order is not, and the refusal explains itself rather
    // than merely happening.
    assert_error_at("string_ordering.tc", "2:9");
}

#[test]
fn a_character_literal_with_two_characters_points_at_the_literal() {
    assert_error_at("char_literal_too_long.tc", "2:12");
}

#[test]
fn a_number_that_names_no_character_points_at_the_number() {
    // Settled here rather than at run time, like a constant index out of range.
    assert_error_at("not_a_character.tc", "2:14");
}

#[test]
fn joining_a_number_to_a_string_points_at_the_number() {
    assert_error_at("joining_a_number.tc", "3:22");
}

// -- input ----------------------------------------------------------------

#[test]
fn redefining_a_builtin_points_at_the_name() {
    // The built-ins are in the signature table before the program's own
    // functions are, so this collides with something already there.
    assert_error_at("redefining_a_builtin.tc", "2:4");
}

// -- lists ----------------------------------------------------------------

#[test]
fn pushing_onto_a_parameter_points_at_the_parameter() {
    // Growing may move the list, and a caller cannot be told where it went —
    // so this is refused rather than silently working when it happens to fit.
    assert_error_at("push_onto_parameter.tc", "2:8");
}

#[test]
fn pushing_onto_an_array_points_at_the_keyword() {
    assert_error_at("push_onto_array.tc", "3:3");
}

#[test]
fn printing_a_list_points_at_the_list() {
    assert_error_at("printing_a_list.tc", "3:9");
}

// -- classes --------------------------------------------------------------

#[test]
fn a_missing_field_points_at_the_whole_literal() {
    // No one field is wrong — it is what the literal does not say, so the
    // caret covers the object that had to be complete.
    assert_error_at("object_missing_field.tc", "7:14");
}

#[test]
fn a_class_containing_itself_points_at_the_field_that_closes_the_ring() {
    // The field is what could be deleted to fix it, so it is what the caret
    // covers — on the class, nothing would look wrong.
    assert_error_at("class_contains_itself.tc", "6:3");
}

#[test]
fn a_ring_of_classes_points_at_the_field_that_closes_it() {
    assert_error_at("classes_contain_each_other.tc", "8:3");
}

#[test]
fn an_object_too_big_points_at_the_class() {
    // The opposite case: no one field is at fault, it is what they add up to.
    assert_error_at("object_too_big.tc", "8:7");
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

/// Enums and classes share one namespace, and the caret goes on whichever was
/// written second — even though the two are collected by different passes, in a
/// fixed order that has nothing to do with the order they were written in.
#[test]
fn an_enum_and_a_class_of_the_same_name_point_at_the_second_one() {
    assert_error_at("enum_and_class_collide.tc", "8:7");
}

#[test]
fn a_bare_return_in_a_returning_function_points_at_the_keyword() {
    assert_error_at("return_without_value.tc", "2:3");
}


// -- format strings -------------------------------------------------------
//
// Every one of these points *inside* a string literal, which is the only place
// in the compiler a span has to be worked out rather than read off a token. An
// escape earlier in the literal turns two characters of source into one, so the
// character count is not the distance from the opening quote — the offset each
// character came from is kept with it, and these tests are what say so.

#[test]
fn an_unknown_specifier_points_at_the_two_characters_of_it() {
    assert_error_at("unknown_specifier.tc", "2:19");
}

#[test]
fn a_percent_at_the_end_of_a_format_points_at_the_percent() {
    assert_error_at("unfinished_specifier.tc", "2:21");
}

/// The caret goes on the specifier with nothing to write, not on the format as
/// a whole: which one ran out is the useful half of the answer.
#[test]
fn a_format_short_of_values_points_at_the_specifier_that_has_none() {
    assert_error_at("format_wants_more_values.tc", "3:19");
}

#[test]
fn a_spare_value_points_at_the_value_rather_than_the_format() {
    assert_error_at("format_has_a_spare_value.tc", "3:20");
}

/// And a mismatch points at the *value*, since that is the half a reader would
/// change. The specifier it has to match is where the note points.
#[test]
fn a_value_of_the_wrong_type_points_at_the_value() {
    assert_error_at("format_type_mismatch.tc", "3:21");
}

#[test]
fn a_format_that_is_not_a_literal_points_at_what_was_written_instead() {
    assert_error_at("format_is_not_a_literal.tc", "3:11");
}

/// The note of a mismatch points back at the specifier that asked, so both
/// halves of the disagreement are on screen.
#[test]
fn a_mismatch_notes_the_specifier_it_came_from() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/errors/format_type_mismatch.tc");
    let source = SourceFile::new("format_type_mismatch.tc", std::fs::read_to_string(&path).unwrap());
    let Err(errors) = tinyc::compile(source.text(), TARGET) else {
        panic!("the file was expected to fail");
    };
    let (_, note_span) = errors[0].note.clone().expect("a note pointing at the specifier");
    let span = note_span.expect("the note carries a span");
    assert_eq!(source.line_col(span.offset), (3, 16), "the `%d` itself");
    assert_eq!(span.len, 2, "just the two characters of it");
}
