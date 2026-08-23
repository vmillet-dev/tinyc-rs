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

    /// The echoed source line and the caret line beneath it, with the
    /// `N | ` gutter taken off both, so a test can compare columns inside the
    /// snippet without counting the width of a line number.
    fn snippet(rendered: &str) -> (String, String) {
        let body = |line: &str| line.split_once("| ").map_or("", |(_, rest)| rest).to_string();
        let rows: Vec<&str> = rendered.lines().collect();
        let source = rows.iter().rev().nth(1).expect("the echoed source line");
        let carets = rows.last().expect("the caret line");
        (body(source), body(carets))
    }

    // -- spans -------------------------------------------------------------

    #[test]
    fn joining_two_spans_covers_both_of_them() {
        // `Span::to` is how a diagnostic about an *operation* gets a caret over
        // the whole thing: the operator's span joined to each operand's.
        let left = Span::new(4, 2);
        let right = Span::new(10, 3);
        assert_eq!(left.to(right), Span::new(4, 9));

        // The order the two are written in cannot matter, or an overflow
        // reported from the right operand would underline backwards.
        assert_eq!(right.to(left), Span::new(4, 9));
    }

    #[test]
    fn joining_a_span_to_itself_changes_nothing() {
        let span = Span::new(7, 3);
        assert_eq!(span.to(span), span);
    }

    #[test]
    fn joining_a_span_to_one_inside_it_keeps_the_outer_one() {
        let outer = Span::new(0, 20);
        let inner = Span::new(5, 2);
        assert_eq!(outer.to(inner), outer);
        assert_eq!(inner.to(outer), outer);
    }

    // -- line and column ---------------------------------------------------

    #[test]
    fn an_offset_past_the_end_lands_on_the_last_position() {
        // Nothing should produce one, but a diagnostic that did would take the
        // whole compiler down with it rather than merely pointing oddly.
        let sf = SourceFile::new("t.tc", "ab\ncd".to_string());
        assert_eq!(sf.line_col(999), (2, 3));
    }

    #[test]
    fn an_empty_file_still_has_a_first_position() {
        // `no_main.tc` is reported at 1:1 of a file that may hold nothing at all.
        let sf = SourceFile::new("t.tc", String::new());
        assert_eq!(sf.line_col(0), (1, 1));

        let rendered = sf.render(&Diagnostic::new("no main", Span::new(0, 0)));
        assert!(rendered.contains("t.tc:1:1"), "{rendered}");
        assert!(rendered.contains('^'), "{rendered}");
    }

    #[test]
    fn a_newline_belongs_to_the_line_it_ends() {
        let sf = SourceFile::new("t.tc", "ab\ncd\n".to_string());
        assert_eq!(sf.line_col(2), (1, 3), "the newline itself is still line 1");
        assert_eq!(sf.line_col(3), (2, 1), "the character after it starts line 2");
    }

    #[test]
    fn a_carriage_return_is_not_part_of_the_line_that_is_echoed() {
        // A file written on Windows has `\r\n`, and echoing the `\r` would put
        // the terminal's cursor back at the start of the line — printing the
        // carets *over* the source they are supposed to sit under.
        let sf = SourceFile::new("t.tc", "int x = 1;\r\nprint(y);\r\n".to_string());
        let offset = sf.text().find('y').unwrap();
        assert_eq!(sf.line_col(offset as u32), (2, 7));

        let rendered = sf.render(&Diagnostic::new("undeclared", Span::new(offset, 1)));
        assert!(!rendered.contains('\r'), "a carriage return survived: {rendered:?}");
        assert!(rendered.contains("print(y);"), "{rendered}");
    }

    #[test]
    fn a_last_line_with_no_newline_is_still_a_line() {
        let sf = SourceFile::new("t.tc", "int x = 1;\nprint(y);".to_string());
        let offset = sf.text().find('y').unwrap();
        let rendered = sf.render(&Diagnostic::new("undeclared", Span::new(offset, 1)));
        assert!(rendered.contains("t.tc:2:7"), "{rendered}");
        assert!(rendered.contains("print(y);"), "{rendered}");
    }

    // -- what the carets sit under -----------------------------------------

    #[test]
    fn the_carets_count_characters_of_the_span() {
        let sf = SourceFile::new("t.tc", "print(hello);\n".to_string());
        let offset = sf.text().find("hello").unwrap();
        let rendered = sf.render(&Diagnostic::new("undeclared", Span::new(offset, 5)));
        assert!(rendered.contains("^^^^^"), "five characters, five carets: {rendered}");
        assert!(!rendered.contains("^^^^^^"), "{rendered}");
    }

    #[test]
    fn a_span_of_multibyte_characters_gets_one_caret_each() {
        // The span's length is in *bytes*; the underline is in characters, so a
        // three-byte character must not widen it to three carets.
        let sf = SourceFile::new("t.tc", "print(\"日本語\");\n".to_string());
        let offset = sf.text().find('日').unwrap();
        let rendered = sf.render(&Diagnostic::new("nope", Span::new(offset, "日本語".len())));
        let carets = rendered.lines().last().unwrap().matches('^').count();
        assert_eq!(carets, 3, "{rendered}");
    }

    #[test]
    fn a_span_with_no_width_still_gets_one_caret() {
        // A caret with nothing under it would point at nothing at all.
        let sf = SourceFile::new("t.tc", "int x = 1;\n".to_string());
        let rendered = sf.render(&Diagnostic::new("here", Span::new(4, 0)));
        assert_eq!(rendered.lines().last().unwrap().matches('^').count(), 1, "{rendered}");
    }

    #[test]
    fn a_span_running_past_its_line_is_clipped_to_it() {
        // A missing `}` can leave a span covering the rest of the file. The
        // underline belongs on the line being echoed and nowhere else.
        let sf = SourceFile::new("t.tc", "ab\ncdef\n".to_string());
        let rendered = sf.render(&Diagnostic::new("unclosed", Span::new(0, 8)));
        let carets = rendered.lines().last().unwrap().matches('^').count();
        assert_eq!(carets, 2, "only `ab` is on the line that was echoed: {rendered}");
    }

    #[test]
    fn a_span_at_the_end_of_a_line_puts_the_caret_after_the_last_character() {
        // The shape of a missing semicolon: the span points at where something
        // should have been, which is past everything that is there.
        let sf = SourceFile::new("t.tc", "int x = 1\nprint(x);\n".to_string());
        let rendered = sf.render(&Diagnostic::new("expected `;`", Span::new(9, 1)));
        let (source, carets) = snippet(&rendered);
        assert_eq!(source, "int x = 1", "{rendered}");
        // The caret sits one past the last character, which is where the `;`
        // was wanted — not on the `1`, which is not the mistake.
        assert_eq!(carets.find('^'), Some(source.len()), "{rendered}");
    }

    #[test]
    fn tabs_are_expanded_so_the_carets_land_under_the_right_character() {
        // The source line is echoed with tabs expanded, so the underline has to
        // be measured in the same expanded columns — otherwise it points at a
        // character several places to the left of the one that is wrong.
        let sf = SourceFile::new("t.tc", "\t\tprint(y);\n".to_string());
        let offset = sf.text().find('y').unwrap();
        let rendered = sf.render(&Diagnostic::new("undeclared", Span::new(offset, 1)));

        let source_line = rendered.lines().nth(3).unwrap();
        let caret_line = rendered.lines().last().unwrap();
        assert!(!source_line.contains('\t'), "a tab was echoed as itself: {rendered:?}");
        assert_eq!(caret_line.find('^'), source_line.find('y'), "{rendered}");
    }

    // -- notes -------------------------------------------------------------

    #[test]
    fn a_note_without_a_span_is_text_and_nothing_else() {
        let sf = SourceFile::new("t.tc", "int x = 1;\n".to_string());
        let d = Diagnostic::new("something", Span::new(4, 1)).with_note("try this instead", None);
        let rendered = sf.render(&d);

        assert!(rendered.contains("= note: try this instead"), "{rendered}");
        // One snippet, so one arrow: a note with nowhere to point adds no second.
        assert_eq!(rendered.matches("-->").count(), 1, "{rendered}");
    }

    #[test]
    fn a_note_with_a_span_gets_a_snippet_of_its_own() {
        // What a redeclaration looks like: the caret on the second declaration,
        // and the note pointing back at the first.
        let sf = SourceFile::new("t.tc", "int x = 1;\nint x = 2;\n".to_string());
        let d = Diagnostic::new("`x` is already declared", Span::new(15, 1))
            .with_note("first declared here", Some(Span::new(4, 1)));
        let rendered = sf.render(&d);

        assert_eq!(rendered.matches("-->").count(), 2, "both places are shown: {rendered}");
        assert!(rendered.contains("t.tc:2:5"), "{rendered}");
        assert!(rendered.contains("t.tc:1:5"), "{rendered}");
        // The note comes between the two, not after both.
        let note = rendered.find("= note:").unwrap();
        assert!(note > rendered.find("t.tc:2:5").unwrap(), "{rendered}");
        assert!(note < rendered.find("t.tc:1:5").unwrap(), "{rendered}");
    }

    #[test]
    fn the_gutter_is_wide_enough_for_the_widest_line_number_shown() {
        // The note may point at a line whose number is wider than the error's.
        // A gutter sized for the error alone would leave the second snippet's
        // bar out of line with the first's.
        let mut text = String::new();
        for n in 1..=12 {
            text.push_str(&format!("int v{n} = {n};\n"));
        }
        let sf = SourceFile::new("t.tc", text);
        let first = sf.text().find("v1 ").unwrap();
        let twelfth = sf.text().find("v12").unwrap();

        // Error on line 1, note on line 12: the wider number is the note's.
        let d = Diagnostic::new("something", Span::new(first, 2))
            .with_note("and here", Some(Span::new(twelfth, 3)));
        let rendered = sf.render(&d);

        let bars: Vec<&str> =
            rendered.lines().filter(|line| line.trim_start().starts_with('|')).collect();
        assert!(bars.len() >= 2, "{rendered}");
        let column = bars[0].find('|').unwrap();
        for bar in &bars {
            assert_eq!(bar.find('|'), Some(column), "the bars do not line up:\n{rendered}");
        }
    }

    // -- long lines --------------------------------------------------------

    #[test]
    fn a_window_slides_back_so_the_end_of_a_long_line_is_reachable() {
        // Centring on the caret would run the window off the end for anything
        // near it, and then show fewer characters than it could.
        let line = format!("int a = 1; // {} target", "x".repeat(500));
        let offset = line.find("target").unwrap();
        let sf = SourceFile::new("t.tc", line);

        let rendered = sf.render(&Diagnostic::new("here", Span::new(offset, 6)));
        let (echoed, carets) = snippet(&rendered);
        assert!(echoed.contains("target"), "the caret's own text must survive: {echoed}");
        assert!(echoed.starts_with(ELLIPSIS), "the cut on the left is marked: {echoed}");
        assert!(!echoed.ends_with(ELLIPSIS), "there is nothing left to cut on the right: {echoed}");
        assert_eq!(carets.find('^'), echoed.find("target"), "{rendered}");
    }

    #[test]
    fn a_window_at_the_start_of_a_long_line_is_not_cut_on_the_left() {
        let line = format!("target = 1; // {}", "x".repeat(500));
        let sf = SourceFile::new("t.tc", line);

        let rendered = sf.render(&Diagnostic::new("here", Span::new(0, 6)));
        let (echoed, carets) = snippet(&rendered);
        assert!(echoed.starts_with("target"), "nothing was cut before it: {echoed}");
        assert!(echoed.ends_with(ELLIPSIS), "the cut is on the right: {echoed}");
        assert_eq!(carets.find('^'), Some(0), "{rendered}");
    }

    #[test]
    fn an_underline_on_a_long_line_never_runs_past_the_window() {
        // The span may cover more than the window shows, and carets past the
        // end of the echoed text would point at nothing.
        let line = format!("{} end", "x".repeat(400));
        let sf = SourceFile::new("t.tc", line.clone());

        let rendered = sf.render(&Diagnostic::new("all of it", Span::new(0, line.len())));
        let echoed = rendered.lines().nth(3).unwrap();
        let carets = rendered.lines().last().unwrap().matches('^').count();
        assert!(carets <= echoed.chars().count(), "{rendered}");
        assert!(carets >= 1, "{rendered}");
    }

    #[test]
    fn every_rendering_ends_in_exactly_one_newline() {
        // The CLI joins diagnostics with a blank line between them, which only
        // reads right if each one ends the same way.
        let sf = SourceFile::new("t.tc", "int x = 1;\nint y = 2;\n".to_string());
        let cases = [
            Diagnostic::new("plain", Span::new(4, 1)),
            Diagnostic::new("labelled", Span::new(4, 1)).with_label("here"),
            Diagnostic::new("noted", Span::new(4, 1)).with_note("also", None),
            Diagnostic::new("noted at", Span::new(4, 1)).with_note("also", Some(Span::new(15, 1))),
        ];
        for d in cases {
            let rendered = sf.render(&d);
            assert!(rendered.ends_with('\n'), "{rendered:?}");
            assert!(!rendered.ends_with("\n\n"), "{rendered:?}");
        }
    }
}
