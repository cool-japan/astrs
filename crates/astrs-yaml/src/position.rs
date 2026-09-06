//! One-based source locations, and the spans built out of them.
//!
//! Everything this crate reports — a scanner complaint, a duplicate key, a
//! `serde` type mismatch — is anchored to a [`Position`] or a [`Span`]. The
//! whole reason `astrs-yaml` exists rather than a third-party parser is that
//! a manifest diagnostic has to be able to say *where*, and that is only
//! possible if the location travels with the value from the very first byte
//! the scanner looks at.

use std::fmt;

/// A one-based line/column location in a YAML source document.
///
/// Every scanner token, every tree node and every error this crate produces
/// carries one, so a manifest diagnostic can always name the exact place the
/// problem is rather than the value it eventually became.
///
/// # Examples
///
/// ```
/// use astrs_yaml::Position;
///
/// let position = Position::new(12, 5);
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
}

impl Position {
    /// The position a document starts at: line 1, column 1.
    #[must_use]
    pub const fn start() -> Self {
        Self { line: 1, column: 1 }
    }

    /// A position at an explicit one-based `line` and `column`.
    #[must_use]
    pub const fn new(line: usize, column: usize) -> Self {
        Self { line, column }
    }

    /// Advance this position over one consumed character.
    ///
    /// A `'\n'` moves to the start of the next line; anything else — a
    /// `'\r'` included, since YAML 1.2 line breaks are normalized by the
    /// scanner before positions are tracked — advances the column by one.
    pub const fn advance(&mut self, ch: char) {
        if ch == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
    }

    /// The zero-based indentation this position implies.
    ///
    /// Block-structure decisions are all expressed in terms of "how many
    /// columns in from the left margin is this token", which is one less
    /// than the one-based column an editor shows.
    #[must_use]
    pub const fn indent(&self) -> usize {
        self.column - 1
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
/// A span is what turns "something is wrong with this document" into a
/// caret under the offending characters — see [`Span::render`], which is the
/// renderer `astrs-manifest` will call when it prints a parse failure next
/// to the source line it came from.
///
/// # Examples
///
/// ```
/// use astrs_yaml::{Position, Span};
///
/// let span = Span::new(Position::new(2, 3), Position::new(2, 6));
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
    /// rather than an extent (an unexpected character, an unclosed bracket's
    /// opening brace).
    #[must_use]
    pub const fn point(position: Position) -> Self {
        Self {
            start: position,
            end: position,
        }
    }

    /// True when both ends sit on the same source line, which is the only
    /// case [`Span::render`] can draw a caret for.
    #[must_use]
    pub const fn is_single_line(&self) -> bool {
        self.start.line == self.end.line
    }

    /// Render the source line this span starts on, with a caret run under
    /// the spanned columns.
    ///
    /// Returns `None` when `source` has no such line — which happens only if
    /// the span and the text were not produced from the same document.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_yaml::{Position, Span};
    ///
    /// let source = "name: demo\nnodes: [\n";
    /// let span = Span::new(Position::new(2, 8), Position::new(2, 9));
    /// let rendered = span.render(source).expect("line 2 exists");
    /// assert_eq!(rendered, "nodes: [\n       ^");
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
    fn a_document_starts_at_one_one() {
        assert_eq!(Position::start(), Position::new(1, 1));
        assert_eq!(Position::default(), Position::start());
        assert_eq!(Position::start().indent(), 0);
    }

    #[test]
    fn a_newline_resets_the_column_and_bumps_the_line() {
        let mut position = Position::start();
        position.advance('a');
        position.advance('\n');
        assert_eq!(position, Position::new(2, 1));
    }

    #[test]
    fn a_multi_byte_scalar_advances_exactly_one_column() {
        let mut position = Position::start();
        position.advance('あ');
        assert_eq!(position, Position::new(1, 2));
    }

    #[test]
    fn display_is_the_editor_style_line_colon_column() {
        assert_eq!(Position::new(3, 17).to_string(), "3:17");
    }

    #[test]
    fn positions_order_by_line_then_column() {
        assert!(Position::new(1, 9) < Position::new(2, 1));
        assert!(Position::new(2, 1) < Position::new(2, 2));
    }

    #[test]
    fn a_point_span_renders_a_single_caret() {
        let span = Span::point(Position::new(1, 4));
        assert_eq!(span.render("abcdef").expect("line 1"), "abcdef\n   ^");
    }

    #[test]
    fn a_wide_span_renders_a_caret_run() {
        let span = Span::new(Position::new(1, 2), Position::new(1, 5));
        assert_eq!(span.render("abcdef").expect("line 1"), "abcdef\n ^^^");
    }

    #[test]
    fn a_multi_line_span_renders_one_caret_on_its_first_line() {
        let span = Span::new(Position::new(1, 2), Position::new(3, 5));
        assert!(!span.is_single_line());
        assert_eq!(span.render("abc\ndef\nghi").expect("line 1"), "abc\n ^");
    }

    #[test]
    fn rendering_a_line_that_does_not_exist_is_none() {
        assert!(Span::point(Position::new(9, 1)).render("abc").is_none());
        assert!(Span::point(Position::new(0, 1)).render("abc").is_none());
    }

    #[test]
    fn a_span_displays_as_its_start() {
        let span = Span::new(Position::new(4, 2), Position::new(9, 9));
        assert_eq!(span.to_string(), "4:2");
        assert_eq!(
            Span::from(Position::new(1, 1)),
            Span::point(Position::start())
        );
    }
}
