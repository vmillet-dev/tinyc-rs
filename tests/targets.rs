//! The contracts every backend has to keep, checked against *every* target the
//! compiler lists.
//!
//! The claim this file exists to hold the compiler to is the one in
//! `codegen`'s module docs: **adding a target means writing one new module and
//! adding a variant, and nothing else in the compiler changes**. A claim like
//! that is worth nothing until something checks it, and nothing could while
//! every test named `Target::X86_64Windows` outright.
//!
//! So nothing here names a target. Each test walks [`Target::names`], which is
//! the same list `--target` accepts — so the day a second backend is added, it
//! arrives already covered, and any of these that it fails is a real answer
//! about the port rather than a test to go and write.
//!
//! What is *not* here is anything about the text a backend emits. A prologue,
//! a symbol prefix and a mnemonic belong to the one backend that has them, and
//! are checked in its own module.

use std::path::Path;

use tinyc::codegen::{Target, backend_for, regalloc};

/// The examples that must keep compiling, whatever the target.
///
/// `examples/hello.tc` is deliberately absent: it is a scratch file for trying
/// things out by hand, so it is allowed to be broken at any time. Anything that
/// should stay working belongs in its own example listed here.
const EXAMPLES: [&str; 13] = [
    "arith.tc",
    "float.tc",
    "spill.tc",
    "reassign.tc",
    "bool.tc",
    "control_flow.tc",
    "functions.tc",
    "enums.tc",
    "arrays.tc",
    "classes.tc",
    "strings.tc",
    "lists.tc",
    "interactive.tc",
];

fn example(file: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples").join(file);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Every target the CLI will accept, as the thing itself rather than its name.
///
/// Going through `from_name` rather than over `TARGETS` directly is deliberate:
/// it is the same door `--target` comes through, so a target that is listed but
/// unreachable fails here rather than only when someone types it.
fn every_target() -> Vec<(&'static str, Target)> {
    Target::names()
        .into_iter()
        .map(|name| {
            let target = Target::from_name(name)
                .unwrap_or_else(|| panic!("`{name}` is listed by `Target::names` but unknown"));
            (name, target)
        })
        .collect()
}

#[test]
fn there_is_at_least_one_target_to_check() {
    // Every other test in this file passes vacuously on an empty list, which
    // would be the quietest way for this whole file to stop meaning anything.
    assert!(!every_target().is_empty(), "the compiler lists no targets at all");
}

/// Every example compiles, for every target.
///
/// The broadest statement of the claim: a backend that cannot lower some
/// construct the language has does not get to be a target.
#[test]
fn every_target_compiles_every_example() {
    for (name, target) in every_target() {
        for file in EXAMPLES {
            let text = example(file);
            let compiled = tinyc::with_compiler_stack(|| tinyc::compile(&text, target))
                .unwrap_or_else(|errors| panic!("{file} failed for {name}: {errors:?}"));

            assert!(!compiled.asm.is_empty(), "{name} emitted nothing for {file}");
            assert_eq!(compiled.backend, name, "a backend reported a name it is not listed under");
            assert!(
                compiled.asm.contains(name),
                "{file}: the assembly for {name} does not say what it was built for"
            );
        }
    }
}

/// One allocation per function, in the program's own order, and each one
/// internally consistent.
///
/// The backend pairs `allocations[i]` with `functions[i]` by position and
/// nothing else, so a target whose register file makes the allocator do
/// something unexpected would show up as assembly nobody can read — this is the
/// check that comes first.
#[test]
fn every_target_allocates_consistently() {
    for (name, target) in every_target() {
        let backend = backend_for(target);
        for file in EXAMPLES {
            let text = example(file);
            let compiled = tinyc::with_compiler_stack(|| tinyc::compile(&text, target)).unwrap();

            assert_eq!(
                compiled.allocations.len(),
                compiled.ir.functions.len(),
                "{name}/{file}: one allocation per function"
            );
            for (function, allocation) in compiled.ir.functions.iter().zip(&compiled.allocations) {
                if let Err(problem) = regalloc::verify(allocation, backend.register_file()) {
                    panic!(
                        "{name}/{file}: {} was allocated inconsistently: {problem}",
                        function.signature(&compiled.ir.table)
                    );
                }
            }
        }
    }
}

/// Every target describes a register file the allocator can actually work from.
///
/// The allocator is target-independent precisely because it is handed this
/// description instead of knowing any machine, so a description that does not
/// hold together is the one way a new backend can break code the port never
/// touched.
#[test]
fn every_target_describes_a_usable_register_file() {
    for (name, target) in every_target() {
        let backend = backend_for(target);
        let registers = backend.register_file();

        assert!(!backend.name().is_empty(), "{name} has no name");
        assert!(registers.max_args > 0, "{name} passes no arguments in registers at all");
        assert!(
            !registers.caller_saved.is_empty() || !registers.callee_saved.is_empty(),
            "{name} gives the allocator nothing to hand out"
        );

        // Every allocatable register has to be nameable, and belong to exactly
        // one pool: one in both would be handed out twice.
        for reg in registers.caller_saved.iter().chain(&registers.callee_saved) {
            assert!(!registers.name(*reg).is_empty(), "{name}: {reg:?} has no name");
            assert!(
                (reg.0 as usize) < registers.names.len(),
                "{name}: {reg:?} is outside the register file"
            );
        }
        for reg in &registers.caller_saved {
            assert!(!registers.callee_saved.contains(reg), "{name}: {reg:?} is in both pools");
        }

        // A value live across a call has to have somewhere to go that a call
        // does not destroy. With no callee-saved register it would have to
        // spill, which the allocator can do — but a target claiming none is far
        // more likely to have forgotten to fill the list in.
        assert!(
            !registers.callee_saved.is_empty(),
            "{name} lists no callee-saved register, so nothing can stay live across a call"
        );
    }
}

/// The front end does not depend on the target.
///
/// Tokens and a tree are facts about the source text, so every target has to
/// see exactly the same ones. If this ever fails, something target-specific has
/// leaked in front of `codegen` — which is the failure the whole design is
/// arranged to prevent, and the one that would be hardest to spot from the
/// output.
#[test]
fn the_tree_the_front_end_builds_is_the_same_for_every_target() {
    for file in EXAMPLES {
        let text = example(file);
        let mut agreed: Option<(String, usize)> = None;

        for (name, target) in every_target() {
            let mut seen = None;
            let _ = tinyc::compile_with(&text, target, tinyc::Options::default(), |stage| match stage {
                tinyc::Stage::Ast(ast) => {
                    seen = Some((tinyc::ast::dump(ast), 0));
                    false
                }
                tinyc::Stage::Tokens(tokens) => {
                    seen = Some((String::new(), tokens.len()));
                    true
                }
                _ => true,
            });
            let (tree, _) = seen.expect("the pipeline reaches the tree");

            match &agreed {
                None => agreed = Some((tree, 0)),
                Some((first, _)) => {
                    assert_eq!(first, &tree, "{file}: {name} parsed a different tree");
                }
            }
        }
    }
}

/// How many parameters a function may declare comes from the target, not from
/// the type checker.
///
/// `sema` is handed `RegisterFile::max_args` rather than a number of its own,
/// which is what lets a target with a different calling convention accept a
/// different count without the type checker knowing it exists. Both halves are
/// checked here: the limit is reached, and one past it is refused in words.
#[test]
fn the_parameter_limit_is_the_targets_own() {
    for (name, target) in every_target() {
        let max = backend_for(target).register_file().max_args;

        let parameters =
            |n: usize| (0..n).map(|i| format!("int p{i}")).collect::<Vec<_>>().join(", ");
        let program = |n: usize| {
            format!("fn f({}) -> int {{\n  return p0;\n}}\nfn main() {{\n  println(1);\n}}\n", parameters(n))
        };

        assert!(
            tinyc::with_compiler_stack(|| tinyc::compile(&program(max), target)).is_ok(),
            "{name} refused a function with the {max} parameters it says it passes"
        );

        let Err(errors) = tinyc::with_compiler_stack(|| tinyc::compile(&program(max + 1), target))
        else {
            panic!("{name} accepted {} parameters, one more than it can pass", max + 1)
        };
        assert!(
            errors[0].message.contains(&max.to_string()),
            "{name}: the refusal should say how many fit, said: {}",
            errors[0].message
        );
    }
}

/// Every program in `examples/errors/` is refused, whatever the target.
///
/// These are all front-end mistakes, so no backend gets to have an opinion
/// about them: a target that compiled one would be accepting a program the
/// language does not have.
///
/// The directory is read rather than listed, so an example added without a test
/// beside it is still checked for the one thing that matters most — that it
/// fails at all.
#[test]
fn every_target_refuses_every_error_example() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/errors");
    let mut checked = 0;

    for entry in std::fs::read_dir(&directory).expect("examples/errors should be readable") {
        let path = entry.expect("a readable directory entry").path();
        if path.extension().is_none_or(|ext| ext != "tc") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let file = path.file_name().unwrap().to_string_lossy().into_owned();

        for (name, target) in every_target() {
            let refused = tinyc::with_compiler_stack(|| tinyc::compile(&text, target));
            let Err(errors) = refused else { panic!("{file} compiled for {name}, but must not") };
            assert!(!errors.is_empty(), "{file}: refused for {name} with no diagnostic");
            assert!(
                errors.iter().all(|d| !d.message.is_empty()),
                "{file}: {name} produced a diagnostic with no message"
            );
        }
        checked += 1;
    }

    assert!(checked > 0, "no error examples were found in {}", directory.display());
}

// -- the door `--target` comes through -------------------------------------

#[test]
fn every_target_answers_to_the_name_it_is_listed_under() {
    // `--target` matches on these strings, and `Target::names` is what the CLI
    // prints when it does not recognise one. A name in the list that does not
    // resolve would be advice that does not work.
    for name in Target::names() {
        assert!(Target::from_name(name).is_some(), "`{name}` is listed but unknown");
    }
}

#[test]
fn no_two_targets_share_a_name() {
    // `from_name` takes the first match, so a duplicate would make one of them
    // permanently unreachable.
    let mut names = Target::names();
    let listed = names.len();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), listed, "two targets are listed under the same name");
}

#[test]
fn an_unknown_target_is_none_rather_than_a_default() {
    // A default would silently compile for something other than what was asked
    // for — the one mistake in a compiler that is invisible until it runs.
    for unknown in ["", " ", "x86_64", "nonsense", "x86_64-freebsd"] {
        assert_eq!(Target::from_name(unknown), None, "`{unknown}` should not resolve");
    }
    for name in Target::names() {
        assert_eq!(Target::from_name(&name.to_uppercase()), None, "the match is exact");
    }
}
