//! Stage 4: AST -> IR.
//!
//! One `Lowering` per function walks the tree and appends instructions to the
//! block it is currently filling. The submodules split that walk by what it is
//! walking: statements, places and aggregates, expressions, and `print`.

use std::collections::{HashMap, HashSet};

use crate::ast::{
    ArmBody, BinOp, Block as AstBlock, Builtin, ClassId, CmpOp, EnumId, Expr, ExprKind, FieldInit,
    FnDecl, LogicOp, MatchArm, Pattern, Place, Prim, PrintPart, Program as Ast, Stmt, Ty,
    TypeTable, fits_in_an_int, is_scalar_value,
};
use crate::diag::{Diagnostic, Result};
use crate::sema::Types;

use super::{
    Block, BlockId, BlockKind, CHAR_BYTES, Function, FuncId, Instr, MAX_FRAME_BYTES, Num, Program,
    Runtime, StrId, Terminator, TextId, VReg, Value, fold_bin, fold_cmp, fold_logic,
    negate_const, prune_unreachable, zero_to_subtract_from,
};

mod expr;
mod owned;
mod place;
mod print;
mod stmt;

use owned::{builds_its_own, mentions, owned_strings};

/// Lower a type-checked AST to IR. Assumes [`crate::sema::check`] succeeded.
///
/// The one thing that can still fail here is the size of a frame, and it can
/// only fail here: how much stack a function wants is not a fact about its
/// types, it is the sum of every aggregate this stage hands room to. The number
/// that goes into `sub rsp` is the number checked, so the two cannot drift.
pub fn lower(ast: &Ast, types: &Types) -> Result<Program> {
    // Function ids follow declaration order, so a call can be lowered to an
    // index without caring whether the callee has been lowered yet — which is
    // exactly what recursion and forward calls need.
    //
    // The first declaration of a name wins, matching the table `sema` built:
    // a duplicate is an error, and the two stages must at least agree on which
    // one they were talking about.
    // A method's name lives in its class rather than in the program, so two
    // classes may both have an `area`. Qualifying it here is what keeps the one
    // flat table of callables — and, further on, keeps their symbols apart.
    let names: Vec<String> = qualified_names(ast);
    let func_ids: Vec<FuncId> = (0..ast.functions.len() as u32).map(FuncId).collect();

    let mut ids: HashMap<String, FuncId> = HashMap::new();
    for (index, name) in names.iter().enumerate() {
        ids.entry(name.clone()).or_insert(FuncId(index as u32));
    }

    let mut strings = Strings::default();
    let mut functions = Vec::new();
    let mut errors = Vec::new();
    for (index, decl) in ast.functions.iter().enumerate() {
        let lowering = Lowering {
            blocks: Vec::new(),
            vreg_names: Vec::new(),
            current: BlockId(0),
            scopes: vec![HashMap::new()],
            loops: Vec::new(),
            frame_bytes: 0,
            frame_peak: 0,
            out_pointer: None,
            name_counts: HashMap::new(),
            types,
            table: types.table(),
            func_ids: &func_ids,
            strings: &mut strings,
            ids: &ids,
            owned: owned_strings(decl, types),
        };
        let mut lowered =
            lowering.run(decl, types.ret_of(index), types.params_of(index));
        lowered.name = names[index].clone();
        if lowered.frame_bytes > MAX_FRAME_BYTES {
            errors.push(too_much_stack(&lowered, decl));
        }
        functions.push(lowered);
    }

    // One vtable per class, holding the implementation each slot resolved to.
    let vtables: Vec<Vec<FuncId>> = types
        .table()
        .classes
        .iter()
        .map(|class| class.methods.iter().map(|m| func_ids[m.function]).collect())
        .collect();

    // Before the pruning, deliberately: a frame nothing reaches is still one
    // the program asked for, and `sema` reports a mistake in an uncalled
    // function too. What is emitted must not decide what is diagnosed, or a
    // program would start failing to compile the moment something called it.
    if !errors.is_empty() {
        errors.sort_by_key(|d| d.span.offset);
        return Err(errors);
    }

    let (functions, vtables) =
        prune_unreachable_functions(functions, vtables, ids.get(crate::sema::ENTRY_POINT));
    Ok(Program {
        functions,
        strings: strings.chars,
        texts: strings.texts,
        table: types.table().clone(),
        vtables,
    })
}


/// A function whose locals no stack would hold.
fn too_much_stack(lowered: &Function, decl: &FnDecl) -> Diagnostic {
    let bytes = lowered.frame_bytes;
    let size = match bytes == u32::MAX {
        true => "more than four gigabytes".to_string(),
        false => format!("{bytes} bytes"),
    };
    Diagnostic::new(format!("`{}` needs too much stack", decl.name), decl.name_span)
        .with_label(format!("{size} of locals, and at most {MAX_FRAME_BYTES} are supported"))
        .with_note(
            "every value too big for a register lives in the frame, and the frame is reserved \
             for the whole call; `int[]` is what holds a quantity the stack cannot",
            None,
        )
}

/// The name each of the program's functions is known by once methods and free
/// functions share one list: `Circle$area` for a method, the plain name
/// otherwise.
fn qualified_names(ast: &Ast) -> Vec<String> {
    let mut names: Vec<String> = ast.functions.iter().map(|f| f.name.clone()).collect();
    for class in &ast.classes {
        for &at in &class.methods {
            names[at] = format!("{}${}", class.name, ast.functions[at].name);
        }
    }
    names
}

/// The literals every function shares, plus the index that keeps interning them
/// a lookup rather than a scan.
#[derive(Default)]
struct Strings {
    chars: Vec<Vec<char>>,
    ids: HashMap<Vec<char>, StrId>,
    texts: Vec<String>,
    text_ids: HashMap<String, TextId>,
}

/// Drop the functions nothing can call, and renumber the survivors.
///
/// The same walk as [`prune_unreachable`], one level up: the call graph instead
/// of the control flow graph, rooted at the entry point rather than at block 0.
/// A helper nobody calls costs a label, a prologue and an epilogue otherwise.
fn prune_unreachable_functions(
    functions: Vec<Function>,
    vtables: Vec<Vec<FuncId>>,
    entry: Option<&FuncId>,
) -> (Vec<Function>, Vec<Vec<FuncId>>) {
    let Some(&entry) = entry else {
        // No entry point: `sema` has already rejected the program, and there is
        // no root to walk from.
        return (functions, vtables);
    };

    let mut reachable = vec![false; functions.len()];
    let mut stack = vec![entry];
    while let Some(id) = stack.pop() {
        let index = id.0 as usize;
        if std::mem::replace(&mut reachable[index], true) {
            continue;
        }
        for block in &functions[index].blocks {
            for instr in &block.instrs {
                match instr {
                    Instr::Call { callee, .. } => stack.push(*callee),
                    // Making an object is what makes its methods callable, and
                    // the only thing that does: a virtual call names a slot,
                    // and the objects that could answer it are exactly the ones
                    // some `New` built.
                    Instr::VTable { class, .. } => {
                        stack.extend(&vtables[class.0 as usize]);
                    }
                    _ => {}
                }
            }
        }
    }

    // Old index -> new index, for the calls and the tables that name them.
    let mut renumber = vec![FuncId(0); functions.len()];
    let mut next = 0;
    for (index, keep) in reachable.iter().enumerate() {
        if *keep {
            renumber[index] = FuncId(next);
            next += 1;
        }
    }

    let vtables = vtables
        .into_iter()
        .map(|slots| slots.into_iter().map(|at| renumber[at.0 as usize]).collect())
        .collect();

    let functions = functions
        .into_iter()
        .zip(&reachable)
        .filter(|(_, keep)| **keep)
        .map(|(mut function, _)| {
            for block in &mut function.blocks {
                for instr in &mut block.instrs {
                    if let Instr::Call { callee, .. } = instr {
                        *callee = renumber[callee.0 as usize];
                    }
                }
            }
            function
        })
        .collect();
    (functions, vtables)
}


/// Whether the room being written into is brand new, or already has a name.
///
/// It decides one thing, and it is the difference between two and four
/// instructions per element: whether an aggregate **literal** may be built where
/// it is going, rather than somewhere else and then copied over.
///
/// * [`Room::Fresh`] — a field of an object being constructed, an element of an
///   array literal, the room a declaration just reserved, the room a caller
///   passed for a return. Nothing can name it yet, so the expression filling it
///   cannot read it, and filling it piece by piece is not observable.
/// * [`Room::Named`] — the target of an assignment. Here it very much can:
///
///   ```text
///   int[2] a = [1, 2];
///   a = [a[1], a[0]];      // a swap
///   ```
///
///   Filling `a` element by element would write `a[1]` into `a[0]` and then
///   read it straight back out, and the swap would answer `[2, 2]`. So an
///   assignment builds the literal elsewhere and copies it, which is what makes
///   the whole value change at once — the same reason assignment copies rather
///   than aliasing in the first place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Room {
    Fresh,
    Named,
}


/// A block while it is still being filled in.
///
/// The terminator is an `Option` on purpose: "not decided yet" is a state the
/// lowering really is in, and giving it a `Terminator` value instead would mean
/// a block whose terminator was never patched still assembles — into a plausible
/// but wrong `ret`. [`Lowering::run`] turns the `None` that should be impossible
/// into a panic rather than a miscompilation.
struct PendingBlock {
    kind: BlockKind,
    instrs: Vec<Instr>,
    term: Option<Terminator>,
}

/// One open loop, and the blocks whose exit it still owes an answer.
///
/// A `break` knows where it is going only once the loop's `done` block exists,
/// and a `continue` only once it is settled whether the loop needs a step block
/// — both of which happen *after* the body has been lowered. So each jump
/// finishes its own block later: it records which block it left and the loop
/// patches the terminator in on the way out, the same way [`Lowering::if_stmt`]
/// patches the arms of a diamond.
#[derive(Default)]
struct LoopFrame {
    /// Blocks ending in a `break`, waiting for the loop's exit.
    breaks: Vec<BlockId>,
    /// Blocks ending in a `continue`, waiting for the loop's back edge.
    continues: Vec<BlockId>,
}

struct Lowering<'a> {
    blocks: Vec<PendingBlock>,
    vreg_names: Vec<String>,
    /// The block instructions are currently appended to.
    current: BlockId,
    /// Variable name -> virtual register, one map per open scope.
    scopes: Vec<HashMap<String, (VReg, Ty)>>,
    /// The loops enclosing the statement being lowered, innermost last.
    loops: Vec<LoopFrame>,
    /// Where the next aggregate goes, which is how much of the frame is in use
    /// *here*. It goes back down when a block ends — see [`Self::block_stmts`].
    frame_bytes: u32,
    /// The most that was ever in use at once, and so what the prologue reserves.
    ///
    /// Two counters rather than one because room can now be given back: what a
    /// block took is available again to the block after it, and only the
    /// high-water mark is a fact about the function.
    frame_peak: u32,
    /// Where a `return` copies to, for a function whose answer does not fit in
    /// a register. `None` for every other function.
    out_pointer: Option<VReg>,
    /// How many registers have borne each name, so a shadowing declaration in
    /// another scope gets a distinct dump name (`i`, then `i.1`).
    name_counts: HashMap<String, u32>,
    types: &'a Types,
    /// Every type the program has, for turning a variant name into its tag and
    /// an array type into a size.
    table: &'a TypeTable,
    /// Which lowered function each of the program's functions became, so a
    /// method's implementation can be named from its class's table.
    func_ids: &'a [FuncId],
    /// Shared with every other function: the strings all land in one section.
    strings: &'a mut Strings,
    ids: &'a HashMap<String, FuncId>,
    /// The string variables of this function nothing else can be holding, and
    /// so the ones `s = s + e` may grow where they stand. See
    /// [`owned_strings`].
    owned: HashSet<String>,
}


impl Lowering<'_> {
    pub(super) fn run(mut self, decl: &FnDecl, ret: Option<Ty>, param_types: &[Ty]) -> Function {
        self.new_block(BlockKind::Entry);

        // An aggregate does not come back in a register, so the caller reserves
        // the room and hands its address in ahead of everything else. The
        // callee fills what the caller already owns, which is why returning one
        // hands nothing outward and nothing can dangle.
        let returns_aggregate = ret.is_some_and(|ty| !ty.fits_in_a_register());
        if returns_aggregate {
            let dst = self.fresh("out");
            self.emit(Instr::Param { dst, index: 0 });
            self.out_pointer = Some(dst);
        }
        let first = u32::from(returns_aggregate);

        // Parameters next, so each one is defined at the top of the function.
        //
        // An aggregate parameter is no different here: what arrived in the
        // register is its address, and the address is what the register keeps.
        let mut params = Vec::new();
        for (index, (param, ty)) in decl.params.iter().zip(param_types).enumerate() {
            let dst = self.declare(&param.name, *ty);
            self.emit(Instr::Param { dst, index: index as u32 + first });
            params.push(dst);
        }

        for stmt in &decl.body.stmts {
            self.stmt(stmt);
        }
        // Falling off the end returns nothing. For a function with a return
        // type sema has already proved this is unreachable.
        self.terminate(Terminator::Return(None));

        // Every block that stopped being the current one was finished by the
        // construct that moved away from it, and the last one was just finished
        // above. A `None` here would mean a path forgot to.
        let blocks = self
            .blocks
            .into_iter()
            .enumerate()
            .map(|(index, block)| Block {
                kind: block.kind,
                index: index as u32,
                instrs: block.instrs,
                term: block.term.unwrap_or_else(|| {
                    panic!("block {index} of `{}` was left unterminated", decl.name)
                }),
            })
            .collect();

        Function {
            name: decl.name.clone(),
            params,
            ret,
            // What the prologue reserves is the most that was ever in use at
            // once, not what is in use at the end — which is nothing, since
            // every scope has closed by now.
            frame_bytes: self.frame_peak,
            blocks: prune_unreachable(blocks),
            vreg_names: self.vreg_names,
        }
    }

    // -- blocks ------------------------------------------------------------

    /// Append a new block, make it current, and return its id. It has no
    /// terminator until [`Self::terminate`] gives it one.
    pub(super) fn new_block(&mut self, kind: BlockKind) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        self.blocks.push(PendingBlock { kind, instrs: Vec::new(), term: None });
        self.current = id;
        id
    }

    pub(super) fn emit(&mut self, instr: Instr) {
        self.blocks[self.current.0 as usize].instrs.push(instr);
    }

    /// Finish the current block.
    pub(super) fn terminate(&mut self, term: Terminator) {
        self.finish(self.current, term);
    }

    /// Finish a block that is no longer the current one.
    pub(super) fn finish(&mut self, block: BlockId, term: Terminator) {
        self.blocks[block.0 as usize].term = Some(term);
    }

    pub(super) fn switch_to(&mut self, block: BlockId) {
        self.current = block;
    }

    // -- names -------------------------------------------------------------

    pub(super) fn fresh(&mut self, name: &str) -> VReg {
        let reg = VReg(self.vreg_names.len() as u32);
        let count = self.name_counts.entry(name.to_string()).or_insert(0);
        let label = if *count == 0 { name.to_string() } else { format!("{name}.{count}") };
        *count += 1;
        self.vreg_names.push(label);
        reg
    }

    /// Temporaries are named after their own index, so `%t7` is always virtual
    /// register 7.
    pub(super) fn fresh_temp(&mut self) -> VReg {
        let reg = VReg(self.vreg_names.len() as u32);
        self.vreg_names.push(format!("t{}", reg.0));
        reg
    }

    /// Give a name a register, and remember the type it holds.
    ///
    /// The type is carried because an array's *length* is part of it, and the
    /// length is what a bounds check and an allocation both need. Nothing else
    /// asks.
    pub(super) fn declare(&mut self, name: &str, ty: Ty) -> VReg {
        let reg = self.fresh(name);
        self.scopes
            .last_mut()
            .expect("a scope is always open")
            .insert(name.to_string(), (reg, ty));
        reg
    }

    pub(super) fn lookup(&self, name: &str) -> VReg {
        self.binding(name).0
    }

    pub(super) fn lookup_type(&self, name: &str) -> Ty {
        self.binding(name).1
    }

    pub(super) fn binding(&self, name: &str) -> (VReg, Ty) {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
            .expect("sema rejects undeclared variables")
    }

}
