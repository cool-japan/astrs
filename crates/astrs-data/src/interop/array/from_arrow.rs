//! arrow-rs array -> astrs-data array (copying).
//!
//! Every conversion here downcasts to arrow-rs's own concrete array type
//! (`arrow_array::BooleanArray`, `arrow_array::PrimitiveArray<AT>`,
//! `arrow_array::GenericByteArray<T>`, ...) and reads through *its* own
//! accessors, deliberately never through `arrow_data::ArrayData`'s raw
//! `buffers()`/`offset()` directly. Two reasons, not one:
//!
//! 1. **It is the direction that must handle an arbitrary
//!    [`arrow_data::ArrayData::offset`].** A concrete arrow-rs type's own
//!    `From<ArrayData>` already resolves that offset correctly for its own
//!    layout — confirmed against arrow-rs 59.2.0 source for `PrimitiveArray`
//!    (`ScalarBuffer::new(buffer, offset, len)`), `StructArray` and
//!    `FixedSizeListArray` (both re-slice every child by
//!    `(offset, len)`/`(offset * size, len * size)` before this module ever
//!    sees them) — so this module gets a pre-resolved, correctly windowed
//!    view for free by using `.values()`/`.value_data()`/`.column(i)`
//!    instead of hand-deriving the same arithmetic. `FixedSizeBinary` is the
//!    cautionary tale: `ArrayData::buffer::<u8>(0)` skips `self.offset`
//!    *bytes*, not `self.offset * byte_width` bytes — exactly the kind of
//!    per-type width bug this module avoids by never touching that method.
//! 2. **It needs no `half` dependency for `Float16`.** `PrimitiveArray<AT>::
//!    values()` returns `&arrow_buffer::ScalarBuffer<AT::Native>` — for
//!    `AT = Float16Type`, `AT::Native` is `half::f16`, but this module never
//!    names it: `ScalarBuffer::inner()` strips straight back to the untyped
//!    `arrow_buffer::Buffer`, which is reinterpreted through this crate's own
//!    [`crate::ScalarBuffer::<F16>::try_new`] instead.
//!
//! Every buffer this module produces is a copy (see
//! `crate::interop::buffer`'s module docs for why the direction as a whole
//! cannot avoid one); the `unwrap_or_else` fallbacks scattered through it are
//! not reachable in practice (a freshly copied, correctly sized buffer always
//! satisfies the crate's own alignment/length constructors) but keep every
//! function here total without `unwrap`/`expect`, matching this crate's panic
//! policy (`crate::lib`'s module docs).

use arrow_array::types::{
    DurationNanosecondType, Float16Type, Float32Type, Float64Type, Int8Type, Int16Type, Int32Type,
    Int64Type, TimestampNanosecondType, UInt8Type, UInt16Type, UInt32Type, UInt64Type,
};
use arrow_array::{Array as ArrowArray, ArrowPrimitiveType};

use crate::array::{
    ArrayRef, BinaryArray, BooleanArray, DurationArray, FixedSizeBinaryArray, FixedSizeListArray,
    IntoArrayRef, LargeBinaryArray, LargeStringArray, ListArray, NullArray, PrimitiveArray,
    StringArray, StructArray, TimestampArray,
};
use crate::buffer::{Buffer, ScalarBuffer};
use crate::datatype::{ArrowNativeType, DataType};
use crate::interop::buffer::{from_arrow_boolean_buffer, from_arrow_buffer, from_arrow_nulls};
use crate::interop::datatype::from_arrow_data_type;
use crate::interop::error::{InteropError, Result};

/// Converts any arrow-rs array to its astrs-data equivalent, copying every
/// buffer (see the module docs for why).
///
/// # Errors
///
/// Whatever [`from_arrow_data_type`] reports for `array.data_type()`, or a
/// [`crate::interop::InteropError::Data`] from the astrs-data constructor a
/// leaf or nested type delegates to (a logical-content problem in the source
/// array — a null under a non-nullable field, a malformed offset sequence —
/// never a bug in this conversion's own buffer handling).
///
/// ```
/// use arrow_array::Int32Array as ArrowInt32Array;
/// use astrs_data::array::{Array, ArrayExt, Int32Array};
/// use astrs_data::interop::from_arrow_array;
///
/// let arrow_array = ArrowInt32Array::from(vec![Some(1), None, Some(3)]);
/// let array = from_arrow_array(&arrow_array)?;
/// assert_eq!(array.len(), 3);
/// assert_eq!(array.downcast::<Int32Array>().unwrap().get(1), None);
/// # Ok::<(), astrs_data::interop::InteropError>(())
/// ```
pub fn from_arrow_array(array: &dyn ArrowArray) -> Result<ArrayRef> {
    let data_type = from_arrow_data_type(array.data_type())?;
    match &data_type {
        DataType::Null => Ok(NullArray::new(array.len()).into_array_ref()),
        DataType::Bool => {
            let a = downcast::<arrow_array::BooleanArray>(array)?;
            let values = from_arrow_boolean_buffer(a.values());
            let validity = from_arrow_nulls(a.nulls());
            Ok(BooleanArray::new(values, validity)?.into_array_ref())
        }
        DataType::Int8 => primitive_from_arrow::<Int8Type, i8>(array),
        DataType::Int16 => primitive_from_arrow::<Int16Type, i16>(array),
        DataType::Int32 => primitive_from_arrow::<Int32Type, i32>(array),
        DataType::Int64 => primitive_from_arrow::<Int64Type, i64>(array),
        DataType::UInt8 => primitive_from_arrow::<UInt8Type, u8>(array),
        DataType::UInt16 => primitive_from_arrow::<UInt16Type, u16>(array),
        DataType::UInt32 => primitive_from_arrow::<UInt32Type, u32>(array),
        DataType::UInt64 => primitive_from_arrow::<UInt64Type, u64>(array),
        DataType::Float16 => primitive_from_arrow::<Float16Type, crate::datatype::F16>(array),
        DataType::Float32 => primitive_from_arrow::<Float32Type, f32>(array),
        DataType::Float64 => primitive_from_arrow::<Float64Type, f64>(array),
        DataType::Timestamp => {
            let raw = primitive_bytes::<TimestampNanosecondType>(array)?;
            let values = typed_scalar_buffer::<i64>(raw, array.len());
            let validity = from_arrow_nulls(array.nulls());
            Ok(TimestampArray::try_new(values, validity)?.into_array_ref())
        }
        DataType::Duration => {
            let raw = primitive_bytes::<DurationNanosecondType>(array)?;
            let values = typed_scalar_buffer::<i64>(raw, array.len());
            let validity = from_arrow_nulls(array.nulls());
            Ok(DurationArray::try_new(values, validity)?.into_array_ref())
        }
        DataType::Binary => {
            let a = downcast::<arrow_array::BinaryArray>(array)?;
            let offsets = typed_scalar_buffer::<i32>(
                from_arrow_buffer(a.offsets().inner().inner()),
                a.len() + 1,
            );
            let values = from_arrow_buffer(a.values());
            let validity = from_arrow_nulls(a.nulls());
            Ok(BinaryArray::try_new(offsets, values, validity)?.into_array_ref())
        }
        DataType::LargeBinary => {
            let a = downcast::<arrow_array::LargeBinaryArray>(array)?;
            let offsets = typed_scalar_buffer::<i64>(
                from_arrow_buffer(a.offsets().inner().inner()),
                a.len() + 1,
            );
            let values = from_arrow_buffer(a.values());
            let validity = from_arrow_nulls(a.nulls());
            Ok(LargeBinaryArray::try_new(offsets, values, validity)?.into_array_ref())
        }
        DataType::Utf8 => {
            let a = downcast::<arrow_array::StringArray>(array)?;
            let offsets = typed_scalar_buffer::<i32>(
                from_arrow_buffer(a.offsets().inner().inner()),
                a.len() + 1,
            );
            let values = Buffer::from_slice(a.value_data());
            let validity = from_arrow_nulls(a.nulls());
            Ok(StringArray::try_new(offsets, values, validity)?.into_array_ref())
        }
        DataType::LargeUtf8 => {
            let a = downcast::<arrow_array::LargeStringArray>(array)?;
            let offsets = typed_scalar_buffer::<i64>(
                from_arrow_buffer(a.offsets().inner().inner()),
                a.len() + 1,
            );
            let values = Buffer::from_slice(a.value_data());
            let validity = from_arrow_nulls(a.nulls());
            Ok(LargeStringArray::try_new(offsets, values, validity)?.into_array_ref())
        }
        DataType::FixedSizeBinary(size) => {
            let a = downcast::<arrow_array::FixedSizeBinaryArray>(array)?;
            let values = Buffer::from_slice(a.value_data());
            let validity = from_arrow_nulls(a.nulls());
            Ok(FixedSizeBinaryArray::try_new(*size, values, validity)?.into_array_ref())
        }
        DataType::FixedSizeList(field, size) => {
            let a = downcast::<arrow_array::FixedSizeListArray>(array)?;
            let values = from_arrow_array(a.values().as_ref())?;
            let validity = from_arrow_nulls(a.nulls());
            Ok(
                FixedSizeListArray::try_new(field.as_ref().clone(), *size, values, validity)?
                    .into_array_ref(),
            )
        }
        DataType::List(field) => {
            let a = downcast::<arrow_array::ListArray>(array)?;
            let offsets = typed_scalar_buffer::<i32>(
                from_arrow_buffer(a.offsets().inner().inner()),
                a.len() + 1,
            );
            let values = from_arrow_array(a.values().as_ref())?;
            let validity = from_arrow_nulls(a.nulls());
            Ok(
                ListArray::try_new(field.as_ref().clone(), offsets, values, validity)?
                    .into_array_ref(),
            )
        }
        DataType::Struct(fields) => {
            let a = downcast::<arrow_array::StructArray>(array)?;
            let mut columns = Vec::with_capacity(a.num_columns());
            for index in 0..a.num_columns() {
                columns.push(from_arrow_array(a.column(index).as_ref())?);
            }
            let validity = from_arrow_nulls(a.nulls());
            Ok(StructArray::try_new(fields.clone(), columns, validity)?.into_array_ref())
        }
    }
}

/// Downcasts `array` to the concrete arrow-rs type `A`, or reports
/// [`InteropError::ArrowDowncastFailed`].
fn downcast<A: 'static>(array: &dyn ArrowArray) -> Result<&A> {
    array
        .as_any()
        .downcast_ref::<A>()
        .ok_or_else(|| InteropError::ArrowDowncastFailed {
            data_type: array.data_type().clone(),
        })
}

/// Downcasts to the concrete `arrow_array::PrimitiveArray<AT>` and copies its
/// values buffer, without ever naming `AT::Native` — see the module docs.
fn primitive_bytes<AT: ArrowPrimitiveType>(array: &dyn ArrowArray) -> Result<Buffer> {
    let concrete = downcast::<arrow_array::PrimitiveArray<AT>>(array)?;
    Ok(from_arrow_buffer(concrete.values().inner()))
}

/// [`primitive_bytes`] plus the astrs-data
/// [`PrimitiveArray`](crate::array::PrimitiveArray) construction every
/// fixed-width family shares.
fn primitive_from_arrow<AT, T>(array: &dyn ArrowArray) -> Result<ArrayRef>
where
    AT: ArrowPrimitiveType,
    T: ArrowNativeType,
{
    let raw = primitive_bytes::<AT>(array)?;
    let values = typed_scalar_buffer::<T>(raw, array.len());
    let validity = from_arrow_nulls(array.nulls());
    Ok(PrimitiveArray::<T>::try_new(values, validity)?.into_array_ref())
}

/// Reinterprets a freshly copied byte [`Buffer`] as `ScalarBuffer<T>`.
///
/// Always succeeds in practice: `raw` was just built by [`from_arrow_buffer`]
/// (`Buffer::from_slice`), which always returns a 64-byte aligned allocation
/// — satisfying any `T` this crate supports — of exactly the byte length its
/// source arrow buffer had, which callers here have already sized to exactly
/// `expected_len * size_of::<T>()` bytes. The `unwrap_or_else` fallback keeps
/// the function total without `unwrap`/`expect` rather than asserting that
/// away.
fn typed_scalar_buffer<T: ArrowNativeType>(raw: Buffer, expected_len: usize) -> ScalarBuffer<T> {
    ScalarBuffer::try_new(raw).unwrap_or_else(|_| ScalarBuffer::zeroed(expected_len))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::Arc;

    use arrow_array::builder::{BooleanBuilder, Int32Builder, ListBuilder, StringBuilder};
    use arrow_array::{ArrayRef as ArrowArrayRef, Int32Array as ArrowInt32Array};
    use arrow_schema::{DataType as ArrowDataType, Field as ArrowField, Fields as ArrowFields};

    use super::*;
    use crate::array::{Array, ArrayExt};
    use crate::datatype::F16;

    #[test]
    fn null_array_converts_by_length_only() {
        let arrow_array = arrow_array::NullArray::new(4);
        let array = from_arrow_array(&arrow_array).unwrap();
        assert_eq!(array.len(), 4);
        assert_eq!(array.null_count(), 4);
    }

    #[test]
    fn primitive_array_round_trips_nulls() {
        let arrow_array = ArrowInt32Array::from(vec![Some(1), None, Some(3)]);
        let array = from_arrow_array(&arrow_array).unwrap();
        let downcast = array.downcast::<crate::array::Int32Array>().unwrap();
        assert_eq!(downcast.get(0), Some(1));
        assert_eq!(downcast.get(1), None);
        assert_eq!(downcast.get(2), Some(3));
    }

    #[test]
    fn float16_round_trips_without_this_crate_ever_naming_half() {
        // This module never depends on the `half` crate (see its module
        // docs), so the arrow-side `Float16Array` fixture is built by
        // round-tripping through `to_arrow_array` (tested independently in
        // `to_arrow`'s own suite) rather than constructed with `half::f16`
        // literals here.
        let original =
            crate::array::Float16Array::from_values([F16::from_f32(1.5), F16::from_f32(-2.0)]);
        let arrow_array = crate::interop::to_arrow_array(&original).unwrap();
        let array = from_arrow_array(arrow_array.as_ref()).unwrap();
        let downcast = array.downcast::<crate::array::Float16Array>().unwrap();
        assert_eq!(downcast.get(0), Some(F16::from_f32(1.5)));
        assert_eq!(downcast.get(1), Some(F16::from_f32(-2.0)));
    }

    #[test]
    fn timestamp_and_duration_decode_from_the_pinned_unit() {
        let arrow_stamps = arrow_array::TimestampNanosecondArray::from(vec![1_i64, 2]);
        let array = from_arrow_array(&arrow_stamps).unwrap();
        assert_eq!(array.data_type(), &DataType::Timestamp);
        let downcast = array.downcast::<TimestampArray>().unwrap();
        assert_eq!(downcast.get(0), Some(1));

        let arrow_gaps = arrow_array::DurationNanosecondArray::from(vec![9_i64]);
        let array = from_arrow_array(&arrow_gaps).unwrap();
        assert_eq!(array.data_type(), &DataType::Duration);
    }

    #[test]
    fn boolean_array_round_trips() {
        let mut builder = BooleanBuilder::new();
        builder.append_value(true);
        builder.append_null();
        builder.append_value(false);
        let arrow_array = builder.finish();
        let array = from_arrow_array(&arrow_array).unwrap();
        let downcast = array.downcast::<BooleanArray>().unwrap();
        assert_eq!(downcast.get(0), Some(true));
        assert_eq!(downcast.get(1), None);
        assert_eq!(downcast.get(2), Some(false));
    }

    #[test]
    fn binary_and_string_families_decode() {
        let arrow_array = arrow_array::BinaryArray::from_opt_vec(vec![
            Some(b"aa".as_slice()),
            None,
            Some(b"cccc"),
        ]);
        let array = from_arrow_array(&arrow_array).unwrap();
        let downcast = array.downcast::<BinaryArray>().unwrap();
        assert_eq!(downcast.get(0), Some(&b"aa"[..]));
        assert_eq!(downcast.get(1), None);
        assert_eq!(downcast.get(2), Some(&b"cccc"[..]));

        let mut builder = StringBuilder::new();
        builder.append_value("lidar");
        builder.append_value("camera");
        let arrow_strings = builder.finish();
        let array = from_arrow_array(&arrow_strings).unwrap();
        let downcast = array.downcast::<StringArray>().unwrap();
        assert_eq!(downcast.get(0), Some("lidar"));
        assert_eq!(downcast.get(1), Some("camera"));
    }

    #[test]
    fn fixed_size_binary_decodes() {
        let arrow_array = arrow_array::FixedSizeBinaryArray::try_from_iter(
            [b"abcd".as_slice(), b"efgh"].into_iter(),
        )
        .unwrap();
        let array = from_arrow_array(&arrow_array).unwrap();
        let downcast = array.downcast::<FixedSizeBinaryArray>().unwrap();
        assert_eq!(downcast.get(0), Some(&b"abcd"[..]));
        assert_eq!(downcast.get(1), Some(&b"efgh"[..]));
    }

    #[test]
    fn list_decodes_with_its_child() {
        let mut builder = ListBuilder::new(Int32Builder::new());
        builder.values().append_value(1);
        builder.values().append_value(2);
        builder.append(true);
        builder.append(false);
        builder.values().append_value(3);
        builder.append(true);
        let arrow_array = builder.finish();

        let array = from_arrow_array(&arrow_array).unwrap();
        let downcast = array.downcast::<ListArray>().unwrap();
        assert_eq!(downcast.value_length(0), Some(2));
        assert!(downcast.is_null(1), "null slot");
        assert_eq!(downcast.value_length(2), Some(1));
    }

    #[test]
    fn struct_decodes_every_column() {
        let a: ArrowArrayRef = Arc::new(ArrowInt32Array::from(vec![1, 2]));
        let b: ArrowArrayRef = Arc::new(arrow_array::StringArray::from(vec!["x", "y"]));
        let fields = ArrowFields::from(vec![
            ArrowField::new("a", ArrowDataType::Int32, false),
            ArrowField::new("b", ArrowDataType::Utf8, false),
        ]);
        let arrow_array = arrow_array::StructArray::new(fields, vec![a, b], None);

        let array = from_arrow_array(&arrow_array).unwrap();
        let downcast = array.downcast::<StructArray>().unwrap();
        assert_eq!(downcast.num_columns(), 2);
        assert_eq!(downcast.column(0).unwrap().len(), 2);
    }

    #[test]
    fn downcast_failure_reports_the_data_type() {
        // Exercised indirectly: every arm above already downcasts to the type
        // `from_arrow_data_type` names, so a mismatch cannot arise through
        // `from_arrow_array` itself. This test instead exercises the helper
        // directly so its error path has coverage.
        let arrow_array = ArrowInt32Array::from(vec![1]);
        let err = downcast::<arrow_array::BooleanArray>(&arrow_array).unwrap_err();
        assert_eq!(
            err,
            InteropError::ArrowDowncastFailed {
                data_type: ArrowDataType::Int32
            }
        );
    }

    #[test]
    fn unmappable_arrow_type_is_rejected_before_any_downcast() {
        let arrow_array = arrow_array::Date32Array::from(vec![1]);
        let err = from_arrow_array(&arrow_array).unwrap_err();
        assert!(matches!(err, InteropError::UnmappableArrowType { .. }));
    }
}
