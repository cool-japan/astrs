//! `take` — gather rows by an index array.
//!
//! Unlike [`crate::kernel::filter()`], the indices may repeat, run in any
//! order, and carry nulls (a null index produces a null output row, the
//! same as an out-of-range one would be an error rather than — this is the
//! one place a "row selector" kernel has to reject bad input instead of
//! silently coping with it, because an index is a *value* a caller computed,
//! not a structural fact about the array the way a mask position is).

use crate::array::{Array, ArrayRef, PrimitiveArray};
use crate::error::{DataError, Result};
use crate::kernel::gather::gather;
use crate::record_batch::RecordBatch;

/// Converts an arbitrary integer array of indices into `Vec<Option<usize>>`:
/// `None` for a null slot, `Some(row)` for a valid one.
///
/// # Errors
///
/// [`DataError::TakeIndexInvalid`] when a non-null index is negative, does
/// not fit `usize`, or is not smaller than `len` — and
/// [`DataError::DowncastFailed`] when `indices` is not one of the eight
/// integer array types.
fn index_positions(indices: &dyn Array, len: usize) -> Result<Vec<Option<usize>>> {
    macro_rules! extract {
        ($ty:ty) => {{
            let typed = indices
                .as_any()
                .downcast_ref::<PrimitiveArray<$ty>>()
                .ok_or_else(|| {
                    DataError::downcast_failed::<PrimitiveArray<$ty>>(indices.data_type().clone())
                })?;
            let mut positions = Vec::with_capacity(typed.len());
            for position in 0..typed.len() {
                positions.push(match typed.get(position) {
                    None => None,
                    Some(value) => Some(
                        usize::try_from(value)
                            .ok()
                            .filter(|&row| row < len)
                            .ok_or(DataError::TakeIndexInvalid { position, len })?,
                    ),
                });
            }
            positions
        }};
    }

    Ok(match indices.data_type() {
        crate::DataType::Int8 => extract!(i8),
        crate::DataType::Int16 => extract!(i16),
        crate::DataType::Int32 => extract!(i32),
        crate::DataType::Int64 => extract!(i64),
        crate::DataType::UInt8 => extract!(u8),
        crate::DataType::UInt16 => extract!(u16),
        crate::DataType::UInt32 => extract!(u32),
        crate::DataType::UInt64 => extract!(u64),
        other => {
            return Err(DataError::downcast_failed::<PrimitiveArray<i64>>(
                other.clone(),
            ));
        }
    })
}

/// Gathers `array`'s rows named by `indices` into a new array of
/// `indices.len()` rows.
///
/// `indices` may be any of the eight integer array types; a null index
/// produces a null output row.
///
/// # Errors
///
/// * [`DataError::DowncastFailed`] when `indices` is not an integer array.
/// * [`DataError::TakeIndexInvalid`] when a non-null index is negative, does
///   not fit `usize`, or is not smaller than `array.len()`.
///
/// ```
/// use astrs_data::array::{Array, Int32Array, IntoArrayRef};
/// use astrs_data::kernel::take;
///
/// let array = Int32Array::from_values([10, 20, 30, 40]).into_array_ref();
/// let indices = Int32Array::from_opt_iter([Some(3), None, Some(0)]).into_array_ref();
/// let taken = take(&array, indices.as_ref())?;
///
/// assert_eq!(taken.len(), 3);
/// assert!(taken.is_null(1));
/// # Ok::<(), astrs_data::DataError>(())
/// ```
pub fn take(array: &ArrayRef, indices: &dyn Array) -> Result<ArrayRef> {
    let positions = index_positions(indices, array.len())?;
    gather(array, &positions)
}

/// [`take`], applied to every column of `batch` with the same `indices`.
///
/// The row-selection kernel [`crate::kernel::filter()`]/replay need at the
/// message level: `indices` names which recorded rows survive, in whatever
/// order the caller wants them (a sort, a dedup, …), across every column at
/// once.
///
/// # Errors
///
/// Whatever [`take`] reports, for the first column it fails on.
///
/// ```
/// use astrs_data::array::{Int32Array, IntoArrayRef};
/// use astrs_data::kernel::take_batch;
/// use astrs_data::RecordBatch;
///
/// let batch = RecordBatch::from_payload(Int32Array::from_values([1, 2, 3]).into_array_ref());
/// let indices = Int32Array::from_values([2, 0]).into_array_ref();
/// let taken = take_batch(&batch, indices.as_ref())?;
/// assert_eq!(taken.num_rows(), 2);
/// # Ok::<(), astrs_data::DataError>(())
/// ```
pub fn take_batch(batch: &RecordBatch, indices: &dyn Array) -> Result<RecordBatch> {
    let mut columns = Vec::with_capacity(batch.num_columns());
    for column in batch.columns() {
        columns.push(take(column, indices)?);
    }
    RecordBatch::try_new_with_row_count(batch.schema_ref(), columns, indices.len())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::array::{Int32Array, IntoArrayRef, StringArray, UInt8Array};
    use crate::datatype::{DataType, Field, Schema};
    use std::sync::Arc;

    #[test]
    fn take_gathers_in_the_requested_order_with_repeats() {
        let array = Int32Array::from_values([10, 20, 30, 40]).into_array_ref();
        let indices = Int32Array::from_values([3, 3, 0]).into_array_ref();
        let taken = take(&array, indices.as_ref()).unwrap();
        let taken = taken.as_any().downcast_ref::<Int32Array>().unwrap();
        assert_eq!(taken.values(), &[40, 40, 10]);
    }

    #[test]
    fn a_null_index_produces_a_null_row() {
        let array = Int32Array::from_values([10, 20, 30]).into_array_ref();
        let indices = Int32Array::from_opt_iter([Some(1), None]).into_array_ref();
        let taken = take(&array, indices.as_ref()).unwrap();
        assert!(!taken.is_null(0));
        assert!(taken.is_null(1));
    }

    #[test]
    fn a_negative_index_is_rejected() {
        let array = Int32Array::from_values([10, 20, 30]).into_array_ref();
        let indices = Int32Array::from_values([-1]).into_array_ref();
        assert_eq!(
            take(&array, indices.as_ref()).unwrap_err(),
            DataError::TakeIndexInvalid {
                position: 0,
                len: 3
            }
        );
    }

    #[test]
    fn an_out_of_range_index_is_rejected() {
        let array = Int32Array::from_values([10, 20, 30]).into_array_ref();
        let indices = UInt8Array::from_values([3]).into_array_ref();
        assert_eq!(
            take(&array, indices.as_ref()).unwrap_err(),
            DataError::TakeIndexInvalid {
                position: 0,
                len: 3
            }
        );
    }

    #[test]
    fn every_integer_index_type_works() {
        let array = Int32Array::from_values([10, 20, 30]).into_array_ref();
        macro_rules! check {
            ($ty:ty) => {{
                let indices: crate::array::PrimitiveArray<$ty> = [2, 0].into_iter().collect();
                assert_eq!(take(&array, &indices).unwrap().len(), 2);
            }};
        }
        check!(i8);
        check!(i16);
        check!(i32);
        check!(i64);
        check!(u8);
        check!(u16);
        check!(u32);
        check!(u64);
    }

    #[test]
    fn a_non_integer_index_array_is_rejected() {
        let array = Int32Array::from_values([10, 20, 30]).into_array_ref();
        let indices = StringArray::from_values(["x"]).into_array_ref();
        assert!(matches!(
            take(&array, indices.as_ref()),
            Err(DataError::DowncastFailed { .. })
        ));
    }

    #[test]
    fn empty_indices_produce_an_empty_result() {
        let array = Int32Array::from_values([10, 20, 30]).into_array_ref();
        let indices = Int32Array::from_values([] as [i32; 0]).into_array_ref();
        let taken = take(&array, indices.as_ref()).unwrap();
        assert!(taken.is_empty());
    }

    #[test]
    fn take_batch_reorders_every_column_together() {
        let schema = Arc::new(Schema::new(vec![
            Field::required("id", DataType::Int32),
            Field::nullable("name", DataType::Utf8),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Int32Array::from_values([1, 2, 3]).into_array_ref(),
                StringArray::from_values(["a", "b", "c"]).into_array_ref(),
            ],
        )
        .unwrap();
        let indices = Int32Array::from_values([2, 0]).into_array_ref();
        let taken = take_batch(&batch, indices.as_ref()).unwrap();
        assert_eq!(taken.num_rows(), 2);
        let ids = taken.columns()[0]
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        assert_eq!(ids.values(), &[3, 1]);
        let names = taken.columns()[1]
            .as_any()
            .downcast_ref::<StringArray>()
            .unwrap();
        assert_eq!(names.get(0), Some("c"));
        assert_eq!(names.get(1), Some("a"));
    }
}
