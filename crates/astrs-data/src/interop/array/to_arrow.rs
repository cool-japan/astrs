//! astrs-data array -> arrow-rs array (zero-copy).
//!
//! Every family funnels through one [`arrow_data::ArrayData`] built by
//! `to_array_data`, then [`arrow_array::make_array`] wraps it in the
//! concrete arrow-rs type its [`arrow_schema::DataType`] names. Doing that
//! centrally (one validating call, [`arrow_data::ArrayDataBuilder::build`])
//! rather than importing each concrete arrow-rs array type once per family is
//! what keeps this file the size it is — the alternative is thirteen
//! constructors, each with its own idea of which buffer goes where, instead
//! of one `match` over [`crate::DataType`] that mirrors
//! `arrow_data::layout()`'s own per-type buffer table exactly (verified
//! against arrow-rs 59.2.0 source, one type at a time, before this module was
//! written).
//!
//! # `ArrayData.offset` stays `0` — with one exception
//!
//! Every buffer this module hands `ArrayDataBuilder` already represents the
//! array's exact logical window: `crate-data`'s own array families reslice
//! `values`/`offsets` directly on every `Array::slice` (verified against
//! `primitive.rs`, `binary.rs`, `list.rs`, `fixed_size_list.rs`,
//! `structs.rs`), the same "buffers are pre-windowed, no separate offset
//! field" shape `arrow_array`'s own concrete types use internally. So
//! `ArrayData.offset` is left at its default `0` throughout, **except** for
//! `Boolean`'s single values buffer: arrow's `BufferSpec::BitMap` layout has
//! no per-buffer bit-offset field of its own (unlike `nulls`, which carries
//! one independent of `ArrayData.offset` — confirmed against
//! `ArrayData::nulls`'s own doc comment), so a [`crate::Bitmap`] with a
//! non-byte-aligned [`crate::Bitmap::bit_offset`] is represented by setting
//! `ArrayData.offset` to that bit offset and passing the bitmap's backing
//! buffer through unchanged — confirmed against `BooleanArray::from(&self)`'s
//! implementation, which reconstructs its values as exactly
//! `BooleanBuffer::new(buffer, data.offset(), data.len())`.

use std::sync::Arc;

use arrow_array::ArrayRef as ArrowArrayRef;
use arrow_data::{ArrayData, ArrayDataBuilder};
use arrow_schema::{DataType as ArrowDataType, TimeUnit};

use crate::array::{
    Array, ArrayExt, BinaryArray, BooleanArray, DurationArray, FixedSizeBinaryArray,
    FixedSizeListArray, Float16Array, Float32Array, Float64Array, Int8Array, Int16Array,
    Int32Array, Int64Array, LargeBinaryArray, LargeStringArray, ListArray, PrimitiveArray,
    StringArray, StructArray, TimestampArray, UInt8Array, UInt16Array, UInt32Array, UInt64Array,
};
use crate::buffer::Buffer;
use crate::datatype::{ArrowNativeType, DataType};
use crate::interop::buffer::{to_arrow_buffer, to_arrow_nulls};
use crate::interop::datatype::to_arrow_field;
use crate::interop::error::Result;

/// Converts any astrs-data array to its arrow-rs equivalent, zero-copy.
///
/// # Errors
///
/// Whatever [`to_arrow_field`] (hence, transitively, this module's
/// `DataType`-to-`ArrowDataType` mapping) reports for a nested type's child
/// field, or an [`crate::interop::InteropError::Arrow`] from arrow-rs's own
/// [`arrow_data::ArrayDataBuilder::build`] validation (in practice
/// unreachable — every buffer this module builds already satisfies it — but
/// not asserted away, so a real problem is a typed error, not a panic).
///
/// ```
/// use astrs_data::array::{Array, Int32Array, IntoArrayRef};
/// use astrs_data::interop::to_arrow_array;
///
/// let array = Int32Array::from_opt_iter([Some(1), None, Some(3)]).into_array_ref();
/// let arrow_array = to_arrow_array(array.as_ref())?;
/// assert_eq!(arrow_array.len(), 3);
/// assert_eq!(arrow_array.null_count(), 1);
/// # Ok::<(), astrs_data::interop::InteropError>(())
/// ```
pub fn to_arrow_array(array: &dyn Array) -> Result<ArrowArrayRef> {
    Ok(arrow_array::make_array(to_array_data(array)?))
}

/// The recursive core [`to_arrow_array`] and every nested-type branch below
/// call to convert a child array or column.
pub(crate) fn to_array_data(array: &dyn Array) -> Result<ArrayData> {
    let nulls = array.validity().map(to_arrow_nulls);
    match array.data_type() {
        DataType::Null => Ok(ArrayDataBuilder::new(ArrowDataType::Null)
            .len(array.len())
            .build()?),
        DataType::Bool => bool_to_array_data(array.try_downcast::<BooleanArray>()?),
        DataType::Int8 => {
            primitive_to_array_data(array.try_downcast::<Int8Array>()?, ArrowDataType::Int8)
        }
        DataType::Int16 => {
            primitive_to_array_data(array.try_downcast::<Int16Array>()?, ArrowDataType::Int16)
        }
        DataType::Int32 => {
            primitive_to_array_data(array.try_downcast::<Int32Array>()?, ArrowDataType::Int32)
        }
        DataType::Int64 => {
            primitive_to_array_data(array.try_downcast::<Int64Array>()?, ArrowDataType::Int64)
        }
        DataType::UInt8 => {
            primitive_to_array_data(array.try_downcast::<UInt8Array>()?, ArrowDataType::UInt8)
        }
        DataType::UInt16 => {
            primitive_to_array_data(array.try_downcast::<UInt16Array>()?, ArrowDataType::UInt16)
        }
        DataType::UInt32 => {
            primitive_to_array_data(array.try_downcast::<UInt32Array>()?, ArrowDataType::UInt32)
        }
        DataType::UInt64 => {
            primitive_to_array_data(array.try_downcast::<UInt64Array>()?, ArrowDataType::UInt64)
        }
        DataType::Float16 => primitive_to_array_data(
            array.try_downcast::<Float16Array>()?,
            ArrowDataType::Float16,
        ),
        DataType::Float32 => primitive_to_array_data(
            array.try_downcast::<Float32Array>()?,
            ArrowDataType::Float32,
        ),
        DataType::Float64 => primitive_to_array_data(
            array.try_downcast::<Float64Array>()?,
            ArrowDataType::Float64,
        ),
        DataType::Timestamp => primitive_to_array_data(
            array.try_downcast::<TimestampArray>()?.as_primitive(),
            ArrowDataType::Timestamp(TimeUnit::Nanosecond, None),
        ),
        DataType::Duration => primitive_to_array_data(
            array.try_downcast::<DurationArray>()?.as_primitive(),
            ArrowDataType::Duration(TimeUnit::Nanosecond),
        ),
        DataType::Binary => {
            let a = array.try_downcast::<BinaryArray>()?;
            variable_length_to_array_data(
                ArrowDataType::Binary,
                a.len(),
                a.offsets_buffer().inner(),
                a.value_data(),
                nulls,
            )
        }
        DataType::LargeBinary => {
            let a = array.try_downcast::<LargeBinaryArray>()?;
            variable_length_to_array_data(
                ArrowDataType::LargeBinary,
                a.len(),
                a.offsets_buffer().inner(),
                a.value_data(),
                nulls,
            )
        }
        DataType::Utf8 => {
            let a = array.try_downcast::<StringArray>()?;
            variable_length_to_array_data(
                ArrowDataType::Utf8,
                a.len(),
                a.offsets_buffer().inner(),
                a.value_data(),
                nulls,
            )
        }
        DataType::LargeUtf8 => {
            let a = array.try_downcast::<LargeStringArray>()?;
            variable_length_to_array_data(
                ArrowDataType::LargeUtf8,
                a.len(),
                a.offsets_buffer().inner(),
                a.value_data(),
                nulls,
            )
        }
        DataType::FixedSizeBinary(size) => {
            let a = array.try_downcast::<FixedSizeBinaryArray>()?;
            Ok(ArrayDataBuilder::new(ArrowDataType::FixedSizeBinary(*size))
                .len(a.len())
                .align_buffers(true)
                .nulls(nulls)
                .add_buffer(to_arrow_buffer(a.value_data()))
                .build()?)
        }
        DataType::FixedSizeList(field, size) => {
            let a = array.try_downcast::<FixedSizeListArray>()?;
            let arrow_field = Arc::new(to_arrow_field(field)?);
            let child = to_array_data(a.values().as_ref())?;
            Ok(
                ArrayDataBuilder::new(ArrowDataType::FixedSizeList(arrow_field, *size))
                    .len(a.len())
                    .align_buffers(true)
                    .nulls(nulls)
                    .add_child_data(child)
                    .build()?,
            )
        }
        DataType::List(field) => {
            let a = array.try_downcast::<ListArray>()?;
            let arrow_field = Arc::new(to_arrow_field(field)?);
            let child = to_array_data(a.values().as_ref())?;
            Ok(ArrayDataBuilder::new(ArrowDataType::List(arrow_field))
                .len(a.len())
                .align_buffers(true)
                .nulls(nulls)
                .add_buffer(to_arrow_buffer(a.offsets_buffer().inner()))
                .add_child_data(child)
                .build()?)
        }
        DataType::Struct(fields) => {
            let a = array.try_downcast::<StructArray>()?;
            let mut arrow_fields = Vec::with_capacity(fields.len());
            let mut children = Vec::with_capacity(a.columns().len());
            for (field, column) in fields.iter().zip(a.columns()) {
                arrow_fields.push(to_arrow_field(field)?);
                children.push(to_array_data(column.as_ref())?);
            }
            Ok(
                ArrayDataBuilder::new(ArrowDataType::Struct(arrow_fields.into_iter().collect()))
                    .len(a.len())
                    .align_buffers(true)
                    .nulls(nulls)
                    .child_data(children)
                    .build()?,
            )
        }
    }
}

/// `Boolean`: one `BufferSpec::BitMap` buffer, addressed through
/// `ArrayData.offset` — see the module docs for why this is the one type that
/// sets it to something other than `0`.
fn bool_to_array_data(array: &BooleanArray) -> Result<ArrayData> {
    let values = array.values();
    Ok(ArrayDataBuilder::new(ArrowDataType::Boolean)
        .len(array.len())
        .offset(values.bit_offset())
        .align_buffers(true)
        .nulls(array.validity().map(to_arrow_nulls))
        .add_buffer(to_arrow_buffer(values.buffer()))
        .build()?)
}

/// Every fixed-width scalar family (`Int8`..`Float64`, and `Timestamp`/
/// `Duration` through their `as_primitive()` view): one `BufferSpec::
/// FixedWidth` values buffer, alignment guaranteed by construction (module
/// docs of `crate::interop::buffer`).
fn primitive_to_array_data<T: ArrowNativeType>(
    array: &PrimitiveArray<T>,
    arrow_type: ArrowDataType,
) -> Result<ArrayData> {
    Ok(ArrayDataBuilder::new(arrow_type)
        .len(array.len())
        .align_buffers(true)
        .nulls(array.validity().map(to_arrow_nulls))
        .add_buffer(to_arrow_buffer(array.values_buffer().inner()))
        .build()?)
}

/// The four `Binary`/`LargeBinary`/`Utf8`/`LargeUtf8` families share this
/// shape exactly: an offsets buffer (`BufferSpec::FixedWidth`) followed by a
/// `BufferSpec::VariableWidth` value-bytes buffer, neither carrying any
/// alignment requirement `crate-data`'s own buffers do not already meet (see
/// `crate::interop::buffer`'s module docs).
fn variable_length_to_array_data(
    arrow_type: ArrowDataType,
    len: usize,
    offsets: &Buffer,
    values: &Buffer,
    nulls: Option<arrow_buffer::NullBuffer>,
) -> Result<ArrayData> {
    Ok(ArrayDataBuilder::new(arrow_type)
        .len(len)
        .align_buffers(true)
        .nulls(nulls)
        .add_buffer(to_arrow_buffer(offsets))
        .add_buffer(to_arrow_buffer(values))
        .build()?)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::Arc as StdArc;

    use arrow_array::Array as ArrowArray;

    use super::*;
    use crate::array::{Float16Array, IntoArrayRef};
    use crate::datatype::{F16, Field};

    #[test]
    fn null_array_has_no_buffers_and_no_nulls() {
        let array = crate::array::NullArray::new(5).into_array_ref();
        let arrow_array = to_arrow_array(array.as_ref()).unwrap();
        assert_eq!(arrow_array.len(), 5);
        // `Null`'s "every slot is null" is implicit in the type, not encoded
        // in a bitmap, so arrow-rs reports it only through
        // `logical_null_count` (`null_count`, the physical/bitmap count, is
        // legitimately `0` here — there is no bitmap at all, matching
        // `to_array_data`'s `DataType::Null` arm, which sets no `nulls`).
        assert_eq!(arrow_array.null_count(), 0);
        assert_eq!(arrow_array.logical_null_count(), 5);
        assert_eq!(arrow_array.data_type(), &ArrowDataType::Null);
    }

    #[test]
    fn primitive_values_convert_pointer_identical() {
        let array = Int32Array::from_values([1, 2, 3]);
        let source_ptr = array.values_buffer().inner().as_ptr();
        let array = array.into_array_ref();
        let arrow_array = to_arrow_array(array.as_ref()).unwrap();
        assert_eq!(arrow_array.data_type(), &ArrowDataType::Int32);
        let downcast = arrow_array
            .as_any()
            .downcast_ref::<arrow_array::Int32Array>()
            .unwrap();
        assert_eq!(downcast.values(), &[1, 2, 3]);
        assert_eq!(
            downcast.values().inner().as_ptr(),
            source_ptr,
            "primitive values must convert with no copy"
        );
    }

    #[test]
    fn float16_converts_by_raw_bytes_no_half_dependency_needed() {
        let array =
            Float16Array::from_values([F16::from_f32(1.5), F16::from_f32(-2.0)]).into_array_ref();
        let arrow_array = to_arrow_array(array.as_ref()).unwrap();
        let downcast = arrow_array
            .as_any()
            .downcast_ref::<arrow_array::Float16Array>()
            .unwrap();
        assert_eq!(downcast.value(0).to_f32(), 1.5);
        assert_eq!(downcast.value(1).to_f32(), -2.0);
    }

    #[test]
    fn timestamp_and_duration_use_the_pinned_nanosecond_unit() {
        let stamps = crate::array::TimestampArray::from_nanos([1, 2]).into_array_ref();
        let arrow_stamps = to_arrow_array(stamps.as_ref()).unwrap();
        assert_eq!(
            arrow_stamps.data_type(),
            &ArrowDataType::Timestamp(TimeUnit::Nanosecond, None)
        );

        let gaps = crate::array::DurationArray::from_nanos([9]).into_array_ref();
        let arrow_gaps = to_arrow_array(gaps.as_ref()).unwrap();
        assert_eq!(
            arrow_gaps.data_type(),
            &ArrowDataType::Duration(TimeUnit::Nanosecond)
        );
    }

    #[test]
    fn boolean_values_convert_at_a_non_byte_aligned_offset() {
        let source: Vec<bool> = (0..40).map(|i| i % 5 == 0).collect();
        let full: crate::array::ArrayRef = StdArc::new(BooleanArray::from_values(source.clone()));
        let sliced = Array::slice(full.as_ref(), 11, 21);
        assert_ne!(
            sliced
                .downcast::<BooleanArray>()
                .unwrap()
                .values()
                .bit_offset()
                % 8,
            0,
            "the interesting case"
        );

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
        assert_eq!(downcast.len(), 21);
        for i in 0..21 {
            assert_eq!(downcast.value(i), source[11 + i], "index {i}");
        }
        assert_eq!(
            downcast.values().inner().as_ptr(),
            source_ptr,
            "boolean values must convert with no copy even at a bit offset"
        );
    }

    #[test]
    fn binary_and_string_families_carry_offsets_and_values() {
        let source = BinaryArray::from_opt_iter([Some(&b"aa"[..]), None, Some(&b"cccc"[..])]);
        let values_ptr = source.value_data().as_ptr();
        let array = source.into_array_ref();
        let arrow_array = to_arrow_array(array.as_ref()).unwrap();
        let downcast = arrow_array
            .as_any()
            .downcast_ref::<arrow_array::BinaryArray>()
            .unwrap();
        assert_eq!(downcast.value(0), b"aa");
        assert!(downcast.is_null(1));
        assert_eq!(downcast.value(2), b"cccc");
        assert_eq!(
            downcast.values().as_ptr(),
            values_ptr,
            "binary value bytes must convert with no copy"
        );

        let strings = StringArray::from_values(["lidar", "camera"]).into_array_ref();
        let arrow_strings = to_arrow_array(strings.as_ref()).unwrap();
        let downcast = arrow_strings
            .as_any()
            .downcast_ref::<arrow_array::StringArray>()
            .unwrap();
        assert_eq!(downcast.value(0), "lidar");
        assert_eq!(downcast.value(1), "camera");
    }

    #[test]
    fn fixed_size_binary_converts() {
        let source =
            FixedSizeBinaryArray::try_from_values(4, [b"abcd".as_slice(), b"efgh"]).unwrap();
        let values_ptr = source.value_data().as_ptr();
        let array = source.into_array_ref();
        let arrow_array = to_arrow_array(array.as_ref()).unwrap();
        let downcast = arrow_array
            .as_any()
            .downcast_ref::<arrow_array::FixedSizeBinaryArray>()
            .unwrap();
        assert_eq!(downcast.value(0), b"abcd");
        assert_eq!(downcast.value(1), b"efgh");
        assert_eq!(
            downcast.value_data().as_ptr(),
            values_ptr,
            "fixed-size binary bytes must convert with no copy"
        );
    }

    #[test]
    fn list_of_primitives_converts_with_its_child() {
        let array = ListArray::try_from_lengths(
            Field::new("item", DataType::Int32, true),
            [2usize, 0, 3],
            crate::array::Int32Array::from_values([1, 2, 3, 4, 5]).into_array_ref(),
        )
        .unwrap()
        .into_array_ref();
        let arrow_array = to_arrow_array(array.as_ref()).unwrap();
        let downcast = arrow_array
            .as_any()
            .downcast_ref::<arrow_array::ListArray>()
            .unwrap();
        assert_eq!(downcast.len(), 3);
        assert_eq!(downcast.value_length(0), 2);
        assert_eq!(downcast.value_length(1), 0);
        assert_eq!(downcast.value_length(2), 3);
    }

    #[test]
    fn fixed_size_list_converts_with_its_windowed_child() {
        let values = crate::array::Float32Array::from_values([1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        let array = FixedSizeListArray::try_new(
            Field::new("xyz", DataType::Float32, false),
            3,
            values.into_array_ref(),
            None,
        )
        .unwrap();
        let full: crate::array::ArrayRef = StdArc::new(array);
        let sliced = Array::slice(full.as_ref(), 1, 1);
        let arrow_array = to_arrow_array(sliced.as_ref()).unwrap();
        let downcast = arrow_array
            .as_any()
            .downcast_ref::<arrow_array::FixedSizeListArray>()
            .unwrap();
        assert_eq!(downcast.len(), 1);
        assert_eq!(downcast.value_length(), 3);
    }

    #[test]
    fn struct_array_converts_every_column() {
        let a = crate::array::Int32Array::from_values([1, 2]).into_array_ref();
        let b = StringArray::from_values(["x", "y"]).into_array_ref();
        let array = StructArray::try_new(
            vec![
                Field::new("a", DataType::Int32, false),
                Field::new("b", DataType::Utf8, false),
            ],
            vec![a, b],
            None,
        )
        .unwrap()
        .into_array_ref();
        let arrow_array = to_arrow_array(array.as_ref()).unwrap();
        let downcast = arrow_array
            .as_any()
            .downcast_ref::<arrow_array::StructArray>()
            .unwrap();
        assert_eq!(downcast.num_columns(), 2);
        assert_eq!(downcast.column(0).len(), 2);
    }

    #[test]
    fn nested_struct_of_list_round_trips_the_shape() {
        let child = ListArray::try_from_lengths(
            Field::new("item", DataType::Int8, true),
            [1usize, 3],
            crate::array::Int8Array::from_values([1, 2, 3, 4]).into_array_ref(),
        )
        .unwrap()
        .into_array_ref();
        let array = StructArray::try_new(
            vec![Field::new(
                "seq",
                DataType::list(Field::new("item", DataType::Int8, true)),
                false,
            )],
            vec![child],
            None,
        )
        .unwrap()
        .into_array_ref();
        let arrow_array = to_arrow_array(array.as_ref()).unwrap();
        let downcast = arrow_array
            .as_any()
            .downcast_ref::<arrow_array::StructArray>()
            .unwrap();
        let list_col = downcast
            .column(0)
            .as_any()
            .downcast_ref::<arrow_array::ListArray>()
            .unwrap();
        assert_eq!(list_col.value_length(0), 1);
        assert_eq!(list_col.value_length(1), 3);
    }
}
