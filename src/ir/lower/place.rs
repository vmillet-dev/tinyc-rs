//! Lowering the things that live in memory: arrays, lists, objects and places.

use super::*;

impl Lowering<'_> {
    // -- arrays ------------------------------------------------------------

    /// The address of `array[index]`, in a fresh register.
    ///
    /// One `Elem` and nothing else. The multiply-and-add that turns an index
    /// into an offset is *addressing*, not arithmetic the program wrote, so it
    /// is not lowered as arithmetic and never picks up the overflow guard `Bin`
    /// carries — an index `sema` or the backend has already bounds-checked
    /// cannot put the address anywhere but inside the object.
    pub(super) fn element_address(&mut self, array: &Expr, index: &Expr) -> VReg {
        let ty = self.types.of(array.id);
        let base = self.expr(array);
        let (len, scale) = self.shape_of(ty, base);
        let index = self.expr(index);
        let dst = self.fresh_temp();
        self.emit(Instr::Elem { dst, base, index, len, scale });
        dst
    }

    /// How long the thing in `base` is and how wide each of its elements is.
    ///
    /// For an array both are facts about the *type*, known here with nothing
    /// computed. For a string the width still is, but the length is a load —
    /// which is the whole difference between the two, and the reason a constant
    /// index into a string is checked at run time like any other.
    pub(super) fn shape_of(&mut self, ty: Ty, base: Value) -> (Value, u32) {
        match ty {
            Ty::Array(id) => {
                let info = self.table.array(id);
                (Value::Const(i64::from(info.len)), self.table.size_of(info.elem))
            }
            Ty::Str => (self.length_of(base), CHAR_BYTES),
            // A list holds its elements where it is, so one of objects scales
            // by the whole object — the same arithmetic an array of them does,
            // with a length that has to be read rather than known.
            Ty::List(_) => {
                let (_, bytes) = self.element_of(ty);
                (self.length_of(base), bytes)
            }
            _ => unreachable!("sema rejects indexing anything without elements"),
        }
    }

    /// What one element of this list type is, and how many bytes it takes.
    ///
    /// The second half is what the routines have to be told: they walk the
    /// elements rather than reading one, so they cannot work it out.
    pub(super) fn element_of(&self, list: Ty) -> (Ty, u32) {
        let Ty::List(id) = list else {
            unreachable!("sema rejects a list operation on anything but a list");
        };
        let elem = self.table.element(id);
        (elem, self.table.size_of(elem))
    }

    /// The count in front of a string's characters or a list's elements.
    pub(super) fn length_of(&mut self, of: Value) -> Value {
        let dst = self.fresh_temp();
        self.emit(Instr::Count { dst, of });
        Value::Reg(dst)
    }

    /// Lower a value that is about to be **kept** — put in a variable, or
    /// handed back from a function.
    ///
    /// A list is one pointer, so lowering one straight into a variable would
    /// give two names to one run of elements. Every other type in the language
    /// either cannot be written to, so the sharing could not be observed, or is
    /// too big to fit in a register and is copied by the code above. A list is
    /// neither, so this is where "assignment copies, never aliases" is paid
    /// for.
    ///
    /// Something that built its own is nobody else's already, and moves in as
    /// it is — which is why a function that returns a list costs no copy at the
    /// call site: it cloned at its own `return`, if it had anything to clone.
    pub(super) fn keep_into(&mut self, dst: VReg, expr: &Expr) {
        self.expr_into(dst, expr);
        let ty = self.types.of(expr.id);
        if matches!(ty, Ty::List(_)) && !builds_its_own(expr) {
            let (elem, bytes) = self.element_of(ty);
            self.emit(Instr::RtCall {
                dst: Some(dst),
                callee: Runtime::ListClone,
                args: vec![
                    Value::Reg(dst),
                    Value::Const(i64::from(bytes)),
                    // The elements are copies too. If one of them holds a list
                    // of its own, copying its bytes shared that list, and the
                    // clone has to go one level further in.
                    Value::Const(i64::from(self.table.holds_a_list(elem))),
                ],
            });
        }
    }

    /// `s = s + a + b + …` taken apart into the pieces to add, in order, for a
    /// string `s` that [`owned_strings`] proved nothing else can be holding.
    ///
    /// The chain matters as much as the single step. `+` leans left, so
    /// `s = s + string(i) + ","` is `s = ((s + string(i)) + ",")` — its
    /// outermost operand is not the variable, and matching only `s = s + e`
    /// would leave the commonest way of building a line quadratic.
    ///
    /// The shape has to start at the variable. `s = "a" + s` is not it —
    /// prepending cannot grow a block where it stands, whatever is known about
    /// it — and neither is `s = t + e`, which is somebody else's string.
    ///
    /// **No piece may mention `s` itself.** Appending them one at a time makes
    /// the intermediate values visible where the single expression would have
    /// read the variable once, at the start; `s = s + f(s)` would hand `f` a
    /// string the original never would.
    pub(super) fn append_chain<'e>(&self, name: &str, value: &'e Expr) -> Option<Vec<&'e Expr>> {
        if !self.owned.contains(name) {
            return None;
        }
        let mut pieces = Vec::new();
        let mut at = value;
        while let ExprKind::Bin { op: BinOp::Add, lhs, rhs } = &at.kind {
            if self.types.of(lhs.id) != Ty::Str {
                return None;
            }
            pieces.push(&**rhs);
            at = lhs;
        }
        let ExprKind::Var(left) = &at.kind else { return None };
        if left != name || pieces.is_empty() {
            return None;
        }
        if pieces.iter().any(|piece| mentions(piece, name)) {
            return None;
        }
        pieces.reverse();
        Some(pieces)
    }

    /// Whether this expression allocates the string it produces *here*, so that
    /// what it produced is this statement's own and nobody else's.
    ///
    /// The narrow question the arena needs in order to hand a block back: a
    /// temporary built and consumed inside one statement is the one thing the
    /// bump pointer can safely retract. A literal is deliberately not one — it
    /// lives in `.data` and there is nothing to retract — and neither is a
    /// variable, a call, an element or a field, all of which hand on a string
    /// that already had a name.
    pub(super) fn builds_a_temporary(&self, expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::Bin { op: BinOp::Add, lhs, .. } => self.types.of(lhs.id) == Ty::Str,
            ExprKind::Convert { to: Prim::Str, .. } => true,
            ExprKind::Call { name, args, .. } => {
                name == Builtin::ReadLine.name() && args.is_empty()
            }
            _ => false,
        }
    }

    /// Whether this is one of the operators a string gives a second meaning to,
    /// and so one that becomes a call rather than an instruction.
    pub(super) fn is_string_op(&self, expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::Bin { op: BinOp::Add, lhs, .. } | ExprKind::Cmp { lhs, .. } => {
                self.types.of(lhs.id) == Ty::Str
            }
            _ => false,
        }
    }

    /// Lower `a + b` or `a == b` on two strings into `dst`.
    pub(super) fn string_op_into(&mut self, dst: VReg, expr: &Expr) {
        let (op, lhs, rhs) = match &expr.kind {
            ExprKind::Bin { lhs, rhs, .. } => (None, lhs, rhs),
            ExprKind::Cmp { op, lhs, rhs } => (Some(*op), lhs, rhs),
            _ => unreachable!("the caller checked this is a string operator"),
        };
        let args = vec![self.expr(lhs), self.expr(rhs)];

        let Some(op) = op else {
            self.emit(Instr::RtCall { dst: Some(dst), callee: Runtime::Concat, args });
            return;
        };

        // The routine answers whether the two are the same, and `!=` is that
        // question read the other way round — so one routine serves both, and
        // the negation costs the comparison against zero that `!` already is.
        if op == CmpOp::Eq {
            self.emit(Instr::RtCall { dst: Some(dst), callee: Runtime::StrEq, args });
            return;
        }
        let same = self.fresh_temp();
        self.emit(Instr::RtCall { dst: Some(same), callee: Runtime::StrEq, args });
        self.emit(Instr::Cmp {
            num: Num::Int,
            op: CmpOp::Eq,
            dst,
            lhs: Value::Reg(same),
            rhs: Value::Const(0),
        });
    }

    /// Lower `int(c)` or `char(n)` into `dst`.
    ///
    /// One direction is free and the other is not, and the asymmetry is the
    /// design: every character has a code point, but not every number names a
    /// character. So only that direction can fail — and it fails where it was
    /// written, rather than handing on a value nothing else in the language
    /// could have produced.
    pub(super) fn convert_into(&mut self, dst: VReg, to: Prim, value: &Expr) {
        let from = self.types.of(value.id);
        let src = self.expr(value);
        // A constant `sema` has already accepted needs no check at run time,
        // which is the same bargain a constant index strikes.
        let settled = matches!(src, Value::Const(c) if is_scalar_value(c));

        // Between `int` and `float` the word itself changes, so this is the one
        // conversion that is neither a routine nor a move. A constant is
        // settled here for the same reason a constant character is: what is
        // left to check at run time is only ever a value the running program
        // alone knows.
        let cast = match (from, to) {
            (Ty::Int, Prim::Float) => Some(Num::Float),
            (Ty::Float, Prim::Int) => Some(Num::Int),
            _ => None,
        };
        if let Some(to) = cast {
            match (to, src) {
                (Num::Float, Value::Const(c)) => {
                    self.emit(Instr::Const { dst, val: (c as f64).to_bits() as i64 })
                }
                (Num::Int, Value::Const(c)) if fits_in_an_int(f64::from_bits(c as u64)) => {
                    self.emit(Instr::Const { dst, val: f64::from_bits(c as u64) as i64 })
                }
                _ => self.emit(Instr::Cast { dst, to, src }),
            }
            return;
        }

        let callee = match (from, to) {
            (Ty::Int, Prim::Char) if !settled => Runtime::CheckChar,
            (Ty::Str, Prim::Int) => Runtime::StrToInt,
            (Ty::Char, Prim::Str) => Runtime::CharToStr,
            (Ty::Int, Prim::Str) => Runtime::IntToStr,
            (Ty::List(_), Prim::Str) => Runtime::CharsToStr,
            // A code point *is* the character's representation, so reading one
            // as the other moves nothing at all.
            _ => return self.emit(Instr::Copy { dst, src }),
        };
        self.emit(Instr::RtCall { dst: Some(dst), callee, args: vec![src] });
    }

    /// Room in the frame for a value of `ty`, with its address in `dst`.
    pub(super) fn allocate_for(&mut self, dst: VReg, ty: Ty) {
        let bytes = self.table.size_of(ty);
        self.allocate(dst, bytes);
    }

    /// Put `value` where `addr` points, whichever kind of value it is.
    ///
    /// A scalar is one store. An aggregate does not fit in a register, so what
    /// the expression produced is an *address* and the value is copied out of
    /// it — which is what makes assignment value semantics rather than
    /// aliasing, and what carries an object's vtable pointer along with it.
    pub(super) fn write_through(&mut self, addr: Value, value: &Expr, room: Room) {
        let ty = self.types.of(value.id);
        if ty.fits_in_a_register() {
            // A list fits, and is the one thing that fits and can still be
            // written to — so storing what the expression produced would give
            // this room a second name for somebody else's elements. The same
            // reason `keep_into` exists, one indirection further along, and
            // what makes a list *field* copy like every other field.
            if matches!(ty, Ty::List(_)) {
                let held = self.fresh_temp();
                self.keep_into(held, value);
                self.emit(Instr::Store { addr, value: Value::Reg(held) });
                return;
            }
            let value = self.expr(value);
            self.emit(Instr::Store { addr, value });
            return;
        }
        // A literal has no room of its own until something gives it some, so
        // where the room here is new it may as well be this room — see [`Room`]
        // for why "new" is the condition and not merely "aggregate".
        if matches!(room, Room::Fresh) {
            match &value.kind {
                ExprKind::New { fields, .. } => {
                    let Ty::Class(id) = ty else {
                        unreachable!("sema gives an object literal its class's type");
                    };
                    return self.fill_object(addr, id, fields);
                }
                // A *list* literal is not one of these: its elements live in the
                // arena, so what it produces is an address rather than room, and
                // it never reaches here — a list fits in a register.
                ExprKind::Array { elements, .. } => return self.fill_array(addr, ty, elements),
                _ => {}
            }
        }
        let src = self.expr(value);
        let bytes = self.table.size_of(ty);
        self.emit(Instr::CopyBytes { dst: addr, src, bytes });
        self.fixup_after_copy(addr, ty);
    }

    /// Give a fresh copy its own elements, where what was copied may hold a
    /// list.
    ///
    /// Emitted only where [`TypeTable::holds_a_list`] says it can be needed, so
    /// a program whose classes hold nothing but numbers and objects carries
    /// none of this — not the instruction, not the routines, not the word in
    /// front of its vtables.
    pub(super) fn fixup_after_copy(&mut self, at: Value, ty: Ty) {
        if !self.table.holds_a_list(ty) {
            return;
        }
        // An array was copied whole, so every element of it is a fresh copy.
        // Anything else is one value.
        let (count, stride) = match ty {
            Ty::Array(id) => {
                let info = self.table.array(id);
                (i64::from(info.len), self.table.size_of(info.elem))
            }
            _ => (1, 0),
        };
        self.emit(Instr::Fixup { at, count: Value::Const(count), stride });
    }

    /// Put a class's vtable pointer and every field where `at` points.
    ///
    /// The vtable pointer goes in first, at offset 0. It is what makes the
    /// object *this* class rather than merely its shape, and it is what travels
    /// with a copy — so the object is a complete one of its class from the
    /// first instruction.
    pub(super) fn fill_object(&mut self, at: Value, id: ClassId, fields: &[FieldInit]) {
        let info = self.table.class(id).clone();
        let vptr = self.fresh_temp();
        self.emit(Instr::VTable { dst: vptr, class: id });
        self.emit(Instr::Store { addr: at, value: Value::Reg(vptr) });

        for init in fields {
            let offset =
                info.field(&init.name).expect("sema rejects an unknown field").offset;
            let addr = self.fresh_temp();
            self.emit(Instr::Field { dst: addr, base: at, offset });
            // A field of an object being built is room nothing can name, so
            // whatever fills it may be built there directly.
            self.write_through(Value::Reg(addr), &init.value, Room::Fresh);
        }
    }

    /// Put every element of an array or list literal where `at` points.
    ///
    /// The room is filled in written order. An element's value may mention the
    /// array being built only in ways `sema` has already ruled out, so the order
    /// is not observable.
    pub(super) fn fill_array(&mut self, at: Value, ty: Ty, elements: &[Expr]) {
        let (len, scale) = self.shape_of(ty, at);
        for (index, element) in elements.iter().enumerate() {
            let addr = self.fresh_temp();
            self.emit(Instr::Elem {
                dst: addr,
                base: at,
                index: Value::Const(index as i64),
                len,
                scale,
            });
            self.write_through(Value::Reg(addr), element, Room::Fresh);
        }
    }

    /// The address of `object.field`, which is the object's plus a fixed offset.
    ///
    /// The same `Elem` an array index uses, with the offset in place of the
    /// index — a field's place in an object is settled once, by `sema`, so
    /// there is nothing to check and nothing to compute.
    pub(super) fn field_address(&mut self, object: Value, class: ClassId, name: &str) -> VReg {
        let offset = self
            .table
            .class(class)
            .field(name)
            .expect("sema rejects an unknown field")
            .offset;
        let dst = self.fresh_temp();
        self.emit(Instr::Field { dst, base: object, offset });
        dst
    }

    /// Lower `receiver.method(args)`.
    ///
    /// The receiver is the first argument, and also where the vtable comes
    /// from. Which of the two calls goes out is settled here: a class nothing
    /// derives from has one possible implementation, so the indirection would
    /// decide a question with one answer — whole-program compilation is what
    /// makes that knowable.
    pub(super) fn method_call(&mut self, dst: Option<VReg>, expr: &Expr) {
        let ExprKind::MethodCall { receiver, name, args, .. } = &expr.kind else {
            unreachable!("the caller matched a method call");
        };
        let Ty::Class(id) = self.types.of(receiver.id) else {
            unreachable!("sema rejects a method call on anything but an object");
        };

        let method = self.table.class(id).method(name).expect("sema rejects an unknown method");
        let (slot, function) = (method.slot as u32, method.function);
        let sealed = self.table.is_sealed(id);

        // The receiver is the first written argument, so it is evaluated before
        // the rest.
        let object = self.expr(receiver);
        let mut values = vec![object];
        values.extend(args.iter().map(|arg| self.expr(arg)));

        // An aggregate answer goes in room the caller reserves, whose address
        // leads the arguments — ahead of the receiver, since the receiver is
        // an ordinary argument and this is not.
        let ty = self.types.of(expr.id);
        let dst = match (dst, ty.fits_in_a_register()) {
            (Some(dst), false) => {
                self.allocate_for(dst, ty);
                values.insert(0, Value::Reg(dst));
                None
            }
            (dst, _) => dst,
        };

        if sealed {
            self.emit(Instr::Call { dst, callee: self.func_ids[function], args: values });
        } else {
            self.emit(Instr::CallVirtual { dst, slot, receiver: object, args: values });
        }
    }

    /// The type a place has, read off the chain of names that leads to it.
    pub(super) fn place_type(&self, place: &Place) -> Ty {
        match place {
            Place::Var { name, .. } => self.lookup_type(name),
            Place::Element { base, .. } => match self.place_type(base) {
                Ty::Array(id) => self.table.array(id).elem,
                // Reached once a list can hold objects: `xs[i].f` asks what
                // `xs[i]` is before it can ask where its field is.
                Ty::List(id) => self.table.element(id),
                _ => unreachable!("sema rejects indexing anything but an array or a list"),
            },
            Place::Field { base, name, .. } => match self.place_type(base) {
                Ty::Class(id) => {
                    self.table.class(id).field(name).expect("sema rejects an unknown field").ty
                }
                _ => unreachable!("sema rejects a field on anything but an object"),
            },
        }
    }

    /// The address a place names, for a write.
    ///
    /// Only a variable has no address — it is a register — and the caller has
    /// already dealt with that case.
    pub(super) fn place_address(&mut self, place: &Place) -> VReg {
        match place {
            Place::Var { .. } => unreachable!("a variable is a register, not an address"),
            Place::Element { base, index, .. } => {
                let ty = self.place_type(base);
                let object = self.place_value(base);
                let (len, scale) = self.shape_of(ty, object);
                let index = self.expr(index);
                let dst = self.fresh_temp();
                self.emit(Instr::Elem { dst, base: object, index, len, scale });
                dst
            }
            Place::Field { base, name, .. } => {
                let Ty::Class(id) = self.place_type(base) else {
                    unreachable!("sema rejects a field on anything but an object");
                };
                let object = self.place_value(base);
                self.field_address(object, id, name)
            }
        }
    }

    /// What a place *holds*, which for anything but a variable means reading it.
    ///
    /// An aggregate is the exception, and it is the same exception an element
    /// and a field make when they are read as expressions: what a place of that
    /// type holds does not fit in a register, so its *address* is the value.
    /// Reading eight bytes out of it would produce an object's vtable pointer
    /// rather than the object, which is the address of nothing at all.
    pub(super) fn place_value(&mut self, place: &Place) -> Value {
        match place {
            Place::Var { name, .. } => Value::Reg(self.lookup(name)),
            other => {
                let addr = self.place_address(other);
                if !self.place_type(other).fits_in_a_register() {
                    return Value::Reg(addr);
                }
                let dst = self.fresh_temp();
                self.emit(Instr::Load { dst, addr: Value::Reg(addr) });
                Value::Reg(dst)
            }
        }
    }

    /// Reserve `bytes` of frame and put their address in `dst`.
    ///
    /// Saturating, like the object layout it is the sum of: a function with
    /// four gigabytes of locals is answered by [`too_much_stack`], not by a
    /// total that wrapped and looked reasonable.
    pub(super) fn allocate(&mut self, dst: VReg, bytes: u32) {
        let offset = self.frame_bytes;
        self.frame_bytes = self.frame_bytes.saturating_add(bytes);
        self.frame_peak = self.frame_peak.max(self.frame_bytes);
        self.emit(Instr::Frame { dst, offset });
    }

}
