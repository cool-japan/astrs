//! Flow collections: `[a, b]` and `{a: 1, b: 2}`.
//!
//! Flow context is where YAML looks most like JSON, and where the one rule
//! that trips people up lives: **`:` only separates a key from its value
//! when it is followed by whitespace, or when the key was written
//! "JSON-like"** (quoted, or itself a flow collection). So
//!
//! ```text
//! {a:b}      # one plain scalar key `a:b`, with a null value
//! {"a":1}    # a quoted key `a` with value 1
//! {a: b}     # a plain key `a` with value `b`
//! ```
//!
//! all mean different things, and all three are accepted. Getting this wrong
//! is how a URL in a flow mapping (`{url: http://x}`) turns into nonsense.
//!
//! Flow collections may span lines and may carry comments between their
//! tokens; indentation inside them is not significant, but a `---`/`...`
//! marker still ends the document and is therefore reported as an unclosed
//! bracket rather than silently swallowed.

use crate::error::{ErrorKind, Result, err_at};
use crate::mapping::Mapping;
use crate::position::{Position, Span};
use crate::value::Value;

use super::Parser;
use super::tag;

impl Parser<'_> {
    /// Parse `[ ... ]`.
    pub(super) fn parse_flow_sequence(&mut self, min_indent: isize) -> Result<Value> {
        self.enter()?;
        let open = self.cursor.position();
        self.cursor.bump();
        let mut items = Vec::new();
        loop {
            self.skip_flow_space();
            self.require_flow_open(']', open)?;
            if self.cursor.at(']') {
                self.cursor.bump();
                break;
            }
            items.push(self.parse_flow_entry(min_indent)?);
            self.skip_flow_space();
            self.require_flow_open(']', open)?;
            match self.cursor.peek() {
                Some(',') => {
                    self.cursor.bump();
                }
                Some(']') => {
                    self.cursor.bump();
                    break;
                }
                Some(found) => {
                    return err_at(
                        ErrorKind::UnexpectedCharacter {
                            found,
                            context: "a flow sequence",
                        },
                        self.cursor.position(),
                    );
                }
                None => return err_at(ErrorKind::UnclosedFlow { expected: ']' }, open),
            }
        }
        self.leave();
        Ok(Value::Sequence(items))
    }

    /// Parse `{ ... }`.
    pub(super) fn parse_flow_mapping(&mut self, min_indent: isize) -> Result<Value> {
        self.enter()?;
        let open = self.cursor.position();
        self.cursor.bump();
        let mut mapping = Mapping::new();
        loop {
            self.skip_flow_space();
            self.require_flow_open('}', open)?;
            if self.cursor.at('}') {
                self.cursor.bump();
                break;
            }
            let explicit = self.cursor.at('?') && self.at_flow_separator(1);
            if explicit {
                self.cursor.bump();
                self.skip_flow_space();
            }
            let key_position = self.cursor.position();
            let (key, json_like) = if self.cursor.at(':') && !explicit {
                (Value::Null, true)
            } else {
                self.parse_flow_node(min_indent)?
            };
            self.skip_flow_space();
            let value =
                if self.cursor.at(':') && (json_like || explicit || self.at_flow_separator(1)) {
                    self.cursor.bump();
                    self.skip_flow_space();
                    if matches!(self.cursor.peek(), None | Some(',' | '}')) {
                        Value::Null
                    } else {
                        self.parse_flow_node(min_indent)?.0
                    }
                } else {
                    Value::Null
                };
            self.insert_unique(&mut mapping, key, value, key_position)?;
            self.skip_flow_space();
            self.require_flow_open('}', open)?;
            match self.cursor.peek() {
                Some(',') => {
                    self.cursor.bump();
                }
                Some('}') => {
                    self.cursor.bump();
                    break;
                }
                Some(found) => {
                    return err_at(
                        ErrorKind::UnexpectedCharacter {
                            found,
                            context: "a flow mapping",
                        },
                        self.cursor.position(),
                    );
                }
                None => return err_at(ErrorKind::UnclosedFlow { expected: '}' }, open),
            }
        }
        self.leave();
        Ok(Value::Mapping(mapping))
    }

    /// One entry of a flow sequence, which may itself be a `key: value` pair
    /// and therefore a single-entry mapping (`[a: 1]`).
    fn parse_flow_entry(&mut self, min_indent: isize) -> Result<Value> {
        let key_position = self.cursor.position();
        let (key, json_like) = self.parse_flow_node(min_indent)?;
        self.skip_flow_space();
        if !(self.cursor.at(':') && (json_like || self.at_flow_separator(1))) {
            return Ok(key);
        }
        self.cursor.bump();
        self.skip_flow_space();
        let value = if matches!(self.cursor.peek(), None | Some(',' | ']' | '}')) {
            Value::Null
        } else {
            self.parse_flow_node(min_indent)?.0
        };
        let mut mapping = Mapping::new();
        self.insert_unique(&mut mapping, key, value, key_position)?;
        Ok(Value::Mapping(mapping))
    }

    /// One node inside a flow collection.
    ///
    /// The `bool` says whether the node was written "JSON-like" — quoted or
    /// bracketed — which is what allows a `:` to follow it with no space.
    pub(super) fn parse_flow_node(&mut self, min_indent: isize) -> Result<(Value, bool)> {
        let properties = self.parse_properties()?;
        self.skip_flow_space();
        if properties.is_explicit() && self.at_empty_flow_node() {
            // `[&a , *a]`, `[!t , x]`, `{? &k : &v }`: the anchor or tag is
            // the whole node, and its content is the empty scalar. Without
            // this the alias below it would dangle, so refusing here would
            // reject a document `serde_yaml` reads as a sequence of nulls.
            let span = Span::point(self.cursor.position());
            let value = tag::apply_to_scalar(&properties.tag, "", true, span)?;
            self.register_anchor(properties.anchor, &value);
            return Ok((value, false));
        }
        let (value, json_like) = match self.cursor.peek() {
            None => {
                return err_at(ErrorKind::ExpectedNodeContent, self.cursor.position());
            }
            Some('[') => (
                tag::apply_to_collection(&properties.tag, self.parse_flow_sequence(min_indent)?),
                true,
            ),
            Some('{') => (
                tag::apply_to_collection(&properties.tag, self.parse_flow_mapping(min_indent)?),
                true,
            ),
            Some('\'') => {
                let (text, span) = self.parse_single_quoted()?;
                (
                    tag::apply_to_scalar(&properties.tag, &text, false, span)?,
                    true,
                )
            }
            Some('"') => {
                let (text, span) = self.parse_double_quoted()?;
                (
                    tag::apply_to_scalar(&properties.tag, &text, false, span)?,
                    true,
                )
            }
            Some('*') => (self.parse_alias()?, false),
            Some(found @ ('@' | '`' | ',' | ']' | '}' | '#')) => {
                return err_at(
                    ErrorKind::UnexpectedCharacter {
                        found,
                        context: "a flow node",
                    },
                    self.cursor.position(),
                );
            }
            Some(_) => {
                let (text, span) = self.parse_plain_scalar(min_indent, true);
                (
                    tag::apply_to_scalar(&properties.tag, &text, true, span)?,
                    false,
                )
            }
        };
        self.register_anchor(properties.anchor, &value);
        Ok((value, json_like))
    }

    /// Skip whitespace, line breaks and comments between flow tokens.
    pub(super) fn skip_flow_space(&mut self) {
        loop {
            self.cursor.skip_blanks();
            if self.cursor.at('#') {
                self.cursor.skip_to_line_end();
                continue;
            }
            if !self.cursor.skip_line_break() {
                return;
            }
        }
    }

    /// True when the character `n` ahead ends a flow token — whitespace, a
    /// line break, end of input, or one of `,`/`]`/`}`.
    fn at_flow_separator(&self, n: usize) -> bool {
        self.cursor.blank_or_end_at(n) || matches!(self.cursor.peek_at(n), Some(',' | ']' | '}'))
    }

    /// True when the cursor sits where a node was expected but the flow
    /// collection has already moved on — the node is therefore empty.
    ///
    /// Only ever consulted for a node that carried an anchor or a tag; an
    /// undecorated empty entry (`[a,,b]`) stays the error it is in both this
    /// crate and `serde_yaml`.
    fn at_empty_flow_node(&self) -> bool {
        match self.cursor.peek() {
            Some(',' | ']' | '}') => true,
            // `{? &k : v}` — the `:` belongs to the mapping, not to a plain
            // scalar starting here.
            Some(':') => self.at_flow_separator(1),
            _ => false,
        }
    }

    /// Fail with [`ErrorKind::UnclosedFlow`] when the stream ended, or a new
    /// document began, while a bracket was still open.
    fn require_flow_open(&self, expected: char, open: Position) -> Result<()> {
        if self.cursor.is_eof() || self.cursor.at_document_marker() {
            return err_at(ErrorKind::UnclosedFlow { expected }, open);
        }
        Ok(())
    }
}
