//! One-based source locations, and the spans built out of them.
//!
//! Every event and every error [`super::Reader`] produces is anchored to a
//! [`Position`] or a [`Span`] — the whole reason this crate carries its own
//! XML pull parser rather than reaching for a third-party one is that a
//! malformed `<joint>` tag has to be able to say *where*, in a document a
//! human is going to open in an editor and fix.

use std::fmt;

/// A one-based line/column location in an XML source document, plus the
/// zero-based byte offset a caller can use to slice the original `&str`.
///
/// # Examples
///
/// ```
/// use astrs_urdf::xml::Position;
///
/// let position = Position::new(12, 5, 87);
/// assert_eq!(position.line, 12);
/// assert_eq!(position.column, 5);
/// assert_eq!(position.to_string(), "12:5");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position {
    /// The one-based line number.
    pub line: usize,
    /// The one-based column number, counted in [`char`]s rather than bytes
    /// (a multi-byte UTF-8 scalar advances the column by exactly one).
    pub column: usize,
    /// The zero-based byte offset into the source `&str`.
    pub offset: usize,
}

impl Position {
    /// The position a document starts at: line 1, column 1, byte offset 0.
    #[must_use]
    pub const fn start() -> Self {
        Self {
            line: 1,
            column: 1,
            offset: 0,
        }
    }

    /// A position at an explicit one-based `line`/`column` and byte offset.
    #[must_use]
    pub const fn new(line: usize, column: usize, offset: usize) -> Self {
        Self {
            line,
            column,
            offset,
        }
    }

    /// Advances this position over one consumed, already
    /// end-of-line-normalized logical character: `'\n'` moves to the start
    /// of the next line, anything else advances the column by one.
    /// `byte_len` is how many bytes of the *original* source that logical
    /// character consumed (which can be two, for a normalized `"\r\n"`
    /// pair — see [`super::Reader`]'s own end-of-line handling).
    pub const fn advance(&mut self, logical_char: char, byte_len: usize) {
        if logical_char == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        self.offset += byte_len;
    }
}

impl Default for Position {
    fn default() -> Self {
        Self::start()
    }
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.column)
    }
}

/// A half-open range of source text, from `start` up to (but not including)
/// `end`.
///
/// # Examples
///
/// ```
/// use astrs_urdf::xml::{Position, Span};
///
/// let span = Span::new(Position::new(2, 3, 10), Position::new(2, 6, 13));
/// assert_eq!(span.to_string(), "2:3");
/// assert!(span.is_single_line());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    /// The first character covered by this span.
    pub start: Position,
    /// One past the last character covered by this span.
    pub end: Position,
}

impl Span {
    /// A span covering `start..end`.
    #[must_use]
    pub const fn new(start: Position, end: Position) -> Self {
        Self { start, end }
    }

    /// An empty span at `position`, used where a failure has an exact point
    /// rather than an extent (an unexpected character, an unclosed tag's
    /// opening `<`).
    #[must_use]
    pub const fn point(position: Position) -> Self {
        Self {
            start: position,
            end: position,
        }
    }

    /// True when both ends sit on the same source line — the only case
    /// [`Span::render`] can draw a single-line caret run for.
    #[must_use]
    pub const fn is_single_line(&self) -> bool {
        self.start.line == self.end.line
    }

    /// Renders the source line this span starts on, with a caret run under
    /// the spanned columns — the diagnostic a caller (e.g. `astrs-cli`)
    /// shows next to `Display`'s one-line message.
    ///
    /// Returns `None` when `source` has no such line, which only happens if
    /// the span and the text were not produced from the same document.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_urdf::xml::{Position, Span};
    ///
    /// let source = "<robot>\n  <link name=\"a\">\n";
    /// let span = Span::new(Position::new(2, 3, 8), Position::new(2, 7, 12));
    /// let rendered = span.render(source).expect("line 2 exists");
    /// assert_eq!(rendered, "  <link name=\"a\">\n  ^^^^");
    /// ```
    #[must_use]
    pub fn render(&self, source: &str) -> Option<String> {
        let line = source.lines().nth(self.start.line.checked_sub(1)?)?;
        let mut out = String::with_capacity(line.len() * 2 + 4);
        out.push_str(line);
        out.push('\n');
        for _ in 1..self.start.column {
            out.push(' ');
        }
        let width = if self.is_single_line() {
            self.end.column.saturating_sub(self.start.column).max(1)
        } else {
            1
        };
        for _ in 0..width {
            out.push('^');
        }
        Some(out)
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.start, f)
    }
}

impl From<Position> for Span {
    fn from(position: Position) -> Self {
        Self::point(position)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn a_document_starts_at_one_one_zero() {
        assert_eq!(Position::start(), Position::new(1, 1, 0));
        assert_eq!(Position::default(), Position::start());
    }

    #[test]
    fn a_newline_resets_the_column_and_bumps_the_line() {
        let mut position = Position::start();
        position.advance('a', 1);
        position.advance('\n', 1);
        assert_eq!(position, Position::new(2, 1, 2));
    }

    #[test]
    fn a_multi_byte_scalar_advances_exactly_one_column_by_its_byte_length() {
        let mut position = Position::start();
        position.advance('あ', 'あ'.len_utf8());
        assert_eq!(position, Position::new(1, 2, 3));
    }

    #[test]
    fn a_normalized_crlf_pair_advances_the_line_exactly_once() {
        // The Reader passes a normalized '\n' logical char but charges the
        // two source bytes "\r\n" actually consumed.
        let mut position = Position::start();
        position.advance('\n', 2);
        assert_eq!(position, Position::new(2, 1, 2));
    }

    #[test]
    fn display_is_the_editor_style_line_colon_column() {
        assert_eq!(Position::new(3, 17, 40).to_string(), "3:17");
    }

    #[test]
    fn positions_order_by_line_then_column() {
        assert!(Position::new(1, 9, 8) < Position::new(2, 1, 9));
        assert!(Position::new(2, 1, 9) < Position::new(2, 2, 10));
    }

    #[test]
    fn a_point_span_renders_a_single_caret() {
        let span = Span::point(Position::new(1, 4, 3));
        assert_eq!(span.render("abcdef").expect("line 1"), "abcdef\n   ^");
    }

    #[test]
    fn a_wide_span_renders_a_caret_run() {
        let span = Span::new(Position::new(1, 2, 1), Position::new(1, 5, 4));
        assert_eq!(span.render("abcdef").expect("line 1"), "abcdef\n ^^^");
    }

    #[test]
    fn a_multi_line_span_renders_one_caret_on_its_first_line() {
        let span = Span::new(Position::new(1, 2, 1), Position::new(3, 5, 12));
        assert!(!span.is_single_line());
        assert_eq!(span.render("abc\ndef\nghi").expect("line 1"), "abc\n ^");
    }

    #[test]
    fn rendering_a_line_that_does_not_exist_is_none() {
        assert!(Span::point(Position::new(9, 1, 0)).render("abc").is_none());
        assert!(Span::point(Position::new(0, 1, 0)).render("abc").is_none());
    }

    #[test]
    fn a_span_displays_as_its_start() {
        let span = Span::new(Position::new(4, 2, 9), Position::new(9, 9, 40));
        assert_eq!(span.to_string(), "4:2");
        assert_eq!(
            Span::from(Position::new(1, 1, 0)),
            Span::point(Position::start())
        );
    }
}
