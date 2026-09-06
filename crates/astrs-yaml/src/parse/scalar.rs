//! The four scalar styles: plain, single-quoted, double-quoted, and the two
//! block forms (`|` literal and `>` folded).
//!
//! # Line folding, once
//!
//! Plain and quoted scalars share YAML's folding rule and implement it in
//! the same shape: *k* consecutive line breaks between two pieces of content
//! become one space when `k == 1` and `k - 1` newlines otherwise, with the
//! whitespace on both sides of each break discarded. That single rule is why
//!
//! ```text
//! description: a long sentence
//!   continued on the next line
//! ```
//!
//! reads back as one string with a single space in it, and why a blank line
//! in the middle of one becomes a real paragraph break.
//!
//! Block scalars fold differently — `|` does not fold at all, and `>` leaves
//! more-indented lines alone — so they get their own pass over the collected
//! lines rather than sharing the quoted-scalar path.

use crate::error::{ErrorKind, Result, err_at};
use crate::position::Span;

use super::Parser;

/// What the trailing line breaks of a block scalar become.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chomping {
    /// `-`: drop every trailing break.
    Strip,
    /// the default: keep exactly one trailing break, if there is content.
    Clip,
    /// `+`: keep every trailing break.
    Keep,
}

impl<'a> Parser<'a> {
    /// Read one line's worth of a plain scalar, stopping at the first
    /// character that ends it, and return the text with trailing blanks
    /// removed.
    ///
    /// In block context a plain scalar ends at a line break, at a `#` that
    /// follows whitespace, or at a `:` that is followed by whitespace — the
    /// last of which is what makes `a: b: c` a *reported* error instead of a
    /// silently accepted string. In flow context it additionally ends at any
    /// of `,`, `[`, `]`, `{`, `}`.
    pub(super) fn read_plain_segment(&mut self, flow: bool) -> &'a str {
        let start = self.cursor.offset();
        let mut previous_was_blank = false;
        while let Some(ch) = self.cursor.peek() {
            match ch {
                '\n' => break,
                '#' if previous_was_blank => break,
                ':' if self.cursor.blank_or_end_at(1) => break,
                ':' if flow && matches!(self.cursor.peek_at(1), Some(',' | ']' | '}')) => break,
                ',' | '[' | ']' | '{' | '}' if flow => break,
                _ => {
                    previous_was_blank = ch == ' ' || ch == '\t';
                    self.cursor.bump();
                }
            }
        }
        self.cursor.slice_from(start).trim_end_matches([' ', '\t'])
    }

    /// Read a plain scalar, following its continuation lines.
    ///
    /// A continuation line must be indented strictly more than
    /// `min_indent` — the enclosing block collection's indentation — which
    /// is what stops the scalar at the next sibling key instead of eating
    /// the rest of the document.
    pub(super) fn parse_plain_scalar(&mut self, min_indent: isize, flow: bool) -> (String, Span) {
        let start = self.cursor.position();
        let mut text = self.read_plain_segment(flow).to_owned();
        loop {
            let checkpoint = self.cursor;
            let breaks = self.consume_folded_breaks();
            if breaks == 0
                || self.cursor.is_eof()
                || self.cursor.at_document_marker()
                || self.cursor.at('#')
                || (self.cursor.indent() as isize) <= min_indent
            {
                self.cursor = checkpoint;
                break;
            }
            let segment = self.read_plain_segment(flow);
            if segment.is_empty() {
                self.cursor = checkpoint;
                break;
            }
            push_fold(&mut text, breaks);
            text.push_str(segment);
            if !flow && self.cursor.at(':') {
                // `key: a\n  b: c` — the caller turns this into a
                // `MappingValueNotAllowed` with the colon's position.
                break;
            }
        }
        (text, Span::new(start, self.cursor.position()))
    }

    /// Consume a run of line breaks together with the blank space around
    /// them, returning how many breaks were crossed.
    fn consume_folded_breaks(&mut self) -> usize {
        let mut breaks = 0;
        loop {
            self.cursor.skip_blanks();
            if self.cursor.skip_line_break() {
                breaks += 1;
            } else {
                return breaks;
            }
        }
    }

    /// Read a `'single-quoted'` scalar, in which the only escape is `''`.
    pub(super) fn parse_single_quoted(&mut self) -> Result<(String, Span)> {
        let start = self.cursor.position();
        self.cursor.bump();
        let mut text = String::new();
        loop {
            match self.cursor.peek() {
                None => {
                    return err_at(
                        ErrorKind::UnexpectedEndOfInput {
                            context: "a single-quoted scalar",
                        },
                        self.cursor.position(),
                    );
                }
                Some('\'') => {
                    self.cursor.bump();
                    if self.cursor.at('\'') {
                        self.cursor.bump();
                        text.push('\'');
                    } else {
                        break;
                    }
                }
                Some('\n') => self.fold_quoted_break(&mut text),
                Some(ch) => {
                    self.cursor.bump();
                    text.push(ch);
                }
            }
        }
        Ok((text, Span::new(start, self.cursor.position())))
    }

    /// Read a `"double-quoted"` scalar, with the full escape table.
    pub(super) fn parse_double_quoted(&mut self) -> Result<(String, Span)> {
        let start = self.cursor.position();
        self.cursor.bump();
        let mut text = String::new();
        loop {
            match self.cursor.peek() {
                None => {
                    return err_at(
                        ErrorKind::UnexpectedEndOfInput {
                            context: "a double-quoted scalar",
                        },
                        self.cursor.position(),
                    );
                }
                Some('"') => {
                    self.cursor.bump();
                    break;
                }
                Some('\\') => self.read_escape(&mut text)?,
                Some('\n') => self.fold_quoted_break(&mut text),
                Some(ch) => {
                    self.cursor.bump();
                    text.push(ch);
                }
            }
        }
        Ok((text, Span::new(start, self.cursor.position())))
    }

    /// Fold the line break the cursor is sitting on into `text`.
    fn fold_quoted_break(&mut self, text: &mut String) {
        while text.ends_with(' ') || text.ends_with('\t') {
            text.pop();
        }
        let breaks = self.consume_folded_breaks();
        push_fold(text, breaks);
    }

    /// Consume one `\`-escape and append what it stands for.
    fn read_escape(&mut self, text: &mut String) -> Result<()> {
        let escape_start = self.cursor.position();
        self.cursor.bump();
        let Some(ch) = self.cursor.bump() else {
            return err_at(
                ErrorKind::UnexpectedEndOfInput {
                    context: "an escape sequence",
                },
                self.cursor.position(),
            );
        };
        let simple = match ch {
            '0' => Some('\0'),
            'a' => Some('\u{7}'),
            'b' => Some('\u{8}'),
            't' | '\t' => Some('\t'),
            'n' => Some('\n'),
            'v' => Some('\u{b}'),
            'f' => Some('\u{c}'),
            'r' => Some('\r'),
            'e' => Some('\u{1b}'),
            ' ' => Some(' '),
            '"' => Some('"'),
            '/' => Some('/'),
            '\\' => Some('\\'),
            'N' => Some('\u{85}'),
            '_' => Some('\u{a0}'),
            'L' => Some('\u{2028}'),
            'P' => Some('\u{2029}'),
            _ => None,
        };
        if let Some(resolved) = simple {
            text.push(resolved);
            return Ok(());
        }
        match ch {
            'x' => self.read_hex_escape(2, 'x', escape_start, text),
            'u' => self.read_hex_escape(4, 'u', escape_start, text),
            'U' => self.read_hex_escape(8, 'U', escape_start, text),
            '\n' => {
                // An escaped line break joins the lines with nothing at all,
                // discarding the next line's indentation.
                self.cursor.skip_blanks();
                Ok(())
            }
            other => err_at(ErrorKind::UnknownEscape { found: other }, escape_start),
        }
    }

    fn read_hex_escape(
        &mut self,
        digits: usize,
        kind: char,
        escape_start: crate::position::Position,
        text: &mut String,
    ) -> Result<()> {
        let start = self.cursor.offset();
        let mut value: u32 = 0;
        for _ in 0..digits {
            let Some(ch) = self.cursor.peek().and_then(|ch| ch.to_digit(16)) else {
                let literal = format!("{kind}{}", self.cursor.slice_from(start));
                return err_at(ErrorKind::InvalidEscapeValue { literal }, escape_start);
            };
            self.cursor.bump();
            value = value * 16 + ch;
        }
        match char::from_u32(value) {
            Some(resolved) => {
                text.push(resolved);
                Ok(())
            }
            None => err_at(
                ErrorKind::InvalidEscapeValue {
                    literal: format!("{kind}{}", self.cursor.slice_from(start)),
                },
                escape_start,
            ),
        }
    }

    /// Read a `|` literal or `>` folded block scalar.
    ///
    /// `parent_indent` is the enclosing block collection's indentation: the
    /// content must be indented further than that, and an explicit
    /// indentation indicator (`|2`) is counted from it.
    pub(super) fn parse_block_scalar(&mut self, parent_indent: isize) -> Result<(String, Span)> {
        let start = self.cursor.position();
        let folded = self.cursor.at('>');
        self.cursor.bump();
        let (explicit_indent, chomping) = self.read_block_scalar_header()?;
        let (lines, final_break) = self.read_block_scalar_lines(parent_indent, explicit_indent)?;
        let raw = if folded {
            fold_block_lines(&lines)
        } else {
            let mut out = String::new();
            for line in &lines {
                out.push_str(line);
                out.push('\n');
            }
            out
        };
        Ok((
            chomp(raw, chomping, final_break),
            Span::new(start, self.cursor.position()),
        ))
    }

    /// Read the indicators that follow `|`/`>`, then the rest of that line.
    fn read_block_scalar_header(&mut self) -> Result<(Option<usize>, Chomping)> {
        let mut explicit_indent = None;
        let mut chomping = Chomping::Clip;
        for _ in 0..2 {
            match self.cursor.peek() {
                Some(digit @ '1'..='9') if explicit_indent.is_none() => {
                    explicit_indent = Some(digit as usize - '0' as usize);
                    self.cursor.bump();
                }
                Some('-') if chomping == Chomping::Clip => {
                    chomping = Chomping::Strip;
                    self.cursor.bump();
                }
                Some('+') if chomping == Chomping::Clip => {
                    chomping = Chomping::Keep;
                    self.cursor.bump();
                }
                _ => break,
            }
        }
        self.cursor.skip_blanks();
        if self.cursor.at('#') {
            self.cursor.skip_to_line_end();
        }
        if !self.cursor.at_line_end() {
            let found = self.cursor.peek().unwrap_or('\0');
            return err_at(
                ErrorKind::UnexpectedCharacter {
                    found,
                    context: "a block scalar header",
                },
                self.cursor.position(),
            );
        }
        self.cursor.skip_line_break();
        Ok((explicit_indent, chomping))
    }

    /// Collect the block scalar's lines, already stripped of their common
    /// indentation.
    ///
    /// The returned flag says whether the last line the scalar owns was
    /// actually terminated by a line break. A file that ends `a: |\n  x`
    /// with no final newline has one fewer break than the same file with
    /// one, and every chomping mode — `+` included — has to see that
    /// difference rather than being handed a break the input never had.
    fn read_block_scalar_lines(
        &mut self,
        parent_indent: isize,
        explicit_indent: Option<usize>,
    ) -> Result<(Vec<&'a str>, bool)> {
        let base = parent_indent.max(0) as usize;
        let mut content_indent = explicit_indent.map(|extra| base + extra);
        let mut lines: Vec<&'a str> = Vec::new();
        let mut widest_leading_blank = 0usize;
        let mut final_break = true;
        while !self.cursor.is_eof() {
            if self.cursor.at_document_marker() {
                break;
            }
            let mut probe = self.cursor;
            let mut spaces = 0;
            while probe.at(' ') {
                probe.bump();
                spaces += 1;
            }
            let blank = probe.at_line_end();
            if content_indent.is_none() {
                if blank {
                    widest_leading_blank = widest_leading_blank.max(spaces);
                    lines.push("");
                    final_break = self.consume_rest_of_line();
                    continue;
                }
                if (spaces as isize) <= parent_indent {
                    break;
                }
                if widest_leading_blank > spaces {
                    return err_at(
                        ErrorKind::InvalidIndentation {
                            expected: spaces,
                            found: widest_leading_blank,
                        },
                        self.cursor.position(),
                    );
                }
                content_indent = Some(spaces);
            }
            let indent = content_indent.unwrap_or(base);
            if blank {
                lines.push("");
                final_break = self.consume_rest_of_line();
                continue;
            }
            if spaces < indent {
                break;
            }
            self.cursor.bump_n(indent);
            let start = self.cursor.offset();
            self.cursor.skip_to_line_end();
            lines.push(self.cursor.slice_from(start));
            final_break = self.cursor.skip_line_break();
        }
        Ok((lines, final_break))
    }

    /// Consume the rest of the line, reporting whether it ended with a break
    /// rather than with the end of the input.
    fn consume_rest_of_line(&mut self) -> bool {
        self.cursor.skip_to_line_end();
        self.cursor.skip_line_break()
    }
}

/// Append the separator that `breaks` line breaks fold into.
fn push_fold(text: &mut String, breaks: usize) {
    if breaks <= 1 {
        text.push(' ');
    } else {
        for _ in 1..breaks {
            text.push('\n');
        }
    }
}

/// Apply `>` folding to already-stripped block scalar lines.
fn fold_block_lines(lines: &[&str]) -> String {
    let Some(first) = lines.iter().position(|line| !line.is_empty()) else {
        return "\n".repeat(lines.len());
    };
    let mut out = "\n".repeat(first);
    out.push_str(lines[first]);
    let mut previous_more_indented = lines[first].starts_with(' ');
    let mut pending_breaks = 1usize;
    for line in &lines[first + 1..] {
        if line.is_empty() {
            pending_breaks += 1;
            continue;
        }
        let more_indented = line.starts_with(' ');
        if previous_more_indented || more_indented {
            for _ in 0..pending_breaks {
                out.push('\n');
            }
        } else if pending_breaks == 1 {
            out.push(' ');
        } else {
            for _ in 1..pending_breaks {
                out.push('\n');
            }
        }
        out.push_str(line);
        previous_more_indented = more_indented;
        pending_breaks = 1;
    }
    for _ in 0..pending_breaks {
        out.push('\n');
    }
    out
}

/// Apply the chomping indicator to a block scalar's assembled text.
///
/// `final_break` says whether the input really ended the last line with a
/// break. The line assemblers above always write one `\n` per line, so when
/// it is `false` that last break is an artefact and is taken back before
/// chomping runs — otherwise `|+` would keep a newline the file never had,
/// and `|` would clip its way back to one.
fn chomp(mut raw: String, chomping: Chomping, final_break: bool) -> String {
    if !final_break && raw.ends_with('\n') {
        raw.pop();
    }
    match chomping {
        Chomping::Keep => raw,
        Chomping::Strip => {
            while raw.ends_with('\n') {
                raw.pop();
            }
            raw
        }
        Chomping::Clip => {
            while raw.ends_with('\n') {
                raw.pop();
            }
            if final_break && !raw.is_empty() {
                raw.push('\n');
            }
            raw
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn folding_a_single_break_yields_a_space() {
        let mut text = String::from("a");
        push_fold(&mut text, 1);
        assert_eq!(text, "a ");
    }

    #[test]
    fn folding_several_breaks_yields_one_fewer_newline() {
        let mut text = String::from("a");
        push_fold(&mut text, 3);
        assert_eq!(text, "a\n\n");
    }

    #[test]
    fn literal_lines_keep_every_break() {
        assert_eq!(fold_block_lines(&["x"]), "x\n");
    }

    #[test]
    fn folded_lines_join_with_a_space() {
        assert_eq!(fold_block_lines(&["x", "y"]), "x y\n");
        assert_eq!(fold_block_lines(&["x", "", "y"]), "x\ny\n");
        assert_eq!(fold_block_lines(&["x", "", "", "y"]), "x\n\ny\n");
    }

    #[test]
    fn folded_more_indented_lines_are_never_joined() {
        assert_eq!(fold_block_lines(&["x", " y", "z"]), "x\n y\nz\n");
        assert_eq!(fold_block_lines(&[" x", "y"]), " x\ny\n");
    }

    #[test]
    fn folded_leading_and_trailing_blanks_become_breaks() {
        assert_eq!(fold_block_lines(&["", "x"]), "\nx\n");
        assert_eq!(fold_block_lines(&["x", ""]), "x\n\n");
        assert_eq!(fold_block_lines(&["", ""]), "\n\n");
        assert_eq!(fold_block_lines(&[]), "");
    }

    #[test]
    fn chomping_covers_all_three_indicators() {
        assert_eq!(chomp("x\n\n\n".into(), Chomping::Strip, true), "x");
        assert_eq!(chomp("x\n\n\n".into(), Chomping::Clip, true), "x\n");
        assert_eq!(chomp("x\n\n\n".into(), Chomping::Keep, true), "x\n\n\n");
        assert_eq!(chomp(String::new(), Chomping::Clip, true), "");
        assert_eq!(chomp("\n\n".into(), Chomping::Clip, true), "");
        assert_eq!(chomp("\n".into(), Chomping::Keep, true), "\n");
    }

    #[test]
    fn a_missing_final_break_is_never_invented() {
        // `a: |\n  x` with no trailing newline: every indicator drops the
        // break the assembler synthesized, `+` included.
        for chomping in [Chomping::Strip, Chomping::Clip, Chomping::Keep] {
            assert_eq!(chomp("x\n".into(), chomping, false), "x", "{chomping:?}");
        }
        assert_eq!(chomp("x\ny\n".into(), Chomping::Clip, false), "x\ny");
        // A blank line before the unterminated end still counts as content
        // for `+`, which is the whole point of keeping the flag separate
        // from "the text ends in a newline".
        assert_eq!(chomp("x\n\n".into(), Chomping::Keep, false), "x\n");
        assert_eq!(chomp("x\n\n".into(), Chomping::Clip, false), "x");
        assert_eq!(chomp(String::new(), Chomping::Clip, false), "");
    }
}
