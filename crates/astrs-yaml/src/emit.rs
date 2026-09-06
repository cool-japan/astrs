//! The emitter: [`Value`] in, deterministic block-style YAML out.
//!
//! # Deterministic, and byte-compatible with what this workspace already has
//!
//! Several fixtures in this repository are *generated* YAML — `astrs expand`
//! and `astrs migrate` write them, and their golden files are committed. So
//! the emitter is not free to pick its own house style: it reproduces the
//! layout `serde_yaml` produces, down to the byte, for every document shape
//! a manifest can take. Concretely:
//!
//! - No `---` document-start marker.
//! - A mapping's **mapping** value is indented two columns; a mapping's
//!   **sequence** value is *not* indented at all. That asymmetry looks odd
//!   written down and is what almost every hand-written YAML file in the
//!   wild does:
//!
//!   ```text
//!   nodes:
//!   - id: camera
//!     outputs:
//!     - frames
//!   ```
//!
//! - Empty collections are written in flow style (`{}`, `[]`).
//! - Strings are left unquoted unless quoting is needed — either because the
//!   text would resolve back to something that is not a string (`true`,
//!   `1`, `.inf`, `017`), or because a YAML indicator in it would change how
//!   the line parses. Then single quotes, and only if the text contains a
//!   character no single-quoted scalar can hold does it fall back to double
//!   quotes with escapes.
//! - A multi-line string becomes a literal block scalar (`|`, `|-`, `|+`)
//!   whenever one can hold it losslessly.
//! - Floats use Rust's shortest round-trip form, so `2.0` keeps its `.0` and
//!   `f64::MAX` comes back bit-identical; the non-finite ones use YAML's
//!   `.inf` / `-.inf` / `.nan`.
//!
//! # The property that matters
//!
//! `parse(to_string(v)) == v`, for every `v`, including mappings with
//! sequence keys and strings full of control characters. `tests/roundtrip.rs`
//! asserts it over generated values; the rules above are what make it true.

use crate::mapping::Mapping;
use crate::parse::needs_quoting_to_stay_a_string;
use crate::value::{TaggedValue, Value};

/// Render `value` as a single YAML document, ending with a newline.
#[must_use]
pub(crate) fn to_string(value: &Value) -> String {
    let mut emitter = Emitter { out: String::new() };
    emitter.document(value);
    emitter.out
}

struct Emitter {
    out: String,
}

impl Emitter {
    fn document(&mut self, value: &Value) {
        match value {
            Value::Sequence(items) if !items.is_empty() => self.sequence(items, 0, false),
            Value::Mapping(mapping) if !mapping.is_empty() => self.mapping(mapping, 0, false),
            Value::Tagged(tagged) => {
                self.out.push_str(&tagged.tag.to_string());
                // A root block scalar's content sits at column 2 even though
                // a root mapping or sequence sits at column 0.
                self.tagged_body(&tagged.value, 0, 0, 2);
            }
            _ => self.leaf(value, 2),
        }
    }

    /// Write a block mapping at `indent`.
    ///
    /// `inline_first` suppresses the indentation of the first key, which is
    /// how `- id: camera` puts the first pair on the dash's own line.
    fn mapping(&mut self, mapping: &Mapping, indent: usize, inline_first: bool) {
        for (index, (key, value)) in mapping.iter().enumerate() {
            if index > 0 || !inline_first {
                self.pad(indent);
            }
            if is_inline_key(key) {
                self.out.push_str(&inline_scalar(key));
            } else {
                self.out.push_str("? ");
                self.node_after_marker(key, indent + 2);
                self.pad(indent);
            }
            self.out.push(':');
            self.mapping_value(value, indent);
        }
    }

    /// Write a block sequence at `indent`.
    fn sequence(&mut self, items: &[Value], indent: usize, inline_first: bool) {
        for (index, item) in items.iter().enumerate() {
            if index > 0 || !inline_first {
                self.pad(indent);
            }
            self.out.push('-');
            self.out.push(' ');
            self.node_after_marker(item, indent + 2);
        }
    }

    /// Write the node that follows a `- ` or `? ` marker: its first line is
    /// already positioned, and everything after it is indented to `indent`.
    fn node_after_marker(&mut self, value: &Value, indent: usize) {
        match value {
            Value::Sequence(items) if !items.is_empty() => self.sequence(items, indent, true),
            Value::Mapping(mapping) if !mapping.is_empty() => self.mapping(mapping, indent, true),
            Value::Tagged(tagged) => {
                self.out.push_str(&tagged.tag.to_string());
                self.tagged_body(&tagged.value, indent, indent, indent);
            }
            _ => self.leaf(value, indent),
        }
    }

    /// Write the `: value` half of a mapping entry whose key sits at
    /// `indent`.
    fn mapping_value(&mut self, value: &Value, indent: usize) {
        match value {
            Value::Sequence(items) if !items.is_empty() => {
                self.out.push('\n');
                // Not `indent + 2`: a sequence under a mapping key keeps the
                // key's own indentation.
                self.sequence(items, indent, false);
            }
            Value::Mapping(mapping) if !mapping.is_empty() => {
                self.out.push('\n');
                self.mapping(mapping, indent + 2, false);
            }
            Value::Tagged(tagged) => {
                self.out.push(' ');
                self.out.push_str(&tagged.tag.to_string());
                self.tagged_body(&tagged.value, indent, indent + 2, indent + 2);
            }
            _ => {
                self.out.push(' ');
                self.leaf(value, indent + 2);
            }
        }
    }

    /// Write what follows a tag that has already been emitted.
    ///
    /// `sequence_indent`, `mapping_indent` and `leaf_indent` differ because a
    /// sequence under a mapping key keeps the key's indentation (see
    /// [`Emitter::mapping_value`]) while a nested mapping and a block
    /// scalar's content both step in by two.
    fn tagged_body(
        &mut self,
        value: &Value,
        sequence_indent: usize,
        mapping_indent: usize,
        leaf_indent: usize,
    ) {
        match value {
            Value::Sequence(items) if !items.is_empty() => {
                self.out.push('\n');
                self.sequence(items, sequence_indent, false);
            }
            Value::Mapping(mapping) if !mapping.is_empty() => {
                self.out.push('\n');
                self.mapping(mapping, mapping_indent, false);
            }
            Value::Tagged(inner) => {
                // A node carries at most one tag, so a doubly tagged value
                // has to put the inner tag on its own, more-indented line —
                // where it reads as a fresh node under the outer tag.
                self.out.push('\n');
                self.pad(mapping_indent);
                self.out.push_str(&inner.tag.to_string());
                self.tagged_body(&inner.value, mapping_indent, mapping_indent, leaf_indent);
            }
            _ => {
                self.out.push(' ');
                self.leaf(value, leaf_indent);
            }
        }
    }

    /// Write a scalar (or an empty collection) plus its trailing newline.
    ///
    /// `indent` is where a literal block scalar's content would go.
    fn leaf(&mut self, value: &Value, indent: usize) {
        if let Value::String(text) = value
            && let Some(lines) = literal_block_lines(text)
        {
            self.literal_block(text, &lines, indent);
            return;
        }
        self.out.push_str(&inline_scalar(value));
        self.out.push('\n');
    }

    fn literal_block(&mut self, text: &str, lines: &[&str], indent: usize) {
        let trailing = text.len() - text.trim_end_matches('\n').len();
        let chomping = if trailing == 0 {
            "-"
        } else if trailing == 1 && text != "\n" {
            ""
        } else {
            "+"
        };
        // Without an explicit indicator a parser infers the block's
        // indentation from its first non-empty line, which is wrong when
        // that line is itself indented — or absent.
        let indicator = if lines
            .first()
            .is_none_or(|line| line.is_empty() || line.starts_with(' '))
        {
            "2"
        } else {
            ""
        };
        self.out.push('|');
        self.out.push_str(indicator);
        self.out.push_str(chomping);
        self.out.push('\n');
        for line in lines {
            if !line.is_empty() {
                self.pad(indent);
                self.out.push_str(line);
            }
            self.out.push('\n');
        }
    }

    fn pad(&mut self, indent: usize) {
        for _ in 0..indent {
            self.out.push(' ');
        }
    }
}

/// True for the scalar kinds that can be written directly before a `:`.
fn is_inline_key(key: &Value) -> bool {
    match key {
        Value::Null | Value::Bool(_) | Value::Number(_) => true,
        Value::String(text) => !text.contains('\n'),
        _ => false,
    }
}

/// Render a scalar on one line.
#[must_use]
pub(crate) fn inline_scalar(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(true) => "true".to_owned(),
        Value::Bool(false) => "false".to_owned(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => quote_string(text),
        Value::Sequence(items) if items.is_empty() => "[]".to_owned(),
        Value::Mapping(mapping) if mapping.is_empty() => "{}".to_owned(),
        // Non-empty collections never reach here: every caller routes them
        // through the block writers. Rendering them in flow style keeps the
        // function total rather than partial.
        Value::Sequence(items) => {
            let inner: Vec<String> = items.iter().map(inline_scalar).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Mapping(mapping) => {
            let inner: Vec<String> = mapping
                .iter()
                .map(|(key, value)| format!("{}: {}", inline_scalar(key), inline_scalar(value)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
        Value::Tagged(tagged) => {
            let TaggedValue { tag, value } = tagged.as_ref();
            format!("{tag} {}", inline_scalar(value))
        }
    }
}

/// The lines a literal block scalar would hold, or `None` when a literal
/// block cannot round-trip `text`.
fn literal_block_lines(text: &str) -> Option<Vec<&str>> {
    if !text.contains('\n') {
        return None;
    }
    if text.chars().any(|ch| ch != '\n' && !is_printable(ch)) {
        return None;
    }
    let body = text.strip_suffix('\n').unwrap_or(text);
    let lines: Vec<&str> = body.split('\n').collect();
    // A line of nothing but spaces reads back as empty, and a trailing space
    // is invisible in the source — neither survives the round trip.
    if lines
        .iter()
        .any(|line| line.ends_with(' ') || (!line.is_empty() && line.trim().is_empty()))
    {
        return None;
    }
    Some(lines)
}

/// Quote `text` as little as possible while keeping it a string.
#[must_use]
pub(crate) fn quote_string(text: &str) -> String {
    if plain_allowed(text) && !needs_quoting_to_stay_a_string(text) {
        return text.to_owned();
    }
    if single_quote_allowed(text) {
        let mut out = String::with_capacity(text.len() + 2);
        out.push('\'');
        for ch in text.chars() {
            if ch == '\'' {
                out.push('\'');
            }
            out.push(ch);
        }
        out.push('\'');
        return out;
    }
    double_quote(text)
}

/// True when `text` can be written with no quotes at all.
///
/// Mirrors the "block plain allowed" analysis every YAML emitter performs:
/// no leading or trailing blank, no line break, no unprintable character,
/// and no indicator in a position where it would change the parse.
fn plain_allowed(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    // At the start of a line these *are* the document markers, so a scalar
    // that spells one has to be quoted even though nothing in it is an
    // indicator character.
    if text.starts_with("---") || text.starts_with("...") {
        return false;
    }
    if text.starts_with([' ', '\t']) || text.ends_with([' ', '\t']) {
        return false;
    }
    if text.chars().any(|ch| !is_printable(ch)) {
        return false;
    }
    let bytes: Vec<char> = text.chars().collect();
    for (index, &ch) in bytes.iter().enumerate() {
        let next_is_blank = bytes
            .get(index + 1)
            .is_none_or(|next| matches!(next, ' ' | '\t'));
        if index == 0 {
            match ch {
                '#' | ',' | '[' | ']' | '{' | '}' | '&' | '*' | '!' | '|' | '>' | '\'' | '"'
                | '%' | '@' | '`' => return false,
                '?' | ':' | '-' if next_is_blank => return false,
                _ => {}
            }
        } else {
            match ch {
                ':' if next_is_blank => return false,
                '#' if matches!(bytes[index - 1], ' ' | '\t') => return false,
                _ => {}
            }
        }
    }
    true
}

/// True when `text` can be written inside single quotes, whose only escape
/// is `''` and which therefore cannot hold a line break or a control
/// character.
fn single_quote_allowed(text: &str) -> bool {
    text.chars().all(is_printable)
}

/// YAML's printable set, minus the characters this crate refuses to write
/// unescaped: a tab (whose width is undefined), the Unicode line separators
/// (which some parsers fold), and a byte-order mark (which is a stream
/// marker, not content).
fn is_printable(ch: char) -> bool {
    match ch {
        '\t' | '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}' | '\u{feff}' => false,
        c if (c as u32) < 0x20 => false,
        c if ('\u{7f}'..='\u{9f}').contains(&c) => false,
        _ => true,
    }
}

/// Render `text` inside double quotes, escaping what has to be escaped.
fn double_quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\0' => out.push_str("\\0"),
            '\u{7}' => out.push_str("\\a"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{b}' => out.push_str("\\v"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            '\u{1b}' => out.push_str("\\e"),
            '\u{85}' => out.push_str("\\N"),
            '\u{2028}' => out.push_str("\\L"),
            '\u{2029}' => out.push_str("\\P"),
            '\u{feff}' => out.push_str("\\uFEFF"),
            c if !is_printable(c) => out.push_str(&format!("\\x{:02X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::number::Number;

    fn map(pairs: &[(&str, Value)]) -> Value {
        let mut mapping = Mapping::new();
        for (key, value) in pairs {
            mapping.insert(Value::from(*key), value.clone());
        }
        Value::Mapping(mapping)
    }

    #[test]
    fn a_root_scalar_is_one_line() {
        assert_eq!(to_string(&Value::Null), "null\n");
        assert_eq!(to_string(&Value::from("hello")), "hello\n");
        assert_eq!(to_string(&Value::from(true)), "true\n");
        assert_eq!(to_string(&Value::from(1.5)), "1.5\n");
    }

    #[test]
    fn empty_collections_use_flow_style() {
        assert_eq!(to_string(&Value::Sequence(vec![])), "[]\n");
        assert_eq!(to_string(&Value::Mapping(Mapping::new())), "{}\n");
        assert_eq!(
            to_string(&map(&[
                ("a", Value::Mapping(Mapping::new())),
                ("b", Value::Sequence(vec![])),
            ])),
            "a: {}\nb: []\n"
        );
    }

    #[test]
    fn a_sequence_under_a_key_keeps_the_keys_indentation() {
        let value = map(&[(
            "nodes",
            Value::Sequence(vec![map(&[
                ("id", Value::from("camera")),
                ("outputs", Value::Sequence(vec![Value::from("frames")])),
            ])]),
        )]);
        assert_eq!(
            to_string(&value),
            "nodes:\n- id: camera\n  outputs:\n  - frames\n"
        );
    }

    #[test]
    fn nested_mappings_indent_by_two() {
        let value = map(&[("a", map(&[("b", map(&[("c", Value::from(1u8))]))]))]);
        assert_eq!(to_string(&value), "a:\n  b:\n    c: 1\n");
    }

    #[test]
    fn a_nested_sequence_puts_its_first_item_on_the_dash_line() {
        let value = map(&[(
            "a",
            Value::Sequence(vec![Value::Sequence(vec![
                Value::from(1u8),
                Value::from(2u8),
            ])]),
        )]);
        assert_eq!(to_string(&value), "a:\n- - 1\n  - 2\n");
    }

    #[test]
    fn a_non_scalar_key_uses_the_explicit_question_mark_form() {
        let mut mapping = Mapping::new();
        mapping.insert(
            Value::Sequence(vec![Value::from(1u8), Value::from(2u8)]),
            Value::from("v"),
        );
        mapping.insert(Value::from("b"), Value::from(2u8));
        assert_eq!(
            to_string(&Value::Mapping(mapping)),
            "? - 1\n  - 2\n: v\nb: 2\n"
        );
    }

    #[test]
    fn scalar_keys_of_every_kind_stay_inline() {
        let mut mapping = Mapping::new();
        mapping.insert(Value::Null, Value::from(1u8));
        mapping.insert(Value::from(true), Value::from(2u8));
        mapping.insert(Value::from(1.5), Value::from(3u8));
        assert_eq!(
            to_string(&Value::Mapping(mapping)),
            "null: 1\ntrue: 2\n1.5: 3\n"
        );
    }

    #[test]
    fn strings_are_quoted_only_when_they_have_to_be() {
        for text in [
            "hello", "y", "on", "1_0", "x#y", ":x", "?x", "-x", "=", "a b", "it's", "a\"b", "--",
            "x -", "a:b", "a,b",
        ] {
            assert_eq!(quote_string(text), text, "{text}");
        }
        for (text, expected) in [
            ("true", "'true'"),
            ("null", "'null'"),
            ("~", "'~'"),
            ("1", "'1'"),
            ("00", "'00'"),
            ("0x1f", "'0x1f'"),
            (".inf", "'.inf'"),
            ("", "''"),
            ("x: y", "'x: y'"),
            ("x #y", "'x #y'"),
            (" lead", "' lead'"),
            ("trail ", "'trail '"),
            ("-", "'-'"),
            ("- ", "'- '"),
            ("?", "'?'"),
            ("#x", "'#x'"),
            ("@x", "'@x'"),
            ("`x", "'`x'"),
            ("%YAML", "'%YAML'"),
            ("---", "'---'"),
            ("...x", "'...x'"),
        ] {
            assert_eq!(quote_string(text), expected, "{text}");
        }
    }

    #[test]
    fn unprintable_text_falls_back_to_double_quotes() {
        assert_eq!(quote_string("\tx"), "\"\\tx\"");
        assert_eq!(quote_string("a\u{7f}b"), "\"a\\x7Fb\"");
        assert_eq!(quote_string("a\u{1b}b"), "\"a\\eb\"");
        assert_eq!(quote_string("a\u{85}b"), "\"a\\Nb\"");
        assert_eq!(quote_string("\u{feff}x"), "\"\\uFEFFx\"");
        assert_eq!(quote_string("x\0y"), "\"x\\0y\"");
        assert_eq!(quote_string("x\r\ny"), "\"x\\r\\ny\"");
        // Printable non-ASCII stays raw.
        assert_eq!(quote_string("é中"), "é中");
        assert_eq!(quote_string("a\u{a0}b"), "a\u{a0}b");
    }

    #[test]
    fn multi_line_strings_become_literal_blocks() {
        assert_eq!(
            to_string(&map(&[("a", Value::from("line1\nline2"))])),
            "a: |-\n  line1\n  line2\n"
        );
        assert_eq!(to_string(&map(&[("a", Value::from("x\n"))])), "a: |\n  x\n");
        assert_eq!(to_string(&map(&[("a", Value::from("\n"))])), "a: |2+\n\n");
        assert_eq!(
            to_string(&map(&[("a", Value::from("x\n\n"))])),
            "a: |+\n  x\n\n"
        );
        assert_eq!(
            to_string(&map(&[("a", Value::from(" x\ny"))])),
            "a: |2-\n   x\n  y\n"
        );
    }

    #[test]
    fn a_multi_line_string_that_cannot_round_trip_is_double_quoted() {
        // Trailing space on a line, an all-space line, and a tab each defeat
        // the literal block form.
        assert_eq!(
            to_string(&map(&[("a", Value::from("x \ny"))])),
            "a: \"x \\ny\"\n"
        );
        assert_eq!(
            to_string(&map(&[("a", Value::from("x\n \ny"))])),
            "a: \"x\\n \\ny\"\n"
        );
        assert_eq!(
            to_string(&map(&[("a", Value::from("x\n\ty"))])),
            "a: \"x\\n\\ty\"\n"
        );
    }

    #[test]
    fn numbers_render_in_their_core_schema_spellings() {
        assert_eq!(
            to_string(&map(&[
                ("a", Value::from(f64::INFINITY)),
                ("b", Value::from(f64::NEG_INFINITY)),
                ("c", Value::from(f64::NAN)),
                ("d", Value::from(5.0)),
                ("e", Value::Number(Number::from(u64::MAX))),
            ])),
            "a: .inf\nb: -.inf\nc: .nan\nd: 5.0\ne: 18446744073709551615\n"
        );
    }

    #[test]
    fn tagged_values_write_their_tag_first() {
        let scalar = Value::from(TaggedValue {
            tag: crate::value::Tag::new("New"),
            value: Value::from(1u8),
        });
        assert_eq!(to_string(&scalar), "!New 1\n");

        let sequence = Value::from(TaggedValue {
            tag: crate::value::Tag::new("Tup"),
            value: Value::Sequence(vec![Value::from(1u8), Value::from(2u8)]),
        });
        assert_eq!(to_string(&sequence), "!Tup\n- 1\n- 2\n");

        let structure = Value::from(TaggedValue {
            tag: crate::value::Tag::new("Struct"),
            value: map(&[("a", Value::from(1u8))]),
        });
        assert_eq!(to_string(&structure), "!Struct\na: 1\n");
        assert_eq!(
            to_string(&map(&[("k", structure.clone())])),
            "k: !Struct\n  a: 1\n"
        );
        assert_eq!(
            to_string(&Value::Sequence(vec![structure])),
            "- !Struct\n  a: 1\n"
        );
    }

    #[test]
    fn inline_scalar_stays_total_for_non_empty_collections() {
        let value = Value::Sequence(vec![Value::from(1u8), Value::Sequence(vec![])]);
        assert_eq!(inline_scalar(&value), "[1, []]");
        let mut mapping = Mapping::new();
        mapping.insert(Value::from("a"), Value::from(1u8));
        assert_eq!(inline_scalar(&Value::Mapping(mapping)), "{a: 1}");
    }
}
