use super::*;
use crate::ast::{BinOp, ClassId, CmpOp, Ty};
use crate::diag::Result;
use crate::target::Machine;
use crate::{lexer, parser, sema};

fn lower_src(src: &str) -> Program {
    try_lower(src).expect("the frames should fit")
}

fn try_lower(src: &str) -> Result<Program> {
    let ast = parser::parse(&lexer::lex(src).unwrap()).unwrap();
    let types = sema::check(&ast, crate::target::Machine::TEST).unwrap();
    lower(&ast, &types)
}

/// Lower a `main` body and return that one function.
fn lower_main(body: &str) -> Program {
    lower_src(&format!("fn main() {{\n{body}\n}}\n"))
}

/// The dump of a single-function program, without its signature line and
/// trailing blank, so the existing block-shape assertions stay readable.
fn body_dump(program: &Program) -> String {
    let dump = program.dump();
    let start = dump.find(":\n").expect("a signature line") + 2;
    dump[start..].trim_end().to_string() + "\n"
}

fn labels(function: &Function) -> Vec<String> {
    function.blocks.iter().map(|b| b.label()).collect()
}

#[test]
fn lowers_the_sample_program() {
    let ir = lower_main("int x = 10;\nint y = 20;\nstring s = \"hi\";\nprint(x + y);\nprint(s);");
    assert_eq!(
        body_dump(&ir),
        concat!(
            "entry0:\n",
            "  0  %x = const 10\n",
            "  1  %y = const 20\n",
            "  2  %s = straddr str0\n",
            "  3  %t3 = add %x, %y\n",
            "  4  print int %t3\n",
            "  5  print string %s\n",
            "  6  return\n",
        )
    );
}

#[test]
fn an_assignment_writes_the_variables_own_register() {
    // No `%n.1`: with control flow a variable must have one home, so the
    // second write targets the same register.
    let ir = lower_main("int n = 1;\nn = n + 41;\nprint(n);");
    assert_eq!(
        body_dump(&ir),
        concat!(
            "entry0:\n",
            "  0  %n = const 1\n",
            "  1  %n = add %n, 41\n",
            "  2  print int %n\n",
            "  3  return\n",
        )
    );
}

#[test]
fn an_if_produces_a_diamond() {
    let ir = lower_main("int n = 0;\nif (n < 1) {\n  n = 2;\n} else {\n  n = 3;\n}\nprint(n);");
    let main = &ir.functions[0];
    assert_eq!(labels(main), vec!["entry0", "then1", "else2", "join3"]);
    assert!(matches!(main.blocks[0].term, Terminator::Branch { .. }));
    assert!(matches!(main.blocks[1].term, Terminator::Jump(Target { block: BlockId(3), .. })));
    assert!(matches!(main.blocks[2].term, Terminator::Jump(Target { block: BlockId(3), .. })));
}

#[test]
fn an_if_without_else_branches_straight_to_the_join() {
    let ir = lower_main("int n = 0;\nif (n < 1) {\n  n = 2;\n}\nprint(n);");
    let main = &ir.functions[0];
    assert_eq!(labels(main), vec!["entry0", "then1", "join2"]);
    match &main.blocks[0].term {
        Terminator::Branch { then_blk, else_blk, .. } => {
            assert_eq!((then_blk.block, else_blk.block), (BlockId(1), BlockId(2)));
        }
        other => panic!("expected a branch, got {other:?}"),
    }
}

#[test]
fn a_while_loop_closes_a_back_edge() {
    let ir = lower_main("int i = 0;\nwhile (i < 3) {\n  i = i + 1;\n}\nprint(i);");
    let main = &ir.functions[0];
    assert_eq!(labels(main), vec!["entry0", "loop1", "body2", "done3"]);
    // The body jumps back to the header, which re-tests the condition.
    assert!(matches!(main.blocks[2].term, Terminator::Jump(Target { block: BlockId(1), .. })));
    assert!(matches!(main.blocks[0].term, Terminator::Jump(Target { block: BlockId(1), .. })));
}

#[test]
fn a_for_loop_desugars_into_the_same_shape() {
    let with_for = lower_main("for (int i = 0; i < 3; i = i + 1) {\n  print(i);\n}");
    let with_while = lower_main("int i = 0;\nwhile (i < 3) {\n  print(i);\n  i = i + 1;\n}");
    assert_eq!(with_for.dump(), with_while.dump());
}

// -- classes -----------------------------------------------------------

#[test]
fn an_object_is_room_a_vtable_pointer_and_its_fields() {
    let ir = lower_src(
        "class Circle {\n  int r;\n  fn area(self) -> int { return self.r; }\n}\n\
         fn main() {\n  Circle c = Circle { r: 5 };\n  print(c.r);\n}",
    );
    let main = ir.functions.iter().find(|f| f.name == "main").expect("main survives");
    let text: Vec<String> =
        main.blocks[0].instrs.iter().map(|i| ir.instr_text(main, i)).collect();
    assert_eq!(
        text,
        vec![
            "%c = frame 0",
            "%t1 = vtable Circle",
            "store %c, %t1",
            "%t2 = field %c + 8",
            "store %t2, 5",
            "%t4 = field %c + 8",
            "%t3 = load %t4",
            "print int %t3",
        ]
    );
}

#[test]
fn a_field_of_the_base_comes_before_one_of_the_subclass() {
    // The prefix rule, which is what makes an upcast free.
    let ir = lower_src(
        "class Base {\n  int a;\n}\nclass Derived : Base {\n  int b;\n}\n\
         fn main() {\n  Derived d = Derived { a: 1, b: 2 };\n  print(d.a + d.b);\n}",
    );
    let offsets: Vec<u32> =
        ir.table.class(ClassId(1)).fields.iter().map(|f| f.offset).collect();
    // The vtable pointer takes offset 0.
    assert_eq!(offsets, vec![8, 16]);
}

#[test]
fn an_aggregate_field_is_its_address_rather_than_something_to_read() {
    // The rule an element already followed: a value too big for a register
    // *is* where it lives. Reading eight bytes out of `s.b` would produce
    // the inner object's vtable pointer, and writing through that would
    // land in the vtable rather than in the object.
    let ir = lower_src(
        "class Point {\n  int x;\n}\n\
         class Segment {\n  Point a;\n  Point b;\n}\n\
         fn main() {\n  \
         Segment s = Segment { a: Point { x: 1 }, b: Point { x: 2 } };\n  \
         s.b.x = 3;\n  \
         print(s.b.x);\n}",
    );
    // `b` starts past the whole of `a`, rather than one register after it.
    let offsets: Vec<u32> =
        ir.table.class(ClassId(1)).fields.iter().map(|f| f.offset).collect();
    assert_eq!(offsets, vec![8, 24]);

    let main = ir.functions.iter().find(|f| f.name == "main").expect("main survives");
    let loads =
        main.blocks[0].instrs.iter().filter(|i| matches!(i, Instr::Load { .. })).count();
    // One, and it is the `print`. Reaching `s.b` to write through it reads
    // nothing at all.
    assert_eq!(loads, 1, "{}", ir.dump());
}

#[test]
fn writing_through_an_element_that_is_an_object_reaches_the_element() {
    let ir = lower_src(
        "class Point {\n  int x;\n}\n\
         fn main() {\n  \
         Point[2] ps = [Point { x: 1 }, Point { x: 2 }];\n  \
         ps[1].x = 7;\n  \
         print(ps[1].x);\n}",
    );
    let main = ir.functions.iter().find(|f| f.name == "main").expect("main survives");
    let loads =
        main.blocks[0].instrs.iter().filter(|i| matches!(i, Instr::Load { .. })).count();
    assert_eq!(loads, 1, "{}", ir.dump());
}

#[test]
fn a_call_on_a_class_with_subclasses_goes_through_the_vtable() {
    let ir = lower_src(
        "class Shape {\n  fn area(self) -> int { return 0; }\n}\n\
         class Circle : Shape {\n  int r;\n  fn area(self) -> int { return self.r; }\n}\n\
         fn report(Shape s) {\n  print(s.area());\n}\n\
         fn main() {\n  report(Circle { r: 1 });\n}",
    );
    let report = ir.functions.iter().find(|f| f.name == "report").expect("report survives");
    assert!(
        report.blocks[0].instrs.iter().any(|i| matches!(i, Instr::CallVirtual { slot: 0, .. })),
        "{}",
        ir.dump()
    );
}

#[test]
fn a_call_on_a_class_with_none_is_settled_at_compile_time() {
    // Whole-program compilation is what makes this knowable: nothing can
    // derive from `Point` afterwards, so there is one answer and no reason
    // to ask at run time.
    let ir = lower_src(
        "class Point {\n  int x;\n  fn get(self) -> int { return self.x; }\n}\n\
         fn main() {\n  Point p = Point { x: 1 };\n  print(p.get());\n}",
    );
    let main = ir.functions.iter().find(|f| f.name == "main").expect("main survives");
    assert!(
        !main.blocks[0].instrs.iter().any(|i| matches!(i, Instr::CallVirtual { .. })),
        "{}",
        ir.dump()
    );
    assert!(
        main.blocks[0].instrs.iter().any(|i| matches!(i, Instr::Call { .. })),
        "{}",
        ir.dump()
    );
}

#[test]
fn a_subclass_vtable_is_its_base_with_the_overrides_replaced() {
    let ir = lower_src(
        "class Shape {\n  fn area(self) -> int { return 0; }\n  \
         fn name(self) -> string { return \"shape\"; }\n}\n\
         class Circle : Shape {\n  int r;\n  fn area(self) -> int { return self.r; }\n}\n\
         fn main() {\n  Circle c = Circle { r: 1 };\n  print(c.area());\n}",
    );
    // Same two slots in the same order; only the first was overridden.
    let shape = ir.table.class(ClassId(0));
    let circle = ir.table.class(ClassId(1));
    let names: Vec<&str> = circle.methods.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(names, vec!["area", "name"]);
    assert_eq!(circle.methods[0].slot, shape.methods[0].slot);
    assert_ne!(circle.methods[0].function, shape.methods[0].function);
    assert_eq!(circle.methods[1].function, shape.methods[1].function);
}

#[test]
fn storage_is_the_biggest_in_the_hierarchy() {
    // What will let a value of a base class hold any of its subclasses.
    let ir = lower_src(
        "class Shape {\n  fn area(self) -> int { return 0; }\n}\n\
         class Circle : Shape {\n  int r;\n  fn area(self) -> int { return self.r; }\n}\n\
         class Rect : Shape {\n  int w;\n  int h;\n  \
         fn area(self) -> int { return self.w; }\n}\n\
         fn main() {\n  Rect r = Rect { w: 1, h: 2 };\n  print(r.area());\n}",
    );
    // `Shape` is 8 on its own; the biggest thing that *is* one is `Rect`.
    assert_eq!(ir.table.class(ClassId(0)).size, 8);
    assert_eq!(ir.table.class(ClassId(0)).storage, 24);
    assert_eq!(ir.table.class(ClassId(2)).storage, 24);
}

#[test]
fn a_method_is_named_after_its_class() {
    // Two classes may both have a `go`, so the flat list of callables has
    // to keep them apart — and so do their symbols.
    let ir = lower_src(
        "class A {\n  fn go(self) -> int { return 1; }\n}\n\
         class B {\n  fn go(self) -> int { return 2; }\n}\n\
         fn main() {\n  A a = A { };\n  B b = B { };\n  print(a.go() + b.go());\n}",
    );
    let names: Vec<&str> = ir.functions.iter().map(|f| f.name.as_str()).collect();
    assert!(names.contains(&"A$go"), "{names:?}");
    assert!(names.contains(&"B$go"), "{names:?}");
}

#[test]
fn a_class_nothing_builds_keeps_none_of_its_methods() {
    // Making an object is what makes its methods callable, and the only
    // thing that does.
    let ir = lower_src(
        "class Used {\n  fn f(self) -> int { return 1; }\n}\n\
         class Unused {\n  fn f(self) -> int { return 2; }\n}\n\
         fn main() {\n  Used u = Used { };\n  print(u.f());\n}",
    );
    let names: Vec<&str> = ir.functions.iter().map(|f| f.name.as_str()).collect();
    assert!(names.contains(&"Used$f"), "{names:?}");
    assert!(!names.contains(&"Unused$f"), "{names:?}");
}

// -- arrays ------------------------------------------------------------

#[test]
fn an_array_is_room_in_the_frame_and_a_store_per_element() {
    let ir = lower_main("int[3] xs = [10, 20, 30];\nprint(xs[0]);");
    assert_eq!(
        body_dump(&ir),
        concat!(
            "entry0:\n",
            "  0  %xs = frame 0\n",
            "  1  %t1 = elem %xs[0] of 3 by 8\n",
            "  2  store %t1, 10\n",
            "  3  %t2 = elem %xs[1] of 3 by 8\n",
            "  4  store %t2, 20\n",
            "  5  %t3 = elem %xs[2] of 3 by 8\n",
            "  6  store %t3, 30\n",
            "  7  %t4 = elem %xs[0] of 3 by 8\n",
            "  8  %t5 = load %t4\n",
            "  9  print int %t5\n",
            " 10  return\n",
        )
    );
}

#[test]
fn the_frame_is_sized_by_what_the_function_declared() {
    let ir = lower_main("int[3] a = [1, 2, 3];\nbool[2] b = [true, false];\nprint(a[0]);");
    // Two arrays, five elements, eight bytes each.
    assert_eq!(ir.functions[0].frame_bytes, 40);
    // And they do not overlap.
    let offsets: Vec<u32> = ir.functions[0].blocks[0]
        .instrs
        .iter()
        .filter_map(|i| match i {
            Instr::Frame { offset, .. } => Some(*offset),
            _ => None,
        })
        .collect();
    assert_eq!(offsets, vec![0, 24]);
}

/// A literal at a place that already has an address is built *there*.
///
/// It used to be built in room of its own and copied, which cost the room
/// for the whole call — the room stayed reserved whether or not anything
/// still needed it — and a `CopyBytes` nobody asked for.
#[test]
fn a_literal_in_a_place_that_has_room_reserves_none_of_its_own() {
    let ir = lower_src(
        "class P { int[2] xs; }\nfn main() {\n  P p = P { xs: [1, 2] };\n  print(p.xs[0]);\n}",
    );
    let main = &ir.functions[0];
    // One reservation, for the object. The array goes inside it.
    let frames: Vec<u32> = main.blocks[0]
        .instrs
        .iter()
        .filter_map(|i| match i {
            Instr::Frame { offset, .. } => Some(*offset),
            _ => None,
        })
        .collect();
    assert_eq!(frames, vec![0], "the literal reserved room of its own: {}", ir.dump());
    // `P` is a vtable pointer and two ints, and that is the whole frame.
    assert_eq!(main.frame_bytes, 24);
    // And nothing is copied, because nothing was built anywhere else.
    assert!(
        !main.blocks[0].instrs.iter().any(|i| matches!(i, Instr::CopyBytes { .. })),
        "{}",
        ir.dump()
    );
}

/// The same literal *assigned* is not, because it may read what it is
/// overwriting — see [`Room`].
#[test]
fn a_literal_assigned_over_a_place_is_built_elsewhere_and_copied() {
    let ir = lower_main("int[2] a = [1, 2];\na = [a[1], a[0]];\nprint(a[0]);");
    let main = &ir.functions[0];
    assert!(
        main.blocks[0].instrs.iter().any(|i| matches!(i, Instr::CopyBytes { .. })),
        "the swap has to change all at once: {}",
        ir.dump()
    );
    // Two reservations: the variable, and the room the swap is built in.
    assert_eq!(main.frame_bytes, 32);
}

/// Two blocks that cannot be running at once share their room.
#[test]
fn a_blocks_frame_goes_back_when_the_block_ends() {
    let shared = lower_main(
        "int n = 0;\nif (n == 0) {\n  int[3] a = [1, 2, 3];\n  n = a[0];\n}\n\
         else {\n  int[3] b = [4, 5, 6];\n  n = b[0];\n}\nprint(n);",
    );
    // Three ints, once, rather than once per arm.
    assert_eq!(shared.functions[0].frame_bytes, 24);

    // What is reserved is the most ever needed at *once*, so two arrays
    // that really are live together still get room each.
    let both = lower_main("int[3] a = [1, 2, 3];\nint[3] b = [4, 5, 6];\nprint(a[0] + b[0]);");
    assert_eq!(both.functions[0].frame_bytes, 48);
}

/// One `int[1024]` is 8,192 bytes, so the limit falls between the
/// thirty-second and the thirty-third. The boundary is checked from both
/// sides because an off-by-one here is a program that either crashes or is
/// refused for no reason.
fn arrays_worth(bytes: u32) -> String {
    let elements = vec!["0"; 1024].join(", ");
    let count = bytes.div_ceil(1024 * 8);
    let declarations: String =
        (0..count).map(|i| format!("  int[1024] a{i} = [{elements}];\n")).collect();
    format!("fn main() {{\n{declarations}  println(a0[0]);\n}}\n")
}

#[test]
fn a_frame_no_stack_would_hold_is_refused() {
    let Err(errors) = try_lower(&arrays_worth(Machine::TEST.layout.max_frame + 1)) else {
        panic!("past the limit is past the limit");
    };
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].message.contains("needs too much stack"), "{:?}", errors[0]);
    // The caret goes on the function's name, which is the one place a
    // reader can act on: no single declaration is the culprit.
    assert!(errors[0].label.as_ref().is_some_and(|l| l.contains("262144")), "{:?}", errors[0]);
}

#[test]
fn a_frame_that_only_just_fits_is_not() {
    let ir = try_lower(&arrays_worth(Machine::TEST.layout.max_frame)).expect("exactly the limit is allowed");
    assert_eq!(ir.functions[0].frame_bytes, Machine::TEST.layout.max_frame);
}

#[test]
fn a_frame_is_measured_in_the_function_that_declares_it() {
    // Two functions of half the limit each. Neither is refused: a frame is
    // one call's, and these two are never on the stack at the same time
    // unless one calls the other — which is what the *runtime* check is
    // for, and it is a question about depth, not about size.
    let half = Machine::TEST.layout.max_frame / 2;
    let mut src = arrays_worth(half).replace("fn main()", "fn other()");
    src.push_str(&arrays_worth(half));
    try_lower(&src).expect("two half-sized frames are two frames, not one");
}

#[test]
fn a_function_with_no_arrays_reserves_nothing() {
    assert_eq!(lower_main("int n = 1;\nprint(n);").functions[0].frame_bytes, 0);
}

#[test]
fn len_is_a_constant_and_costs_nothing() {
    // It is a fact about the type, so nothing computes it.
    let ir = lower_main("int[4] xs = [1, 2, 3, 4];\nprint(len(xs));");
    let main = &ir.functions[0];
    assert!(
        matches!(main.blocks[0].instrs.last(), Some(Instr::Print { val: Value::Const(4), .. })),
        "{}",
        ir.dump()
    );
}

#[test]
fn an_element_address_is_one_instruction_and_never_arithmetic() {
    // `base + index * 8` is an addressing mode, so it picks up none of the
    // overflow guards `Bin` carries.
    let ir = lower_main("int[3] xs = [1, 2, 3];\nint i = 1;\nprint(xs[i]);");
    let main = &ir.functions[0];
    let elems = main.blocks[0].instrs.iter().filter(|i| matches!(i, Instr::Elem { .. })).count();
    // Three to build it, one to read it.
    assert_eq!(elems, 4, "{}", ir.dump());
    assert!(
        !main.blocks[0]
            .instrs
            .iter()
            .any(|i| matches!(i, Instr::Bin { op: BinOp::Mul, .. })),
        "{}",
        ir.dump()
    );
}

#[test]
fn writing_through_an_index_stores_rather_than_copying_a_register() {
    let ir = lower_main("int[2] xs = [1, 2];\nxs[1] = 7;\nprint(xs[1]);");
    let main = &ir.functions[0];
    // Two stores for the literal, one for the assignment.
    let stores = main.blocks[0].instrs.iter().filter(|i| matches!(i, Instr::Store { .. })).count();
    assert_eq!(stores, 3, "{}", ir.dump());
}

#[test]
fn an_array_parameter_is_an_address_like_any_other_value() {
    let ir = lower_src(
        "fn first(int[2] xs) -> int {\n  return xs[0];\n}\n\
         fn main() {\n  int[2] xs = [1, 2];\n  print(first(xs));\n}",
    );
    let first = &ir.functions[0];
    // Nothing is copied in: the register holds the caller's address.
    assert!(matches!(first.blocks[0].instrs[0], Instr::Param { index: 0, .. }));
    assert!(!first.blocks.iter().flat_map(|b| &b.instrs).any(|i| matches!(i, Instr::Frame { .. })));
}

// -- enums and match ---------------------------------------------------

/// A `Colour` enum and a `main`, so the tests about enums stay about enums.
fn lower_colour(body: &str) -> Program {
    lower_src(&format!("enum Colour {{ Red, Green, Blue }}\nfn main() {{\n{body}\n}}\n"))
}

#[test]
fn a_variant_lowers_to_its_tag_and_nothing_else() {
    // The whole representation: a variant is where it was written in the
    // declaration, so it is an immediate like any other integer.
    let ir = lower_colour("Colour c = Colour::Blue;\nprint(c);");
    assert_eq!(
        body_dump(&ir),
        concat!(
            "entry0:\n",
            "  0  %c = const 2\n",
            "  1  print Colour %c\n",
            "  2  return\n",
        )
    );
}

#[test]
fn a_variant_used_as_an_operand_stays_an_immediate() {
    let ir = lower_colour("Colour c = Colour::Red;\nprint(c == Colour::Green);");
    assert!(
        ir.functions[0].blocks[0]
            .instrs
            .iter()
            .any(|i| matches!(i, Instr::Cmp { rhs: Value::Const(1), .. })),
        "{}",
        ir.dump()
    );
}

#[test]
fn a_match_tests_every_variant_but_the_last() {
    // Exhaustiveness pays for itself here: there is nowhere else for the
    // value to be, so the final test would always succeed.
    let ir = lower_colour(
        "Colour c = Colour::Red;\nmatch (c) {\n  Colour::Red => { print(1); }\n  \
         Colour::Green => { print(2); }\n  Colour::Blue => { print(3); }\n}",
    );
    let main = &ir.functions[0];
    assert_eq!(labels(main), vec!["entry0", "arm1", "case2", "arm3", "arm4", "join5"]);

    let comparisons = main
        .blocks
        .iter()
        .flat_map(|b| &b.instrs)
        .filter(|i| matches!(i, Instr::Cmp { .. }))
        .count();
    assert_eq!(comparisons, 2, "three variants need two tests: {}", ir.dump());
}

#[test]
fn each_match_arm_reaches_the_same_join() {
    let ir = lower_colour(
        "Colour c = Colour::Red;\nmatch (c) {\n  Colour::Red => { print(1); }\n  \
         Colour::Green => { print(2); }\n  Colour::Blue => { print(3); }\n}\nprint(9);",
    );
    let main = &ir.functions[0];
    let join = BlockId(5);
    for arm in [1usize, 3, 4] {
        assert!(
            matches!(&main.blocks[arm].term, Terminator::Jump(target) if target.block == join),
            "arm {arm}: {}",
            ir.dump()
        );
    }
}

#[test]
fn a_single_variant_enum_needs_no_test_at_all() {
    // There is only one place the value can be, so the scrutinee's block
    // simply runs into the one arm.
    let ir = lower_src(
        "enum Unit { Only }\nfn main() {\n  Unit u = Unit::Only;\n  \
         match (u) {\n    Unit::Only => { print(1); }\n  }\n}",
    );
    let main = &ir.functions[0];
    assert!(
        !main.blocks.iter().flat_map(|b| &b.instrs).any(|i| matches!(i, Instr::Cmp { .. })),
        "{}",
        ir.dump()
    );
    assert!(matches!(main.blocks[0].term, Terminator::Jump(_)), "{}", ir.dump());
}

#[test]
fn the_arms_are_matched_in_the_order_they_are_written() {
    // Not in declaration order: an arm's tag comes from its own pattern.
    let ir = lower_colour(
        "Colour c = Colour::Red;\nmatch (c) {\n  Colour::Blue => { print(1); }\n  \
         Colour::Red => { print(2); }\n  Colour::Green => { print(3); }\n}",
    );
    let tags: Vec<i64> = ir.functions[0]
        .blocks
        .iter()
        .flat_map(|b| &b.instrs)
        .filter_map(|i| match i {
            Instr::Cmp { rhs: Value::Const(tag), .. } => Some(*tag),
            _ => None,
        })
        .collect();
    assert_eq!(tags, vec![2, 0], "{}", ir.dump());
}

#[test]
fn a_loop_jump_inside_an_arm_belongs_to_the_loop() {
    let ir = lower_src(
        "enum A { X, Y }\nfn main() {\n  while (true) {\n    A a = A::X;\n    \
         match (a) {\n      A::X => { break; }\n      A::Y => { print(1); }\n    }\n  }\n}",
    );
    // The `break` leaves for the loop's exit, not the match's join.
    let main = &ir.functions[0];
    let done = main
        .blocks
        .iter()
        .position(|b| b.kind == BlockKind::Done)
        .expect("the loop has an exit");
    assert!(
        main.blocks.iter().any(|b| matches!(&b.term, Terminator::Jump(t) if t.block.0 as usize == done)),
        "{}",
        ir.dump()
    );
}

#[test]
fn every_value_arm_writes_the_same_register() {
    // The trick `&&` already plays, with more arms: lowering emits a register
    // per variable, so the join reads the one several blocks wrote. SSA turns
    // that into a parameter afterwards — this is what it is handed.
    let ir = lower_colour(
        "string s = match (Colour::Red) {\n  Colour::Red => \"a\",\n  \
         Colour::Green => \"b\",\n  Colour::Blue => \"c\",\n};\nprint(s);",
    );
    let main = &ir.functions[0];
    let written: Vec<VReg> = main
        .blocks
        .iter()
        .filter(|b| b.kind == BlockKind::Arm)
        .filter_map(|b| b.instrs.last().and_then(|i| i.def()))
        .collect();
    assert_eq!(written.len(), 3, "{}", ir.dump());
    assert!(written.windows(2).all(|w| w[0] == w[1]), "{written:?} in {}", ir.dump());
}

#[test]
fn a_block_arm_writes_nothing_and_never_reaches_the_join() {
    let ir = lower_src(
        "enum A { X, Y }\nfn f(A a) -> int {\n  return match (a) {\n    A::X => 1,\n    \
         A::Y => { print(9); return 2; }\n  };\n}\nfn main() {\n  print(f(A::X));\n}",
    );
    let f = &ir.functions[0];
    // The diverging arm ends in a `return`, not a jump to the join.
    assert!(
        f.blocks
            .iter()
            .filter(|b| b.kind == BlockKind::Arm)
            .any(|b| matches!(b.term, Terminator::Return(Some(_)))),
        "{}",
        ir.dump()
    );
}

#[test]
fn a_match_statement_needs_no_destination() {
    // Nothing reads it, so no temporary is spent on one.
    let ir = lower_colour(
        "match (Colour::Red) {\n  Colour::Red => { print(1); }\n  \
         Colour::Green => { print(2); }\n  Colour::Blue => { print(3); }\n}",
    );
    let main = &ir.functions[0];
    let joins: Vec<&Block> =
        main.blocks.iter().filter(|b| b.kind == BlockKind::Join).collect();
    assert_eq!(joins.len(), 1);
    assert!(joins[0].instrs.is_empty(), "{}", ir.dump());
}

#[test]
fn a_match_expression_folds_its_arms_like_any_other_operand() {
    // A variant arm is a constant, so it never reaches a register at all.
    let ir = lower_src(
        "enum A { X, Y }\nfn main() {\n  A a = match (A::X) {\n    A::X => A::Y,\n    \
         A::Y => A::X,\n  };\n  print(a);\n}",
    );
    let main = &ir.functions[0];
    assert!(
        main.blocks
            .iter()
            .flat_map(|b| &b.instrs)
            .any(|i| matches!(i, Instr::Const { val: 1, .. })),
        "{}",
        ir.dump()
    );
}

#[test]
fn the_enums_travel_with_the_program() {
    // The backend needs the names to print one, and the dump to name the
    // type; the values themselves need nothing.
    let ir = lower_colour("print(Colour::Red);");
    assert_eq!(ir.table.enums.len(), 1);
    assert_eq!(ir.table.enums[0].name, "Colour");
    assert_eq!(ir.table.enums[0].names(), vec!["Red", "Green", "Blue"]);
}

// -- negation and remainder --------------------------------------------

#[test]
fn negating_a_comparison_inverts_it_in_place() {
    // `!(a < b)` is `a >= b`: one comparison, not a comparison plus a
    // comparison against its result.
    let ir = lower_main("int a = 1;\nint b = 2;\nprint(!(a < b));");
    let comparisons: Vec<&Instr> = ir.functions[0].blocks[0]
        .instrs
        .iter()
        .filter(|i| matches!(i, Instr::Cmp { .. }))
        .collect();
    assert_eq!(comparisons.len(), 1, "{}", ir.dump());
    assert!(matches!(comparisons[0], Instr::Cmp { op: CmpOp::Ge, .. }), "{comparisons:?}");
}

#[test]
fn negating_anything_else_compares_it_against_zero() {
    // There is no `not` instruction, and none is needed: `!ok` *is*
    // `ok == 0`, which folds and fuses like any other comparison.
    let ir = lower_main("bool ok = true;\nint n = 1;\nok = n > 0;\nprint(!ok);");
    assert!(
        ir.functions[0].blocks[0]
            .instrs
            .iter()
            .any(|i| matches!(i, Instr::Cmp { op: CmpOp::Eq, rhs: Value::Const(0), .. })),
        "{}",
        ir.dump()
    );
}

#[test]
fn negating_a_literal_is_folded() {
    let ir = lower_main("print(!true);");
    let main = &ir.functions[0];
    assert_eq!(main.blocks[0].instrs.len(), 1, "{}", ir.dump());
    assert!(matches!(main.blocks[0].instrs[0], Instr::Print { val: Value::Const(0), .. }));
}

#[test]
fn a_remainder_between_literals_is_computed_at_compile_time() {
    let ir = lower_main("print(17 % 5);");
    let main = &ir.functions[0];
    assert!(
        matches!(main.blocks[0].instrs[0], Instr::Print { val: Value::Const(2), .. }),
        "{}",
        ir.dump()
    );
}

#[test]
fn an_operation_on_something_unknown_stays_an_instruction() {
    // The other half: what the folder cannot answer becomes a guarded
    // instruction, and the program fails where it was written.
    for (source, op) in [
        ("int z = 0;\nprint(1 / z);", BinOp::Div),
        ("int z = 0;\nprint(1 % z);", BinOp::Rem),
        ("int n = 1;\nprint(n + 9223372036854775807);", BinOp::Add),
    ] {
        let ir = lower_main(source);
        assert!(
            ir.functions[0]
                .blocks
                .iter()
                .flat_map(|b| &b.instrs)
                .any(|i| matches!(i, Instr::Bin { op: found, .. } if *found == op)),
            "{source}: {}",
            ir.dump()
        );
    }
}

// -- short-circuiting operators ----------------------------------------

#[test]
fn a_logical_operator_lowers_to_a_diamond() {
    let ir = lower_main("int x = 5;\nbool ok = x > 1 && x < 9;\nprint(ok);");
    let main = &ir.functions[0];
    assert_eq!(labels(main), vec!["entry0", "rhs1", "short2", "join3"]);
    // Both arms write the same register, which is what makes this an
    // expression: the join needs no phi.
    assert!(matches!(main.blocks[1].instrs.last(), Some(Instr::Cmp { dst, .. }) if *dst == main.blocks[2].instrs[0].def().unwrap()));
    assert!(matches!(main.blocks[2].instrs[0], Instr::Const { val: 0, .. }));
}

#[test]
fn or_lays_its_short_circuit_out_first() {
    // The arm the branch continues into comes first, so the backend reaches
    // it by falling through: for `&&` that is the right operand, for `||`
    // the short circuit.
    let ir = lower_main("int x = 5;\nbool ok = x > 1 || x < 9;\nprint(ok);");
    let main = &ir.functions[0];
    assert_eq!(labels(main), vec!["entry0", "short1", "rhs2", "join3"]);
    assert!(matches!(main.blocks[1].instrs[0], Instr::Const { val: 1, .. }));
    match &main.blocks[0].term {
        Terminator::Branch { then_blk, else_blk, .. } => {
            assert_eq!((then_blk.block, else_blk.block), (BlockId(1), BlockId(2)));
        }
        other => panic!("expected a branch, got {other:?}"),
    }
}

#[test]
fn a_left_operand_that_settles_the_answer_drops_the_right_one() {
    // Not an optimisation but the semantics: `f` must not be called.
    let ir = lower_src(
        "fn f() -> bool {\n  return true;\n}\nfn main() {\n  print(false && f());\n}",
    );
    let main = ir.functions.iter().find(|f| f.name == "main").expect("main survives");
    assert_eq!(labels(main), vec!["entry0"]);
    assert!(
        matches!(main.blocks[0].instrs[0], Instr::Print { val: Value::Const(0), .. }),
        "{}",
        ir.dump()
    );
    // Nothing calls `f` any more, so it is pruned along with unused ones.
    assert!(!ir.functions.iter().any(|f| f.name == "f"), "{}", ir.dump());
}

#[test]
fn a_left_operand_that_settles_nothing_leaves_only_the_right_one() {
    // `true && e` is `e`: no branch, no temporary, no constant.
    let ir = lower_main("int x = 5;\nprint(true && x > 1);");
    let main = &ir.functions[0];
    assert_eq!(labels(main), vec!["entry0"]);
    assert!(matches!(main.blocks[0].instrs[1], Instr::Cmp { .. }), "{}", ir.dump());
}

#[test]
fn logic_between_literals_is_folded_all_the_way() {
    for (source, expected) in
        [("print(true || false);", 1), ("print(true && false);", 0), ("print(1 < 2 && 3 < 4);", 1)]
    {
        let ir = lower_main(source);
        let main = &ir.functions[0];
        assert_eq!(main.blocks[0].instrs.len(), 1, "{source}: {}", ir.dump());
        assert!(
            matches!(main.blocks[0].instrs[0], Instr::Print { val: Value::Const(v), .. } if v == expected),
            "{source}: {}",
            ir.dump()
        );
    }
}

#[test]
fn a_logical_operator_may_be_a_condition_of_its_own() {
    // The branch belongs to the block the condition *ended* in, which for a
    // short-circuiting operator is its join rather than the loop header.
    let ir = lower_main("int i = 0;\nwhile (i < 3 && i != 2) {\n  i = i + 1;\n}\nprint(i);");
    let main = &ir.functions[0];
    assert_eq!(labels(main), vec!["entry0", "loop1", "rhs2", "short3", "join4", "body5", "done6"]);
    // The loop is still a loop: the body jumps back to the header, not to
    // the join the condition finished in.
    assert!(matches!(main.blocks[5].term, Terminator::Jump(Target { block: BlockId(1), .. })));
}

// -- break and continue ------------------------------------------------

#[test]
fn break_jumps_to_the_block_after_the_loop() {
    let ir = lower_main("while (true) {\n  break;\n}\nprint(1);");
    let main = &ir.functions[0];
    // The block the `break` left was terminated by the loop on its way out,
    // and the unreachable block opened after it is gone.
    assert_eq!(labels(main), vec!["entry0", "loop1", "body2", "done3"]);
    assert!(matches!(main.blocks[2].term, Terminator::Jump(Target { block: BlockId(3), .. })));
}

#[test]
fn continue_in_a_while_jumps_back_to_the_header() {
    let ir = lower_main("int i = 0;\nwhile (i < 3) {\n  i = i + 1;\n  continue;\n}\nprint(i);");
    let main = &ir.functions[0];
    // A `while` has no step, so its header *is* its latch and no extra
    // block appears.
    assert_eq!(labels(main), vec!["entry0", "loop1", "body2", "done3"]);
    assert!(matches!(main.blocks[2].term, Terminator::Jump(Target { block: BlockId(1), .. })));
}

#[test]
fn continue_in_a_for_runs_the_step_on_its_way_past() {
    // The whole reason the step needs a block: skipping it would leave the
    // counter alone and the loop would never end.
    let ir = lower_main("for (int i = 0; i < 3; i = i + 1) {\n  continue;\n}");
    let main = &ir.functions[0];
    assert_eq!(labels(main), vec!["entry0", "loop1", "body2", "step3", "done4"]);
    assert!(matches!(main.blocks[3].instrs[0], Instr::Bin { op: BinOp::Add, .. }));
    assert!(matches!(main.blocks[2].term, Terminator::Jump(Target { block: BlockId(3), .. })));
    assert!(matches!(main.blocks[3].term, Terminator::Jump(Target { block: BlockId(1), .. })));
}

#[test]
fn a_for_without_a_continue_needs_no_step_block() {
    // Which is what keeps a plain `for` lowering to exactly the `while` it
    // desugars into.
    let ir = lower_main("for (int i = 0; i < 3; i = i + 1) {\n  print(i);\n}");
    assert_eq!(labels(&ir.functions[0]), vec!["entry0", "loop1", "body2", "done3"]);
}

#[test]
fn a_continue_in_a_nested_loop_belongs_to_that_loop() {
    // The inner `while` takes the `continue`, so the outer `for` still has
    // none of its own and stays step-block free.
    let ir = lower_main(
        "for (int i = 0; i < 3; i = i + 1) {\n  while (i < 2) {\n    i = i + 1;\n    continue;\n  }\n}",
    );
    let labels = labels(&ir.functions[0]);
    assert!(!labels.iter().any(|l| l.starts_with("step")), "{labels:?}");
}

#[test]
fn break_leaves_only_the_innermost_loop() {
    let ir = lower_main(
        "while (true) {\n  while (true) {\n    break;\n  }\n  print(1);\n  break;\n}",
    );
    let main = &ir.functions[0];
    // The inner `break` lands where the inner loop leaves, which is where
    // the `print` still runs — not at the outer loop's exit.
    let done: Vec<usize> =
        main.blocks.iter().enumerate().filter(|(_, b)| b.kind == BlockKind::Done).map(|(i, _)| i).collect();
    assert_eq!(done.len(), 2, "{}", ir.dump());
    let inner_exit = done[0];
    assert!(
        main.blocks[inner_exit].instrs.iter().any(|i| matches!(i, Instr::Print { .. })),
        "{}",
        ir.dump()
    );
}

#[test]
fn code_after_a_loop_jump_is_pruned() {
    let ir = lower_main("while (true) {\n  break;\n  print(1);\n}");
    assert!(
        !ir.functions[0].blocks.iter().flat_map(|b| &b.instrs).any(|i| matches!(i, Instr::Print { .. })),
        "{}",
        ir.dump()
    );
}

#[test]
fn literal_operands_stay_immediates() {
    // `x` is unknown, so the addition survives — but the 2 next to it is
    // still an operand rather than a register of its own.
    let ir = lower_main("int x = 1;\nprint(x + 2);");
    let main = &ir.functions[0];
    assert!(
        main.blocks[0]
            .instrs
            .iter()
            .any(|i| matches!(i, Instr::Bin { rhs: Value::Const(2), .. })),
        "{}",
        ir.dump()
    );
}

#[test]
fn arithmetic_between_literals_is_done_at_compile_time() {
    let ir = lower_main("print(1 + 2 * 3);");
    let main = &ir.functions[0];
    // The whole tree collapses into the operand of the print: no `add`, no
    // `mul`, and no register to hold the answer either.
    assert_eq!(main.blocks[0].instrs.len(), 1, "{}", ir.dump());
    assert!(
        matches!(main.blocks[0].instrs[0], Instr::Print { val: Value::Const(7), .. }),
        "{}",
        ir.dump()
    );
}

#[test]
fn the_folder_refuses_every_answer_the_machine_would_refuse() {
    // Today `sema` rejects all of these before lowering is ever reached, so
    // this is a unit test rather than a program: it keeps the folder from
    // becoming the stage that invents an answer, should the two ever come
    // to see a different set of constants.
    for (op, a, b) in [
        (BinOp::Add, i64::MAX, 1),
        (BinOp::Sub, i64::MIN, 1),
        (BinOp::Mul, i64::MAX, 2),
        (BinOp::Div, 1, 0),
        (BinOp::Rem, 1, 0),
        (BinOp::Div, i64::MIN, -1),
        // 0 on paper, and still refused: the machine gets there through the
        // division whose quotient does not fit.
        (BinOp::Rem, i64::MIN, -1),
    ] {
        assert_eq!(
            fold_bin(Num::Int, op, Value::Const(a), Value::Const(b)),
            None,
            "{} {a}, {b}",
            op.symbol()
        );
    }
}

#[test]
fn an_operation_the_machine_accepts_is_still_folded() {
    for (op, a, b, expected) in [
        (BinOp::Add, 2, 3, 5),
        (BinOp::Sub, 2, 3, -1),
        (BinOp::Mul, 6, 7, 42),
        (BinOp::Div, 17, 5, 3),
        (BinOp::Rem, 17, 5, 2),
        // The largest each operator can produce, one step short of refusing.
        (BinOp::Add, i64::MAX - 1, 1, i64::MAX),
        (BinOp::Sub, i64::MIN + 1, 1, i64::MIN),
    ] {
        assert_eq!(
            fold_bin(Num::Int, op, Value::Const(a), Value::Const(b)),
            Some(expected),
            "{} {a}, {b}",
            op.symbol()
        );
    }
}

#[test]
fn a_comparison_between_literals_is_folded_too() {
    let ir = lower_main("bool b = 1 < 2;\nprint(b);");
    assert!(
        matches!(ir.functions[0].blocks[0].instrs[0], Instr::Const { val: 1, .. }),
        "{}",
        ir.dump()
    );
}

#[test]
fn a_function_nothing_calls_is_dropped() {
    let ir = lower_src(
        "fn used() -> int {\n  return 1;\n}\n\
         fn unused() -> int {\n  return 2;\n}\n\
         fn main() {\n  print(used());\n}",
    );
    let names: Vec<&str> = ir.functions.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, vec!["used", "main"]);
}

#[test]
fn dropping_a_function_renumbers_the_calls_that_survive() {
    // `unused` sits between the entry point and its callee, so every
    // `FuncId` after it shifts down by one.
    let ir = lower_src(
        "fn unused() {\n}\n\
         fn helper() -> int {\n  return 7;\n}\n\
         fn main() {\n  print(helper());\n}",
    );
    let main = ir.functions.iter().find(|f| f.name == "main").expect("main survives");
    let Instr::Call { callee, .. } = &main.blocks[0].instrs[0] else { panic!("a call") };
    assert_eq!(ir.function(*callee).name, "helper");
}

#[test]
fn a_function_only_an_unused_one_calls_goes_too() {
    let ir = lower_src(
        "fn deep() -> int {\n  return 1;\n}\n\
         fn unused() -> int {\n  return deep();\n}\n\
         fn main() {\n}",
    );
    let names: Vec<&str> = ir.functions.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, vec!["main"]);
}

#[test]
fn recursion_keeps_a_function_alive_through_itself() {
    let ir = lower_src(
        "fn fib(int n) -> int {\n  if (n < 2) {\n    return n;\n  }\n  \
         return fib(n - 1) + fib(n - 2);\n}\n\
         fn main() {\n  print(fib(5));\n}",
    );
    assert!(ir.functions.iter().any(|f| f.name == "fib"), "{}", ir.dump());
}

#[test]
fn bools_lower_to_integer_constants() {
    let ir = lower_main("bool ready = true;\nbool done = false;\nprint(ready);\nprint(done);");
    assert_eq!(
        body_dump(&ir),
        concat!(
            "entry0:\n",
            "  0  %ready = const 1\n",
            "  1  %done = const 0\n",
            "  2  print bool %ready\n",
            "  3  print bool %done\n",
            "  4  return\n",
        )
    );
}

#[test]
fn a_printed_bool_literal_stays_an_immediate() {
    let ir = lower_main("print(false);");
    let main = &ir.functions[0];
    assert_eq!(main.blocks[0].instrs.len(), 1);
    assert!(matches!(
        main.blocks[0].instrs[0],
        Instr::Print { ty: Ty::Bool, val: Value::Const(0), .. }
    ));
}

#[test]
fn shadowed_variables_get_distinct_registers() {
    let ir = lower_main("int i = 1;\nif (true) {\n  int i = 2;\n  print(i);\n}\nprint(i);");
    let names = &ir.functions[0].vreg_names;
    assert!(names.contains(&"i".to_string()));
    assert!(names.contains(&"i.1".to_string()));
}

#[test]
fn identical_strings_are_interned_once() {
    let ir = lower_main("string a = \"hi\";\nstring b = \"hi\";\nprint(a);\nprint(b);");
    assert_eq!(ir.strings.len(), 1);
}

#[test]
fn a_literal_is_interned_as_characters() {
    let ir = lower_main("string s = \"é\";\nprint(s);");
    assert_eq!(ir.strings[0], vec!['é']);
}

/// A literal that is only ever *written* never becomes a string at all.
///
/// The two tables answer different questions: `strings` holds values the
/// program can index and join, laid out as characters four bytes each;
/// `texts` holds bytes `printf` is handed. A literal that goes straight out
/// needs no run-time form, and so is not given one — which is what makes
/// `print("hi")` cost one call and no memory.
#[test]
fn a_literal_that_is_only_printed_becomes_text_rather_than_a_string() {
    let ir = lower_main("print(\"é\");");
    assert!(ir.strings.is_empty(), "{:?}", ir.strings);
    assert_eq!(ir.texts, vec!["é".to_string()]);
}

/// Every instruction a function lowered to, flattened.
fn instrs(ir: &Program) -> Vec<&Instr> {
    ir.functions[0].blocks.iter().flat_map(|b| &b.instrs).collect()
}

#[test]
fn joining_two_strings_becomes_a_call_and_not_an_add() {
    let ir = lower_main("string a = \"x\";\nprint(a + a);");
    assert!(
        instrs(&ir)
            .iter()
            .any(|i| matches!(i, Instr::RtCall { callee: Runtime::Concat, .. })),
        "{}",
        ir.dump()
    );
    assert!(!instrs(&ir).iter().any(|i| matches!(i, Instr::Bin { .. })), "{}", ir.dump());
}

#[test]
fn comparing_two_strings_asks_the_runtime_rather_than_the_processor() {
    let ir = lower_main("string a = \"x\";\nprint(a == a);");
    let lowered = instrs(&ir);
    assert!(
        lowered.iter().any(|i| matches!(i, Instr::RtCall { callee: Runtime::StrEq, .. })),
        "{}",
        ir.dump()
    );
    // `!=` is the same routine read the other way round, so it costs one
    // comparison against zero and not a second routine.
    let ir = lower_main("string a = \"x\";\nprint(a != a);");
    let lowered = instrs(&ir);
    assert!(lowered.iter().any(|i| matches!(i, Instr::RtCall { callee: Runtime::StrEq, .. })));
    assert!(lowered.iter().any(|i| matches!(i, Instr::Cmp { op: CmpOp::Eq, .. })));
}

#[test]
fn an_arrays_length_is_a_constant_and_a_strings_is_a_load() {
    let ir = lower_main("int[3] xs = [1, 2, 3];\nprint(len(xs));");
    assert!(!instrs(&ir).iter().any(|i| matches!(i, Instr::Count { .. })), "{}", ir.dump());

    let ir = lower_main("print(len(\"abc\"));");
    assert!(instrs(&ir).iter().any(|i| matches!(i, Instr::Count { .. })), "{}", ir.dump());
}

#[test]
fn a_constant_index_into_a_string_is_still_checked() {
    // An array's length is part of its type, so `sema` settled the index
    // and the `Elem` carries a constant length the backend can drop the
    // check for. A string's length is a load, so there is nothing to settle.
    let ir = lower_main("int[3] xs = [1, 2, 3];\nprint(xs[1]);");
    let lengths: Vec<&Value> = instrs(&ir)
        .iter()
        .filter_map(|i| match i {
            Instr::Elem { len, .. } => Some(len),
            _ => None,
        })
        .collect();
    assert!(lengths.iter().all(|len| matches!(len, Value::Const(_))), "{}", ir.dump());

    let ir = lower_main("print(\"abc\"[1]);");
    let lengths: Vec<&Value> = instrs(&ir)
        .iter()
        .filter_map(|i| match i {
            Instr::Elem { len, .. } => Some(len),
            _ => None,
        })
        .collect();
    assert_eq!(lengths.len(), 1);
    assert!(matches!(lengths[0], Value::Reg(_)), "{}", ir.dump());
}

#[test]
fn a_code_point_and_its_character_are_the_same_value() {
    // `int(c)` moves nothing: a character *is* its code point. Only the
    // direction that can fail costs an instruction.
    let ir = lower_main("char c = 'a';\nprint(int(c));");
    assert!(!instrs(&ir).iter().any(|i| matches!(i, Instr::RtCall { .. })), "{}", ir.dump());

    let ir = lower_main("int n = 65;\nprint(char(n));");
    assert!(
        instrs(&ir)
            .iter()
            .any(|i| matches!(i, Instr::RtCall { callee: Runtime::CheckChar, .. })),
        "{}",
        ir.dump()
    );
}

/// Every runtime routine a lowering reached.
fn routines(ir: &Program) -> Vec<Runtime> {
    instrs(ir)
        .iter()
        .filter_map(|i| match i {
            Instr::RtCall { callee, .. } => Some(*callee),
            _ => None,
        })
        .collect()
}

/// The proof `append` rests on, read off the IR: which shapes get it and
/// which fall back to an ordinary join.
///
/// The negative half matters more than the positive one. Getting this wrong
/// in the permissive direction would lengthen a string somebody else is
/// holding — so every one of these is a case the analysis has to refuse.
#[test]
fn a_string_is_grown_in_place_only_where_nothing_else_can_hold_it() {
    let grows = |src: &str| {
        let program = format!(
            "class Box {{ string text; }}\n\
             fn keep(string s) -> string {{ return s; }}\n\
             fn main() {{\n{src}\n}}\n"
        );
        routines(&lower_src(&program)).contains(&Runtime::Append)
    };

    // The shape it exists for, and the chain that is how a line is written.
    assert!(grows("string s = \"\";\ns = s + \"x\";\nprint(s);"));
    assert!(grows("string s = \"\";\ns = s + string(1) + \",\";\nprint(s);"));
    // Built by a conversion or by reading, which are blocks of their own.
    assert!(grows("string s = string(1);\ns = s + \"x\";\nprint(s);"));

    // Another name for it, taken anywhere in the function — before or
    // after, since the analysis is not a flow one and does not pretend to
    // be.
    assert!(!grows("string s = \"a\";\nstring t = s;\ns = s + \"x\";\nprint(t);"));
    assert!(!grows("string s = \"a\";\ns = s + \"x\";\nstring t = s;\nprint(t);"));
    // Handed to a function, put in a list, put in an object, returned.
    assert!(!grows("string s = \"a\";\nprint(keep(s));\ns = s + \"x\";"));
    assert!(!grows(
        "string s = \"a\";\nstring[] all = [];\npush(all, s);\ns = s + \"x\";\nprint(s);"
    ));
    assert!(!grows(
        "string s = \"a\";\nBox b = Box { text: s };\ns = s + \"x\";\nprint(b.text);"
    ));
    // Given a value that already had a name.
    assert!(!grows(
        "string o = \"a\" + \"b\";\nstring s = o;\ns = s + \"x\";\nprint(s);"
    ));
    // Given the answer of a call, which may be anybody's.
    assert!(!grows("string s = keep(\"a\");\ns = s + \"x\";\nprint(s);"));
    // Prepending cannot grow a block where it stands.
    assert!(!grows("string s = \"a\";\ns = \"x\" + s;\nprint(s);"));
    // A piece that reads the variable: appending one at a time would let
    // the second piece see what the first one wrote.
    assert!(!grows(
        "string s = \"a\";\ns = s + string(len(s)) + \"!\";\nprint(s);"
    ));

    // A parameter is the caller's string, whatever the body does with it.
    let ir = lower_src(
        "fn grow(string s) -> string {\n  s = s + \"x\";\n  return s;\n}\n\
         fn main() {\n  print(grow(\"a\"));\n}\n",
    );
    assert!(!routines(&ir).contains(&Runtime::Append), "{}", ir.dump());
}

/// A copy is only followed by a fix-up where the type says one can be
/// needed, so a program whose classes hold nothing but numbers carries
/// none of the machinery.
#[test]
fn only_a_copy_that_can_share_is_followed_by_a_fixup() {
    let fixups = |ir: &Program| {
        ir.functions
            .iter()
            .flat_map(|f| &f.blocks)
            .flat_map(|b| &b.instrs)
            .filter(|i| matches!(i, Instr::Fixup { .. }))
            .count()
    };

    // Nothing in a `Point` is anybody else's, so its bytes are the whole
    // of it.
    let plain = lower_src(
        "class Point { int x; }\n\
         fn main() {\n  Point a = Point { x: 1 };\n  Point b = a;\n  print(b.x);\n}\n",
    );
    assert_eq!(fixups(&plain), 0, "{}", plain.dump());

    // A list field, and the copy has to be told to go and get its own.
    let sharing = lower_src(
        "class Bag { int[] items; }\n\
         fn main() {\n  Bag a = Bag { items: [1] };\n  Bag b = a;\n  \
         print(len(b.items));\n}\n",
    );
    assert_eq!(fixups(&sharing), 1, "{}", sharing.dump());

    // An array of them is one fix-up over the run rather than one each:
    // the copy was one `CopyBytes`, and this is its other half.
    let array = lower_src(
        "class Bag { int[] items; }\n\
         fn main() {\n  Bag[2] a = [Bag { items: [1] }, Bag { items: [2] }];\n  \
         Bag[2] b = a;\n  print(len(b[0].items));\n}\n",
    );
    let run = instrs(&array)
        .into_iter()
        .find_map(|i| match i {
            Instr::Fixup { count, stride, .. } => Some((*count, *stride)),
            _ => None,
        })
        .expect("the array copy has one");
    assert_eq!(run, (Value::Const(2), 16), "two objects, sixteen bytes apart");
}

#[test]
fn assigning_a_list_copies_it_and_passing_one_does_not() {
    // The whole of "assignment copies, never aliases" for the one mutable
    // thing in the language, in two lowerings.
    let ir = lower_main("int[] a = [1];\nint[] b = a;\nprint(len(b));");
    assert!(routines(&ir).contains(&Runtime::ListClone), "{}", ir.dump());

    let ir = lower_src(
        "fn f(int[] xs) -> int {\n  return len(xs);\n}\n\
         fn main() {\n  int[] a = [1];\n  print(f(a));\n}\n",
    );
    let calls: Vec<Runtime> = ir
        .functions
        .iter()
        .flat_map(|f| &f.blocks)
        .flat_map(|b| &b.instrs)
        .filter_map(|i| match i {
            Instr::RtCall { callee, .. } => Some(*callee),
            _ => None,
        })
        .collect();
    assert!(!calls.contains(&Runtime::ListClone), "a parameter borrows: {}", ir.dump());
}

#[test]
fn a_freshly_built_list_moves_in_rather_than_being_copied() {
    // A literal is nobody else's already, so there is nothing to copy.
    let ir = lower_main("int[] a = [1, 2];\nprint(len(a));");
    assert!(!routines(&ir).contains(&Runtime::ListClone), "{}", ir.dump());
    assert!(routines(&ir).contains(&Runtime::ListNew), "{}", ir.dump());
}

#[test]
fn returning_a_borrowed_list_copies_it_at_the_return() {
    // Copied here rather than at the call site, which is what lets every
    // caller treat a returned list as its own.
    let ir = lower_src(
        "fn f(int[] xs) -> int[] {\n  return xs;\n}\nfn main() {\n  print(len(f([1])));\n}\n",
    );
    let returned: Vec<Runtime> = ir.functions[0]
        .blocks
        .iter()
        .flat_map(|b| &b.instrs)
        .filter_map(|i| match i {
            Instr::RtCall { callee, .. } => Some(*callee),
            _ => None,
        })
        .collect();
    assert!(returned.contains(&Runtime::ListClone), "{}", ir.dump());
}

#[test]
fn push_writes_the_new_address_back_where_the_list_is_named() {
    let ir = lower_main("int[] a = [];\npush(a, 1);\nprint(len(a));");
    let pushes: Vec<&Instr> = instrs(&ir)
        .into_iter()
        .filter(|i| matches!(i, Instr::RtCall { callee: Runtime::ListPush, .. }))
        .collect();
    assert_eq!(pushes.len(), 1);
    // The list's own register is both what goes in and what comes back.
    let Instr::RtCall { dst: Some(dst), args, .. } = pushes[0] else { panic!("a push") };
    assert_eq!(args[0], Value::Reg(*dst), "{}", ir.dump());
}

#[test]
fn a_list_of_objects_is_measured_in_whole_objects() {
    // The routines walk the elements rather than reading one, so how wide
    // an element is has to travel with every call — and the addressing has
    // to scale by the same number.
    let ir = lower_src(
        "class Point {\n  int x;\n  int y;\n}\n\
         fn main() {\n  Point[] ps = [Point { x: 1, y: 2 }];\n  print(ps[0].x);\n}\n",
    );
    // A vtable pointer and two fields.
    let width = Value::Const(24);
    let new = instrs(&ir)
        .into_iter()
        .find(|i| matches!(i, Instr::RtCall { callee: Runtime::ListNew, .. }))
        .expect("the literal builds one");
    let Instr::RtCall { args, .. } = new else { panic!("a call") };
    assert_eq!(args[1], width, "{}", ir.dump());
    assert!(
        instrs(&ir).iter().any(|i| matches!(i, Instr::Elem { scale: 24, .. })),
        "{}",
        ir.dump()
    );
}

#[test]
fn pushing_an_object_hands_over_where_it_is() {
    // An element too big for a register cannot travel in one, so the push
    // that takes an address is a different routine — the compiler knows
    // which from the element's type, and the runtime is not left to guess
    // from a width that an object of one word would make ambiguous.
    let ir = lower_src(
        "class Point {\n  int x;\n}\n\
         fn main() {\n  Point[] ps = [];\n  push(ps, Point { x: 1 });\n  \
         print(len(ps));\n}\n",
    );
    assert!(routines(&ir).contains(&Runtime::ListPushBig), "{}", ir.dump());
    assert!(!routines(&ir).contains(&Runtime::ListPush), "{}", ir.dump());

    // A register-sized element still goes through the plain one.
    let ir = lower_main("int[] xs = [];\npush(xs, 1);\nprint(len(xs));");
    assert!(routines(&ir).contains(&Runtime::ListPush), "{}", ir.dump());
    assert!(!routines(&ir).contains(&Runtime::ListPushBig), "{}", ir.dump());
}

#[test]
fn a_builtin_call_becomes_a_routine_and_not_a_function_call() {
    // There is no body to compile and no `FuncId` to name, so the ordinary
    // call instruction could not carry it.
    let ir = lower_main("print(read_line());");
    assert!(routines(&ir).contains(&Runtime::ReadLine), "{}", ir.dump());
    assert!(!instrs(&ir).iter().any(|i| matches!(i, Instr::Call { .. })), "{}", ir.dump());

    // Discarded on its own, which is a line skipped.
    let ir = lower_main("read_line();");
    let calls: Vec<&Instr> = instrs(&ir)
        .into_iter()
        .filter(|i| matches!(i, Instr::RtCall { callee: Runtime::ReadLine, dst: None, .. }))
        .collect();
    assert_eq!(calls.len(), 1, "{}", ir.dump());

    // A built-in that takes something is no different: the argument is
    // lowered where it stands and travels as an operand.
    let ir = lower_main("print(is_int(\"42\"));");
    assert!(routines(&ir).contains(&Runtime::IsInt), "{}", ir.dump());
    assert!(!instrs(&ir).iter().any(|i| matches!(i, Instr::Call { .. })), "{}", ir.dump());
}

#[test]
fn a_list_index_is_checked_against_a_length_it_has_to_load() {
    let ir = lower_main("int[] a = [1, 2];\nprint(a[0]);");
    assert!(instrs(&ir).iter().any(|i| matches!(i, Instr::Count { .. })), "{}", ir.dump());
}

#[test]
fn a_constant_character_needs_no_check_at_run_time() {
    let ir = lower_main("print(char(65));");
    assert!(!instrs(&ir).iter().any(|i| matches!(i, Instr::RtCall { .. })), "{}", ir.dump());
}

// -- functions ---------------------------------------------------------

#[test]
fn each_function_gets_its_own_graph_and_registers() {
    let ir = lower_src(
        "fn add(int a, int b) -> int {\n  return a + b;\n}\nfn main() {\n  print(add(1, 2));\n}",
    );
    assert_eq!(ir.functions.len(), 2);
    // Both functions number their blocks and registers from zero.
    assert_eq!(ir.functions[0].blocks[0].label(), "entry0");
    assert_eq!(ir.functions[1].blocks[0].label(), "entry0");
    assert_eq!(ir.functions[0].params, vec![VReg(0), VReg(1)]);
}

#[test]
fn parameters_are_defined_at_the_top_of_the_entry_block() {
    let ir = lower_src("fn f(int a, int b) {\n  print(a);\n}\nfn main() {\n  f(1, 2);\n}");
    let f = &ir.functions[0];
    assert!(matches!(f.blocks[0].instrs[0], Instr::Param { index: 0, .. }));
    assert!(matches!(f.blocks[0].instrs[1], Instr::Param { index: 1, .. }));
}

#[test]
fn a_call_lowers_to_an_instruction_naming_its_callee() {
    let ir = lower_src(
        "fn add(int a, int b) -> int {\n  return a + b;\n}\nfn main() {\n  print(add(1, 2));\n}",
    );
    let main = &ir.functions[1];
    match &main.blocks[0].instrs[0] {
        Instr::Call { dst: Some(_), callee, args } => {
            assert_eq!(*callee, FuncId(0));
            assert_eq!(args, &vec![Value::Const(1), Value::Const(2)]);
        }
        other => panic!("expected a call, got {other:?}"),
    }
}

#[test]
fn a_call_statement_discards_its_result() {
    let ir = lower_src("fn f() -> int {\n  return 1;\n}\nfn main() {\n  f();\n}");
    assert!(matches!(ir.functions[1].blocks[0].instrs[0], Instr::Call { dst: None, .. }));
}

#[test]
fn a_return_carries_its_value_in_the_terminator() {
    let ir = lower_src("fn one() -> int {\n  return 1;\n}\nfn main() {\n  print(one());\n}");
    assert!(matches!(
        ir.functions[0].blocks[0].term,
        Terminator::Return(Some(Value::Const(1)))
    ));
}

#[test]
fn a_bare_return_carries_nothing() {
    let ir = lower_src("fn f() {\n  return;\n}\nfn main() {\n  f();\n}");
    assert!(matches!(ir.functions[0].blocks[0].term, Terminator::Return(None)));
}

#[test]
fn code_after_a_return_is_pruned() {
    // The `print` is lowered into a block nothing jumps to, and that block
    // never reaches the backend.
    let ir = lower_src("fn f() {\n  return;\n  print(1);\n}\nfn main() {\n  f();\n}");
    assert_eq!(labels(&ir.functions[0]), vec!["entry0"]);
}

#[test]
fn an_if_where_both_arms_return_keeps_both_returns() {
    // The join block is unreachable and goes away, but the two `return`
    // terminators must survive the pruning intact.
    let ir = lower_src(
        "fn f(int n) -> int {\n  if (n < 2) {\n    return 1;\n  } else {\n    \
         return 2;\n  }\n}\nfn main() {\n  print(f(1));\n}",
    );
    let f = &ir.functions[0];
    assert_eq!(labels(f), vec!["entry0", "then1", "else2"]);
    assert!(matches!(f.blocks[1].term, Terminator::Return(Some(Value::Const(1)))));
    assert!(matches!(f.blocks[2].term, Terminator::Return(Some(Value::Const(2)))));
}

#[test]
fn a_recursive_call_names_its_own_function() {
    let ir = lower_src(
        "fn fib(int n) -> int {\n  if (n < 2) {\n    return n;\n  } else {\n    \
         return fib(n - 1) + fib(n - 2);\n  }\n}\nfn main() {\n  print(fib(10));\n}",
    );
    let fib = &ir.functions[0];
    let calls: Vec<&Instr> = fib
        .blocks
        .iter()
        .flat_map(|b| &b.instrs)
        .filter(|i| matches!(i, Instr::Call { .. }))
        .collect();
    assert_eq!(calls.len(), 2);
    for call in calls {
        assert!(matches!(call, Instr::Call { callee: FuncId(0), .. }), "{call:?}");
    }
}

#[test]
fn strings_are_shared_across_functions() {
    let ir = lower_src(
        "fn a() {\n  string s = \"hi\";\n  print(s);\n}\n\
         fn main() {\n  string s = \"hi\";\n  print(s);\n  a();\n}",
    );
    assert_eq!(ir.strings.len(), 1);
}

/// And so is literal text, which has a table of its own for the same reason.
#[test]
fn text_is_shared_across_functions() {
    let ir = lower_src(
        "fn a() {\n  print(\"hi\");\n}\nfn main() {\n  print(\"hi\");\n  a();\n}",
    );
    assert_eq!(ir.texts.len(), 1);
}

#[test]
fn a_value_used_by_a_call_does_not_cross_it_but_a_nested_one_does() {
    // In `f(g(1), 2)` the result of `g` is live across nothing; in
    // `f(g(1), h(2))` it is live across the call to `h`.
    let ir = lower_src(
        "fn g(int n) -> int {\n  return n;\n}\nfn h(int n) -> int {\n  return n;\n}\n\
         fn f(int a, int b) -> int {\n  return a;\n}\n\
         fn main() {\n  print(f(g(1), h(2)));\n}",
    );
    let main = ir.functions.last().unwrap();
    let calls = main.blocks[0].instrs.iter().filter(|i| i.is_call()).count();
    assert_eq!(calls, 4); // g, h, f, and the print
}

// -- writing things out -------------------------------------------------

/// A format lowers to one write per piece, in the order they were written.
#[test]
fn a_format_lowers_to_one_write_per_piece() {
    let ir = lower_main("int n = 7;\nprintln(\"a %d b\", n);");
    assert_eq!(
        body_dump(&ir),
        concat!(
            "entry0:\n",
            "  0  %n = const 7\n",
            "  1  print text0 \"a \"\n",
            "  2  print int %n\n",
            "  3  print text1 \" b\\n\"\n",
            "  4  return\n",
        )
    );
}

/// `println` is `print` with a newline, and this is where the two become
/// one statement. The newline joins the text in front of it rather than
/// costing a write of its own.
#[test]
fn a_trailing_newline_joins_the_text_before_it() {
    assert_eq!(lower_main("println(\"done\");").texts, vec!["done\n".to_string()]);
    assert_eq!(lower_main("print(\"done\");").texts, vec!["done".to_string()]);
}

/// When a format ends in a specifier there is no text to join the newline
/// to — so the *value* ends the line instead, and nothing is written after
/// it. That is what makes `println(n)` one call rather than two.
#[test]
fn a_value_that_ends_a_line_says_so_rather_than_being_followed_by_one() {
    let ir = lower_main("int n = 1;\nprintln(n);\nprintln(n);");
    assert!(ir.texts.is_empty(), "there is nothing left to write: {:?}", ir.texts);
    let printed: Vec<bool> = ir.functions[0].blocks[0]
        .instrs
        .iter()
        .filter_map(|i| match i {
            Instr::Print { newline, .. } => Some(*newline),
            _ => None,
        })
        .collect();
    assert_eq!(printed, vec![true, true]);
}

/// Only the *last* write of a `println` ends the line, and a `print` never
/// does — which is the whole of what the flag means.
#[test]
fn only_the_last_value_of_a_println_ends_the_line() {
    let ends: Vec<bool> = ["println(\"%d %d\", 1, 2);", "print(\"%d %d\", 1, 2);"]
        .iter()
        .flat_map(|body| {
            lower_main(body).functions[0].blocks[0]
                .instrs
                .iter()
                .filter_map(|i| match i {
                    Instr::Print { newline, .. } => Some(*newline),
                    _ => None,
                })
                .collect::<Vec<bool>>()
        })
        .collect();
    assert_eq!(ends, vec![false, true, false, false]);
}

/// A `println()` with nothing to write is a blank line, and must not reach
/// back and attach its line ending to the write *before* it.
///
/// The first shape of this rule looked for text left over rather than for a
/// value of its own, and an empty `println` has none either — so the blank
/// line disappeared into the line above it. Nothing in a dump showed it;
/// running `examples/format.tc` did.
#[test]
fn an_empty_println_is_a_blank_line_and_not_a_second_one_on_the_line_above() {
    let ir = lower_main("println(1);\nprintln();\nprintln(2);");
    assert_eq!(ir.texts, vec!["\n".to_string()], "the blank line is its own write");
    let kinds: Vec<&str> = ir.functions[0].blocks[0]
        .instrs
        .iter()
        .filter_map(|i| match i {
            Instr::Print { newline: true, .. } => Some("println"),
            Instr::Print { .. } => Some("print"),
            Instr::PrintText { .. } => Some("text"),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, vec!["println", "text", "println"]);
}

/// A `println` whose last part is text still ends the line with that text,
/// exactly as before: there is a piece to join the newline to.
#[test]
fn a_println_ending_in_text_still_ends_the_line_with_the_text() {
    let ir = lower_main("int n = 1;\nprintln(\"n is %d.\", n);");
    assert_eq!(ir.texts, vec!["n is ".to_string(), ".\n".to_string()]);
    assert!(
        ir.functions[0].blocks[0]
            .instrs
            .iter()
            .all(|i| !matches!(i, Instr::Print { newline: true, .. })),
        "the text ends the line, so no value does"
    );
}

/// Every value is evaluated before anything is written.
///
/// A `print` is written like a call and read like one, so its arguments go
/// first — otherwise a call that writes something itself would land in the
/// middle of this line rather than before it.
#[test]
fn the_values_are_evaluated_before_the_first_write() {
    let ir = lower_src(
        "fn f() -> int {\n  return 1;\n}\n\
         fn main() {\n  println(\"a %d b %d\", f(), f());\n}",
    );
    let main = ir.functions.iter().find(|f| f.name == "main").expect("main");
    let kinds: Vec<&str> = main.blocks[0]
        .instrs
        .iter()
        .filter_map(|i| match i {
            Instr::Call { .. } => Some("call"),
            Instr::Print { .. } => Some("print"),
            Instr::PrintText { .. } => Some("text"),
            _ => None,
        })
        .collect();
    // Six pieces, not seven: the format ends in a specifier, so the last
    // *value* ends the line and there is nothing to write after it.
    assert_eq!(kinds, vec!["call", "call", "text", "print", "text", "print"]);
    assert_eq!(ir.texts, vec!["a ".to_string(), " b ".to_string()]);
}

/// A `print` with nothing to write lowers to nothing at all; a `println`
/// with nothing to write lowers to the line ending alone.
#[test]
fn writing_nothing_costs_nothing() {
    assert!(lower_main("print();").functions[0].blocks[0].instrs.is_empty());
    assert_eq!(lower_main("println();").texts, vec!["\n".to_string()]);
}

/// Text and strings are interned in separate tables because they are laid
/// out differently — and the same words in both roles land in both.
#[test]
fn the_same_words_can_be_text_in_one_place_and_a_string_in_another() {
    let ir = lower_main("string s = \"hi\";\nprint(\"hi\");\nprint(s);");
    assert_eq!(ir.texts, vec!["hi".to_string()]);
    assert_eq!(ir.strings, vec![vec!['h', 'i']]);
}

// -- float ---------------------------------------------------------------

/// A float literal becomes the bits of its double, and the instructions
/// that read those bits say so. Nothing else in the IR changes shape.
#[test]
fn a_float_travels_as_the_bits_of_its_double() {
    let ir = lower_main("float a = 1.5;\nfloat b = a * 2.0;\nprintln(b < a);");
    assert_eq!(
        body_dump(&ir),
        concat!(
            "entry0:\n",
            "  0  %a = const 4609434218613702656\n",
            "  1  %b = mul.f %a, 2f\n",
            "  2  %t2 = cmp.f < %b, %a\n",
            "  3  println bool %t2\n",
            "  4  return\n",
        )
    );
    assert_eq!(f64::from_bits(4609434218613702656u64), 1.5);
}

/// Folding a float is done in `f64`, not on the bits, so the compiler and
/// the machine cannot come to different answers.
#[test]
fn float_constants_fold_as_floats() {
    let ir = lower_main("println(1.5 + 2.25);");
    assert_eq!(body_dump(&ir), "entry0:\n  0  println float 3.75f\n  1  return\n");

    // Adding the two bit patterns as integers would answer this instead,
    // which is what makes the `num` on the instruction load-bearing.
    let wrong = f64::to_bits(1.5) + f64::to_bits(2.25);
    assert_ne!(wrong, f64::to_bits(3.75));
}

/// `-x` is a subtraction from **negative** zero. `0.0 - x` would answer
/// `+0.0` where `x` was `+0.0`, and the difference is invisible until
/// something divides by the result.
#[test]
fn negating_a_float_subtracts_from_negative_zero() {
    let ir = lower_main("float a = 0.0;\nfloat b = -a;\nprintln(b);");
    assert!(body_dump(&ir).contains("%b = sub.f -0f, %a"), "{}", body_dump(&ir));
    assert_eq!(negate_const(Num::Float, 0.0f64.to_bits() as i64), (-0.0f64).to_bits() as i64);
}

/// A conversion that can be settled here is, and the one that cannot stays
/// an instruction — the same bargain `char(n)` strikes.
#[test]
fn a_constant_conversion_is_settled_where_it_can_be() {
    // Both fold to a `const`, and neither leaves an instruction that could
    // stop the program.
    let up = body_dump(&lower_main("float f = float(3);\nprintln(f);"));
    assert!(up.contains("%f = const 4613937818241073152"), "{up}");
    let down = body_dump(&lower_main("int n = int(3.75);\nprintln(n);"));
    assert!(down.contains("%n = const 3"), "{down}");

    // What only the running program knows stays an instruction, and only in
    // the direction that can fail.
    let ir = lower_main("int n = 3;\nfloat f = float(n) / 2.0;\nprintln(int(f));");
    let dump = body_dump(&ir);
    assert!(dump.contains(" = int %"), "{dump}");
}

/// Float arithmetic answers whatever it is given — an infinity, a NaN — so
/// there is nothing to guard and nothing keeping a result nobody reads.
#[test]
fn float_arithmetic_cannot_fail() {
    let division = |num| Instr::Bin {
        num,
        op: BinOp::Div,
        dst: VReg(0),
        lhs: Value::Reg(VReg(1)),
        rhs: Value::Reg(VReg(2)),
    };
    assert!(!division(Num::Float).can_fail());
    assert!(
        division(Num::Int).can_fail(),
        "an int division still has a zero divisor to worry about"
    );

    // Only one direction of the conversion can.
    let cast = |to| Instr::Cast { dst: VReg(0), to, src: Value::Reg(VReg(1)) };
    assert!(cast(Num::Int).can_fail());
    assert!(!cast(Num::Float).can_fail());
}
