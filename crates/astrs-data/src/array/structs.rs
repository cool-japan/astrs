//! [`StructArray`] — a column of named child columns, all the same length.
//!
//! A struct array is a record batch that happens to be a column: `n` child
//! arrays of equal length, plus one validity bitmap that nulls out a whole
//! row across every child at once. AstRS uses it for compound payloads —
//! `Pose{position, orientation}`, `Detections{boxes, scores, labels}` — where
//! the fields must travel together.
//!
//! A struct with **no** children carries no length information in its
//! children, so [`StructArray::try_new`] needs an explicit one; that is what
//! [`crate::DataError::UnknownStructLength`] reports.
//!
//! ```
//! use astrs_data::array::{Array, Float64Array, IntoArrayRef, StringArray, StructArray};
//! use astrs_data::{DataType, Field};
//!
//! let pose = StructArray::try_new(
//!     vec![
//!         Field::new("frame", DataType::Utf8, false),
//!         Field::new("x", DataType::Float64, false),
//!     ],
//!     vec![
//!         StringArray::from_values(["odom", "map"]).into_array_ref(),
//!         Float64Array::from_values([1.0, 2.0]).into_array_ref(),
//!     ],
//!     None,
//! )?;
//!
//! assert_eq!(pose.len(), 2);
//! assert_eq!(pose.num_columns(), 2);
//! assert!(pose.column_by_name("x").is_some());
//! # Ok::<(), astrs_data::DataError>(())
//! ```

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use crate::array::{Array, ArrayRef, check_validity, clamp_window, slice_array, validity_eq};
use crate::buffer::Bitmap;
use crate::datatype::{DataType, Field};
use crate::error::{DataError, Result};
use crate::sealed::Sealed;

/// A column of named child columns of equal length.
#[derive(Clone)]
pub struct StructArray {
    /// Always `DataType::Struct(fields)`.
    data_type: DataType,
    /// One array per field, in field order, each exactly `len` long.
    columns: Vec<ArrayRef>,
    /// Logical length.
    len: usize,
    /// Row-level validity, or `None` when every row is valid.
    validity: Option<Bitmap>,
}

impl StructArray {
    /// Builds a struct array from its fields and columns.
    ///
    /// # Errors
    ///
    /// * [`DataError::ColumnCountMismatch`] when the counts disagree.
    /// * [`DataError::TypeMismatch`] when a column's type differs from its
    ///   field's.
    /// * [`DataError::ColumnLengthMismatch`] when the columns are not all the
    ///   same length.
    /// * [`DataError::UnknownStructLength`] when there are no columns.
    /// * [`DataError::ValidityLengthMismatch`] when the bitmap length is wrong.
    pub fn try_new(
        fields: Vec<Field>,
        columns: Vec<ArrayRef>,
        validity: Option<Bitmap>,
    ) -> Result<Self> {
        let Some(first) = columns.first() else {
            return Err(DataError::UnknownStructLength);
        };
        Self::try_new_with_len(fields, columns.clone(), first.len(), validity)
    }

    /// Builds a struct array with an explicit length.
    ///
    /// Required for the zero-column case, and useful when the caller already
    /// knows the row count.
    ///
    /// # Errors
    ///
    /// The same set as [`StructArray::try_new`], minus
    /// [`DataError::UnknownStructLength`].
    ///
    /// ```
    /// use astrs_data::array::{Array, StructArray};
    ///
    /// let empty = StructArray::try_new_with_len(Vec::new(), Vec::new(), 4, None)?;
    /// assert_eq!(empty.len(), 4);
    /// assert_eq!(empty.num_columns(), 0);
    /// # Ok::<(), astrs_data::DataError>(())
    /// ```
    pub fn try_new_with_len(
        fields: Vec<Field>,
        columns: Vec<ArrayRef>,
        len: usize,
        validity: Option<Bitmap>,
    ) -> Result<Self> {
        if fields.len() != columns.len() {
            return Err(DataError::ColumnCountMismatch {
                fields: fields.len(),
                columns: columns.len(),
            });
        }
        for (index, (field, column)) in fields.iter().zip(columns.iter()).enumerate() {
            if field.data_type() != column.data_type() {
                return Err(DataError::type_mismatch(
                    field.data_type().clone(),
                    column.data_type().clone(),
                ));
            }
            if column.len() != len {
                return Err(DataError::ColumnLengthMismatch {
                    index,
                    name: field.name().to_owned(),
                    expected: len,
                    actual: column.len(),
                });
            }
        }
        check_validity(validity.as_ref(), len)?;
        Ok(Self {
            data_type: DataType::Struct(fields),
            columns,
            len,
            validity,
        })
    }

    /// An all-null struct array of `len` rows.
    ///
    /// Every child column is itself all-null, which keeps the layout valid.
    ///
    /// # Errors
    ///
    /// [`DataError::InvalidFixedSize`] when a child type carries a
    /// non-positive fixed width.
    pub fn new_null(fields: Vec<Field>, len: usize) -> Result<Self> {
        let mut columns = Vec::with_capacity(fields.len());
        for field in &fields {
            columns.push(crate::array::new_null_array(field.data_type(), len)?);
        }
        Ok(Self {
            data_type: DataType::Struct(fields),
            columns,
            len,
            validity: Some(Bitmap::new_unset(len)),
        })
    }

    /// Assembles the parts without revalidating.
    ///
    /// Safe, and crate-internal: see [`crate::array::ListArray::from_parts`].
    pub(crate) fn from_parts(
        fields: Vec<Field>,
        columns: Vec<ArrayRef>,
        len: usize,
        validity: Option<Bitmap>,
    ) -> Self {
        debug_assert_eq!(fields.len(), columns.len());
        debug_assert!(columns.iter().all(|column| column.len() == len));
        Self {
            data_type: DataType::Struct(fields),
            columns,
            len,
            validity,
        }
    }

    /// The struct's fields, in column order.
    #[must_use]
    pub fn fields(&self) -> &[Field] {
        match &self.data_type {
            DataType::Struct(fields) => fields,
            // Unreachable: the constructor always stores a `Struct`.
            _ => &[],
        }
    }

    /// The child columns, in field order.
    #[inline]
    #[must_use]
    pub fn columns(&self) -> &[ArrayRef] {
        &self.columns
    }

    /// Number of child columns.
    #[inline]
    #[must_use]
    pub const fn num_columns(&self) -> usize {
        self.columns.len()
    }

    /// The child column at `index`.
    #[inline]
    #[must_use]
    pub fn column(&self, index: usize) -> Option<&ArrayRef> {
        self.columns.get(index)
    }

    /// The first child column named `name`.
    #[must_use]
    pub fn column_by_name(&self, name: &str) -> Option<&ArrayRef> {
        let index = self.fields().iter().position(|f| f.name() == name)?;
        self.columns.get(index)
    }

    /// The first child column named `name`.
    ///
    /// # Errors
    ///
    /// [`DataError::FieldNotFound`] when no field has that name.
    pub fn try_column_by_name(&self, name: &str) -> Result<&ArrayRef> {
        self.column_by_name(name)
            .ok_or_else(|| DataError::FieldNotFound {
                name: name.to_owned(),
            })
    }

    /// The row at `index` as a one-row struct array, ignoring validity.
    ///
    /// Returns `None` only when `index` is out of range.
    #[must_use]
    pub fn value(&self, index: usize) -> Option<Self> {
        (index < self.len).then(|| self.slice(index, 1))
    }

    /// The logical row at `index`: `None` when the row is null or out of range.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<Self> {
        if self.is_valid_index(index) {
            self.value(index)
        } else {
            None
        }
    }

    /// A zero-copy sub-range, clamped to the array (the crate-wide slicing
    /// convention).
    ///
    /// Every child column is sliced by the same window.
    #[must_use]
    pub fn slice(&self, offset: usize, len: usize) -> Self {
        let (offset, len) = clamp_window(self.len, offset, len);
        Self {
            data_type: self.data_type.clone(),
            columns: self
                .columns
                .iter()
                .map(|column| slice_array(column, offset, len))
                .collect(),
            len,
            validity: self.validity.as_ref().map(|bits| bits.slice(offset, len)),
        }
    }

    /// Checked [`StructArray::slice`].
    ///
    /// # Errors
    ///
    /// [`DataError::SliceOutOfBounds`] when the window leaves the array.
    pub fn try_slice(&self, offset: usize, len: usize) -> Result<Self> {
        if offset.saturating_add(len) > self.len {
            return Err(DataError::SliceOutOfBounds {
                offset,
                len,
                available: self.len,
            });
        }
        Ok(self.slice(offset, len))
    }

    /// Returns a copy with a different validity bitmap.
    ///
    /// # Errors
    ///
    /// [`DataError::ValidityLengthMismatch`] when the bitmap length is wrong.
    pub fn with_validity(&self, validity: Option<Bitmap>) -> Result<Self> {
        check_validity(validity.as_ref(), self.len)?;
        Ok(Self {
            data_type: self.data_type.clone(),
            columns: self.columns.clone(),
            len: self.len,
            validity,
        })
    }

    /// A struct holding only the columns at `indices`, in the order given.
    ///
    /// # Errors
    ///
    /// [`DataError::IndexOutOfBounds`] when an index has no column.
    pub fn project(&self, indices: &[usize]) -> Result<Self> {
        let fields = self.fields();
        let mut projected_fields = Vec::with_capacity(indices.len());
        let mut projected_columns = Vec::with_capacity(indices.len());
        for &index in indices {
            let field = fields.get(index).ok_or(DataError::IndexOutOfBounds {
                index,
                len: fields.len(),
            })?;
            let column = self.columns.get(index).ok_or(DataError::IndexOutOfBounds {
                index,
                len: self.columns.len(),
            })?;
            projected_fields.push(field.clone());
            projected_columns.push(Arc::clone(column));
        }
        Ok(Self {
            data_type: DataType::Struct(projected_fields),
            columns: projected_columns,
            len: self.len,
            validity: self.validity.clone(),
        })
    }

    /// Whether `index` is inside the array and not null.
    #[inline]
    fn is_valid_index(&self, index: usize) -> bool {
        index < self.len && self.validity.as_ref().is_none_or(|bits| bits.value(index))
    }
}

impl Sealed for StructArray {}

impl Array for StructArray {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn data_type(&self) -> &DataType {
        &self.data_type
    }

    fn len(&self) -> usize {
        self.len
    }

    fn validity(&self) -> Option<&Bitmap> {
        self.validity.as_ref()
    }

    fn slice(&self, offset: usize, len: usize) -> ArrayRef {
        Arc::new(Self::slice(self, offset, len))
    }

    fn buffer_memory_size(&self) -> usize {
        self.columns
            .iter()
            .map(|column| column.buffer_memory_size())
            .sum::<usize>()
            + self
                .validity
                .as_ref()
                .map_or(0, |bits| bits.buffer().backing_len())
    }

    fn children(&self) -> Vec<&ArrayRef> {
        self.columns.iter().collect()
    }

    fn equals(&self, other: &dyn Array) -> bool {
        let Some(other) = other.as_any().downcast_ref::<Self>() else {
            return false;
        };
        self.data_type == other.data_type
            && self.len == other.len
            && validity_eq(self.validity.as_ref(), other.validity.as_ref(), self.len)
            && self.columns.len() == other.columns.len()
            && self
                .columns
                .iter()
                .zip(other.columns.iter())
                .all(|(a, b)| a == b)
    }
}

impl PartialEq for StructArray {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.equals(other)
    }
}

impl fmt::Debug for StructArray {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "StructArray[len={}, nulls={}] {{",
            self.len,
            self.null_count()
        )?;
        for (index, (field, column)) in self.fields().iter().zip(self.columns.iter()).enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{}: {}", field.name(), column.data_type())?;
        }
        f.write_str("}")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::array::{Float64Array, Int32Array, IntoArrayRef, StringArray};

    fn fields() -> Vec<Field> {
        vec![
            Field::new("id", DataType::Int32, false),
            Field::new("name", DataType::Utf8, true),
        ]
    }

    fn sample() -> StructArray {
        StructArray::try_new(
            fields(),
            vec![
                Int32Array::from_values([1, 2, 3]).into_array_ref(),
                StringArray::from_opt_iter([Some("a"), None, Some("c")]).into_array_ref(),
            ],
            None,
        )
        .unwrap()
    }

    #[test]
    fn no_null_array() {
        let strukt = sample();
        assert_eq!(strukt.len(), 3);
        assert_eq!(strukt.num_columns(), 2);
        assert_eq!(strukt.null_count(), 0);
        assert_eq!(strukt.fields().len(), 2);
        assert!(strukt.column(0).is_some());
        assert!(strukt.column(2).is_none());
        assert!(strukt.column_by_name("name").is_some());
        assert!(strukt.column_by_name("missing").is_none());
        assert!(strukt.try_column_by_name("id").is_ok());
        assert_eq!(
            strukt.try_column_by_name("nope").unwrap_err(),
            DataError::FieldNotFound {
                name: "nope".to_owned()
            }
        );
        assert_eq!(strukt.children().len(), 2);
    }

    #[test]
    fn zero_column_struct_needs_an_explicit_length() {
        assert_eq!(
            StructArray::try_new(Vec::new(), Vec::new(), None).unwrap_err(),
            DataError::UnknownStructLength
        );
        let empty = StructArray::try_new_with_len(Vec::new(), Vec::new(), 4, None).unwrap();
        assert_eq!(empty.len(), 4);
        assert_eq!(empty.num_columns(), 0);
        assert_eq!(empty.slice(1, 2).len(), 2);
    }

    #[test]
    fn empty_array() {
        let strukt = StructArray::try_new(
            fields(),
            vec![
                Int32Array::from_values([] as [i32; 0]).into_array_ref(),
                StringArray::from_values([] as [&str; 0]).into_array_ref(),
            ],
            None,
        )
        .unwrap();
        assert!(strukt.is_empty());
        assert!(strukt.get(0).is_none());
    }

    #[test]
    fn all_null_array() {
        let strukt = StructArray::new_null(fields(), 3).unwrap();
        assert_eq!(strukt.len(), 3);
        assert_eq!(strukt.null_count(), 3);
        assert_eq!(strukt.num_columns(), 2);
        for index in 0..3 {
            assert!(strukt.get(index).is_none());
            assert_eq!(strukt.value(index).map(|s| s.len()), Some(1));
        }
        assert_eq!(strukt.columns()[0].null_count(), 3);
    }

    #[test]
    fn row_level_nulls() {
        let strukt = sample()
            .with_validity(Some([true, false, true].into_iter().collect()))
            .unwrap();
        assert_eq!(strukt.null_count(), 1);
        assert!(strukt.get(1).is_none());
        assert_eq!(strukt.value(1).map(|s| s.len()), Some(1));
        assert!(strukt.get(0).is_some());
    }

    #[test]
    fn column_count_must_match_the_fields() {
        assert_eq!(
            StructArray::try_new(
                fields(),
                vec![Int32Array::from_values([1]).into_array_ref()],
                None
            )
            .unwrap_err(),
            DataError::ColumnCountMismatch {
                fields: 2,
                columns: 1
            }
        );
    }

    #[test]
    fn column_types_must_match_the_fields() {
        let err = StructArray::try_new(
            fields(),
            vec![
                Float64Array::from_values([1.0]).into_array_ref(),
                StringArray::from_values(["a"]).into_array_ref(),
            ],
            None,
        )
        .unwrap_err();
        assert!(matches!(err, DataError::TypeMismatch { .. }));
    }

    #[test]
    fn column_lengths_must_agree() {
        let err = StructArray::try_new(
            fields(),
            vec![
                Int32Array::from_values([1, 2]).into_array_ref(),
                StringArray::from_values(["a"]).into_array_ref(),
            ],
            None,
        )
        .unwrap_err();
        assert_eq!(
            err,
            DataError::ColumnLengthMismatch {
                index: 1,
                name: "name".to_owned(),
                expected: 2,
                actual: 1
            }
        );
    }

    #[test]
    fn validity_length_is_checked() {
        assert!(matches!(
            StructArray::try_new(
                fields(),
                vec![
                    Int32Array::from_values([1]).into_array_ref(),
                    StringArray::from_values(["a"]).into_array_ref(),
                ],
                Some(Bitmap::new_set(5))
            ),
            Err(DataError::ValidityLengthMismatch { .. })
        ));
    }

    #[test]
    fn slicing_slices_every_column() {
        let strukt = StructArray::try_new(
            fields(),
            vec![
                Int32Array::from_values(0..10).into_array_ref(),
                StringArray::from_values((0..10).map(|i| i.to_string())).into_array_ref(),
            ],
            Some((0..10).map(|i| i % 3 != 0).collect()),
        )
        .unwrap();
        let window = strukt.slice(2, 5);
        assert_eq!(window.len(), 5);
        for column in window.columns() {
            assert_eq!(column.len(), 5);
        }
        assert_eq!(
            window.columns()[0]
                .as_any()
                .downcast_ref::<Int32Array>()
                .map(|a| a.values().to_vec()),
            Some(vec![2, 3, 4, 5, 6])
        );
        assert_eq!(window.null_count(), 2);
        assert_eq!(strukt.slice(9, 99).len(), 1);
        assert_eq!(strukt.slice(99, 1).len(), 0);
        assert_eq!(strukt.slice(1, 8).slice(2, 3).len(), 3);
        assert!(strukt.try_slice(9, 2).is_err());
    }

    #[test]
    fn projection_reorders_columns() {
        let strukt = sample();
        let projected = strukt.project(&[1, 0]).unwrap();
        assert_eq!(projected.num_columns(), 2);
        assert_eq!(projected.fields()[0].name(), "name");
        assert_eq!(projected.len(), 3);
        assert_eq!(strukt.project(&[]).unwrap().num_columns(), 0);
        assert_eq!(
            strukt.project(&[5]).unwrap_err(),
            DataError::IndexOutOfBounds { index: 5, len: 2 }
        );
    }

    #[test]
    fn nested_structs() {
        let inner = sample();
        let outer = StructArray::try_new(
            vec![Field::new("inner", inner.data_type().clone(), true)],
            vec![inner.clone().into_array_ref()],
            None,
        )
        .unwrap();
        assert_eq!(outer.len(), 3);
        assert_eq!(outer.num_columns(), 1);
        let child = outer.column(0).unwrap();
        assert_eq!(child.as_any().downcast_ref::<StructArray>(), Some(&inner));
        assert!(outer.buffer_memory_size() > 0);
    }

    #[test]
    fn equality_is_by_logical_content() {
        assert_eq!(sample(), sample());
        let different = StructArray::try_new(
            fields(),
            vec![
                Int32Array::from_values([1, 2, 9]).into_array_ref(),
                StringArray::from_opt_iter([Some("a"), None, Some("c")]).into_array_ref(),
            ],
            None,
        )
        .unwrap();
        assert_ne!(sample(), different);
        assert_ne!(sample(), sample().slice(0, 2));
        assert_ne!(sample(), sample().project(&[0]).unwrap());
    }

    #[test]
    fn debug_output_lists_fields() {
        let rendered = format!("{:?}", sample());
        assert!(rendered.contains("len=3"), "{rendered}");
        assert!(rendered.contains("id: Int32"), "{rendered}");
        assert!(rendered.contains("name: Utf8"), "{rendered}");
    }

    #[test]
    fn trait_object_round_trip() {
        let column: ArrayRef = sample().into_array_ref();
        assert_eq!(column.len(), 3);
        assert_eq!(column.children().len(), 2);
        assert_eq!(Array::slice(column.as_ref(), 1, 1).len(), 1);
    }
}
