//! [`Parameter`] — the closed value set that message metadata may carry.
//!
//! Blueprint §6.1 pins metadata to a small, fixed set of value shapes:
//! `Bool | Integer | Float | String | ListInt | ListFloat | ListString |
//! Timestamp`. Deliberately closed, and deliberately *not* a general JSON
//! value: metadata rides beside every single message, so a recursive value
//! type would make the hot path pay for arbitrary nesting nobody needs.
//! Anything richer belongs in the payload, which is columnar and typed.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::Parameter;
//!
//! let seq = Parameter::from(42i64);
//! assert_eq!(seq.as_integer(), Some(42));
//! assert_eq!(seq.type_name(), "integer");
//!
//! let labels = Parameter::from(vec!["a".to_owned(), "b".to_owned()]);
//! assert_eq!(labels.as_list_string().map(<[String]>::len), Some(2));
//! ```

use core::fmt;

use astrs_time::HlcTimestamp;
use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

/// One metadata value.
///
/// Variant indices are frozen by the protocol snapshot; new shapes are
/// appended at the tail.
///
/// # Examples
///
/// ```
/// use astrs_wire::Parameter;
///
/// assert!(Parameter::Bool(true).as_bool().unwrap_or(false));
/// assert_eq!(Parameter::Float(1.5).as_float(), Some(1.5));
/// assert_eq!(Parameter::String("x".into()).as_str(), Some("x"));
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Encode, Decode)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Parameter {
    /// A boolean flag — `fin`, `flush`.
    #[oxicode(variant = 0)]
    Bool(bool),
    /// A signed 64-bit integer — `seq`, `segment_id`, `goal_status`.
    #[oxicode(variant = 1)]
    Integer(i64),
    /// A 64-bit float.
    ///
    /// `NaN` is representable and round-trips bit-for-bit through the codec,
    /// but it makes `Parameter` only `PartialEq`, never `Eq` — which is why
    /// neither this type nor [`crate::Metadata`] derives `Eq` or `Hash`.
    #[oxicode(variant = 2)]
    Float(f64),
    /// A UTF-8 string — `request_id`, `goal_id`, `session_id`,
    /// `_schema_hash`.
    #[oxicode(variant = 3)]
    String(String),
    /// A list of signed 64-bit integers.
    #[oxicode(variant = 4)]
    ListInt(Vec<i64>),
    /// A list of 64-bit floats.
    #[oxicode(variant = 5)]
    ListFloat(Vec<f64>),
    /// A list of UTF-8 strings.
    #[oxicode(variant = 6)]
    ListString(Vec<String>),
    /// A hybrid-logical-clock timestamp, for correlating causally with other
    /// events (blueprint §4.3).
    #[oxicode(variant = 7)]
    Timestamp(HlcTimestamp),
}

impl Parameter {
    /// A stable, lower-case name for the value's shape.
    ///
    /// Used in type-mismatch diagnostics and in `astrs topic info --json`.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::Parameter;
    ///
    /// assert_eq!(Parameter::Bool(false).type_name(), "bool");
    /// assert_eq!(Parameter::ListFloat(vec![]).type_name(), "list_float");
    /// ```
    #[must_use]
    pub const fn type_name(&self) -> &'static str {
        match self {
            Self::Bool(_) => "bool",
            Self::Integer(_) => "integer",
            Self::Float(_) => "float",
            Self::String(_) => "string",
            Self::ListInt(_) => "list_int",
            Self::ListFloat(_) => "list_float",
            Self::ListString(_) => "list_string",
            Self::Timestamp(_) => "timestamp",
        }
    }

    /// Whether this value is one of the three list shapes.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::Parameter;
    ///
    /// assert!(Parameter::ListInt(vec![1]).is_list());
    /// assert!(!Parameter::Integer(1).is_list());
    /// ```
    #[must_use]
    pub const fn is_list(&self) -> bool {
        matches!(
            self,
            Self::ListInt(_) | Self::ListFloat(_) | Self::ListString(_)
        )
    }

    /// The number of elements, for list shapes; `1` for scalars.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::Parameter;
    ///
    /// assert_eq!(Parameter::ListString(vec![]).len(), 0);
    /// assert_eq!(Parameter::Bool(true).len(), 1);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::ListInt(values) => values.len(),
            Self::ListFloat(values) => values.len(),
            Self::ListString(values) => values.len(),
            _ => 1,
        }
    }

    /// Whether this is an empty list. Scalars are never empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::Parameter;
    ///
    /// assert!(Parameter::ListInt(vec![]).is_empty());
    /// assert!(!Parameter::Integer(0).is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The boolean value, if this is a [`Parameter::Bool`].
    #[must_use]
    pub const fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// The integer value, if this is a [`Parameter::Integer`].
    #[must_use]
    pub const fn as_integer(&self) -> Option<i64> {
        match self {
            Self::Integer(value) => Some(*value),
            _ => None,
        }
    }

    /// The float value, if this is a [`Parameter::Float`].
    ///
    /// Integers are **not** widened into floats: a silent numeric conversion
    /// in a metadata accessor is exactly the kind of implicit behaviour that
    /// makes a protocol hard to reason about.
    #[must_use]
    pub const fn as_float(&self) -> Option<f64> {
        match self {
            Self::Float(value) => Some(*value),
            _ => None,
        }
    }

    /// The string value, if this is a [`Parameter::String`].
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    /// The integer list, if this is a [`Parameter::ListInt`].
    #[must_use]
    pub fn as_list_int(&self) -> Option<&[i64]> {
        match self {
            Self::ListInt(values) => Some(values),
            _ => None,
        }
    }

    /// The float list, if this is a [`Parameter::ListFloat`].
    #[must_use]
    pub fn as_list_float(&self) -> Option<&[f64]> {
        match self {
            Self::ListFloat(values) => Some(values),
            _ => None,
        }
    }

    /// The string list, if this is a [`Parameter::ListString`].
    #[must_use]
    pub fn as_list_string(&self) -> Option<&[String]> {
        match self {
            Self::ListString(values) => Some(values),
            _ => None,
        }
    }

    /// The timestamp, if this is a [`Parameter::Timestamp`].
    #[must_use]
    pub const fn as_timestamp(&self) -> Option<HlcTimestamp> {
        match self {
            Self::Timestamp(value) => Some(*value),
            _ => None,
        }
    }

    /// Bitwise equality, so that two `NaN` floats compare equal.
    ///
    /// Round-trip tests need a total notion of "the same value came back";
    /// `PartialEq` cannot provide one because IEEE-754 says `NaN != NaN`.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::Parameter;
    ///
    /// let nan = Parameter::Float(f64::NAN);
    /// assert_ne!(nan, Parameter::Float(f64::NAN));
    /// assert!(nan.bitwise_eq(&Parameter::Float(f64::NAN)));
    ///
    /// // Signed zeros are distinguishable bitwise, as they should be.
    /// assert!(!Parameter::Float(0.0).bitwise_eq(&Parameter::Float(-0.0)));
    /// ```
    #[must_use]
    pub fn bitwise_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Float(left), Self::Float(right)) => left.to_bits() == right.to_bits(),
            (Self::ListFloat(left), Self::ListFloat(right)) => {
                left.len() == right.len()
                    && left
                        .iter()
                        .zip(right.iter())
                        .all(|(a, b)| a.to_bits() == b.to_bits())
            }
            _ => self == other,
        }
    }
}

impl fmt::Display for Parameter {
    /// Renders a compact, single-line form suitable for a log field.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::Parameter;
    ///
    /// assert_eq!(Parameter::Integer(-3).to_string(), "-3");
    /// assert_eq!(Parameter::String("hi".into()).to_string(), "hi");
    /// assert_eq!(Parameter::ListInt(vec![1, 2]).to_string(), "[1,2]");
    /// assert_eq!(Parameter::ListString(vec![]).to_string(), "[]");
    /// ```
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        /// Writes a comma-separated list between brackets.
        fn write_list<T: fmt::Display>(f: &mut fmt::Formatter<'_>, values: &[T]) -> fmt::Result {
            f.write_str("[")?;
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    f.write_str(",")?;
                }
                write!(f, "{value}")?;
            }
            f.write_str("]")
        }

        match self {
            Self::Bool(value) => write!(f, "{value}"),
            Self::Integer(value) => write!(f, "{value}"),
            Self::Float(value) => write!(f, "{value}"),
            Self::String(value) => f.write_str(value),
            Self::ListInt(values) => write_list(f, values),
            Self::ListFloat(values) => write_list(f, values),
            Self::ListString(values) => write_list(f, values),
            Self::Timestamp(value) => write!(f, "{value}"),
        }
    }
}

impl From<bool> for Parameter {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<i64> for Parameter {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

impl From<i32> for Parameter {
    fn from(value: i32) -> Self {
        Self::Integer(i64::from(value))
    }
}

impl From<u32> for Parameter {
    fn from(value: u32) -> Self {
        Self::Integer(i64::from(value))
    }
}

impl From<f64> for Parameter {
    fn from(value: f64) -> Self {
        Self::Float(value)
    }
}

impl From<f32> for Parameter {
    fn from(value: f32) -> Self {
        Self::Float(f64::from(value))
    }
}

impl From<String> for Parameter {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for Parameter {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

impl From<Vec<i64>> for Parameter {
    fn from(values: Vec<i64>) -> Self {
        Self::ListInt(values)
    }
}

impl From<Vec<f64>> for Parameter {
    fn from(values: Vec<f64>) -> Self {
        Self::ListFloat(values)
    }
}

impl From<Vec<String>> for Parameter {
    fn from(values: Vec<String>) -> Self {
        Self::ListString(values)
    }
}

impl From<HlcTimestamp> for Parameter {
    fn from(value: HlcTimestamp) -> Self {
        Self::Timestamp(value)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::codec::{WireDecode, WireEncode};

    fn samples() -> Vec<Parameter> {
        vec![
            Parameter::Bool(true),
            Parameter::Bool(false),
            Parameter::Integer(0),
            Parameter::Integer(i64::MIN),
            Parameter::Integer(i64::MAX),
            Parameter::Float(0.0),
            Parameter::Float(-0.0),
            Parameter::Float(1.5),
            Parameter::Float(f64::MIN),
            Parameter::Float(f64::MAX),
            Parameter::Float(f64::INFINITY),
            Parameter::Float(f64::NEG_INFINITY),
            Parameter::Float(f64::NAN),
            Parameter::String(String::new()),
            Parameter::String("hello".to_owned()),
            Parameter::String("日本語".to_owned()),
            Parameter::ListInt(vec![]),
            Parameter::ListInt(vec![-1, 0, 1]),
            Parameter::ListFloat(vec![]),
            Parameter::ListFloat(vec![f64::NAN, 0.5]),
            Parameter::ListString(vec![]),
            Parameter::ListString(vec!["a".to_owned(), String::new()]),
            Parameter::Timestamp(HlcTimestamp::new(1_700_000_000_000_000_000, 7)),
        ]
    }

    #[test]
    fn every_sample_round_trips_bitwise() {
        for sample in samples() {
            let bytes = sample.encode_to_vec().unwrap();
            let decoded = Parameter::decode_exact(&bytes).unwrap();
            assert!(
                sample.bitwise_eq(&decoded),
                "{sample:?} did not survive the wire"
            );
        }
    }

    #[test]
    fn variant_indices_are_frozen() {
        let expected: [(Parameter, u8); 8] = [
            (Parameter::Bool(false), 0),
            (Parameter::Integer(0), 1),
            (Parameter::Float(0.0), 2),
            (Parameter::String(String::new()), 3),
            (Parameter::ListInt(vec![]), 4),
            (Parameter::ListFloat(vec![]), 5),
            (Parameter::ListString(vec![]), 6),
            (Parameter::Timestamp(HlcTimestamp::EPOCH), 7),
        ];
        for (value, index) in expected {
            let bytes = value.encode_to_vec().unwrap();
            assert_eq!(bytes[0], index, "wrong tag for {}", value.type_name());
        }
    }

    #[test]
    fn type_names_are_unique_and_stable() {
        let mut seen = std::collections::BTreeSet::new();
        for sample in samples() {
            seen.insert(sample.type_name());
        }
        assert_eq!(seen.len(), 8);
    }

    #[test]
    fn accessors_only_answer_for_their_own_shape() {
        let value = Parameter::Integer(7);
        assert_eq!(value.as_integer(), Some(7));
        assert_eq!(value.as_float(), None, "integers must not widen to float");
        assert_eq!(value.as_bool(), None);
        assert_eq!(value.as_str(), None);
        assert_eq!(value.as_list_int(), None);
        assert_eq!(value.as_list_float(), None);
        assert_eq!(value.as_list_string(), None);
        assert_eq!(value.as_timestamp(), None);
    }

    #[test]
    fn every_accessor_answers_for_its_own_shape() {
        assert_eq!(Parameter::Bool(true).as_bool(), Some(true));
        assert_eq!(Parameter::Float(2.5).as_float(), Some(2.5));
        assert_eq!(Parameter::String("s".into()).as_str(), Some("s"));
        assert_eq!(Parameter::ListInt(vec![1]).as_list_int(), Some(&[1i64][..]));
        assert_eq!(
            Parameter::ListFloat(vec![1.0]).as_list_float(),
            Some(&[1.0f64][..])
        );
        assert_eq!(
            Parameter::ListString(vec!["s".into()]).as_list_string(),
            Some(&["s".to_owned()][..])
        );
        let ts = HlcTimestamp::new(9, 1);
        assert_eq!(Parameter::Timestamp(ts).as_timestamp(), Some(ts));
    }

    #[test]
    fn list_predicates_and_lengths() {
        assert!(Parameter::ListInt(vec![]).is_list());
        assert!(Parameter::ListFloat(vec![]).is_list());
        assert!(Parameter::ListString(vec![]).is_list());
        assert!(!Parameter::Timestamp(HlcTimestamp::EPOCH).is_list());

        assert_eq!(Parameter::ListInt(vec![1, 2, 3]).len(), 3);
        assert!(Parameter::ListString(vec![]).is_empty());
        assert_eq!(Parameter::Integer(0).len(), 1);
        assert!(!Parameter::Integer(0).is_empty());
    }

    #[test]
    fn bitwise_equality_handles_nan_and_signed_zero() {
        let nan = Parameter::Float(f64::NAN);
        assert_ne!(nan, Parameter::Float(f64::NAN));
        assert!(nan.bitwise_eq(&Parameter::Float(f64::NAN)));
        assert!(!Parameter::Float(0.0).bitwise_eq(&Parameter::Float(-0.0)));
        assert!(
            Parameter::ListFloat(vec![f64::NAN]).bitwise_eq(&Parameter::ListFloat(vec![f64::NAN]))
        );
        assert!(!Parameter::ListFloat(vec![f64::NAN]).bitwise_eq(&Parameter::ListFloat(vec![])));
        assert!(Parameter::Integer(1).bitwise_eq(&Parameter::Integer(1)));
        assert!(!Parameter::Integer(1).bitwise_eq(&Parameter::Bool(true)));
    }

    #[test]
    fn conversions_pick_the_expected_variant() {
        assert_eq!(Parameter::from(true), Parameter::Bool(true));
        assert_eq!(Parameter::from(1i64), Parameter::Integer(1));
        assert_eq!(Parameter::from(1i32), Parameter::Integer(1));
        assert_eq!(Parameter::from(1u32), Parameter::Integer(1));
        assert_eq!(Parameter::from(1.5f64), Parameter::Float(1.5));
        assert_eq!(Parameter::from(0.5f32), Parameter::Float(0.5));
        assert_eq!(Parameter::from("s"), Parameter::String("s".to_owned()));
        assert_eq!(
            Parameter::from("s".to_owned()),
            Parameter::String("s".to_owned())
        );
        assert_eq!(Parameter::from(vec![1i64]), Parameter::ListInt(vec![1]));
        assert_eq!(
            Parameter::from(vec![1.0f64]),
            Parameter::ListFloat(vec![1.0])
        );
        assert_eq!(
            Parameter::from(vec!["s".to_owned()]),
            Parameter::ListString(vec!["s".to_owned()])
        );
        let ts = HlcTimestamp::new(3, 4);
        assert_eq!(Parameter::from(ts), Parameter::Timestamp(ts));
    }

    #[test]
    fn display_is_compact() {
        assert_eq!(Parameter::Bool(true).to_string(), "true");
        assert_eq!(Parameter::Integer(-3).to_string(), "-3");
        assert_eq!(Parameter::String("hi".into()).to_string(), "hi");
        assert_eq!(Parameter::ListInt(vec![1, 2]).to_string(), "[1,2]");
        assert_eq!(Parameter::ListFloat(vec![1.5]).to_string(), "[1.5]");
        assert_eq!(
            Parameter::ListString(vec!["a".into(), "b".into()]).to_string(),
            "[a,b]"
        );
        assert_eq!(Parameter::ListInt(vec![]).to_string(), "[]");
        assert_eq!(
            Parameter::Timestamp(HlcTimestamp::new(5, 2)).to_string(),
            HlcTimestamp::new(5, 2).to_string()
        );
    }

    #[test]
    fn serde_round_trips_every_json_representable_shape() {
        // JSON has no syntax for NaN or the infinities — `serde_json` writes
        // `null` for them — so those samples are excluded here. The wire codec
        // (the one that matters for the protocol) handles them exactly, as
        // `every_sample_round_trips_bitwise` shows.
        let json_representable = |parameter: &Parameter| match parameter {
            Parameter::Float(value) => value.is_finite(),
            Parameter::ListFloat(values) => values.iter().all(|value| value.is_finite()),
            _ => true,
        };

        for sample in samples().into_iter().filter(json_representable) {
            let json = serde_json::to_string(&sample).unwrap();
            let decoded: Parameter = serde_json::from_str(&json).unwrap();
            assert!(sample.bitwise_eq(&decoded), "{sample:?} vs {decoded:?}");
        }
    }

    #[test]
    fn serde_uses_snake_case_variant_names() {
        assert_eq!(
            serde_json::to_string(&Parameter::ListInt(vec![1])).unwrap(),
            r#"{"list_int":[1]}"#
        );
    }

    #[test]
    fn a_truncated_parameter_is_rejected() {
        let bytes = Parameter::String("hello".into()).encode_to_vec().unwrap();
        assert!(Parameter::decode_exact(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn an_unknown_variant_tag_is_rejected() {
        assert!(Parameter::decode_exact(&[200]).is_err());
    }
}
