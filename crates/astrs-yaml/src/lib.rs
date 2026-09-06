//! AstRS's own pure-Rust YAML 1.2 reader and writer.
//!
//! The dataflow manifest (blueprint §8) is the user-facing contract, and its
//! diagnostics are only ever as good as the parser underneath them. A stray
//! tab, a mis-indented `inputs:` block or a duplicate key has to be reported
//! at the line and column where it actually happens — not as an opaque
//! "invalid type: string" surfacing somewhere inside a `serde` visitor with
//! no source location attached at all. Owning the parser is what makes that
//! possible, and it is also what removes the last unmaintained third-party
//! dependency from the workspace's graph.
//!
//! ```
//! use astrs_yaml::Value;
//!
//! let manifest: Value = astrs_yaml::from_str(
//!     "name: perception\nnodes:\n  - id: camera\n    outputs: [frames]\n",
//! )?;
//! assert_eq!(manifest.get("name").and_then(Value::as_str), Some("perception"));
//! # Ok::<(), astrs_yaml::Error>(())
//! ```
//!
//! # What is supported, exactly
//!
//! This is a **YAML 1.2 core-schema** parser covering the subset a
//! configuration format needs. The table below is exhaustive in both
//! directions — everything in the left column works, and anything not
//! mentioned is not implemented.
//!
//! | Feature | Status |
//! |---|---|
//! | Block mappings and sequences, any nesting | yes |
//! | Compact entries: `- id: x`, `- - nested` | yes |
//! | Flow collections `[a, b]`, `{a: 1}`, spanning lines | yes |
//! | Plain scalars, incl. multi-line folding | yes |
//! | `'single'` and `"double"` quoted scalars, multi-line | yes |
//! | Every `"…"` escape: `\n \t \\ \" \/ \0 \a \b \v \f \e \N \_ \L \P \x41 é \U0001F600`, escaped space, and `\`-at-end-of-line | yes |
//! | Literal `\|` and folded `>` block scalars | yes |
//! | Chomping `-`/`+` and explicit indentation indicators (`\|2`) | yes |
//! | Comments, anywhere a comment is legal | yes |
//! | Explicit keys (`? key` / `: value`), non-scalar keys | yes |
//! | Anchors `&a`, aliases `*a`, with an expansion budget | yes |
//! | An anchor or tag standing alone as an empty node: `[&a , *a]`, `&a: 1` | yes |
//! | Core-schema tag resolution: null / bool / int (dec, `0x`, `0o`, `0b`) / float (incl. `.inf`, `.nan`) / str | yes |
//! | Explicit `!!null !!bool !!int !!float !!str !!seq !!map`, which *force* the type | yes |
//! | Local tags `!Variant`, kept as [`Value::Tagged`] | yes |
//! | Verbatim tags `!<uri>`, `%TAG` handles | yes |
//! | Multiple documents (`---`, `...`) | yes, via [`from_str_multi`] |
//! | Duplicate mapping keys | **rejected**, with the key's position |
//! | Tabs used as indentation | **rejected**, with the tab's position |
//! | Characters outside YAML's `c-printable` — the C0 controls other than tab and newline, `DEL`, the C1 controls, `U+FFFE`/`U+FFFF` | **rejected**, with the character's position |
//! | Merge keys (`<<: *base`) | **no** — `<<` is an ordinary key, as in `serde_yaml` |
//! | YAML 1.1 types: `!!binary`, `!!timestamp`, `!!omap`, `!!set`, sexagesimals, `yes`/`no` booleans | **no** — but a tag this crate does not implement still suppresses core-schema resolution, so `!!binary 42` is the *string* `"42"`. `on:` with no tag is the *string* `"on"` |
//! | Recursive aliases (an anchor referring to itself) | **no** — [`Value`] is a tree |
//! | `%YAML`/unknown `%` directives | parsed and **ignored**, as the specification directs — `libyaml` refuses them instead |
//! | `U+2028`/`U+2029` as *line breaks* | **no** — YAML 1.2 removed that 1.1 rule, so they are ordinary characters here and line breaks to `libyaml` |
//!
//! Three details are worth stating outright because they are the ones a
//! reader is most likely to assume the other way:
//!
//! - **A block scalar's last line keeps only the break the file really
//!   has.** `a: \|\n  x` with no trailing newline is `"x"`, not `"x\n"` —
//!   and `\|+` does not resurrect it either.
//! - **Continuation lines inside a flow collection must be indented past
//!   the block node that owns it.** `b: [x\ny]` is refused here and accepted
//!   by `libyaml`; this is the one construct on which this crate is stricter
//!   than `serde_yaml`.
//! - **A named tag handle is `!`, word characters, `!`.** `!:!bool` is
//!   therefore a local tag, not a reference to an undefined handle.
//!
//! # Resource limits
//!
//! Every parse is bounded — input size, nesting depth, and total alias
//! expansion — see [`Limits`]. [`Error::is_limit`] distinguishes "this
//! document is over a ceiling" from "this document is malformed".
//!
//! # Round-tripping
//!
//! [`to_string`] emits deterministic block-style YAML, and
//! `from_str(&to_string(&v)?)? == v` holds for every [`Value`] — including
//! mappings keyed by sequences, strings full of control characters, and
//! `f64::MAX`. The emitter also reproduces `serde_yaml`'s exact layout for
//! manifest-shaped documents, so the generated fixtures committed to this
//! repository stay byte-identical across the switchover.
//!
//! # Positions are one-based
//!
//! [`Position`] counts lines and columns the way an editor's status bar does
//! — from `1`, not from `0` — because every consumer of a manifest error is
//! a human reading it next to their editor.
//!
//! ```
//! use astrs_yaml::{ErrorKind, Value};
//!
//! let source = "nodes:\n\t- id: camera\n";
//! let error = astrs_yaml::from_str::<Value>(source).unwrap_err();
//! assert!(matches!(error.kind(), ErrorKind::TabInIndentation));
//! assert_eq!((error.line(), error.column()), (Some(2), Some(1)));
//! ```

mod de;
mod emit;
mod error;
mod limits;
mod mapping;
mod number;
mod parse;
mod position;
mod ser;
mod value;

pub use crate::error::{Error, ErrorKind, Result};
pub use crate::limits::Limits;
pub use crate::mapping::Mapping;
pub use crate::number::Number;
pub use crate::position::{Position, Span};
pub use crate::value::{Tag, TaggedValue, Value};

use serde::Serialize;
use serde::de::{Deserialize, DeserializeOwned};

/// Parse one YAML document into `T`.
///
/// The input must hold exactly one document; a stream with several is
/// reported as [`ErrorKind::MultipleDocuments`] rather than silently using
/// the first. An empty input is the null document.
///
/// # Errors
///
/// Returns the first syntax, limit, or `serde` conversion failure. Syntax
/// failures carry a [`Span`]; see [`Error::render`] to draw a caret under
/// the offending source line.
///
/// # Examples
///
/// ```
/// #[derive(serde::Deserialize, PartialEq, Debug)]
/// struct Node {
///     id: String,
///     outputs: Vec<String>,
/// }
///
/// let node: Node = astrs_yaml::from_str("id: camera\noutputs: [frames]\n")?;
/// assert_eq!(node.id, "camera");
/// # Ok::<(), astrs_yaml::Error>(())
/// ```
pub fn from_str<T: DeserializeOwned>(input: &str) -> Result<T> {
    from_str_with(input, Limits::default())
}

/// Parse one YAML document into `T`, under explicit [`Limits`].
///
/// # Errors
///
/// As [`from_str`], plus the limit failures `limits` enables.
pub fn from_str_with<T: DeserializeOwned>(input: &str, limits: Limits) -> Result<T> {
    de::from_value(parse_single(input, limits)?)
}

/// Parse every document in a `---`-separated stream into `T`.
///
/// # Errors
///
/// As [`from_str`], except that any number of documents is accepted.
///
/// # Examples
///
/// ```
/// let counts: Vec<u32> = astrs_yaml::from_str_multi("1\n---\n2\n")?;
/// assert_eq!(counts, vec![1, 2]);
/// # Ok::<(), astrs_yaml::Error>(())
/// ```
pub fn from_str_multi<T: DeserializeOwned>(input: &str) -> Result<Vec<T>> {
    from_str_multi_with(input, Limits::default())
}

/// Parse every document in a stream into `T`, under explicit [`Limits`].
///
/// # Errors
///
/// As [`from_str_multi`], plus the limit failures `limits` enables.
pub fn from_str_multi_with<T: DeserializeOwned>(input: &str, limits: Limits) -> Result<Vec<T>> {
    parse::parse_documents(input, limits)?
        .into_iter()
        .map(de::from_value)
        .collect()
}

/// Parse one YAML document into a [`Value`].
///
/// # Errors
///
/// As [`from_str`].
///
/// # Examples
///
/// ```
/// use astrs_yaml::Value;
///
/// let value = astrs_yaml::parse_str("- 1\n- two\n")?;
/// assert_eq!(value.get_index(1).and_then(Value::as_str), Some("two"));
/// # Ok::<(), astrs_yaml::Error>(())
/// ```
pub fn parse_str(input: &str) -> Result<Value> {
    parse_single(input, Limits::default())
}

/// Parse every document in a stream into [`Value`]s.
///
/// # Errors
///
/// As [`from_str_multi`].
pub fn parse_str_multi(input: &str) -> Result<Vec<Value>> {
    parse::parse_documents(input, Limits::default())
}

/// Parse every document in a stream into [`Value`]s, under explicit
/// [`Limits`].
///
/// # Errors
///
/// As [`from_str_multi_with`].
pub fn parse_str_multi_with(input: &str, limits: Limits) -> Result<Vec<Value>> {
    parse::parse_documents(input, limits)
}

fn parse_single(input: &str, limits: Limits) -> Result<Value> {
    let mut documents = parse::parse_documents(input, limits)?;
    match documents.len() {
        0 => Ok(Value::Null),
        1 => Ok(documents.remove(0)),
        found => Err(Error::new(ErrorKind::MultipleDocuments { found })),
    }
}

/// Convert `value` into a [`Value`] without going through text.
///
/// # Errors
///
/// Fails only for values YAML cannot represent — an `i128`/`u128` outside
/// the `i64`/`u64` range, or a `Serialize` implementation that reports its
/// own error.
///
/// # Examples
///
/// ```
/// use astrs_yaml::Value;
///
/// let value = astrs_yaml::to_value(&vec![1u8, 2])?;
/// assert_eq!(value, Value::Sequence(vec![Value::from(1u8), Value::from(2u8)]));
/// # Ok::<(), astrs_yaml::Error>(())
/// ```
pub fn to_value<T: Serialize + ?Sized>(value: &T) -> Result<Value> {
    ser::to_value(value)
}

/// Deserialize `T` out of an owned [`Value`].
///
/// # Errors
///
/// Returns the `serde` conversion failure, if any.
pub fn from_value<T: DeserializeOwned>(value: Value) -> Result<T> {
    de::from_value(value)
}

/// Deserialize `T` out of a borrowed [`Value`], letting `T` borrow its
/// strings.
///
/// This is the zero-copy path: a type with `&'de str` fields is filled in
/// with slices of the [`Value`] itself rather than fresh allocations.
///
/// # Errors
///
/// Returns the `serde` conversion failure, if any.
///
/// # Examples
///
/// ```
/// #[derive(serde::Deserialize)]
/// struct View<'a> {
///     id: &'a str,
/// }
///
/// let value = astrs_yaml::parse_str("id: camera\n")?;
/// let view: View<'_> = astrs_yaml::from_borrowed_value(&value)?;
/// assert_eq!(view.id, "camera");
/// # Ok::<(), astrs_yaml::Error>(())
/// ```
pub fn from_borrowed_value<'de, T: Deserialize<'de>>(value: &'de Value) -> Result<T> {
    de::from_borrowed_value(value)
}

/// Render `value` as a YAML document.
///
/// The output is deterministic block-style YAML ending in a newline, with no
/// `---` marker — see the crate documentation for the exact layout rules.
///
/// # Errors
///
/// As [`to_value`].
///
/// # Examples
///
/// ```
/// #[derive(serde::Serialize)]
/// struct Node {
///     id: &'static str,
///     outputs: Vec<&'static str>,
/// }
///
/// let yaml = astrs_yaml::to_string(&Node { id: "camera", outputs: vec!["frames"] })?;
/// assert_eq!(yaml, "id: camera\noutputs:\n- frames\n");
/// # Ok::<(), astrs_yaml::Error>(())
/// ```
pub fn to_string<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    Ok(emit::to_string(&ser::to_value(value)?))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn an_empty_input_is_the_null_document() {
        assert_eq!(parse_str("").expect("empty"), Value::Null);
        assert_eq!(
            parse_str("# just a comment\n").expect("comment"),
            Value::Null
        );
        assert_eq!(parse_str("---\n").expect("marker"), Value::Null);
        assert_eq!(
            parse_str_multi("").expect("empty stream"),
            Vec::<Value>::new()
        );
    }

    #[test]
    fn several_documents_are_refused_by_the_single_document_api() {
        let error = parse_str("a: 1\n---\nb: 2\n").expect_err("two documents");
        assert!(matches!(
            error.kind(),
            ErrorKind::MultipleDocuments { found: 2 }
        ));
        let documents = parse_str_multi("a: 1\n---\nb: 2\n").expect("stream");
        assert_eq!(documents.len(), 2);
    }

    #[test]
    fn the_round_trip_holds_for_a_manifest_shaped_document() {
        let source = "name: perception\nnodes:\n- id: camera\n  outputs:\n  - frames\n";
        let value = parse_str(source).expect("parse");
        assert_eq!(to_string(&value).expect("emit"), source);
    }

    #[test]
    fn limits_are_reachable_from_the_public_api() {
        let limits = Limits::default().with_max_depth(2);
        assert!(from_str_with::<Value>("a: [1]", limits).is_ok());
        let error = from_str_with::<Value>("a: [[1]]", limits).expect_err("too deep");
        assert!(error.is_limit());
        assert!(parse_str_multi_with("a: 1", limits).is_ok());
        assert!(from_str_multi_with::<Value>("a: 1", limits).is_ok());
    }

    #[test]
    fn values_convert_both_ways_without_text() {
        let value = to_value(&vec![1u8, 2]).expect("to_value");
        let back: Vec<u8> = from_value(value.clone()).expect("from_value");
        assert_eq!(back, vec![1, 2]);
        let borrowed: Vec<u8> = from_borrowed_value(&value).expect("borrowed");
        assert_eq!(borrowed, vec![1, 2]);
    }
}
