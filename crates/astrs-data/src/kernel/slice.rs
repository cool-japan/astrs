//! Batch-level slicing.
//!
//! [`Array::slice`](crate::array::Array::slice) already gives every array a zero-copy, clamping window
//! (see its own doc for the crate-wide convention). What is missing at the
//! single-array level is a *batch* operation: narrow every column in a set
//! by the same window in one call, which [`RecordBatch::slice`](crate::record_batch::RecordBatch::slice) already does
//! for a batch that has a [`Schema`](crate::datatype::Schema) attached — this module is the same
//! operation over a bare `&[ArrayRef]`, for the callers upstream of a batch
//! (a decoder assembling one, a kernel operating column-wise before a
//! `Schema` exists yet) that only have that.

use crate::array::{ArrayRef, slice_array};
use crate::error::{DataError, Result};

/// Slices every column in `columns` to the same `[offset, offset + len)`
/// window, clamped to the shortest column (the crate-wide convention — see
/// [`Array::slice`](crate::array::Array::slice)).
///
/// A full-range request on a column is a cheap `Arc` clone rather than a
/// rebuild (exactly what [`crate::array::slice_array`] already does per
/// column).
///
/// ```
/// use astrs_data::array::{Int32Array, IntoArrayRef, StringArray};
/// use astrs_data::kernel::slice_columns;
///
/// let columns = vec![
///     Int32Array::from_values([1, 2, 3, 4]).into_array_ref(),
///     StringArray::from_values(["a", "b", "c", "d"]).into_array_ref(),
/// ];
/// let window = slice_columns(&columns, 1, 2);
/// assert_eq!(window[0].len(), 2);
/// assert_eq!(window[1].len(), 2);
/// ```
#[must_use]
pub fn slice_columns(columns: &[ArrayRef], offset: usize, len: usize) -> Vec<ArrayRef> {
    columns
        .iter()
        .map(|column| slice_array(column, offset, len))
        .collect()
}

/// Checked [`slice_columns`].
///
/// # Errors
///
/// [`DataError::SliceOutOfBounds`] naming the shortest column's length when
/// `[offset, offset + len)` leaves it.
pub fn try_slice_columns(columns: &[ArrayRef], offset: usize, len: usize) -> Result<Vec<ArrayRef>> {
    let Some(shortest) = columns.iter().map(|column| column.len()).min() else {
        return Ok(Vec::new());
    };
    if offset.saturating_add(len) > shortest {
        return Err(DataError::SliceOutOfBounds {
            offset,
            len,
            available: shortest,
        });
    }
    Ok(slice_columns(columns, offset, len))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::array::{Int32Array, IntoArrayRef, StringArray};
    use std::sync::Arc;

    fn sample() -> Vec<ArrayRef> {
        vec![
            Int32Array::from_values(0..10).into_array_ref(),
            StringArray::from_values((0..10).map(|i| i.to_string())).into_array_ref(),
        ]
    }

    #[test]
    fn slices_every_column_by_the_same_window() {
        let window = slice_columns(&sample(), 2, 3);
        assert_eq!(window[0].len(), 3);
        assert_eq!(window[1].len(), 3);
        assert_eq!(
            window[0]
                .as_any()
                .downcast_ref::<Int32Array>()
                .unwrap()
                .values(),
            &[2, 3, 4]
        );
    }

    #[test]
    fn clamps_like_every_other_slice_in_the_crate() {
        let window = slice_columns(&sample(), 8, 99);
        assert_eq!(window[0].len(), 2);
        let window = slice_columns(&sample(), 99, 1);
        assert_eq!(window[0].len(), 0);
    }

    #[test]
    fn a_full_range_request_is_a_pointer_clone() {
        let columns = sample();
        let window = slice_columns(&columns, 0, 10);
        assert!(Arc::ptr_eq(&columns[0], &window[0]));
    }

    #[test]
    fn empty_column_set_slices_to_nothing() {
        assert!(slice_columns(&[], 0, 5).is_empty());
        assert!(try_slice_columns(&[], 0, 5).unwrap().is_empty());
    }

    #[test]
    fn try_slice_columns_reports_out_of_range_against_the_shortest_column() {
        let columns = vec![
            Int32Array::from_values(0..10).into_array_ref(),
            StringArray::from_values(["a", "b", "c"]).into_array_ref(),
        ];
        assert!(try_slice_columns(&columns, 0, 3).is_ok());
        assert_eq!(
            try_slice_columns(&columns, 0, 4).unwrap_err(),
            DataError::SliceOutOfBounds {
                offset: 0,
                len: 4,
                available: 3
            }
        );
    }
}
