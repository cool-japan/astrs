//! [`RecordBatch`] <-> arrow-rs [`ArrowRecordBatch`] conversion.
//!
//! A straight column-wise map through [`to_arrow_array`]/[`from_arrow_array`]
//! plus [`to_arrow_schema`]/[`from_arrow_schema`] for the schema — every
//! interesting decision (buffer zero-copy, `DataType` mapping, error
//! shape) already lives in those modules. The one thing this module owns is
//! the row count: both sides can express a batch with zero columns but a
//! non-zero row count (blueprint §6.1's `RecordBatch::try_new_with_row_count`
//! doc explains why AstRS needs this — Arrow IPC's `RecordBatch` header
//! itself carries the row count as its own field, independent of column
//! presence, and the golden corpus under `tests/golden/arrow/` has a case
//! that is exactly this shape), so both directions state the row count
//! explicitly rather than inferring it from the first column.

use std::sync::Arc;

use arrow_array::{RecordBatch as ArrowRecordBatch, RecordBatchOptions as ArrowRecordBatchOptions};

use crate::datatype::Schema;
use crate::interop::array::{from_arrow_array, to_arrow_array};
use crate::interop::datatype::{from_arrow_schema, to_arrow_schema};
use crate::interop::error::{InteropError, Result};
use crate::record_batch::RecordBatch;

/// Converts a [`RecordBatch`] to its arrow-rs equivalent, column by column.
///
/// # Errors
///
/// Whatever [`to_arrow_schema`] or [`to_arrow_array`] report for the first
/// offending field or column, or an
/// [`crate::interop::InteropError::Arrow`] from arrow-rs's own
/// `RecordBatch::try_new_with_options` validation.
///
/// ```
/// use astrs_data::array::{Int32Array, IntoArrayRef};
/// use astrs_data::interop::to_arrow_record_batch;
/// use astrs_data::RecordBatch;
///
/// let batch = RecordBatch::from_payload(Int32Array::from_values([1, 2, 3]).into_array_ref());
/// let arrow_batch = to_arrow_record_batch(&batch)?;
/// assert_eq!(arrow_batch.num_rows(), 3);
/// assert_eq!(arrow_batch.num_columns(), 1);
/// # Ok::<(), astrs_data::interop::InteropError>(())
/// ```
pub fn to_arrow_record_batch(batch: &RecordBatch) -> Result<ArrowRecordBatch> {
    let schema = Arc::new(to_arrow_schema(batch.schema())?);
    let mut columns = Vec::with_capacity(batch.num_columns());
    for column in batch.columns() {
        columns.push(to_arrow_array(column.as_ref())?);
    }
    let options = ArrowRecordBatchOptions::new().with_row_count(Some(batch.num_rows()));
    Ok(ArrowRecordBatch::try_new_with_options(
        schema, columns, &options,
    )?)
}

/// Converts an arrow-rs record batch to a [`RecordBatch`], column by column.
///
/// # Errors
///
/// Whatever [`from_arrow_schema`] or [`from_arrow_array`] report for the
/// first offending field or column, or an
/// [`crate::interop::InteropError::Data`] from
/// [`RecordBatch::try_new_with_row_count`] (a logical mismatch between the
/// arrow schema and its own columns — arrow-rs already guarantees these
/// agree for any `ArrowRecordBatch` that exists, so unreachable in practice).
///
/// ```
/// use arrow_array::{Int32Array as ArrowInt32Array, RecordBatch as ArrowRecordBatch};
/// use arrow_schema::{DataType as ArrowDataType, Field as ArrowField, Schema as ArrowSchema};
/// use astrs_data::interop::from_arrow_record_batch;
/// use std::sync::Arc;
///
/// let arrow_schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
///     "data",
///     ArrowDataType::Int32,
///     false,
/// )]));
/// let arrow_batch = ArrowRecordBatch::try_new(
///     arrow_schema,
///     vec![Arc::new(ArrowInt32Array::from(vec![1, 2]))],
/// )?;
/// let batch = from_arrow_record_batch(&arrow_batch)?;
/// assert_eq!(batch.num_rows(), 2);
/// # Ok::<(), astrs_data::interop::InteropError>(())
/// ```
pub fn from_arrow_record_batch(batch: &ArrowRecordBatch) -> Result<RecordBatch> {
    let schema: Schema = from_arrow_schema(batch.schema_ref())?;
    let mut columns = Vec::with_capacity(batch.num_columns());
    for index in 0..batch.num_columns() {
        columns.push(from_arrow_array(batch.column(index).as_ref())?);
    }
    Ok(RecordBatch::try_new_with_row_count(
        Arc::new(schema),
        columns,
        batch.num_rows(),
    )?)
}

/// `TryFrom` shape over [`to_arrow_record_batch`], for callers who prefer the
/// trait spelling (blueprint §6.1's own wording) to the free function.
///
/// Both exist rather than only the trait: the array-level conversions
/// (`crate::interop::array`) cannot be `TryFrom` impls at all (their `Self`
/// would be the opaque `Arc<dyn Array>`/`Arc<dyn ArrowArray>` trait objects —
/// see that module's own docs for why that makes a trait impl strictly worse
/// than a named function), so every array-level and `RecordBatch`-level
/// conversion in this crate is available as a plain function regardless; this
/// impl is a three-line delegation on top, not a second implementation to
/// keep in sync.
impl TryFrom<&RecordBatch> for ArrowRecordBatch {
    type Error = InteropError;

    fn try_from(batch: &RecordBatch) -> Result<Self> {
        to_arrow_record_batch(batch)
    }
}

/// `TryFrom` shape over [`from_arrow_record_batch`] — see
/// `impl TryFrom<&RecordBatch> for ArrowRecordBatch` above for why both the
/// function and this impl exist.
impl TryFrom<&ArrowRecordBatch> for RecordBatch {
    type Error = InteropError;

    fn try_from(batch: &ArrowRecordBatch) -> Result<Self> {
        from_arrow_record_batch(batch)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::Arc;

    use arrow_array::{Int32Array as ArrowInt32Array, RecordBatch as ArrowRecordBatch};
    use arrow_schema::{DataType as ArrowDataType, Field as ArrowField, Schema as ArrowSchema};

    use super::*;
    use crate::array::{ArrayExt, Int32Array, IntoArrayRef, StringArray};
    use crate::datatype::{DataType, Field};

    #[test]
    fn single_column_payload_batch_round_trips() {
        let batch = RecordBatch::from_payload(
            Int32Array::from_opt_iter([Some(1), None, Some(3)]).into_array_ref(),
        );
        let arrow_batch = to_arrow_record_batch(&batch).unwrap();
        assert_eq!(arrow_batch.num_rows(), 3);
        assert_eq!(arrow_batch.num_columns(), 1);

        let back = from_arrow_record_batch(&arrow_batch).unwrap();
        assert_eq!(back, batch);
    }

    #[test]
    fn multi_column_batch_round_trips_with_metadata() {
        let schema = Arc::new(
            Schema::new(vec![
                Field::new("id", DataType::Int32, false),
                Field::new("name", DataType::Utf8, true),
            ])
            .with_metadata_entry("_schema_hash", "deadbeef"),
        );
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Int32Array::from_values([7, 8]).into_array_ref(),
                StringArray::from_opt_iter([Some("lidar"), None]).into_array_ref(),
            ],
        )
        .unwrap();

        let arrow_batch = to_arrow_record_batch(&batch).unwrap();
        assert_eq!(
            arrow_batch.schema_ref().metadata().get("_schema_hash"),
            Some(&"deadbeef".to_owned())
        );

        let back = from_arrow_record_batch(&arrow_batch).unwrap();
        assert_eq!(back, batch);
        assert_eq!(
            back.schema().metadata_value("_schema_hash"),
            Some("deadbeef")
        );
    }

    #[test]
    fn zero_column_batch_with_rows_round_trips() {
        let batch =
            RecordBatch::try_new_with_row_count(Arc::new(Schema::default()), Vec::new(), 12)
                .unwrap();
        let arrow_batch = to_arrow_record_batch(&batch).unwrap();
        assert_eq!(arrow_batch.num_rows(), 12);
        assert_eq!(arrow_batch.num_columns(), 0);

        let back = from_arrow_record_batch(&arrow_batch).unwrap();
        assert_eq!(back.num_rows(), 12);
        assert_eq!(back.num_columns(), 0);
    }

    #[test]
    fn from_arrow_side_round_trips_too() {
        let arrow_schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
            "data",
            ArrowDataType::Int32,
            false,
        )]));
        let arrow_batch = ArrowRecordBatch::try_new(
            arrow_schema,
            vec![Arc::new(ArrowInt32Array::from(vec![1, 2, 3]))],
        )
        .unwrap();

        let batch = from_arrow_record_batch(&arrow_batch).unwrap();
        assert_eq!(batch.num_rows(), 3);
        let column = batch.column(0).unwrap();
        assert_eq!(
            column.downcast::<Int32Array>().unwrap().values(),
            &[1, 2, 3]
        );
    }

    #[test]
    fn the_try_from_impls_agree_with_the_functions_they_delegate_to() {
        let batch = RecordBatch::from_payload(Int32Array::from_values([1, 2, 3]).into_array_ref());

        let via_function = to_arrow_record_batch(&batch).unwrap();
        let via_trait = ArrowRecordBatch::try_from(&batch).unwrap();
        assert_eq!(via_function, via_trait);

        let back_via_function = from_arrow_record_batch(&via_trait).unwrap();
        let back_via_trait = RecordBatch::try_from(&via_trait).unwrap();
        assert_eq!(back_via_function, batch);
        assert_eq!(back_via_trait, batch);
    }
}
