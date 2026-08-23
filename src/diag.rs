//! Source positions and compiler diagnostics.
//!
//! Spans carry byte offsets only; the human-readable line/column is derived from
//! the [`SourceFile`] when a diagnostic is rendered. Columns are counted in
//! characters, not bytes, so a non-ASCII string literal earlier on the line does
//! not shift the reported column.

use std::path::PathBuf;

/// How many columns a tab character occupies when a source line is echoed.
const TAB_WIDTH: usize = 4;

/// Most characters of a source line a diagnostic will echo. A longer line is
/// shown as a window around the caret, with `...` for what was cut.
const MAX_ECHOED_LINE: usize = 100;

/// The marker standing in for the part of a line that was cut.
const ELLIPSIS: &str = "...";

/// A half-open byte range `[offset, offset + len)` into a [`SourceFile`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub offset: u32,
    pub len: u32,
}

impl Span {
    pub fn new(offset: usize, len: usize) -> Span {
        Span { offset: offset as u32, len: len as u32 }
    }

    /// The smallest span covering both `self` and `other`.
    pub fn to(self, other: Span) -> Span {
        let start = self.offset.min(other.offset);
        let end = (self.offset + self.len).max(other.offset + other.len);
        Span { offset: start, len: end - start }
    }
}

/// A compile error, always anchored to a span of source text.
#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub message: String,
    pub span: Span,
    /// Short text printed next to the carets, e.g. `expected int, found string`.
    pub label: Option<String>,
    /// Secondary remark, optionally pointing at another span (e.g. a previous
    /// declaration).
    pub note: Option<(String, Option<Span>)>,
}

impl Diagnostic {
    pub fn new(message: impl Into<String>, span: Span) -> Diagnostic {
        Diagnostic { message: message.into(), span, label: None, note: None }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Diagnostic {
        self.label = Some(label.into());
        self
    }

    pub fn with_note(mut self, note: impl Into<String>, span: Option<Span>) -> Diagnostic {
        self.note = Some((note.into(), span));
        self
    }
}

/// Convenience for stages that bail out on the first error.
pub type Result<T> = std::result::Result<T, Vec<Diagnostic>>;

/// A source file plus the index needed to map byte offsets back to line/column.
pub struct SourceFile {
    path: PathBuf,
    text: String,
    /// Byte offset of the first character of each line.
    line_starts: Vec<usize>,
}

impl SourceFile {
    pub fn new(path: impl Into<PathBuf>, text: String) -> SourceFile {
        let mut line_starts = vec![0];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        SourceFile { path: path.into(), text, line_starts }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// 1-based line and column (in characters) for a byte offset.
    pub fn line_col(&self, offset: u32) -> (usize, usize) {
        let offset = (offset as usize).min(self.text.len());
        // The last line whose start is <= offset.
        let line = self.line_starts.partition_point(|&start| start <= offset) - 1;
        let col = self.text[self.line_starts[line]..offset].chars().count() + 1;
        (line + 1, col)
    }

    /// The text of a 1-based line, without its terminator.
    fn line_text(&self, line: usize) -> &str {
        let start = self.line_starts[line - 1];
        let end = self
            .line_starts
            .get(line)
            .copied()
            .unwrap_or(self.text.len());
        self.text[start..end].trim_end_matches(['\n', '\r'])
    }

    /// Render a diagnostic in the familiar `rustc` style:
    ///
    /// ```text
    /// error: cannot apply `+` to `int` and `string`
    ///  --> examples/errors/type_mismatch.tc:4:11
    ///   |
    /// 4 | print(x + s);
    ///   |           ^ expected int, found string
    /// ```
    pub fn render(&self, d: &Diagnostic) -> String {
        let (line, _) = self.line_col(d.span.offset);
        let gutter = line
            .max(d.note.as_ref().and_then(|(_, s)| *s).map_or(0, |s| self.line_col(s.offset).0))
            .to_string()
            .len();

        let mut out = format!("error: {}\n", d.message);
        self.render_snippet(&mut out, d.span, d.label.as_deref(), gutter);

        if let Some((note, span)) = &d.note {
            out.push_str(&format!("{:w$} = note: {note}\n", "", w = gutter));
            if let Some(span) = span {
                self.render_snippet(&mut out, *span, None, gutter);
            }
        }
        out
    }

    fn render_snippet(&self, out: &mut String, span: Span, label: Option<&str>, gutter: usize) {
        let (line, col) = self.line_col(span.offset);
        let text = self.line_text(line);

        // Expand tabs so the carets line up with what the terminal shows.
        let mut shown = String::new();
        let mut caret_col = 0;
        let mut chars_seen = 0;
        for c in text.chars() {
            if chars_seen == col - 1 {
                caret_col = shown.chars().count();
            }
            if c == '\t' {
                shown.push_str(&" ".repeat(TAB_WIDTH));
            } else {
                shown.push(c);
            }
            chars_seen += 1;
        }
        if chars_seen < col {
            // Span points at (or past) the end of the line, e.g. a missing `;`.
            caret_col = shown.chars().count();
        }

        // Clamp the underline to the remainder of this line, but always show one caret.
        let span_end = (span.offset + span.len) as usize;
        let line_end = self.line_starts[line - 1] + text.len();
        let end = span_end.min(line_end);
        let mut width = self.text[span.offset as usize..end.max(span.offset as usize)]
            .chars()
            .count()
            .max(1);

        // A machine-generated line can be arbitrarily long, and echoing all of
        // it buries the message. Keep a window around the caret instead.
        let (shown, caret_col) = window(&shown, caret_col, &mut width);

        let path = self.path.display();
        out.push_str(&format!("{:w$}--> {path}:{line}:{col}\n", " ", w = gutter));
        out.push_str(&format!("{:w$} |\n", "", w = gutter));
        out.push_str(&format!("{line:>w$} | {shown}\n", w = gutter));
        out.push_str(&format!(
            "{:w$} | {:c$}{}{}\n",
            "",
            "",
            "^".repeat(width),
            label.map(|l| format!(" {l}")).unwrap_or_default(),
            w = gutter,
            c = caret_col,
        ));
    }
}

/// Cut `line` down to [`MAX_ECHOED_LINE`] characters around `caret`, answering
/// the text to print and the column the caret lands on inside it. `width`, the
/// length of the underline, is clipped to what is still visible.
fn window(line: &str, caret: usize, width: &mut usize) -> (String, usize) {
    let length = line.chars().count();
    if length <= MAX_ECHOED_LINE {
        return (line.to_string(), caret);
    }

    // Centre the window on the caret, then slide it back inside the line so the
    // last characters are still reachable.
    let half = MAX_ECHOED_LINE / 2;
    let start = caret.saturating_sub(half).min(length - MAX_ECHOED_LINE);
    let end = start + MAX_ECHOED_LINE;

    let mut shown = String::new();
    if start > 0 {
        shown.push_str(ELLIPSIS);
    }
    shown.extend(line.chars().skip(start).take(MAX_ECHOED_LINE));
    if end < length {
        shown.push_str(ELLIPSIS);
    }

    *width = (*width).min(end - caret.min(end)).max(1);
    (shown, caret - start + if start > 0 { ELLIPSIS.len() } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_col_is_one_based() {
        let sf = SourceFile::new("t.tc", "int x = 1;\nprint(x);\n".to_string());
        assert_eq!(sf.line_col(0), (1, 1));
        assert_eq!(sf.line_col(4), (1, 5));
        assert_eq!(sf.line_col(11), (2, 1));
        assert_eq!(sf.line_col(17), (2, 7));
    }

    #[test]
    fn columns_count_chars_not_bytes() {
        // The "é" is two bytes, so a byte-based column would report 13 here.
        let sf = SourceFile::new("t.tc", "string s = \"é\"; x".to_string());
        let offset = sf.text().find('x').unwrap() as u32;
        assert_eq!(sf.line_col(offset), (1, 17));
    }

    #[test]
    fn a_very_long_line_is_shown_as_a_window_around_the_caret() {
        // A generated file can hold a line of any length; echoing all of it
        // would bury the message it is supposed to illustrate.
        let padding = "x".repeat(500);
        let text = format!("int a = 1; // {padding} here");
        let offset = text.find("here").unwrap() as u32;
        let sf = SourceFile::new("t.tc", text);

        let rendered = sf.render(&Diagnostic::new("something", Span::new(offset as usize, 4)));
        let echoed = rendered.lines().nth(3).expect("the source line");
        assert!(echoed.len() < MAX_ECHOED_LINE + 20, "line was not trimmed: {echoed}");
        assert!(echoed.contains(ELLIPSIS), "the cut should be marked: {echoed}");
        assert!(echoed.contains("here"), "the caret's own text must survive: {echoed}");
    }

    #[test]
    fn a_short_line_is_echoed_whole() {
        let sf = SourceFile::new("t.tc", "int x = 1;\n".to_string());
        let rendered = sf.render(&Diagnostic::new("something", Span::new(4, 1)));
        assert!(rendered.contains("int x = 1;"), "{rendered}");
        assert!(!rendered.contains(ELLIPSIS), "{rendered}");
    }

    #[test]
    fn render_points_at_the_span() {
        let sf = SourceFile::new("t.tc", "int x = 1;\nprint(y);\n".to_string());
        let d = Diagnostic::new("undeclared variable `y`", Span::new(17, 1))
            .with_label("not found in this scope");
        let rendered = sf.render(&d);
        assert!(rendered.contains("t.tc:2:7"), "{rendered}");
        assert!(rendered.contains("      ^ not found in this scope"), "{rendered}");
    }
}
