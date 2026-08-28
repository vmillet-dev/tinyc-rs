use super::*;
use crate::lexer::lex;
use crate::ast::{Prim, Spec, Stmt, Ty};
use crate::parser::parse;

fn check_src(src: &str) -> Result<Types> {
    // Four is what every backend in the tree reports today.
    check(&parse(&lex(src)?)?, 4)
}

/// Wrap statements in a `main`, so the tests about statements stay about
/// statements.
fn check_main(body: &str) -> Result<Types> {
    check_src(&format!("fn main() {{\n{body}\n}}\n"))
}

fn errors_in_main(body: &str) -> Vec<Diagnostic> {
    check_main(body).unwrap_err()
}

/// Whether a body checks with nothing to report, for the tests that are
/// about a form being *accepted*.
fn errors_in_main_none(body: &str) -> bool {
    check_main(body).is_ok()
}

// -- how many diagnostics, and in what order ---------------------------

#[test]
fn one_missing_declaration_is_one_diagnostic() {
    // The name is undeclared on both sides of the `=`, but the mistake is
    // the same one: a reader should be told once.
    let errors = errors_in_main("y = y + 1;");
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert!(errors[0].label.as_deref().unwrap().contains("assign to it"), "{errors:#?}");
}

#[test]
fn a_name_mentioned_many_times_is_still_reported_once() {
    let errors = errors_in_main("print(nope);\nprint(nope);\nprint(nope);");
    assert_eq!(errors.len(), 1, "{errors:#?}");
}

#[test]
fn each_function_gets_its_own_say_about_the_same_name() {
    // Two functions, two independent mistakes.
    let errors = check_src(
        "fn a() {\n  print(nope);\n}\nfn b() {\n  print(nope);\n}\nfn main() {\n  a();\n}",
    )
    .unwrap_err();
    assert_eq!(errors.len(), 2, "{errors:#?}");
}

#[test]
fn diagnostics_are_reported_in_source_order() {
    // A statement's value is checked before its name, so without sorting
    // these would come out back to front.
    let errors = errors_in_main("int x = 1;\nx = \"a\";\nstring s = 1;\nprint(s + 1);");
    assert!(errors.len() > 1, "this program should produce several: {errors:#?}");
    let offsets: Vec<u32> = errors.iter().map(|d| d.span.offset).collect();
    let mut sorted = offsets.clone();
    sorted.sort_unstable();
    assert_eq!(offsets, sorted, "{errors:#?}");
}

#[test]
fn a_call_statement_records_the_type_it_produced() {
    // Nothing reads it today, but every expression node has an entry and a
    // hole here would be a trap for whoever reads one next.
    let types = check_src(
        "fn label() -> string {\n  return \"hi\";\n}\nfn main() {\n  label();\n}",
    )
    .unwrap();
    let ast = parse(&lex("fn label() -> string {\n  return \"hi\";\n}\nfn main() {\n  label();\n}")
        .unwrap())
    .unwrap();
    let Stmt::Call(call) = &ast.functions[1].body.stmts[0] else { panic!("a call statement") };
    assert_eq!(types.of(call.id), Ty::Str);
}

#[test]
fn accepts_the_sample_program() {
    assert!(
        check_main("int x = 10;\nint y = 20;\nstring s = \"hi\";\nprint(x + y);\nprint(s);")
            .is_ok()
    );
}

#[test]
fn rejects_arithmetic_on_strings() {
    let errors = errors_in_main("string s = \"a\";\nprint(1 + s);");
    assert_eq!(errors.len(), 1);
    assert!(errors[0].message.contains("cannot apply `+`"));
}

#[test]
fn rejects_a_mistyped_initializer() {
    assert!(errors_in_main("int x = \"nope\";")[0].message.contains("cannot initialize"));
}

#[test]
fn rejects_undeclared_variables() {
    assert!(errors_in_main("print(nope);")[0].message.contains("undeclared variable `nope`"));
}

#[test]
fn rejects_redeclaration_and_points_at_the_original() {
    let errors = errors_in_main("int x = 1;\nint x = 2;");
    assert!(errors[0].message.contains("already declared"));
    assert!(errors[0].note.as_ref().unwrap().1.is_some());
}

#[test]
fn accepts_assignment_of_the_declared_type() {
    assert!(check_main("string s = \"a\";\ns = \"b\";\nprint(s);").is_ok());
    assert!(check_main("int n = 1;\nn = n * 2;\nprint(n);").is_ok());
}

#[test]
fn rejects_assignment_of_the_wrong_type() {
    let errors = errors_in_main("int n = 1;\nn = \"two\";");
    assert!(errors[0].message.contains("cannot assign"), "{}", errors[0].message);
    assert!(errors[0].note.as_ref().unwrap().1.is_some());
}

#[test]
fn rejects_assignment_to_an_undeclared_variable() {
    assert!(errors_in_main("nope = 1;")[0].message.contains("undeclared variable `nope`"));
}

#[test]
fn accepts_bool_declarations_assignment_and_printing() {
    assert!(check_main("bool ready = true;\nready = false;\nprint(ready);").is_ok());
    assert!(check_main("print(true);").is_ok());
}

#[test]
fn rejects_an_int_initializer_for_a_bool() {
    assert!(errors_in_main("bool ready = 1;")[0].message.contains("cannot initialize"));
}

#[test]
fn rejects_a_bool_initializer_for_an_int() {
    assert!(errors_in_main("int n = true;")[0].message.contains("cannot initialize"));
}

#[test]
fn rejects_assigning_a_bool_to_a_string() {
    assert!(errors_in_main("string s = \"hi\";\ns = true;")[0].message.contains("cannot assign"));
}

#[test]
fn rejects_arithmetic_on_bools() {
    let errors = errors_in_main("bool ready = true;\nprint(ready + 1);");
    assert!(errors[0].message.contains("cannot apply `+`"), "{}", errors[0].message);
}

#[test]
fn rejects_negating_a_bool() {
    let errors = errors_in_main("bool ready = true;\nprint(-ready);");
    assert!(errors[0].message.contains("cannot apply `-`"), "{}", errors[0].message);
}

#[test]
fn a_comparison_produces_a_bool() {
    assert!(check_main("bool ok = 1 < 2;\nprint(ok);").is_ok());
    assert!(check_main("if (1 == 2) {\n  print(1);\n}").is_ok());
}

#[test]
fn rejects_a_condition_that_is_not_a_bool() {
    for src in ["if (1) {\n}", "while (1) {\n}", "for (int i = 0; i; i = i + 1) {\n}"] {
        let errors = errors_in_main(src);
        assert!(errors[0].message.contains("must be a `bool`"), "{src}: {}", errors[0].message);
    }
}

#[test]
fn rejects_comparing_different_types() {
    let errors = errors_in_main("string s = \"a\";\nprint(s == 1);");
    assert!(errors[0].message.contains("cannot compare"), "{}", errors[0].message);
}

#[test]
fn rejects_ordering_comparisons_that_make_no_sense() {
    assert!(errors_in_main("print(true < false);")[0].message.contains("cannot be compared"));
    // Two strings answer `==`, but not `<`: sorting them is a question
    // about a language rather than about the characters.
    let error = &errors_in_main("print(\"a\" < \"b\");")[0];
    assert!(error.message.contains("cannot be compared"), "{}", error.message);
    assert!(error.note.is_some(), "the refusal explains itself");
}

#[test]
fn two_strings_can_be_compared_for_equality() {
    assert!(check_main("print(\"a\" == \"b\");\nprint(\"a\" != \"b\");").is_ok());
}

// -- strings and characters --------------------------------------------

#[test]
fn joining_two_strings_produces_a_string() {
    assert!(check_main("string s = \"a\" + \"b\";\nprint(s + s);").is_ok());
}

#[test]
fn rejects_joining_a_string_to_anything_else() {
    // The mistake every language with a looser `+` accepts, and the note
    // says how to write what was meant.
    let errors = errors_in_main("print(\"n = \" + 1);");
    assert!(errors[0].message.contains("cannot apply `+`"), "{}", errors[0].message);
    assert!(errors[0].note.as_ref().is_some_and(|(text, _)| text.contains("string(n)")));
}

#[test]
fn a_string_a_list_and_an_array_all_have_a_length() {
    assert!(check_main("print(len(\"abc\"));").is_ok());
    let errors = errors_in_main("print(len(1));");
    assert!(errors[0].message.contains("something with a length"), "{}", errors[0].message);
}

#[test]
fn indexing_a_string_produces_a_character() {
    assert!(check_main("char c = \"abc\"[0];\nprint(c);").is_ok());
    // Never an int, and never a string of length one: no conversion is
    // implied anywhere, so the declared type has to agree.
    let errors = errors_in_main("int n = \"abc\"[0];");
    assert!(errors[0].message.contains("with a `char` value"), "{}", errors[0].message);
}

#[test]
fn a_string_cannot_be_written_into() {
    // Immutability is what makes sharing a string unobservable, so this is
    // load-bearing rather than a restriction.
    let errors = errors_in_main("string s = \"abc\";\ns[0] = 'x';");
    assert!(errors[0].message.contains("cannot be modified"), "{}", errors[0].message);
}

#[test]
fn characters_compare_but_do_not_do_arithmetic() {
    assert!(check_main("print('a' == 'b');\nprint('a' < 'b');").is_ok());
    let errors = errors_in_main("print('a' + 1);");
    assert!(errors[0].message.contains("cannot apply `+`"), "{}", errors[0].message);
    assert!(errors[0].note.as_ref().is_some_and(|(text, _)| text.contains("int(c)")));
}

#[test]
fn the_four_conversions_are_accepted_and_nothing_else_is() {
    assert!(check_main("print(int('a'));").is_ok());
    assert!(check_main("print(char(65));").is_ok());
    assert!(check_main("print(string('a'));").is_ok());
    assert!(check_main("print(string(65));").is_ok());

    let errors = errors_in_main("print(int(true));");
    assert!(errors[0].message.contains("no conversion from `bool`"), "{}", errors[0].message);
}

// -- lists -------------------------------------------------------------

#[test]
fn a_list_takes_its_element_type_from_the_declaration() {
    assert!(check_main("int[] xs = [];\npush(xs, 1);\nprint(len(xs));").is_ok());
    assert!(check_main("int[] xs = [1, 2, 3];\nprint(xs[0]);").is_ok());
    // The same literal is an array where an array was asked for.
    assert!(check_main("int[3] xs = [1, 2, 3];\nprint(xs[0]);").is_ok());

    let errors = errors_in_main("int[] xs = [1, true];");
    assert!(errors[0].message.contains("this element is a `bool`"), "{}", errors[0].message);
}

#[test]
fn an_empty_literal_is_a_list_and_nothing_else() {
    // An array cannot be empty, so `[]` on its own has no type to read.
    let errors = errors_in_main("int[3] xs = [];");
    assert!(errors[0].message.contains("no elements"), "{}", errors[0].message);
}

#[test]
fn a_list_and_an_array_of_the_same_thing_are_different_types() {
    let errors = errors_in_main("int[3] a = [1, 2, 3];\nint[] b = a;");
    assert!(errors[0].message.contains("cannot initialize"), "{}", errors[0].message);
}

#[test]
fn push_needs_a_list_that_the_function_owns() {
    assert!(check_main("int[] xs = [];\npush(xs, 1);").is_ok());

    let errors = errors_in_main("int[3] xs = [1, 2, 3];\npush(xs, 4);");
    assert!(errors[0].message.contains("`push` needs a list"), "{}", errors[0].message);

    // A parameter is the caller's, and growing may move it.
    let errors = check_src("fn f(int[] xs) {\n  push(xs, 1);\n}\nfn main() {\n}\n")
        .expect_err("pushing onto a parameter is refused");
    assert!(errors[0].message.contains("which is a parameter"), "{}", errors[0].message);
}

#[test]
fn a_list_cannot_be_printed_or_compared() {
    // Both would answer a question about the address rather than about the
    // elements, which is the same reason an array answers neither.
    let errors = errors_in_main("int[] xs = [1];\nprint(xs);");
    assert!(errors[0].message.contains("cannot print"), "{}", errors[0].message);

    let errors = errors_in_main("int[] xs = [1];\nprint(xs == xs);");
    assert!(errors[0].message.contains("cannot be compared"), "{}", errors[0].message);
}

#[test]
fn a_list_may_hold_objects() {
    // The elements live in the list, so an object is no harder to hold
    // than an `int`: growing copies whole elements, and nothing is shared.
    assert!(
        check_src(
            "class Shape {\n  int n;\n}\n\
             fn main() {\n  Shape[] xs = [];\n  push(xs, Shape { n: 1 });\n  \
             print(xs[0].n);\n}\n"
        )
        .is_ok()
    );
    // And a subclass may go into a list of its base, as it may into an
    // array of one: every slot is the hierarchy's size.
    assert!(
        check_src(
            "class Shape {\n  fn area(self) -> int { return 0; }\n}\n\
             class Circle : Shape {\n  int r;\n  fn area(self) -> int { return self.r; }\n}\n\
             fn main() {\n  Shape[] xs = [];\n  push(xs, Circle { r: 2 });\n  \
             print(xs[0].area());\n}\n"
        )
        .is_ok()
    );
    // A list of lists or of arrays cannot even be *written*: a type carries
    // at most one pair of brackets.
    assert!(check_src("fn main() {\n  int[][] xs = [];\n}\n").is_err());
}

// -- input -------------------------------------------------------------

#[test]
fn the_builtins_are_called_like_any_other_function() {
    assert!(check_main("string s = read_line();\nprint(s);").is_ok());
    assert!(check_main("while (!eof()) {\n  print(read_line());\n}").is_ok());
    // `read_line();` on its own throws the line away, which is a way to
    // skip one — a call statement is allowed to discard a value.
    assert!(check_main("read_line();").is_ok());
}

#[test]
fn a_builtin_checks_its_arguments_like_any_other_function() {
    let errors = errors_in_main("print(read_line(1));");
    assert!(errors[0].message.contains("takes 0 arguments"), "{}", errors[0].message);
    assert!(errors[0].note.as_ref().is_some_and(|(text, at)| {
        // A built-in was declared nowhere the program can be shown.
        text.contains("built in") && at.is_none()
    }));

    let errors = errors_in_main("int n = eof();");
    assert!(errors[0].message.contains("with a `bool` value"), "{}", errors[0].message);
}

#[test]
fn a_program_cannot_take_a_builtins_name() {
    let errors = check_src("fn eof() -> bool {\n  return true;\n}\nfn main() {\n}\n")
        .expect_err("`eof` is taken");
    assert!(errors[0].message.contains("is built in"), "{}", errors[0].message);
}

#[test]
fn text_converts_back_into_a_number() {
    assert!(check_main("print(int(read_line()));").is_ok());
    assert!(check_main("print(int(\"42\"));").is_ok());
}

#[test]
fn there_is_a_way_to_ask_before_converting() {
    // The bargain `eof` strikes with `read_line`, one type further along:
    // `int(s)` stops the program on text that spells no number, and text
    // that spells no number is data rather than a mistake.
    assert!(
        check_main("string s = read_line();\nif (is_int(s)) {\n  print(int(s));\n}").is_ok()
    );

    let errors = errors_in_main("print(is_int(42));");
    assert!(
        errors[0].message.contains("where a `string` is expected"),
        "{}",
        errors[0].message
    );

    let errors = errors_in_main("int n = is_int(\"1\");");
    assert!(errors[0].message.contains("with a `bool` value"), "{}", errors[0].message);

    let errors = check_src("fn is_int(string s) -> bool {\n  return true;\n}\nfn main() {\n}\n")
        .expect_err("`is_int` is taken");
    assert!(errors[0].message.contains("is built in"), "{}", errors[0].message);
}

#[test]
fn a_char_list_seals_into_a_string() {
    // The point of the whole feature: characters accumulate in one place
    // that grows, and become a string once.
    assert!(check_main("char[] cs = ['a'];\nprint(string(cs));").is_ok());

    let errors = errors_in_main("int[] ns = [1];\nprint(string(ns));");
    assert!(errors[0].message.contains("no conversion from `int[]`"), "{}", errors[0].message);
}

#[test]
fn a_conversion_to_its_own_type_is_refused_rather_than_ignored() {
    let errors = errors_in_main("print(int(1));");
    assert!(errors[0].message.contains("already an `int`"), "{}", errors[0].message);
}

#[test]
fn a_constant_that_names_no_character_is_settled_at_compile_time() {
    // The same bargain a constant index strikes: what reaches the emitted
    // code is only ever a value the running program alone knows.
    for bad in ["1114112", "55296", "0 - 1"] {
        let errors = errors_in_main(&format!("print(char({bad}));"));
        assert!(errors[0].message.contains("not a character"), "{bad}: {}", errors[0].message);
    }
    assert!(check_main("print(char(1114111));").is_ok());
}

#[test]
fn a_block_scopes_its_declarations() {
    let errors = errors_in_main("if (true) {\n  int inner = 1;\n}\nprint(inner);");
    assert!(errors[0].message.contains("undeclared variable `inner`"));
}

#[test]
fn an_inner_block_may_shadow_an_outer_name() {
    assert!(
        check_main("int i = 1;\nif (true) {\n  string i = \"x\";\n  print(i);\n}\nprint(i);")
            .is_ok()
    );
}

#[test]
fn a_for_variable_does_not_escape_its_loop() {
    assert!(
        check_main("for (int i = 0; i < 1; i = i + 1) {\n}\nfor (int i = 0; i < 1; i = i + 1) {\n}")
            .is_ok()
    );
    let errors = errors_in_main("for (int i = 0; i < 1; i = i + 1) {\n}\nprint(i);");
    assert!(errors[0].message.contains("undeclared variable `i`"));
}

#[test]
fn the_remainder_operator_is_int_only_like_the_rest_of_arithmetic() {
    assert!(check_main("print(17 % 5);").is_ok());
    assert!(check_main("bool even = 4 % 2 == 0;\nprint(even);").is_ok());
    assert!(errors_in_main("print(true % 2);")[0].message.contains("cannot apply `%`"));
}

#[test]
fn a_remainder_by_a_literal_zero_is_rejected_like_a_division() {
    // It is the same instruction underneath, and traps the same way.
    assert!(errors_in_main("print(1 % 0);")[0].message.contains("division by zero"));
}

// -- logical operators -------------------------------------------------

#[test]
fn negation_takes_a_bool_and_produces_one() {
    assert!(check_main("bool ok = true;\nprint(!ok);").is_ok());
    assert!(check_main("if (!(1 < 2)) {\n}").is_ok());
    assert!(check_main("bool a = !!false;\nprint(a);").is_ok());
}

#[test]
fn rejects_negating_anything_that_is_not_a_bool() {
    // `!n` on an int is the habit from languages with truthiness, so the
    // diagnostic says there is no implicit truth test.
    let errors = errors_in_main("int n = 1;\nprint(!n);");
    assert!(errors[0].message.contains("cannot apply `!`"), "{}", errors[0].message);
    assert!(errors[0].note.is_some(), "{errors:#?}");
    assert!(errors_in_main("print(!\"a\");")[0].message.contains("cannot apply `!`"));
}

#[test]
fn a_negation_is_a_bool_wherever_one_is_wanted() {
    assert!(check_main("bool ok = true;\nwhile (!ok) {\n  ok = true;\n}").is_ok());
    assert!(errors_in_main("bool ok = true;\nint n = !ok;")[0].message.contains("cannot initialize"));
}


#[test]
fn logical_operators_take_bools_and_produce_one() {
    assert!(check_main("bool ok = true && false;\nprint(ok);").is_ok());
    assert!(check_main("int n = 5;\nif (n > 1 && n < 10) {\n  print(n);\n}").is_ok());
    assert!(check_main("bool a = true;\nwhile (a || 1 < 2) {\n  a = false;\n}").is_ok());
}

#[test]
fn rejects_a_non_bool_operand_of_a_logical_operator() {
    for body in ["print(1 && true);", "print(true || 1);", "print(\"a\" && \"b\");"] {
        let errors = errors_in_main(body);
        assert!(errors[0].message.contains("cannot apply"), "{body}: {}", errors[0].message);
    }
}

#[test]
fn a_mistake_in_the_right_operand_is_reported_even_though_it_may_not_run() {
    // Short circuiting decides what is *evaluated*, not what is checked.
    let errors = errors_in_main("print(false && nope);");
    assert!(errors[0].message.contains("undeclared variable `nope`"), "{errors:#?}");
}

#[test]
fn a_logical_operator_is_a_bool_wherever_one_is_wanted() {
    assert!(check_main("bool b = 1 < 2 || 3 < 4;\nif (b && true) {\n}").is_ok());
    assert!(errors_in_main("int n = true && false;")[0].message.contains("cannot initialize"));
}

// -- break and continue ------------------------------------------------

#[test]
fn accepts_break_and_continue_inside_a_loop() {
    assert!(check_main("while (true) {\n  break;\n}").is_ok());
    assert!(check_main("for (int i = 0; i < 3; i = i + 1) {\n  continue;\n}").is_ok());
    // Nested inside an `if`, which is the usual way they are written.
    assert!(
        check_main("for (int i = 0; i < 3; i = i + 1) {\n  if (i == 1) {\n    continue;\n  }\n}")
            .is_ok()
    );
}

#[test]
fn rejects_break_and_continue_outside_a_loop() {
    for (body, keyword) in [("break;", "break"), ("continue;", "continue")] {
        let errors = errors_in_main(body);
        assert!(
            errors[0].message.contains(&format!("`{keyword}` outside of a loop")),
            "{body}: {}",
            errors[0].message
        );
    }
    // An `if` is not a loop, and neither is a function body.
    assert!(errors_in_main("if (true) {\n  break;\n}")[0].message.contains("outside of a loop"));
}

#[test]
fn a_loop_that_has_closed_no_longer_counts() {
    // The depth is decremented on the way out, so a `break` after the loop
    // is as wrong as one that was never inside it.
    let errors = errors_in_main("while (true) {\n  break;\n}\nbreak;");
    assert_eq!(errors.len(), 1, "{errors:#?}");
}

#[test]
fn an_inner_loop_satisfies_break_for_a_body_nested_in_an_outer_one() {
    assert!(
        check_main(
            "while (true) {\n  if (false) {\n    while (true) {\n      break;\n    }\n  }\n  break;\n}"
        )
        .is_ok()
    );
}

#[test]
fn rejects_division_by_zero() {
    assert!(errors_in_main("print(1 / 0);")[0].message.contains("division by zero"));
    // The divisor alone settles it, however unknown the dividend is.
    assert!(errors_in_main("int n = 1;\nprint(n / 0);")[0].message.contains("division by zero"));
    assert!(errors_in_main("int n = 1;\nprint(n % 0);")[0].message.contains("division by zero"));
}

// -- arithmetic that has no answer -------------------------------------

#[test]
fn rejects_arithmetic_that_overflows_an_int() {
    for (body, noun) in [
        ("print(9223372036854775807 + 1);", "addition"),
        ("print(0 - 9223372036854775807 - 1 - 1);", "subtraction"),
        ("print(9223372036854775807 * 2);", "multiplication"),
    ] {
        let errors = errors_in_main(body);
        assert!(
            errors[0].message.contains(&format!("this {noun} overflows")),
            "{body}: {}",
            errors[0].message
        );
    }
}

#[test]
fn an_overflow_diagnostic_names_the_value_that_did_not_fit() {
    let errors = errors_in_main("print(9223372036854775807 + 1);");
    assert!(
        errors[0].label.as_deref().unwrap().contains("9223372036854775808"),
        "{errors:#?}"
    );
}

#[test]
fn an_overflow_is_caught_however_deeply_the_constants_are_nested() {
    // A check that only looked at the two literals either side of one
    // operator would miss this: the left operand is itself an expression.
    let errors = errors_in_main("print(2 * 2 * 4611686018427387904);");
    assert!(errors[0].message.contains("overflows"), "{}", errors[0].message);
    // The same reach makes a hidden zero divisor visible too.
    assert!(errors_in_main("print(1 / (3 - 3));")[0].message.contains("division by zero"));
}

#[test]
fn one_overflow_is_reported_once_however_much_is_built_on_it() {
    // The operators above an impossible one cannot evaluate it either, so
    // they say nothing rather than repeating the complaint.
    let errors = errors_in_main("print(9223372036854775807 + 1 + 1 + 1);");
    assert_eq!(errors.len(), 1, "{errors:#?}");
}

#[test]
fn arithmetic_that_fits_is_left_alone() {
    // Including right up to the edge, which a check written with the wrong
    // comparison would reject.
    assert!(check_main("print(9223372036854775806 + 1);").is_ok());
    assert!(check_main("print(0 - 9223372036854775807 - 1);").is_ok());
    assert!(check_main("print(4611686018427387903 * 2);").is_ok());
}

#[test]
fn an_overflow_that_depends_on_a_variable_is_left_to_the_runtime() {
    // `sema` never looks a variable up, so this compiles — and the emitted
    // code carries the guard that catches it instead.
    assert!(check_main("int n = 9223372036854775807;\nprint(n + 1);").is_ok());
}

#[test]
fn collects_several_errors() {
    assert_eq!(errors_in_main("print(a);\nprint(b);").len(), 2);
}

// -- classes -----------------------------------------------------------

/// A `Shape`/`Circle` hierarchy and a `main`, so the tests about classes
/// stay about classes.
fn check_shapes(body: &str) -> Result<Types> {
    check_src(&format!(
        "class Shape {{\n  fn area(self) -> int {{ return 0; }}\n}}\n\
         class Circle : Shape {{\n  int r;\n  \
         fn area(self) -> int {{ return 3 * self.r * self.r; }}\n}}\n\
         fn main() {{\n{body}\n}}\n"
    ))
}

fn shape_errors(body: &str) -> Vec<Diagnostic> {
    check_shapes(body).unwrap_err()
}

#[test]
fn accepts_a_class_built_read_and_dispatched_on() {
    assert!(
        check_src(
            "class Shape {\n  fn area(self) -> int { return 0; }\n}\n\
             class Circle : Shape {\n  int r;\n  \
             fn area(self) -> int { return 3 * self.r * self.r; }\n}\n\
             fn report(Shape s) {\n  print(s.area());\n}\n\
             fn main() {\n  Circle c = Circle { r: 5 };\n  report(c);\n}"
        )
        .is_ok()
    );
}

#[test]
fn a_subclass_may_stand_for_its_base_but_not_the_other_way() {
    // The one widening in the language, and it only goes one way: every
    // `Circle` is a `Shape`, and no `Shape` is known to be a `Circle`.
    assert!(check_shapes("Circle c = Circle { r: 1 };\nShape s = c;").is_ok());
    let errors = check_src(
        "class Shape {\n  fn area(self) -> int { return 0; }\n}\n\
         class Circle : Shape {\n  int r;\n  \
         fn area(self) -> int { return self.r; }\n}\n\
         fn take(Circle c) {\n}\n\
         fn main() {\n  Shape s = Circle { r: 1 };\n  take(s);\n}",
    )
    .unwrap_err();
    assert!(errors[0].message.contains("cannot pass"), "{}", errors[0].message);
}

#[test]
fn a_field_is_read_and_written_through_the_object() {
    assert!(
        check_shapes("Circle c = Circle { r: 1 };\nc.r = 2;\nprint(c.r);").is_ok()
    );
    assert!(shape_errors("Circle c = Circle { r: 1 };\nc.r = true;")[0]
        .message
        .contains("cannot assign"));
}

#[test]
fn rejects_an_unknown_field_or_method_and_lists_the_real_ones() {
    let errors = shape_errors("Circle c = Circle { r: 1 };\nprint(c.nope);");
    assert!(errors[0].message.contains("has no field `nope`"), "{}", errors[0].message);
    assert!(errors[0].note.as_ref().unwrap().0.contains("`r`"), "{errors:#?}");

    let errors = shape_errors("Circle c = Circle { r: 1 };\nc.zap();");
    assert!(errors[0].message.contains("has no method `zap`"), "{}", errors[0].message);
    assert!(errors[0].note.as_ref().unwrap().0.contains("`area`"), "{errors:#?}");
}

#[test]
fn an_object_is_complete_or_it_does_not_exist() {
    // No default and no partial object, which is what removes the question
    // `null` would have answered.
    let errors = shape_errors("Circle c = Circle { };");
    assert!(errors[0].message.contains("missing a field"), "{}", errors[0].message);
    assert!(shape_errors("Circle c = Circle { r: 1, r: 2 };")[0]
        .message
        .contains("is given twice"));
    assert!(shape_errors("Circle c = Circle { q: 1 };")[0].message.contains("has no field `q`"));
}

#[test]
fn an_inherited_field_is_named_in_the_literal_too() {
    assert!(
        check_src(
            "class Base {\n  int a;\n}\nclass Derived : Base {\n  int b;\n}\n\
             fn main() {\n  Derived d = Derived { a: 1, b: 2 };\n  print(d.a + d.b);\n}"
        )
        .is_ok()
    );
    let errors = check_src(
        "class Base {\n  int a;\n}\nclass Derived : Base {\n  int b;\n}\n\
         fn main() {\n  Derived d = Derived { b: 2 };\n}",
    )
    .unwrap_err();
    assert!(errors[0].message.contains("missing a field"), "{}", errors[0].message);
}

#[test]
fn a_base_class_is_laid_out_before_what_extends_it() {
    // Which is what makes the upcast free, and what a subclass's field
    // offsets depend on — so a derived class declared *before* its base
    // still has to come out right.
    assert!(
        check_src(
            "class Derived : Base {\n  int b;\n}\nclass Base {\n  int a;\n}\n\
             fn main() {\n  Derived d = Derived { a: 1, b: 2 };\n  print(d.a);\n}"
        )
        .is_ok()
    );
}

#[test]
fn rejects_a_class_that_is_its_own_ancestor() {
    for src in [
        "class A : A {\n}\nfn main() {\n}",
        "class A : B {\n}\nclass B : A {\n}\nfn main() {\n}",
    ] {
        let errors = check_src(src).unwrap_err();
        assert!(errors[0].message.contains("inherits from itself"), "{src}: {}", errors[0].message);
    }
}

#[test]
fn rejects_an_unknown_base_and_a_duplicate_class() {
    assert!(
        check_src("class A : Nope {\n}\nfn main() {\n}").unwrap_err()[0]
            .message
            .contains("unknown class `Nope`")
    );
    let errors = check_src("class A {\n}\nclass A {\n}\nfn main() {\n}").unwrap_err();
    assert!(errors[0].message.contains("already declared"), "{}", errors[0].message);
    assert!(errors[0].note.as_ref().unwrap().1.is_some());
}

/// A name answers with one type, so a class and an enum cannot both have
/// it: whichever lost would be a declaration no program could ever name.
#[test]
fn an_enum_and_a_class_cannot_share_a_name() {
    for src in [
        "enum A {\n  X\n}\nclass A {\n}\nfn main() {\n}",
        "class A {\n}\nenum A {\n  X\n}\nfn main() {\n}",
    ] {
        let errors = check_src(src).unwrap_err();
        assert!(errors[0].message.contains("`A` is already declared"), "{src}: {errors:?}");

        // Whichever pass noticed, the caret goes on the one written second
        // and the note on the one written first.
        let previous = errors[0].note.as_ref().unwrap().1.expect("a span to point at");
        assert!(previous.offset < errors[0].span.offset, "{src}: reported back to front");
    }
}

#[test]
fn a_field_may_not_be_named_twice_base_included() {
    let errors = check_src("class A {\n  int x;\n  int x;\n}\nfn main() {\n}").unwrap_err();
    assert!(errors[0].message.contains("already has a field"), "{}", errors[0].message);
    let errors =
        check_src("class A {\n  int x;\n}\nclass B : A {\n  int x;\n}\nfn main() {\n}")
            .unwrap_err();
    assert!(errors[0].message.contains("already has a field"), "{}", errors[0].message);
}

#[test]
fn an_override_has_to_match_what_it_overrides() {
    let errors = check_src(
        "class A {\n  fn f(self) -> int { return 1; }\n}\n\
         class B : A {\n  fn f(self) -> string { return \"x\"; }\n}\nfn main() {\n}",
    )
    .unwrap_err();
    assert!(errors[0].message.contains("does not match"), "{}", errors[0].message);
}

/// A field and a method of one class may share a name, and this is the one
/// place the compiler is not guessing when two things answer to one: the
/// syntax decides, because a `(` is what makes `a.f` a call.
///
/// Nothing else in the language would let the two be confused — there are
/// no function values, so a method's name in a value position could only
/// ever have meant the field.
#[test]
fn a_field_and_a_method_may_share_a_name_because_the_syntax_tells_them_apart() {
    let checked = check_src(
        "class A {\n  int f;\n  fn f(self) -> int { return self.f * 2; }\n}\n\
         fn main() {\n  A a = A { f: 5 };\n  print(a.f);\n  print(a.f());\n}",
    );
    assert!(checked.is_ok(), "{:?}", checked.err());
}

/// `self` is the receiver's name inside a method and an ordinary field name
/// everywhere else — including on a class whose methods use both.
#[test]
fn a_field_may_be_called_self() {
    let checked = check_src(
        "class A {\n  int self;\n  fn get(self) -> int { return self.self; }\n}\n\
         fn main() {\n  A a = A { self: 5 };\n  print(a.self + a.get());\n}",
    );
    assert!(checked.is_ok(), "{:?}", checked.err());
}

#[test]
fn two_classes_may_both_have_a_method_of_the_same_name() {
    // A method's name lives in its class, not in the program.
    assert!(
        check_src(
            "class A {\n  fn go(self) -> int { return 1; }\n}\n\
             class B {\n  fn go(self) -> int { return 2; }\n}\n\
             fn go() -> int {\n  return 3;\n}\n\
             fn main() {\n  A a = A { };\n  print(a.go() + go());\n}"
        )
        .is_ok()
    );
}

#[test]
fn rejects_self_where_there_is_no_receiver() {
    let errors = check_src("fn f(self) {\n}\nfn main() {\n}").unwrap_err();
    assert!(errors[0].message.contains("`self` outside a class"), "{}", errors[0].message);
    let errors =
        check_src("class A {\n  fn f(int n, self) {\n  }\n}\nfn main() {\n}").unwrap_err();
    assert!(errors[0].message.contains("`self` must come first"), "{}", errors[0].message);
}

// -- what objects are not allowed to do --------------------------------

#[test]
fn an_object_may_be_returned_and_may_be_any_of_its_hierarchy() {
    assert!(
        check_src(
            "class Shape {\n  fn area(self) -> int { return 0; }\n}\n\
             class Circle : Shape {\n  int r;\n  fn area(self) -> int { return self.r; }\n}\n\
             fn make(int n) -> Shape {\n  return Circle { r: n };\n}\n\
             fn main() {\n  Shape s = make(3);\n  print(s.area());\n}"
        )
        .is_ok()
    );
}

#[test]
fn rejects_printing_and_comparing_an_object() {
    let refused = &shape_errors("Circle c = Circle { r: 1 };\nprint(c);")[0];
    assert!(refused.message.contains("cannot print"), "{}", refused.message);
    // An object is not a run of values, so it is not answered like one.
    assert!(
        refused.label.as_ref().is_some_and(|text| text.contains("an object is several")),
        "{:?}",
        refused.label
    );
    assert!(shape_errors("Circle c = Circle { r: 1 };\nprint(c == c);")[0]
        .message
        .contains("cannot be compared"));
}

#[test]
fn a_field_may_be_an_array_or_another_object() {
    let types = check_src(
        "class Point {\n  int x;\n  int y;\n}\n\
         class Segment {\n  Point a;\n  Point b;\n}\n\
         class Board {\n  int[4] cells;\n}\n\
         fn main() {\n}",
    )
    .unwrap();
    let table = types.table();
    // A `Point` is a vtable pointer and two ints, so the second of the two
    // a `Segment` holds starts on the far side of the first.
    let segment = table.class(ClassId(1));
    assert_eq!(segment.field("a").unwrap().offset, 8);
    assert_eq!(segment.field("b").unwrap().offset, 8 + 24);
    assert_eq!(segment.size, 8 + 24 + 24);
    // An array field takes its whole length with it.
    assert_eq!(table.class(ClassId(2)).size, 8 + 4 * 8);
}

#[test]
fn a_subclass_starts_where_its_base_stops_however_big_that_is() {
    let types = check_src(
        "class Point {\n  int x;\n}\n\
         class Base {\n  Point p;\n}\n\
         class Derived : Base {\n  int n;\n}\n\
         fn main() {\n}",
    )
    .unwrap();
    let derived = types.table().class(ClassId(2));
    assert_eq!(derived.field("p").unwrap().offset, 8);
    // The prefix rule with a field wider than a register: `n` starts after
    // the whole `Point`, not eight bytes after it.
    assert_eq!(derived.field("n").unwrap().offset, 8 + 16);
}

#[test]
fn a_field_may_be_a_list_and_costs_the_object_one_pointer() {
    let types = check_src("class Bag {\n  int[] items;\n}\nfn main() {\n}").unwrap();
    let bag = types.table().class(ClassId(0));
    // The elements live in the arena, so what the object holds is their
    // address — one register, like every other field that fits in one.
    assert_eq!(bag.field("items").unwrap().offset, 8);
    assert_eq!(bag.size, 16, "a vtable pointer and one address");
    // And it is what makes copying a `Bag` more than a copy of its bytes.
    assert!(types.table().holds_a_list(Ty::Class(ClassId(0))));
}

/// The one thing a list field buys that nothing else could: a class that
/// contains *itself*, and so a tree.
///
/// By value it is still refused — the object would have to be bigger than
/// itself. Through a list it is not, because the elements are not in the
/// object at all. That is how TinyC gets a recursive type without a
/// reference type and without `null`.
#[test]
fn a_class_may_reach_itself_through_a_list() {
    let types = check_src(
        "class Node {\n  int v;\n  Node[] kids;\n}\nfn main() {\n}",
    )
    .unwrap();
    let node = types.table().class(ClassId(0));
    assert_eq!(node.size, 24, "a vtable pointer, an int and one address");
    assert!(types.table().holds_a_list(Ty::Class(ClassId(0))));
}

#[test]
fn a_class_cannot_contain_itself_however_it_is_reached() {
    for src in [
        // Directly.
        "class Node {\n  Node next;\n}\nfn main() {\n}",
        // Through an array, which holds its elements where it is.
        "class Node {\n  Node[2] children;\n}\nfn main() {\n}",
        // Through another class.
        "class A {\n  B b;\n}\nclass B {\n  A a;\n}\nfn main() {\n}",
        // Through a subclass: every class in a hierarchy reserves the room
        // of the biggest, so holding a `B` is holding an `A`.
        "class A {\n  B b;\n}\nclass B : A {\n}\nfn main() {\n}",
    ] {
        let errors = check_src(src).unwrap_err();
        assert!(errors[0].message.contains("cannot contain"), "{src}: {}", errors[0].message);
    }
}

#[test]
fn a_containment_that_is_merely_deep_is_fine() {
    // The same class held twice is not a cycle: this is a tree, and the
    // walk has to tell the two apart.
    assert!(
        check_src(
            "class Point {\n  int x;\n}\n\
             class Segment {\n  Point a;\n  Point b;\n}\n\
             class Path {\n  Segment[2] parts;\n  Point start;\n}\n\
             fn main() {\n}"
        )
        .is_ok()
    );
}

#[test]
fn an_object_too_big_for_the_frame_is_refused_rather_than_crashing() {
    // Two nested arrays of the longest length allowed is eight megabytes,
    // which is more stack than a thread has.
    let errors = check_src(
        "class Row {\n  int[1024] cells;\n}\n\
         class Grid {\n  Row[1024] rows;\n}\n\
         fn main() {\n}",
    )
    .unwrap_err();
    assert!(errors[0].message.contains("is too big"), "{}", errors[0].message);

    // One level further, the size stops fitting in the number sizes are
    // counted in — and a refusal has to survive that rather than becoming
    // a panic, which is why every step of it saturates.
    let errors = check_src(
        "class Row {\n  int[1024] cells;\n}\n\
         class Grid {\n  Row[1024] rows;\n}\n\
         class Cube {\n  Grid[1024] layers;\n}\n\
         fn main() {\n}",
    )
    .unwrap_err();
    assert!(errors.iter().all(|e| e.message.contains("is too big")), "{errors:?}");

    // And the same for a sum rather than a product: enough big fields that
    // the running total is what stops fitting. `Grid` is recorded at the
    // limit once it has been refused, so what makes these big is the array
    // around it — sixty-four of those is four gigabytes.
    let fields: String = (0..70).map(|at| format!("  Grid[1024] g{at};\n")).collect();
    let errors = check_src(&format!(
        "class Row {{\n  int[1024] cells;\n}}\n\
         class Grid {{\n  Row[1024] rows;\n}}\n\
         class Wide {{\n{fields}}}\n\
         fn main() {{\n}}"
    ))
    .unwrap_err();
    let wide = errors.iter().find(|e| e.message.contains("`Wide`")).expect("`Wide` is refused");
    assert!(
        wide.label.as_ref().is_some_and(|text| text.contains("more than four gigabytes")),
        "{:?}",
        wide.label
    );
}

#[test]
fn rejects_a_field_or_method_on_something_that_is_not_an_object() {
    assert!(errors_in_main("int n = 1;\nprint(n.x);")[0]
        .message
        .contains("cannot read a field"));
    assert!(errors_in_main("int n = 1;\nn.f();")[0]
        .message
        .contains("cannot call a method"));
}

// -- arrays ------------------------------------------------------------

#[test]
fn accepts_an_array_declared_read_written_and_passed() {
    assert!(
        check_src(
            "fn total(int[3] xs) -> int {\n  int sum = 0;\n  \
             for (int i = 0; i < len(xs); i = i + 1) {\n    sum = sum + xs[i];\n  }\n  \
             return sum;\n}\n\
             fn main() {\n  int[3] xs = [1, 2, 3];\n  xs[0] = 9;\n  print(total(xs));\n}"
        )
        .is_ok()
    );
}

#[test]
fn the_length_is_part_of_the_type() {
    // So two arrays of different lengths are different types, and a
    // declaration and its literal have to agree.
    assert!(errors_in_main("int[2] xs = [1, 2, 3];")[0].message.contains("cannot initialize"));
    let errors = check_src(
        "fn f(int[3] xs) {\n}\nfn main() {\n  int[4] xs = [1, 2, 3, 4];\n  f(xs);\n}",
    )
    .unwrap_err();
    assert!(errors[0].message.contains("cannot pass"), "{}", errors[0].message);
}

#[test]
fn every_element_of_a_literal_has_to_agree() {
    let errors = errors_in_main("int[3] xs = [1, true, 3];");
    assert!(errors[0].message.contains("but the ones before it are"), "{}", errors[0].message);
    assert!(errors[0].note.as_ref().unwrap().1.is_some(), "and points at the first");
}

#[test]
fn elements_of_one_hierarchy_settle_on_their_common_ancestor() {
    // The first element does not decide for the rest, which is what makes
    // a mixed collection possible at all.
    assert!(
        check_src(
            "class Shape {\n  fn area(self) -> int { return 0; }\n}\n\
             class Circle : Shape {\n  int r;\n  fn area(self) -> int { return self.r; }\n}\n\
             class Rect : Shape {\n  int w;\n  fn area(self) -> int { return self.w; }\n}\n\
             fn main() {\n  \
             Shape[2] all = [Circle { r: 1 }, Rect { w: 2 }];\n  print(all[0].area());\n}"
        )
        .is_ok()
    );
}

#[test]
fn arrays_stay_invariant_even_though_their_elements_widen() {
    // A `Circle[2]` is not a `Shape[2]`: writing a `Rect` through the
    // second would put one in the first.
    let errors = check_src(
        "class Shape {\n  fn area(self) -> int { return 0; }\n}\n\
         class Circle : Shape {\n  int r;\n  fn area(self) -> int { return self.r; }\n}\n\
         fn take(Shape[2] s) {\n}\n\
         fn main() {\n  Circle[2] cs = [Circle { r: 1 }, Circle { r: 2 }];\n  take(cs);\n}",
    )
    .unwrap_err();
    assert!(errors[0].message.contains("cannot pass"), "{}", errors[0].message);
}

#[test]
fn arrays_of_every_element_type_work() {
    assert!(check_main("string[2] ws = [\"a\", \"b\"];\nprint(ws[0]);").is_ok());
    assert!(check_main("bool[2] bs = [true, false];\nprint(bs[1]);").is_ok());
    assert!(
        check_src("enum A { X, Y }\nfn main() {\n  A[2] as = [A::X, A::Y];\n  print(as[0]);\n}")
            .is_ok()
    );
}

#[test]
fn arrays_do_not_nest_yet() {
    let errors = errors_in_main("int[2] a = [1, 2];\nint[1] b = [a];");
    assert!(errors[0].message.contains("cannot make an array of"), "{}", errors[0].message);
}

// -- what arrays are not allowed to do ---------------------------------

#[test]
fn an_aggregate_may_be_returned_because_the_caller_owns_the_room() {
    // Nothing is handed outward: the caller reserves the room and passes
    // its address in, so the callee copies into what already belongs to it.
    assert!(
        check_src(
            "fn make() -> int[2] {\n  int[2] xs = [1, 2];\n  return xs;\n}\n\
             fn main() {\n  int[2] ys = make();\n  print(ys[0]);\n}"
        )
        .is_ok()
    );
}

#[test]
fn the_hidden_address_costs_one_of_the_argument_registers() {
    // Four parameters is fine for a function that returns a value, and one
    // too many for a function that returns room.
    assert!(
        check_src("fn f(int a, int b, int c, int d) -> int {\n  return a;\n}\nfn main() {\n}")
            .is_ok()
    );
    let errors = check_src(
        "fn f(int a, int b, int c, int d) -> int[2] {\n  int[2] xs = [1, 2];\n  \
         return xs;\n}\nfn main() {\n}",
    )
    .unwrap_err();
    assert!(errors[0].message.contains("at most 3 are supported"), "{}", errors[0].message);
    assert!(
        errors[0].note.as_ref().unwrap().0.contains("carries the address"),
        "{errors:#?}"
    );
}

#[test]
fn rejects_printing_an_array() {
    let errors = errors_in_main("int[2] xs = [1, 2];\nprint(xs);");
    assert!(errors[0].message.contains("cannot print"), "{}", errors[0].message);
}

#[test]
fn arrays_answer_no_comparison_and_no_arithmetic() {
    assert!(errors_in_main("int[2] a = [1, 2];\nprint(a == a);")[0]
        .message
        .contains("cannot be compared"));
    assert!(errors_in_main("int[2] a = [1, 2];\nprint(a + 1);")[0]
        .message
        .contains("cannot apply `+`"));
}

#[test]
fn rejects_indexing_something_that_is_not_an_array() {
    for body in ["int n = 1;\nprint(n[0]);", "int n = 1;\nn[0] = 1;"] {
        let errors = errors_in_main(body);
        assert!(errors[0].message.contains("cannot index"), "{body}: {}", errors[0].message);
    }
    assert!(errors_in_main("int n = 1;
print(len(n));")[0].message.contains("`len` needs"));
}

#[test]
fn an_index_must_be_an_int() {
    let errors = errors_in_main("int[2] xs = [1, 2];\nprint(xs[true]);");
    assert!(errors[0].message.contains("cannot index with"), "{}", errors[0].message);
}

// -- bounds ------------------------------------------------------------

#[test]
fn an_index_the_compiler_can_see_is_checked_at_compile_time() {
    for body in [
        "int[3] xs = [1, 2, 3];\nprint(xs[3]);",
        "int[3] xs = [1, 2, 3];\nprint(xs[-1]);",
        "int[3] xs = [1, 2, 3];\nxs[9] = 1;",
        // Reaches through arithmetic, as the overflow check does.
        "int[3] xs = [1, 2, 3];\nprint(xs[1 + 2]);",
    ] {
        let errors = errors_in_main(body);
        assert!(errors[0].message.contains("out of bounds"), "{body}: {}", errors[0].message);
    }
}

#[test]
fn every_index_the_array_really_has_is_accepted() {
    assert!(check_main("int[3] xs = [1, 2, 3];\nprint(xs[0]);\nprint(xs[2]);").is_ok());
}

#[test]
fn an_index_that_depends_on_a_variable_is_left_to_the_runtime() {
    // `sema` never looks a variable up, so this compiles — and the emitted
    // code carries the check that catches it.
    assert!(check_main("int[3] xs = [1, 2, 3];\nint i = 9;\nprint(xs[i]);").is_ok());
}

#[test]
fn rejects_a_length_no_array_could_have() {
    assert!(errors_in_main("int[0] xs = [1];")[0].message.contains("not a valid array length"));
    let errors = errors_in_main("int[99999] xs = [1];");
    assert!(errors[0].message.contains("not a valid array length"), "{}", errors[0].message);
}

#[test]
fn the_longest_array_is_accepted_and_the_next_one_is_not() {
    // `MAX_ARRAY_LEN` is a limit on the emitted *code* — a literal is
    // unrolled into one store per element — so the boundary is worth
    // pinning: one either way is where an off-by-one would show.
    let elements = vec!["1"; MAX_ARRAY_LEN as usize].join(", ");
    let longest = format!("int[{MAX_ARRAY_LEN}] xs = [{elements}];\nprint(xs[0]);");
    assert!(check_main(&longest).is_ok(), "the longest array should be allowed");

    let too_long = MAX_ARRAY_LEN + 1;
    let errors = errors_in_main(&format!("int[{too_long}] xs = [1];"));
    assert!(errors[0].message.contains("not a valid array length"), "{}", errors[0].message);
}

// -- enums -------------------------------------------------------------

/// A `Colour` enum, a `main`, and whatever body is given, so the tests
/// about enums stay about enums.
fn check_colour(body: &str) -> Result<Types> {
    check_src(&format!("enum Colour {{ Red, Green, Blue }}\nfn main() {{\n{body}\n}}\n"))
}

fn colour_errors(body: &str) -> Vec<Diagnostic> {
    check_colour(body).unwrap_err()
}

#[test]
fn accepts_an_enum_declared_used_and_matched() {
    assert!(
        check_colour(
            "Colour c = Colour::Red;\nmatch (c) {\n  Colour::Red => { print(1); }\n  \
             Colour::Green => { print(2); }\n  Colour::Blue => { print(3); }\n}"
        )
        .is_ok()
    );
}

#[test]
fn an_enum_is_a_type_of_its_own() {
    // Not an int with a different name: nothing converts either way.
    assert!(colour_errors("int n = Colour::Red;")[0].message.contains("cannot initialize"));
    assert!(colour_errors("Colour c = 0;")[0].message.contains("cannot initialize"));
    assert!(colour_errors("Colour c = Colour::Red;\nc = 1;")[0].message.contains("cannot assign"));
}

#[test]
fn arithmetic_and_conditions_reject_an_enum() {
    assert!(colour_errors("print(Colour::Red + 1);")[0].message.contains("cannot apply `+`"));
    assert!(colour_errors("print(-Colour::Red);")[0].message.contains("cannot apply `-`"));
    assert!(colour_errors("print(!Colour::Red);")[0].message.contains("cannot apply `!`"));
    assert!(colour_errors("if (Colour::Red) {\n}")[0].message.contains("must be a `bool`"));
}

#[test]
fn enums_answer_equality_but_not_order() {
    assert!(check_colour("print(Colour::Red == Colour::Blue);").is_ok());
    assert!(check_colour("print(Colour::Red != Colour::Blue);").is_ok());
    // The declaration puts the variants in a sequence, but the program
    // never said that sequence meant anything.
    let errors = colour_errors("print(Colour::Red < Colour::Blue);");
    assert!(errors[0].message.contains("cannot be compared"), "{}", errors[0].message);
    assert!(errors[0].label.as_deref().unwrap().contains("Colour"), "{errors:#?}");
}

#[test]
fn two_enums_are_not_interchangeable() {
    let errors = check_src(
        "enum A { X }\nenum B { X }\nfn main() {\n  A a = A::X;\n  a = B::X;\n}",
    )
    .unwrap_err();
    assert!(errors[0].message.contains("cannot assign"), "{}", errors[0].message);
}

#[test]
fn two_enums_may_share_a_variant_name() {
    // A variant is always written qualified, so there is nothing to
    // disambiguate.
    assert!(
        check_src("enum A { Red }\nenum B { Red }\nfn main() {\n  A a = A::Red;\n  print(a);\n}")
            .is_ok()
    );
}

#[test]
fn rejects_an_unknown_type() {
    for src in [
        "fn main() {\n  Nope n = 1;\n}",
        "fn f(Nope n) {\n}\nfn main() {\n}",
        "fn f() -> Nope {\n}\nfn main() {\n}",
    ] {
        let errors = check_src(src).unwrap_err();
        assert!(errors[0].message.contains("unknown type `Nope`"), "{src}: {}", errors[0].message);
    }
}

#[test]
fn rejects_an_unknown_enum_or_variant() {
    assert!(colour_errors("print(Nope::Red);")[0].message.contains("unknown enum `Nope`"));
    let errors = colour_errors("print(Colour::Purple);");
    assert!(errors[0].message.contains("no variant `Purple`"), "{}", errors[0].message);
    // The note lists the ones it does have, which is the useful half.
    let note = errors[0].note.as_ref().unwrap().0.clone();
    assert!(note.contains("`Red`, `Green` and `Blue`"), "{note}");
}

#[test]
fn rejects_a_duplicate_enum_or_variant() {
    let errors = check_src("enum A { X }\nenum A { Y }\nfn main() {\n}").unwrap_err();
    assert!(errors[0].message.contains("already declared"), "{}", errors[0].message);
    assert!(errors[0].note.as_ref().unwrap().1.is_some());

    let errors = check_src("enum A { X, X }\nfn main() {\n}").unwrap_err();
    assert!(errors[0].message.contains("declared twice"), "{}", errors[0].message);
}

#[test]
fn rejects_an_enum_with_no_variants() {
    // No value could ever have the type, so nothing could be done with it.
    let errors = check_src("enum Void { }\nfn main() {\n}").unwrap_err();
    assert!(errors[0].message.contains("has no variants"), "{}", errors[0].message);
}

#[test]
fn an_enum_may_be_a_parameter_and_a_return_type() {
    assert!(
        check_src(
            "enum A { X, Y }\nfn flip(A a) -> A {\n  match (a) {\n    A::X => { return A::Y; }\n    \
             A::Y => { return A::X; }\n  }\n}\nfn main() {\n  print(flip(A::X));\n}"
        )
        .is_ok()
    );
}

// -- exhaustiveness ----------------------------------------------------

#[test]
fn rejects_a_match_that_misses_a_variant() {
    let errors = colour_errors(
        "Colour c = Colour::Red;\nmatch (c) {\n  Colour::Red => { print(1); }\n}",
    );
    assert!(errors[0].message.contains("does not cover every variant"), "{}", errors[0].message);
    // The label names exactly what is missing, in declaration order.
    let label = errors[0].label.as_deref().unwrap();
    assert!(label.contains("`Green` and `Blue` are not handled"), "{label}");
}

#[test]
fn a_single_missing_variant_reads_as_one() {
    let errors = colour_errors(
        "Colour c = Colour::Red;\nmatch (c) {\n  Colour::Red => { }\n  Colour::Green => { }\n}",
    );
    assert!(
        errors[0].label.as_deref().unwrap().contains("`Blue` is not handled"),
        "{errors:#?}"
    );
}

#[test]
fn an_empty_match_is_missing_everything() {
    let errors = colour_errors("Colour c = Colour::Red;\nmatch (c) {\n}");
    assert!(errors[0].message.contains("does not cover"), "{}", errors[0].message);
}

#[test]
fn rejects_a_variant_covered_twice() {
    let errors = colour_errors(
        "Colour c = Colour::Red;\nmatch (c) {\n  Colour::Red => { }\n  Colour::Red => { }\n  \
         Colour::Green => { }\n  Colour::Blue => { }\n}",
    );
    assert!(errors[0].message.contains("already covered"), "{}", errors[0].message);
    assert!(errors[0].note.as_ref().unwrap().1.is_some(), "and points at the first arm");
}

#[test]
fn rejects_an_arm_belonging_to_another_enum() {
    let errors = check_src(
        "enum A { X }\nenum B { Y }\nfn main() {\n  A a = A::X;\n  match (a) {\n    \
         B::Y => { }\n  }\n}",
    )
    .unwrap_err();
    assert!(errors[0].message.contains("but the value is a `A`"), "{}", errors[0].message);
}

/// A `match` needs something a pattern can be compared *to*. An array, a
/// list and an object are exactly the types with no equality, so there is
/// nothing a pattern could ask of one.
#[test]
fn rejects_a_match_on_something_no_pattern_could_be_about() {
    for body in [
        "int[3] xs = [1, 2, 3];\nmatch (xs) {\n  _ => { }\n}",
        "int[] ys = [1];\nmatch (ys) {\n  _ => { }\n}",
    ] {
        let errors = errors_in_main(body);
        assert!(errors[0].message.contains("cannot match on"), "{body}: {}", errors[0].message);
    }
    let errors = check_src(
        "class P { int x; }\nfn main() {\n  P p = P { x: 1 };\n  match (p) {\n    _ => { }\n  }\n}",
    )
    .unwrap_err();
    assert!(errors[0].message.contains("cannot match on"), "{}", errors[0].message);
}

/// The four that are not enums and *can* be matched, each needing the
/// catch-all that says what is left — except `bool`, whose two values a
/// program can simply write out.
#[test]
fn a_match_on_a_value_with_no_list_of_variants_needs_a_catch_all() {
    for body in [
        "int n = 1;\nprint(match (n) {\n  1 => 1,\n});",
        "char c = 'a';\nprint(match (c) {\n  'a' => 1,\n});",
        "string s = \"a\";\nprint(match (s) {\n  \"a\" => 1,\n});",
    ] {
        let errors = errors_in_main(body);
        assert!(
            errors[0].message.contains("does not cover every"),
            "{body}: {}",
            errors[0].message
        );
    }
    // With one, each is accepted.
    for body in [
        "int n = 1;\nprint(match (n) {\n  1 => 1,\n  _ => 0,\n});",
        "char c = 'a';\nprint(match (c) {\n  'a' => 1,\n  _ => 0,\n});",
        "string s = \"a\";\nprint(match (s) {\n  \"a\" => 1,\n  _ => 0,\n});",
        // A `bool` has two values and they can be written out, so it needs
        // no catch-all — though it may have one.
        "bool b = true;\nprint(match (b) {\n  true => 1,\n  false => 0,\n});",
        "bool b = true;\nprint(match (b) {\n  true => 1,\n  _ => 0,\n});",
    ] {
        assert!(errors_in_main_none(body), "{body}");
    }
}

/// What a variant carries is checked exactly as a call's arguments are,
/// and named exactly as many times in the pattern that takes it apart.
#[test]
fn a_variant_takes_what_it_declares_and_a_pattern_names_all_of_it() {
    let with = |body: &str| {
        format!("enum E {{ A(int), B(int, string), C }}\nfn main() {{\n{body}\n}}\n")
    };
    let refuses = |body: &str, message: &str| {
        let errors = check_src(&with(body)).unwrap_err();
        assert!(
            errors.iter().any(|d| d.message.contains(message)),
            "{body}: {:#?}",
            errors.iter().map(|d| &d.message).collect::<Vec<_>>()
        );
    };

    refuses("E e = E::A(1, 2);", "was given 2 values");
    refuses("E e = E::A();", "was given 0 values");
    refuses("E e = E::C(1);", "was given 1 value");
    refuses("E e = E::A(\"x\");", "carries an `int` here");
    refuses(
        "E e = E::A(1);\nprint(match (e) { E::A(x, y) => x, E::B(a, b) => a, E::C => 0 });",
        "names 2 values",
    );
    refuses(
        "E e = E::A(1);\nprint(match (e) { E::A => 1, E::B(a, b) => a, E::C => 0 });",
        "names 0 values",
    );

    // And the shapes that are right.
    assert!(check_src(&with("E e = E::C;\nprint(\"%e\", e);")).is_ok());
    assert!(
        check_src(&with(
            "E e = E::B(1, \"x\");\n\
             print(match (e) { E::A(n) => n, E::B(n, s) => n + len(s), E::C => 0 });"
        ))
        .is_ok()
    );
}

/// A payload has to fit in a register: an object or an array would have to
/// live inside the value, which would give an enum a size of its own.
#[test]
fn a_variant_cannot_carry_something_that_does_not_fit_in_a_register() {
    for payload in ["P", "int[3]"] {
        let src = format!(
            "class P {{ int x; }}\nenum E {{ A({payload}), B }}\nfn main() {{\n}}\n"
        );
        let errors = check_src(&src).unwrap_err();
        assert!(
            errors[0].message.contains("a variant cannot carry"),
            "{payload}: {}",
            errors[0].message
        );
    }
    // A list does fit, and is allowed: an enum is read-only, so what goes
    // in is copied in and what a pattern hands back is copied out.
    assert!(
        check_src("enum E { A(int[]), B }\nfn main() {\n}\n").is_ok(),
        "a list payload is one pointer like any other"
    );
}

/// Two values of a payload-carrying enum are equal when they carry equal
/// things, and comparing the two pointers would answer something else.
#[test]
fn an_enum_that_carries_something_cannot_be_compared() {
    let errors = check_src(
        "enum E { A(int), B }\nfn main() {\n  E e = E::A(1);\n  \
         if (e == E::B) {\n  }\n}\n",
    )
    .unwrap_err();
    assert!(errors[0].message.contains("cannot be compared with `==`"), "{}", errors[0].message);
    // One whose variants carry nothing still compares, exactly as before.
    assert!(
        check_src(
            "enum E { A, B }\nfn main() {\n  E e = E::A;\n  if (e == E::B) {\n  }\n}\n"
        )
        .is_ok()
    );
}

/// A pattern's names belong to its arm and to nothing else, which is what
/// lets two arms call quite different things by one name.
#[test]
fn what_a_pattern_names_is_in_scope_for_its_arm_alone() {
    let src = "enum E { A(int), B(string) }\nfn main() {\n  E e = E::A(1);\n  \
               match (e) {\n    E::A(v) => { print(v); }\n    E::B(v) => { print(v); }\n  }\n}\n";
    assert!(check_src(src).is_ok(), "one name, two types, two arms");

    let leaks = "enum E { A(int), B }\nfn main() {\n  E e = E::A(1);\n  \
                 match (e) {\n    E::A(v) => { print(v); }\n    E::B => { }\n  }\n  \
                 print(v);\n}\n";
    let errors = check_src(leaks).unwrap_err();
    assert!(errors[0].message.contains("undeclared variable `v`"), "{}", errors[0].message);
}

/// The catch-all stops exactly at the type where it would do harm.
#[test]
fn an_enum_still_has_no_catch_all() {
    let errors =
        colour_errors("Colour c = Colour::Red;\nprint(match (c) {\n  _ => 1,\n});");
    assert!(errors[0].message.contains("`_` cannot be used here"), "{}", errors[0].message);
    assert!(
        errors[0].note.as_ref().is_some_and(|(text, _)| text.contains("adding a variant")),
        "the note says why: {:?}",
        errors[0].note
    );
}

#[test]
fn a_literal_pattern_is_held_to_the_scrutinee_s_type() {
    let errors = errors_in_main("int n = 1;\nprint(match (n) {\n  'a' => 1,\n  _ => 0,\n});");
    assert!(
        errors[0].message.contains("this arm matches a `char`, but the value is an `int`"),
        "{}",
        errors[0].message
    );
}

#[test]
fn two_arms_that_select_the_same_value_are_refused() {
    for body in [
        "int n = 1;\nprint(match (n) {\n  1 => 1,\n  1 => 2,\n  _ => 0,\n});",
        "string s = \"a\";\nprint(match (s) {\n  \"a\" => 1,\n  \"a\" => 2,\n  _ => 0,\n});",
        "bool b = true;\nprint(match (b) {\n  true => 1,\n  true => 2,\n  _ => 0,\n});",
    ] {
        let errors = errors_in_main(body);
        assert!(
            errors[0].message.contains("is already covered"),
            "{body}: {}",
            errors[0].message
        );
    }
}

#[test]
fn nothing_may_follow_the_catch_all() {
    let errors = errors_in_main("int n = 1;\nprint(match (n) {\n  _ => 0,\n  1 => 1,\n});");
    assert!(errors[0].message.contains("this arm can never run"), "{}", errors[0].message);
}

#[test]
fn an_arm_body_is_checked_whatever_is_wrong_with_its_pattern() {
    // A mistake inside an arm is worth reporting even when the arm itself
    // will never run.
    let errors = colour_errors(
        "Colour c = Colour::Red;\nmatch (c) {\n  Colour::Purple => { print(nope); }\n}",
    );
    assert!(errors.iter().any(|d| d.message.contains("undeclared variable `nope`")), "{errors:#?}");
}

// -- match as an expression --------------------------------------------

#[test]
fn a_match_of_values_is_an_expression_of_their_type() {
    assert!(
        check_colour(
            "string s = match (Colour::Red) {\n  Colour::Red => \"warm\",\n  \
             Colour::Green => \"cool\",\n  Colour::Blue => \"cold\",\n};\nprint(s);"
        )
        .is_ok()
    );
    // And is checked against what wanted it.
    let errors = colour_errors(
        "int n = match (Colour::Red) {\n  Colour::Red => \"warm\",\n  \
         Colour::Green => \"cool\",\n  Colour::Blue => \"cold\",\n};",
    );
    assert!(errors[0].message.contains("cannot initialize"), "{}", errors[0].message);
}

#[test]
fn every_arm_of_a_match_has_to_agree() {
    let errors = colour_errors(
        "string s = match (Colour::Red) {\n  Colour::Red => \"warm\",\n  \
         Colour::Green => 1,\n  Colour::Blue => \"cold\",\n};",
    );
    assert!(errors[0].message.contains("but an earlier one produces"), "{}", errors[0].message);
    // The note points back at the arm that set the type.
    assert!(errors[0].note.as_ref().unwrap().1.is_some(), "{errors:#?}");
}

#[test]
fn a_block_arm_is_admissible_in_value_position_only_if_it_diverges() {
    // `return` keeps its one meaning, so a block hands nothing back — it
    // has to be one control never falls out of.
    assert!(
        check_colour(
            "string s = match (Colour::Red) {\n  Colour::Red => \"warm\",\n  \
             Colour::Green => { print(\"x\"); return; }\n  Colour::Blue => \"cold\",\n};"
        )
        .is_ok()
    );
    let errors = colour_errors(
        "string s = match (Colour::Red) {\n  Colour::Red => \"warm\",\n  \
         Colour::Green => { print(\"x\"); }\n  Colour::Blue => \"cold\",\n};",
    );
    assert!(errors[0].message.contains("produces no value"), "{}", errors[0].message);
}

#[test]
fn a_break_or_a_continue_makes_a_block_arm_diverge_too() {
    // The question is whether control can reach the end of the block, and
    // a loop jump answers it as well as a `return` does.
    assert!(
        check_colour(
            "while (true) {\n  int n = match (Colour::Red) {\n    Colour::Red => 1,\n    \
             Colour::Green => { break; }\n    Colour::Blue => { continue; }\n  };\n  print(n);\n}"
        )
        .is_ok()
    );
}

#[test]
fn a_match_where_every_arm_leaves_produces_nothing() {
    let errors = colour_errors(
        "int n = match (Colour::Red) {\n  Colour::Red => { return; }\n  \
         Colour::Green => { return; }\n  Colour::Blue => { return; }\n};",
    );
    assert!(errors[0].message.contains("produces no value"), "{}", errors[0].message);
}

#[test]
fn a_value_arm_has_nowhere_to_go_in_statement_position() {
    // TinyC discards no values; a match written as a statement runs its
    // arms for effect.
    let errors = colour_errors(
        "match (Colour::Red) {\n  Colour::Red => 1,\n  Colour::Green => 2,\n  \
         Colour::Blue => 3,\n}",
    );
    assert!(errors[0].message.contains("nothing uses it"), "{}", errors[0].message);
}

#[test]
fn a_match_of_blocks_is_still_a_statement() {
    assert!(
        check_colour(
            "match (Colour::Red) {\n  Colour::Red => { print(1); }\n  \
             Colour::Green => { print(2); }\n  Colour::Blue => { print(3); }\n}"
        )
        .is_ok()
    );
}

#[test]
fn a_match_expression_is_still_checked_for_exhaustiveness() {
    // The two checks are independent: neither form escapes either.
    let errors = colour_errors("string s = match (Colour::Red) {\n  Colour::Red => \"a\",\n};");
    assert!(errors[0].message.contains("does not cover"), "{}", errors[0].message);
}

#[test]
fn a_match_may_be_the_scrutinee_of_another() {
    assert!(
        check_src(
            "enum A { X, Y }\nfn main() {\n  A a = match (A::X) {\n    A::X => A::Y,\n    \
             A::Y => A::X,\n  };\n  match (a) {\n    A::X => { print(1); }\n    \
             A::Y => { print(2); }\n  }\n}"
        )
        .is_ok()
    );
}

#[test]
fn an_exhaustive_match_counts_as_returning() {
    // The payoff of the check: no trailing `return` is needed, and none
    // would be reachable.
    assert!(
        check_src(
            "enum A { X, Y }\nfn f(A a) -> int {\n  match (a) {\n    A::X => { return 1; }\n    \
             A::Y => { return 2; }\n  }\n}\nfn main() {\n  print(f(A::X));\n}"
        )
        .is_ok()
    );
}

#[test]
fn a_match_with_one_arm_not_returning_does_not_count() {
    let errors = check_src(
        "enum A { X, Y }\nfn f(A a) -> int {\n  match (a) {\n    A::X => { return 1; }\n    \
         A::Y => { print(2); }\n  }\n}\nfn main() {\n}",
    )
    .unwrap_err();
    assert!(errors[0].message.contains("may finish without returning"), "{}", errors[0].message);
}

#[test]
fn break_and_continue_work_inside_an_arm() {
    assert!(
        check_src(
            "enum A { X, Y }\nfn main() {\n  while (true) {\n    A a = A::X;\n    \
             match (a) {\n      A::X => { break; }\n      A::Y => { continue; }\n    }\n  }\n}"
        )
        .is_ok()
    );
    // And are still rejected when there is no loop around them.
    let errors = check_src(
        "enum A { X }\nfn main() {\n  A a = A::X;\n  match (a) {\n    A::X => { break; }\n  }\n}",
    )
    .unwrap_err();
    assert!(errors[0].message.contains("outside of a loop"), "{}", errors[0].message);
}

#[test]
fn an_arm_is_a_scope_of_its_own() {
    let errors = check_src(
        "enum A { X, Y }\nfn main() {\n  A a = A::X;\n  match (a) {\n    \
         A::X => { int n = 1; print(n); }\n    A::Y => { print(n); }\n  }\n}",
    )
    .unwrap_err();
    assert!(errors[0].message.contains("undeclared variable `n`"), "{}", errors[0].message);
}

// -- functions ---------------------------------------------------------

#[test]
fn accepts_a_call_with_matching_arguments() {
    assert!(
        check_src(
            "fn add(int a, int b) -> int {\n  return a + b;\n}\n\
             fn main() {\n  print(add(1, 2));\n}"
        )
        .is_ok()
    );
}

#[test]
fn a_function_may_be_called_before_it_is_declared() {
    // This is what the first pass buys: `helper` is in the table before
    // `main`'s body is looked at.
    assert!(
        check_src(
            "fn main() {\n  print(helper());\n}\n\
             fn helper() -> int {\n  return 1;\n}"
        )
        .is_ok()
    );
}

#[test]
fn a_function_may_call_itself() {
    assert!(
        check_src(
            "fn fib(int n) -> int {\n  if (n < 2) {\n    return n;\n  } else {\n    \
             return fib(n - 1) + fib(n - 2);\n  }\n}\n\
             fn main() {\n  print(fib(10));\n}"
        )
        .is_ok()
    );
}

#[test]
fn parameters_are_visible_in_the_body() {
    assert!(check_src("fn f(int a) {\n  print(a);\n}\nfn main() {\n  f(1);\n}").is_ok());
}

#[test]
fn a_parameter_does_not_escape_its_function() {
    let errors =
        check_src("fn f(int a) {\n  print(a);\n}\nfn main() {\n  print(a);\n}").unwrap_err();
    assert!(errors[0].message.contains("undeclared variable `a`"));
}

#[test]
fn rejects_a_local_that_collides_with_a_parameter() {
    let errors = check_src("fn f(int a) {\n  int a = 1;\n}\nfn main() {\n}").unwrap_err();
    assert!(errors[0].message.contains("already declared"), "{}", errors[0].message);
}

#[test]
fn rejects_two_parameters_with_the_same_name() {
    let errors = check_src("fn f(int a, int a) {\n}\nfn main() {\n}").unwrap_err();
    assert!(errors[0].message.contains("already declared"), "{}", errors[0].message);
}

#[test]
fn rejects_an_unknown_callee() {
    let errors = check_src("fn main() {\n  nope();\n}").unwrap_err();
    assert!(errors[0].message.contains("unknown function `nope`"));
}

#[test]
fn rejects_the_wrong_number_of_arguments() {
    let errors =
        check_src("fn add(int a, int b) -> int {\n  return a + b;\n}\nfn main() {\n  print(add(1));\n}")
            .unwrap_err();
    assert!(errors[0].message.contains("takes 2 arguments"), "{}", errors[0].message);
}

#[test]
fn rejects_an_argument_of_the_wrong_type() {
    let errors =
        check_src("fn f(int a) {\n}\nfn main() {\n  f(\"hi\");\n}").unwrap_err();
    assert!(errors[0].message.contains("cannot pass"), "{}", errors[0].message);
}

#[test]
fn rejects_two_functions_with_the_same_name() {
    let errors = check_src("fn f() {\n}\nfn f() {\n}\nfn main() {\n}").unwrap_err();
    assert!(errors[0].message.contains("already defined"), "{}", errors[0].message);
    assert!(errors[0].note.as_ref().unwrap().1.is_some());
}

#[test]
fn rejects_more_than_four_parameters() {
    let errors =
        check_src("fn f(int a, int b, int c, int d, int e) {\n}\nfn main() {\n}").unwrap_err();
    assert!(errors[0].message.contains("at most 4"), "{}", errors[0].message);
}

#[test]
fn accepts_exactly_four_parameters() {
    assert!(check_src("fn f(int a, int b, int c, int d) {\n}\nfn main() {\n}").is_ok());
}

#[test]
fn rejects_a_program_without_main() {
    let errors = check_src("fn f() {\n}").unwrap_err();
    assert!(errors[0].message.contains("no `main` function"), "{}", errors[0].message);
}

#[test]
fn rejects_a_main_that_takes_parameters_or_returns() {
    assert!(
        check_src("fn main(int a) {\n}").unwrap_err()[0]
            .message
            .contains("must not take parameters")
    );
    assert!(
        check_src("fn main() -> int {\n  return 0;\n}").unwrap_err()[0]
            .message
            .contains("must not return a value")
    );
}

// -- returns -----------------------------------------------------------

#[test]
fn rejects_a_return_value_of_the_wrong_type() {
    let errors = check_src("fn f() -> int {\n  return \"hi\";\n}\nfn main() {\n}").unwrap_err();
    assert!(errors[0].message.contains("cannot return"), "{}", errors[0].message);
}

#[test]
fn rejects_a_bare_return_from_a_function_with_a_return_type() {
    let errors = check_src("fn f() -> int {\n  return;\n}\nfn main() {\n}").unwrap_err();
    assert!(errors[0].message.contains("needs a value"), "{}", errors[0].message);
}

#[test]
fn rejects_returning_a_value_from_a_void_function() {
    let errors = check_src("fn f() {\n  return 1;\n}\nfn main() {\n}").unwrap_err();
    assert!(errors[0].message.contains("returns nothing"), "{}", errors[0].message);
}

#[test]
fn a_bare_return_is_fine_in_a_void_function() {
    assert!(check_src("fn f() {\n  return;\n}\nfn main() {\n}").is_ok());
}

#[test]
fn rejects_a_body_that_can_finish_without_returning() {
    let errors =
        check_src("fn f(int n) -> int {\n  if (n > 0) {\n    return 1;\n  }\n}\nfn main() {\n}")
            .unwrap_err();
    assert!(errors[0].message.contains("may finish without returning"), "{}", errors[0].message);
}

#[test]
fn both_arms_of_an_if_else_count_as_returning() {
    assert!(
        check_src(
            "fn f(int n) -> int {\n  if (n > 0) {\n    return 1;\n  } else {\n    \
             return 2;\n  }\n}\nfn main() {\n}"
        )
        .is_ok()
    );
}

#[test]
fn a_loop_is_never_assumed_to_run() {
    // Conservative on purpose: this program is in fact fine, but proving it
    // needs more than the syntax.
    let errors = check_src(
        "fn f() -> int {\n  while (true) {\n    return 1;\n  }\n}\nfn main() {\n}",
    )
    .unwrap_err();
    assert!(errors[0].message.contains("may finish without returning"));
}

// -- void in expression position ---------------------------------------

#[test]
fn a_void_call_is_a_statement_but_not_a_value() {
    assert!(check_src("fn greet() {\n}\nfn main() {\n  greet();\n}").is_ok());
    let errors = check_src("fn greet() {\n}\nfn main() {\n  int n = greet();\n}").unwrap_err();
    assert!(errors[0].message.contains("returns nothing"), "{}", errors[0].message);
}

#[test]
fn a_returning_call_may_be_used_as_a_statement() {
    // The value is simply discarded, exactly as in C.
    assert!(
        check_src("fn f() -> int {\n  return 1;\n}\nfn main() {\n  f();\n}").is_ok()
    );
}

// -- format strings ----------------------------------------------------

/// Each specifier accepts exactly the type it names, and refuses the other
/// four. Written as a matrix rather than as one test per pair, so a
/// specifier that quietly accepted the wrong type could not hide.
#[test]
fn a_specifier_accepts_its_own_type_and_no_other() {
    let value_of = [
        (Spec::Prim(Prim::Int), "1"),
        (Spec::Prim(Prim::Char), "'a'"),
        (Spec::Prim(Prim::Str), "\"s\""),
        (Spec::Prim(Prim::Bool), "true"),
        (Spec::Enum, "Colour::Red"),
    ];
    for spec in crate::ast::Spec::all() {
        for (of, value) in value_of {
            let src = format!(
                "enum Colour {{ Red, Green }}\n\
                 fn main() {{\n  println(\"%{}\", {value});\n}}\n",
                spec.letter()
            );
            let checked = check_src(&src);
            assert_eq!(
                checked.is_ok(),
                spec == of,
                "`%{}` against {value}",
                spec.letter()
            );
        }
    }
}

/// The message names both halves of the disagreement, and the note points
/// back at the specifier that asked.
#[test]
fn a_mismatch_says_what_was_asked_for_and_what_arrived() {
    let errors = errors_in_main("println(\"n = %d\", \"hi\");");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].message, "cannot write a `string` with `%d`");
    assert_eq!(errors[0].label.as_deref(), Some("`%d` writes an int"));
    assert!(errors[0].note.as_ref().expect("a note").1.is_some(), "the note has a span");
}

/// No conversion is offered to rescue a mismatch. A format string is the
/// one place a number most wants to become text on its own, and it does not
/// — which is the same answer `int n = s;` gets.
#[test]
fn a_format_offers_no_conversion() {
    assert!(!errors_in_main("println(\"%s\", 1);").is_empty());
    assert!(check_main("println(\"%s\", string(1));").is_ok());
}

/// A value with no rendering is refused under a specifier too, and the
/// message is the mismatch rather than the older "cannot print" — nothing
/// promised a list, so nothing has to explain that a list has no rendering.
#[test]
fn a_list_is_refused_under_a_specifier() {
    let errors = errors_in_main("int[] xs = [1];\nprintln(\"%s\", xs);");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].message, "cannot write an `int[]` with `%s`");
}

/// And on its own it still gets the older answer, which explains what to do
/// instead.
#[test]
fn a_list_on_its_own_is_still_told_to_loop() {
    let errors = errors_in_main("int[] xs = [1];\nprintln(xs);");
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(errors[0].message, "cannot print an `int[]`");
}

/// Every value in a format is checked, not just the first: one message per
/// mistake here as everywhere.
#[test]
fn every_value_in_a_format_is_checked() {
    let errors = errors_in_main("println(\"%d %d %d\", \"a\", 1, true);");
    assert_eq!(errors.len(), 2, "{errors:?}");
}

// -- the primitive table, end to end ------------------------------------

/// Every type in [`Prim::ALL`] is a type the whole front end agrees about.
///
/// A parameter and a return type is enough to exercise the lot without
/// naming a single literal: the parser has to accept the keyword where a
/// type goes, twice; `resolve_type` has to know the name; and the return
/// check has to compare the type against itself. So a row added to
/// `Prim::ALL` without the type being wired up fails **here**, rather than
/// in the first program someone writes with it.
///
/// It is deliberately free of any per-type table of its own. One would be
/// exactly the thing this refactor removed, put back in the test.
#[test]
fn every_primitive_type_declares_a_parameter_and_a_return() {
    for prim in Prim::ALL {
        let name = prim.name();
        let src = format!("fn same({name} x) -> {name} {{\n  return x;\n}}\nfn main() {{\n}}\n");
        if let Err(errors) = check_src(&src) {
            panic!("`{name}` does not work as a type: {errors:#?}");
        }
    }
}

/// And every one of them is read as a conversion where a value goes, which
/// is the other half of what `Prim::of_keyword` is asked.
///
/// Only that it *parses*: `x` is undeclared and several of these
/// conversions do not exist, so `sema` has plenty to say. A keyword the
/// parser did not recognise would not get that far — it would be "expected
/// an expression", before any of this ran.
#[test]
fn every_primitive_type_is_read_as_a_conversion() {
    for prim in Prim::ALL {
        let src = format!("fn main() {{\n  print({}(x));\n}}\n", prim.name());
        let parsed = parse(&lex(&src).expect("it lexes"));
        assert!(parsed.is_ok(), "`{}(x)` is not read as a conversion", prim.name());
    }
}

/// The keyword cannot be taken as a variable name either — the other thing
/// being in [`Prim::ALL`] is supposed to guarantee.
#[test]
fn no_primitive_type_can_be_used_as_a_name() {
    for prim in Prim::ALL {
        let src = format!("fn main() {{\n  int {} = 1;\n}}\n", prim.name());
        assert!(check_src(&src).is_err(), "`{}` was accepted as a name", prim.name());
    }
}

// -- float --------------------------------------------------------------

/// Everything an `int` does arithmetically, a `float` does too, and unary
/// minus keeps the type rather than answering an `int`.
#[test]
fn a_float_does_arithmetic_and_comparison() {
    assert!(errors_in_main_none(
        "float a = 1.5;\nfloat b = 2.0;\n\
         float c = a + b - a * b / a;\nfloat d = -c;\n\
         bool ordered = d < c && d <= c && c > d && c >= d;\n\
         bool same = c == d || c != d;\nprintln(\"%f %b %b\", c, ordered, same);"
    ));
}

/// Nothing widens on its own, and the message says how to write what was
/// meant rather than only that it was not written.
#[test]
fn an_int_and_a_float_do_not_mix() {
    for body in ["float f = 1.5 + 1;", "float f = 1 + 1.5;"] {
        let errors = errors_in_main(body);
        assert_eq!(errors.len(), 1, "{body}: {errors:#?}");
        assert!(errors[0].message.starts_with("cannot apply `+`"), "{errors:#?}");
        assert!(
            errors[0].note.as_ref().unwrap().0.contains("`float(n)`"),
            "{body}: {errors:#?}"
        );
    }
}

/// A declaration does not widen either: the two are separate types, not two
/// widths of one.
#[test]
fn a_float_variable_refuses_an_int() {
    assert_eq!(errors_in_main("float f = 1;").len(), 1);
    assert_eq!(errors_in_main("int n = 1.0;").len(), 1);
}

/// `%` is `idiv`'s other answer, and a float division has no other half.
#[test]
fn a_float_has_no_remainder() {
    let errors = errors_in_main("float f = 5.5 % 2.0;");
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert_eq!(errors[0].message, "`%` has no meaning on `float`");
}

/// Both directions are written out, and both are accepted.
#[test]
fn a_float_and_an_int_convert_when_asked() {
    assert!(errors_in_main_none("int n = 3;\nfloat f = float(n);\nprintln(int(f) + n);"));
}

/// There is no `string(f)`, and the message points at what does write one
/// rather than leaving a list of conversions to be read as a refusal.
#[test]
fn a_float_is_written_rather_than_stringified() {
    let errors = errors_in_main("float f = 1.5;\nstring s = string(f);");
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert_eq!(errors[0].message, "there is no conversion from `float` to `string`");
    assert!(errors[0].note.as_ref().unwrap().0.contains("%f"), "{errors:#?}");
}

/// A constant that has no `int` is settled here, exactly as a constant
/// character with no scalar value is.
#[test]
fn a_constant_float_with_no_int_is_refused() {
    for body in ["int n = int(100000000000000000000.0);", "int n = int(1.0 / 0.0);"] {
        let errors = errors_in_main(body);
        assert_eq!(errors.len(), 1, "{body}: {errors:#?}");
        assert!(errors[0].message.contains("has no `int`"), "{body}: {errors:#?}");
    }
    // The two ends of the range, which do have one. Truncation is toward
    // zero, so the bottom is exact and the top is one step short of `2^63`.
    assert!(errors_in_main_none("int n = int(0.0 - 9223372036854775808.0);"));
    assert!(errors_in_main_none("int n = int(9223372036854775295.0);"));
}

/// A `match` is an equality test, and equality is the one question about a
/// float that almost never means what it looks like.
#[test]
fn a_float_cannot_be_matched_on() {
    let errors = errors_in_main("float f = 1.5;\nmatch (f) {\n  1.5 => { }\n}");
    assert_eq!(errors[0].message, "cannot match on a `float`");
    assert!(errors[0].note.as_ref().unwrap().0.contains("NaN"), "{errors:#?}");
}

/// And a float *pattern* against something else is the ordinary mismatch,
/// which is why the pattern still exists.
#[test]
fn a_float_pattern_is_refused_against_an_int() {
    let errors = errors_in_main("int n = 1;\nmatch (n) {\n  1.5 => { }\n  _ => { }\n}");
    assert!(!errors.is_empty(), "a float pattern says nothing about an int");
}
