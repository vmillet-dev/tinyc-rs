//! Lowering expressions to a value in a virtual register.

use super::*;

impl Lowering<'_> {
    // -- expressions -------------------------------------------------------

    /// The callee and evaluated arguments of a call expression.
    pub(super) fn call_parts(&mut self, call: &Expr) -> (FuncId, Vec<Value>) {
        let ExprKind::Call { name, args, .. } = &call.kind else {
            unreachable!("sema guarantees this is a call");
        };
        // Arguments are evaluated left to right, before the call itself; a
        // nested call therefore finishes first and leaves its result in a
        // temporary that the outer call reads as an operand.
        let args: Vec<Value> = args.iter().map(|arg| self.expr(arg)).collect();
        let callee = self.ids[name.as_str()];
        (callee, args)
    }

    /// Lower an expression whose result must land in `dst`.
    pub(super) fn expr_into(&mut self, dst: VReg, expr: &Expr) {
        match &expr.kind {
            ExprKind::Int(v) => self.emit(Instr::Const { dst, val: *v }),
            ExprKind::Float(v) => self.emit(Instr::Const { dst, val: v.to_bits() as i64 }),
            ExprKind::Bool(v) => self.emit(Instr::Const { dst, val: i64::from(*v) }),
            ExprKind::Variant { args, .. } => self.variant_into(dst, expr, args),
            // The array's room is reserved first, then filled: an element's
            // value may itself mention the array being built only in ways sema
            // has already ruled out, so the order is not observable.
            // The object's room is reserved at its hierarchy's size, then its
            // vtable pointer goes in at offset 0 and its fields after — so it
            // is a complete object of its class from the first instruction.
            ExprKind::New { fields, .. } => {
                let Ty::Class(id) = self.types.of(expr.id) else {
                    unreachable!("sema gives an object literal its class's type");
                };
                self.allocate(dst, self.table.class(id).storage);
                self.fill_object(Value::Reg(dst), id, fields);
            }
            ExprKind::Field { object, name, .. } => {
                let Ty::Class(id) = self.types.of(object.id) else {
                    unreachable!("sema rejects a field on anything but an object");
                };
                let object = self.expr(object);
                let addr = Value::Reg(self.field_address(object, id, name));
                // A field that is itself an aggregate *is* its address, exactly
                // as an element of one is: it lives inside the object, so there
                // is nothing to read out and nothing to copy until somebody
                // says where to.
                match self.types.of(expr.id).fits_in_a_register() {
                    true => self.emit(Instr::Load { dst, addr }),
                    false => self.emit(Instr::Copy { dst, src: addr }),
                }
            }
            ExprKind::MethodCall { .. } => self.method_call(Some(dst), expr),
            // A list literal reserves nothing in the frame: its elements live
            // in the arena, because how many there will be by the end is not a
            // question this function can answer.
            ExprKind::Array { elements, .. } if matches!(self.types.of(expr.id), Ty::List(_)) => {
                let (_, bytes) = self.element_of(self.types.of(expr.id));
                let len = Value::Const(elements.len() as i64);
                self.emit(Instr::RtCall {
                    dst: Some(dst),
                    callee: Runtime::ListNew,
                    args: vec![len, Value::Const(i64::from(bytes))],
                });
                for (index, element) in elements.iter().enumerate() {
                    let addr = self.fresh_temp();
                    self.emit(Instr::Elem {
                        dst: addr,
                        base: Value::Reg(dst),
                        index: Value::Const(index as i64),
                        len,
                        scale: bytes,
                    });
                    self.write_through(Value::Reg(addr), element, Room::Fresh);
                }
            }
            ExprKind::Array { elements, .. } => {
                let ty = self.types.of(expr.id);
                self.allocate_for(dst, ty);
                self.fill_array(Value::Reg(dst), ty, elements);
            }
            ExprKind::Index { array, index, .. } => {
                let of = self.types.of(array.id);
                let addr = self.element_address(array, index);
                let addr = Value::Reg(addr);
                // An aggregate element *is* its address; only a value that
                // fits in a register has to be read out — and a string's
                // characters are the one thing narrower than a register.
                match (of, self.types.of(expr.id).fits_in_a_register()) {
                    (Ty::Str, _) => self.emit(Instr::LoadChar { dst, addr }),
                    (_, true) => self.emit(Instr::Load { dst, addr }),
                    (_, false) => self.emit(Instr::Copy { dst, src: addr }),
                }
            }
            ExprKind::Len { .. } => {
                let src = self.expr(expr);
                self.emit(Instr::Copy { dst, src });
            }
            ExprKind::Char(c) => self.emit(Instr::Const { dst, val: i64::from(*c as u32) }),
            ExprKind::Convert { to, value, .. } => self.convert_into(dst, *to, value),
            ExprKind::Str(bytes) => {
                let id = self.intern(bytes);
                self.emit(Instr::StrAddr { dst, id });
            }
            // `+` on two strings, and `==` on two strings, are the only
            // operators that are a *loop* rather than an instruction, so they
            // are the only ones that leave through a call.
            ExprKind::Bin { .. } | ExprKind::Cmp { .. } if self.is_string_op(expr) => {
                self.string_op_into(dst, expr)
            }
            ExprKind::Bin { op, lhs, rhs } => {
                let num = self.num_of(expr);
                let lhs = self.expr(lhs);
                let rhs = self.expr(rhs);
                match fold_bin(num, *op, lhs, rhs) {
                    Some(val) => self.emit(Instr::Const { dst, val }),
                    None => self.emit(Instr::Bin { num, op: *op, dst, lhs, rhs }),
                }
            }
            // A comparison answers a bool whatever it compared, so what says
            // how to read the operands is an *operand's* type and not this
            // expression's.
            ExprKind::Cmp { op, lhs, rhs } => {
                let num = self.num_of(lhs);
                let lhs = self.expr(lhs);
                let rhs = self.expr(rhs);
                match fold_cmp(num, *op, lhs, rhs) {
                    Some(val) => self.emit(Instr::Const { dst, val }),
                    None => self.emit(Instr::Cmp { num, op: *op, dst, lhs, rhs }),
                }
            }
            ExprKind::Neg(operand) => {
                let num = self.num_of(expr);
                let val = self.expr(operand);
                match val {
                    Value::Const(c) => self.emit(Instr::Const { dst, val: negate_const(num, c) }),
                    val => self.emit(Instr::Bin {
                        num,
                        op: BinOp::Sub,
                        dst,
                        lhs: zero_to_subtract_from(num),
                        rhs: val,
                    }),
                }
            }
            ExprKind::Not(operand) => {
                let (num, op, lhs, rhs) = self.negated(operand);
                match fold_cmp(num, op, lhs, rhs) {
                    Some(val) => self.emit(Instr::Const { dst, val }),
                    None => self.emit(Instr::Cmp { num, op, dst, lhs, rhs }),
                }
            }
            ExprKind::Logic { op, lhs, rhs } => self.logic_into(dst, *op, lhs, rhs),
            ExprKind::Match { .. } => self.match_lowering(Some(dst), expr),
            // A built-in is a call with no body to compile, so it goes out as
            // the routine it is. Nothing else about the call site differs.
            ExprKind::Call { name, args, .. } if Builtin::from_name(name).is_some() => {
                let args: Vec<Value> = args.iter().map(|arg| self.expr(arg)).collect();
                let callee = Runtime::of(Builtin::from_name(name).expect("just matched"));
                self.emit(Instr::RtCall { dst: Some(dst), callee, args });
            }
            ExprKind::Call { .. } => {
                let (callee, mut args) = self.call_parts(expr);
                let ty = self.types.of(expr.id);
                if ty.fits_in_a_register() {
                    self.emit(Instr::Call { dst: Some(dst), callee, args });
                    return;
                }
                // The room is the caller's, and its address goes in ahead of
                // the written arguments. `dst` ends up naming it, so what the
                // caller gets back is room it already owned.
                self.allocate_for(dst, ty);
                args.insert(0, Value::Reg(dst));
                self.emit(Instr::Call { dst: None, callee, args });
            }
            ExprKind::Var(_) => {
                let src = self.expr(expr);
                self.emit(Instr::Copy { dst, src });
            }
        }
    }

    /// Lower `lhs && rhs` or `lhs || rhs` into `dst`.
    pub(super) fn logic_into(&mut self, dst: VReg, op: LogicOp, lhs: &Expr, rhs: &Expr) {
        let cond = self.expr(lhs);
        if matches!(cond, Value::Const(_)) {
            // A known left operand settles the expression at compile time.
            // Dropping the right one is not just an optimisation here: with
            // `false && f()`, *not* calling `f` is the semantics.
            match fold_logic(op, cond) {
                Some(val) => self.emit(Instr::Const { dst, val }),
                // It decided nothing: `true && e` is simply `e`.
                None => self.expr_into(dst, rhs),
            }
            return;
        }
        self.logic_branch(dst, op, cond, rhs);
    }

    /// The half of `&&` and `||` that really branches, given a left operand
    /// whose value is not known.
    ///
    /// There is no `and` or `or` instruction, and there could not usefully be
    /// one: short circuiting *is* control flow, so this produces the same
    /// diamond an `if` does. Both arms write `dst`, which is only expressible
    /// because the IR is not in SSA form — see the module comment.
    pub(super) fn logic_branch(&mut self, dst: VReg, op: LogicOp, cond: Value, rhs: &Expr) {
        // The value the left operand hands back when it decides on its own.
        let settled = op.short_circuit();
        // The branch belongs to whichever block the left operand ended in.
        let entry = self.current;

        // The arm the branch continues into is laid out first, so the backend
        // reaches it by falling through instead of by jumping.
        let (then_blk, else_blk, rhs_exit, short) = match op {
            LogicOp::And => {
                let (rhs_blk, rhs_exit) = self.logic_rhs(dst, rhs);
                let short = self.logic_short(dst, settled);
                (rhs_blk, short, rhs_exit, short)
            }
            LogicOp::Or => {
                let short = self.logic_short(dst, settled);
                let (rhs_blk, rhs_exit) = self.logic_rhs(dst, rhs);
                (short, rhs_blk, rhs_exit, short)
            }
        };

        let join = self.new_block(BlockKind::Join);
        self.finish(entry, Terminator::Branch { cond, then_blk, else_blk });
        self.finish(rhs_exit, Terminator::Jump(join));
        self.finish(short, Terminator::Jump(join));
        self.switch_to(join);
    }

    /// The arm that evaluates the right operand, as `(entry, exit)`: lowering it
    /// can open blocks of its own, so where it ends is not where it began.
    pub(super) fn logic_rhs(&mut self, dst: VReg, rhs: &Expr) -> (BlockId, BlockId) {
        let id = self.new_block(BlockKind::Rhs);
        self.expr_into(dst, rhs);
        (id, self.current)
    }

    /// The arm the short circuit takes, holding nothing but the answer the left
    /// operand already gave.
    pub(super) fn logic_short(&mut self, dst: VReg, val: i64) -> BlockId {
        let id = self.new_block(BlockKind::Short);
        self.emit(Instr::Const { dst, val });
        id
    }

    /// The comparison that computes `!operand`, as `(op, lhs, rhs)`.
    ///
    /// There is no `not` instruction, and none is wanted: `!x` *is* `x == 0`,
    /// which folds and fuses into a branch like any other comparison. When the
    /// operand is itself a comparison the negation goes one better and inverts
    /// it in place, so `!(a < b)` costs the single `cmp` that `a >= b` does
    /// instead of a comparison followed by a comparison against its result.
    pub(super) fn negated(&mut self, operand: &Expr) -> (Num, CmpOp, Value, Value) {
        if let ExprKind::Cmp { op, lhs, rhs } = &operand.kind {
            let num = self.num_of(lhs);
            let lhs = self.expr(lhs);
            let rhs = self.expr(rhs);
            return (num, op.negate(), lhs, rhs);
        }
        // Anything else `!` can be applied to is a bool, and `!b` is `b == 0`.
        (Num::Int, CmpOp::Eq, self.expr(operand), Value::Const(0))
    }

    /// How an expression's value is read, which is a question about its type.
    pub(super) fn num_of(&self, expr: &Expr) -> Num {
        Num::of(self.types.of(expr.id))
    }

    /// Lower an expression used as an operand, producing a value to read.
    pub(super) fn expr(&mut self, expr: &Expr) -> Value {
        match &expr.kind {
            // Literals stay immediates so the backend can fold them into the
            // instruction that consumes them.
            ExprKind::Int(v) => Value::Const(*v),
            // The bits of the double, which is what a `float` is everywhere
            // past this point — see [`Num`].
            ExprKind::Float(v) => Value::Const(v.to_bits() as i64),
            ExprKind::Bool(v) => Value::Const(i64::from(*v)),
            ExprKind::Var(name) => Value::Reg(self.lookup(name)),
            // A variant of an enum that carries nothing anywhere *is* its tag,
            // so it needs no more machinery than an integer literal does —
            // which is the whole reason such an enum costs the backend nothing.
            // One that carries something has to be built, and building takes a
            // register to build into.
            ExprKind::Variant { .. } if !self.is_boxed_enum(self.types.of(expr.id)) => {
                Value::Const(self.variant_tag(expr))
            }
            ExprKind::Variant { args, .. } => {
                let dst = self.fresh_temp();
                self.variant_into(dst, expr, args);
                Value::Reg(dst)
            }
            // A length is a fact about a type, so it is a constant here and
            // costs nothing at all — `i < len(xs)` compares against a literal.
            ExprKind::Char(c) => Value::Const(i64::from(*c as u32)),
            ExprKind::Len { array, .. } => match self.types.of(array.id) {
                // An array's length is a fact about its type, so it costs
                // nothing at all — `i < len(xs)` compares against a literal.
                // A string's is a load, because a string that had to be built
                // could not have told the compiler how long it would be.
                Ty::Array(id) => Value::Const(i64::from(self.table.array(id).len)),
                _ => {
                    let str = self.expr(array);
                    self.length_of(str)
                }
            },
            ExprKind::Index { array, index, .. } => {
                let of = self.types.of(array.id);
                let addr = self.element_address(array, index);
                if of != Ty::Str && !self.types.of(expr.id).fits_in_a_register() {
                    return Value::Reg(addr);
                }
                let dst = self.fresh_temp();
                let addr = Value::Reg(addr);
                match of {
                    Ty::Str => self.emit(Instr::LoadChar { dst, addr }),
                    _ => self.emit(Instr::Load { dst, addr }),
                }
                Value::Reg(dst)
            }
            ExprKind::Neg(operand) => {
                let num = self.num_of(expr);
                match self.expr(operand) {
                    // An operand that is already a literal folds, and so does
                    // the whole tree above it: `-(2 * 3)` never reaches an
                    // instruction.
                    Value::Const(c) => Value::Const(negate_const(num, c)),
                    val => {
                        let dst = self.fresh_temp();
                        self.emit(Instr::Bin {
                            num,
                            op: BinOp::Sub,
                            dst,
                            lhs: zero_to_subtract_from(num),
                            rhs: val,
                        });
                        Value::Reg(dst)
                    }
                }
            }
            ExprKind::Bin { .. } | ExprKind::Cmp { .. } if self.is_string_op(expr) => {
                let dst = self.fresh_temp();
                self.string_op_into(dst, expr);
                Value::Reg(dst)
            }
            ExprKind::Bin { op, lhs, rhs } => {
                let num = self.num_of(expr);
                let lhs = self.expr(lhs);
                let rhs = self.expr(rhs);
                if let Some(val) = fold_bin(num, *op, lhs, rhs) {
                    return Value::Const(val);
                }
                let dst = self.fresh_temp();
                self.emit(Instr::Bin { num, op: *op, dst, lhs, rhs });
                Value::Reg(dst)
            }
            ExprKind::Cmp { op, lhs, rhs } => {
                let num = self.num_of(lhs);
                let lhs = self.expr(lhs);
                let rhs = self.expr(rhs);
                if let Some(val) = fold_cmp(num, *op, lhs, rhs) {
                    return Value::Const(val);
                }
                let dst = self.fresh_temp();
                self.emit(Instr::Cmp { num, op: *op, dst, lhs, rhs });
                Value::Reg(dst)
            }
            // `!` is a comparison, so it takes the same path as one.
            ExprKind::Not(operand) => {
                let (num, op, lhs, rhs) = self.negated(operand);
                if let Some(val) = fold_cmp(num, op, lhs, rhs) {
                    return Value::Const(val);
                }
                let dst = self.fresh_temp();
                self.emit(Instr::Cmp { num, op, dst, lhs, rhs });
                Value::Reg(dst)
            }
            // A left operand that settles the answer leaves nothing to branch
            // on, and so nothing to hold in a register either.
            ExprKind::Logic { op, lhs, rhs } => {
                let cond = self.expr(lhs);
                if matches!(cond, Value::Const(_)) {
                    return match fold_logic(*op, cond) {
                        Some(val) => Value::Const(val),
                        None => self.expr(rhs),
                    };
                }
                let dst = self.fresh_temp();
                self.logic_branch(dst, *op, cond, rhs);
                Value::Reg(dst)
            }
            ExprKind::Str(_)
            | ExprKind::Convert { .. }
            | ExprKind::Call { .. }
            | ExprKind::Match { .. }
            | ExprKind::Array { .. }
            | ExprKind::New { .. }
            | ExprKind::Field { .. }
            | ExprKind::MethodCall { .. } => {
                let dst = self.fresh_temp();
                self.expr_into(dst, expr);
                Value::Reg(dst)
            }
        }
    }
}
