//! `filter` — select rows by a boolean mask.
//!
//! A null mask entry excludes its row, the same as `false` — Arrow's own
//! filter convention, and the sensible one here too: "keep this row" is a
//! question a mask can only answer affirmatively, not one where "I don't
//! know" is a third option that produces a null *output* row the way a
//! [`crate::kernel::take()`] null index does. There is no row for "unknown
//! whether to keep" to attach to.

use crate::array::{Array, ArrayRef, BooleanArray};
use crate::error::{DataError, Result};
use crate::kernel::gather::gather;
use crate::record_batch::RecordBatch;

/// Keeps exactly the rows of `array` where `mask` is `Some(true)`, in order.
///
/// # Errors
///
/// [`DataError::MaskLengthMismatch`] when `mask.len() != array.len()`.
///
/// ```
/// use astrs_data::array::{Array, BooleanArray, Int32Array, IntoArrayRef};
/// use astrs_data::kernel::filter;
///
/// let array = Int32Array::from_values([10, 20, 30, 40]).into_array_ref();
/// let mask = BooleanArray::from_opt_iter([Some(true), Some(false), None, Some(true)]);
/// let kept = filter(&array, &mask)?;
///
/// assert_eq!(kept.len(), 2, "false and null both drop their row");
/// # Ok::<(), astrs_data::DataError>(())
/// ```
pub fn filter(array: &ArrayRef, mask: &BooleanArray) -> Result<ArrayRef> {
    if array.len() != mask.len() {
        return Err(DataError::MaskLengthMismatch {
            array_len: array.len(),
            mask_len: mask.len(),
        });
    }
    let positions: Vec<Option<usize>> = (0..array.len())
        .filter(|&index| mask.get(index) == Some(true))
        .map(Some)
        .collect();
    gather(array, &positions)
}

/// [`filter`], applied to every column of `batch` with the same `mask`.
///
/// The kernel `astrs-recording`'s replay path needs to select a time range —
/// or any other predicate — out of a recorded batch without hand-rolling a
/// per-column loop.
///
/// # Errors
///
/// [`DataError::MaskLengthMismatch`] when `mask.len() != batch.num_rows()`,
/// or whatever [`filter`] reports for the first column it fails on.
///
/// ```
/// use astrs_data::array::{BooleanArray, Int32Array, IntoArrayRef};
/// use astrs_data::kernel::filter_batch;
/// use astrs_data::RecordBatch;
///
/// let batch = RecordBatch::from_payload(Int32Array::from_values([1, 2, 3]).into_array_ref());
/// let mask = BooleanArray::from_values([true, false, true]);
/// let kept = filter_batch(&batch, &mask)?;
/// assert_eq!(kept.num_rows(), 2);
/// # Ok::<(), astrs_data::DataError>(())
/// ```
pub fn filter_batch(batch: &RecordBatch, mask: &BooleanArray) -> Result<RecordBatch> {
    if batch.num_rows() != mask.len() {
        return Err(DataError::MaskLengthMismatch {
            array_len: batch.num_rows(),
            mask_len: mask.len(),
        });
    }
    let kept_rows = mask.iter().filter(|&value| value == Some(true)).count();
    let mut columns = Vec::with_capacity(batch.num_columns());
    for column in batch.columns() {
        columns.push(filter(column, mask)?);
    }
    RecordBatch::try_new_with_row_count(batch.schema_ref(), columns, kept_rows)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::array::{Int32Array, IntoArrayRef, StringArray};
    use crate::datatype::{DataType, Field, Schema};
    use std::sync::Arc;

    #[test]
    fn keeps_only_true_rows_in_order() {
        let array = Int32Array::from_values([10, 20, 30, 40]).into_array_ref();
        let mask = BooleanArray::from_values([true, false, true, false]);
        let kept = filter(&array, &mask).unwrap();
        let kept = kept.as_any().downcast_ref::<Int32Array>().unwrap();
        assert_eq!(kept.values(), &[10, 30]);
    }

    #[test]
    fn null_mask_entries_drop_their_row_like_false() {
        let array = Int32Array::from_values([10, 20, 30]).into_array_ref();
        let mask = BooleanArray::from_opt_iter([Some(true), None, Some(true)]);
        let kept = filter(&array, &mask).unwrap();
        let kept = kept.as_any().downcast_ref::<Int32Array>().unwrap();
        assert_eq!(kept.values(), &[10, 30]);
        assert_eq!(kept.null_count(), 0);
    }

    #[test]
    fn a_mismatched_mask_length_is_rejected() {
        let array = Int32Array::from_values([10, 20, 30]).into_array_ref();
        let mask = BooleanArray::from_values([true, false]);
        assert_eq!(
            filter(&array, &mask).unwrap_err(),
            DataError::MaskLengthMismatch {
                array_len: 3,
                mask_len: 2
            }
        );
    }

    #[test]
    fn an_all_false_mask_yields_an_empty_array_of_the_same_type() {
        let array = Int32Array::from_values([10, 20, 30]).into_array_ref();
        let mask = BooleanArray::from_values([false, false, false]);
        let kept = filter(&array, &mask).unwrap();
        assert!(kept.is_empty());
        assert_eq!(kept.data_type(), array.data_type());
    }

    #[test]
    fn filter_batch_keeps_every_column_in_step() {
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
        let mask = BooleanArray::from_values([false, true, true]);
        let kept = filter_batch(&batch, &mask).unwrap();
        assert_eq!(kept.num_rows(), 2);
        let ids = kept.columns()[0]
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        assert_eq!(ids.values(), &[2, 3]);
    }

    #[test]
    fn filter_batch_rejects_a_mismatched_mask() {
        let batch = RecordBatch::from_payload(Int32Array::from_values([1, 2]).into_array_ref());
        let mask = BooleanArray::from_values([true]);
        assert_eq!(
            filter_batch(&batch, &mask).unwrap_err(),
            DataError::MaskLengthMismatch {
                array_len: 2,
                mask_len: 1
            }
        );
    }
}
