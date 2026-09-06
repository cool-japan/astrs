//! A character cursor over one normalized YAML document, carrying the
//! [`Position`] every diagnostic in this crate needs.
//!
//! The cursor is [`Copy`], which is what makes YAML's mandatory lookahead
//! affordable: deciding whether a line is `key: value` or a plain scalar
//! means walking the line once with a throwaway copy and then walking it
//! again for real, with no allocation and no borrow gymnastics in between.
//!
//! # Normalization happens before the cursor sees anything
//!
//! [`normalize`] folds `\r\n` and lone `\r` into `\n` and drops a leading
//! byte-order mark, borrowing the input unchanged when — as for every file
//! in this workspace — there is nothing to fold. Everything downstream can
//! then treat `'\n'` as *the* line break instead of re-deriving YAML 1.2's
//! break set at every call site.

use std::borrow::Cow;

use crate::position::Position;

/// The byte-order mark, which is legal at the start of a YAML stream and
/// carries no content.
const BOM: char = '\u{feff}';

/// Fold YAML 1.2 line breaks to `'\n'` and strip a leading BOM.
///
/// Returns [`Cow::Borrowed`] when the input already satisfies both, which is
/// the overwhelmingly common case and costs one scan and no allocation.
#[must_use]
pub(crate) fn normalize(input: &str) -> Cow<'_, str> {
    let trimmed = input.strip_prefix(BOM).unwrap_or(input);
    if !trimmed.contains('\r') {
        return Cow::Borrowed(trimmed);
    }
    let mut out = String::with_capacity(trimmed.len());
    let mut chars = trimmed.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(ch);
        }
    }
    Cow::Owned(out)
}

/// A position-tracking cursor over normalized YAML source.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Cursor<'a> {
    source: &'a str,
    offset: usize,
    position: Position,
}

impl<'a> Cursor<'a> {
    /// Start at the beginning of `source`, which must already be normalized.
    pub(crate) fn new(source: &'a str) -> Self {
        Self {
            source,
            offset: 0,
            position: Position::start(),
        }
    }

    /// True when nothing is left to read.
    pub(crate) fn is_eof(&self) -> bool {
        self.offset >= self.source.len()
    }

    /// The current one-based position.
    pub(crate) fn position(&self) -> Position {
        self.position
    }

    /// The current zero-based indentation (one less than the column).
    pub(crate) fn indent(&self) -> usize {
        self.position.indent()
    }

    /// The current byte offset into the normalized source.
    pub(crate) fn offset(&self) -> usize {
        self.offset
    }

    /// Everything from the cursor to the end of the source.
    pub(crate) fn rest(&self) -> &'a str {
        // `offset` is only ever advanced by whole characters, so this slice
        // is always on a UTF-8 boundary; `get` keeps it panic-free anyway.
        self.source.get(self.offset..).unwrap_or("")
    }

    /// The text between `start` (a previously recorded offset) and here.
    pub(crate) fn slice_from(&self, start: usize) -> &'a str {
        self.source.get(start..self.offset).unwrap_or("")
    }

    /// The next character, without consuming it.
    pub(crate) fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    /// The character `n` positions ahead, without consuming anything.
    ///
    /// Only ever called with a small `n` (the grammar never needs more than
    /// a couple of characters of lookahead), so the linear walk is cheaper
    /// than maintaining a buffer.
    pub(crate) fn peek_at(&self, n: usize) -> Option<char> {
        self.rest().chars().nth(n)
    }

    /// True when the next character is `expected`.
    pub(crate) fn at(&self, expected: char) -> bool {
        self.peek() == Some(expected)
    }

    /// True when the remaining source starts with `prefix`.
    pub(crate) fn starts_with(&self, prefix: &str) -> bool {
        self.rest().starts_with(prefix)
    }

    /// Consume and return the next character.
    pub(crate) fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.offset += ch.len_utf8();
        self.position.advance(ch);
        Some(ch)
    }

    /// Consume `count` characters, stopping early at end of input.
    pub(crate) fn bump_n(&mut self, count: usize) {
        for _ in 0..count {
            if self.bump().is_none() {
                break;
            }
        }
    }

    /// True when the cursor sits on a line break or at end of input.
    pub(crate) fn at_line_end(&self) -> bool {
        matches!(self.peek(), None | Some('\n'))
    }

    /// True when the cursor sits on a space or a tab.
    pub(crate) fn at_blank(&self) -> bool {
        matches!(self.peek(), Some(' ' | '\t'))
    }

    /// True when the cursor sits on a space, a tab, a line break, or the end
    /// of input — the "blank or end" class YAML's grammar leans on
    /// constantly (`- ` versus `-x`, `: ` versus `:x`).
    pub(crate) fn at_blank_or_end(&self) -> bool {
        self.at_blank() || self.at_line_end()
    }

    /// True when the character `n` ahead is a blank or the end of a line.
    pub(crate) fn blank_or_end_at(&self, n: usize) -> bool {
        matches!(self.peek_at(n), None | Some(' ' | '\t' | '\n'))
    }

    /// Consume spaces and tabs, returning how many characters were skipped.
    pub(crate) fn skip_blanks(&mut self) -> usize {
        let mut skipped = 0;
        while self.at_blank() {
            self.bump();
            skipped += 1;
        }
        skipped
    }

    /// Consume everything up to (but not including) the next line break.
    pub(crate) fn skip_to_line_end(&mut self) {
        while !self.at_line_end() {
            self.bump();
        }
    }

    /// Consume a line break, if the cursor is on one.
    pub(crate) fn skip_line_break(&mut self) -> bool {
        if self.at('\n') {
            self.bump();
            true
        } else {
            false
        }
    }

    /// True when the cursor is at column 1 on a `---` or `...` line.
    ///
    /// Both markers only count at the left margin and only when followed by
    /// a blank or the end of the line, so a scalar like `---nope` or a key
    /// named `...x` is left alone.
    pub(crate) fn at_document_marker(&self) -> bool {
        self.at_marker("---") || self.at_marker("...")
    }

    /// True when the cursor is at column 1 on a line beginning with exactly
    /// `marker`.
    pub(crate) fn at_marker(&self, marker: &str) -> bool {
        self.position.column == 1 && self.starts_with(marker) && self.blank_or_end_at(marker.len())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn normalization_borrows_when_there_is_nothing_to_do() {
        assert!(matches!(normalize("a: 1\n"), Cow::Borrowed("a: 1\n")));
    }

    #[test]
    fn normalization_folds_every_break_spelling() {
        assert_eq!(normalize("a\r\nb\rc\nd"), "a\nb\nc\nd");
        assert_eq!(normalize("\u{feff}a: 1\n"), "a: 1\n");
        assert_eq!(normalize("\u{feff}a\r\n"), "a\n");
        // A BOM anywhere but the front is content, not a marker.
        assert_eq!(normalize("a\u{feff}b"), "a\u{feff}b");
    }

    #[test]
    fn positions_track_lines_and_columns() {
        let mut cursor = Cursor::new("ab\ncd");
        assert_eq!(cursor.position(), Position::new(1, 1));
        cursor.bump();
        cursor.bump();
        assert_eq!(cursor.position(), Position::new(1, 3));
        cursor.bump();
        assert_eq!(cursor.position(), Position::new(2, 1));
        assert_eq!(cursor.indent(), 0);
        assert_eq!(cursor.rest(), "cd");
        assert!(!cursor.is_eof());
        cursor.bump_n(9);
        assert!(cursor.is_eof());
        assert_eq!(cursor.peek(), None);
    }

    #[test]
    fn lookahead_never_consumes() {
        let cursor = Cursor::new("key: value");
        assert_eq!(cursor.peek_at(3), Some(':'));
        assert_eq!(cursor.peek_at(400), None);
        assert!(cursor.at('k'));
        assert!(cursor.starts_with("key"));
        assert!(cursor.blank_or_end_at(4));
        assert!(!cursor.blank_or_end_at(0));
        assert_eq!(cursor.offset(), 0);
    }

    #[test]
    fn blank_skipping_stops_at_content_and_at_breaks() {
        let mut cursor = Cursor::new("  \t x\n");
        assert_eq!(cursor.skip_blanks(), 4);
        assert!(cursor.at('x'));
        assert!(!cursor.at_blank_or_end());
        cursor.bump();
        assert!(cursor.at_line_end());
        assert!(cursor.skip_line_break());
        assert!(!cursor.skip_line_break());
    }

    #[test]
    fn slices_recover_the_consumed_text() {
        let mut cursor = Cursor::new("hello world");
        let start = cursor.offset();
        cursor.bump_n(5);
        assert_eq!(cursor.slice_from(start), "hello");
        cursor.skip_to_line_end();
        assert_eq!(cursor.slice_from(start), "hello world");
    }

    #[test]
    fn document_markers_only_count_at_the_left_margin() {
        assert!(Cursor::new("---\n").at_document_marker());
        assert!(Cursor::new("--- a").at_document_marker());
        assert!(Cursor::new("...").at_document_marker());
        assert!(!Cursor::new("----\n").at_document_marker());
        assert!(!Cursor::new("---nope").at_document_marker());
        let mut indented = Cursor::new(" ---");
        indented.bump();
        assert!(!indented.at_document_marker());
    }

    #[test]
    fn a_multi_byte_character_advances_one_column_but_several_bytes() {
        let mut cursor = Cursor::new("あa");
        cursor.bump();
        assert_eq!(cursor.position(), Position::new(1, 2));
        assert_eq!(cursor.offset(), 3);
        assert_eq!(cursor.rest(), "a");
    }
}
