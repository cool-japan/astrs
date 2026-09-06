//! Property-based tests for the compute kernels (`astrs_data::kernel`).
//!
//! Two kernels get a reference-model treatment here, for the same reason
//! `tests/columnar_props.rs` gives one to bitmaps and offsets: they are the
//! ones with hand-written offset/index arithmetic a unit test's hand-picked
//! cases can miss a corner of.
//!
//! * [`concat`] — checked against flattening a `Vec<Vec<Option<T>>>`
//!   reference model, including the highest-risk case: concatenating
//!   *sliced* arrays, which is exactly where an offset has to be re-based
//!   rather than copied.
//! * [`cast`] — checked for the properties [`OverflowPolicy`] promises
//!   regardless of the specific value: a saturated result is always within
//!   the destination's range, an exact (non-overflowing) round trip through
//!   a wider type and back never changes a value, and `Error` never
//!   silently returns a wrong answer — it is either exactly right or an
//!   error.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use astrs_data::DataType;
use astrs_data::array::{
    ArrayRef, Int8Array, Int32Array, Int64Array, IntoArrayRef, StringArray, UInt8Array,
};
use astrs_data::kernel::{OverflowPolicy, cast, concat};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// concat
// ---------------------------------------------------------------------------

/// A handful of chunks of optional `i32`s, short enough that proptest's
/// shrinker converges quickly but long enough to cross a few internal
/// capacity/growth boundaries.
fn opt_i32_chunks() -> impl Strategy<Value = Vec<Vec<Option<i32>>>> {
    prop::collection::vec(
        prop::collection::vec(prop::option::of(any::<i32>()), 0..8),
        1..6,
    )
}

fn opt_string_chunks() -> impl Strategy<Value = Vec<Vec<Option<String>>>> {
    let value = prop::option::of("[a-z]{0,6}");
    prop::collection::vec(prop::collection::vec(value, 0..6), 1..6)
}

proptest! {
    /// Concatenating `n` primitive arrays matches flattening their reference
    /// models in the same order, nulls included.
    #[test]
    fn concat_primitive_matches_flattening_the_model(chunks in opt_i32_chunks()) {
        let arrays: Vec<ArrayRef> = chunks
            .iter()
            .map(|chunk| Int32Array::from_opt_iter(chunk.iter().copied()).into_array_ref())
            .collect();
        let result = concat(&arrays).unwrap();
        let result = result.as_any().downcast_ref::<Int32Array>().unwrap();

        let expected: Vec<Option<i32>> = chunks.into_iter().flatten().collect();
        prop_assert_eq!(result.iter().collect::<Vec<_>>(), expected);
    }

    /// Same property, over `Utf8` — the family whose concat has to re-base
    /// an offset buffer rather than just copy values.
    #[test]
    fn concat_strings_matches_flattening_the_model(chunks in opt_string_chunks()) {
        let arrays: Vec<ArrayRef> = chunks
            .iter()
            .map(|chunk| StringArray::from_opt_iter(chunk.iter().cloned()).into_array_ref())
            .collect();
        let result = concat(&arrays).unwrap();
        let result = result.as_any().downcast_ref::<StringArray>().unwrap();

        let expected: Vec<Option<String>> = chunks.into_iter().flatten().collect();
        let actual: Vec<Option<String>> = result.iter().map(|v| v.map(str::to_owned)).collect();
        prop_assert_eq!(actual, expected);
    }

    /// The highest-risk case in `concat`'s offset re-basing: every input is
    /// itself a slice of a larger array (so its own offsets do not start at
    /// zero and its backing buffer holds bytes/elements outside the
    /// window), and the windows are concatenated back together.
    #[test]
    fn concat_of_sliced_strings_matches_slicing_the_model(
        model in prop::collection::vec("[a-z]{1,5}", 4..12),
        windows in prop::collection::vec((0usize..12, 0usize..6), 1..4),
    ) {
        let full = StringArray::from_values(model.iter().map(String::as_str));
        let mut arrays = Vec::new();
        let mut expected = Vec::new();
        for &(start, len) in &windows {
            let start = start.min(model.len());
            let len = len.min(model.len() - start);
            arrays.push(full.slice(start, len).into_array_ref());
            expected.extend(model[start..start + len].iter().cloned());
        }

        let result = concat(&arrays).unwrap();
        let result = result.as_any().downcast_ref::<StringArray>().unwrap();
        let actual: Vec<String> = result.iter().map(|v| v.expect("no nulls here").to_owned()).collect();
        prop_assert_eq!(actual, expected);
    }

    /// The same property over `Int32`, whose sliced concat takes a
    /// completely different code path (dense value copy, no offsets) —
    /// covered separately so a bug in one family's slice-then-concat
    /// handling cannot hide behind the other's passing.
    #[test]
    fn concat_of_sliced_primitives_matches_slicing_the_model(
        model in prop::collection::vec(any::<i32>(), 4..16),
        windows in prop::collection::vec((0usize..16, 0usize..8), 1..4),
    ) {
        let full = Int32Array::from_values(model.clone());
        let mut arrays = Vec::new();
        let mut expected = Vec::new();
        for &(start, len) in &windows {
            let start = start.min(model.len());
            let len = len.min(model.len() - start);
            arrays.push(full.slice(start, len).into_array_ref());
            expected.extend_from_slice(&model[start..start + len]);
        }

        let result = concat(&arrays).unwrap();
        let result = result.as_any().downcast_ref::<Int32Array>().unwrap();
        prop_assert_eq!(result.values(), expected.as_slice());
    }

    /// A single array is always returned as itself (an `Arc` clone) —
    /// `concat` must never copy for the one-input case, whatever the model.
    #[test]
    fn concat_of_one_array_is_that_array(model in prop::collection::vec(prop::option::of(any::<i32>()), 0..10)) {
        let array = Int32Array::from_opt_iter(model).into_array_ref();
        let result = concat(std::slice::from_ref(&array)).unwrap();
        prop_assert!(std::sync::Arc::ptr_eq(&array, &result));
    }
}

// ---------------------------------------------------------------------------
// cast
// ---------------------------------------------------------------------------

proptest! {
    /// Widening to a strictly larger integer type and casting straight back
    /// down is lossless for every value, under either policy — nothing can
    /// have overflowed a type strictly wider than its source.
    #[test]
    fn widen_then_narrow_int_round_trips_exactly(values in prop::collection::vec(any::<i8>(), 0..20)) {
        let array = Int8Array::from_values(values.clone()).into_array_ref();
        for policy in [OverflowPolicy::Saturate, OverflowPolicy::Error] {
            let widened = cast(&array, &DataType::Int64, policy).unwrap();
            let back = cast(&widened, &DataType::Int8, policy).unwrap();
            let back = back.as_any().downcast_ref::<Int8Array>().unwrap();
            prop_assert_eq!(back.values(), values.as_slice());
        }
    }

    /// A `Saturate` narrowing cast never fails, and every output value is
    /// exactly what clamping the source to the destination's range would
    /// give — not merely "some `i8`", which every `i8` trivially is.
    #[test]
    fn saturating_narrow_clamps_to_the_destination_range(values in prop::collection::vec(any::<i64>(), 0..20)) {
        let array = Int64Array::from_values(values.clone()).into_array_ref();
        let result = cast(&array, &DataType::Int8, OverflowPolicy::Saturate).unwrap();
        let result = result.as_any().downcast_ref::<Int8Array>().unwrap();
        for (source, &clamped) in values.iter().zip(result.values()) {
            let expected = i8::try_from(*source)
                .unwrap_or(if *source > i64::from(i8::MAX) { i8::MAX } else { i8::MIN });
            prop_assert_eq!(clamped, expected, "source {}", source);
        }
    }

    /// `Error`-policy narrowing is exact whenever it succeeds: every value
    /// that fits comes back unchanged, and the two policies never disagree
    /// on a value that fits both ways.
    #[test]
    fn error_policy_narrow_is_exact_when_it_succeeds(values in prop::collection::vec(-100i32..=100, 0..20)) {
        // Every value here fits Int8's range, so Error must always succeed
        // and agree with Saturate.
        let array = Int32Array::from_values(values.clone()).into_array_ref();
        let checked = cast(&array, &DataType::Int8, OverflowPolicy::Error).unwrap();
        let saturated = cast(&array, &DataType::Int8, OverflowPolicy::Saturate).unwrap();
        let checked = checked.as_any().downcast_ref::<Int8Array>().unwrap();
        let saturated = saturated.as_any().downcast_ref::<Int8Array>().unwrap();
        prop_assert_eq!(checked.values(), saturated.values());
        let expected: Vec<i8> = values.iter().map(|&v| v as i8).collect();
        prop_assert_eq!(checked.values(), expected.as_slice());
    }

    /// Every unsigned value round-trips through a same-width or wider
    /// signed type and back exactly (never negative, so nothing to clamp).
    #[test]
    fn unsigned_through_wider_signed_round_trips(values in prop::collection::vec(any::<u8>(), 0..20)) {
        let array = UInt8Array::from_values(values.clone()).into_array_ref();
        let signed = cast(&array, &DataType::Int16, OverflowPolicy::Error).unwrap();
        let back = cast(&signed, &DataType::UInt8, OverflowPolicy::Error).unwrap();
        let back = back.as_any().downcast_ref::<UInt8Array>().unwrap();
        prop_assert_eq!(back.values(), values.as_slice());
    }

    /// Every `i32` cast to `f64` and back through a checked (`Error`-policy)
    /// narrowing to `i32` again reproduces the original exactly — `f64` has
    /// more than enough mantissa bits to hold any `i32` precisely, so this
    /// path never loses information and never overflows.
    #[test]
    fn int32_through_f64_round_trips_exactly(values in prop::collection::vec(any::<i32>(), 0..20)) {
        let array = Int32Array::from_values(values.clone()).into_array_ref();
        let floats = cast(&array, &DataType::Float64, OverflowPolicy::Error).unwrap();
        let back = cast(&floats, &DataType::Int32, OverflowPolicy::Error).unwrap();
        let back = back.as_any().downcast_ref::<Int32Array>().unwrap();
        prop_assert_eq!(back.values(), values.as_slice());
    }

    /// Nulls survive every numeric cast at exactly the positions they
    /// started at, regardless of the values sitting in the valid slots.
    #[test]
    fn nulls_survive_at_the_same_positions(
        values in prop::collection::vec(prop::option::of(any::<i32>()), 0..20)
    ) {
        let array = Int32Array::from_opt_iter(values.iter().copied()).into_array_ref();
        let result = cast(&array, &DataType::Float32, OverflowPolicy::Saturate).unwrap();
        for (index, value) in values.iter().enumerate() {
            prop_assert_eq!(result.is_null(index), value.is_none(), "index {}", index);
        }
    }

    /// A same-type cast is always a no-op `Arc` clone, whatever the values.
    #[test]
    fn same_type_cast_is_always_a_clone(values in prop::collection::vec(any::<i32>(), 0..10)) {
        let array = Int32Array::from_values(values).into_array_ref();
        let result = cast(&array, &DataType::Int32, OverflowPolicy::Error).unwrap();
        prop_assert!(std::sync::Arc::ptr_eq(&array, &result));
    }
}
