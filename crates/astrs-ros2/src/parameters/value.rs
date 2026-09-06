//! [`ParameterValue`]: the ten-way variant `rcl_interfaces` models as a
//! struct with ten fields and a discriminant.
//!
//! `rcl_interfaces/msg/ParameterValue` is an IDL union written the way IDL
//! unions were written before IDL had unions: every arm is a struct member,
//! and a `uint8 type` says which one is real. That is fine on the wire and
//! miserable in a program — nothing stops a caller from setting `type` to
//! `PARAMETER_INTEGER` and filling `string_value`.
//!
//! So the API is a real Rust enum, and [`ParameterValue::to_message`] /
//! [`ParameterValue::from_message`] are the only two places that know about
//! the ten-field form. A value read off the wire with a `type` that does not
//! match any populated field still decodes — to whatever `type` says, with
//! the corresponding field's default — because rejecting it would mean one
//! misbehaving peer could break a whole parameter listing.
//!
//! # The type codes
//!
//! From `rcl_interfaces/msg/ParameterType`, and asserted against the
//! generated constants in this module's tests rather than transcribed and
//! hoped for:
//!
//! | Code | Variant |
//! |---:|---|
//! | 0 | [`NotSet`](ParameterValue::NotSet) |
//! | 1 | [`Bool`](ParameterValue::Bool) |
//! | 2 | [`Integer`](ParameterValue::Integer) |
//! | 3 | [`Double`](ParameterValue::Double) |
//! | 4 | [`String`](ParameterValue::String) |
//! | 5 | [`ByteArray`](ParameterValue::ByteArray) |
//! | 6 | [`BoolArray`](ParameterValue::BoolArray) |
//! | 7 | [`IntegerArray`](ParameterValue::IntegerArray) |
//! | 8 | [`DoubleArray`](ParameterValue::DoubleArray) |
//! | 9 | [`StringArray`](ParameterValue::StringArray) |

use core::fmt;

use crate::msg::rcl_interfaces;

/// `PARAMETER_NOT_SET`.
pub const TYPE_NOT_SET: u8 = 0;
/// `PARAMETER_BOOL`.
pub const TYPE_BOOL: u8 = 1;
/// `PARAMETER_INTEGER`.
pub const TYPE_INTEGER: u8 = 2;
/// `PARAMETER_DOUBLE`.
pub const TYPE_DOUBLE: u8 = 3;
/// `PARAMETER_STRING`.
pub const TYPE_STRING: u8 = 4;
/// `PARAMETER_BYTE_ARRAY`.
pub const TYPE_BYTE_ARRAY: u8 = 5;
/// `PARAMETER_BOOL_ARRAY`.
pub const TYPE_BOOL_ARRAY: u8 = 6;
/// `PARAMETER_INTEGER_ARRAY`.
pub const TYPE_INTEGER_ARRAY: u8 = 7;
/// `PARAMETER_DOUBLE_ARRAY`.
pub const TYPE_DOUBLE_ARRAY: u8 = 8;
/// `PARAMETER_STRING_ARRAY`.
pub const TYPE_STRING_ARRAY: u8 = 9;

/// One parameter's value.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum ParameterValue {
    /// The parameter exists but holds nothing — what an undeclared or
    /// unset parameter reads as.
    #[default]
    NotSet,
    /// A boolean.
    Bool(bool),
    /// A 64-bit signed integer.
    Integer(i64),
    /// A 64-bit float.
    Double(f64),
    /// A UTF-8 string.
    String(String),
    /// A byte string.
    ByteArray(Vec<u8>),
    /// A sequence of booleans.
    BoolArray(Vec<bool>),
    /// A sequence of integers.
    IntegerArray(Vec<i64>),
    /// A sequence of floats.
    DoubleArray(Vec<f64>),
    /// A sequence of strings.
    StringArray(Vec<String>),
}

impl ParameterValue {
    /// The `ParameterType` code for this variant.
    #[must_use]
    pub const fn type_code(&self) -> u8 {
        match self {
            Self::NotSet => TYPE_NOT_SET,
            Self::Bool(_) => TYPE_BOOL,
            Self::Integer(_) => TYPE_INTEGER,
            Self::Double(_) => TYPE_DOUBLE,
            Self::String(_) => TYPE_STRING,
            Self::ByteArray(_) => TYPE_BYTE_ARRAY,
            Self::BoolArray(_) => TYPE_BOOL_ARRAY,
            Self::IntegerArray(_) => TYPE_INTEGER_ARRAY,
            Self::DoubleArray(_) => TYPE_DOUBLE_ARRAY,
            Self::StringArray(_) => TYPE_STRING_ARRAY,
        }
    }

    /// The name `ros2 param describe` prints for this type.
    #[must_use]
    pub const fn type_name(&self) -> &'static str {
        type_name_of(self.type_code())
    }

    /// True when the parameter holds nothing.
    #[must_use]
    pub const fn is_not_set(&self) -> bool {
        matches!(self, Self::NotSet)
    }

    /// True when `other` is the same variant, whatever it holds.
    #[must_use]
    pub const fn same_type_as(&self, other: &Self) -> bool {
        self.type_code() == other.type_code()
    }

    /// The empty value of a given type code.
    ///
    /// What a declaration with a type but no value produces, and what a
    /// `get` on a declared-but-unset parameter returns.
    #[must_use]
    pub fn empty_of(type_code: u8) -> Self {
        match type_code {
            TYPE_BOOL => Self::Bool(false),
            TYPE_INTEGER => Self::Integer(0),
            TYPE_DOUBLE => Self::Double(0.0),
            TYPE_STRING => Self::String(String::new()),
            TYPE_BYTE_ARRAY => Self::ByteArray(Vec::new()),
            TYPE_BOOL_ARRAY => Self::BoolArray(Vec::new()),
            TYPE_INTEGER_ARRAY => Self::IntegerArray(Vec::new()),
            TYPE_DOUBLE_ARRAY => Self::DoubleArray(Vec::new()),
            TYPE_STRING_ARRAY => Self::StringArray(Vec::new()),
            _ => Self::NotSet,
        }
    }

    /// The boolean inside, if this is one.
    #[must_use]
    pub const fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// The integer inside, if this is one.
    #[must_use]
    pub const fn as_integer(&self) -> Option<i64> {
        match self {
            Self::Integer(value) => Some(*value),
            _ => None,
        }
    }

    /// The float inside, if this is one.
    #[must_use]
    pub const fn as_double(&self) -> Option<f64> {
        match self {
            Self::Double(value) => Some(*value),
            _ => None,
        }
    }

    /// The string inside, if this is one.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    /// The string sequence inside, if this is one.
    #[must_use]
    pub fn as_string_array(&self) -> Option<&[String]> {
        match self {
            Self::StringArray(values) => Some(values),
            _ => None,
        }
    }

    /// Render as the ten-field wire message.
    #[must_use]
    pub fn to_message(&self) -> rcl_interfaces::ParameterValue {
        let mut message = rcl_interfaces::ParameterValue {
            r#type: self.type_code(),
            ..rcl_interfaces::ParameterValue::default()
        };
        match self {
            Self::NotSet => {}
            Self::Bool(value) => message.bool_value = *value,
            Self::Integer(value) => message.integer_value = *value,
            Self::Double(value) => message.double_value = *value,
            Self::String(value) => message.string_value.clone_from(value),
            Self::ByteArray(values) => message.byte_array_value.clone_from(values),
            Self::BoolArray(values) => message.bool_array_value.clone_from(values),
            Self::IntegerArray(values) => message.integer_array_value.clone_from(values),
            Self::DoubleArray(values) => message.double_array_value.clone_from(values),
            Self::StringArray(values) => message.string_array_value.clone_from(values),
        }
        message
    }

    /// Read the ten-field wire message.
    ///
    /// The discriminant wins: a message whose `type` says `INTEGER` reads as
    /// an integer even if a peer also filled `string_value`. An unknown
    /// `type` reads as [`NotSet`](Self::NotSet) rather than failing.
    #[must_use]
    pub fn from_message(message: &rcl_interfaces::ParameterValue) -> Self {
        match message.r#type {
            TYPE_BOOL => Self::Bool(message.bool_value),
            TYPE_INTEGER => Self::Integer(message.integer_value),
            TYPE_DOUBLE => Self::Double(message.double_value),
            TYPE_STRING => Self::String(message.string_value.clone()),
            TYPE_BYTE_ARRAY => Self::ByteArray(message.byte_array_value.clone()),
            TYPE_BOOL_ARRAY => Self::BoolArray(message.bool_array_value.clone()),
            TYPE_INTEGER_ARRAY => Self::IntegerArray(message.integer_array_value.clone()),
            TYPE_DOUBLE_ARRAY => Self::DoubleArray(message.double_array_value.clone()),
            TYPE_STRING_ARRAY => Self::StringArray(message.string_array_value.clone()),
            _ => Self::NotSet,
        }
    }
}

/// The name `ros2 param describe` prints for a type code.
#[must_use]
pub const fn type_name_of(type_code: u8) -> &'static str {
    match type_code {
        TYPE_BOOL => "bool",
        TYPE_INTEGER => "integer",
        TYPE_DOUBLE => "double",
        TYPE_STRING => "string",
        TYPE_BYTE_ARRAY => "byte array",
        TYPE_BOOL_ARRAY => "bool array",
        TYPE_INTEGER_ARRAY => "integer array",
        TYPE_DOUBLE_ARRAY => "double array",
        TYPE_STRING_ARRAY => "string array",
        _ => "not set",
    }
}

impl fmt::Display for ParameterValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSet => formatter.write_str("<not set>"),
            Self::Bool(value) => write!(formatter, "{value}"),
            Self::Integer(value) => write!(formatter, "{value}"),
            Self::Double(value) => write!(formatter, "{value}"),
            Self::String(value) => write!(formatter, "{value}"),
            Self::ByteArray(values) => write!(formatter, "[{} bytes]", values.len()),
            Self::BoolArray(values) => write_list(formatter, values),
            Self::IntegerArray(values) => write_list(formatter, values),
            Self::DoubleArray(values) => write_list(formatter, values),
            Self::StringArray(values) => write_list(formatter, values),
        }
    }
}

/// Render a sequence as `[a, b, c]`.
fn write_list<T: fmt::Display>(formatter: &mut fmt::Formatter<'_>, values: &[T]) -> fmt::Result {
    formatter.write_str("[")?;
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            formatter.write_str(", ")?;
        }
        write!(formatter, "{value}")?;
    }
    formatter.write_str("]")
}

impl From<bool> for ParameterValue {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<i64> for ParameterValue {
    fn from(value: i64) -> Self {
        Self::Integer(value)
    }
}

impl From<i32> for ParameterValue {
    fn from(value: i32) -> Self {
        Self::Integer(i64::from(value))
    }
}

impl From<f64> for ParameterValue {
    fn from(value: f64) -> Self {
        Self::Double(value)
    }
}

impl From<String> for ParameterValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for ParameterValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

impl From<Vec<i64>> for ParameterValue {
    fn from(values: Vec<i64>) -> Self {
        Self::IntegerArray(values)
    }
}

impl From<Vec<f64>> for ParameterValue {
    fn from(values: Vec<f64>) -> Self {
        Self::DoubleArray(values)
    }
}

impl From<Vec<bool>> for ParameterValue {
    fn from(values: Vec<bool>) -> Self {
        Self::BoolArray(values)
    }
}

impl From<Vec<String>> for ParameterValue {
    fn from(values: Vec<String>) -> Self {
        Self::StringArray(values)
    }
}

impl From<&ParameterValue> for rcl_interfaces::ParameterValue {
    fn from(value: &ParameterValue) -> Self {
        value.to_message()
    }
}

impl From<&rcl_interfaces::ParameterValue> for ParameterValue {
    fn from(message: &rcl_interfaces::ParameterValue) -> Self {
        ParameterValue::from_message(message)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// Every variant, with a non-default payload, so a round-trip test
    /// cannot pass by both sides producing a default.
    fn every_variant() -> Vec<ParameterValue> {
        vec![
            ParameterValue::NotSet,
            ParameterValue::Bool(true),
            ParameterValue::Integer(-7),
            ParameterValue::Double(1.5),
            ParameterValue::String("hello".to_owned()),
            ParameterValue::ByteArray(vec![1, 2, 3]),
            ParameterValue::BoolArray(vec![true, false]),
            ParameterValue::IntegerArray(vec![1, -2, 3]),
            ParameterValue::DoubleArray(vec![0.5, -0.25]),
            ParameterValue::StringArray(vec!["a".to_owned(), "b".to_owned()]),
        ]
    }

    #[test]
    fn the_type_codes_match_the_generated_constants() {
        use rcl_interfaces::ParameterType as Codes;
        assert_eq!(TYPE_NOT_SET, Codes::PARAMETER_NOT_SET);
        assert_eq!(TYPE_BOOL, Codes::PARAMETER_BOOL);
        assert_eq!(TYPE_INTEGER, Codes::PARAMETER_INTEGER);
        assert_eq!(TYPE_DOUBLE, Codes::PARAMETER_DOUBLE);
        assert_eq!(TYPE_STRING, Codes::PARAMETER_STRING);
        assert_eq!(TYPE_BYTE_ARRAY, Codes::PARAMETER_BYTE_ARRAY);
        assert_eq!(TYPE_BOOL_ARRAY, Codes::PARAMETER_BOOL_ARRAY);
        assert_eq!(TYPE_INTEGER_ARRAY, Codes::PARAMETER_INTEGER_ARRAY);
        assert_eq!(TYPE_DOUBLE_ARRAY, Codes::PARAMETER_DOUBLE_ARRAY);
        assert_eq!(TYPE_STRING_ARRAY, Codes::PARAMETER_STRING_ARRAY);
    }

    #[test]
    fn every_variant_round_trips_through_the_wire_message() {
        for value in every_variant() {
            let message = value.to_message();
            assert_eq!(message.r#type, value.type_code());
            assert_eq!(
                ParameterValue::from_message(&message),
                value,
                "{value} did not survive the round trip"
            );
        }
    }

    #[test]
    fn the_type_codes_are_all_distinct() {
        let mut codes: Vec<u8> = every_variant()
            .iter()
            .map(ParameterValue::type_code)
            .collect();
        codes.sort_unstable();
        let before = codes.len();
        codes.dedup();
        assert_eq!(codes.len(), before);
        assert_eq!(codes, (0..=9).collect::<Vec<u8>>());
    }

    #[test]
    fn the_discriminant_wins_over_a_populated_field() {
        // A peer that fills `string_value` and says `INTEGER`: the value is
        // an integer, and the string is not silently promoted.
        let message = rcl_interfaces::ParameterValue {
            r#type: TYPE_INTEGER,
            integer_value: 5,
            string_value: "not this".to_owned(),
            ..rcl_interfaces::ParameterValue::default()
        };
        assert_eq!(
            ParameterValue::from_message(&message),
            ParameterValue::Integer(5)
        );
    }

    #[test]
    fn an_unknown_type_code_reads_as_not_set_rather_than_failing() {
        let message = rcl_interfaces::ParameterValue {
            r#type: 250,
            integer_value: 5,
            ..rcl_interfaces::ParameterValue::default()
        };
        assert_eq!(
            ParameterValue::from_message(&message),
            ParameterValue::NotSet,
            "a future type code must not break a whole parameter listing"
        );
    }

    #[test]
    fn the_empty_value_of_a_type_has_that_type() {
        for code in 0_u8..=9 {
            let empty = ParameterValue::empty_of(code);
            assert_eq!(
                empty.type_code(),
                code,
                "empty_of({code}) has the wrong type"
            );
        }
        assert_eq!(ParameterValue::empty_of(200), ParameterValue::NotSet);
        assert!(ParameterValue::empty_of(TYPE_STRING).as_str() == Some(""));
    }

    #[test]
    fn type_equality_ignores_the_payload() {
        assert!(ParameterValue::Integer(1).same_type_as(&ParameterValue::Integer(99)));
        assert!(!ParameterValue::Integer(1).same_type_as(&ParameterValue::Double(1.0)));
        assert!(ParameterValue::default().is_not_set());
    }

    #[test]
    fn the_accessors_return_only_their_own_variant() {
        assert_eq!(ParameterValue::Bool(true).as_bool(), Some(true));
        assert_eq!(ParameterValue::Integer(1).as_bool(), None);
        assert_eq!(ParameterValue::Integer(7).as_integer(), Some(7));
        assert_eq!(ParameterValue::Double(0.5).as_double(), Some(0.5));
        assert_eq!(ParameterValue::Double(0.5).as_integer(), None);
        assert_eq!(ParameterValue::String("x".to_owned()).as_str(), Some("x"));
        assert_eq!(ParameterValue::NotSet.as_str(), None);
        assert_eq!(
            ParameterValue::StringArray(vec!["a".to_owned()])
                .as_string_array()
                .map(<[String]>::len),
            Some(1)
        );
    }

    #[test]
    fn the_type_names_are_the_ones_ros2_param_prints() {
        assert_eq!(ParameterValue::NotSet.type_name(), "not set");
        assert_eq!(ParameterValue::Bool(true).type_name(), "bool");
        assert_eq!(ParameterValue::Integer(1).type_name(), "integer");
        assert_eq!(ParameterValue::Double(1.0).type_name(), "double");
        assert_eq!(ParameterValue::String(String::new()).type_name(), "string");
        assert_eq!(
            ParameterValue::StringArray(Vec::new()).type_name(),
            "string array"
        );
        assert_eq!(type_name_of(200), "not set");
    }

    #[test]
    fn the_from_impls_cover_the_common_literals() {
        assert_eq!(ParameterValue::from(true), ParameterValue::Bool(true));
        assert_eq!(ParameterValue::from(7_i64), ParameterValue::Integer(7));
        assert_eq!(ParameterValue::from(7_i32), ParameterValue::Integer(7));
        assert_eq!(ParameterValue::from(0.5_f64), ParameterValue::Double(0.5));
        assert_eq!(
            ParameterValue::from("x"),
            ParameterValue::String("x".to_owned())
        );
        assert_eq!(
            ParameterValue::from("x".to_owned()),
            ParameterValue::String("x".to_owned())
        );
        assert_eq!(
            ParameterValue::from(vec![1_i64]),
            ParameterValue::IntegerArray(vec![1])
        );
        assert_eq!(
            ParameterValue::from(vec![0.5_f64]),
            ParameterValue::DoubleArray(vec![0.5])
        );
        assert_eq!(
            ParameterValue::from(vec![true]),
            ParameterValue::BoolArray(vec![true])
        );
        assert_eq!(
            ParameterValue::from(vec!["x".to_owned()]),
            ParameterValue::StringArray(vec!["x".to_owned()])
        );
    }

    #[test]
    fn the_display_forms_are_readable() {
        assert_eq!(ParameterValue::NotSet.to_string(), "<not set>");
        assert_eq!(ParameterValue::Integer(-3).to_string(), "-3");
        assert_eq!(ParameterValue::String("hi".to_owned()).to_string(), "hi");
        assert_eq!(
            ParameterValue::IntegerArray(vec![1, 2, 3]).to_string(),
            "[1, 2, 3]"
        );
        assert_eq!(
            ParameterValue::ByteArray(vec![1, 2, 3]).to_string(),
            "[3 bytes]"
        );
        assert_eq!(ParameterValue::StringArray(Vec::new()).to_string(), "[]");
    }

    #[test]
    fn the_wire_message_round_trips_through_cdr() {
        for value in every_variant() {
            let message = value.to_message();
            let octets = astrs_cdr::to_vec_ros2(&message).expect("encode");
            let decoded =
                astrs_cdr::from_bytes::<rcl_interfaces::ParameterValue>(&octets).expect("decode");
            assert_eq!(ParameterValue::from_message(&decoded), value);
        }
    }
}
