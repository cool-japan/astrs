//! Concatenation, for every array family in the closed set.
//!
//! `concat` is the one kernel every other kernel in this module either
//! resembles or calls: [`take()`](fn@crate::kernel::take) reduces "select these rows"
//! to "slice each one out, then `concat` the slices back together", and a
//! decoder assembling a multi-chunk `List`/`FixedSizeList` body needs exactly
//! this to re-flatten the pieces. It is also where validity has to be
//! stitched correctly across an *absent* bitmap (an input with no nulls)
//! meeting a *present* one (an input that has some) — get that wrong and a
//! concatenated array silently reports the wrong values as valid.

use std::sync::Arc;

use crate::array::{
    Array, ArrayExt, ArrayRef, BinaryArray, BooleanArray, DurationArray, FixedSizeBinaryArray,
    FixedSizeListArray, GenericBinaryArray, GenericStringArray, LargeBinaryArray, LargeStringArray,
    ListArray, NullArray, OffsetSizeTrait, PrimitiveArray, StringArray, StructArray,
    TimestampArray, new_empty_array, slice_array,
};
use crate::buffer::{AlignedBuf, Bitmap, BitmapBuilder, Buffer, ScalarBuffer};
use crate::datatype::{ArrowNativeType, DataType, Field};
use crate::error::{DataError, Result};

/// Concatenates `arrays` into one array of the same type.
///
/// The arrays must share **exactly** the same [`DataType`] (child field
/// names and nullability included, like
/// [`ColumnTypeCheck::Exact`](crate::record_batch::ColumnTypeCheck::Exact))
/// — a `List` whose child is named `item` does not concatenate with one
/// whose child is named `element` without a [`crate::kernel::cast()`] first,
/// because there is no principled way to pick a name for the result that is
/// not simply "whichever input happened to come first".
///
/// A single input is returned as an `Arc` clone, no copy.
///
/// # Errors
///
/// * [`DataError::ColumnCountMismatch`] `{ fields: 1, columns: 0 }` for zero
///   arrays — there being no `DataType` to build an empty result *of*,
///   mirroring how [`crate::RecordBatch`] reports "the shape you asked for
///   needs at least one thing that tells me the type".
/// * [`DataError::TypeMismatch`] when the arrays' types are not identical.
/// * [`DataError::OffsetWidthOverflow`] when a `Binary`/`Utf8`/`List`
///   concatenation would need more offset range than `i32` (or, for
///   `LargeBinary`/`LargeUtf8`, `i64`) can express.
///
/// ```
/// use astrs_data::array::{Array, Int32Array, IntoArrayRef};
/// use astrs_data::kernel::concat;
///
/// let a = Int32Array::from_values([1, 2]).into_array_ref();
/// let b = Int32Array::from_values([3, 4, 5]).into_array_ref();
/// let combined = concat(&[a, b])?;
/// assert_eq!(combined.len(), 5);
/// # Ok::<(), astrs_data::DataError>(())
/// ```
pub fn concat(arrays: &[ArrayRef]) -> Result<ArrayRef> {
    match arrays {
        [] => Err(DataError::ColumnCountMismatch {
            fields: 1,
            columns: 0,
        }),
        [one] => Ok(Arc::clone(one)),
        [first, rest @ ..] => {
            let data_type = first.data_type();
            for array in rest {
                if array.data_type() != data_type {
                    return Err(DataError::type_mismatch(
                        data_type.clone(),
                        array.data_type().clone(),
                    ));
                }
            }
            concat_dispatch(data_type, arrays)
        }
    }
}

/// The largest value `O` can represent, as a `usize` — `i32::MAX` for
/// `Binary`/`Utf8`, `i64::MAX` for `LargeBinary`/`LargeUtf8`. Used only to
/// report [`DataError::OffsetWidthOverflow`] with a meaningful `max`.
fn max_offset<O: OffsetSizeTrait>() -> usize {
    if O::IS_LARGE {
        usize::try_from(i64::MAX).unwrap_or(usize::MAX)
    } else {
        usize::try_from(i32::MAX).unwrap_or(usize::MAX)
    }
}

fn concat_dispatch(data_type: &DataType, arrays: &[ArrayRef]) -> Result<ArrayRef> {
    macro_rules! primitive_case {
        ($ty:ty) => {{
            let mut typed = Vec::with_capacity(arrays.len());
            for array in arrays {
                typed.push(array.try_downcast::<PrimitiveArray<$ty>>()?);
            }
            Arc::new(concat_primitive(&typed, data_type.clone())?) as ArrayRef
        }};
    }

    Ok(match data_type {
        DataType::Null => {
            let total_len: usize = arrays.iter().map(|array| array.len()).sum();
            Arc::new(NullArray::new(total_len))
        }
        DataType::Bool => {
            let mut typed = Vec::with_capacity(arrays.len());
            for array in arrays {
                typed.push(array.try_downcast::<BooleanArray>()?);
            }
            Arc::new(concat_boolean(&typed)?)
        }
        DataType::Int8 => primitive_case!(i8),
        DataType::Int16 => primitive_case!(i16),
        DataType::Int32 => primitive_case!(i32),
        DataType::Int64 => primitive_case!(i64),
        DataType::UInt8 => primitive_case!(u8),
        DataType::UInt16 => primitive_case!(u16),
        DataType::UInt32 => primitive_case!(u32),
        DataType::UInt64 => primitive_case!(u64),
        DataType::Float16 => primitive_case!(crate::F16),
        DataType::Float32 => primitive_case!(f32),
        DataType::Float64 => primitive_case!(f64),
        DataType::Timestamp => {
            let mut typed = Vec::with_capacity(arrays.len());
            for array in arrays {
                typed.push(array.try_downcast::<TimestampArray>()?.as_primitive());
            }
            let combined = concat_primitive(&typed, DataType::Int64)?;
            Arc::new(TimestampArray::from_primitive(&combined)?)
        }
        DataType::Duration => {
            let mut typed = Vec::with_capacity(arrays.len());
            for array in arrays {
                typed.push(array.try_downcast::<DurationArray>()?.as_primitive());
            }
            let combined = concat_primitive(&typed, DataType::Int64)?;
            Arc::new(DurationArray::from_primitive(&combined)?)
        }
        DataType::Binary => {
            let mut typed = Vec::with_capacity(arrays.len());
            for array in arrays {
                typed.push(array.try_downcast::<BinaryArray>()?);
            }
            Arc::new(concat_bytes(&typed)?)
        }
        DataType::LargeBinary => {
            let mut typed = Vec::with_capacity(arrays.len());
            for array in arrays {
                typed.push(array.try_downcast::<LargeBinaryArray>()?);
            }
            Arc::new(concat_bytes(&typed)?)
        }
        DataType::Utf8 => {
            let mut typed = Vec::with_capacity(arrays.len());
            for array in arrays {
                typed.push(array.try_downcast::<StringArray>()?);
            }
            Arc::new(concat_strings(&typed)?)
        }
        DataType::LargeUtf8 => {
            let mut typed = Vec::with_capacity(arrays.len());
            for array in arrays {
                typed.push(array.try_downcast::<LargeStringArray>()?);
            }
            Arc::new(concat_strings(&typed)?)
        }
        DataType::FixedSizeBinary(size) => {
            let mut typed = Vec::with_capacity(arrays.len());
            for array in arrays {
                typed.push(array.try_downcast::<FixedSizeBinaryArray>()?);
            }
            Arc::new(concat_fixed_size_binary(&typed, *size)?)
        }
        DataType::FixedSizeList(field, size) => {
            let mut typed = Vec::with_capacity(arrays.len());
            for array in arrays {
                typed.push(array.try_downcast::<FixedSizeListArray>()?);
            }
            Arc::new(concat_fixed_size_list(&typed, field, *size)?)
        }
        DataType::List(field) => {
            let mut typed = Vec::with_capacity(arrays.len());
            for array in arrays {
                typed.push(array.try_downcast::<ListArray>()?);
            }
            Arc::new(concat_list(&typed, field)?)
        }
        DataType::Struct(fields) => {
            let mut typed = Vec::with_capacity(arrays.len());
            for array in arrays {
                typed.push(array.try_downcast::<StructArray>()?);
            }
            Arc::new(concat_struct(&typed, fields)?)
        }
    })
}

/// Combines a set of optional validity bitmaps the way concatenation needs:
/// `None` (no nulls at all) only when every input was `None`; otherwise a
/// fresh, tightly packed bitmap with `true` standing in for every input that
/// had no bitmap of its own.
fn concat_validity<A: Array>(arrays: &[&A], total_len: usize) -> Option<Bitmap> {
    if arrays.iter().all(|array| array.validity().is_none()) {
        return None;
    }
    let mut builder = BitmapBuilder::with_capacity(total_len);
    for array in arrays {
        match array.validity() {
            Some(bits) => builder.append_bitmap(bits),
            None => builder.append_n(array.len(), true),
        }
    }
    Some(builder.finish())
}

fn concat_boolean(arrays: &[&BooleanArray]) -> Result<BooleanArray> {
    let total_len: usize = arrays.iter().map(|a| a.len()).sum();
    let mut values = BitmapBuilder::with_capacity(total_len);
    for array in arrays {
        values.append_bitmap(array.values());
    }
    let validity = concat_validity(arrays, total_len);
    BooleanArray::new(values.finish(), validity)
}

fn concat_primitive<T: ArrowNativeType>(
    arrays: &[&PrimitiveArray<T>],
    data_type: DataType,
) -> Result<PrimitiveArray<T>> {
    let total_len: usize = arrays.iter().map(|a| a.len()).sum();
    let mut values = Vec::with_capacity(total_len);
    for array in arrays {
        values.extend_from_slice(array.values());
    }
    let validity = concat_validity(arrays, total_len);
    PrimitiveArray::try_new_with_type(data_type, ScalarBuffer::from_slice(&values), validity)
}

/// The `[first offset, last offset)` byte window `offsets` actually covers,
/// so concatenation never copies bytes a slice left behind.
fn owned_value_window<'a, O: OffsetSizeTrait>(offsets: &[O], values: &'a [u8]) -> &'a [u8] {
    let (Some(&first), Some(&last)) = (offsets.first(), offsets.last()) else {
        return &[];
    };
    let start = first.to_usize().unwrap_or(0);
    let end = last.to_usize().unwrap_or(start);
    values.get(start..end).unwrap_or(&[])
}

/// Re-bases one family's worth of offset-addressed value regions
/// (`Binary`/`LargeBinary`/`Utf8`/`LargeUtf8`) into one fresh, zero-based
/// offset buffer plus one fresh value region.
///
/// Each input's own offsets may start anywhere (a sliced array's offsets do
/// not reset to zero — see [`GenericBinaryArray`]'s own doc) and may share a
/// backing buffer far larger than what its offsets actually cover, so the
/// caller passes in exactly the `[first offset, last offset)` window of each
/// input via [`owned_value_window`], and this re-expresses every offset
/// relative to that window's start.
///
/// # Errors
///
/// [`DataError::OffsetWidthOverflow`] when the combined byte length does not
/// fit `O`.
fn concat_offset_regions<O: OffsetSizeTrait>(
    offset_lists: &[&[O]],
    value_regions: &[&[u8]],
) -> Result<(Vec<O>, Vec<u8>)> {
    let total_rows: usize = offset_lists.iter().map(|o| o.len().saturating_sub(1)).sum();
    let total_bytes: usize = value_regions.iter().map(|region| region.len()).sum();

    let mut offsets = Vec::with_capacity(total_rows + 1);
    offsets.push(O::ZERO);
    let mut values = Vec::with_capacity(total_bytes);
    let mut byte_base = 0usize;

    for (own_offsets, region) in offset_lists.iter().zip(value_regions.iter()) {
        values.extend_from_slice(region);
        let Some(&first) = own_offsets.first() else {
            continue;
        };
        let first = first.to_usize().unwrap_or(0);
        for &entry in &own_offsets[1..] {
            let relative = entry.to_usize().unwrap_or(first).saturating_sub(first);
            let new_total = byte_base + relative;
            offsets.push(O::from_usize(new_total).ok_or_else(|| {
                DataError::OffsetWidthOverflow {
                    total: new_total,
                    max: max_offset::<O>(),
                }
            })?);
        }
        byte_base += region.len();
    }
    Ok((offsets, values))
}

fn concat_bytes<O: OffsetSizeTrait>(
    arrays: &[&GenericBinaryArray<O>],
) -> Result<GenericBinaryArray<O>> {
    let total_len: usize = arrays.iter().map(|a| a.len()).sum();
    let offset_lists: Vec<&[O]> = arrays.iter().map(|a| a.value_offsets()).collect();
    let value_regions: Vec<&[u8]> = arrays
        .iter()
        .map(|a| owned_value_window(a.value_offsets(), a.value_data().as_slice()))
        .collect();
    let (offsets, values) = concat_offset_regions(&offset_lists, &value_regions)?;
    let validity = concat_validity(arrays, total_len);
    GenericBinaryArray::try_new(
        ScalarBuffer::from_slice(&offsets),
        Buffer::from_slice(&values),
        validity,
    )
}

fn concat_strings<O: OffsetSizeTrait>(
    arrays: &[&GenericStringArray<O>],
) -> Result<GenericStringArray<O>> {
    let total_len: usize = arrays.iter().map(|a| a.len()).sum();
    let offset_lists: Vec<&[O]> = arrays.iter().map(|a| a.value_offsets()).collect();
    let value_regions: Vec<&[u8]> = arrays
        .iter()
        .map(|a| owned_value_window(a.value_offsets(), a.value_data().as_slice()))
        .collect();
    let (offsets, values) = concat_offset_regions(&offset_lists, &value_regions)?;
    let validity = concat_validity(arrays, total_len);
    // SAFETY: every byte in `values` came from a `[first offset, last
    // offset)` window of an already-validated `GenericStringArray`'s own
    // value region, copied verbatim and only re-based (the window's
    // *content* is untouched), and the offsets rebuilt above preserve every
    // original slot's exact byte length — so every slot the new offsets
    // carve out is exactly one already-valid-UTF-8 slot from some input,
    // landing on the same char boundary it started on.
    Ok(unsafe {
        GenericStringArray::new_unchecked(
            ScalarBuffer::from_slice(&offsets),
            Buffer::from_slice(&values),
            validity,
        )
    })
}

fn concat_fixed_size_binary(
    arrays: &[&FixedSizeBinaryArray],
    size: i32,
) -> Result<FixedSizeBinaryArray> {
    let total_len: usize = arrays.iter().map(|a| a.len()).sum();
    let width = usize::try_from(size).unwrap_or(0);
    let mut bytes = AlignedBuf::with_capacity(total_len.saturating_mul(width));
    for array in arrays {
        // Unlike the offset-addressed families, `FixedSizeBinaryArray`'s
        // value buffer *is* narrowed by slicing (its own `slice` scales the
        // window by `size`), so the whole buffer is always exactly this
        // array's own `len * size` bytes — no window extraction needed.
        bytes.extend_from_slice(array.value_data().as_slice());
    }
    let validity = concat_validity(arrays, total_len);
    FixedSizeBinaryArray::try_new(size, Buffer::from(bytes), validity)
}

fn concat_fixed_size_list(
    arrays: &[&FixedSizeListArray],
    field: &Field,
    size: i32,
) -> Result<FixedSizeListArray> {
    let total_len: usize = arrays.iter().map(|a| a.len()).sum();
    // Like `FixedSizeBinary`, the child *is* narrowed by slicing, so each
    // array's child is already exactly its own `len * size` values.
    let children: Vec<ArrayRef> = arrays.iter().map(|a| Arc::clone(a.values())).collect();
    let combined_child = concat(&children)?;
    let validity = concat_validity(arrays, total_len);
    Ok(FixedSizeListArray::from_parts(
        field.clone(),
        size,
        combined_child,
        validity,
    ))
}

fn concat_list(arrays: &[&ListArray], field: &Field) -> Result<ListArray> {
    let total_len: usize = arrays.iter().map(|a| a.len()).sum();

    // Unlike `FixedSizeList`, a `ListArray`'s child is *not* narrowed by
    // slicing (see `ListArray::slice`'s own doc), so each input's covered
    // window — `[first offset, last offset)` in child-element units — has
    // to be sliced out explicitly before concatenating, or a sliced input
    // would drag its whole backing child array into the result.
    let mut windows = Vec::with_capacity(arrays.len());
    let mut children = Vec::with_capacity(arrays.len());
    for array in arrays {
        let offsets = array.value_offsets();
        let first = i64::from(offsets.first().copied().unwrap_or(0));
        let last = i64::from(offsets.last().copied().unwrap_or(0));
        let window_len = (last - first).max(0);
        let start = usize::try_from(first).unwrap_or(0);
        let len = usize::try_from(window_len).unwrap_or(0);
        children.push(slice_array(array.values(), start, len));
        windows.push(first);
    }
    let combined_child = concat(&children)?;

    let mut offsets: Vec<i32> = Vec::with_capacity(total_len + 1);
    offsets.push(0);
    let mut element_base: i64 = 0;
    for (array, &first) in arrays.iter().zip(windows.iter()) {
        let own = array.value_offsets();
        for &entry in &own[1..] {
            let relative = i64::from(entry) - first;
            let new_total = element_base + relative;
            offsets.push(
                i32::try_from(new_total).map_err(|_| DataError::OffsetWidthOverflow {
                    total: usize::try_from(new_total).unwrap_or(usize::MAX),
                    max: max_offset::<i32>(),
                })?,
            );
        }
        let last = i64::from(own.last().copied().unwrap_or(0));
        element_base += (last - first).max(0);
    }

    let validity = concat_validity(arrays, total_len);
    Ok(ListArray::from_parts(
        field.clone(),
        ScalarBuffer::from_slice(&offsets),
        combined_child,
        validity,
    ))
}

fn concat_struct(arrays: &[&StructArray], fields: &[Field]) -> Result<StructArray> {
    let total_len: usize = arrays.iter().map(|a| a.len()).sum();
    if fields.is_empty() {
        // No columns to concatenate — `concat_validity` still needs a real
        // `StructArray` slice to read validity from, which we have.
        let validity = concat_validity(arrays, total_len);
        return Ok(StructArray::from_parts(
            Vec::new(),
            Vec::new(),
            total_len,
            validity,
        ));
    }
    let mut columns = Vec::with_capacity(fields.len());
    for index in 0..fields.len() {
        let column_parts: Vec<ArrayRef> = arrays
            .iter()
            .map(|array| Arc::clone(&array.columns()[index]))
            .collect();
        columns.push(concat(&column_parts)?);
    }
    let validity = concat_validity(arrays, total_len);
    Ok(StructArray::from_parts(
        fields.to_vec(),
        columns,
        total_len,
        validity,
    ))
}

/// Concatenates a set of [`crate::RecordBatch`]es that all share the same
/// [`crate::Schema`] (exactly — see [`concat()`]'s own doc on why field names
/// and nullability are not negotiable here either), column by column.
///
/// The natural operation behind assembling one recorded session's messages
/// (or a replayed subset of them, once [`crate::kernel::filter()`] has picked
/// the rows) back into a single batch.
///
/// # Errors
///
/// * [`DataError::ColumnCountMismatch`] `{ fields: 1, columns: 0 }` for zero
///   batches.
/// * [`DataError::TypeMismatch`] when the batches' schemas are not identical.
/// * Whatever [`concat()`] reports for an individual column.
///
/// ```
/// use astrs_data::array::{Int32Array, IntoArrayRef};
/// use astrs_data::kernel::concat_batches;
/// use astrs_data::RecordBatch;
///
/// let a = RecordBatch::from_payload(Int32Array::from_values([1, 2]).into_array_ref());
/// let b = RecordBatch::from_payload(Int32Array::from_values([3]).into_array_ref());
/// let combined = concat_batches(&[a, b])?;
/// assert_eq!(combined.num_rows(), 3);
/// # Ok::<(), astrs_data::DataError>(())
/// ```
pub fn concat_batches(batches: &[crate::RecordBatch]) -> Result<crate::RecordBatch> {
    match batches {
        [] => Err(DataError::ColumnCountMismatch {
            fields: 1,
            columns: 0,
        }),
        [one] => Ok(one.clone()),
        [first, rest @ ..] => {
            let schema = first.schema_ref();
            for batch in rest {
                if batch.schema() != &schema {
                    return Err(DataError::type_mismatch(
                        DataType::strukt(schema.fields().to_vec()),
                        DataType::strukt(batch.schema().fields().to_vec()),
                    ));
                }
            }
            let mut columns = Vec::with_capacity(schema.len());
            for index in 0..schema.len() {
                let parts: Vec<ArrayRef> = batches
                    .iter()
                    .map(|batch| Arc::clone(&batch.columns()[index]))
                    .collect();
                columns.push(if parts.is_empty() {
                    new_empty_array(
                        schema
                            .field(index)
                            .map_or(&DataType::Null, Field::data_type),
                    )?
                } else {
                    concat(&parts)?
                });
            }
            crate::RecordBatch::try_new(schema, columns)
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::array::{Float32Array, Int32Array, IntoArrayRef, TimestampArray, UInt16Array};
    use crate::datatype::Field;

    #[test]
    fn zero_arrays_is_an_error() {
        assert_eq!(
            concat(&[]).unwrap_err(),
            DataError::ColumnCountMismatch {
                fields: 1,
                columns: 0
            }
        );
    }

    #[test]
    fn one_array_is_a_pointer_clone() {
        let array = Int32Array::from_values([1, 2, 3]).into_array_ref();
        let result = concat(std::slice::from_ref(&array)).unwrap();
        assert!(Arc::ptr_eq(&array, &result));
    }

    #[test]
    fn mismatched_types_are_rejected() {
        let a = Int32Array::from_values([1]).into_array_ref();
        let b = crate::array::StringArray::from_values(["x"]).into_array_ref();
        assert!(matches!(
            concat(&[a, b]).unwrap_err(),
            DataError::TypeMismatch { .. }
        ));
    }

    #[test]
    fn concat_null() {
        let a = NullArray::new(2).into_array_ref();
        let b = NullArray::new(3).into_array_ref();
        let result = concat(&[a, b]).unwrap();
        assert_eq!(result.len(), 5);
        assert_eq!(result.null_count(), 5);
    }

    #[test]
    fn concat_boolean_propagates_absent_and_present_validity() {
        let a = BooleanArray::from_values([true, false]).into_array_ref(); // no validity buffer
        let b = BooleanArray::from_opt_iter([Some(true), None]).into_array_ref();
        let result = concat(&[a, b]).unwrap();
        let result = result.as_any().downcast_ref::<BooleanArray>().unwrap();
        assert_eq!(result.null_count(), 1);
        assert_eq!(
            result.iter().collect::<Vec<_>>(),
            vec![Some(true), Some(false), Some(true), None]
        );
    }

    #[test]
    fn concat_primitive_three_way_with_mixed_validity() {
        let a = Int32Array::from_values([1, 2]).into_array_ref();
        let b = Int32Array::from_opt_iter([None, Some(30)]).into_array_ref();
        let c = Int32Array::from_values([40]).into_array_ref();
        let result = concat(&[a, b, c]).unwrap();
        let result = result.as_any().downcast_ref::<Int32Array>().unwrap();
        assert_eq!(
            result.iter().collect::<Vec<_>>(),
            vec![Some(1), Some(2), None, Some(30), Some(40)]
        );
    }

    #[test]
    fn concat_primitive_of_a_sliced_array_only_takes_the_window() {
        let full = Int32Array::from_values(0..10);
        let window = full.slice(2, 3); // [2, 3, 4]
        let other = Int32Array::from_values([100]);
        let result = concat(&[window.into_array_ref(), other.into_array_ref()]).unwrap();
        let result = result.as_any().downcast_ref::<Int32Array>().unwrap();
        assert_eq!(result.values(), &[2, 3, 4, 100]);
    }

    #[test]
    fn concat_timestamp_keeps_its_type() {
        let a = TimestampArray::from_nanos([1, 2]).into_array_ref();
        let b = TimestampArray::from_nanos([3]).into_array_ref();
        let result = concat(&[a, b]).unwrap();
        assert_eq!(result.data_type(), &DataType::Timestamp);
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn concat_utf8_re_bases_offsets_across_sliced_inputs() {
        let full = StringArray::from_values(["aa", "bb", "cc"]);
        let window = full.slice(1, 2); // ["bb", "cc"], offsets not starting at 0
        let other = StringArray::from_values(["dd"]);
        let result = concat(&[window.into_array_ref(), other.into_array_ref()]).unwrap();
        let result = result.as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(result.get(0), Some("bb"));
        assert_eq!(result.get(1), Some("cc"));
        assert_eq!(result.get(2), Some("dd"));
        assert_eq!(result.value_offsets(), &[0, 2, 4, 6]);
    }

    #[test]
    fn concat_utf8_preserves_nulls() {
        let a = StringArray::from_opt_iter([Some("x"), None]).into_array_ref();
        let b = StringArray::from_values(["y"]).into_array_ref();
        let result = concat(&[a, b]).unwrap();
        let result = result.as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(result.get(0), Some("x"));
        assert_eq!(result.get(1), None);
        assert_eq!(result.get(2), Some("y"));
    }

    #[test]
    fn concat_binary_re_bases_like_utf8() {
        let full = BinaryArray::from_values([&b"aa"[..], &b"bb"[..], &b"cc"[..]]);
        let window = full.slice(1, 2);
        let result = concat(&[
            window.into_array_ref(),
            BinaryArray::from_values([&b"dd"[..]]).into_array_ref(),
        ])
        .unwrap();
        let result = result.as_any().downcast_ref::<BinaryArray>().unwrap();
        assert_eq!(result.get(0), Some(&b"bb"[..]));
        assert_eq!(result.get(1), Some(&b"cc"[..]));
        assert_eq!(result.get(2), Some(&b"dd"[..]));
    }

    #[test]
    fn concat_fixed_size_binary() {
        let a = FixedSizeBinaryArray::try_from_values(2, [&b"aa"[..], &b"bb"[..]]).unwrap();
        let b = FixedSizeBinaryArray::try_from_values(2, [&b"cc"[..]]).unwrap();
        let result = concat(&[a.into_array_ref(), b.into_array_ref()]).unwrap();
        let result = result
            .as_any()
            .downcast_ref::<FixedSizeBinaryArray>()
            .unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result.get(2), Some(&b"cc"[..]));
    }

    #[test]
    fn concat_fixed_size_list_of_a_sliced_array_only_takes_the_window() {
        let field = Field::required("v", DataType::Int32);
        let full = FixedSizeListArray::try_new(
            field,
            3,
            Int32Array::from_values(0..12).into_array_ref(),
            None,
        )
        .unwrap();
        let sliced = full.slice(1, 2); // rows 1..3, child narrowed to exactly [3..9)
        let result = concat(&[sliced.into_array_ref()]).unwrap();
        let result = result
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(
            result.values().len(),
            6,
            "the child window, not the whole backing array"
        );
    }

    #[test]
    fn concat_fixed_size_list_combines_children_and_validity() {
        let field = Field::required("v", DataType::Int32);
        let a = FixedSizeListArray::try_new(
            field.clone(),
            2,
            Int32Array::from_values([1, 2, 3, 4]).into_array_ref(),
            Some([true, false].into_iter().collect()),
        )
        .unwrap();
        let b = FixedSizeListArray::try_new(
            field,
            2,
            Int32Array::from_values([5, 6]).into_array_ref(),
            None,
        )
        .unwrap();
        let result = concat(&[a.into_array_ref(), b.into_array_ref()]).unwrap();
        let result = result
            .as_any()
            .downcast_ref::<FixedSizeListArray>()
            .unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result.null_count(), 1);
        assert!(result.get(1).is_none());
        let third = result.get(2).unwrap();
        let third = third.as_any().downcast_ref::<Int32Array>().unwrap();
        assert_eq!(third.values(), &[5, 6]);
    }

    #[test]
    fn concat_list_re_bases_child_windows_across_sliced_inputs() {
        let field = Field::required("v", DataType::Int32);
        let full = ListArray::try_from_lengths(
            field.clone(),
            [2, 0, 3],
            Int32Array::from_values(0..5).into_array_ref(),
        )
        .unwrap();
        let window = full.slice(1, 2); // rows: [] and [2,3,4], child NOT narrowed by ListArray::slice
        let other =
            ListArray::try_from_lengths(field, [1], Int32Array::from_values([99]).into_array_ref())
                .unwrap();
        let result = concat(&[window.into_array_ref(), other.into_array_ref()]).unwrap();
        let result = result.as_any().downcast_ref::<ListArray>().unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result.value_length(0), Some(0));
        let row1 = result.get(1).unwrap();
        let row1 = row1.as_any().downcast_ref::<Int32Array>().unwrap();
        assert_eq!(row1.values(), &[2, 3, 4]);
        let row2 = result.get(2).unwrap();
        let row2 = row2.as_any().downcast_ref::<Int32Array>().unwrap();
        assert_eq!(row2.values(), &[99]);
        // The child must not have dragged in the skipped [0,1] window from `full`.
        assert_eq!(result.values().len(), 4);
    }

    #[test]
    fn concat_list_preserves_null_lists_distinct_from_empty_ones() {
        let field = Field::required("v", DataType::Int32);
        let a = ListArray::try_from_lengths(
            field.clone(),
            [2, 0],
            Int32Array::from_values([1, 2]).into_array_ref(),
        )
        .unwrap()
        .with_validity(Some([true, false].into_iter().collect()))
        .unwrap();
        let b =
            ListArray::try_from_lengths(field, [1], Int32Array::from_values([9]).into_array_ref())
                .unwrap();
        let result = concat(&[a.into_array_ref(), b.into_array_ref()]).unwrap();
        let result = result.as_any().downcast_ref::<ListArray>().unwrap();
        assert!(result.get(0).is_some());
        assert!(result.get(1).is_none(), "null list, not merely empty");
        assert_eq!(result.value_length(1), Some(0));
        assert!(result.get(2).is_some());
    }

    #[test]
    fn concat_list_carries_a_null_rows_nonzero_offset_span_without_corruption() {
        // Arrow's List layout does not require a null row's own offset span
        // to be zero-length — nulling a slot is a validity-bitmap fact, not
        // an offset-rewriting one. `[3, 2]`-length rows means row 1 spans
        // two elements ([3, 5)) *before* it is nulled, so this is a
        // genuinely different fixture from the existing "null row happens
        // to also be empty" coverage above.
        let field = Field::required("v", DataType::Int32);
        let a = ListArray::try_from_lengths(
            field.clone(),
            [3, 2],
            Int32Array::from_values(0..5).into_array_ref(),
        )
        .unwrap()
        .with_validity(Some([true, false].into_iter().collect()))
        .unwrap();
        let b =
            ListArray::try_from_lengths(field, [1], Int32Array::from_values([99]).into_array_ref())
                .unwrap();
        let result = concat(&[a.into_array_ref(), b.into_array_ref()]).unwrap();
        let result = result.as_any().downcast_ref::<ListArray>().unwrap();
        assert_eq!(result.len(), 3);
        assert!(result.get(1).is_none(), "row 1 is null");
        assert_eq!(
            result.value_length(1),
            Some(2),
            "a null row's own offset span is transported as-is, not collapsed to zero"
        );
        let row2 = result.get(2).unwrap();
        let row2 = row2.as_any().downcast_ref::<Int32Array>().unwrap();
        assert_eq!(row2.values(), &[99]);
        assert_eq!(
            result.values().len(),
            6,
            "5 elements from `a` plus 1 from `b`, none dropped or duplicated around the null row"
        );
    }

    #[test]
    fn concat_struct_concatenates_every_column_and_row_validity() {
        let fields = vec![
            Field::required("id", DataType::Int32),
            Field::nullable("name", DataType::Utf8),
        ];
        let a = StructArray::try_new(
            fields.clone(),
            vec![
                Int32Array::from_values([1, 2]).into_array_ref(),
                StringArray::from_values(["a", "b"]).into_array_ref(),
            ],
            Some([true, false].into_iter().collect()),
        )
        .unwrap();
        let b = StructArray::try_new(
            fields,
            vec![
                Int32Array::from_values([3]).into_array_ref(),
                StringArray::from_values(["c"]).into_array_ref(),
            ],
            None,
        )
        .unwrap();
        let result = concat(&[a.into_array_ref(), b.into_array_ref()]).unwrap();
        let result = result.as_any().downcast_ref::<StructArray>().unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result.null_count(), 1);
        assert!(result.get(1).is_none());
        assert!(result.get(2).is_some());
        let ids = result.columns()[0]
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        assert_eq!(ids.values(), &[1, 2, 3]);
    }

    #[test]
    fn concat_struct_with_zero_fields_still_tracks_row_count_and_validity() {
        let a = StructArray::try_new_with_len(Vec::new(), Vec::new(), 2, None).unwrap();
        let b = StructArray::try_new_with_len(
            Vec::new(),
            Vec::new(),
            1,
            Some([false].into_iter().collect()),
        )
        .unwrap();
        let result = concat(&[a.into_array_ref(), b.into_array_ref()]).unwrap();
        assert_eq!(result.len(), 3);
        assert_eq!(result.null_count(), 1);
    }

    #[test]
    fn concat_nested_struct_of_lists() {
        let inner_field = Field::required("v", DataType::Int32);
        let struct_fields = vec![Field::required("xs", DataType::list(inner_field.clone()))];
        let a = StructArray::try_new(
            struct_fields.clone(),
            vec![
                ListArray::try_from_lengths(
                    inner_field.clone(),
                    [2],
                    Int32Array::from_values([1, 2]).into_array_ref(),
                )
                .unwrap()
                .into_array_ref(),
            ],
            None,
        )
        .unwrap();
        let b = StructArray::try_new(
            struct_fields,
            vec![
                ListArray::try_from_lengths(
                    inner_field,
                    [1],
                    Int32Array::from_values([9]).into_array_ref(),
                )
                .unwrap()
                .into_array_ref(),
            ],
            None,
        )
        .unwrap();
        let result = concat(&[a.into_array_ref(), b.into_array_ref()]).unwrap();
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn concat_empty_arrays_of_every_family_produces_an_empty_result() {
        let cases: Vec<ArrayRef> = vec![
            NullArray::new(0).into_array_ref(),
            BooleanArray::from_values([] as [bool; 0]).into_array_ref(),
            Int32Array::from_values([] as [i32; 0]).into_array_ref(),
            Float32Array::from_values([] as [f32; 0]).into_array_ref(),
            StringArray::from_values([] as [&str; 0]).into_array_ref(),
            BinaryArray::from_values([] as [&[u8]; 0]).into_array_ref(),
        ];
        for array in cases {
            let pair = [Arc::clone(&array), Arc::clone(&array)];
            let result = concat(&pair).unwrap();
            assert!(result.is_empty(), "{:?}", array.data_type());
            assert_eq!(result.data_type(), array.data_type());
        }
    }

    #[test]
    fn concat_batches_stitches_rows_across_a_multi_column_schema() {
        use crate::datatype::Schema;
        use crate::record_batch::RecordBatch;

        let schema = Arc::new(Schema::new(vec![
            Field::required("id", DataType::Int32),
            Field::nullable("weight", DataType::Float32),
        ]));
        let a = RecordBatch::try_new(
            schema.clone(),
            vec![
                Int32Array::from_values([1, 2]).into_array_ref(),
                Float32Array::from_opt_iter([Some(1.5), None]).into_array_ref(),
            ],
        )
        .unwrap();
        let b = RecordBatch::try_new(
            schema,
            vec![
                Int32Array::from_values([3]).into_array_ref(),
                Float32Array::from_values([2.5]).into_array_ref(),
            ],
        )
        .unwrap();
        let combined = concat_batches(&[a, b]).unwrap();
        assert_eq!(combined.num_rows(), 3);
        let ids = combined.columns()[0]
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        assert_eq!(ids.values(), &[1, 2, 3]);
    }

    #[test]
    fn concat_batches_rejects_mismatched_schemas() {
        use crate::datatype::Schema;
        use crate::record_batch::RecordBatch;

        let a = RecordBatch::from_payload(Int32Array::from_values([1]).into_array_ref());
        let other_schema = Arc::new(Schema::new(vec![Field::required("x", DataType::UInt16)]));
        let b = RecordBatch::try_new(
            other_schema,
            vec![UInt16Array::from_values([1u16]).into_array_ref()],
        )
        .unwrap();
        assert!(matches!(
            concat_batches(&[a, b]),
            Err(DataError::TypeMismatch { .. })
        ));
    }

    #[test]
    fn concat_batches_zero_is_an_error_and_one_is_a_clone() {
        use crate::record_batch::RecordBatch;

        assert!(concat_batches(&[]).is_err());
        let batch = RecordBatch::from_payload(Int32Array::from_values([1]).into_array_ref());
        let result = concat_batches(std::slice::from_ref(&batch)).unwrap();
        assert_eq!(result, batch);
    }
}
