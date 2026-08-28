//! Rendering the IR as text, for `--emit ir` and for the comments the backend echoes.

use crate::ast::{BinOp, Prim};
use super::{Function, Instr, Num, Program, Terminator, Value};

impl Program {
    /// Render the IR for `--emit ir`.
    pub fn dump(&self) -> String {
        let mut out = String::new();
        for (i, s) in self.strings.iter().enumerate() {
            out.push_str(&format!("str{i} = {:?}
", s.iter().collect::<String>()));
        }
        if !self.strings.is_empty() {
            out.push('\n');
        }

        for function in &self.functions {
            out.push_str(&format!("{}:\n", function.signature(&self.table)));

            // Numbering restarts per function, matching the indices the
            // allocator works with.
            let mut index = 0;
            for block in &function.blocks {
                out.push_str(&format!("{}:\n", block.label()));
                for instr in &block.instrs {
                    let text = self.instr_text(function, instr);
                    out.push_str(&format!("{index:>3}  {text}\n"));
                    index += 1;
                }

                let text = match &block.term {
                    Terminator::Jump(target) => {
                        format!("jump {}", function.block(*target).label())
                    }
                    Terminator::Branch { cond, then_blk, else_blk } => format!(
                        "branch {} ? {} : {}",
                        function.value_name(cond),
                        function.block(*then_blk).label(),
                        function.block(*else_blk).label()
                    ),
                    Terminator::Return(None) => "return".to_string(),
                    Terminator::Return(Some(value)) => {
                        format!("return {}", function.value_name(value))
                    }
                };
                out.push_str(&format!("{index:>3}  {text}\n"));
                index += 1;
            }
            out.push('\n');
        }
        out
    }

    /// One instruction, in the form `--emit ir` prints it. The backend echoes
    /// these as comments, so a reader can line the assembly up against the IR
    /// dump instruction by instruction.
    pub fn instr_text(&self, function: &Function, instr: &Instr) -> String {
        let value = |v: &Value| function.value_name(v);
        match instr {
            Instr::Const { dst, val } => format!("%{} = const {val}", function.name_of(*dst)),
            Instr::StrAddr { dst, id } => {
                format!("%{} = straddr str{}", function.name_of(*dst), id.0)
            }
            Instr::Copy { dst, src } => {
                format!("%{} = copy {}", function.name_of(*dst), value(src))
            }
            Instr::Bin { num, op, dst, lhs, rhs } => format!(
                "%{} = {}{} {}, {}",
                function.name_of(*dst),
                op_name(*op),
                num.suffix(),
                function.value_name_as(*num, lhs),
                function.value_name_as(*num, rhs)
            ),
            Instr::Cmp { num, op, dst, lhs, rhs } => format!(
                "%{} = cmp{} {} {}, {}",
                function.name_of(*dst),
                num.suffix(),
                op.symbol(),
                function.value_name_as(*num, lhs),
                function.value_name_as(*num, rhs)
            ),
            // Named as the conversion the program wrote — `int(f)`, `float(n)`
            // — rather than with a word of the dump's own, and the operand is
            // read the *other* way round from what the instruction produces.
            Instr::Cast { dst, to, src } => format!(
                "%{} = {} {}",
                function.name_of(*dst),
                match to {
                    Num::Int => Prim::Int,
                    Num::Float => Prim::Float,
                }
                .name(),
                match to {
                    Num::Int => function.value_name_as(Num::Float, src),
                    Num::Float => function.value_name_as(Num::Int, src),
                }
            ),
            Instr::Param { dst, index } => {
                format!("%{} = param {index}", function.name_of(*dst))
            }
            Instr::Frame { dst, offset } => {
                format!("%{} = frame {offset}", function.name_of(*dst))
            }
            Instr::Field { dst, base, offset } => {
                format!("%{} = field {} + {offset}", function.name_of(*dst), value(base))
            }
            Instr::Elem { dst, base, index, len, scale } => format!(
                "%{} = elem {}[{}] of {} by {scale}",
                function.name_of(*dst),
                value(base),
                value(index),
                value(len)
            ),
            Instr::Count { dst, of } => {
                format!("%{} = count {}", function.name_of(*dst), value(of))
            }
            Instr::LoadChar { dst, addr } => {
                format!("%{} = loadchar {}", function.name_of(*dst), value(addr))
            }
            Instr::RtCall { dst, callee, args } => {
                let args: Vec<String> = args.iter().map(value).collect();
                let call = format!("rt.{}({})", callee.name(), args.join(", "));
                match dst {
                    Some(dst) => format!("%{} = {call}", function.name_of(*dst)),
                    None => call,
                }
            }
            Instr::Fixup { at, count, stride } => {
                format!("fixup {} of {} bytes at {}", value(count), stride, value(at))
            }
            Instr::CopyBytes { dst, src, bytes } => {
                format!("copy {} bytes to {}, from {}", bytes, value(dst), value(src))
            }
            Instr::Load { dst, addr } => {
                format!("%{} = load {}", function.name_of(*dst), value(addr))
            }
            Instr::Store { addr, value: stored } => {
                format!("store {}, {}", value(addr), value(stored))
            }
            Instr::Call { dst, callee, args } => {
                let args: Vec<String> = args.iter().map(value).collect();
                let call = format!("call {}({})", self.function(*callee).name, args.join(", "));
                match dst {
                    Some(dst) => format!("%{} = {call}", function.name_of(*dst)),
                    None => call,
                }
            }
            Instr::VariantAddr { dst, id, tag } => format!(
                "%{} = value {}::{}",
                function.name_of(*dst),
                self.table.enum_info(*id).name,
                self.table.enum_info(*id).variants[*tag as usize].name
            ),
            Instr::VTable { dst, class } => format!(
                "%{} = vtable {}",
                function.name_of(*dst),
                self.table.class(*class).name
            ),
            Instr::CallVirtual { dst, slot, receiver, args } => {
                let args: Vec<String> = args.iter().map(value).collect();
                let call =
                    format!("callv {}[{slot}]({})", value(receiver), args.join(", "));
                match dst {
                    Some(dst) => format!("%{} = {call}", function.name_of(*dst)),
                    None => call,
                }
            }
            Instr::Print { ty, val, newline } => format!(
                "print{} {} {}",
                match newline {
                    true => "ln",
                    false => "",
                },
                ty.name(&self.table),
                function.value_name_as(Num::of(*ty), val)
            ),
            Instr::PrintText { id } => {
                format!("print text{} {:?}", id.0, self.texts[id.0 as usize])
            }
        }
    }
}

fn op_name(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "add",
        BinOp::Sub => "sub",
        BinOp::Mul => "mul",
        BinOp::Div => "div",
        BinOp::Rem => "rem",
    }
}
