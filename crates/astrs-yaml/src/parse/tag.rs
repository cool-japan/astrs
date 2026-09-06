//! Tag shorthands, `%TAG` handles, and what a tag does to the node it sits
//! on.
//!
//! YAML has three ways to write a tag and this crate accepts all of them:
//!
//! | Written | Means |
//! |---|---|
//! | `!!str` | the secondary handle `!!` → `tag:yaml.org,2002:str` |
//! | `!local` | the primary handle `!` → the local tag `!local` |
//! | `!e!thing` | a named handle, resolved through a `%TAG` directive |
//! | `!<tag:yaml.org,2002:str>` | a verbatim URI, no handle involved |
//!
//! What happens next depends on where the tag lands:
//!
//! - A **core-schema** tag on a matching node forces its type: `!!str 1` is
//!   the string `"1"`, and `!!int 1e3` is an error rather than a float.
//! - A core-schema tag on a node of the wrong *kind* — `!!str [1]`, `!!seq
//!   abc` — is ignored, which is what `serde_yaml` does and what keeps a
//!   mis-tagged document readable.
//! - Any other `tag:yaml.org,2002:` tag (`!!binary`, `!!timestamp`,
//!   `!!omap`) is **not** implemented — this crate is the core schema, not
//!   the full 1.1 type library — but it still suppresses core-schema
//!   resolution, so the node keeps its literal text: `!!binary 42` is the
//!   string `"42"`, not the integer. Silently resolving it as an integer
//!   would be the one outcome that is wrong under *both* readings.
//! - A **local** tag (`!Circle`) survives into [`Value::Tagged`], which is
//!   the representation `serde` externally-tagged enums round-trip through.
//! - Any other global URI behaves the same way, matching `serde_yaml`.

use crate::error::{Error, ErrorKind, Result};
use crate::number::Number;
use crate::position::Span;
use crate::value::{Tag, Value};

use super::resolve::{self, IntOutcome, Resolved};

/// The prefix every core-schema tag shares.
pub(crate) const YAML_ORG_PREFIX: &str = "tag:yaml.org,2002:";

/// The prefix the secondary handle `!!` expands to.
pub(crate) const SECONDARY_HANDLE_PREFIX: &str = YAML_ORG_PREFIX;

/// What a parsed tag turns out to mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TagSpec {
    /// `!` on its own — the non-specific tag, which this crate treats as
    /// "resolve normally".
    NonSpecific,
    /// One of the seven core-schema tags.
    Core(CoreTag),
    /// A tag this crate knows about but does not act on.
    Ignored,
    /// A local tag, kept on the value.
    Local(String),
}

/// The seven tags YAML 1.2's core schema defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoreTag {
    /// `tag:yaml.org,2002:null`
    Null,
    /// `tag:yaml.org,2002:bool`
    Bool,
    /// `tag:yaml.org,2002:int`
    Int,
    /// `tag:yaml.org,2002:float`
    Float,
    /// `tag:yaml.org,2002:str`
    Str,
    /// `tag:yaml.org,2002:seq`
    Seq,
    /// `tag:yaml.org,2002:map`
    Map,
}

impl CoreTag {
    /// The tag as it is written with the secondary handle, for diagnostics.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Null => "!!null",
            Self::Bool => "!!bool",
            Self::Int => "!!int",
            Self::Float => "!!float",
            Self::Str => "!!str",
            Self::Seq => "!!seq",
            Self::Map => "!!map",
        }
    }

    /// True for the five tags that only make sense on a scalar.
    pub(crate) const fn is_scalar_tag(self) -> bool {
        !matches!(self, Self::Seq | Self::Map)
    }
}

/// Classify a fully resolved tag URI (or a local `!name`).
#[must_use]
pub(crate) fn classify(uri: &str) -> TagSpec {
    if uri == "!" {
        return TagSpec::NonSpecific;
    }
    if let Some(name) = uri.strip_prefix('!') {
        return TagSpec::Local(name.to_owned());
    }
    let Some(suffix) = uri.strip_prefix(YAML_ORG_PREFIX) else {
        return TagSpec::Ignored;
    };
    match suffix {
        "null" => TagSpec::Core(CoreTag::Null),
        "bool" => TagSpec::Core(CoreTag::Bool),
        "int" => TagSpec::Core(CoreTag::Int),
        "float" => TagSpec::Core(CoreTag::Float),
        "str" => TagSpec::Core(CoreTag::Str),
        "seq" => TagSpec::Core(CoreTag::Seq),
        "map" => TagSpec::Core(CoreTag::Map),
        _ => TagSpec::Ignored,
    }
}

/// Apply `spec` to an already-built node.
///
/// Scalars reach this function *unresolved*, as their raw text plus whether
/// they were written plainly, because `!!int "42"` has to see `42` and not
/// the string the quoting would otherwise have produced.
pub(crate) fn apply_to_collection(spec: &TagSpec, value: Value) -> Value {
    match spec {
        TagSpec::Local(name) => Value::from(crate::value::TaggedValue {
            tag: Tag::new(name.clone()),
            value,
        }),
        // A core scalar tag on a collection, or `!!seq`/`!!map` on the
        // collection it already is: nothing to do either way.
        _ => value,
    }
}

/// Resolve a scalar under `spec`.
///
/// `plain` says whether the scalar was written without quotes or a block
/// indicator, which is what decides between core-schema resolution and "this
/// is unconditionally a string".
pub(crate) fn apply_to_scalar(
    spec: &TagSpec,
    text: &str,
    plain: bool,
    span: Span,
) -> Result<Value> {
    match spec {
        TagSpec::Core(core) if core.is_scalar_tag() => force_scalar(*core, text, span),
        TagSpec::Local(name) => Ok(Value::from(crate::value::TaggedValue {
            tag: Tag::new(name.clone()),
            value: default_scalar(text, plain, span)?,
        })),
        // A global tag this crate does not implement still says "this node
        // is not a plain scalar", so core-schema resolution must not run:
        // `!!binary 42` is the *text* `42`, never the integer. This is also
        // what `serde_yaml` does, which keeps the two in step on every
        // `!!`-tagged document.
        TagSpec::Ignored => Ok(Value::String(text.to_owned())),
        // `!!seq`/`!!map` on something that turned out to be a scalar, or no
        // tag at all: resolve normally.
        TagSpec::Core(_) | TagSpec::NonSpecific => default_scalar(text, plain, span),
    }
}

/// A scalar with no type-forcing tag: core-schema resolution when it was
/// written plainly, an unconditional string otherwise.
fn default_scalar(text: &str, plain: bool, span: Span) -> Result<Value> {
    if !plain {
        return Ok(Value::String(text.to_owned()));
    }
    match resolve::resolve_plain(text) {
        Resolved::Value(value) => Ok(value),
        Resolved::IntegerOutOfRange => Err(Error::at(
            ErrorKind::IntegerOutOfRange {
                literal: text.to_owned(),
            },
            span,
        )),
    }
}

fn force_scalar(core: CoreTag, text: &str, span: Span) -> Result<Value> {
    let mismatch = || {
        Err(Error::at(
            ErrorKind::TagMismatch {
                tag: core.as_str(),
                literal: text.to_owned(),
            },
            span,
        ))
    };
    match core {
        CoreTag::Str => Ok(Value::String(text.to_owned())),
        CoreTag::Null => match resolve::resolve_null(text) {
            Some(value) => Ok(value),
            None => mismatch(),
        },
        CoreTag::Bool => match resolve::resolve_bool(text) {
            Some(value) => Ok(Value::Bool(value)),
            None => mismatch(),
        },
        CoreTag::Int => match resolve::parse_integer(text) {
            IntOutcome::Parsed(number) => Ok(Value::Number(number)),
            IntOutcome::OutOfRange => Err(Error::at(
                ErrorKind::IntegerOutOfRange {
                    literal: text.to_owned(),
                },
                span,
            )),
            IntOutcome::NotAnInteger => mismatch(),
        },
        CoreTag::Float => {
            if let Some(number) = resolve::parse_float(text) {
                return Ok(Value::Number(number));
            }
            // `!!float 1` is the float 1.0: an integer literal is a
            // perfectly good way to write a float, and refusing it would
            // make `!!float` useless on whole numbers.
            match resolve::parse_integer(text) {
                IntOutcome::Parsed(number) => Ok(Value::Number(Number::from(
                    number.as_f64().unwrap_or(f64::NAN),
                ))),
                _ => mismatch(),
            }
        }
        CoreTag::Seq | CoreTag::Map => default_scalar(text, true, span),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;
    use crate::position::Position;

    fn span() -> Span {
        Span::point(Position::start())
    }

    #[test]
    fn the_seven_core_tags_classify_as_core() {
        for (suffix, expected) in [
            ("null", CoreTag::Null),
            ("bool", CoreTag::Bool),
            ("int", CoreTag::Int),
            ("float", CoreTag::Float),
            ("str", CoreTag::Str),
            ("seq", CoreTag::Seq),
            ("map", CoreTag::Map),
        ] {
            let uri = format!("{YAML_ORG_PREFIX}{suffix}");
            assert_eq!(classify(&uri), TagSpec::Core(expected), "{uri}");
        }
    }

    #[test]
    fn other_yaml_org_tags_are_ignored_not_errors() {
        for suffix in [
            "binary",
            "timestamp",
            "omap",
            "set",
            "merge",
            "value",
            "pairs",
        ] {
            let uri = format!("{YAML_ORG_PREFIX}{suffix}");
            assert_eq!(classify(&uri), TagSpec::Ignored, "{uri}");
        }
        assert_eq!(classify("tag:example.com,2000:thing"), TagSpec::Ignored);
    }

    #[test]
    fn local_and_non_specific_tags_are_distinguished() {
        assert_eq!(classify("!"), TagSpec::NonSpecific);
        assert_eq!(classify("!Circle"), TagSpec::Local("Circle".into()));
        assert_eq!(classify("!a/b"), TagSpec::Local("a/b".into()));
    }

    #[test]
    fn a_core_tag_forces_a_scalars_type() {
        let str_tag = TagSpec::Core(CoreTag::Str);
        assert_eq!(
            apply_to_scalar(&str_tag, "1", true, span()).expect("str"),
            Value::from("1")
        );
        let int_tag = TagSpec::Core(CoreTag::Int);
        assert_eq!(
            apply_to_scalar(&int_tag, "42", false, span()).expect("int"),
            Value::from(42u64)
        );
        assert_eq!(
            apply_to_scalar(&int_tag, "0x10", true, span()).expect("hex"),
            Value::from(16u64)
        );
        let float_tag = TagSpec::Core(CoreTag::Float);
        assert_eq!(
            apply_to_scalar(&float_tag, "1", true, span()).expect("float"),
            Value::from(1.0)
        );
        assert_eq!(
            apply_to_scalar(&float_tag, ".inf", true, span())
                .expect("inf")
                .as_f64(),
            Some(f64::INFINITY)
        );
        let null_tag = TagSpec::Core(CoreTag::Null);
        assert_eq!(
            apply_to_scalar(&null_tag, "~", true, span()).expect("null"),
            Value::Null
        );
    }

    #[test]
    fn a_core_tag_rejects_a_scalar_it_cannot_type() {
        for (tag, text) in [
            (CoreTag::Int, "1e3"),
            (CoreTag::Bool, "yes"),
            (CoreTag::Null, "x"),
            (CoreTag::Float, "abc"),
        ] {
            let error =
                apply_to_scalar(&TagSpec::Core(tag), text, true, span()).expect_err("mismatch");
            assert!(
                matches!(error.kind(), ErrorKind::TagMismatch { .. }),
                "{tag:?} {text}: {error}"
            );
        }
        let overflow = apply_to_scalar(
            &TagSpec::Core(CoreTag::Int),
            "18446744073709551616",
            true,
            span(),
        )
        .expect_err("overflow");
        assert!(matches!(
            overflow.kind(),
            ErrorKind::IntegerOutOfRange { .. }
        ));
        let huge = apply_to_scalar(&TagSpec::Core(CoreTag::Float), "1e400", true, span())
            .expect_err("overflow");
        assert!(matches!(huge.kind(), ErrorKind::TagMismatch { .. }));
    }

    #[test]
    fn a_collection_tag_on_a_scalar_falls_back_to_plain_resolution() {
        for tag in [CoreTag::Seq, CoreTag::Map] {
            assert_eq!(
                apply_to_scalar(&TagSpec::Core(tag), "abc", true, span()).expect("scalar"),
                Value::from("abc")
            );
        }
    }

    #[test]
    fn quoting_defeats_core_resolution_but_not_an_explicit_tag() {
        assert_eq!(
            apply_to_scalar(&TagSpec::NonSpecific, "1", false, span()).expect("quoted"),
            Value::from("1")
        );
        assert_eq!(
            apply_to_scalar(&TagSpec::NonSpecific, "1", true, span()).expect("plain"),
            Value::from(1u64)
        );
    }

    #[test]
    fn an_unimplemented_global_tag_keeps_the_scalars_text() {
        // The node is tagged, so it is not a plain scalar and the core
        // schema must not touch it — `!!binary 42` is text, not 42.
        for text in ["true", "42", "0x10", ".inf", "~", ""] {
            assert_eq!(
                apply_to_scalar(&TagSpec::Ignored, text, true, span()).expect("ignored"),
                Value::String(text.to_owned()),
                "{text:?}"
            );
        }
        // `!!seq` on a scalar stays ignorable, and resolves normally.
        assert_eq!(
            apply_to_scalar(&TagSpec::Core(CoreTag::Seq), "true", true, span()).expect("seq"),
            Value::Bool(true)
        );
    }

    #[test]
    fn a_local_tag_wraps_both_scalars_and_collections() {
        let tag = TagSpec::Local("Circle".into());
        let scalar = apply_to_scalar(&tag, "1", true, span()).expect("scalar");
        assert_eq!(scalar.tag().map(Tag::as_str), Some("Circle"));
        assert_eq!(scalar.untagged(), &Value::from(1u64));

        let collection = apply_to_collection(&tag, Value::Sequence(vec![]));
        assert_eq!(collection.tag().map(Tag::as_str), Some("Circle"));

        let untouched = apply_to_collection(&TagSpec::Core(CoreTag::Seq), Value::Sequence(vec![]));
        assert_eq!(untouched, Value::Sequence(vec![]));
    }

    #[test]
    fn core_tag_names_render_with_the_secondary_handle() {
        assert_eq!(CoreTag::Int.as_str(), "!!int");
        assert!(CoreTag::Str.is_scalar_tag());
        assert!(!CoreTag::Seq.is_scalar_tag());
        assert_eq!(SECONDARY_HANDLE_PREFIX, YAML_ORG_PREFIX);
    }
}
