//! Writing the listing, and the one stack frame shape every runtime routine
//! uses.

use super::{Abi, RUNTIME_LOCALS};

/// The assembly listing as it is written.
pub struct Asm {
    out: String,
}

impl Asm {
    pub fn new() -> Asm {
        Asm { out: String::new() }
    }

    pub fn finish(self) -> String {
        self.out
    }

    pub fn line(&mut self, text: &str) {
        self.out.push_str(text);
        self.out.push('\n');
    }

    pub fn blank(&mut self) {
        self.out.push('\n');
    }

    pub fn comment(&mut self, text: &str) {
        self.line(&format!("    ; {text}"));
    }

    pub fn asm(&mut self, text: &str) {
        self.line(&format!("    {text}"));
    }

    /// `mov` that drops the no-op a reused register often produces.
    pub fn mov(&mut self, dst: &str, src: &str) {
        if dst != src {
            self.asm(&format!("mov  {dst}, {src}"));
        }
    }

    /// Announce that no vector register is being passed, where the convention
    /// asks. System V reads `al` at a variadic call to decide how much of the
    /// register save area to fill; Windows does not look, so this is nothing
    /// there.
    ///
    /// Every variadic call this backend makes is to `printf`, and it passes
    /// integers only — so the answer is always zero.
    pub fn variadic(&mut self, abi: &Abi) {
        if abi.variadic_in_al {
            self.asm("xor  eax, eax    ; no vector register is passed");
        }
    }
}

/// One runtime routine's stack frame: the registers it saves, the bytes it
/// reserves for itself, and the shadow space its own callees expect underneath.
///
/// The prologue and the epilogue are one value so that they cannot come apart.
/// A `push` whose `pop` was forgotten, or a `sub rsp` whose `add` was, leaves
/// `rsp` wrong for whoever runs next — a crash a long way from its cause, and
/// the kind of mistake that only shows up in the routine nobody exercised.
///
/// The alignment rule is *derived* here, once, rather than written down at
/// every routine: a routine is reached by `call`, so `rsp % 16 == 8` on
/// arrival, each push takes eight more, and what is reserved has to bring the
/// total back to a multiple of sixteen. Deriving it is what lets one routine
/// body be emitted for a platform with shadow space and for one without —
/// every hand-computed 40 and 72 in this backend used to be a Windows fact
/// hiding in shared code.
pub struct StubFrame {
    saved: &'static [&'static str],
    reserved: u32,
    /// Where this routine's own bytes start, measured from `rsp`: just above
    /// the shadow space a callee would claim.
    scratch_at: u32,
}

impl StubFrame {
    /// Push `saved` in order, then reserve room for this routine's own
    /// `scratch` bytes plus whatever its callees expect below them.
    ///
    /// `wants` says what the routine wants *its own* bytes for, and is all a
    /// caller has to describe: the shadow space and the padding are this
    /// method's doing, so it writes those parts of the comment itself. A
    /// routine that said "shadow space + alignment" on a platform with neither
    /// would be a comment that only happened to be true.
    pub fn enter(
        asm: &mut Asm,
        abi: &Abi,
        saved: &'static [&'static str],
        scratch: u32,
        wants: &str,
    ) -> StubFrame {
        for register in saved {
            assert!(
                RUNTIME_LOCALS.contains(register),
                "{register} is not callee-saved on both platforms, so a routine may not keep \
                 a value in it — see RUNTIME_LOCALS"
            );
            asm.asm(&format!("push {register}"));
        }
        let reserved = abi.frame(saved.len(), scratch);
        debug_assert_eq!(
            (8 + 8 * saved.len() as u32 + reserved) % 16,
            0,
            "a runtime routine's frame has to leave rsp aligned for the calls it makes"
        );
        debug_assert!(reserved >= abi.shadow_space + scratch, "the frame has to hold what it owes");
        // A routine that owes its callees nothing and lands aligned on its
        // pushes alone reserves nothing — which happens on a platform with no
        // shadow space, and would otherwise read as `sub rsp, 0`.
        if reserved > 0 {
            let mut parts = Vec::new();
            if abi.shadow_space > 0 {
                parts.push(format!("{} bytes of shadow space", abi.shadow_space));
            }
            if scratch > 0 {
                parts.push(format!("{scratch} bytes for {wants}"));
            }
            if parts.is_empty() || reserved > abi.shadow_space + scratch {
                parts.push("alignment".to_string());
            }
            asm.asm(&format!("sub  rsp, {reserved}    ; {}", parts.join(" + ")));
        }
        StubFrame { saved, reserved, scratch_at: abi.shadow_space }
    }

    /// Give the frame back and return — the exact reverse of [`Self::enter`],
    /// which is why the two are never written out by hand.
    pub fn ret(&self, asm: &mut Asm) {
        if self.reserved > 0 {
            asm.asm(&format!("add  rsp, {}", self.reserved));
        }
        for register in self.saved.iter().rev() {
            asm.asm(&format!("pop  {register}"));
        }
        asm.asm("ret");
    }

    /// The routine's own byte `offset`, as an addressing mode.
    ///
    /// These sit *above* the shadow space, which is also why they double as the
    /// stack-argument slots of a call with more arguments than fit in
    /// registers: on Windows the fifth argument goes exactly where the shadow
    /// space stops.
    pub fn local(&self, offset: u32) -> String {
        format!("[rsp+{}]", self.scratch_at + offset)
    }

    /// Where those bytes start, for the one place that has to index into them.
    pub fn scratch_at(&self) -> u32 {
        self.scratch_at
    }
}
