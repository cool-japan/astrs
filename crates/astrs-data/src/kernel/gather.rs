//! The shared row-selection core behind [`crate::kernel::take`] and
//! [`crate::kernel::filter`].
//!
//! Both kernels reduce to the same question — "build a new array whose row
//! `i` is source row `positions[i]`, or null when `positions[i]` is `None`"
//! — [`take`](crate::kernel::take) from an index array (any row, any order,
//! repeats allowed, a null index means a null output row) and
//! [`filter`](crate::kernel::filter) from a boolean mask (only ascending,
//! only the `true` rows). [`gather`] is that one operation, written once.
//!
//! Every flat family (`Null`, `Bool`, the eleven primitives incl.
//! `Timestamp`/`Duration`, the four byte/string families,
//! `FixedSizeBinary`) gathers through a builder, one allocation for the
//! whole result — the allocation-conscious path the task calls for. The
//! three nested families (`FixedSizeList`, `List`, `Struct`) gather by
//! slicing one row at a time and handing the pieces to
//! [`crate::kernel::concat`]; see [`gather_nested`] for why that trade-off is
//! the pragmatic one here rather than a second bespoke builder walk per
//! nested shape.

use std::sync::Arc;

use crate::array::{
    ArrayExt, ArrayRef, BinaryArray, BooleanArray, DurationArray, FixedSizeBinaryArray,
    GenericBinaryArray, GenericStringArray, LargeBinaryArray, LargeStringArray, NullArray,
    OffsetSizeTrait, PrimitiveArray, StringArray, TimestampArray, new_null_array, slice_array,
};
use crate::builder::{
    BooleanBuilder, FixedSizeBinaryBuilder, GenericBinaryBuilder, GenericStringBuilder,
    PrimitiveBuilder,
};
use crate::datatype::{ArrowNativeType, DataType};
use crate::error::Result;
use crate::kernel::concat::concat;

/// Builds a new array of `positions.len()` rows from `array`, gathering row
/// `positions[i]` (or a null row for `None`) into output row `i`.
pub(crate) fn gather(array: &ArrayRef, positions: &[Option<usize>]) -> Result<ArrayRef> {
    match array.data_type() {
        DataType::Null => Ok(Arc::new(NullArray::new(positions.len()))),
        DataType::Bool => Ok(Arc::new(gather_boolean(array.try_downcast()?, positions))),
        DataType::Int8 => Ok(Arc::new(gather_primitive::<i8>(
            array.try_downcast()?,
            positions,
        ))),
        DataType::Int16 => Ok(Arc::new(gather_primitive::<i16>(
            array.try_downcast()?,
            positions,
        ))),
        DataType::Int32 => Ok(Arc::new(gather_primitive::<i32>(
            array.try_downcast()?,
            positions,
        ))),
        DataType::Int64 => Ok(Arc::new(gather_primitive::<i64>(
            array.try_downcast()?,
            positions,
        ))),
        DataType::UInt8 => Ok(Arc::new(gather_primitive::<u8>(
            array.try_downcast()?,
            positions,
        ))),
        DataType::UInt16 => Ok(Arc::new(gather_primitive::<u16>(
            array.try_downcast()?,
            positions,
        ))),
        DataType::UInt32 => Ok(Arc::new(gather_primitive::<u32>(
            array.try_downcast()?,
            positions,
        ))),
        DataType::UInt64 => Ok(Arc::new(gather_primitive::<u64>(
            array.try_downcast()?,
            positions,
        ))),
        DataType::Float16 => Ok(Arc::new(gather_primitive::<crate::F16>(
            array.try_downcast()?,
            positions,
        ))),
        DataType::Float32 => Ok(Arc::new(gather_primitive::<f32>(
            array.try_downcast()?,
            positions,
        ))),
        DataType::Float64 => Ok(Arc::new(gather_primitive::<f64>(
            array.try_downcast()?,
            positions,
        ))),
        DataType::Timestamp => {
            let typed: &TimestampArray = array.try_downcast()?;
            let combined = gather_primitive(typed.as_primitive(), positions);
            Ok(Arc::new(TimestampArray::from_primitive(&combined)?))
        }
        DataType::Duration => {
            let typed: &DurationArray = array.try_downcast()?;
            let combined = gather_primitive(typed.as_primitive(), positions);
            Ok(Arc::new(DurationArray::from_primitive(&combined)?))
        }
        DataType::Binary => Ok(Arc::new(gather_bytes::<i32>(
            array.try_downcast::<BinaryArray>()?,
            positions,
        ))),
        DataType::LargeBinary => Ok(Arc::new(gather_bytes::<i64>(
            array.try_downcast::<LargeBinaryArray>()?,
            positions,
        ))),
        DataType::Utf8 => Ok(Arc::new(gather_string::<i32>(
            array.try_downcast::<StringArray>()?,
            positions,
        ))),
        DataType::LargeUtf8 => Ok(Arc::new(gather_string::<i64>(
            array.try_downcast::<LargeStringArray>()?,
            positions,
        ))),
        DataType::FixedSizeBinary(size) => Ok(Arc::new(gather_fixed_size_binary(
            array.try_downcast()?,
            positions,
            *size,
        )?)),
        DataType::FixedSizeList(..) | DataType::List(_) | DataType::Struct(_) => {
            gather_nested(array, positions)
        }
    }
}

fn gather_boolean(array: &BooleanArray, positions: &[Option<usize>]) -> BooleanArray {
    let mut builder = BooleanBuilder::with_capacity(positions.len());
    for &position in positions {
        builder.append_option(position.and_then(|index| array.get(index)));
    }
    builder.finish()
}

/// Generic gather for any [`PrimitiveArray`], reused for every numeric type
/// and, via [`TimestampArray::as_primitive`]/[`DurationArray::as_primitive`],
/// for the two temporal types too.
fn gather_primitive<T: ArrowNativeType>(
    array: &PrimitiveArray<T>,
    positions: &[Option<usize>],
) -> PrimitiveArray<T> {
    let mut builder = PrimitiveBuilder::<T>::with_capacity(positions.len());
    for &position in positions {
        builder.append_option(position.and_then(|index| array.get(index)));
    }
    builder.finish()
}

fn gather_bytes<O: OffsetSizeTrait>(
    array: &GenericBinaryArray<O>,
    positions: &[Option<usize>],
) -> GenericBinaryArray<O> {
    let mut builder = GenericBinaryBuilder::<O>::with_capacity(positions.len(), 0);
    for &position in positions {
        builder.append_option(position.and_then(|index| array.get(index)));
    }
    builder.finish()
}

fn gather_string<O: OffsetSizeTrait>(
    array: &GenericStringArray<O>,
    positions: &[Option<usize>],
) -> GenericStringArray<O> {
    let mut builder = GenericStringBuilder::<O>::with_capacity(positions.len(), 0);
    for &position in positions {
        builder.append_option(position.and_then(|index| array.get(index)));
    }
    builder.finish()
}

fn gather_fixed_size_binary(
    array: &FixedSizeBinaryArray,
    positions: &[Option<usize>],
    size: i32,
) -> Result<FixedSizeBinaryArray> {
    let mut builder = FixedSizeBinaryBuilder::with_capacity(size, positions.len())?;
    for &position in positions {
        // Every value this can produce is either `None` or exactly `size`
        // bytes (it came from `array`, whose own invariant guarantees that),
        // so the only way `append_option` fails here is a bug in this
        // function, not in the caller's input.
        builder.append_option(position.and_then(|index| array.get(index)))?;
    }
    Ok(builder.finish())
}

/// Gathers a nested (`FixedSizeList`/`List`/`Struct`) row set by slicing one
/// row at a time and concatenating the pieces.
///
/// A dedicated builder walk (as the flat families above use) would need one
/// hand-written traversal per nested shape, recursing into whatever the
/// child type turns out to be — essentially reimplementing
/// [`crate::kernel::concat`]'s own per-family dispatch a second time, just
/// fed one row at a time instead of one array at a time. Reusing `concat`
/// directly costs an extra small allocation per selected row (one
/// single-row slice, which is zero-copy — an `Arc` bump — right up until
/// `concat` copies it into the merged result) in exchange for not
/// maintaining that logic twice. `take`/`filter`'s primary use, selecting a
/// subset of *messages* out of a recording, is not a per-row hot loop the
/// way decoding every field of every message is.
fn gather_nested(array: &ArrayRef, positions: &[Option<usize>]) -> Result<ArrayRef> {
    if positions.is_empty() {
        return new_null_array(array.data_type(), 0);
    }
    let mut rows = Vec::with_capacity(positions.len());
    for &position in positions {
        match position {
            Some(index) if index < array.len() => rows.push(slice_array(array, index, 1)),
            _ => rows.push(new_null_array(array.data_type(), 1)?),
        }
    }
    concat(&rows)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::array::{Float32Array, Int8Array, Int32Array, IntoArrayRef};

    #[test]
    fn gather_primitive_handles_nulls_from_either_side() {
        let array = Int32Array::from_opt_iter([Some(10), None, Some(30)]);
        let result = gather_primitive(&array, &[Some(2), None, Some(1), Some(99)]);
        assert_eq!(
            result.iter().collect::<Vec<_>>(),
            vec![Some(30), None, None, None]
        );
    }

    #[test]
    fn gather_dispatches_every_flat_family() {
        for array in [
            NullArray::new(3).into_array_ref(),
            BooleanArray::from_values([true, false, true]).into_array_ref(),
            Int8Array::from_values([1, 2, 3]).into_array_ref(),
            Float32Array::from_values([1.0, 2.0, 3.0]).into_array_ref(),
            StringArray::from_values(["a", "b", "c"]).into_array_ref(),
            BinaryArray::from_values([&b"a"[..], &b"b"[..], &b"c"[..]]).into_array_ref(),
            TimestampArray::from_nanos([1, 2, 3]).into_array_ref(),
            FixedSizeBinaryArray::try_from_values(2, [&b"aa"[..], &b"bb"[..], &b"cc"[..]])
                .unwrap()
                .into_array_ref(),
        ] {
            let result = gather(&array, &[Some(2), None, Some(0)]).unwrap();
            assert_eq!(result.len(), 3, "{:?}", array.data_type());
            assert_eq!(result.data_type(), array.data_type());
            assert!(result.is_null(1), "{:?}", array.data_type());
            // `Null` has no non-null slots at all, by definition — every
            // other family's row 0 came from a valid source position and
            // must not be null.
            if *array.data_type() != DataType::Null {
                assert!(!result.is_null(0), "{:?}", array.data_type());
            }
        }
    }

    #[test]
    fn gather_nested_dispatches_through_concat() {
        use crate::array::{FixedSizeListArray, ListArray, StructArray};
        use crate::datatype::Field;

        let fsl = FixedSizeListArray::try_new(
            Field::required("v", DataType::Int32),
            2,
            Int32Array::from_values(0..8).into_array_ref(),
            None,
        )
        .unwrap()
        .into_array_ref();
        let list = ListArray::try_from_lengths(
            Field::required("v", DataType::Int32),
            [2, 0, 1],
            Int32Array::from_values(0..3).into_array_ref(),
        )
        .unwrap()
        .into_array_ref();
        let strukt = StructArray::try_new(
            vec![Field::required("a", DataType::Int32)],
            vec![Int32Array::from_values(0..4).into_array_ref()],
            None,
        )
        .unwrap()
        .into_array_ref();

        for array in [fsl, list, strukt] {
            let result = gather(&array, &[Some(1), None, Some(0)]).unwrap();
            assert_eq!(result.len(), 3);
            assert!(result.is_null(1));
        }
    }

    #[test]
    fn gather_nested_handles_an_empty_position_list() {
        use crate::array::FixedSizeListArray;
        use crate::datatype::Field;

        let fsl = FixedSizeListArray::try_new(
            Field::required("v", DataType::Int32),
            2,
            Int32Array::from_values(0..8).into_array_ref(),
            None,
        )
        .unwrap()
        .into_array_ref();
        let result = gather(&fsl, &[]).unwrap();
        assert!(result.is_empty());
    }
}
