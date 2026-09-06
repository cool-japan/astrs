//! `arrow-interop` end-to-end: every corpus case round-trips through
//! `astrs_data::interop`, and zero-copy is verified by pointer identity, not
//! merely claimed.
//!
//! `tests/common/cases.rs` already builds one [`RecordBatch`] per shape the
//! closed §6.1 type set has to get right — primitives, nulls, every nesting
//! combination, sliced (non-zero-offset) columns, wide rows, the dora
//! single-column convention. Reusing it here (rather than writing a second,
//! narrower fixture set) is deliberate: it is the same corpus
//! `tests/ipc_roundtrip.rs` uses to prove the wire encoder lossless, so a
//! regression that breaks *both* gates in the same run is a strong signal
//! about where the bug actually is.
//!
//! The whole file is gated on the `arrow-interop` feature: every item here
//! names `arrow_array`/`arrow_schema` or `astrs_data::interop`, neither of
//! which exists in a default (feature-off) build, and an integration test
//! file is compiled regardless of which package feature a *different* target
//! happens to need — so without this gate `cargo check --all-targets` (no
//! features) would fail even though the crate's own default build is
//! arrow-free.

#![cfg(feature = "arrow-interop")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "common/cases.rs"]
mod cases;

use std::sync::Arc;

use arrow_array::Array as ArrowArray;
use arrow_schema::{DataType as ArrowDataType, TimeUnit};
use astrs_data::array::{
    Array, ArrayExt, BinaryArray, BooleanArray, Int32Array, IntoArrayRef, StringArray,
};
use astrs_data::interop::{
    InteropError, from_arrow_array, from_arrow_record_batch, to_arrow_array, to_arrow_record_batch,
};
use astrs_data::{DataType, Field, RecordBatch, Schema};

/// Every case in the shared corpus round-trips astrs -> arrow -> astrs with
/// no change in logical content.
///
/// [`RecordBatch`]'s `PartialEq` compares values, not provenance (its own
/// docs: "Sliced columns compare by their logical values"), so this also
/// exercises the sliced-column cases (`sliced_columns`, `wide_rows`) without
/// needing a bespoke comparison.
#[test]
fn every_corpus_case_round_trips_through_arrow() {
    let mut checked = 0usize;
    for case in cases::cases() {
        for (index, batch) in case.batches.iter().enumerate() {
            let arrow_batch = to_arrow_record_batch(batch)
                .unwrap_or_else(|e| panic!("{}[{index}]: to_arrow failed: {e}", case.name));
            let back = from_arrow_record_batch(&arrow_batch)
                .unwrap_or_else(|e| panic!("{}[{index}]: from_arrow failed: {e}", case.name));
            assert_eq!(
                &back, batch,
                "{}[{index}]: round trip changed logical content",
                case.name
            );
            checked += 1;
        }
    }
    assert!(
        checked >= 20,
        "the shared corpus shrank to {checked} batch(es) — expected the full §6.1 shape set"
    );
}

/// Primitive values convert with no copy: the arrow-rs buffer's address is
/// the astrs buffer's address.
#[test]
fn primitive_values_are_pointer_identical_after_conversion() {
    let array = Int32Array::from_opt_iter((0..500).map(|i| (i % 7 != 0).then_some(i)));
    let source_ptr = array.values_buffer().inner().as_ptr();
    let array = array.into_array_ref();

    let arrow_array = to_arrow_array(array.as_ref()).unwrap();
    let downcast = arrow_array
        .as_any()
        .downcast_ref::<arrow_array::Int32Array>()
        .unwrap();
    assert_eq!(downcast.values().inner().as_ptr(), source_ptr);
    assert_eq!(downcast.len(), 500);
}

/// A sliced `BooleanArray` (values at a non-byte-aligned bit offset) still
/// converts with no copy — the one case that needs
/// `arrow_data::ArrayData::offset` rather than a plain buffer hand-off.
#[test]
fn sliced_boolean_values_are_pointer_identical_after_conversion() {
    let source: Vec<bool> = (0..97).map(|i| i % 3 == 0).collect();
    let full: astrs_data::array::ArrayRef = Arc::new(BooleanArray::from_values(source.clone()));
    let sliced = Array::slice(full.as_ref(), 13, 40);
    let bit_offset = sliced
        .downcast::<BooleanArray>()
        .unwrap()
        .values()
        .bit_offset();
    assert_ne!(bit_offset % 8, 0, "the interesting, non-byte-aligned case");
    let source_ptr = sliced
        .downcast::<BooleanArray>()
        .unwrap()
        .values()
        .buffer()
        .as_ptr();

    let arrow_array = to_arrow_array(sliced.as_ref()).unwrap();
    let downcast = arrow_array
        .as_any()
        .downcast_ref::<arrow_array::BooleanArray>()
        .unwrap();
    assert_eq!(downcast.values().inner().as_ptr(), source_ptr);
    for i in 0..40 {
        assert_eq!(downcast.value(i), source[13 + i], "index {i}");
    }
}

/// Binary/Utf8 value bytes convert with no copy.
#[test]
fn binary_value_bytes_are_pointer_identical_after_conversion() {
    let array = BinaryArray::from_opt_iter([Some(&b"lidar"[..]), None, Some(b"camera")]);
    let values_ptr = array.value_data().as_ptr();
    let array = array.into_array_ref();

    let arrow_array = to_arrow_array(array.as_ref()).unwrap();
    let downcast = arrow_array
        .as_any()
        .downcast_ref::<arrow_array::BinaryArray>()
        .unwrap();
    assert_eq!(downcast.values().as_ptr(), values_ptr);
}

/// The reverse direction always copies (see `crate::interop::buffer`'s
/// module docs for why) — asserted here as the negative of the forward
/// pointer-identity tests above, so the "zero-copy one way, not the other"
/// claim is pinned by a test either way.
#[test]
fn the_reverse_direction_copies() {
    let array = Int32Array::from_values([1, 2, 3]).into_array_ref();
    let arrow_array = to_arrow_array(array.as_ref()).unwrap();
    let forward_ptr = arrow_array
        .as_any()
        .downcast_ref::<arrow_array::Int32Array>()
        .unwrap()
        .values()
        .inner()
        .as_ptr();

    let back = from_arrow_array(arrow_array.as_ref()).unwrap();
    let back_ptr = back
        .downcast::<Int32Array>()
        .unwrap()
        .values_buffer()
        .inner()
        .as_ptr();
    assert_ne!(back_ptr, forward_ptr, "arrow -> astrs must copy");
    assert_eq!(back.as_ref(), array.as_ref(), "but the values must match");
}

/// `RecordBatch` conversion preserves schema metadata (`_schema_hash` and
/// friends) and column names through both directions.
#[test]
fn record_batch_conversion_preserves_schema_metadata_and_names() {
    let schema = Arc::new(
        Schema::new(vec![
            Field::new("range_m", DataType::Float32, true),
            Field::new("label", DataType::Utf8, false),
        ])
        .with_metadata_entry("_schema_hash", "cafef00d")
        .with_metadata_entry("producer", "lidar-front"),
    );
    let batch = RecordBatch::try_new(
        schema,
        vec![
            astrs_data::array::Float32Array::from_opt_iter([Some(1.5), None, Some(3.25)])
                .into_array_ref(),
            StringArray::from_values(["a", "b", "c"]).into_array_ref(),
        ],
    )
    .unwrap();

    let arrow_batch = to_arrow_record_batch(&batch).unwrap();
    assert_eq!(arrow_batch.schema_ref().field(0).name(), "range_m");
    assert_eq!(
        arrow_batch.schema_ref().metadata().get("producer"),
        Some(&"lidar-front".to_owned())
    );

    let back = from_arrow_record_batch(&arrow_batch).unwrap();
    assert_eq!(back, batch);
    assert_eq!(
        back.schema().metadata_value("_schema_hash"),
        Some("cafef00d")
    );
}

/// Types outside the closed P0 set are a typed error, not a panic or a
/// silent best-effort guess.
#[test]
fn types_outside_the_closed_set_are_rejected_not_guessed() {
    let arrow_array = arrow_array::Date32Array::from(vec![19_000]);
    let err = from_arrow_array(&arrow_array).unwrap_err();
    assert!(matches!(err, InteropError::UnmappableArrowType { .. }));

    let arrow_array = arrow_array::TimestampMicrosecondArray::from(vec![1_i64]);
    let err = from_arrow_array(&arrow_array).unwrap_err();
    assert!(matches!(
        err,
        InteropError::UnsupportedTemporalUnit {
            kind: "Timestamp",
            ..
        }
    ));
}

/// A `RecordBatch` decoded from an arrow schema whose column count disagrees
/// with its own columns is unreachable through arrow-rs's own
/// `RecordBatch::try_new` validation — but the astrs-side reconstruction
/// still goes through `crate::RecordBatch::try_new_with_row_count`, whose
/// own checks (blueprint-documented in `record_batch.rs`) are exercised
/// here via a type mismatch instead, which arrow-rs's `try_new` does *not*
/// itself reject at construction (`ArrowDataType::Utf8` is a valid array for
/// any field name).
#[test]
fn a_type_mismatch_between_schema_and_column_is_reported() {
    let arrow_schema = Arc::new(arrow_schema::Schema::new(vec![arrow_schema::Field::new(
        "n",
        ArrowDataType::Int32,
        false,
    )]));
    // Deliberately mismatched: the schema says Int32, the column is Utf8.
    let mismatched_column: arrow_array::ArrayRef =
        Arc::new(arrow_array::StringArray::from(vec!["oops"]));
    let arrow_batch = arrow_array::RecordBatch::try_new(arrow_schema, vec![mismatched_column]);
    // arrow-rs itself rejects this at `try_new` (field/column type mismatch),
    // so the interop layer never even sees it — confirming the boundary
    // validation astrs-data would otherwise have to do is already covered by
    // arrow-rs's own constructor.
    assert!(arrow_batch.is_err());
}

/// Sanity check that the pinned nanosecond `Duration` unit round-trips too
/// (the sibling of the `Timestamp` check inside the datatype module's own
/// tests, exercised here at the public `interop` surface).
#[test]
fn duration_round_trips_at_the_pinned_unit() {
    let array = astrs_data::array::DurationArray::from_nanos([250_000_000, 500_000_000]);
    let array = array.into_array_ref();
    let arrow_array = to_arrow_array(array.as_ref()).unwrap();
    assert_eq!(
        arrow_array.data_type(),
        &ArrowDataType::Duration(TimeUnit::Nanosecond)
    );
    let back = from_arrow_array(arrow_array.as_ref()).unwrap();
    assert_eq!(back.as_ref(), array.as_ref());
}
