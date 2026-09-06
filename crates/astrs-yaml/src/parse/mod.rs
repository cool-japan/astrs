//! The recursive-descent parser: source text in, [`Value`] tree out.
//!
//! # Shape of the algorithm
//!
//! There is no separate token stream. YAML's block structure is decided by
//! *columns*, so a token-first design would have to carry indentation through
//! the token type anyway; instead a [`Cursor`] walks the text and a handful
//! of mutually recursive functions decide what they are looking at:
//!
//! - [`Parser::parse_node`] is the dispatcher. Given a cursor sitting on the
//!   first character of a node, it decides between a block sequence, a block
//!   mapping, a flow collection, a block scalar, an alias and a plain or
//!   quoted scalar.
//! - Every block construct carries two indentation numbers: the column its
//!   own entries live at, and `min_indent`, the column that *continuation*
//!   lines must exceed. Keeping them separate is what makes both `- foo` +
//!   a wrapped second line and `key:` + a same-column `- item` work.
//! - Deciding "is this line `key: value`?" needs lookahead, which is why
//!   [`Cursor`] is [`Copy`]: [`Parser::looks_like_block_mapping`] walks a
//!   throwaway copy to the end of the line and the real cursor never moves.
//!
//! # Safety rails
//!
//! [`Parser::enter`] is called on the way into every collection — block and
//! flow alike — and refuses to recurse past [`Limits::max_depth`], so a file
//! of nothing but `[` cannot exhaust the stack. Alias expansion is charged
//! against [`Limits::max_alias_nodes`] *before* the subtree is cloned, which
//! is what stops the billion-laughs bomb at the point it starts multiplying
//! rather than after it has taken the machine's memory.

mod cursor;
mod flow;
mod resolve;
mod scalar;
mod tag;

use std::collections::HashMap;

use crate::error::{Error, ErrorKind, Result, err_at};
use crate::limits::Limits;
use crate::mapping::Mapping;
use crate::position::{Position, Span};
use crate::value::Value;

use cursor::Cursor;
use tag::TagSpec;

pub(crate) use resolve::needs_quoting_to_stay_a_string;

/// Parse every document in `input`.
pub(crate) fn parse_documents(input: &str, limits: Limits) -> Result<Vec<Value>> {
    if input.len() > limits.max_input_bytes {
        return Err(Error::new(ErrorKind::InputTooLarge {
            limit: limits.max_input_bytes,
            found: input.len(),
        }));
    }
    let normalized = cursor::normalize(input);
    reject_control_characters(&normalized)?;
    Parser::new(&normalized, limits).parse_stream()
}

/// True for the characters YAML 1.2's `c-printable` production allows.
///
/// `\r` is absent on purpose: [`cursor::normalize`] has already rewritten
/// every carriage return into a line feed by the time this runs.
const fn is_printable(ch: char) -> bool {
    matches!(ch, '\t' | '\n' | ' '..='\u{7e}' | '\u{a0}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
}

/// Refuse a stream containing a character YAML does not permit.
///
/// Done in one pass up front rather than inside each scalar reader: these
/// characters are illegal *everywhere* in a stream, so checking once is both
/// cheaper and impossible to forget in a new code path. The scan is skipped
/// entirely for the overwhelmingly common all-ASCII-printable input.
fn reject_control_characters(input: &str) -> Result<()> {
    if input
        .bytes()
        .all(|byte| matches!(byte, b'\t' | b'\n' | 0x20..=0x7e))
    {
        return Ok(());
    }
    let mut position = Position::start();
    for ch in input.chars() {
        if !is_printable(ch) {
            return err_at(ErrorKind::ControlCharacter { found: ch }, position);
        }
        position.advance(ch);
    }
    Ok(())
}

/// What a `-` at the enclosing collection's own indentation means for the
/// node currently being parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sibling {
    /// This node is a mapping value, so `key:` followed by a same-column
    /// `- item` starts *this node's* sequence.
    IsChild,
    /// This node is a sequence entry, so a same-column `- ` is the next
    /// entry of the sequence this node already belongs to.
    IsNextEntry,
}

/// Node properties: an anchor to register and a tag to apply.
#[derive(Debug, Clone)]
struct Properties {
    anchor: Option<String>,
    tag: TagSpec,
    /// Whether a `!tag` was actually written, as opposed to `tag` merely
    /// defaulting to [`TagSpec::NonSpecific`]. A bare `!` resolves to
    /// `NonSpecific` too, so the tag alone cannot answer this.
    saw_tag: bool,
}

impl Properties {
    /// True when the node was actually decorated with an anchor or a tag.
    ///
    /// Properties are what make an *empty* node legal in places that
    /// otherwise demand content: `[&a , *a]` is a two-element sequence of
    /// nulls, while the undecorated `[, ]` is a syntax error.
    const fn is_explicit(&self) -> bool {
        self.anchor.is_some() || self.saw_tag
    }
}

/// The parser state for one stream.
struct Parser<'a> {
    cursor: Cursor<'a>,
    limits: Limits,
    depth: usize,
    alias_budget: usize,
    anchors: HashMap<String, Value>,
    tag_handles: HashMap<String, String>,
}

impl<'a> Parser<'a> {
    fn new(source: &'a str, limits: Limits) -> Self {
        Self {
            cursor: Cursor::new(source),
            limits,
            depth: 0,
            alias_budget: limits.max_alias_nodes,
            anchors: HashMap::new(),
            tag_handles: HashMap::new(),
        }
    }

    // ---------------------------------------------------------------- stream

    /// Read every document, `---`/`...` markers and directives included.
    fn parse_stream(&mut self) -> Result<Vec<Value>> {
        let mut documents = Vec::new();
        loop {
            self.skip_to_content()?;
            if self.cursor.is_eof() {
                break;
            }
            let mut saw_directive = false;
            while self.cursor.position().column == 1 && self.cursor.at('%') {
                self.parse_directive()?;
                saw_directive = true;
                self.skip_to_content()?;
            }
            let explicit = self.cursor.at_marker("---");
            if explicit {
                self.cursor.bump_n(3);
            } else if self.cursor.at_marker("...") {
                return err_at(ErrorKind::UnexpectedDocumentEnd, self.cursor.position());
            } else if saw_directive {
                return err_at(
                    ErrorKind::InvalidDirective {
                        name: "YAML".to_owned(),
                        reason: "a directive must be followed by a `---` document start",
                    },
                    self.cursor.position(),
                );
            }
            documents.push(self.parse_document_body(explicit)?);
            self.anchors.clear();
            self.tag_handles.clear();
            self.finish_document()?;
        }
        Ok(documents)
    }

    /// Consume whatever follows a document: a `...` marker, the start of the
    /// next document, or the end of the stream. Anything else is content
    /// that could not be attached to the document that just ended.
    fn finish_document(&mut self) -> Result<()> {
        self.skip_to_content()?;
        if self.cursor.at_marker("...") {
            self.cursor.bump_n(3);
            self.cursor.skip_blanks();
            if self.cursor.at('#') {
                self.cursor.skip_to_line_end();
            }
            if !self.cursor.at_line_end() {
                let found = self.cursor.peek().unwrap_or('\0');
                return err_at(
                    ErrorKind::UnexpectedCharacter {
                        found,
                        context: "a `...` document end marker",
                    },
                    self.cursor.position(),
                );
            }
            return Ok(());
        }
        if self.cursor.is_eof() || self.cursor.at_marker("---") || self.cursor.at('%') {
            return Ok(());
        }
        let found = self.cursor.peek().unwrap_or('\0');
        err_at(
            ErrorKind::UnexpectedCharacter {
                found,
                context: "the end of a document",
            },
            self.cursor.position(),
        )
    }

    /// Parse one document's root node.
    fn parse_document_body(&mut self, explicit: bool) -> Result<Value> {
        if explicit {
            self.cursor.skip_blanks();
            if self.cursor.at('#') {
                self.cursor.skip_to_line_end();
            }
            if !self.cursor.at_line_end() {
                return self.parse_node(-1, Sibling::IsChild);
            }
        }
        self.skip_to_content()?;
        if self.cursor.is_eof() || self.cursor.at_document_marker() {
            return Ok(Value::Null);
        }
        self.parse_node(-1, Sibling::IsChild)
    }

    /// Read one `%YAML`/`%TAG` directive line.
    fn parse_directive(&mut self) -> Result<()> {
        let start = self.cursor.position();
        self.cursor.bump();
        let name = self.read_word().to_owned();
        self.cursor.skip_blanks();
        match name.as_str() {
            "YAML" => {
                let version = self.read_word();
                if !version.starts_with("1.") {
                    return err_at(
                        ErrorKind::InvalidDirective {
                            name,
                            reason: "this parser implements YAML 1.x only",
                        },
                        start,
                    );
                }
            }
            "TAG" => {
                let handle = self.read_word().to_owned();
                self.cursor.skip_blanks();
                let prefix = self.read_word().to_owned();
                if handle.is_empty() || prefix.is_empty() {
                    return err_at(
                        ErrorKind::InvalidDirective {
                            name,
                            reason: "expected `%TAG <handle> <prefix>`",
                        },
                        start,
                    );
                }
                self.tag_handles.insert(handle, prefix);
            }
            // Unknown directives are reserved for future YAML versions and
            // are required to be ignored rather than rejected.
            _ => {}
        }
        self.cursor.skip_to_line_end();
        Ok(())
    }

    /// Read up to the next blank or end of line.
    fn read_word(&mut self) -> &'a str {
        let start = self.cursor.offset();
        while !self.cursor.at_blank_or_end() {
            self.cursor.bump();
        }
        self.cursor.slice_from(start)
    }

    // ------------------------------------------------------------ whitespace

    /// Advance to the next content character, skipping blank lines, comment
    /// lines and leading indentation.
    fn skip_to_content(&mut self) -> Result<()> {
        loop {
            let at_line_start = self.cursor.position().column == 1;
            let mut tab = None;
            while self.cursor.at_blank() {
                if tab.is_none() && self.cursor.at('\t') {
                    tab = Some(self.cursor.position());
                }
                self.cursor.bump();
            }
            if self.cursor.at('#') {
                self.cursor.skip_to_line_end();
            }
            if self.cursor.skip_line_break() {
                continue;
            }
            if self.cursor.is_eof() {
                return Ok(());
            }
            if at_line_start && let Some(position) = tab {
                return err_at(ErrorKind::TabInIndentation, position);
            }
            return Ok(());
        }
    }

    /// True when nothing but a comment separates the cursor from the end of
    /// the line.
    fn at_end_of_line_content(&self) -> bool {
        self.cursor.at_line_end() || self.cursor.at('#')
    }

    // ------------------------------------------------------------ dispatcher

    /// Parse a node whose first character is under the cursor.
    fn parse_node(&mut self, min_indent: isize, sibling: Sibling) -> Result<Value> {
        self.parse_node_with(min_indent, TagSpec::NonSpecific, sibling)
    }

    /// Parse a node, applying `inherited` when the node carries no tag of
    /// its own — which is how `key: !!str` followed by an indented block
    /// still types the block it introduces.
    fn parse_node_with(
        &mut self,
        min_indent: isize,
        inherited: TagSpec,
        sibling: Sibling,
    ) -> Result<Value> {
        let here = self.cursor.indent();
        let start = self.cursor.position();
        let mut properties = self.parse_properties()?;
        // A tag introduced on an earlier line (`key: !Outer` with the node
        // below it) applies to this node too. When this node names its own
        // tag as well, both apply: the inherited one wraps the result.
        let mut outer = None;
        if properties.tag == TagSpec::NonSpecific {
            properties.tag = inherited;
        } else if inherited != TagSpec::NonSpecific {
            outer = Some(inherited);
        }
        if self.at_end_of_line_content() {
            let value = self.parse_trailing_node(min_indent, &properties, start, sibling)?;
            let value = wrap_outer(outer.as_ref(), value);
            self.register_anchor(properties.anchor, &value);
            return Ok(value);
        }
        if properties.is_explicit() && self.cursor.at(':') && self.cursor.blank_or_end_at(1) {
            // `&a: 1` and `!t : 1`. The `:` proves this node is a mapping,
            // and that the anchor or tag we just read decorates its *empty
            // key* — not the mapping the key opens. Registering the anchor
            // against the mapping instead would make `b: *a` clone the whole
            // mapping in place of the null it names.
            let span = Span::point(self.cursor.position());
            let key = tag::apply_to_scalar(&properties.tag, "", true, span)?;
            self.register_anchor(properties.anchor.take(), &key);
            let value = self.parse_block_mapping_from(here, Some(key))?;
            return Ok(wrap_outer(outer.as_ref(), value));
        }
        let mut inline_scalar = false;
        let value = match self.cursor.peek() {
            Some('-') if self.cursor.blank_or_end_at(1) => {
                tag::apply_to_collection(&properties.tag, self.parse_block_sequence(here)?)
            }
            Some('?') if self.cursor.blank_or_end_at(1) => {
                tag::apply_to_collection(&properties.tag, self.parse_block_mapping(here)?)
            }
            Some(_) if self.looks_like_block_mapping() => {
                tag::apply_to_collection(&properties.tag, self.parse_block_mapping(here)?)
            }
            Some('[') => {
                tag::apply_to_collection(&properties.tag, self.parse_flow_sequence(min_indent)?)
            }
            Some('{') => {
                tag::apply_to_collection(&properties.tag, self.parse_flow_mapping(min_indent)?)
            }
            Some('|' | '>') => {
                // A block scalar always ends at a line boundary, so the
                // `a: b: c` check below must not run on it: the `:` it would
                // see belongs to whatever line comes next.
                let (text, span) = self.parse_block_scalar(min_indent)?;
                tag::apply_to_scalar(&properties.tag, &text, false, span)?
            }
            Some('*') => self.parse_alias()?,
            Some('\'') => {
                inline_scalar = true;
                let (text, span) = self.parse_single_quoted()?;
                tag::apply_to_scalar(&properties.tag, &text, false, span)?
            }
            Some('"') => {
                inline_scalar = true;
                let (text, span) = self.parse_double_quoted()?;
                tag::apply_to_scalar(&properties.tag, &text, false, span)?
            }
            Some(found @ ('@' | '`' | ',' | ']' | '}' | '#')) => {
                return err_at(
                    ErrorKind::UnexpectedCharacter {
                        found,
                        context: "a node",
                    },
                    self.cursor.position(),
                );
            }
            Some(_) => {
                inline_scalar = true;
                let (text, span) = self.parse_plain_scalar(min_indent, false);
                tag::apply_to_scalar(&properties.tag, &text, true, span)?
            }
            None => tag::apply_to_scalar(&properties.tag, "", true, Span::point(start))?,
        };
        if inline_scalar {
            self.reject_trailing_mapping_value()?;
        }
        let value = wrap_outer(outer.as_ref(), value);
        self.register_anchor(properties.anchor, &value);
        Ok(value)
    }

    /// The node introduced by a line that ended right after its properties
    /// (or right after a `:` / `-`): its content, if any, lives on the
    /// following lines.
    fn parse_trailing_node(
        &mut self,
        min_indent: isize,
        properties: &Properties,
        start: Position,
        sibling: Sibling,
    ) -> Result<Value> {
        match self.parse_indented_content(min_indent, &properties.tag, sibling)? {
            Some(value) => Ok(value),
            // No content at all: an empty node, which a tag can still type
            // (`key: !!str` is the empty string, not null).
            None => tag::apply_to_scalar(&properties.tag, "", true, Span::point(start)),
        }
    }

    /// Look for a node on the lines after the cursor.
    ///
    /// Returns `None` — without consuming the content it found — when the
    /// next content sits at or left of `min_indent` and therefore belongs to
    /// an enclosing collection rather than to this node.
    fn parse_indented_content(
        &mut self,
        min_indent: isize,
        inherited: &TagSpec,
        sibling: Sibling,
    ) -> Result<Option<Value>> {
        self.skip_to_content()?;
        if self.cursor.is_eof() || self.cursor.at_document_marker() {
            return Ok(None);
        }
        let here = self.cursor.indent() as isize;
        if here > min_indent {
            return self
                .parse_node_with(min_indent, inherited.clone(), sibling)
                .map(Some);
        }
        // A block sequence may sit at the same column as the *key* that
        // introduces it — the one place YAML lets a child share its parent's
        // indentation. It is only ever a child there: after a `-`, a marker
        // at the same column is the next entry of the sequence we are
        // already in, and `- \n- b` is two entries, not one nested sequence.
        if sibling == Sibling::IsChild
            && here == min_indent
            && self.cursor.at('-')
            && self.cursor.blank_or_end_at(1)
        {
            let sequence = self.parse_block_sequence(self.cursor.indent())?;
            return Ok(Some(tag::apply_to_collection(inherited, sequence)));
        }
        Ok(None)
    }

    // ------------------------------------------------------ block structures

    /// Parse a block mapping whose keys all sit at column `indent`.
    fn parse_block_mapping(&mut self, indent: usize) -> Result<Value> {
        self.parse_block_mapping_from(indent, None)
    }

    /// Parse a block mapping whose first key may already have been built.
    ///
    /// `first_key` is `Some` only for the `&a: 1` shape, where the caller had
    /// to consume the key's properties before it could tell a mapping was
    /// starting at all.
    fn parse_block_mapping_from(
        &mut self,
        indent: usize,
        mut first_key: Option<Value>,
    ) -> Result<Value> {
        self.enter()?;
        let mut mapping = Mapping::new();
        loop {
            let key_position = self.cursor.position();
            let explicit =
                first_key.is_none() && self.cursor.at('?') && self.cursor.blank_or_end_at(1);
            let key = if let Some(key) = first_key.take() {
                key
            } else if explicit {
                self.cursor.bump();
                self.parse_marker_node(indent, Sibling::IsChild)?
            } else {
                self.parse_simple_key(indent)?
            };
            if explicit {
                self.skip_to_content()?;
            } else {
                self.cursor.skip_blanks();
            }
            let value = if self.cursor.at(':')
                && self.cursor.blank_or_end_at(1)
                && (!explicit || self.cursor.indent() == indent)
            {
                self.cursor.bump();
                self.parse_marked_value(indent)?
            } else if explicit {
                Value::Null
            } else {
                let found = self.cursor.peek().unwrap_or('\0');
                return err_at(
                    ErrorKind::UnexpectedCharacter {
                        found,
                        context: "a block mapping (expected `:` after the key)",
                    },
                    self.cursor.position(),
                );
            };
            self.insert_unique(&mut mapping, key, value, key_position)?;
            self.skip_to_content()?;
            if self.cursor.is_eof() || self.cursor.at_document_marker() {
                break;
            }
            let here = self.cursor.indent();
            if here < indent {
                break;
            }
            if here > indent {
                return err_at(
                    ErrorKind::InvalidIndentation {
                        expected: indent,
                        found: here,
                    },
                    self.cursor.position(),
                );
            }
        }
        self.leave();
        Ok(Value::Mapping(mapping))
    }

    /// Parse a block sequence whose `-` markers all sit at column `indent`.
    fn parse_block_sequence(&mut self, indent: usize) -> Result<Value> {
        self.enter()?;
        let mut items = Vec::new();
        loop {
            self.cursor.bump();
            items.push(self.parse_marker_node(indent, Sibling::IsNextEntry)?);
            self.skip_to_content()?;
            if self.cursor.is_eof() || self.cursor.at_document_marker() {
                break;
            }
            let here = self.cursor.indent();
            if here < indent {
                break;
            }
            if here > indent {
                return err_at(
                    ErrorKind::InvalidIndentation {
                        expected: indent,
                        found: here,
                    },
                    self.cursor.position(),
                );
            }
            if !(self.cursor.at('-') && self.cursor.blank_or_end_at(1)) {
                break;
            }
        }
        self.leave();
        Ok(Value::Sequence(items))
    }

    /// The value of a `key:` pair (or of an explicit `?` key).
    ///
    /// Block collections are not allowed to start on this line: `a: b: c`
    /// and `a: - 1` are both errors, reported where they happen.
    fn parse_marked_value(&mut self, parent_indent: usize) -> Result<Value> {
        let min_indent = parent_indent as isize;
        self.cursor.skip_blanks();
        let start = self.cursor.position();
        if self.at_end_of_line_content() {
            return Ok(self
                .parse_indented_content(min_indent, &TagSpec::NonSpecific, Sibling::IsChild)?
                .unwrap_or(Value::Null));
        }
        let properties = self.parse_properties()?;
        if self.at_end_of_line_content() {
            let value =
                self.parse_trailing_node(min_indent, &properties, start, Sibling::IsChild)?;
            self.register_anchor(properties.anchor, &value);
            return Ok(value);
        }
        let mut inline_scalar = false;
        let value = match self.cursor.peek() {
            Some('-') if self.cursor.blank_or_end_at(1) => {
                return err_at(
                    ErrorKind::UnexpectedCharacter {
                        found: '-',
                        context: "a mapping value (a block sequence must start on its own line)",
                    },
                    self.cursor.position(),
                );
            }
            Some('[') => {
                tag::apply_to_collection(&properties.tag, self.parse_flow_sequence(min_indent)?)
            }
            Some('{') => {
                tag::apply_to_collection(&properties.tag, self.parse_flow_mapping(min_indent)?)
            }
            Some('|' | '>') => {
                // A block scalar always ends at a line boundary, so the
                // `a: b: c` check below must not run on it: the `:` it would
                // see belongs to whatever line comes next.
                let (text, span) = self.parse_block_scalar(min_indent)?;
                tag::apply_to_scalar(&properties.tag, &text, false, span)?
            }
            Some('*') => self.parse_alias()?,
            Some('\'') => {
                inline_scalar = true;
                let (text, span) = self.parse_single_quoted()?;
                tag::apply_to_scalar(&properties.tag, &text, false, span)?
            }
            Some('"') => {
                inline_scalar = true;
                let (text, span) = self.parse_double_quoted()?;
                tag::apply_to_scalar(&properties.tag, &text, false, span)?
            }
            Some(found @ ('@' | '`' | ',' | ']' | '}')) => {
                return err_at(
                    ErrorKind::UnexpectedCharacter {
                        found,
                        context: "a mapping value",
                    },
                    self.cursor.position(),
                );
            }
            Some(_) => {
                inline_scalar = true;
                let (text, span) = self.parse_plain_scalar(min_indent, false);
                tag::apply_to_scalar(&properties.tag, &text, true, span)?
            }
            None => Value::Null,
        };
        if inline_scalar {
            self.reject_trailing_mapping_value()?;
        }
        self.register_anchor(properties.anchor, &value);
        Ok(value)
    }

    /// The node that follows a `-` or `?` marker, which — unlike a mapping
    /// value — may start a compact block collection on the same line:
    /// `- id: camera` and `? - 1` are both one marker plus a nested
    /// collection whose first line shares the marker's.
    fn parse_marker_node(&mut self, marker_indent: usize, sibling: Sibling) -> Result<Value> {
        self.cursor.skip_blanks();
        if self.at_end_of_line_content() {
            return Ok(self
                .parse_indented_content(marker_indent as isize, &TagSpec::NonSpecific, sibling)?
                .unwrap_or(Value::Null));
        }
        self.parse_node(marker_indent as isize, sibling)
    }

    /// A mapping key written on one line, in any style a key may take.
    fn parse_simple_key(&mut self, indent: usize) -> Result<Value> {
        let min_indent = indent as isize;
        let properties = self.parse_properties()?;
        let value = match self.cursor.peek() {
            Some('\'') => {
                let (text, span) = self.parse_single_quoted()?;
                tag::apply_to_scalar(&properties.tag, &text, false, span)?
            }
            Some('"') => {
                let (text, span) = self.parse_double_quoted()?;
                tag::apply_to_scalar(&properties.tag, &text, false, span)?
            }
            Some('[') => {
                tag::apply_to_collection(&properties.tag, self.parse_flow_sequence(min_indent)?)
            }
            Some('{') => {
                tag::apply_to_collection(&properties.tag, self.parse_flow_mapping(min_indent)?)
            }
            Some('*') => self.parse_alias()?,
            None => return err_at(ErrorKind::ExpectedNodeContent, self.cursor.position()),
            Some(_) => {
                let (text, span) = self.parse_plain_scalar(min_indent, false);
                tag::apply_to_scalar(&properties.tag, &text, true, span)?
            }
        };
        self.register_anchor(properties.anchor, &value);
        Ok(value)
    }

    /// Report `a: b: c`, where a scalar value is followed by another `:`.
    fn reject_trailing_mapping_value(&mut self) -> Result<()> {
        self.cursor.skip_blanks();
        if self.cursor.at(':') && self.cursor.blank_or_end_at(1) {
            return err_at(ErrorKind::MappingValueNotAllowed, self.cursor.position());
        }
        Ok(())
    }

    /// Walk a throwaway copy of the cursor to decide whether this line is a
    /// `key: value` pair.
    fn looks_like_block_mapping(&self) -> bool {
        let mut probe = self.cursor;
        let start = probe.offset();
        let mut previous_was_blank = false;
        loop {
            // Quotes and brackets only open a node at the *start* of the
            // key. Anywhere else they are ordinary characters of a plain
            // scalar — `a"b: 1` and `a[b: 1` are both mappings whose key
            // contains the character, not malformed quoting.
            let at_start = probe.offset() == start;
            match probe.peek() {
                None | Some('\n') => return false,
                Some('#') if previous_was_blank => return false,
                Some(':') if probe.blank_or_end_at(1) => return true,
                Some(quote @ ('\'' | '"')) if at_start => {
                    if !skip_quoted(&mut probe, quote) {
                        return false;
                    }
                    previous_was_blank = false;
                }
                Some('[' | '{') if at_start => {
                    if !skip_flow_collection(&mut probe) {
                        return false;
                    }
                    previous_was_blank = false;
                }
                Some(ch) => {
                    previous_was_blank = ch == ' ' || ch == '\t';
                    probe.bump();
                }
            }
        }
    }

    // ------------------------------------------------- properties & anchors

    /// Read a node's `&anchor` and `!tag`, in either order.
    fn parse_properties(&mut self) -> Result<Properties> {
        let mut anchor = None;
        let mut tag = TagSpec::NonSpecific;
        let mut saw_tag = false;
        loop {
            match self.cursor.peek() {
                Some('&') if anchor.is_none() => {
                    self.cursor.bump();
                    anchor = Some(self.read_anchor_name()?.to_owned());
                }
                Some('!') if !saw_tag => {
                    tag = self.read_tag()?;
                    saw_tag = true;
                }
                _ => break,
            }
            if self.cursor.at_blank() {
                self.cursor.skip_blanks();
            } else {
                break;
            }
        }
        Ok(Properties {
            anchor,
            tag,
            saw_tag,
        })
    }

    /// Read the name of an `&anchor` or `*alias`.
    ///
    /// The name stops at the first character outside `[0-9A-Za-z_-]`, which
    /// is narrower than YAML's own `ns-anchor-char` and deliberately so: it
    /// is what `serde_yaml` accepts, and it is the only reading under which
    /// `&base: 1` is the mapping `{null: 1}` rather than the scalar `1`
    /// carrying an anchor absurdly named `base:`. Getting this wrong loses
    /// the mapping silently, with no error to notice.
    fn read_anchor_name(&mut self) -> Result<&'a str> {
        let start = self.cursor.offset();
        while self
            .cursor
            .peek()
            .is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        {
            self.cursor.bump();
        }
        let name = self.cursor.slice_from(start);
        if name.is_empty() {
            return err_at(
                ErrorKind::UnexpectedCharacter {
                    found: self.cursor.peek().unwrap_or('\0'),
                    context: "an anchor name",
                },
                self.cursor.position(),
            );
        }
        Ok(name)
    }

    /// Read a tag in any of its four spellings and resolve it to a
    /// [`TagSpec`].
    fn read_tag(&mut self) -> Result<TagSpec> {
        let start = self.cursor.position();
        self.cursor.bump();
        if self.cursor.at('<') {
            self.cursor.bump();
            let uri_start = self.cursor.offset();
            while !self.cursor.at('>') && !self.cursor.at_line_end() {
                self.cursor.bump();
            }
            if !self.cursor.at('>') {
                return err_at(
                    ErrorKind::InvalidTag {
                        reason: "a verbatim `!<...>` tag is missing its closing `>`",
                    },
                    start,
                );
            }
            let uri = self.cursor.slice_from(uri_start);
            if uri.is_empty() {
                return err_at(
                    ErrorKind::InvalidTag {
                        reason: "a verbatim `!<...>` tag cannot be empty",
                    },
                    start,
                );
            }
            let spec = tag::classify(uri);
            self.cursor.bump();
            return Ok(spec);
        }
        if self.cursor.at_blank_or_end() {
            return Ok(TagSpec::NonSpecific);
        }
        if self.cursor.at('!') {
            self.cursor.bump();
            let suffix = self.read_tag_text();
            if suffix.is_empty() {
                return err_at(
                    ErrorKind::InvalidTag {
                        reason: "the `!!` handle needs a suffix",
                    },
                    start,
                );
            }
            let prefix = self
                .tag_handles
                .get("!!")
                .map_or(tag::SECONDARY_HANDLE_PREFIX, String::as_str);
            return Ok(tag::classify(&format!("{prefix}{suffix}")));
        }
        let text = self.read_tag_text();
        // A *named* handle is `!`, word characters, `!` — nothing else. The
        // `!` in `!:!bool` therefore does not open one: that tag is the
        // primary handle carrying the suffix `:!bool`, i.e. an ordinary
        // local tag. Splitting on the first `!` regardless would reject a
        // document `serde_yaml` reads happily.
        let named_handle = text
            .find('!')
            .filter(|split| text[..*split].chars().all(Self::is_tag_word_char));
        if let Some(split) = named_handle {
            let handle = format!("!{}!", &text[..split]);
            let suffix = &text[split + 1..];
            let Some(prefix) = self.tag_handles.get(&handle) else {
                return err_at(ErrorKind::UnknownTagHandle { handle }, start);
            };
            return Ok(tag::classify(&format!("{prefix}{suffix}")));
        }
        match self.tag_handles.get("!") {
            Some(prefix) => Ok(tag::classify(&format!("{prefix}{text}"))),
            None => Ok(tag::classify(&format!("!{text}"))),
        }
    }

    /// True for YAML's `ns-word-char`, the only characters a *named* tag
    /// handle (`!handle!suffix`) may be spelled with.
    const fn is_tag_word_char(ch: char) -> bool {
        ch.is_ascii_alphanumeric() || ch == '-'
    }

    fn read_tag_text(&mut self) -> &'a str {
        let start = self.cursor.offset();
        while !self.cursor.at_blank_or_end()
            && !matches!(self.cursor.peek(), Some(',' | '[' | ']' | '{' | '}'))
        {
            self.cursor.bump();
        }
        self.cursor.slice_from(start)
    }

    /// Resolve a `*alias`, charging the cloned subtree to the alias budget
    /// *before* cloning it.
    fn parse_alias(&mut self) -> Result<Value> {
        let start = self.cursor.position();
        self.cursor.bump();
        let name = self.read_anchor_name()?.to_owned();
        let Some(anchored) = self.anchors.get(&name) else {
            return err_at(ErrorKind::UnknownAnchor { name }, start);
        };
        let cost = anchored.node_count();
        if cost > self.alias_budget {
            return err_at(
                ErrorKind::AliasBudgetExhausted {
                    limit: self.limits.max_alias_nodes,
                },
                start,
            );
        }
        let expanded = anchored.clone();
        self.alias_budget -= cost;
        Ok(expanded)
    }

    fn register_anchor(&mut self, anchor: Option<String>, value: &Value) {
        if let Some(name) = anchor {
            self.anchors.insert(name, value.clone());
        }
    }

    fn insert_unique(
        &self,
        mapping: &mut Mapping,
        key: Value,
        value: Value,
        position: Position,
    ) -> Result<()> {
        if mapping.contains_key(&key) {
            return err_at(
                ErrorKind::DuplicateKey {
                    key: render_key(&key),
                },
                position,
            );
        }
        mapping.insert(key, value);
        Ok(())
    }

    // ------------------------------------------------------------- recursion

    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > self.limits.max_depth {
            return err_at(
                ErrorKind::DepthLimitExceeded {
                    limit: self.limits.max_depth,
                },
                self.cursor.position(),
            );
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }
}

/// Apply a tag inherited from an enclosing line on top of a node that
/// already carries one of its own.
fn wrap_outer(outer: Option<&TagSpec>, value: Value) -> Value {
    match outer {
        Some(spec) => tag::apply_to_collection(spec, value),
        None => value,
    }
}

/// A mapping key, rendered short enough for a one-line error message.
fn render_key(key: &Value) -> String {
    match key {
        Value::String(text) => text.clone(),
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(number) => number.to_string(),
        Value::Sequence(_) => "<sequence>".to_owned(),
        Value::Mapping(_) => "<mapping>".to_owned(),
        Value::Tagged(tagged) => format!("{} {}", tagged.tag, render_key(&tagged.value)),
    }
}

/// Skip a quoted scalar during lookahead, refusing to cross a line break.
fn skip_quoted(probe: &mut Cursor<'_>, quote: char) -> bool {
    probe.bump();
    loop {
        match probe.peek() {
            None | Some('\n') => return false,
            Some('\\') if quote == '"' => {
                probe.bump();
                if probe.at_line_end() {
                    return false;
                }
                probe.bump();
            }
            Some(ch) if ch == quote => {
                probe.bump();
                if quote == '\'' && probe.at('\'') {
                    probe.bump();
                    continue;
                }
                return true;
            }
            Some(_) => {
                probe.bump();
            }
        }
    }
}

/// Skip a balanced flow collection during lookahead.
fn skip_flow_collection(probe: &mut Cursor<'_>) -> bool {
    let mut depth = 0usize;
    loop {
        match probe.peek() {
            None | Some('\n') => return false,
            Some('[' | '{') => {
                depth += 1;
                probe.bump();
            }
            Some(']' | '}') => {
                probe.bump();
                depth -= 1;
                if depth == 0 {
                    return true;
                }
            }
            Some(quote @ ('\'' | '"')) => {
                if !skip_quoted(probe, quote) {
                    return false;
                }
            }
            Some(_) => {
                probe.bump();
            }
        }
    }
}

#[cfg(test)]
mod tests;
