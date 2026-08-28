//! Lowering statements: declarations, assignments, control flow and `match`.

use super::*;

impl Lowering<'_> {
    // -- statements --------------------------------------------------------

    /// Lower a block's statements in a scope of their own.
    ///
    /// The frame is given back with the scope, so two blocks that cannot be
    /// running at the same time share their room:
    ///
    /// ```text
    /// if (c) { int[1000] a = ...; } else { int[1000] b = ...; }
    /// ```
    ///
    /// takes eight kilobytes rather than sixteen. That is sound for the same
    /// reason nothing in this language dangles: **no address ever travels
    /// outward**, and inside a function that means no frame address is ever
    /// stored into memory or kept in a variable declared outside — assignment
    /// copies. So when a block's names go out of scope, so does every way of
    /// reaching what they named.
    ///
    /// A block inside a loop is lowered once and re-entered at run time, so its
    /// room is the same room on every iteration, which is what a local in a loop
    /// body has always been.
    pub(super) fn block_stmts(&mut self, block: &AstBlock) {
        let outer = self.frame_bytes;
        self.scopes.push(HashMap::new());
        for stmt in &block.stmts {
            self.stmt(stmt);
        }
        self.scopes.pop();
        self.frame_bytes = outer;
    }

    pub(super) fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Decl { id, name, init, .. } => {
                // The declared type, not the initialiser's: `Shape s = c;`
                // makes a `Shape`, and what it holds may be any of them.
                let ty = self.types.of(*id);
                let dst = self.declare(name, ty);
                match ty.fits_in_a_register() {
                    true => self.keep_into(dst, init),
                    // An aggregate variable owns its room. An expression that
                    // reserved some already moves in; anything else names room
                    // that belongs to something else, and is copied out of it.
                    false if builds_its_own(init) => self.expr_into(dst, init),
                    false => {
                        self.allocate_for(dst, ty);
                        self.write_through(Value::Reg(dst), init, Room::Fresh);
                    }
                }
            }
            Stmt::Assign { target, value } => match target {
                Place::Var { name, .. } => {
                    let (dst, ty) = self.binding(name);
                    // `s = s + a + b`, where nothing else can be holding `s`.
                    // The same answer, added to where `s` already is whenever
                    // the arena can still give that room back — which is what
                    // turns building a string in a loop from quadratic in
                    // *memory* into linear. See `owned_strings`.
                    let chain = match ty.fits_in_a_register() {
                        true => self.append_chain(name, value),
                        false => None,
                    };
                    match chain {
                        Some(pieces) => {
                            for piece in pieces {
                                // Whether this piece was built by this very
                                // statement, and so is nobody else's to lose.
                                let own = self.builds_a_temporary(piece);
                                let rhs = self.expr(piece);
                                self.emit(Instr::RtCall {
                                    dst: Some(dst),
                                    callee: Runtime::Append,
                                    args: vec![
                                        Value::Reg(dst),
                                        rhs,
                                        Value::Const(i64::from(own)),
                                    ],
                                });
                            }
                        }
                        // The variable keeps its register; the assignment
                        // overwrites it. An aggregate variable keeps its
                        // *room*, so the value is copied into it rather than
                        // the address swapped — anything else would make
                        // assignment aliasing.
                        None => match ty.fits_in_a_register() {
                            true => self.keep_into(dst, value),
                            false => self.write_through(Value::Reg(dst), value, Room::Named),
                        },
                    }
                }
                // Everything else names memory rather than a register, so the
                // write goes through an address.
                target => {
                    let addr = self.place_address(target);
                    self.write_through(Value::Reg(addr), value, Room::Named);
                }
            },
            // The routine answers where the list *now* is, and that answer has
            // to land back where the list is named — which is the whole reason
            // `push` takes a place rather than a value.
            Stmt::Push { target, value, .. } => {
                let (elem, bytes) = self.element_of(self.place_type(target));
                let value = self.expr(value);
                // An element too big for a register arrives as its address, and
                // what the routine does with it is a copy. The list may move
                // out from under that copy, and the block it moved *from* is
                // still there to read — the arena never gives anything back,
                // which is what makes `push(xs, xs[0])` mean what it says.
                let (callee, mut rest) = match elem.fits_in_a_register() {
                    true => (Runtime::ListPush, vec![value]),
                    // What goes in is a copy, and a copy owns nothing yet —
                    // so the routine is told whether to give it its own.
                    false => (
                        Runtime::ListPushBig,
                        vec![
                            value,
                            Value::Const(i64::from(bytes)),
                            Value::Const(i64::from(self.table.holds_a_list(elem))),
                        ],
                    ),
                };
                match target {
                    Place::Var { name, .. } => {
                        let (dst, _) = self.binding(name);
                        let mut args = vec![Value::Reg(dst)];
                        args.append(&mut rest);
                        self.emit(Instr::RtCall { dst: Some(dst), callee, args });
                    }
                    target => {
                        let addr = self.place_address(target);
                        let held = self.fresh_temp();
                        self.emit(Instr::Load { dst: held, addr: Value::Reg(addr) });
                        let grown = self.fresh_temp();
                        let mut args = vec![Value::Reg(held)];
                        args.append(&mut rest);
                        self.emit(Instr::RtCall { dst: Some(grown), callee, args });
                        self.emit(Instr::Store {
                            addr: Value::Reg(addr),
                            value: Value::Reg(grown),
                        });
                    }
                }
            }
            Stmt::Print { newline, parts, .. } => self.print_stmt(*newline, parts),
            Stmt::If { cond, then_block, else_block } => self.if_stmt(cond, then_block, else_block),
            Stmt::While { cond, body } => self.while_stmt(cond, body),
            // `for (init; cond; step) body` is exactly `init; while (cond) { body; step; }`
            // with the initialiser's variable scoped to the loop.
            Stmt::For { init, cond, step, body } => {
                let outer = self.frame_bytes;
                self.scopes.push(HashMap::new());
                self.stmt(init);
                self.loop_with_step(cond, body, Some(step));
                self.scopes.pop();
                // The initialiser's variable is scoped to the loop, so its room
                // goes back with it.
                self.frame_bytes = outer;
            }
            // An aggregate answer is copied into the room the caller reserved,
            // and the function then leaves with nothing — there is no address
            // to hand back, which is exactly why none can dangle.
            Stmt::Return { value: Some(expr), .. } if self.out_pointer.is_some() => {
                let out = self.out_pointer.expect("just matched");
                self.write_through(Value::Reg(out), expr, Room::Fresh);
                self.terminate(Terminator::Return(None));
                self.new_block(BlockKind::Unreachable);
            }
            Stmt::Return { value, .. } => {
                // Handed *outward*, so a list somebody else owns is copied
                // here rather than at the call site — which is what lets the
                // caller treat every returned list as its own. Nothing else
                // needs a register of its own: `return 0` stays an immediate.
                let value = value.as_ref().map(|expr| match self.types.of(expr.id) {
                    Ty::List(_) => {
                        let dst = self.fresh_temp();
                        self.keep_into(dst, expr);
                        Value::Reg(dst)
                    }
                    _ => self.expr(expr),
                });
                self.terminate(Terminator::Return(value));
                // Anything written after a `return` still needs somewhere to
                // go. This block has no predecessor, so it is dead code the
                // backend simply never reaches.
                self.new_block(BlockKind::Unreachable);
            }
            Stmt::Match(expr) => self.match_lowering(None, expr),
            Stmt::Break { .. } => self.loop_jump(|frame| &mut frame.breaks),
            Stmt::Continue { .. } => self.loop_jump(|frame| &mut frame.continues),
            Stmt::Call(call) => match &call.kind {
                ExprKind::MethodCall { .. } => self.method_call(None, call),
                // `read_line();` is a line skipped: the call happens, and what
                // it answered is thrown away like any other call statement's.
                ExprKind::Call { name, args, .. } if Builtin::from_name(name).is_some() => {
                    let args: Vec<Value> = args.iter().map(|arg| self.expr(arg)).collect();
                    let callee = Runtime::of(Builtin::from_name(name).expect("just matched"));
                    self.emit(Instr::RtCall { dst: None, callee, args });
                }
                _ => {
                    let (callee, args) = self.call_parts(call);
                    self.emit(Instr::Call { dst: None, callee, args });
                }
            },
        }
    }

    /// Lower a `match` into a chain of equality tests, optionally leaving what
    /// its arms produced in `dst`.
    ///
    /// A variant is its tag, so this is the same shape as `if / else if`, with
    /// one saving that only exhaustiveness makes safe: **the last arm is not
    /// tested at all.** There is nowhere else for a value to be, so the last
    /// test's failure is already the answer — and no arm has to be written for
    /// "none of the above", because there is no such case.
    ///
    /// When `dst` is given, every value arm writes it before jumping to the
    /// join — the same trick `&&` plays, and the same one a non-SSA IR is what
    /// allows. A block arm writes nothing: `sema` has established that control
    /// never reaches its end.
    ///
    /// A jump table would beat the chain on a large enum. It would need an
    /// indirect terminator, which nothing else in this IR wants yet.
    pub(super) fn match_lowering(&mut self, dst: Option<VReg>, expr: &Expr) {
        let ExprKind::Match { scrutinee, arms, .. } = &expr.kind else {
            unreachable!("the caller matched a match");
        };
        let value = self.expr(scrutinee);
        let scrutinee_ty = self.types.of(scrutinee.id);
        // A boxed enum is a pointer, and every test below is about the tag it
        // points at — so that is read once here rather than once per arm. The
        // pointer itself is still what an arm's bindings are read out of.
        let tested = self.tag_of(scrutinee_ty, value);
        let before = self.current;

        // Where each arm's decision begins, so a failing test knows where to
        // send control; and the tests themselves, which cannot be finished
        // until the arm *after* them exists.
        let mut entries = Vec::new();
        let mut tests = Vec::new();
        let mut exits = Vec::new();

        for (index, arm) in arms.iter().enumerate() {
            if index + 1 == arms.len() {
                // The last arm is never tested: either it is the `_` that takes
                // whatever is left, or the domain was countable and the arms
                // before it took everything else. Control simply runs in.
                entries.push(self.new_block(BlockKind::Arm));
            } else {
                // The first test belongs to the block the scrutinee was
                // computed in; each later one gets a block of its own for the
                // previous test to fail into.
                if index > 0 {
                    self.new_block(BlockKind::Case);
                }
                entries.push(self.current);
                let cond = self.arm_test(scrutinee, tested, arm);
                let test = self.current;
                let arm_block = self.new_block(BlockKind::Arm);
                tests.push((test, cond, arm_block));
            }
            // What the pattern named is in scope for exactly this arm, so the
            // arm gets a scope of its own — which is what lets two arms use one
            // name for quite different things.
            self.scopes.push(HashMap::new());
            if let Ty::Enum(id) = scrutinee_ty {
                self.bind_arm_payload(id, value, arm);
            }
            match &arm.body {
                // A value arm leaves its answer where the join will read it.
                // Where the match is a statement `dst` is absent, and `sema`
                // has already rejected an arm that produced one.
                ArmBody::Value(value) => match dst {
                    Some(dst) => self.expr_into(dst, value),
                    None => unreachable!("sema rejects a value arm in statement position"),
                },
                ArmBody::Block(block) => self.block_stmts(block),
            }
            self.scopes.pop();
            exits.push(self.current);
        }

        let join = self.new_block(BlockKind::Join);

        // A single-variant enum has nothing to test, so control simply runs
        // into the one arm.
        if tests.is_empty() {
            self.finish(before, Terminator::jump(entries[0]));
        }
        for (index, (test, cond, arm)) in tests.into_iter().enumerate() {
            self.finish(
                test,
                Terminator::branch(cond, arm, entries[index + 1]),
            );
        }
        for exit in exits {
            self.finish(exit, Terminator::jump(join));
        }
        self.switch_to(join);
    }

    /// The tag a `Color::Red` expression stands for, which `sema` has already
    /// established exists.
    /// Bytes in front of a boxed enum.s payload, holding its tag, and the room
    /// one payload slot takes.
    ///
    /// The same word a string and a list spend on their length, and it sits in
    /// the same place: at the front, where the value points.
    pub(super) fn payload_at(&self, index: u32) -> u32 {
        let word = self.table.layout.word;
        self.table.layout.tag() + index * word
    }

    /// Build `Enum::Variant(...)` into `dst`.
    ///
    /// An enum whose variants all carry nothing *is* its tag, and costs exactly
    /// what an integer literal does — which is what every TinyC enum was until
    /// payloads existed, and what most still are.
    ///
    /// One that carries something is a **pointer** to its tag and payload in
    /// the arena, laid out like every other run of values here: the thing that
    /// tells the value apart in front, the values after it. It can be a pointer
    /// rather than something in the frame because an enum is read-only — there
    /// is no syntax that writes into a payload — so two names for one of them
    /// cannot be told apart. That is the same bargain a string strikes, and it
    /// is why an enum still fits in a register however much it carries.
    pub(super) fn variant_into(&mut self, dst: VReg, expr: &Expr, args: &[Expr]) {
        let ExprKind::Variant { variant, .. } = &expr.kind else {
            unreachable!("the caller matched a variant");
        };
        let Ty::Enum(id) = self.types.of(expr.id) else {
            unreachable!("sema gives a variant its enum's type");
        };
        let info = self.table.enum_info(id);
        let tag = info.tag(variant).expect("sema rejects an unknown variant");
        if !info.carries_data() {
            return self.emit(Instr::Const { dst, val: tag });
        }

        // A variant of a boxed enum that carries nothing is the same value
        // every time it is written, so it is written down once, in `.data`.
        // Nothing else would be gained by allocating a fresh eight bytes to
        // hold a number the compiler already knows.
        if args.is_empty() {
            return self.emit(Instr::VariantAddr { dst, id, tag: tag as u32 });
        }

        let slots = info.slots() as u32;
        let bytes = self.payload_at(slots);
        self.emit(Instr::RtCall {
            dst: Some(dst),
            callee: Runtime::Alloc,
            args: vec![Value::Const(i64::from(bytes))],
        });
        self.emit(Instr::Store { addr: Value::Reg(dst), value: Value::Const(tag) });
        for (index, arg) in args.iter().enumerate() {
            let at = self.fresh_temp();
            self.emit(Instr::Field {
                dst: at,
                base: Value::Reg(dst),
                offset: self.payload_at(index as u32),
            });
            // `Room::Fresh`, and through the same path a field takes: what goes
            // into a variant is the variant's from then on, so a list is copied
            // in rather than shared. There is no way to reach it again except
            // by matching, which copies back out.
            self.write_through(Value::Reg(at), arg, Room::Fresh);
        }
    }

    /// Whether a value of this type is a pointer to a tag rather than the tag.
    pub(super) fn is_boxed_enum(&self, ty: Ty) -> bool {
        matches!(ty, Ty::Enum(id) if self.table.enum_info(id).carries_data())
    }

    /// The tag of an enum value, whichever of the two shapes it has.
    ///
    /// For an enum that carries nothing anywhere the value *is* the tag and
    /// this is the identity — which is what keeps every such program emitting
    /// exactly the instructions it emitted before payloads existed.
    pub(super) fn tag_of(&mut self, ty: Ty, value: Value) -> Value {
        if !self.is_boxed_enum(ty) {
            return value;
        }
        let dst = self.fresh_temp();
        self.emit(Instr::Load { dst, addr: value });
        Value::Reg(dst)
    }

    /// Name what the matched variant carries, at the top of its arm.
    ///
    /// A list comes out as a copy, exactly as it went in. That is what makes an
    /// enum's payload the enum's: there is no way to reach the elements it
    /// holds except through a pattern, and a pattern hands back something of
    /// the arm's own.
    pub(super) fn bind_arm_payload(&mut self, id: EnumId, value: Value, arm: &MatchArm) {
        let Pattern::Variant { variant, bindings, .. } = &arm.pattern else { return };
        if bindings.is_empty() {
            return;
        }
        let info = self.table.enum_info(id);
        let payload = info.variant(variant).map(|v| v.payload.clone()).unwrap_or_default();
        for (index, (name, _)) in bindings.iter().enumerate() {
            let Some(&ty) = payload.get(index) else { break };
            let dst = self.declare(name, ty);
            let at = self.fresh_temp();
            self.emit(Instr::Field {
                dst: at,
                base: value,
                offset: self.payload_at(index as u32),
            });
            self.emit(Instr::Load { dst, addr: Value::Reg(at) });
            if let Ty::List(list) = ty {
                let elem = self.table.element(list);
                let bytes = self.table.size_of(elem);
                let deep = i64::from(self.table.holds_a_list(elem));
                self.emit(Instr::RtCall {
                    dst: Some(dst),
                    callee: Runtime::ListClone,
                    args: vec![
                        Value::Reg(dst),
                        Value::Const(i64::from(bytes)),
                        Value::Const(deep),
                    ],
                });
            }
        }
    }

    /// The tag of a variant that carries nothing, for the enums that are still
    /// a bare tag.
    pub(super) fn variant_tag(&self, expr: &Expr) -> i64 {
        let ExprKind::Variant { variant, .. } = &expr.kind else {
            unreachable!("the caller matched a variant");
        };
        let Ty::Enum(id) = self.types.of(expr.id) else {
            unreachable!("sema gives a variant its enum's type");
        };
        self.table.enum_info(id).tag(variant).expect("sema rejects an unknown variant")
    }

    /// Whether the scrutinee is what this arm's pattern selects, as a `bool`.
    ///
    /// One comparison for everything a register holds — a variant's tag, a
    /// number, a character, a `bool` — because in every one of those cases the
    /// pattern is a value settled while the program was compiled. A string is
    /// the exception, and the same exception `==` already is: comparing the
    /// addresses would answer a different question, so it costs a call.
    ///
    /// Every pattern here was checked by [`crate::sema`], which is what makes
    /// the lookups `expect`s rather than diagnostics.
    pub(super) fn arm_test(&mut self, scrutinee: &Expr, value: Value, arm: &MatchArm) -> Value {
        if let Pattern::Str(chars) = &arm.pattern {
            let id = self.intern(chars);
            let literal = self.fresh_temp();
            self.emit(Instr::StrAddr { dst: literal, id });
            let dst = self.fresh_temp();
            self.emit(Instr::RtCall {
                dst: Some(dst),
                callee: Runtime::StrEq,
                args: vec![value, Value::Reg(literal)],
            });
            return Value::Reg(dst);
        }
        let wanted = match &arm.pattern {
            Pattern::Variant { variant, .. } => {
                let Ty::Enum(id) = self.types.of(scrutinee.id) else {
                    unreachable!("sema rejects a variant pattern on anything but an enum");
                };
                self.table.enum_info(id).tag(variant).expect("sema rejects an unknown variant")
            }
            Pattern::Int(v) => *v,
            Pattern::Char(c) => i64::from(u32::from(*c)),
            Pattern::Bool(v) => i64::from(*v),
            Pattern::Str(_) => unreachable!("handled above"),
            // Matching is equality on a machine word, and equality on a float
            // is not that: `-0.0` and `0.0` are the same number written two
            // ways, and a NaN is equal to nothing at all. So `sema` refuses to
            // match on one rather than have this quietly mean something else.
            Pattern::Float(_) => unreachable!("sema rejects matching on a float"),
            // A catch-all is the last arm, and the last arm is the one control
            // simply runs into — so nothing ever asks it a question.
            Pattern::Wildcard => unreachable!("sema puts `_` last, where nothing is tested"),
        };
        let dst = self.fresh_temp();
        self.emit(Instr::Cmp {
            num: Num::Int,
            op: CmpOp::Eq,
            dst,
            lhs: value,
            rhs: Value::Const(wanted),
        });
        Value::Reg(dst)
    }

    /// Lower a `break` or a `continue`: hand the block it ends to the innermost
    /// loop, which will terminate it once it knows where the jump goes.
    ///
    /// `which` picks the list to join, and is the only difference between the
    /// two statements at this stage.
    pub(super) fn loop_jump(&mut self, which: impl FnOnce(&mut LoopFrame) -> &mut Vec<BlockId>) {
        let leaving = self.current;
        let frame = self.loops.last_mut().expect("sema rejects a loop jump outside a loop");
        which(frame).push(leaving);
        // As after a `return`: whatever was written next still has to be
        // lowered somewhere, and nothing reaches it.
        self.new_block(BlockKind::Unreachable);
    }

    pub(super) fn if_stmt(&mut self, cond: &Expr, then_block: &AstBlock, else_block: &Option<AstBlock>) {
        let cond = self.expr(cond);
        // The branch belongs to whichever block the condition was computed in.
        let entry = self.current;

        let then_id = self.new_block(BlockKind::Then);
        self.block_stmts(then_block);
        let then_exit = self.current;

        // `None` all the way through: with no `else`, there is no block to name
        // and no exit to send to the join — the branch goes straight there.
        let alternative = else_block.as_ref().map(|block| {
            let id = self.new_block(BlockKind::Else);
            self.block_stmts(block);
            (id, self.current)
        });

        let join = self.new_block(BlockKind::Join);

        self.finish(
            entry,
            Terminator::branch(cond, then_id, alternative.map_or(join, |(id, _)| id)),
        );

        self.finish(then_exit, Terminator::jump(join));
        if let Some((_, exit)) = alternative {
            self.finish(exit, Terminator::jump(join));
        }

        self.switch_to(join);
    }

    pub(super) fn while_stmt(&mut self, cond: &Expr, body: &AstBlock) {
        self.loop_with_step(cond, body, None);
    }

    /// The shape shared by `while` and `for`: a header that re-tests the
    /// condition on every iteration, a body, and an optional step run at the
    /// end of the body.
    pub(super) fn loop_with_step(&mut self, cond: &Expr, body: &AstBlock, step: Option<&Stmt>) {
        let before = self.current;

        // The condition must be re-evaluated each time round, so it gets a
        // block of its own that the body jumps back to.
        let header = self.new_block(BlockKind::Loop);
        let cond = self.expr(cond);
        let header_exit = self.current;

        let body_id = self.new_block(BlockKind::Body);
        self.loops.push(LoopFrame::default());
        self.block_stmts(body);
        let frame = self.loops.pop().expect("the frame pushed just above");

        // Where the back edge starts, and where a `continue` goes.
        //
        // A `for` has to run its step at the end of *every* iteration, the ones
        // a `continue` cuts short included — so when one exists the step needs a
        // block of its own to jump to. When none does the step simply ends the
        // body, and the `for` lowers to exactly the `while` it desugars into.
        let latch = match step {
            Some(step) if !frame.continues.is_empty() => {
                let body_exit = self.current;
                let latch = self.new_block(BlockKind::Step);
                self.stmt(step);
                self.finish(body_exit, Terminator::jump(latch));
                latch
            }
            Some(step) => {
                self.stmt(step);
                header
            }
            None => header,
        };
        // The step may itself open blocks — `i = i + 1` does not, but
        // `ok = ok && f()` would — so where it ended is not where it began.
        let latch_exit = self.current;

        let after = self.new_block(BlockKind::Done);

        self.finish(before, Terminator::jump(header));
        self.finish(header_exit, Terminator::branch(cond, body_id, after));
        // The back edge: this is what makes liveness need a fixpoint.
        self.finish(latch_exit, Terminator::jump(header));

        for block in frame.continues {
            self.finish(block, Terminator::jump(latch));
        }
        for block in frame.breaks {
            self.finish(block, Terminator::jump(after));
        }

        self.switch_to(after);
    }

}
