//! Property tests: `parse(to_string(v)) == v`, for arbitrary values.
//!
//! The emitter's job is not "produce readable YAML" but "produce YAML that
//! reads back as the value it came from", and the two are not the same
//! problem. A string that happens to spell `true`, a mapping keyed by a
//! sequence, a float that is `-0.0`, a scalar with a tab in it, a multi-line
//! string whose last line has a trailing space — each of these is a place
//! where a plausible emitter silently loses information. Generating them at
//! random is the only way to be confident none is left.
//!
//! The generator is deliberately hostile: its string strategy includes a
//! second alternative drawn purely from YAML's indicator characters, so
//! `#`, `: `, `- `, quotes and brackets show up far more often than they
//! would in natural text.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_yaml::{Limits, Mapping, Tag, TaggedValue, Value};
use proptest::prelude::*;

/// Arbitrary values, four levels deep, with hostile scalars.
fn any_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<u64>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<f64>().prop_map(Value::from),
        ".{0,12}".prop_map(Value::from),
        // Nothing but YAML indicators, whitespace and line breaks.
        "[ \t\n#:'\"|>&*!%@`,\\[\\]{}\\-?]{0,6}".prop_map(Value::from),
    ];
    leaf.prop_recursive(4, 40, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Sequence),
            prop::collection::vec((inner.clone(), inner.clone()), 0..4).prop_map(|pairs| {
                let mut mapping = Mapping::new();
                for (key, value) in pairs {
                    // `insert` deduplicates, which is what keeps the
                    // generated mapping re-parsable: a duplicate key is a
                    // parse error by design.
                    mapping.insert(key, value);
                }
                Value::Mapping(mapping)
            }),
            ("[A-Za-z][A-Za-z0-9_]{0,5}", inner).prop_map(|(tag, value)| {
                Value::from(TaggedValue {
                    tag: Tag::new(tag),
                    value,
                })
            }),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2048))]

    /// The headline property.
    #[test]
    fn emitting_and_re_parsing_is_the_identity(value in any_value()) {
        let text = astrs_yaml::to_string(&value)
            .unwrap_or_else(|error| panic!("emit failed: {error}"));
        let reread = match astrs_yaml::parse_str(&text) {
            Ok(reread) => reread,
            Err(error) => panic!("re-parse failed: {error}\n--- emitted ---\n{text}"),
        };
        prop_assert_eq!(&reread, &value, "\n--- emitted ---\n{}", text);
    }

    /// Emission is a function of the value, not of how it was obtained.
    #[test]
    fn emission_is_deterministic(value in any_value()) {
        let once = astrs_yaml::to_string(&value).expect("emit");
        let twice = astrs_yaml::to_string(&value).expect("emit");
        prop_assert_eq!(once, twice);
    }

    /// A second round trip changes nothing, so the emitted form is a fixed
    /// point rather than merely equivalent.
    #[test]
    fn the_emitted_text_is_a_fixed_point(value in any_value()) {
        let once = astrs_yaml::to_string(&value).expect("emit");
        let reread = astrs_yaml::parse_str(&once).expect("parse");
        let twice = astrs_yaml::to_string(&reread).expect("emit");
        prop_assert_eq!(once, twice);
    }

    /// Values survive the `serde` layer as well as the text layer.
    #[test]
    fn to_value_and_from_value_are_inverses(value in any_value()) {
        let converted = astrs_yaml::to_value(&value).expect("to_value");
        prop_assert_eq!(&converted, &value);
        let back: Value = astrs_yaml::from_value(converted).expect("from_value");
        prop_assert_eq!(&back, &value);
        let borrowed: Value = astrs_yaml::from_borrowed_value(&value).expect("borrowed");
        prop_assert_eq!(&borrowed, &value);
    }

    /// Parsing never panics, whatever the bytes say — and any failure is a
    /// reported error, never a hang or an abort.
    #[test]
    fn parsing_arbitrary_text_never_panics(text in ".{0,200}") {
        let _ = astrs_yaml::parse_str_multi(&text);
    }

    /// Nesting deeper than the limit is refused rather than recursed into.
    #[test]
    fn deep_nesting_is_refused_not_recursed(depth in 129usize..400) {
        let text = format!("{}{}", "[".repeat(depth), "]".repeat(depth));
        let error = astrs_yaml::parse_str(&text).expect_err("too deep");
        prop_assert!(error.is_limit());
    }

    /// Whatever the limits, a document that parses under them re-parses to
    /// the same value under them.
    #[test]
    fn limits_do_not_change_accepted_values(value in any_value(), depth in 1usize..12) {
        let limits = Limits::default().with_max_depth(depth);
        let text = astrs_yaml::to_string(&value).expect("emit");
        if let Ok(parsed) = astrs_yaml::parse_str_multi_with(&text, limits) {
            prop_assert_eq!(parsed.first(), Some(&value));
        }
    }
}
