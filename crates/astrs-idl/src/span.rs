//! Source positions and spans.
//!
//! The `.msg`/`.srv`/`.action` grammar (blueprint §10.3) is line-oriented:
//! [`crate::lexer::classify_line`] resets its column at every `\n` and the
//! parser never looks across a line boundary except for the `---` section
//! separators, which [`crate::parser`] recognises before tokenizing. Every
//! [`Span`] produced by this crate is therefore built from one line, but the
//! type itself stays two-position-general rather than "one line plus a
//! column range" — a future multi-line construct (a block comment, say)
//! costs nothing to add on top of it.
//!
//! Both fields of [`Position`] are 1-based, matching how editors and `rustc`
//! report locations, so a [`Position`] can be printed directly into a
//! `file.msg:12:5`-shaped diagnostic without an off-by-one adjustment.

use std::fmt;

/// A 1-based line/column location within one source file.
///
/// `line` and `column` count *characters*, not bytes: a field name after a
/// multi-byte UTF-8 comment still reports the column a human counts when
/// looking at the line, matching `rustc`'s own convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position {
    /// 1-based line number.
    pub line: u32,
    /// 1-based column number, in characters.
    pub column: u32,
}

impl Position {
    /// The start of a file: line 1, column 1.
    pub const START: Self = Self { line: 1, column: 1 };

    /// Builds a position from its 1-based coordinates.
    #[must_use]
    pub const fn new(line: u32, column: u32) -> Self {
        Self { line, column }
    }

    /// This position advanced by `columns` characters on the same line.
    #[must_use]
    pub const fn advance(self, columns: u32) -> Self {
        Self {
            line: self.line,
            column: self.column.saturating_add(columns),
        }
    }
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.line, self.column)
    }
}

/// A half-open range of source text, `[start, end)` in [`Position`] terms.
///
/// `end` is the position one character past the span's last character, so an
/// empty span (`start == end`) is representable and a span's character width
/// on one line is `end.column - start.column`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Span {
    /// First character of the span.
    pub start: Position,
    /// One character past the span's last character.
    pub end: Position,
}

impl Span {
    /// A zero-width span at `position`, used for end-of-line/end-of-file
    /// diagnostics that have no offending text to underline.
    #[must_use]
    pub const fn empty(position: Position) -> Self {
        Self {
            start: position,
            end: position,
        }
    }

    /// Builds a span from two positions.
    ///
    /// Does not enforce `start <= end`: a caller building a span from two
    /// independently computed positions gets back exactly what it asked for,
    /// which is more useful for a `#[cfg(test)]` assertion than a silent
    /// swap would be.
    #[must_use]
    pub const fn new(start: Position, end: Position) -> Self {
        Self { start, end }
    }

    /// A span covering `len` characters starting at `start`, on `start`'s
    /// line.
    #[must_use]
    pub const fn from_len(start: Position, len: u32) -> Self {
        Self {
            start,
            end: start.advance(len),
        }
    }

    /// The smallest span covering both `self` and `other`.
    #[must_use]
    pub fn join(self, other: Self) -> Self {
        let start = if self.start <= other.start {
            self.start
        } else {
            other.start
        };
        let end = if self.end >= other.end {
            self.end
        } else {
            other.end
        };
        Self { start, end }
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.start == self.end {
            write!(f, "{}", self.start)
        } else if self.start.line == self.end.line {
            write!(
                f,
                "{}:{}-{}",
                self.start.line, self.start.column, self.end.column
            )
        } else {
            write!(f, "{}-{}", self.start, self.end)
        }
    }
}

/// Pairs a value with the span of source text it came from.
///
/// Used sparingly — most AST nodes carry their own `span: Span` field
/// directly — but handy for the handful of places (a resolved literal, a
/// looked-up type) where the value's own type has no room for one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Spanned<T> {
    /// The value.
    pub value: T,
    /// Where it came from.
    pub span: Span,
}

impl<T> Spanned<T> {
    /// Pairs a value with its span.
    #[must_use]
    pub const fn new(value: T, span: Span) -> Self {
        Self { value, span }
    }

    /// Maps the inner value, keeping the span.
    #[must_use]
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Spanned<U> {
        Spanned {
            value: f(self.value),
            span: self.span,
        }
    }

    /// Borrows the inner value.
    #[must_use]
    pub const fn as_ref(&self) -> Spanned<&T> {
        Spanned {
            value: &self.value,
            span: self.span,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn position_display_is_line_colon_column() {
        assert_eq!(Position::new(12, 5).to_string(), "12:5");
        assert_eq!(Position::START.to_string(), "1:1");
    }

    #[test]
    fn position_advance_stays_on_the_same_line() {
        let p = Position::new(3, 10).advance(4);
        assert_eq!(p, Position::new(3, 14));
    }

    #[test]
    fn position_ordering_is_line_major() {
        assert!(Position::new(1, 99) < Position::new(2, 1));
        assert!(Position::new(2, 1) < Position::new(2, 2));
    }

    #[test]
    fn span_from_len_covers_the_expected_width() {
        let span = Span::from_len(Position::new(4, 1), 5);
        assert_eq!(span.start, Position::new(4, 1));
        assert_eq!(span.end, Position::new(4, 6));
    }

    #[test]
    fn empty_span_displays_as_a_single_position() {
        let span = Span::empty(Position::new(7, 3));
        assert_eq!(span.to_string(), "7:3");
    }

    #[test]
    fn same_line_span_displays_as_a_column_range() {
        let span = Span::new(Position::new(2, 1), Position::new(2, 9));
        assert_eq!(span.to_string(), "2:1-9");
    }

    #[test]
    fn cross_line_span_displays_as_two_positions() {
        let span = Span::new(Position::new(2, 1), Position::new(4, 3));
        assert_eq!(span.to_string(), "2:1-4:3");
    }

    #[test]
    fn join_takes_the_widest_extent() {
        let a = Span::new(Position::new(2, 5), Position::new(2, 9));
        let b = Span::new(Position::new(2, 1), Position::new(2, 6));
        let joined = a.join(b);
        assert_eq!(joined.start, Position::new(2, 1));
        assert_eq!(joined.end, Position::new(2, 9));
    }

    #[test]
    fn spanned_map_preserves_the_span() {
        let s = Spanned::new(41, Span::empty(Position::new(1, 1)));
        let mapped = s.map(|v| v + 1);
        assert_eq!(mapped.value, 42);
        assert_eq!(mapped.span, s.span);
    }

    #[test]
    fn spanned_as_ref_borrows() {
        let s = Spanned::new(String::from("hi"), Span::empty(Position::new(1, 1)));
        let borrowed = s.as_ref();
        assert_eq!(borrowed.value, "hi");
    }
}
