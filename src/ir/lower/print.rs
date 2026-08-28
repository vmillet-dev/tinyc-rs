//! Lowering `print` and `println`, and interning what they write.

use super::*;

impl Lowering<'_> {
    /// `print(...)` and `println(...)`: one write per thing written.
    ///
    /// The parts were settled by the parser, so nothing here reads a `%`. What
    /// this stage adds is the line ending, and that is where the two spellings
    /// stop being two statements: a `println` is a `print` with one more piece
    /// of text at the end. The same desugaring `for` gets, one stage after the
    /// tree has been dumped.
    ///
    /// **Every value is evaluated before anything is written.** A `print` is
    /// written like a call and is read like one, so its arguments go first —
    /// otherwise `println("n: %d", noisy())` would put whatever `noisy` writes
    /// in the middle of this line rather than before it. The values stay live
    /// across the writes, which is the register allocator's business.
    pub(super) fn print_stmt(&mut self, newline: bool, parts: &[PrintPart]) {
        let mut written: Vec<Instr> = Vec::new();
        for part in parts {
            if let PrintPart::Value(expr) | PrintPart::Spec { expr, .. } = part {
                let ty = self.types.of(expr.id);
                let val = self.expr(expr);
                // What an enum prints is the *name* of its variant, which the
                // backend looks up by tag. A boxed one is a pointer, so the tag
                // is read here — and the backend goes on doing exactly what it
                // did, with the number it always expected.
                let val = self.tag_of(ty, val);
                written.push(Instr::Print { ty, val, newline: false });
            }
        }

        let mut written = written.into_iter();
        let mut text: Vec<char> = Vec::new();
        // Whether *this* statement's last piece was a value. Not the same as
        // finding no text left over: `println()` has none either, and the write
        // it would attach a line ending to belongs to the statement before it.
        let mut ended_with_a_value = false;
        for part in parts {
            match part {
                PrintPart::Text(chars) => {
                    text.extend(chars);
                    ended_with_a_value = false;
                }
                _ => {
                    self.flush_text(&mut text);
                    self.emit(written.next().expect("one per value part"));
                    ended_with_a_value = true;
                }
            }
        }
        // Where the last piece written was a value, the line ends with it: the
        // backend reaches for a format that already ends in one, so `println(n)`
        // is a single call. Where it was text — `println("done")` — the newline
        // joins that text below, exactly as it always has.
        if newline && ended_with_a_value {
            self.end_the_line_written_last();
            return;
        }
        if newline {
            text.push('\n');
        }
        self.flush_text(&mut text);
    }

    /// Make the write just emitted end its line.
    ///
    /// Only ever called straight after emitting one, which is what makes the
    /// last instruction in the block certain to be it.
    pub(super) fn end_the_line_written_last(&mut self) {
        match self.blocks[self.current.0 as usize].instrs.last_mut() {
            Some(Instr::Print { newline, .. }) => *newline = true,
            other => unreachable!("a value was just written, not {other:?}"),
        }
    }

    /// Write out the literal text collected so far, if there is any.
    ///
    /// Collected rather than written piece by piece, so that the newline of
    /// `println("done")` joins the word in front of it and the whole line
    /// leaves in one call.
    pub(super) fn flush_text(&mut self, text: &mut Vec<char>) {
        if text.is_empty() {
            return;
        }
        let id = self.intern_text(std::mem::take(text).into_iter().collect());
        self.emit(Instr::PrintText { id });
    }

    /// The same for a run of literal text, kept in its own table because it is
    /// laid out differently: a string literal is characters four bytes each
    /// with a count in front, and this is the UTF-8 `printf` will be handed.
    pub(super) fn intern_text(&mut self, text: String) -> TextId {
        if let Some(&id) = self.strings.text_ids.get(&text) {
            return id;
        }
        let id = TextId(self.strings.texts.len() as u32);
        self.strings.texts.push(text.clone());
        self.strings.text_ids.insert(text, id);
        id
    }

    pub(super) fn intern(&mut self, chars: &[char]) -> StrId {
        if let Some(&id) = self.strings.ids.get(chars) {
            return id;
        }
        let id = StrId(self.strings.chars.len() as u32);
        self.strings.chars.push(chars.to_vec());
        self.strings.ids.insert(chars.to_vec(), id);
        id
    }
}
