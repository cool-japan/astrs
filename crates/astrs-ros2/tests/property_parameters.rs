//! Property tests over [`ParameterValue`]'s type conversions
//! (`crate::parameters::value`, blueprint §9's parameter surface):
//! `rcl_interfaces/msg/ParameterValue`'s ten-fields-and-a-discriminant wire
//! shape, collapsed to and recovered from a real Rust enum.
//!
//! `crates/astrs-ros2/src/parameters/value.rs` already hand-tests one
//! non-default example of each of the ten variants; this suite generates
//! arbitrary *payloads* for each variant instead — arbitrary integers at
//! the `i64` boundary, arbitrary UTF-8 (subject to the IDL `string`
//! contract's no-interior-NUL rule, matching `astrs-cdr`'s own generators —
//! see `crates/astrs-cdr/tests/property.rs`), arbitrary-length sequences —
//! so a bug that only shows up for, say, an empty `StringArray` or a
//! `Double` at a float boundary has somewhere to be found.
//!
//! Two properties:
//!
//! 1. **The message round trip.** `from_message(&value.to_message()) ==
//!    value`, for every variant. The message's own `type` discriminant
//!    always wins over whichever of the ten fields happen to be non-default
//!    (`value.rs`'s own docs: "nothing stops a caller from setting `type`
//!    to `PARAMETER_INTEGER` and filling `string_value`" — `to_message`
//!    never does this, but the round trip only proves that if the
//!    generator actually varies the payload, which is why every payload
//!    strategy below is a real generator, never a constant).
//! 2. **The wire round trip.** The same message, CDR-encoded and decoded,
//!    reads back as the identical [`ParameterValue`] — composing this
//!    crate's own mangling with `astrs-cdr`'s serializer, the same
//!    combination `crate::parameters::service` sends over the RTPS wire for
//!    real.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_ros2::parameters::ParameterValue;
use proptest::prelude::*;

mod arb {
    use super::*;

    /// A `Double` payload with no NaN: `NaN != NaN` would make a correct
    /// round trip look broken, which is a bug in the property, not in
    /// `ParameterValue` — infinities are kept, since IEEE-754 equality
    /// handles those correctly (`INFINITY == INFINITY`).
    pub fn finite_or_infinite_f64() -> impl Strategy<Value = f64> {
        any::<f64>().prop_filter("NaN breaks PartialEq, not the round trip", |v| !v.is_nan())
    }

    /// A string with no interior NUL — the IDL `string` contract
    /// `astrs-cdr`'s own property suite documents and generates under the
    /// same rule.
    pub fn text() -> impl Strategy<Value = String> {
        proptest::collection::vec(any::<char>().prop_filter("no NUL", |c| *c != '\0'), 0..12)
            .prop_map(|chars| chars.into_iter().collect())
    }

    pub fn parameter_value() -> impl Strategy<Value = ParameterValue> {
        prop_oneof![
            Just(ParameterValue::NotSet),
            any::<bool>().prop_map(ParameterValue::Bool),
            any::<i64>().prop_map(ParameterValue::Integer),
            finite_or_infinite_f64().prop_map(ParameterValue::Double),
            text().prop_map(ParameterValue::String),
            proptest::collection::vec(any::<u8>(), 0..8).prop_map(ParameterValue::ByteArray),
            proptest::collection::vec(any::<bool>(), 0..8).prop_map(ParameterValue::BoolArray),
            proptest::collection::vec(any::<i64>(), 0..8).prop_map(ParameterValue::IntegerArray),
            proptest::collection::vec(finite_or_infinite_f64(), 0..8)
                .prop_map(ParameterValue::DoubleArray),
            proptest::collection::vec(text(), 0..8).prop_map(ParameterValue::StringArray),
        ]
    }
}

proptest! {
    /// `type_code()` always matches the message's own discriminant, whatever
    /// the payload — the field the wire form actually dispatches on.
    #[test]
    fn to_message_always_stamps_the_matching_type_code(value in arb::parameter_value()) {
        prop_assert_eq!(value.to_message().r#type, value.type_code());
    }

    /// The core property: every generated value survives a trip through the
    /// ten-field message form and back, whatever the payload.
    #[test]
    fn every_generated_value_round_trips_through_the_message(value in arb::parameter_value()) {
        let message = value.to_message();
        prop_assert_eq!(ParameterValue::from_message(&message), value);
    }

    /// The same generated value survives CDR too — `to_message`'s output is
    /// exactly what `crate::parameters::service` puts on the wire, so this
    /// is the property that would actually catch a `ParameterValue` a real
    /// `ros2 param set` from a stock ROS 2 peer could send and have this
    /// stack misread.
    #[test]
    fn every_generated_value_round_trips_through_cdr(value in arb::parameter_value()) {
        let message = value.to_message();
        let octets = astrs_cdr::to_vec_ros2(&message).expect("encode");
        let decoded: astrs_ros2::msg::rcl_interfaces::ParameterValue =
            astrs_cdr::from_bytes(&octets).expect("decode");
        prop_assert_eq!(ParameterValue::from_message(&decoded), value);
    }

    /// A value never claims another variant's type code, whatever its own
    /// payload happens to be — `same_type_as` is exactly `type_code`
    /// equality, and this is the property that makes it meaningful.
    #[test]
    fn distinct_variants_never_share_a_type_code(
        left in arb::parameter_value(),
        right in arb::parameter_value(),
    ) {
        let same_variant = std::mem::discriminant(&left) == std::mem::discriminant(&right);
        prop_assert_eq!(left.same_type_as(&right), same_variant);
    }
}
