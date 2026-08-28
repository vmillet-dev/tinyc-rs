use super::*;
use crate::ir::{Program, Terminator};
use crate::{lexer, parser, sema};

fn program(src: &str) -> Program {
    let ast = parser::parse(&lexer::lex(src).unwrap()).unwrap();
    let types = sema::check(&ast, 4).unwrap();
    crate::ir::lower(&ast, &types).expect("the frames should fit")
}

fn main_of(src: &str) -> Function {
    let mut ir = program(&format!("fn main() {{\n{src}\n}}\n"));
    construct(&mut ir);
    ir.functions.into_iter().next().expect("a main")
}

/// The dump of `main` in SSA form, without its signature line.
fn ssa(src: &str) -> String {
    let mut ir = program(&format!("fn main() {{\n{src}\n}}\n"));
    construct(&mut ir);
    body(&ir)
}

/// The same, put back into the form the backend reads.
fn back(src: &str) -> String {
    let mut ir = program(&format!("fn main() {{\n{src}\n}}\n"));
    construct(&mut ir);
    destruct(&mut ir);
    body(&ir)
}

fn body(ir: &Program) -> String {
    let dump = ir.dump();
    let start = dump.find(":\n").expect("a signature line") + 2;
    dump[start..].trim_end().to_string() + "\n"
}

/// Every register written once, and every operand naming one that is.
fn is_ssa(function: &Function) -> Result<(), String> {
    let mut written = vec![0usize; function.vreg_count()];
    for block in &function.blocks {
        for &param in &block.params {
            written[param.0 as usize] += 1;
        }
        for instr in &block.instrs {
            if let Some(def) = instr.def() {
                written[def.0 as usize] += 1;
            }
        }
    }
    for (index, count) in written.iter().enumerate() {
        if *count > 1 {
            return Err(format!("%{} is written {count} times", function.name_of(VReg(index as u32))));
        }
    }

    let mut read = Vec::new();
    for block in &function.blocks {
        for instr in &block.instrs {
            instr.uses(|reg| read.push(reg));
        }
        block.term.uses(|reg| read.push(reg));
    }
    match read.iter().find(|reg| written[reg.0 as usize] == 0) {
        Some(reg) => Err(format!("%{} is read but never written", function.name_of(*reg))),
        None => Ok(()),
    }
}

/// Every target hands its block exactly one value per parameter.
fn arities_agree(function: &Function) -> Result<(), String> {
    for block in &function.blocks {
        for target in block.term.targets() {
            let wanted = function.block(target.block).params.len();
            if target.args.len() != wanted {
                return Err(format!(
                    "{} hands {} {} arguments for {wanted} parameters",
                    block.label(),
                    function.block(target.block).label(),
                    target.args.len()
                ));
            }
        }
    }
    Ok(())
}

const PROGRAMS: &[&str] = &[
    "int n = 1;\nif (n < 5) {\n  n = n + 1;\n} else {\n  n = n + 2;\n}\nprintln(n);",
    "int t = 0;\nfor (int i = 0; i < 3; i = i + 1) {\n  t = t + i;\n}\nprintln(t);",
    "int i = 0;\nwhile (i < 10) {\n  i = i + 1;\n  if (i == 5) { continue; }\n  if (i == 8) { break; }\n}\nprintln(i);",
    "bool b = true && false;\nprintln(b);",
    "int n = 3;\nmatch (n) {\n  1 => { println(1); }\n  _ => { println(0); }\n}",
    "string s = \"a\";\nfor (int i = 0; i < 3; i = i + 1) {\n  s = s + \"b\";\n}\nprintln(s);",
    "int[3] xs = [1, 2, 3];\nint sum = 0;\nfor (int i = 0; i < 3; i = i + 1) {\n  sum = sum + xs[i];\n}\nprintln(sum);",
];

#[test]
fn construction_leaves_every_function_in_ssa_form() {
    for source in PROGRAMS {
        let function = main_of(source);
        is_ssa(&function).unwrap_or_else(|why| panic!("{why}\nfor:\n{source}"));
        arities_agree(&function).unwrap_or_else(|why| panic!("{why}\nfor:\n{source}"));
    }
}

#[test]
fn destruction_leaves_no_parameter_and_no_argument_behind() {
    for source in PROGRAMS {
        let mut ir = program(&format!("fn main() {{\n{source}\n}}\n"));
        construct(&mut ir);
        destruct(&mut ir);
        for function in &ir.functions {
            for block in &function.blocks {
                assert!(block.params.is_empty(), "{} kept parameters", block.label());
                for target in block.term.targets() {
                    assert!(target.args.is_empty(), "{} kept arguments", block.label());
                }
            }
        }
    }
}

#[test]
fn two_definitions_meeting_grow_a_parameter_on_the_block_they_meet_in() {
    let printed = ssa("int n = 1;\nif (n < 5) {\n  n = n + 1;\n} else {\n  n = n + 2;\n}\nprintln(n);");
    assert!(printed.contains("join3(%n.1):"), "{printed}");
    assert!(printed.contains("jump join3(%n.3)"), "{printed}");
    assert!(printed.contains("jump join3(%n.2)"), "{printed}");
    assert!(printed.contains("println int %n.1"), "{printed}");
}

#[test]
fn a_loop_variable_becomes_a_parameter_of_its_header() {
    // The one place the old form could say nothing: `%i` at the top of the loop
    // is the entry's value on the first round and the body's on every one
    // after, and with one register for both there was no way to write that
    // down.
    let printed = ssa("int t = 0;\nfor (int i = 0; i < 3; i = i + 1) {\n  t = t + i;\n}\nprintln(t);");
    assert!(printed.contains("loop1(%t.1, %i.1):"), "{printed}");
    assert!(printed.contains("jump loop1(%t, %i)"), "{printed}");
    assert!(printed.contains("jump loop1(%t.2, %i.2)"), "{printed}");
}

#[test]
fn a_temporary_written_once_keeps_the_name_it_had() {
    // Renaming everything would have turned every `%t3` in every dump into
    // `%t3.1` for no reason a reader could see.
    let printed = ssa("println(1 + 2);");
    assert!(!printed.contains(".1"), "a single definition was versioned:\n{printed}");
}

#[test]
fn arguments_become_copies_in_the_block_the_edge_leaves() {
    let printed = back("int n = 1;\nif (n < 5) {\n  n = n + 1;\n} else {\n  n = n + 2;\n}\nprintln(n);");
    assert!(printed.contains("%n.1 = copy %n.3"), "{printed}");
    assert!(printed.contains("%n.1 = copy %n.2"), "{printed}");
    assert!(!printed.contains('('), "a target kept its arguments:\n{printed}");
}

#[test]
fn a_parameter_handed_its_own_definition_back_costs_nothing() {
    // `while (i < n) { ... }` with a body that does not touch `n` hands the
    // header the same register it already has. A copy of a register to itself
    // is not an instruction worth emitting.
    let printed = back("int n = 10;\nint i = 0;\nwhile (i < n) {\n  i = i + 1;\n}\nprintln(n);");
    assert!(!printed.contains("copy %n\n"), "a self-copy was emitted:\n{printed}");
}

#[test]
fn an_edge_out_of_a_branch_gets_a_block_of_its_own_to_copy_in() {
    // A block ending in a branch cannot hold the copies for one of its edges:
    // they would run on the other one too. It is also where the backend fuses
    // the comparison into the jump, which an instruction in between would stop.
    let mut ir = program(
        "fn main() {\n  int n = 0;\n  while (n < 10) {\n    if (n == 3) { n = n + 2; }\n    n = n + 1;\n  }\n  println(n);\n}\n",
    );
    construct(&mut ir);
    destruct(&mut ir);
    let main = &ir.functions[0];
    for block in &main.blocks {
        if matches!(block.term, Terminator::Branch { .. }) {
            assert!(
                !matches!(block.instrs.last(), Some(Instr::Copy { .. })),
                "{} ends in a copy between its comparison and its branch",
                block.label()
            );
        }
    }
}

#[test]
fn a_swap_between_two_parameters_is_not_lost() {
    // Both copies on one edge happen at once. Writing `%a = %b` before reading
    // `%b` would answer with two copies of the same value.
    let mut function = main_of("int a = 1;
int b = 2;
println(a + b);");
    let mut names = Names::of(&mut function);
    let (a, b) = (VReg(0), VReg(1));
    let copies = sequence(&mut names, vec![(a, Value::Reg(b)), (b, Value::Reg(a))]);

    // Run them, and check the two ended up holding each other's value rather
    // than two copies of one.
    let mut held: Vec<i64> = (0..names.finish().len() as i64).collect();
    for copy in &copies {
        let Instr::Copy { dst, src: Value::Reg(src) } = copy else { panic!("{copies:?}") };
        held[dst.0 as usize] = held[src.0 as usize];
    }
    assert_eq!(held[a.0 as usize], b.0 as i64, "{copies:?}");
    assert_eq!(held[b.0 as usize], a.0 as i64, "{copies:?}");
}

#[test]
fn a_definition_that_reaches_nowhere_else_is_left_alone() {
    // Nothing about a straight run of code needs SSA to say anything, and a
    // pass that rewrote it anyway would make every dump harder to read for
    // nothing.
    let source = "println(1);\nprintln(2);";
    let mut ir = program(&format!("fn main() {{\n{source}\n}}\n"));
    let before = body(&ir);
    construct(&mut ir);
    assert_eq!(before, body(&ir));
}
