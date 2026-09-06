//! [`RecordBatch`] — a schema plus a matching set of equal-length columns.
//!
//! A record batch is the unit an AstRS message carries: blueprint §6.1 fixes
//! **one record batch per message**, and by convention a single top-level
//! column named [`DATA_COLUMN`](crate::DATA_COLUMN). The type still supports
//! arbitrary column sets, because the recorder, the ROS 2 bridge and the CLI
//! all build multi-column batches internally before projecting down to the
//! payload shape.
//!
//! ```
//! use astrs_data::builder::{ArrayBuilder, Float32Builder, Int64Builder};
//! use astrs_data::{DataType, Field, RecordBatch, Schema};
//! use std::sync::Arc;
//!
//! let mut stamps = Int64Builder::with_capacity(2);
//! stamps.append_value(1);
//! stamps.append_value(2);
//!
//! let mut ranges = Float32Builder::with_capacity(2);
//! ranges.append_value(0.5);
//! ranges.append_null();
//!
//! let schema = Arc::new(Schema::new(vec![
//!     Field::required("stamp", DataType::Int64),
//!     Field::nullable("range", DataType::Float32),
//! ]));
//! let batch = RecordBatch::try_new(schema, vec![stamps.finish_array(), ranges.finish_array()])?;
//!
//! assert_eq!(batch.num_rows(), 2);
//! assert_eq!(batch.num_columns(), 2);
//! assert!(batch.column_by_name("range").is_some());
//! # Ok::<(), astrs_data::DataError>(())
//! ```
//!
//! # What `try_new` checks
//!
//! | Check | Error |
//! |---|---|
//! | column count equals field count | [`DataError::ColumnCountMismatch`] |
//! | every column has the batch's row count | [`DataError::ColumnLengthMismatch`] |
//! | every column's type matches its field | [`DataError::TypeMismatch`] |
//! | a non-nullable field's column holds no nulls | [`DataError::NullsInNonNullableColumn`] |
//!
//! The last two are configurable through [`RecordBatchOptions`], because a
//! decoder reading a foreign Arrow IPC stream (stage 2) needs the *layout*
//! comparison rather than the exact one: producers disagree about the child
//! field name inside a `List` (`item` vs `element` vs `entries`) while
//! encoding byte-identical buffers.
//!
//! # Zero columns
//!
//! A schema with no fields cannot imply a row count, yet a zero-column batch
//! with a non-zero row count is representable in Arrow IPC and appears in the
//! golden vectors. Use [`RecordBatch::try_new_with_row_count`] for that case;
//! [`RecordBatch::try_new`] infers `0`.

use std::fmt;
use std::sync::Arc;

use crate::array::{Array, ArrayRef, new_empty_array};
use crate::datatype::{DataType, Field, Schema};
use crate::error::{DataError, Result};

/// How [`RecordBatch::try_new_with_options`] compares a column's type against
/// the field that declares it.
///
/// ```
/// use astrs_data::record_batch::ColumnTypeCheck;
///
/// assert_eq!(ColumnTypeCheck::default(), ColumnTypeCheck::Exact);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum ColumnTypeCheck {
    /// Full [`DataType`] equality, including nested child field names and
    /// nullability. This is the default: within AstRS both sides build their
    /// types from the same [`crate::urn`] registry, so any difference is a
    /// bug worth surfacing.
    #[default]
    Exact,
    /// [`DataType::layout_eq`] — the buffers must agree, the labels need not.
    /// Used when adopting a schema decoded from a foreign producer.
    Layout,
    /// No type check at all. Lengths and nullability are still verified.
    None,
}

impl ColumnTypeCheck {
    /// Applies the check to one pair of types.
    ///
    /// ```
    /// use astrs_data::record_batch::ColumnTypeCheck;
    /// use astrs_data::{DataType, Field};
    ///
    /// let declared = DataType::list(Field::new("item", DataType::Int32, true));
    /// let actual = DataType::list(Field::new("element", DataType::Int32, false));
    ///
    /// assert!(!ColumnTypeCheck::Exact.accepts(&declared, &actual));
    /// assert!(ColumnTypeCheck::Layout.accepts(&declared, &actual));
    /// assert!(ColumnTypeCheck::None.accepts(&declared, &DataType::Utf8));
    /// ```
    #[must_use]
    pub fn accepts(self, declared: &DataType, actual: &DataType) -> bool {
        match self {
            Self::Exact => declared == actual,
            Self::Layout => declared.layout_eq(actual),
            Self::None => true,
        }
    }
}

/// The knobs [`RecordBatch::try_new_with_options`] exposes.
///
/// The [`Default`] is the strict setting used by [`RecordBatch::try_new`]:
/// exact types, nullability enforced, row count inferred.
///
/// ```
/// use astrs_data::record_batch::{ColumnTypeCheck, RecordBatchOptions};
///
/// let options = RecordBatchOptions::default()
///     .with_row_count(4)
///     .with_column_types(ColumnTypeCheck::Layout);
/// assert_eq!(options.row_count, Some(4));
/// assert!(options.check_nullability);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct RecordBatchOptions {
    /// The batch's row count. `None` infers it from the first column, or `0`
    /// when there are no columns.
    pub row_count: Option<usize>,
    /// How a column's type is compared against its field.
    pub column_types: ColumnTypeCheck,
    /// Whether a non-nullable field's column is rejected when it holds nulls.
    pub check_nullability: bool,
}

impl Default for RecordBatchOptions {
    #[inline]
    fn default() -> Self {
        Self {
            row_count: None,
            column_types: ColumnTypeCheck::Exact,
            check_nullability: true,
        }
    }
}

impl RecordBatchOptions {
    /// The strict default, spelled out.
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fixes the row count instead of inferring it.
    #[inline]
    #[must_use]
    pub const fn with_row_count(mut self, rows: usize) -> Self {
        self.row_count = Some(rows);
        self
    }

    /// Selects the column type comparison.
    #[inline]
    #[must_use]
    pub const fn with_column_types(mut self, check: ColumnTypeCheck) -> Self {
        self.column_types = check;
        self
    }

    /// Enables or disables the non-nullable-column check.
    #[inline]
    #[must_use]
    pub const fn with_nullability_check(mut self, check: bool) -> Self {
        self.check_nullability = check;
        self
    }
}

/// A schema and one equal-length column per field.
///
/// Cloning a batch clones an `Arc` per column plus the column vector — no
/// buffer bytes move. [`RecordBatch::slice`] is likewise zero-copy: it
/// re-windows every column against the same allocations.
#[derive(Clone)]
pub struct RecordBatch {
    schema: Arc<Schema>,
    columns: Vec<ArrayRef>,
    len: usize,
}

impl RecordBatch {
    /// Builds a batch, running every check in the table above.
    ///
    /// The row count is taken from the first column; with no columns it is
    /// `0`. Use [`RecordBatch::try_new_with_row_count`] to state it.
    ///
    /// # Errors
    ///
    /// [`DataError::ColumnCountMismatch`], [`DataError::ColumnLengthMismatch`],
    /// [`DataError::TypeMismatch`] or [`DataError::NullsInNonNullableColumn`].
    ///
    /// ```
    /// use astrs_data::array::{Int32Array, IntoArrayRef};
    /// use astrs_data::{DataError, DataType, Field, RecordBatch, Schema};
    /// use std::sync::Arc;
    ///
    /// let schema = Arc::new(Schema::new(vec![Field::required("a", DataType::Int32)]));
    /// let column = Int32Array::from_values([1, 2, 3]).into_array_ref();
    /// let batch = RecordBatch::try_new(Arc::clone(&schema), vec![column])?;
    /// assert_eq!(batch.num_rows(), 3);
    ///
    /// let err = RecordBatch::try_new(schema, Vec::new()).unwrap_err();
    /// assert_eq!(err, DataError::ColumnCountMismatch { fields: 1, columns: 0 });
    /// # Ok::<(), astrs_data::DataError>(())
    /// ```
    pub fn try_new(schema: Arc<Schema>, columns: Vec<ArrayRef>) -> Result<Self> {
        Self::try_new_with_options(schema, columns, RecordBatchOptions::default())
    }

    /// Builds a batch with an explicit row count.
    ///
    /// The only way to express a zero-column batch that still has rows — the
    /// shape an Arrow IPC stream produces for a schema with no fields and a
    /// non-zero `length` in its `RecordBatch` header.
    ///
    /// # Errors
    ///
    /// As [`RecordBatch::try_new`], plus [`DataError::ColumnLengthMismatch`]
    /// when a column disagrees with the stated count.
    ///
    /// ```
    /// use astrs_data::{RecordBatch, Schema};
    /// use std::sync::Arc;
    ///
    /// let batch = RecordBatch::try_new_with_row_count(
    ///     Arc::new(Schema::default()),
    ///     Vec::new(),
    ///     16,
    /// )?;
    /// assert_eq!(batch.num_rows(), 16);
    /// assert_eq!(batch.num_columns(), 0);
    /// # Ok::<(), astrs_data::DataError>(())
    /// ```
    pub fn try_new_with_row_count(
        schema: Arc<Schema>,
        columns: Vec<ArrayRef>,
        rows: usize,
    ) -> Result<Self> {
        Self::try_new_with_options(
            schema,
            columns,
            RecordBatchOptions::default().with_row_count(rows),
        )
    }

    /// The general constructor every other one delegates to.
    ///
    /// # Errors
    ///
    /// Whichever of the checks enabled by `options` fails first, reported in
    /// column order so the diagnostic names the earliest offender.
    ///
    /// ```
    /// use astrs_data::array::{Int64Array, IntoArrayRef};
    /// use astrs_data::record_batch::{ColumnTypeCheck, RecordBatchOptions};
    /// use astrs_data::{DataType, Field, RecordBatch, Schema};
    /// use std::sync::Arc;
    ///
    /// // A column with nulls under a field the schema calls non-nullable is
    /// // accepted only when the check is switched off.
    /// let schema = Arc::new(Schema::new(vec![Field::required("t", DataType::Int64)]));
    /// let column = Int64Array::from_opt_iter([Some(1), None]).into_array_ref();
    ///
    /// assert!(RecordBatch::try_new(Arc::clone(&schema), vec![column.clone()]).is_err());
    /// let relaxed = RecordBatchOptions::default()
    ///     .with_nullability_check(false)
    ///     .with_column_types(ColumnTypeCheck::Layout);
    /// assert!(RecordBatch::try_new_with_options(schema, vec![column], relaxed).is_ok());
    /// ```
    pub fn try_new_with_options(
        schema: Arc<Schema>,
        columns: Vec<ArrayRef>,
        options: RecordBatchOptions,
    ) -> Result<Self> {
        if schema.len() != columns.len() {
            return Err(DataError::ColumnCountMismatch {
                fields: schema.len(),
                columns: columns.len(),
            });
        }

        let len = match options.row_count {
            Some(rows) => rows,
            None => columns.first().map_or(0, |column| column.len()),
        };

        for (index, column) in columns.iter().enumerate() {
            // `schema.len() == columns.len()` was just established, so the
            // field always exists; the `else` arm cannot be reached and is
            // written as a real error rather than an unwrap.
            let Some(field) = schema.field(index) else {
                return Err(DataError::ColumnCountMismatch {
                    fields: schema.len(),
                    columns: columns.len(),
                });
            };
            Self::check_column(index, field, column.as_ref(), len, options)?;
        }

        Ok(Self {
            schema,
            columns,
            len,
        })
    }

    /// Validates one column against one field.
    fn check_column(
        index: usize,
        field: &Field,
        column: &dyn Array,
        len: usize,
        options: RecordBatchOptions,
    ) -> Result<()> {
        if column.len() != len {
            return Err(DataError::ColumnLengthMismatch {
                index,
                name: field.name().to_owned(),
                expected: len,
                actual: column.len(),
            });
        }
        if !options
            .column_types
            .accepts(field.data_type(), column.data_type())
        {
            return Err(DataError::type_mismatch(
                field.data_type().clone(),
                column.data_type().clone(),
            ));
        }
        if options.check_nullability && !field.is_nullable() {
            let nulls = column.null_count();
            if nulls > 0 {
                return Err(DataError::NullsInNonNullableColumn {
                    index,
                    name: field.name().to_owned(),
                    null_count: nulls,
                });
            }
        }
        Ok(())
    }

    /// The canonical AstRS payload shape: one column named
    /// [`DATA_COLUMN`](crate::DATA_COLUMN).
    ///
    /// The schema is derived from the column, so this cannot fail: the field
    /// is marked nullable exactly when the column carries nulls.
    ///
    /// ```
    /// use astrs_data::array::{Array, Float32Array, IntoArrayRef};
    /// use astrs_data::{DATA_COLUMN, RecordBatch};
    ///
    /// let batch = RecordBatch::from_payload(Float32Array::from_values([1.0, 2.0]).into_array_ref());
    /// assert_eq!(batch.num_rows(), 2);
    /// assert_eq!(batch.schema().field(0).map(|f| f.name()), Some(DATA_COLUMN));
    /// assert!(batch.payload_column().is_some());
    /// ```
    #[must_use]
    pub fn from_payload(column: ArrayRef) -> Self {
        let len = column.len();
        let schema = Arc::new(Schema::payload(
            column.data_type().clone(),
            column.null_count() > 0,
        ));
        Self {
            schema,
            columns: vec![column],
            len,
        }
    }

    /// An empty batch matching `schema`, with one empty column per field.
    ///
    /// # Errors
    ///
    /// [`DataError::InvalidFixedSize`] when a field carries a non-positive
    /// fixed width, which is the only way an empty column cannot be built.
    ///
    /// ```
    /// use astrs_data::{DataType, Field, RecordBatch, Schema};
    /// use std::sync::Arc;
    ///
    /// let schema = Arc::new(Schema::new(vec![Field::nullable("a", DataType::Utf8)]));
    /// let batch = RecordBatch::try_new_empty(schema)?;
    /// assert_eq!(batch.num_rows(), 0);
    /// assert_eq!(batch.num_columns(), 1);
    /// # Ok::<(), astrs_data::DataError>(())
    /// ```
    pub fn try_new_empty(schema: Arc<Schema>) -> Result<Self> {
        let mut columns = Vec::with_capacity(schema.len());
        for field in schema.fields() {
            columns.push(new_empty_array(field.data_type())?);
        }
        Ok(Self {
            schema,
            columns,
            len: 0,
        })
    }

    /// The schema, shared.
    #[inline]
    #[must_use]
    pub fn schema(&self) -> &Arc<Schema> {
        &self.schema
    }

    /// A cheap clone of the shared schema handle.
    #[inline]
    #[must_use]
    pub fn schema_ref(&self) -> Arc<Schema> {
        Arc::clone(&self.schema)
    }

    /// Every column, in field order.
    #[inline]
    #[must_use]
    pub fn columns(&self) -> &[ArrayRef] {
        &self.columns
    }

    /// Number of rows.
    #[inline]
    #[must_use]
    pub const fn num_rows(&self) -> usize {
        self.len
    }

    /// Number of columns.
    #[inline]
    #[must_use]
    pub fn num_columns(&self) -> usize {
        self.columns.len()
    }

    /// Returns `true` when the batch carries no rows.
    ///
    /// A batch can be empty while still having columns — the schema survives.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The column at `index`, or `None`.
    #[inline]
    #[must_use]
    pub fn column(&self, index: usize) -> Option<&ArrayRef> {
        self.columns.get(index)
    }

    /// The column at `index`.
    ///
    /// # Errors
    ///
    /// [`DataError::IndexOutOfBounds`] when there is no such column.
    pub fn try_column(&self, index: usize) -> Result<&ArrayRef> {
        self.columns.get(index).ok_or(DataError::IndexOutOfBounds {
            index,
            len: self.columns.len(),
        })
    }

    /// The first column whose field is named `name`, or `None`.
    ///
    /// [`Schema::new`] does not reject duplicate names (only
    /// [`Schema::try_new`] does), so this deliberately resolves to the
    /// **first** match rather than reporting ambiguity.
    #[must_use]
    pub fn column_by_name(&self, name: &str) -> Option<&ArrayRef> {
        self.schema
            .index_of(name)
            .and_then(|index| self.columns.get(index))
    }

    /// The first column whose field is named `name`.
    ///
    /// # Errors
    ///
    /// [`DataError::FieldNotFound`] when the schema has no such field.
    pub fn try_column_by_name(&self, name: &str) -> Result<&ArrayRef> {
        let index = self.schema.try_index_of(name)?;
        self.try_column(index)
    }

    /// The single [`DATA_COLUMN`](crate::DATA_COLUMN) column, when present.
    #[inline]
    #[must_use]
    pub fn payload_column(&self) -> Option<&ArrayRef> {
        self.column_by_name(crate::DATA_COLUMN)
    }

    /// The field / column pairs, in order.
    ///
    /// ```
    /// use astrs_data::array::{Array, Int32Array, IntoArrayRef};
    /// use astrs_data::{DataType, Field, RecordBatch, Schema};
    /// use std::sync::Arc;
    ///
    /// let schema = Arc::new(Schema::new(vec![Field::required("a", DataType::Int32)]));
    /// let batch = RecordBatch::try_new(schema, vec![Int32Array::from_values([1]).into_array_ref()])?;
    /// for (field, column) in batch.iter() {
    ///     assert_eq!(field.name(), "a");
    ///     assert_eq!(column.len(), 1);
    /// }
    /// # Ok::<(), astrs_data::DataError>(())
    /// ```
    pub fn iter(&self) -> impl Iterator<Item = (&Field, &ArrayRef)> {
        self.schema.fields().iter().zip(self.columns.iter())
    }

    /// Total buffer memory the batch pins, in bytes.
    ///
    /// Shared allocations are counted once per column that references them,
    /// so this is an upper bound on the resident set, not an exact figure for
    /// a batch whose columns are slices of one buffer.
    #[must_use]
    pub fn buffer_memory_size(&self) -> usize {
        self.columns
            .iter()
            .map(|column| column.buffer_memory_size())
            .sum()
    }

    /// A zero-copy row window, clamped to the available range.
    ///
    /// Follows the crate-wide clamping convention (see
    /// [`crate::array::Array::slice`]): an out-of-range request yields the
    /// largest valid window instead of panicking.
    ///
    /// ```
    /// use astrs_data::array::{Int32Array, IntoArrayRef};
    /// use astrs_data::{DataType, Field, RecordBatch, Schema};
    /// use std::sync::Arc;
    ///
    /// let schema = Arc::new(Schema::new(vec![Field::required("a", DataType::Int32)]));
    /// let column = Int32Array::from_values([1, 2, 3, 4]).into_array_ref();
    /// let batch = RecordBatch::try_new(schema, vec![column])?;
    ///
    /// assert_eq!(batch.slice(1, 2).num_rows(), 2);
    /// assert_eq!(batch.slice(3, 99).num_rows(), 1);
    /// assert_eq!(batch.slice(99, 1).num_rows(), 0);
    /// # Ok::<(), astrs_data::DataError>(())
    /// ```
    #[must_use]
    pub fn slice(&self, offset: usize, len: usize) -> Self {
        let offset = offset.min(self.len);
        let len = len.min(self.len - offset);
        Self {
            schema: Arc::clone(&self.schema),
            columns: self
                .columns
                .iter()
                .map(|column| column.slice(offset, len))
                .collect(),
            len,
        }
    }

    /// A checked row window.
    ///
    /// # Errors
    ///
    /// [`DataError::SliceOutOfBounds`] when the window leaves the batch.
    pub fn try_slice(&self, offset: usize, len: usize) -> Result<Self> {
        let end = offset.checked_add(len).ok_or(DataError::SliceOutOfBounds {
            offset,
            len,
            available: self.len,
        })?;
        if end > self.len {
            return Err(DataError::SliceOutOfBounds {
                offset,
                len,
                available: self.len,
            });
        }
        Ok(self.slice(offset, len))
    }

    /// A batch holding only the columns at `indices`, in the order given.
    ///
    /// # Errors
    ///
    /// [`DataError::IndexOutOfBounds`] when an index has no column.
    ///
    /// ```
    /// use astrs_data::array::{Int32Array, IntoArrayRef};
    /// use astrs_data::{DataType, Field, RecordBatch, Schema};
    /// use std::sync::Arc;
    ///
    /// let schema = Arc::new(Schema::new(vec![
    ///     Field::required("a", DataType::Int32),
    ///     Field::required("b", DataType::Int32),
    /// ]));
    /// let batch = RecordBatch::try_new(
    ///     schema,
    ///     vec![
    ///         Int32Array::from_values([1]).into_array_ref(),
    ///         Int32Array::from_values([2]).into_array_ref(),
    ///     ],
    /// )?;
    ///
    /// let projected = batch.project(&[1])?;
    /// assert_eq!(projected.num_columns(), 1);
    /// assert_eq!(projected.schema().field(0).map(|f| f.name()), Some("b"));
    /// # Ok::<(), astrs_data::DataError>(())
    /// ```
    pub fn project(&self, indices: &[usize]) -> Result<Self> {
        let schema = Arc::new(self.schema.project(indices)?);
        let mut columns = Vec::with_capacity(indices.len());
        for &index in indices {
            columns.push(Arc::clone(self.try_column(index)?));
        }
        Ok(Self {
            schema,
            columns,
            len: self.len,
        })
    }

    /// A batch holding only the named columns, in the order given.
    ///
    /// # Errors
    ///
    /// [`DataError::FieldNotFound`] when a name has no field.
    pub fn project_by_name(&self, names: &[&str]) -> Result<Self> {
        let mut indices = Vec::with_capacity(names.len());
        for name in names {
            indices.push(self.schema.try_index_of(name)?);
        }
        self.project(&indices)
    }

    /// Replaces the schema, re-validating the columns against it.
    ///
    /// The row count is preserved, so this is how a decoder adopts the schema
    /// it read off the wire for columns it built from raw buffers.
    ///
    /// # Errors
    ///
    /// Whatever [`RecordBatch::try_new_with_options`] reports.
    ///
    /// ```
    /// use astrs_data::array::{Int32Array, IntoArrayRef};
    /// use astrs_data::record_batch::{ColumnTypeCheck, RecordBatchOptions};
    /// use astrs_data::{DataType, Field, RecordBatch, Schema};
    /// use std::sync::Arc;
    ///
    /// let schema = Arc::new(Schema::new(vec![Field::required("a", DataType::Int32)]));
    /// let batch = RecordBatch::try_new(schema, vec![Int32Array::from_values([1]).into_array_ref()])?;
    ///
    /// let renamed = Arc::new(Schema::new(vec![Field::required("alpha", DataType::Int32)]));
    /// let batch = batch.try_with_schema(renamed, RecordBatchOptions::default())?;
    /// assert!(batch.column_by_name("alpha").is_some());
    ///
    /// let wrong = Arc::new(Schema::new(vec![Field::required("a", DataType::Utf8)]));
    /// assert!(batch.clone().try_with_schema(wrong, RecordBatchOptions::default()).is_err());
    /// # Ok::<(), astrs_data::DataError>(())
    /// ```
    pub fn try_with_schema(self, schema: Arc<Schema>, options: RecordBatchOptions) -> Result<Self> {
        let options = match options.row_count {
            Some(_) => options,
            None => options.with_row_count(self.len),
        };
        Self::try_new_with_options(schema, self.columns, options)
    }

    /// A copy whose schema carries one extra metadata entry.
    ///
    /// Stage 2 uses this to stamp the schema hash before encoding.
    ///
    /// ```
    /// use astrs_data::array::{Int32Array, IntoArrayRef};
    /// use astrs_data::RecordBatch;
    ///
    /// let batch = RecordBatch::from_payload(Int32Array::from_values([1]).into_array_ref())
    ///     .with_metadata_entry("producer", "lidar");
    /// assert_eq!(batch.schema().metadata_value("producer"), Some("lidar"));
    /// ```
    #[must_use]
    pub fn with_metadata_entry(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let schema = Arc::unwrap_or_clone(self.schema).with_metadata_entry(key, value);
        self.schema = Arc::new(schema);
        self
    }

    /// Returns `true` when the two batches describe the same buffer layout,
    /// ignoring field names, nullability and schema metadata.
    ///
    /// The comparison the graph type system applies to an edge.
    #[must_use]
    pub fn layout_eq(&self, other: &Self) -> bool {
        self.len == other.len && self.schema.layout_eq(&other.schema)
    }
}

impl PartialEq for RecordBatch {
    /// Schema equality (including metadata) plus column-wise value equality.
    ///
    /// Sliced columns compare by their logical values, so a batch built
    /// directly equals the same rows carved out of a larger one.
    fn eq(&self, other: &Self) -> bool {
        self.len == other.len
            && self.schema == other.schema
            && self.columns.len() == other.columns.len()
            && self
                .columns
                .iter()
                .zip(other.columns.iter())
                .all(|(a, b)| a.as_ref() == b.as_ref())
    }
}

impl fmt::Debug for RecordBatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecordBatch")
            .field("rows", &self.len)
            .field("schema", &self.schema)
            .field("columns", &self.columns)
            .finish()
    }
}

impl<'a> IntoIterator for &'a RecordBatch {
    type Item = (&'a Field, &'a ArrayRef);
    type IntoIter = std::iter::Zip<std::slice::Iter<'a, Field>, std::slice::Iter<'a, ArrayRef>>;

    fn into_iter(self) -> Self::IntoIter {
        self.schema.fields().iter().zip(self.columns.iter())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::array::{
        BooleanArray, Int32Array, Int64Array, IntoArrayRef, NullArray, StringArray,
    };

    fn int_schema(nullable: bool) -> Arc<Schema> {
        Arc::new(Schema::new(vec![Field::new(
            "a",
            DataType::Int32,
            nullable,
        )]))
    }

    #[test]
    fn try_new_accepts_matching_columns() {
        let batch = RecordBatch::try_new(
            int_schema(false),
            vec![Int32Array::from_values([1, 2, 3]).into_array_ref()],
        )
        .unwrap();
        assert_eq!(batch.num_rows(), 3);
        assert_eq!(batch.num_columns(), 1);
        assert!(!batch.is_empty());
    }

    #[test]
    fn column_count_is_checked() {
        let err = RecordBatch::try_new(int_schema(false), Vec::new()).unwrap_err();
        assert_eq!(
            err,
            DataError::ColumnCountMismatch {
                fields: 1,
                columns: 0
            }
        );
    }

    #[test]
    fn column_length_is_checked() {
        let schema = Arc::new(Schema::new(vec![
            Field::required("a", DataType::Int32),
            Field::required("b", DataType::Int32),
        ]));
        let err = RecordBatch::try_new(
            schema,
            vec![
                Int32Array::from_values([1, 2]).into_array_ref(),
                Int32Array::from_values([1]).into_array_ref(),
            ],
        )
        .unwrap_err();
        assert_eq!(
            err,
            DataError::ColumnLengthMismatch {
                index: 1,
                name: "b".to_owned(),
                expected: 2,
                actual: 1,
            }
        );
    }

    #[test]
    fn column_type_is_checked() {
        let err = RecordBatch::try_new(
            int_schema(true),
            vec![StringArray::from_values(["x"]).into_array_ref()],
        )
        .unwrap_err();
        assert_eq!(
            err,
            DataError::type_mismatch(DataType::Int32, DataType::Utf8)
        );
    }

    #[test]
    fn nulls_in_non_nullable_columns_are_rejected() {
        let column = Int32Array::from_opt_iter([Some(1), None]).into_array_ref();
        let err = RecordBatch::try_new(int_schema(false), vec![column.clone()]).unwrap_err();
        assert_eq!(
            err,
            DataError::NullsInNonNullableColumn {
                index: 0,
                name: "a".to_owned(),
                null_count: 1,
            }
        );

        // The same batch is fine once the field is nullable.
        assert!(RecordBatch::try_new(int_schema(true), vec![column]).is_ok());
    }

    #[test]
    fn nullability_check_can_be_disabled() {
        let column = Int32Array::from_opt_iter([Some(1), None]).into_array_ref();
        let options = RecordBatchOptions::default().with_nullability_check(false);
        assert!(
            RecordBatch::try_new_with_options(int_schema(false), vec![column], options).is_ok()
        );
    }

    #[test]
    fn layout_type_check_tolerates_child_field_names() {
        let declared = DataType::list(Field::new("item", DataType::Int32, true));
        let actual = DataType::list(Field::new("element", DataType::Int32, true));
        let schema = Arc::new(Schema::new(vec![Field::nullable("l", declared)]));
        let column = crate::array::ListArray::try_new(
            Field::new("element", DataType::Int32, true),
            crate::buffer::ScalarBuffer::<i32>::from_slice(&[0i32, 1]),
            Int32Array::from_values([7]).into_array_ref(),
            None,
        )
        .unwrap();
        assert_eq!(column.data_type(), &actual);
        let column = column.into_array_ref();

        assert!(RecordBatch::try_new(Arc::clone(&schema), vec![Arc::clone(&column)]).is_err());
        let options = RecordBatchOptions::default().with_column_types(ColumnTypeCheck::Layout);
        assert!(RecordBatch::try_new_with_options(schema, vec![column], options).is_ok());
    }

    #[test]
    fn zero_columns_can_still_have_rows() {
        let batch =
            RecordBatch::try_new_with_row_count(Arc::new(Schema::default()), Vec::new(), 12)
                .unwrap();
        assert_eq!(batch.num_rows(), 12);
        assert_eq!(batch.num_columns(), 0);
        assert!(!batch.is_empty());
        assert_eq!(batch.slice(4, 100).num_rows(), 8);
    }

    #[test]
    fn zero_columns_infer_zero_rows() {
        let batch = RecordBatch::try_new(Arc::new(Schema::default()), Vec::new()).unwrap();
        assert_eq!(batch.num_rows(), 0);
        assert!(batch.is_empty());
    }

    #[test]
    fn explicit_row_count_is_enforced_against_columns() {
        let err = RecordBatch::try_new_with_row_count(
            int_schema(false),
            vec![Int32Array::from_values([1, 2]).into_array_ref()],
            3,
        )
        .unwrap_err();
        assert!(matches!(err, DataError::ColumnLengthMismatch { .. }));
    }

    #[test]
    fn payload_batch_uses_the_data_column() {
        let batch = RecordBatch::from_payload(Int64Array::from_values([1, 2]).into_array_ref());
        assert_eq!(
            batch.schema().field(0).map(Field::name),
            Some(crate::DATA_COLUMN)
        );
        assert!(batch.payload_column().is_some());
        assert!(!batch.schema().fields()[0].is_nullable());

        let nullable =
            RecordBatch::from_payload(Int64Array::from_opt_iter([None, Some(1)]).into_array_ref());
        assert!(nullable.schema().fields()[0].is_nullable());
    }

    #[test]
    fn empty_batch_matches_its_schema() {
        let schema = Arc::new(Schema::new(vec![
            Field::nullable("s", DataType::Utf8),
            Field::nullable("b", DataType::Bool),
        ]));
        let batch = RecordBatch::try_new_empty(Arc::clone(&schema)).unwrap();
        assert_eq!(batch.num_rows(), 0);
        assert_eq!(batch.num_columns(), 2);
        assert_eq!(batch.schema(), &schema);
    }

    #[test]
    fn slicing_clamps_and_stays_zero_copy() {
        let schema = Arc::new(Schema::new(vec![
            Field::required("a", DataType::Int32),
            Field::nullable("s", DataType::Utf8),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Int32Array::from_values([1, 2, 3, 4]).into_array_ref(),
                StringArray::from_opt_iter([Some("a"), None, Some("c"), Some("d")])
                    .into_array_ref(),
            ],
        )
        .unwrap();

        let window = batch.slice(1, 2);
        assert_eq!(window.num_rows(), 2);
        assert_eq!(window.columns()[0].len(), 2);
        assert_eq!(window.columns()[1].null_count(), 1);

        assert_eq!(batch.slice(4, 4).num_rows(), 0);
        assert_eq!(batch.slice(99, 4).num_rows(), 0);
        assert_eq!(batch.slice(2, 99).num_rows(), 2);
    }

    #[test]
    fn try_slice_reports_out_of_range() {
        let batch = RecordBatch::from_payload(Int32Array::from_values([1, 2, 3]).into_array_ref());
        assert_eq!(batch.try_slice(1, 2).unwrap().num_rows(), 2);
        assert_eq!(
            batch.try_slice(2, 2).unwrap_err(),
            DataError::SliceOutOfBounds {
                offset: 2,
                len: 2,
                available: 3
            }
        );
        assert!(batch.try_slice(1, usize::MAX).is_err());
    }

    #[test]
    fn projection_reorders_and_drops() {
        let schema = Arc::new(Schema::new(vec![
            Field::required("a", DataType::Int32),
            Field::required("b", DataType::Bool),
            Field::nullable("c", DataType::Utf8),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Int32Array::from_values([1]).into_array_ref(),
                BooleanArray::from_values([true]).into_array_ref(),
                StringArray::from_values(["z"]).into_array_ref(),
            ],
        )
        .unwrap();

        let projected = batch.project(&[2, 0]).unwrap();
        assert_eq!(projected.num_columns(), 2);
        assert_eq!(projected.schema().field(0).map(Field::name), Some("c"));
        assert_eq!(projected.num_rows(), 1);

        assert!(batch.project(&[9]).is_err());

        let by_name = batch.project_by_name(&["b"]).unwrap();
        assert_eq!(by_name.schema().field(0).map(Field::name), Some("b"));
        assert!(batch.project_by_name(&["missing"]).is_err());
    }

    #[test]
    fn projecting_nothing_keeps_the_row_count() {
        let batch = RecordBatch::from_payload(Int32Array::from_values([1, 2, 3]).into_array_ref());
        let projected = batch.project(&[]).unwrap();
        assert_eq!(projected.num_columns(), 0);
        assert_eq!(projected.num_rows(), 3);
    }

    #[test]
    fn column_lookup_by_name_and_index() {
        let batch = RecordBatch::from_payload(Int32Array::from_values([1]).into_array_ref());
        assert!(batch.column(0).is_some());
        assert!(batch.column(1).is_none());
        assert!(batch.try_column(1).is_err());
        assert!(batch.column_by_name(crate::DATA_COLUMN).is_some());
        assert!(batch.column_by_name("nope").is_none());
        assert_eq!(
            batch.try_column_by_name("nope").unwrap_err(),
            DataError::FieldNotFound {
                name: "nope".to_owned()
            }
        );
    }

    #[test]
    fn duplicate_names_resolve_to_the_first_column() {
        let schema = Arc::new(Schema::new(vec![
            Field::required("dup", DataType::Int32),
            Field::required("dup", DataType::Int64),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Int32Array::from_values([1]).into_array_ref(),
                Int64Array::from_values([2]).into_array_ref(),
            ],
        )
        .unwrap();
        assert_eq!(
            batch.column_by_name("dup").map(|c| c.data_type().clone()),
            Some(DataType::Int32)
        );
    }

    #[test]
    fn schema_can_be_replaced_when_compatible() {
        let batch = RecordBatch::from_payload(Int32Array::from_values([1, 2]).into_array_ref());
        let renamed = Arc::new(Schema::new(vec![Field::required("x", DataType::Int32)]));
        let batch = batch
            .try_with_schema(renamed, RecordBatchOptions::default())
            .unwrap();
        assert!(batch.column_by_name("x").is_some());

        let wrong = Arc::new(Schema::new(vec![Field::required("x", DataType::Int64)]));
        assert!(
            batch
                .try_with_schema(wrong, RecordBatchOptions::default())
                .is_err()
        );
    }

    #[test]
    fn replacing_a_schema_on_a_zero_column_batch_keeps_the_row_count() {
        let batch = RecordBatch::try_new_with_row_count(Arc::new(Schema::default()), Vec::new(), 5)
            .unwrap();
        let batch = batch
            .try_with_schema(Arc::new(Schema::default()), RecordBatchOptions::default())
            .unwrap();
        assert_eq!(batch.num_rows(), 5);
    }

    #[test]
    fn metadata_entries_are_attached_to_the_schema() {
        let batch = RecordBatch::from_payload(Int32Array::from_values([1]).into_array_ref())
            .with_metadata_entry("_schema_hash", "0x1234")
            .with_metadata_entry("producer", "camera");
        assert_eq!(
            batch.schema().metadata_value("_schema_hash"),
            Some("0x1234")
        );
        assert_eq!(batch.schema().metadata_value("producer"), Some("camera"));
    }

    #[test]
    fn equality_compares_values_not_provenance() {
        let long =
            RecordBatch::from_payload(Int32Array::from_values([9, 1, 2, 9]).into_array_ref());
        let window = long.slice(1, 2);
        let direct = RecordBatch::from_payload(Int32Array::from_values([1, 2]).into_array_ref());
        assert_eq!(window, direct);

        let other = RecordBatch::from_payload(Int32Array::from_values([1, 3]).into_array_ref());
        assert_ne!(window, other);
    }

    #[test]
    fn equality_distinguishes_schema_metadata() {
        let a = RecordBatch::from_payload(Int32Array::from_values([1]).into_array_ref());
        let b = a.clone().with_metadata_entry("k", "v");
        assert_ne!(a, b);
        assert!(a.layout_eq(&b));
    }

    #[test]
    fn layout_equality_ignores_labels() {
        let a = RecordBatch::from_payload(Int32Array::from_values([1, 2]).into_array_ref());
        let renamed = Arc::new(Schema::new(vec![Field::nullable("other", DataType::Int32)]));
        let b = a
            .clone()
            .try_with_schema(renamed, RecordBatchOptions::default())
            .unwrap();
        assert!(a.layout_eq(&b));
        assert_ne!(a, b);

        let shorter = a.slice(0, 1);
        assert!(!a.layout_eq(&shorter));
    }

    #[test]
    fn iteration_pairs_fields_with_columns() {
        let schema = Arc::new(Schema::new(vec![
            Field::required("a", DataType::Int32),
            Field::required("b", DataType::Bool),
        ]));
        let batch = RecordBatch::try_new(
            schema,
            vec![
                Int32Array::from_values([1]).into_array_ref(),
                BooleanArray::from_values([false]).into_array_ref(),
            ],
        )
        .unwrap();

        let names: Vec<_> = batch.iter().map(|(field, _)| field.name()).collect();
        assert_eq!(names, vec!["a", "b"]);

        let types: Vec<_> = (&batch)
            .into_iter()
            .map(|(_, column)| column.data_type().clone())
            .collect();
        assert_eq!(types, vec![DataType::Int32, DataType::Bool]);
    }

    #[test]
    fn null_columns_are_ordinary_columns() {
        let schema = Arc::new(Schema::new(vec![Field::nullable("n", DataType::Null)]));
        let batch = RecordBatch::try_new(schema, vec![NullArray::new(4).into_array_ref()]).unwrap();
        assert_eq!(batch.num_rows(), 4);
        assert_eq!(batch.columns()[0].null_count(), 4);
        assert_eq!(batch.slice(1, 2).num_rows(), 2);
    }

    #[test]
    fn null_column_under_a_required_field_is_rejected() {
        let schema = Arc::new(Schema::new(vec![Field::required("n", DataType::Null)]));
        assert!(RecordBatch::try_new(schema, vec![NullArray::new(1).into_array_ref()]).is_err());
    }

    #[test]
    fn buffer_memory_is_reported() {
        let batch =
            RecordBatch::from_payload(Int32Array::from_values([1, 2, 3, 4]).into_array_ref());
        assert!(batch.buffer_memory_size() >= 16);
        let empty = RecordBatch::try_new(Arc::new(Schema::default()), Vec::new()).unwrap();
        assert_eq!(empty.buffer_memory_size(), 0);
    }

    #[test]
    fn debug_names_the_row_count() {
        let batch = RecordBatch::from_payload(Int32Array::from_values([1]).into_array_ref());
        let text = format!("{batch:?}");
        assert!(text.contains("RecordBatch"), "{text}");
        assert!(text.contains("rows"), "{text}");
    }

    #[test]
    fn options_are_composable() {
        let options = RecordBatchOptions::new()
            .with_row_count(3)
            .with_column_types(ColumnTypeCheck::None)
            .with_nullability_check(false);
        assert_eq!(options.row_count, Some(3));
        assert_eq!(options.column_types, ColumnTypeCheck::None);
        assert!(!options.check_nullability);

        // With every check relaxed only the length still matters.
        let schema = Arc::new(Schema::new(vec![Field::required("a", DataType::Utf8)]));
        let column = Int32Array::from_opt_iter([Some(1), None, Some(3)]).into_array_ref();
        assert!(RecordBatch::try_new_with_options(schema, vec![column], options).is_ok());
    }
}
