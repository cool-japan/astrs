//! Cross-module integration tests for the columnar core.
//!
//! Everything here goes through the *public* API only, which is the point:
//! the unit tests inside each module can reach private helpers, so a test that
//! only passes there proves nothing about what a node author can actually
//! write.
//!
//! Four themes:
//!
//! 1. **Alignment.** Every owned allocation starts on a 64-byte boundary, in
//!    every construction path, at every size, after every growth step.
//! 2. **The state matrix.** Each array family is exercised empty, all-null,
//!    no-null and sliced — the four shapes that break decoders.
//! 3. **Builder round-trips.** What goes in comes out, for every builder.
//! 4. **Assembly.** Batches, schemas and URNs, used together.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use astrs_data::array::{
    Array, ArrayExt, ArrayRef, BinaryArray, BooleanArray, DurationArray, FixedSizeBinaryArray,
    FixedSizeListArray, Float32Array, Float64Array, Int8Array, Int16Array, Int32Array, Int64Array,
    IntoArrayRef, LargeBinaryArray, LargeStringArray, ListArray, NullArray, StringArray,
    StructArray, TimestampArray, UInt8Array, UInt16Array, UInt32Array, UInt64Array,
    new_empty_array, new_null_array,
};
use astrs_data::buffer::{ALIGNMENT, AlignedBuf, Bitmap, BitmapBuilder, Buffer, ScalarBuffer};
use astrs_data::builder::{
    ArrayBuilder, BinaryBuilder, BooleanBuilder, BuilderExt, DurationBuilder,
    FixedSizeBinaryBuilder, FixedSizeListBuilder, Float32Builder, Float64Builder, Int8Builder,
    Int16Builder, Int32Builder, Int64Builder, LargeBinaryBuilder, LargeStringBuilder, ListBuilder,
    NullBuilder, StringBuilder, StructBuilder, TimestampBuilder, UInt8Builder, UInt16Builder,
    UInt32Builder, UInt64Builder,
};
use astrs_data::urn::{TypeUrn, layout_of, urn_for_data_type};
use astrs_data::{
    DATA_COLUMN, DataError, DataType, F16, Field, RecordBatch, RecordBatchOptions, Schema,
};

// ---------------------------------------------------------------------------
// 1. Alignment
// ---------------------------------------------------------------------------

/// The invariant the whole crate rests on, spelled out once.
fn assert_aligned(address: usize, what: &str) {
    assert_eq!(
        address % ALIGNMENT,
        0,
        "{what}: address {address:#x} is not {ALIGNMENT}-byte aligned"
    );
}

#[test]
fn aligned_buf_is_aligned_at_every_capacity() {
    for capacity in [0usize, 1, 7, 8, 63, 64, 65, 127, 128, 1000, 4096, 65_537] {
        let buf = AlignedBuf::with_capacity(capacity);
        assert_aligned(buf.as_ptr() as usize, "with_capacity");
        assert!(buf.capacity() >= capacity);
        assert!(buf.is_empty());

        let zeroed = AlignedBuf::zeroed(capacity);
        assert_aligned(zeroed.as_ptr() as usize, "zeroed");
        assert_eq!(zeroed.len(), capacity);
        assert!(zeroed.as_slice().iter().all(|&byte| byte == 0));
    }
}

#[test]
fn aligned_buf_stays_aligned_across_growth() {
    let mut buf = AlignedBuf::new();
    assert_aligned(buf.as_ptr() as usize, "empty sentinel");

    for step in 0..2_000u32 {
        buf.push(u8::try_from(step % 251).unwrap_or(0));
        if step % 97 == 0 {
            assert_aligned(buf.as_ptr() as usize, "after push");
        }
    }
    assert_eq!(buf.len(), 2_000);
    assert_aligned(buf.as_ptr() as usize, "after 2000 pushes");

    buf.extend_from_slice(&[7u8; 333]);
    assert_aligned(buf.as_ptr() as usize, "after extend_from_slice");
    buf.extend_zeroed(1_111);
    assert_aligned(buf.as_ptr() as usize, "after extend_zeroed");
    buf.reserve(100_000);
    assert_aligned(buf.as_ptr() as usize, "after reserve");
    buf.shrink_to_fit();
    assert_aligned(buf.as_ptr() as usize, "after shrink_to_fit");
    buf.pad_to_alignment();
    assert_aligned(buf.as_ptr() as usize, "after pad_to_alignment");
    assert_eq!(buf.len() % ALIGNMENT, 0, "padding rounds the length up");
}

#[test]
fn cloning_an_aligned_buf_produces_an_independent_aligned_copy() {
    let mut original = AlignedBuf::from_slice(b"lidar frame");
    let copy = original.clone();

    assert_aligned(original.as_ptr() as usize, "original");
    assert_aligned(copy.as_ptr() as usize, "clone");
    assert_ne!(
        original.as_ptr(),
        copy.as_ptr(),
        "a clone must own its bytes"
    );
    assert_eq!(original.as_slice(), copy.as_slice());

    original.push(b'!');
    assert_eq!(copy.as_slice(), b"lidar frame", "the clone is unaffected");
}

#[test]
fn shared_buffers_keep_the_base_allocation_aligned() {
    let buffer = Buffer::from_slice(&[9u8; 500]);
    assert!(buffer.is_aligned_to(ALIGNMENT));
    assert_aligned(buffer.as_ptr() as usize, "Buffer::from_slice");
    assert_eq!(buffer.offset(), 0);

    // A window is offset by design; the base allocation behind it is not.
    let window = buffer.slice(3, 100);
    assert_eq!(window.len(), 100);
    assert_eq!(window.offset(), 3);
    assert_eq!(window.as_slice(), &[9u8; 100]);
    assert!(!window.is_aligned_to(ALIGNMENT));

    // `realigned` gives back an aligned copy for the SIMD path.
    let fixed = window.realigned();
    assert!(fixed.is_aligned_to(ALIGNMENT));
    assert_eq!(fixed.as_slice(), window.as_slice());

    // Slicing is zero-copy: the share count rises, the address does not move.
    assert_eq!(buffer.as_ptr(), window.as_ptr().wrapping_sub(3));
    assert!(buffer.share_count() >= 2);
}

#[test]
fn every_array_family_allocates_aligned_buffers() {
    let int = Int32Array::from_values(0..100);
    assert!(int.values_buffer().inner().is_aligned_to(ALIGNMENT));

    let float = Float64Array::from_values((0..50).map(f64::from));
    assert!(float.values_buffer().inner().is_aligned_to(ALIGNMENT));

    let boolean = BooleanArray::from_values((0..100).map(|index| index % 3 == 0));
    assert!(boolean.values().buffer().is_aligned_to(ALIGNMENT));

    let binary = BinaryArray::from_values([&b"abc"[..], b"defgh"]);
    assert!(binary.value_data().is_aligned_to(ALIGNMENT));

    let text = StringArray::from_values(["lidar", "camera", "imu"]);
    assert!(text.value_data().is_aligned_to(ALIGNMENT));

    let fixed = FixedSizeBinaryArray::try_from_values(4, [b"abcd", b"efgh"]).unwrap();
    assert!(fixed.value_data().is_aligned_to(ALIGNMENT));

    let stamps = TimestampArray::from_nanos([1, 2, 3]);
    assert!(stamps.values_buffer().inner().is_aligned_to(ALIGNMENT));
}

#[test]
fn scalar_buffers_reject_misaligned_windows() {
    let bytes = Buffer::from_scalars(&[1i32, 2, 3, 4]);
    assert!(bytes.typed::<i32>().is_ok());

    // One byte in is neither element-aligned nor a whole number of elements.
    let skewed = bytes.slice(1, 12);
    assert!(matches!(
        skewed.typed::<i32>(),
        Err(DataError::UnalignedBuffer { .. })
    ));

    let ragged = bytes.slice(0, 10);
    assert!(matches!(
        ragged.typed::<i32>(),
        Err(DataError::BufferLengthNotMultiple { len: 10, width: 4 })
    ));
}

#[test]
fn buffers_can_be_recovered_as_owned_allocations() {
    let owned = AlignedBuf::from_slice(&[1u8, 2, 3]);
    let buffer = Buffer::from_arc(Arc::new(owned));
    let recovered = buffer.into_aligned().expect("sole owner, full window");
    assert_eq!(recovered.as_slice(), &[1, 2, 3]);
    assert_aligned(recovered.as_ptr() as usize, "recovered");

    // A shared buffer cannot be reclaimed.
    let shared = Buffer::from_slice(&[1u8, 2, 3]);
    let _second = shared.clone();
    assert!(shared.into_aligned().is_err());
}

// ---------------------------------------------------------------------------
// 2. The state matrix: empty / all-null / no-null / sliced
// ---------------------------------------------------------------------------

/// Checks the invariants every array must satisfy whatever its family.
fn assert_array_invariants(array: &ArrayRef, expected_len: usize, expected_nulls: usize) {
    assert_eq!(array.len(), expected_len, "{array:?}");
    assert_eq!(array.is_empty(), expected_len == 0);
    assert_eq!(array.null_count(), expected_nulls, "{array:?}");

    for index in 0..expected_len {
        assert_eq!(
            array.is_null(index),
            !array.is_valid(index),
            "slot {index} disagrees with itself"
        );
    }
    // Out-of-range slots read as null, never panic.
    assert!(array.is_null(expected_len));
    assert!(array.is_null(usize::MAX));

    // Slicing clamps in every direction.
    assert_eq!(array.slice(0, expected_len).len(), expected_len);
    assert_eq!(array.slice(expected_len, 1).len(), 0);
    assert_eq!(array.slice(usize::MAX, 1).len(), 0);
    assert_eq!(array.slice(0, usize::MAX).len(), expected_len);

    // A full-width slice equals the original.
    assert!(array.slice(0, expected_len).as_ref() == array.as_ref());

    // Equality is reflexive even through a clone.
    let cloned = Arc::clone(array);
    assert!(cloned.as_ref() == array.as_ref());
}

/// One family's four shapes, built from the caller's constructors.
fn assert_state_matrix(
    label: &str,
    empty: ArrayRef,
    all_null: ArrayRef,
    no_null: ArrayRef,
    expected_len: usize,
) {
    assert_array_invariants(&empty, 0, 0);
    assert_array_invariants(&all_null, expected_len, expected_len);
    assert_array_invariants(&no_null, expected_len, 0);

    assert_eq!(empty.data_type(), all_null.data_type(), "{label}");
    assert_eq!(empty.data_type(), no_null.data_type(), "{label}");

    // A slice out of the middle keeps the null pattern of that window.
    if expected_len >= 3 {
        let window = no_null.slice(1, 2);
        assert_eq!(window.len(), 2, "{label}");
        assert_eq!(window.null_count(), 0, "{label}");

        let null_window = all_null.slice(1, 2);
        assert_eq!(null_window.null_count(), 2, "{label}");

        // Slicing a slice composes.
        assert_eq!(window.slice(1, 5).len(), 1, "{label}");
    }

    // An all-null array never equals a no-null one of the same length.
    assert!(all_null.as_ref() != no_null.as_ref(), "{label}");
}

#[test]
fn primitive_arrays_cover_the_matrix() {
    assert_state_matrix(
        "Int32",
        Int32Array::from_values([]).into_array_ref(),
        Int32Array::new_null(4).into_array_ref(),
        Int32Array::from_values([1, 2, 3, 4]).into_array_ref(),
        4,
    );
    assert_state_matrix(
        "UInt64",
        UInt64Array::from_values([]).into_array_ref(),
        UInt64Array::new_null(4).into_array_ref(),
        UInt64Array::from_values([1u64, 2, 3, 4]).into_array_ref(),
        4,
    );
    assert_state_matrix(
        "Float32",
        Float32Array::from_values([]).into_array_ref(),
        Float32Array::new_null(4).into_array_ref(),
        Float32Array::from_values([1.0f32, 2.0, 3.0, 4.0]).into_array_ref(),
        4,
    );
}

#[test]
fn all_primitive_widths_round_trip_through_their_arrays() {
    assert_eq!(Int8Array::from_values([-1i8, 2]).values(), &[-1, 2]);
    assert_eq!(Int16Array::from_values([-1i16, 2]).values(), &[-1, 2]);
    assert_eq!(Int32Array::from_values([-1i32, 2]).values(), &[-1, 2]);
    assert_eq!(Int64Array::from_values([-1i64, 2]).values(), &[-1, 2]);
    assert_eq!(UInt8Array::from_values([1u8, 2]).values(), &[1, 2]);
    assert_eq!(UInt16Array::from_values([1u16, 2]).values(), &[1, 2]);
    assert_eq!(UInt32Array::from_values([1u32, 2]).values(), &[1, 2]);
    assert_eq!(UInt64Array::from_values([1u64, 2]).values(), &[1, 2]);

    let halves = astrs_data::array::Float16Array::from_values([F16::ONE, F16::ZERO]);
    assert_eq!(halves.value(0).map(F16::to_f32), Some(1.0));
    assert_eq!(halves.value(1).map(F16::to_f32), Some(0.0));
}

#[test]
fn boolean_arrays_cover_the_matrix() {
    assert_state_matrix(
        "Bool",
        BooleanArray::from_values([]).into_array_ref(),
        BooleanArray::new_null(4).into_array_ref(),
        BooleanArray::from_values([true, false, true, true]).into_array_ref(),
        4,
    );

    let mixed = BooleanArray::from_opt_iter([Some(true), None, Some(false), Some(true)]);
    assert_eq!(mixed.true_count(), 2);
    assert_eq!(mixed.false_count(), 1);
    assert_eq!(mixed.null_count(), 1);

    let window = mixed.slice(1, 2);
    assert_eq!(window.len(), 2);
    assert_eq!(window.true_count(), 0);
    assert_eq!(window.false_count(), 1);
    assert_eq!(window.null_count(), 1);
}

#[test]
fn binary_arrays_cover_the_matrix() {
    assert_state_matrix(
        "Binary",
        BinaryArray::from_values(Vec::<&[u8]>::new()).into_array_ref(),
        BinaryArray::new_null(3).into_array_ref(),
        BinaryArray::from_values([&b"a"[..], b"bb", b"ccc"]).into_array_ref(),
        3,
    );
    assert_state_matrix(
        "LargeBinary",
        LargeBinaryArray::from_values(Vec::<&[u8]>::new()).into_array_ref(),
        LargeBinaryArray::new_null(3).into_array_ref(),
        LargeBinaryArray::from_values([&b"a"[..], b"bb", b"ccc"]).into_array_ref(),
        3,
    );

    let values = BinaryArray::from_opt_iter([Some(&b"one"[..]), None, Some(b"three")]);
    assert_eq!(values.get(0), Some(&b"one"[..]));
    assert_eq!(values.get(1), None, "get respects validity");
    assert_eq!(
        values.value(1),
        Some(&b""[..]),
        "value returns the raw slot, which a null leaves empty"
    );
    assert_eq!(values.value_length(2), Some(5));
    assert_eq!(values.total_value_bytes(), 8);

    let window = values.slice(1, 2);
    assert_eq!(window.get(0), None);
    assert_eq!(window.get(1), Some(&b"three"[..]));
}

#[test]
fn string_arrays_cover_the_matrix() {
    assert_state_matrix(
        "Utf8",
        StringArray::from_values(Vec::<&str>::new()).into_array_ref(),
        StringArray::new_null(3).into_array_ref(),
        StringArray::from_values(["a", "bb", "ccc"]).into_array_ref(),
        3,
    );
    assert_state_matrix(
        "LargeUtf8",
        LargeStringArray::from_values(Vec::<&str>::new()).into_array_ref(),
        LargeStringArray::new_null(3).into_array_ref(),
        LargeStringArray::from_values(["a", "bb", "ccc"]).into_array_ref(),
        3,
    );

    // Multi-byte code points survive slicing at the value level.
    let text = StringArray::from_values(["日本語", "ασδφ", "🤖"]);
    assert_eq!(text.value(0), Some("日本語"));
    assert_eq!(text.value_length(0), Some(9));
    assert_eq!(text.slice(2, 1).value(0), Some("🤖"));
    assert_eq!(text.total_value_bytes(), 9 + 8 + 4);
}

#[test]
fn utf8_validation_rejects_bad_bytes_and_bad_boundaries() {
    // A lone continuation byte is never valid UTF-8.
    let offsets = ScalarBuffer::<i32>::from_slice(&[0, 1]);
    let values = Buffer::from_slice(&[0x80]);
    assert!(matches!(
        StringArray::try_new(offsets, values, None),
        Err(DataError::InvalidUtf8 { .. })
    ));

    // Valid UTF-8 overall, but an offset splits a code point.
    let text = "é".as_bytes(); // two bytes
    let offsets = ScalarBuffer::<i32>::from_slice(&[0, 1, 2]);
    let values = Buffer::from_slice(text);
    assert!(matches!(
        StringArray::try_new(offsets, values, None),
        Err(DataError::OffsetNotCharBoundary { .. })
    ));

    // The same bytes are accepted when the boundary is right.
    let offsets = ScalarBuffer::<i32>::from_slice(&[0, 2]);
    let values = Buffer::from_slice(text);
    let array = StringArray::try_new(offsets, values, None).unwrap();
    assert_eq!(array.value(0), Some("é"));
}

#[test]
fn value_regions_may_be_larger_than_the_offsets_cover() {
    // An Arrow IPC body is padded to a 64-byte boundary, so a decoded values
    // buffer is routinely longer than the last offset. That slack must be
    // ignored, not rejected — stage 2 depends on it for every golden vector.
    let mut values = b"onetwo".to_vec();
    values.extend_from_slice(&[0u8; 58]); // padding, and not valid text either
    assert_eq!(values.len() % ALIGNMENT, 0);

    let offsets = ScalarBuffer::<i32>::from_slice(&[0, 3, 6]);
    let array = StringArray::try_new(offsets, Buffer::from_slice(&values), None)
        .expect("trailing slack is allowed");
    assert_eq!(array.len(), 2);
    assert_eq!(array.get(0), Some("one"));
    assert_eq!(array.get(1), Some("two"));
    assert_eq!(array.total_value_bytes(), 6);

    // Non-UTF-8 slack is fine too: only the covered region is validated.
    let mut ragged = b"ok".to_vec();
    ragged.extend_from_slice(&[0xff, 0xfe]);
    let array = StringArray::try_new(
        ScalarBuffer::<i32>::from_slice(&[0, 2]),
        Buffer::from_slice(&ragged),
        None,
    )
    .expect("slack is not validated");
    assert_eq!(array.get(0), Some("ok"));

    // A leading gap — what a sliced producer emits — is equally fine.
    let array = BinaryArray::try_new(
        ScalarBuffer::<i32>::from_slice(&[2, 4]),
        Buffer::from_slice(b"skipme"),
        None,
    )
    .expect("a non-zero first offset is allowed");
    assert_eq!(array.len(), 1);
    assert_eq!(array.get(0), Some(&b"ip"[..]));

    // Running off the end is still rejected.
    assert!(matches!(
        BinaryArray::try_new(
            ScalarBuffer::<i32>::from_slice(&[0, 9]),
            Buffer::from_slice(b"short"),
            None,
        ),
        Err(DataError::OffsetOutOfBounds { .. })
    ));
}

#[test]
fn fixed_size_binary_arrays_cover_the_matrix() {
    assert_state_matrix(
        "FixedSizeBinary(2)",
        FixedSizeBinaryArray::try_from_values(2, Vec::<&[u8]>::new())
            .unwrap()
            .into_array_ref(),
        FixedSizeBinaryArray::new_null(2, 3)
            .unwrap()
            .into_array_ref(),
        FixedSizeBinaryArray::try_from_values(2, [b"ab", b"cd", b"ef"])
            .unwrap()
            .into_array_ref(),
        3,
    );

    assert!(matches!(
        FixedSizeBinaryArray::try_from_values(0, [b""]),
        Err(DataError::InvalidFixedSize { size: 0 })
    ));
    assert!(FixedSizeBinaryArray::try_from_values(2, [&b"abc"[..]]).is_err());
}

#[test]
fn nested_arrays_cover_the_matrix() {
    let item = Field::nullable("item", DataType::Int32);

    let empty = ListArray::try_from_lengths(
        item.clone(),
        [],
        Int32Array::from_values([]).into_array_ref(),
    )
    .unwrap()
    .into_array_ref();
    let all_null = ListArray::new_null(item.clone(), 3).into_array_ref();
    let no_null = ListArray::try_from_lengths(
        item.clone(),
        [1usize, 2, 0],
        Int32Array::from_values([1, 2, 3]).into_array_ref(),
    )
    .unwrap()
    .into_array_ref();
    assert_state_matrix("List", empty, all_null, no_null, 3);

    let child = Int32Array::from_values([1, 2, 3, 4, 5, 6]).into_array_ref();
    let fixed = FixedSizeListArray::try_new(item.clone(), 2, child, None).unwrap();
    assert_eq!(fixed.len(), 3);
    assert_eq!(fixed.value_size(), 2);
    assert_eq!(fixed.value(1).map(|v| v.len()), Some(2));
    assert_state_matrix(
        "FixedSizeList",
        FixedSizeListArray::try_new(
            item.clone(),
            2,
            Int32Array::from_values([]).into_array_ref(),
            None,
        )
        .unwrap()
        .into_array_ref(),
        FixedSizeListArray::new_null(item.clone(), 2, 3)
            .unwrap()
            .into_array_ref(),
        fixed.into_array_ref(),
        3,
    );

    let fields = vec![
        Field::required("a", DataType::Int32),
        Field::nullable("b", DataType::Utf8),
    ];
    let structs = StructArray::try_new(
        fields.clone(),
        vec![
            Int32Array::from_values([1, 2, 3]).into_array_ref(),
            StringArray::from_opt_iter([Some("x"), None, Some("z")]).into_array_ref(),
        ],
        None,
    )
    .unwrap();
    assert_eq!(structs.num_columns(), 2);
    assert!(structs.column_by_name("b").is_some());
    assert_state_matrix(
        "Struct",
        StructArray::try_new(
            fields.clone(),
            vec![
                Int32Array::from_values([]).into_array_ref(),
                StringArray::from_values(Vec::<&str>::new()).into_array_ref(),
            ],
            None,
        )
        .unwrap()
        .into_array_ref(),
        StructArray::new_null(fields, 3).unwrap().into_array_ref(),
        structs.into_array_ref(),
        3,
    );
}

#[test]
fn temporal_arrays_cover_the_matrix() {
    assert_state_matrix(
        "Timestamp",
        TimestampArray::from_nanos([]).into_array_ref(),
        TimestampArray::new_null(3).into_array_ref(),
        TimestampArray::from_nanos([1, 2, 3]).into_array_ref(),
        3,
    );
    assert_state_matrix(
        "Duration",
        DurationArray::from_nanos([]).into_array_ref(),
        DurationArray::new_null(3).into_array_ref(),
        DurationArray::from_nanos([1, 2, 3]).into_array_ref(),
        3,
    );

    let stamps = TimestampArray::from_nanos([1_500_000_000, 2_000_000_000]);
    assert_eq!(stamps.value_as_secs_f64(0), Some(1.5));
    assert_eq!(
        stamps.value_as_duration(1),
        Some(std::time::Duration::from_secs(2))
    );

    // Two i64 columns with the same bytes are still different types.
    let stamp: ArrayRef = TimestampArray::from_nanos([1]).into_array_ref();
    let span: ArrayRef = DurationArray::from_nanos([1]).into_array_ref();
    assert_ne!(stamp.data_type(), span.data_type());
    assert!(stamp.as_ref() != span.as_ref());
}

#[test]
fn null_arrays_are_all_null_at_every_length() {
    for len in [0usize, 1, 7, 64, 1000] {
        let array = NullArray::new(len).into_array_ref();
        assert_array_invariants(&array, len, len);
        assert_eq!(array.data_type(), &DataType::Null);
        assert!(array.validity().is_none(), "Null carries no bitmap");
    }
}

#[test]
fn dynamic_construction_covers_every_type() {
    let types = [
        DataType::Null,
        DataType::Bool,
        DataType::Int8,
        DataType::Int16,
        DataType::Int32,
        DataType::Int64,
        DataType::UInt8,
        DataType::UInt16,
        DataType::UInt32,
        DataType::UInt64,
        DataType::Float16,
        DataType::Float32,
        DataType::Float64,
        DataType::Binary,
        DataType::LargeBinary,
        DataType::Utf8,
        DataType::LargeUtf8,
        DataType::FixedSizeBinary(3),
        DataType::fixed_size_list(Field::nullable("item", DataType::Int32), 2),
        DataType::list(Field::nullable("item", DataType::Utf8)),
        DataType::strukt([Field::nullable("a", DataType::Int8)]),
        DataType::Timestamp,
        DataType::Duration,
    ];

    for data_type in &types {
        let empty = new_empty_array(data_type).unwrap();
        assert_eq!(empty.data_type(), data_type, "{data_type}");
        assert_array_invariants(&empty, 0, 0);

        let nulls = new_null_array(data_type, 5).unwrap();
        assert_eq!(nulls.data_type(), data_type, "{data_type}");
        assert_array_invariants(&nulls, 5, 5);
    }
}

#[test]
fn downcasting_is_checked() {
    let column: ArrayRef = Int32Array::from_values([1, 2]).into_array_ref();
    assert!(column.downcast::<Int32Array>().is_some());
    assert!(column.downcast::<Int64Array>().is_none());
    assert!(column.try_downcast::<Int32Array>().is_ok());
    assert!(matches!(
        column.try_downcast::<StringArray>(),
        Err(DataError::DowncastFailed { .. })
    ));
}

// ---------------------------------------------------------------------------
// 3. Builder round-trips
// ---------------------------------------------------------------------------

#[test]
fn primitive_builders_round_trip() {
    let mut builder = Int32Builder::with_capacity(4);
    builder.append_value(1);
    builder.append_null();
    builder.append_option(Some(3));
    builder.append_option(None);
    let array = builder.finish();

    assert_eq!(array.len(), 4);
    assert_eq!(array.null_count(), 2);
    assert_eq!(
        array.iter().collect::<Vec<_>>(),
        vec![Some(1), None, Some(3), None]
    );
    assert!(builder.is_empty(), "finish resets");

    // Bulk appends and reuse.
    let mut builder = Int64Builder::new();
    builder.append_slice(&[1, 2, 3]);
    builder.append_n(2, 9);
    let array = builder.finish();
    assert_eq!(array.values(), &[1, 2, 3, 9, 9]);
    assert_eq!(array.null_count(), 0);
}

#[test]
fn every_primitive_builder_is_wired_to_its_type() {
    let mut int8 = Int8Builder::new();
    int8.append_value(-1);
    assert_eq!(int8.finish().data_type(), &DataType::Int8);

    let mut int16 = Int16Builder::new();
    int16.append_value(-1);
    assert_eq!(int16.finish().data_type(), &DataType::Int16);

    let mut uint8 = UInt8Builder::new();
    uint8.append_value(1);
    assert_eq!(uint8.finish().data_type(), &DataType::UInt8);

    let mut uint16 = UInt16Builder::new();
    uint16.append_value(1);
    assert_eq!(uint16.finish().data_type(), &DataType::UInt16);

    let mut uint32 = UInt32Builder::new();
    uint32.append_value(1);
    assert_eq!(uint32.finish().data_type(), &DataType::UInt32);

    let mut uint64 = UInt64Builder::new();
    uint64.append_value(1);
    assert_eq!(uint64.finish().data_type(), &DataType::UInt64);

    let mut float32 = Float32Builder::new();
    float32.append_value(0.5);
    assert_eq!(float32.finish().data_type(), &DataType::Float32);

    let mut float64 = Float64Builder::new();
    float64.append_value(0.5);
    assert_eq!(float64.finish().data_type(), &DataType::Float64);
}

#[test]
fn byte_builders_round_trip() {
    let mut binary = BinaryBuilder::with_capacity(3, 16);
    binary.append_value(b"one");
    binary.append_null();
    binary.append_option(Some(b"three"));
    let array = binary.finish();
    assert_eq!(
        array.iter().collect::<Vec<_>>(),
        vec![Some(&b"one"[..]), None, Some(&b"three"[..])]
    );

    let mut large = LargeBinaryBuilder::new();
    large.append_value(b"x");
    assert_eq!(large.finish().data_type(), &DataType::LargeBinary);

    let mut text = StringBuilder::with_capacity(3, 16);
    text.append_value("lidar");
    text.append_option(None::<&str>);
    text.append_value("日本語");
    let array = text.finish();
    assert_eq!(
        array.iter().collect::<Vec<_>>(),
        vec![Some("lidar"), None, Some("日本語")]
    );

    let mut large_text = LargeStringBuilder::new();
    large_text.append_value("x");
    assert_eq!(large_text.finish().data_type(), &DataType::LargeUtf8);
}

#[test]
fn boolean_and_null_builders_round_trip() {
    let mut boolean = BooleanBuilder::with_capacity(4);
    boolean.append_value(true);
    boolean.append_null();
    boolean.append_option(Some(false));
    boolean.append_option(None);
    let array = boolean.finish();
    assert_eq!(
        array.iter().collect::<Vec<_>>(),
        vec![Some(true), None, Some(false), None]
    );

    let mut nulls = NullBuilder::with_capacity(3);
    nulls.append_nulls(3);
    let array = nulls.finish();
    assert_eq!(array.len(), 3);
    assert_eq!(array.null_count(), 3);
}

#[test]
fn fixed_size_binary_builder_round_trips_and_rejects_bad_widths() {
    let mut builder = FixedSizeBinaryBuilder::with_capacity(2, 3).unwrap();
    assert_eq!(builder.value_size(), 2);
    builder.append_value(b"ab").unwrap();
    builder.append_null();
    builder.append_option(Some(b"cd")).unwrap();
    assert!(builder.append_value(b"toolong").is_err());
    assert!(builder.append_value(b"x").is_err(), "short values too");
    let array = builder.finish();

    assert_eq!(array.len(), 3);
    assert_eq!(array.get(0), Some(&b"ab"[..]));
    assert_eq!(array.get(1), None);
    assert_eq!(array.get(2), Some(&b"cd"[..]));

    assert!(FixedSizeBinaryBuilder::new(0).is_err());
    assert!(FixedSizeBinaryBuilder::new(-1).is_err());
}

#[test]
fn temporal_builders_round_trip() {
    let mut stamps = TimestampBuilder::with_capacity(3);
    stamps.append_value(1);
    stamps.append_null();
    stamps.append_option(Some(3));
    let array = stamps.finish();
    assert_eq!(array.data_type(), &DataType::Timestamp);
    assert_eq!(
        array.iter().collect::<Vec<_>>(),
        vec![Some(1), None, Some(3)]
    );

    let mut spans = DurationBuilder::new();
    spans.append_slice(&[10, 20]);
    let array = spans.finish();
    assert_eq!(array.data_type(), &DataType::Duration);
    assert_eq!(array.values(), &[10, 20]);
}

#[test]
fn nested_builders_round_trip() {
    let mut lists = ListBuilder::new(
        Field::nullable("item", DataType::Int32),
        Int32Builder::new(),
    );
    lists.append_values(|values| {
        values.append_value(1);
        values.append_value(2);
    });
    lists.append_list_null();
    lists.append_values(|values| values.append_value(3));
    lists.append_values(|_| {});
    let array = lists.finish();

    assert_eq!(array.len(), 4);
    assert_eq!(array.null_count(), 1);
    assert_eq!(array.value_length(0), Some(2));
    assert_eq!(array.value_length(1), Some(0));
    assert_eq!(array.value_length(3), Some(0));

    let mut runs = FixedSizeListBuilder::try_new(
        Field::nullable("item", DataType::Float32),
        2,
        Float32Builder::new(),
    )
    .unwrap();
    {
        let values = runs.values();
        values.append_value(1.0);
        values.append_value(2.0);
    }
    runs.append(true).unwrap();
    runs.append_run_null();
    let array = runs.finish();
    assert_eq!(array.len(), 2);
    assert_eq!(array.null_count(), 1);
    assert_eq!(array.value_size(), 2);
}

#[test]
fn struct_builder_round_trips_through_dynamic_children() {
    let fields = vec![
        Field::nullable("id", DataType::Int32),
        Field::nullable("name", DataType::Utf8),
    ];
    let builders: Vec<Box<dyn ArrayBuilder>> = vec![
        Box::new(Int32Builder::new()),
        Box::new(StringBuilder::new()),
    ];
    let mut builder = StructBuilder::new(fields, builders).unwrap();

    for (id, name) in [(1, "lidar"), (2, "camera")] {
        builder
            .field_builder(0)
            .and_then(|b| b.downcast_mut::<Int32Builder>())
            .map(|b| b.append_value(id))
            .expect("child 0 is an Int32Builder");
        builder
            .field_builder_by_name("name")
            .and_then(|b| b.downcast_mut::<StringBuilder>())
            .map(|b| b.append_value(name))
            .expect("child 1 is a StringBuilder");
        builder.append(true);
    }
    builder.append_row_null();

    let array = builder.finish();
    assert_eq!(array.len(), 3);
    assert_eq!(array.null_count(), 1);
    assert_eq!(array.num_columns(), 2);
    let names = array
        .column_by_name("name")
        .and_then(|c| c.downcast::<StringArray>())
        .expect("a string column");
    assert_eq!(names.value(0), Some("lidar"));
}

#[test]
fn builders_can_be_reused_after_finish() {
    let mut builder = Int32Builder::with_capacity(2);
    builder.append_value(1);
    let first = builder.finish();
    assert_eq!(first.len(), 1);

    builder.append_value(2);
    builder.append_value(3);
    let second = builder.finish();
    assert_eq!(second.values(), &[2, 3]);
    assert_eq!(first.values(), &[1], "the first array is untouched");
}

#[test]
fn finish_cloned_leaves_the_builder_intact() {
    let mut builder = StringBuilder::new();
    builder.append_value("a");
    let snapshot = builder.finish_cloned();
    builder.append_value("b");
    let final_array = builder.finish();

    assert_eq!(snapshot.len(), 1);
    assert_eq!(final_array.len(), 2);
}

#[test]
fn validity_is_lazy_until_the_first_null() {
    let mut builder = Int32Builder::with_capacity(4);
    builder.append_value(1);
    builder.append_value(2);
    let dense = builder.finish();
    assert!(dense.validity().is_none(), "no nulls, no bitmap");

    builder.append_value(1);
    builder.append_null();
    let sparse = builder.finish();
    assert!(sparse.validity().is_some());
    assert_eq!(sparse.null_count(), 1);
    assert!(sparse.is_valid(0));
    assert!(sparse.is_null(1));
}

// ---------------------------------------------------------------------------
// 4. Assembly: bitmaps, batches, schemas and URNs
// ---------------------------------------------------------------------------

#[test]
fn bitmap_builder_and_bitmap_agree() {
    let pattern: Vec<bool> = (0..200).map(|index| index % 7 == 0).collect();

    let mut builder = BitmapBuilder::with_capacity(pattern.len());
    builder.append_slice(&pattern);
    let bitmap = builder.finish();

    assert_eq!(bitmap.len(), pattern.len());
    assert_eq!(bitmap.count_set(), pattern.iter().filter(|b| **b).count());
    assert_eq!(bitmap.count_unset(), pattern.len() - bitmap.count_set());
    for (index, expected) in pattern.iter().enumerate() {
        assert_eq!(bitmap.value(index), *expected, "bit {index}");
        assert_eq!(bitmap.get(index), Some(*expected));
    }
    assert_eq!(bitmap.get(pattern.len()), None);

    let set: Vec<usize> = bitmap.set_indices().collect();
    let expected: Vec<usize> = pattern
        .iter()
        .enumerate()
        .filter_map(|(index, bit)| bit.then_some(index))
        .collect();
    assert_eq!(set, expected);
}

#[test]
fn bitmap_boolean_ops_follow_their_definitions() {
    let left = Bitmap::from_buffer(Buffer::from_slice(&[0b1010_1010]));
    let right = Bitmap::from_buffer(Buffer::from_slice(&[0b1100_1100]));

    let and = left.and(&right).unwrap();
    let or = left.or(&right).unwrap();
    let xor = left.xor(&right).unwrap();
    let not = left.not();

    for index in 0..8 {
        let a = left.value(index);
        let b = right.value(index);
        assert_eq!(and.value(index), a && b, "AND bit {index}");
        assert_eq!(or.value(index), a || b, "OR bit {index}");
        assert_eq!(xor.value(index), a ^ b, "XOR bit {index}");
        assert_eq!(not.value(index), !a, "NOT bit {index}");
    }

    let short = Bitmap::new_set(4);
    assert!(matches!(
        left.and(&short),
        Err(DataError::BitmapLengthMismatch { left: 8, right: 4 })
    ));
}

#[test]
fn bitmap_slices_are_zero_copy_windows() {
    let bits = Bitmap::from_buffer(Buffer::from_slice(&[0b1111_0000, 0b0000_1111]));
    let window = bits.slice(4, 8);

    assert_eq!(window.len(), 8);
    assert_eq!(window.bit_offset(), 4);
    for index in 0..8 {
        assert_eq!(window.value(index), bits.value(index + 4), "bit {index}");
    }

    // Canonicalising re-bases the window without changing what it says.
    let canonical = window.to_canonical();
    assert_eq!(canonical.bit_offset(), 0);
    assert_eq!(canonical.len(), window.len());
    for index in 0..8 {
        assert_eq!(canonical.value(index), window.value(index));
    }

    assert!(matches!(
        bits.try_slice(12, 8),
        Err(DataError::SliceOutOfBounds { .. })
    ));
    assert_eq!(bits.slice(12, 8).len(), 4, "the infallible form clamps");
}

#[test]
fn record_batches_assemble_from_builders() {
    let mut stamps = TimestampBuilder::with_capacity(3);
    let mut ranges = Float32Builder::with_capacity(3);
    let mut labels = StringBuilder::with_capacity(3, 32);

    for (index, label) in ["near", "mid", "far"].iter().enumerate() {
        stamps.append_value(i64::try_from(index).unwrap_or(0) * 1_000_000);
        ranges.append_value(index as f32 + 0.5);
        labels.append_value(label);
    }

    let schema = Arc::new(Schema::new(vec![
        Field::required("stamp", DataType::Timestamp),
        Field::required("range", DataType::Float32),
        Field::required("label", DataType::Utf8),
    ]));
    let batch = RecordBatch::try_new(
        schema,
        vec![
            stamps.finish_array(),
            ranges.finish_array(),
            labels.finish_array(),
        ],
    )
    .unwrap();

    assert_eq!(batch.num_rows(), 3);
    assert_eq!(batch.num_columns(), 3);
    assert!(batch.buffer_memory_size() > 0);

    let window = batch.slice(1, 2);
    assert_eq!(window.num_rows(), 2);
    let labels = window
        .column_by_name("label")
        .and_then(|c| c.downcast::<StringArray>())
        .expect("a string column");
    assert_eq!(labels.value(0), Some("mid"));
    assert_eq!(labels.value(1), Some("far"));

    let projected = batch.project_by_name(&["label", "stamp"]).unwrap();
    assert_eq!(projected.num_columns(), 2);
    assert_eq!(projected.schema().field(0).map(Field::name), Some("label"));
}

#[test]
fn the_payload_convention_holds_end_to_end() {
    let mut builder = Float32Builder::with_capacity(4);
    builder.append_slice(&[1.0, 2.0, 3.0, 4.0]);
    let batch = RecordBatch::from_payload(builder.finish_array());

    assert_eq!(batch.num_columns(), 1);
    assert_eq!(batch.schema().field(0).map(Field::name), Some(DATA_COLUMN));
    let column = batch.payload_column().expect("the data column");
    assert_eq!(column.data_type(), &DataType::Float32);

    // The schema the URN registry would produce for that port matches.
    let urn = TypeUrn::parse("std/core/v1/Float32").unwrap();
    assert_eq!(layout_of(&urn), Ok(DataType::Float32));
    assert_eq!(
        urn_for_data_type(column.data_type()).map(|u| u.as_str().to_owned()),
        Some("std/core/v1/Float32".to_owned())
    );
}

#[test]
fn schemas_and_batches_agree_about_layout_compatibility() {
    let declared = Arc::new(Schema::new(vec![Field::nullable(
        "l",
        DataType::list(Field::new("item", DataType::Int32, true)),
    )]));

    // A producer that names the child field differently.
    let column = ListArray::try_from_lengths(
        Field::new("element", DataType::Int32, true),
        [1usize, 1],
        Int32Array::from_values([7, 8]).into_array_ref(),
    )
    .unwrap()
    .into_array_ref();

    assert!(RecordBatch::try_new(Arc::clone(&declared), vec![Arc::clone(&column)]).is_err());

    let relaxed =
        RecordBatchOptions::default().with_column_types(astrs_data::ColumnTypeCheck::Layout);
    let batch = RecordBatch::try_new_with_options(declared, vec![column], relaxed).unwrap();
    assert_eq!(batch.num_rows(), 2);
}

#[test]
fn schema_metadata_survives_a_round_trip_through_serde() {
    let schema = Schema::new(vec![
        Field::required("a", DataType::Int32),
        Field::nullable("b", DataType::list(Field::nullable("item", DataType::Utf8))),
    ])
    .with_metadata_entry("_schema_hash", "0xdeadbeef")
    .with_metadata_entry("producer", "lidar");

    let json = serde_json::to_string(&schema).expect("a schema serialises");
    let back: Schema = serde_json::from_str(&json).expect("and deserialises");
    assert_eq!(back, schema);
    assert_eq!(back.metadata_value("producer"), Some("lidar"));
}

#[test]
fn type_urns_survive_a_round_trip_through_serde() {
    let urn = TypeUrn::parse("std/sensor/v1/PointCloud[fields=x:y:z,frame=base_link]").unwrap();
    let json = serde_json::to_string(&urn).expect("a URN serialises");
    assert_eq!(
        json,
        "\"std/sensor/v1/PointCloud[fields=x:y:z,frame=base_link]\""
    );
    let back: TypeUrn = serde_json::from_str(&json).expect("and deserialises");
    assert_eq!(back, urn);
    assert_eq!(back.param("frame"), Some("base_link"));
}

#[test]
fn a_batch_of_every_p0_type_validates() {
    let fields = vec![
        Field::nullable("null", DataType::Null),
        Field::nullable("bool", DataType::Bool),
        Field::nullable("i8", DataType::Int8),
        Field::nullable("i64", DataType::Int64),
        Field::nullable("u8", DataType::UInt8),
        Field::nullable("u64", DataType::UInt64),
        Field::nullable("f16", DataType::Float16),
        Field::nullable("f64", DataType::Float64),
        Field::nullable("bin", DataType::Binary),
        Field::nullable("lbin", DataType::LargeBinary),
        Field::nullable("utf8", DataType::Utf8),
        Field::nullable("lutf8", DataType::LargeUtf8),
        Field::nullable("fsb", DataType::FixedSizeBinary(2)),
        Field::nullable(
            "fsl",
            DataType::fixed_size_list(Field::nullable("item", DataType::Int32), 2),
        ),
        Field::nullable(
            "list",
            DataType::list(Field::nullable("item", DataType::Utf8)),
        ),
        Field::nullable(
            "struct",
            DataType::strukt([Field::nullable("x", DataType::Int8)]),
        ),
        Field::nullable("ts", DataType::Timestamp),
        Field::nullable("dur", DataType::Duration),
    ];

    let columns: Vec<ArrayRef> = fields
        .iter()
        .map(|field| new_null_array(field.data_type(), 3).expect("every P0 type builds"))
        .collect();

    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema, columns).unwrap();
    assert_eq!(batch.num_rows(), 3);
    assert_eq!(batch.num_columns(), 18);
    for (field, column) in batch.iter() {
        assert_eq!(column.data_type(), field.data_type(), "{}", field.name());
        assert_eq!(column.null_count(), 3, "{}", field.name());
    }

    assert_eq!(batch.slice(1, 1).num_rows(), 1);
    assert_eq!(batch, batch.clone());
}
