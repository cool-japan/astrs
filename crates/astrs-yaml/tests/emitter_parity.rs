//! The emitter half of the switchover gate: `astrs_yaml::to_string` must
//! produce the *same bytes* as `serde_yaml::to_string`.
//!
//! `tests/repo_corpus.rs` already checks this on every YAML file in the
//! workspace, but those files are hand-written and their value space is
//! narrow: short strings, small non-negative integers, a couple of floats,
//! and — the gap that matters — **no multi-line strings at all**, so the
//! whole literal-block path (`|`, `|-`, `|+`, `|2-`) never gets compared
//! there. This test generates the shapes the corpus does not contain.
//!
//! # What is excluded, and why
//!
//! Four shapes are skipped, each because `serde_yaml`'s own output is
//! unusable rather than merely different:
//!
//! 1. A **mapping used as a mapping key** — `serde_yaml`'s emitter returns
//!    an error rather than writing it.
//! 2. A **non-inline key** (a multi-line string, a sequence) — `serde_yaml`
//!    writes a block collection value on the `:` line itself (`: - x`),
//!    which YAML 1.2 does not permit and this crate therefore writes on the
//!    following line.
//! 3. **U+2028 / U+2029 / U+0085 / U+FEFF in a string** — `serde_yaml`
//!    writes them raw inside single quotes and pads them with spaces on the
//!    way back, which is lossy; this crate escapes them.
//! 4. A **string of more than eighteen digits** — `serde_yaml` emits it
//!    unquoted, and its own reader then refuses the result; this crate
//!    quotes it.
//!
//! Every one of those is an entry in the switchover's caveat list. Anything
//! else must match byte for byte.
//!
//! Floats are drawn from the shapes a manifest contains (`n/8`, `n/1000`,
//! and the layout thresholds) rather than from random bit patterns: for
//! values needing sixteen or seventeen significant digits the two formatters
//! pick different — equally valid, equally round-tripping — final digits,
//! which `astrs_yaml::Number`'s own tests document and pin.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_yaml::{Mapping, Tag, TaggedValue, Value};
use proptest::prelude::*;

/// Translate a value into `serde_yaml`'s tree, or `None` for a shape its
/// emitter cannot write.
fn to_serde_yaml(value: &Value) -> Option<serde_yaml::Value> {
    Some(match value {
        Value::Null => serde_yaml::Value::Null,
        Value::Bool(inner) => serde_yaml::Value::Bool(*inner),
        Value::Number(number) => {
            if let Some(inner) = number.as_u64() {
                serde_yaml::Value::Number(inner.into())
            } else if let Some(inner) = number.as_i64() {
                serde_yaml::Value::Number(inner.into())
            } else {
                serde_yaml::Value::Number(serde_yaml::Number::from(number.as_f64()?))
            }
        }
        Value::String(inner) => serde_yaml::Value::String(inner.clone()),
        Value::Sequence(items) => serde_yaml::Value::Sequence(
            items
                .iter()
                .map(to_serde_yaml)
                .collect::<Option<Vec<_>>>()?,
        ),
        Value::Mapping(mapping) => {
            let mut out = serde_yaml::Mapping::new();
            for (key, value) in mapping.iter() {
                if !is_writable_key(key) {
                    return None;
                }
                out.insert(to_serde_yaml(key)?, to_serde_yaml(value)?);
            }
            serde_yaml::Value::Mapping(out)
        }
        Value::Tagged(tagged) => {
            serde_yaml::Value::Tagged(Box::new(serde_yaml::value::TaggedValue {
                tag: serde_yaml::value::Tag::new(tagged.tag.as_str()),
                value: to_serde_yaml(&tagged.value)?,
            }))
        }
    })
}

/// True for keys both emitters write the same way: a scalar that needs no
/// escaping, so neither falls back to the explicit `? key` form.
fn is_writable_key(key: &Value) -> bool {
    match key {
        Value::Null | Value::Bool(_) | Value::Number(_) => true,
        Value::String(text) => text
            .chars()
            .all(|ch| ch >= ' ' && !('\u{7f}'..='\u{9f}').contains(&ch)),
        _ => false,
    }
}

/// Strings `serde_yaml` writes in a form its own reader cannot recover.
fn is_unwritable_string(text: &str) -> bool {
    text.contains(['\u{2028}', '\u{2029}', '\u{85}', '\u{feff}'])
        || (text.len() > 18 && text.bytes().all(|byte| byte.is_ascii_digit()))
}

fn contains_excluded(value: &Value) -> bool {
    match value {
        Value::String(text) => is_unwritable_string(text),
        Value::Sequence(items) => items.iter().any(contains_excluded),
        Value::Mapping(mapping) => mapping
            .iter()
            .any(|(key, value)| !is_writable_key(key) || contains_excluded(value)),
        Value::Tagged(tagged) => contains_excluded(&tagged.value),
        _ => false,
    }
}

fn any_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<u64>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        (-100_000i64..100_000).prop_map(|n| Value::from(n as f64 / 8.0)),
        (-100_000i64..100_000).prop_map(|n| Value::from(n as f64 / 1000.0)),
        prop::sample::select(vec![
            0.0f64,
            -0.0,
            1e-6,
            1e-7,
            1e15,
            1e16,
            1e20,
            1e100,
            f64::MAX,
            f64::MIN_POSITIVE,
            5e-324,
            0.1,
            3.5,
            -2.25,
            1.5e300,
        ])
        .prop_map(Value::from),
        ".{0,12}".prop_map(Value::from),
        "[ \t\n#:'\"|>&*!%@`,\\[\\]{}\\-?]{0,6}".prop_map(Value::from),
        // Multi-line strings: the block-scalar path the corpus never reaches.
        "[a-z \n]{0,20}".prop_map(Value::from),
        "( |x|\n){0,10}".prop_map(Value::from),
    ];
    leaf.prop_recursive(3, 24, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..3).prop_map(Value::Sequence),
            prop::collection::vec((inner.clone(), inner.clone()), 0..3).prop_map(|pairs| {
                let mut mapping = Mapping::new();
                for (key, value) in pairs {
                    mapping.insert(key, value);
                }
                Value::Mapping(mapping)
            }),
            ("[A-Za-z][A-Za-z0-9_]{0,3}", inner).prop_map(|(tag, value)| {
                Value::from(TaggedValue {
                    tag: Tag::new(tag),
                    value,
                })
            }),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 2048,
        max_global_rejects: 1_000_000,
        ..ProptestConfig::default()
    })]

    #[test]
    fn both_emitters_write_the_same_bytes(value in any_value()) {
        prop_assume!(!contains_excluded(&value));
        let Some(theirs_value) = to_serde_yaml(&value) else {
            return Ok(());
        };
        let Ok(theirs) = serde_yaml::to_string(&theirs_value) else {
            return Ok(());
        };
        // Where `serde_yaml`'s own output does not read back as the value it
        // came from, it is the one that is wrong; skip rather than assert
        // against a broken reference.
        let Ok(their_reread) = serde_yaml::from_str::<serde_yaml::Value>(&theirs) else {
            return Ok(());
        };
        prop_assume!(their_reread == theirs_value);

        let mine = astrs_yaml::to_string(&value).expect("emit");
        prop_assert_eq!(&mine, &theirs, "\nvalue: {:?}\n", value);
    }
}

/// The float layout window, asserted directly against `serde_yaml` rather
/// than against a remembered constant.
#[test]
fn float_layout_matches_serde_yaml() {
    let mut cases: Vec<f64> = vec![
        0.0,
        -0.0,
        1.0,
        -1.0,
        0.1,
        0.5,
        1.5,
        2.0,
        1e-4,
        1e-5,
        1e-6,
        1e-7,
        1e15,
        1e16,
        1e17,
        1e20,
        1e21,
        1e100,
        123_456_789.0,
        0.0005,
        1000.0,
        f64::MAX,
        f64::MIN_POSITIVE,
        5e-324,
        1.5e300,
        -4.392_382_137_218_392e-5,
    ];
    for n in -2000i64..2000 {
        cases.push(n as f64 / 8.0);
        cases.push(n as f64 / 1000.0);
    }
    for value in cases {
        let mine = astrs_yaml::to_string(&Value::from(value)).expect("emit");
        let theirs =
            serde_yaml::to_string(&serde_yaml::Value::Number(serde_yaml::Number::from(value)))
                .expect("emit");
        assert_eq!(mine, theirs, "{value:e}");
    }
}

/// Whatever the layout, the text must read back to the same bits — the
/// property that outranks byte-parity when the two ever conflict.
#[test]
fn every_emitted_float_round_trips_bit_exactly() {
    let mut state = 0x0BAD_C0DE_1234_5678u64;
    let mut next = move || {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        state.wrapping_mul(0x2545_F491_4F6C_DD1D)
    };
    for _ in 0..20_000 {
        let value = f64::from_bits(next());
        if !value.is_finite() {
            continue;
        }
        let text = astrs_yaml::to_string(&Value::from(value)).expect("emit");
        let reread = astrs_yaml::parse_str(&text).expect("parse");
        let recovered = reread.as_f64().expect("a float");
        assert_eq!(recovered.to_bits(), value.to_bits(), "{text}");
    }
}
