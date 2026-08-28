//! Turning facts into the words a diagnostic uses.

use crate::ast::Ty;

use crate::diag::{Diagnostic, Span};

/// How many elements an array may hold.
///
/// Not a limit of the representation but of the *code*: an array is built by
/// storing every element, so a huge one would emit a huge function. A repeat
/// form like `[0; 1000]` lowered to a loop is what would lift it.
pub const MAX_ARRAY_LEN: i64 = 1024;

/// How a parameter is described where a declaration is, which is also how one
/// is told from a local afterwards. See [`Binding::parameter`].
pub(super) const PARAMETER: &str = "a parameter";

/// What a name in scope stands for.
///
/// The last field is the only one that is not merely bookkeeping: a parameter
/// names a value **the caller owns**, and there is one operation that needs to
/// know the difference — see [`FnChecker::push_stmt`].
#[derive(Clone, Copy, Debug)]
pub(super) struct Binding {
    pub(super) ty: Ty,
    pub(super) name_span: Span,
    pub(super) parameter: bool,
}

/// Every conversion the language has, listed wherever one is refused.
///
/// It is a short list on purpose: nothing converts on its own, so this is also
/// the complete answer to "how do I turn this into that".
pub(super) const CONVERSIONS: &str = "the conversions are `int(c)`, `int(s)`, `int(f)`, `float(n)`, \
                           `char(n)`, `string(c)`, `string(n)`, and `string(cs)` for a `char[]`";

/// Why a particular number is not a character, which is a different sentence
/// depending on where it lands.
pub(super) fn scalar_range_label(value: i64) -> String {
    match value {
        0xD800..=0xDFFF => format!(
            "`{value}` is in the surrogate range 55296..=57343, which names no character"
        ),
        _ => "a character's code point is in 0..=1114111".to_string(),
    }
}

/// `` `A` is `` / `` `A` and `B` are ``, so the label agrees with its subject.
pub(super) fn missing_verb(missing: &[&str]) -> String {
    let verb = if missing.len() == 1 { "is" } else { "are" };
    format!("{} {verb}", list(missing))
}

/// "`Circle` has no field `q`", with the ones it does have listed underneath.
///
/// A field and a method are looked up the same way and are missed the same way,
/// so the message is written once and told which noun to use — `kind` is the
/// singular, and both of them take a plain `s`. Listing the real ones is what
/// turns "no such field" into an answer: the mistake is nearly always a
/// misspelling, and the right spelling is right there.
pub(super) fn no_such_member(class: &str, kind: &str, name: &str, at: Span, known: &[&str]) -> Diagnostic {
    let note = match known.is_empty() {
        true => format!("`{class}` has no {kind}s"),
        false => format!("`{class}` has {}", list(known)),
    };
    Diagnostic::new(format!("`{class}` has no {kind} `{name}`"), at)
        .with_label(format!("not one of its {kind}s"))
        .with_note(note, None)
}

/// `1 value` but `2 values` — a count a message can read out loud.
pub(super) fn count(n: usize, one: &str, many: &str) -> String {
    match n {
        1 => format!("1 {one}"),
        _ => format!("{n} {many}"),
    }
}

/// `A`, `A` and `B`, `A`, `B` and `C` — so a list reads as prose.
pub(super) fn list(items: &[&str]) -> String {
    let quoted: Vec<String> = items.iter().map(|item| format!("`{item}`")).collect();
    match quoted.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
    }
}


/// `1 argument` / `2 arguments`, so messages read like prose.
pub(super) fn plural(count: usize, noun: &str) -> String {
    match (count, noun) {
        (1, "was") => "1 was".to_string(),
        (n, "was") => format!("{n} were"),
        (1, noun) => format!("1 {noun}"),
        (n, noun) => format!("{n} {noun}s"),
    }
}

