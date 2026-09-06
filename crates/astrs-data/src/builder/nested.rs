//! Builders for the nested columns: [`ListBuilder`], [`FixedSizeListBuilder`]
//! and [`StructBuilder`].
//!
//! Nested builders own a *child builder* (or several) and manage the parent's
//! offsets and validity around it. The interaction pattern is the same in all
//! three cases: write into the child, then close the parent row.
//!
//! ```
//! use astrs_data::array::Array;
//! use astrs_data::builder::{ArrayBuilder, Int32Builder, ListBuilder};
//! use astrs_data::{DataType, Field};
//!
//! let mut lists = ListBuilder::new(
//!     Field::new("item", DataType::Int32, false),
//!     Int32Builder::new(),
//! );
//!
//! lists.values().append_slice(&[1, 2]);
//! lists.append(true);          // close a two-element list
//! lists.append_null();         // a null list
//! lists.values().append_value(3);
//! lists.append(true);
//!
//! let array = lists.finish();
//! assert_eq!(array.len(), 3);
//! assert_eq!(array.value_length(0), Some(2));
//! assert!(array.get(1).is_none());
//! ```

use std::any::Any;
use std::sync::Arc;

use crate::array::{ArrayRef, FixedSizeListArray, ListArray, StructArray, check_fixed_size};
use crate::buffer::ScalarBuffer;
use crate::builder::{ArrayBuilder, ValidityBuilder};
use crate::datatype::{DataType, Field};
use crate::error::{DataError, Result};

/// Row-at-a-time writer producing a [`ListArray`].
///
/// `B` is the child builder; the list's element type is whatever `B`
/// produces.
#[derive(Debug)]
pub struct ListBuilder<B: ArrayBuilder> {
    /// Describes one element of the list.
    field: Field,
    /// Writes the flat child column.
    values: B,
    /// `len + 1` offsets into the child; starts as `[0]`.
    offsets: Vec<i32>,
    /// Lazily materialised validity.
    validity: ValidityBuilder,
    /// Latched when the child grows past `i32::MAX` values.
    overflowed: bool,
}

impl<B: ArrayBuilder> ListBuilder<B> {
    /// A builder for lists of `field`'s type, writing through `values`.
    ///
    /// The field's type should match what `values` produces; a mismatch is
    /// reported by [`ListBuilder::try_new`] and, in debug builds, by an
    /// assertion here.
    #[must_use]
    pub fn new(field: Field, values: B) -> Self {
        debug_assert_eq!(field.data_type(), &values.data_type());
        Self {
            field,
            values,
            offsets: vec![0],
            validity: ValidityBuilder::with_capacity(0),
            overflowed: false,
        }
    }

    /// Checked [`ListBuilder::new`].
    ///
    /// # Errors
    ///
    /// [`DataError::TypeMismatch`] when the field's type differs from what the
    /// child builder produces.
    ///
    /// ```
    /// use astrs_data::builder::{Int32Builder, ListBuilder};
    /// use astrs_data::{DataType, Field};
    ///
    /// assert!(ListBuilder::try_new(
    ///     Field::new("item", DataType::Int32, false),
    ///     Int32Builder::new(),
    /// )
    /// .is_ok());
    /// assert!(ListBuilder::try_new(
    ///     Field::new("item", DataType::Int64, false),
    ///     Int32Builder::new(),
    /// )
    /// .is_err());
    /// ```
    pub fn try_new(field: Field, values: B) -> Result<Self> {
        let child_type = values.data_type();
        if field.data_type() != &child_type {
            return Err(DataError::type_mismatch(
                field.data_type().clone(),
                child_type,
            ));
        }
        Ok(Self::new(field, values))
    }

    /// The child builder. Append the current list's elements here.
    #[inline]
    pub const fn values(&mut self) -> &mut B {
        &mut self.values
    }

    /// Closes the current list, marking it valid or null.
    ///
    /// Values written into [`ListBuilder::values`] since the last call form
    /// the list. Closing a null list after writing values keeps those values
    /// in the child (they become unreachable), matching Arrow's semantics.
    pub fn append(&mut self, valid: bool) {
        match i32::try_from(self.values.len()) {
            Ok(offset) => self.offsets.push(offset),
            Err(_) => {
                self.overflowed = true;
                self.offsets.push(0);
            }
        }
        self.validity.append(valid);
    }

    /// Closes the current list as a null.
    pub fn append_list_null(&mut self) {
        self.append(false);
    }

    /// Appends a whole list of values in one call.
    ///
    /// `append` is called for you, so this is the shorthand for the common
    /// "write a slice, close the row" pattern.
    pub fn append_values(&mut self, write: impl FnOnce(&mut B)) {
        write(&mut self.values);
        self.append(true);
    }

    /// Seals the column, resetting the builder.
    #[must_use]
    pub fn finish(&mut self) -> ListArray {
        if self.overflowed {
            self.clear();
            return ListArray::new_null(self.field.clone(), 0);
        }
        let offsets = std::mem::replace(&mut self.offsets, vec![0]);
        let validity = self.validity.finish();
        let values = self.values.finish_array();
        ListArray::from_parts(
            self.field.clone(),
            ScalarBuffer::from_slice(&offsets),
            values,
            validity,
        )
    }

    /// Seals the column without resetting the builder.
    #[must_use]
    pub fn finish_cloned(&self) -> ListArray {
        if self.overflowed {
            return ListArray::new_null(self.field.clone(), 0);
        }
        ListArray::from_parts(
            self.field.clone(),
            ScalarBuffer::from_slice(&self.offsets),
            self.values.finish_array_cloned(),
            self.validity.finish_cloned(),
        )
    }

    /// Drops every appended list, keeping the allocations.
    pub fn clear(&mut self) {
        self.offsets.clear();
        self.offsets.push(0);
        self.validity.clear();
        self.overflowed = false;
        let _ = self.values.finish_array();
    }
}

impl<B: ArrayBuilder> ArrayBuilder for ListBuilder<B> {
    fn len(&self) -> usize {
        self.validity.len()
    }

    fn data_type(&self) -> DataType {
        DataType::list(self.field.clone())
    }

    fn append_null(&mut self) {
        self.append(false);
    }

    fn reserve(&mut self, additional: usize) {
        self.offsets.reserve(additional);
    }

    fn finish_array(&mut self) -> ArrayRef {
        Arc::new(self.finish())
    }

    fn finish_array_cloned(&self) -> ArrayRef {
        Arc::new(self.finish_cloned())
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Row-at-a-time writer producing a [`FixedSizeListArray`].
///
/// Every row must contribute exactly `size` child values;
/// [`FixedSizeListBuilder::append`] enforces it.
#[derive(Debug)]
pub struct FixedSizeListBuilder<B: ArrayBuilder> {
    /// Describes one element of the run.
    field: Field,
    /// Child values per row. Always positive.
    size: usize,
    /// Writes the flat child column.
    values: B,
    /// Lazily materialised validity.
    validity: ValidityBuilder,
}

impl<B: ArrayBuilder> FixedSizeListBuilder<B> {
    /// A builder for `size`-element runs of `field`'s type.
    ///
    /// # Errors
    ///
    /// * [`DataError::InvalidFixedSize`] when `size` is not positive.
    /// * [`DataError::TypeMismatch`] when the field's type differs from what
    ///   the child builder produces.
    ///
    /// ```
    /// use astrs_data::array::Array;
    /// use astrs_data::builder::{FixedSizeListBuilder, Float32Builder};
    /// use astrs_data::{DataType, Field};
    ///
    /// let mut points = FixedSizeListBuilder::try_new(
    ///     Field::new("xyz", DataType::Float32, false),
    ///     3,
    ///     Float32Builder::new(),
    /// )?;
    /// points.values().append_slice(&[1.0, 2.0, 3.0]);
    /// points.append(true)?;
    /// assert_eq!(points.finish().len(), 1);
    /// # Ok::<(), astrs_data::DataError>(())
    /// ```
    pub fn try_new(field: Field, size: i32, values: B) -> Result<Self> {
        let width = check_fixed_size(size)?;
        let child_type = values.data_type();
        if field.data_type() != &child_type {
            return Err(DataError::type_mismatch(
                field.data_type().clone(),
                child_type,
            ));
        }
        Ok(Self {
            field,
            size: width,
            values,
            validity: ValidityBuilder::with_capacity(0),
        })
    }

    /// Child values per row.
    #[inline]
    #[must_use]
    pub const fn value_size(&self) -> usize {
        self.size
    }

    /// The child builder. Append the current run's elements here.
    #[inline]
    pub const fn values(&mut self) -> &mut B {
        &mut self.values
    }

    /// Closes the current run, marking it valid or null.
    ///
    /// # Errors
    ///
    /// [`DataError::ChildLengthMismatch`] when the child has not received
    /// exactly `size` more values since the last close. The builder is left
    /// untouched so the caller can correct and retry.
    pub fn append(&mut self, valid: bool) -> Result<()> {
        let expected = (self.validity.len() + 1) * self.size;
        if self.values.len() != expected {
            return Err(DataError::ChildLengthMismatch {
                expected,
                actual: self.values.len(),
            });
        }
        self.validity.append(valid);
        Ok(())
    }

    /// Closes the current run as a null, filling the child with `size` nulls.
    pub fn append_run_null(&mut self) {
        let expected = (self.validity.len() + 1) * self.size;
        let missing = expected.saturating_sub(self.values.len());
        self.values.append_nulls(missing);
        self.validity.append(false);
    }

    /// Seals the column, resetting the builder.
    #[must_use]
    pub fn finish(&mut self) -> FixedSizeListArray {
        // Pad a short trailing run, then drop anything past the last closed
        // one, so the child is exactly `len * size` values long.
        let complete = self.validity.len() * self.size;
        let missing = complete.saturating_sub(self.values.len());
        self.values.append_nulls(missing);
        let validity = self.validity.finish();
        let values = self.values.finish_array();
        let trimmed = crate::array::slice_array(&values, 0, complete);
        let size = i32::try_from(self.size).unwrap_or(1);
        FixedSizeListArray::from_parts(self.field.clone(), size, trimmed, validity)
    }

    /// Seals the column without resetting the builder.
    ///
    /// A partially written trailing run is excluded.
    #[must_use]
    pub fn finish_cloned(&self) -> FixedSizeListArray {
        let complete = self.validity.len() * self.size;
        let values = self.values.finish_array_cloned();
        let trimmed = crate::array::slice_array(&values, 0, complete);
        let size = i32::try_from(self.size).unwrap_or(1);
        FixedSizeListArray::from_parts(
            self.field.clone(),
            size,
            trimmed,
            self.validity.finish_cloned(),
        )
    }

    /// Drops every appended run, keeping the allocations.
    pub fn clear(&mut self) {
        self.validity.clear();
        let _ = self.values.finish_array();
    }
}

impl<B: ArrayBuilder> ArrayBuilder for FixedSizeListBuilder<B> {
    fn len(&self) -> usize {
        self.validity.len()
    }

    fn data_type(&self) -> DataType {
        DataType::fixed_size_list(self.field.clone(), i32::try_from(self.size).unwrap_or(1))
    }

    fn append_null(&mut self) {
        self.append_run_null();
    }

    fn reserve(&mut self, additional: usize) {
        self.values.reserve(additional.saturating_mul(self.size));
    }

    fn finish_array(&mut self) -> ArrayRef {
        Arc::new(self.finish())
    }

    fn finish_array_cloned(&self) -> ArrayRef {
        Arc::new(self.finish_cloned())
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// Row-at-a-time writer producing a [`StructArray`].
///
/// Holds one boxed child builder per field. Because the children are
/// heterogeneous, values go in through
/// [`StructBuilder::field_builder`] and a downcast:
///
/// ```
/// use astrs_data::array::Array;
/// use astrs_data::builder::{
///     ArrayBuilder, BuilderExt, Int32Builder, StringBuilder, StructBuilder,
/// };
/// use astrs_data::{DataType, Field};
///
/// let mut rows = StructBuilder::new(
///     vec![
///         Field::new("id", DataType::Int32, false),
///         Field::new("name", DataType::Utf8, true),
///     ],
///     vec![Box::new(Int32Builder::new()), Box::new(StringBuilder::new())],
/// )?;
///
/// rows.field_builder(0)
///     .and_then(BuilderExt::downcast_mut::<Int32Builder>)
///     .map(|b| b.append_value(7));
/// rows.field_builder(1)
///     .and_then(BuilderExt::downcast_mut::<StringBuilder>)
///     .map(|b| b.append_value("lidar"));
/// rows.append(true);
///
/// let array = rows.finish();
/// assert_eq!(array.len(), 1);
/// # Ok::<(), astrs_data::DataError>(())
/// ```
#[derive(Debug)]
pub struct StructBuilder {
    /// The struct's fields, in column order.
    fields: Vec<Field>,
    /// One builder per field, in the same order.
    builders: Vec<Box<dyn ArrayBuilder>>,
    /// Lazily materialised row validity.
    validity: ValidityBuilder,
}

impl StructBuilder {
    /// A builder over one child builder per field.
    ///
    /// # Errors
    ///
    /// * [`DataError::ColumnCountMismatch`] when the counts disagree.
    /// * [`DataError::TypeMismatch`] when a builder's output type differs from
    ///   its field's.
    pub fn new(fields: Vec<Field>, builders: Vec<Box<dyn ArrayBuilder>>) -> Result<Self> {
        if fields.len() != builders.len() {
            return Err(DataError::ColumnCountMismatch {
                fields: fields.len(),
                columns: builders.len(),
            });
        }
        for (field, builder) in fields.iter().zip(builders.iter()) {
            let child_type = builder.data_type();
            if field.data_type() != &child_type {
                return Err(DataError::type_mismatch(
                    field.data_type().clone(),
                    child_type,
                ));
            }
        }
        Ok(Self {
            fields,
            builders,
            validity: ValidityBuilder::with_capacity(0),
        })
    }

    /// The struct's fields.
    #[inline]
    #[must_use]
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// Number of child builders.
    #[inline]
    #[must_use]
    pub const fn num_fields(&self) -> usize {
        self.builders.len()
    }

    /// The child builder at `index`.
    #[inline]
    pub fn field_builder(&mut self, index: usize) -> Option<&mut Box<dyn ArrayBuilder>> {
        self.builders.get_mut(index)
    }

    /// The child builder for the field named `name`.
    pub fn field_builder_by_name(&mut self, name: &str) -> Option<&mut Box<dyn ArrayBuilder>> {
        let index = self.fields.iter().position(|f| f.name() == name)?;
        self.builders.get_mut(index)
    }

    /// Closes the current row, marking it valid or null.
    ///
    /// Any child that did not receive a value for this row is padded with a
    /// null, so a caller only has to touch the fields it cares about.
    pub fn append(&mut self, valid: bool) {
        let target = self.validity.len() + 1;
        for builder in &mut self.builders {
            let missing = target.saturating_sub(builder.len());
            builder.append_nulls(missing);
        }
        self.validity.append(valid);
    }

    /// Closes the current row as a null, padding every child.
    pub fn append_row_null(&mut self) {
        self.append(false);
    }

    /// Seals the column, resetting the builder.
    #[must_use]
    pub fn finish(&mut self) -> StructArray {
        let len = self.validity.len();
        for builder in &mut self.builders {
            let missing = len.saturating_sub(builder.len());
            builder.append_nulls(missing);
        }
        let validity = self.validity.finish();
        let columns: Vec<ArrayRef> = self
            .builders
            .iter_mut()
            .map(|builder| builder.finish_array())
            .collect();
        StructArray::from_parts(self.fields.clone(), columns, len, validity)
    }

    /// Seals the column without resetting the builder.
    #[must_use]
    pub fn finish_cloned(&self) -> StructArray {
        let len = self.validity.len();
        let columns: Vec<ArrayRef> = self
            .builders
            .iter()
            .map(|builder| {
                let column = builder.finish_array_cloned();
                crate::array::slice_array(&column, 0, len)
            })
            .collect();
        StructArray::from_parts(
            self.fields.clone(),
            columns,
            len,
            self.validity.finish_cloned(),
        )
    }

    /// Drops every appended row, keeping the allocations.
    pub fn clear(&mut self) {
        self.validity.clear();
        for builder in &mut self.builders {
            let _ = builder.finish_array();
        }
    }
}

impl ArrayBuilder for StructBuilder {
    fn len(&self) -> usize {
        self.validity.len()
    }

    fn data_type(&self) -> DataType {
        DataType::Struct(self.fields.clone())
    }

    fn append_null(&mut self) {
        self.append(false);
    }

    fn reserve(&mut self, additional: usize) {
        for builder in &mut self.builders {
            builder.reserve(additional);
        }
    }

    fn finish_array(&mut self) -> ArrayRef {
        Arc::new(self.finish())
    }

    fn finish_array_cloned(&self) -> ArrayRef {
        Arc::new(self.finish_cloned())
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::array::{Array, Float32Array, Int32Array, StringArray};
    use crate::builder::{BuilderExt, Float32Builder, Int32Builder, StringBuilder};

    fn item_field() -> Field {
        Field::new("item", DataType::Int32, false)
    }

    fn child_values(array: &ArrayRef) -> Vec<i32> {
        array
            .as_any()
            .downcast_ref::<Int32Array>()
            .map(|a| a.values().to_vec())
            .unwrap_or_default()
    }

    #[test]
    fn list_builder_round_trip() {
        let mut lists = ListBuilder::new(item_field(), Int32Builder::new());
        lists.values().append_slice(&[1, 2]);
        lists.append(true);
        lists.append_list_null();
        lists.append(true); // an empty (but non-null) list
        lists.values().append_value(3);
        lists.append(true);

        let array = lists.finish();
        assert_eq!(array.len(), 4);
        assert_eq!(array.null_count(), 1);
        assert_eq!(array.value_offsets(), &[0, 2, 2, 2, 3]);
        assert_eq!(child_values(&array.get(0).unwrap()), vec![1, 2]);
        assert!(array.get(1).is_none(), "null list");
        assert_eq!(array.get(2).map(|v| v.len()), Some(0), "empty list");
        assert_eq!(child_values(&array.get(3).unwrap()), vec![3]);
        assert!(lists.is_empty(), "finish resets the builder");
    }

    #[test]
    fn list_builder_empty() {
        let mut lists = ListBuilder::new(item_field(), Int32Builder::new());
        let array = lists.finish();
        assert!(array.is_empty());
        assert_eq!(array.value_offsets(), &[0]);
        assert_eq!(array.data_type(), &DataType::list(item_field()));
    }

    #[test]
    fn list_builder_all_null() {
        let mut lists = ListBuilder::new(item_field(), Int32Builder::new());
        ArrayBuilder::append_nulls(&mut lists, 3);
        let array = lists.finish();
        assert_eq!(array.len(), 3);
        assert_eq!(array.null_count(), 3);
    }

    #[test]
    fn list_builder_append_values_shorthand() {
        let mut lists = ListBuilder::new(item_field(), Int32Builder::new());
        lists.append_values(|values| values.append_slice(&[1, 2, 3]));
        lists.append_values(|values| values.append_value(4));
        let array = lists.finish();
        assert_eq!(array.len(), 2);
        assert_eq!(array.value_length(0), Some(3));
        assert_eq!(array.value_length(1), Some(1));
    }

    #[test]
    fn list_builder_checks_the_child_type() {
        assert!(ListBuilder::try_new(item_field(), Int32Builder::new()).is_ok());
        let err = ListBuilder::try_new(
            Field::new("item", DataType::Int64, false),
            Int32Builder::new(),
        )
        .unwrap_err();
        assert!(matches!(err, DataError::TypeMismatch { .. }));
    }

    #[test]
    fn list_builder_finish_cloned_and_clear() {
        let mut lists = ListBuilder::new(item_field(), Int32Builder::new());
        lists.append_values(|values| values.append_slice(&[1, 2]));
        let snapshot = lists.finish_cloned();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(lists.len(), 1);
        lists.clear();
        assert!(lists.is_empty());
        assert!(lists.finish().is_empty());
        assert_eq!(child_values(&snapshot.get(0).unwrap()), vec![1, 2]);
    }

    #[test]
    fn nested_list_builder() {
        let inner_field = item_field();
        let inner = ListBuilder::new(inner_field.clone(), Int32Builder::new());
        let outer_field = Field::new("inner", DataType::list(inner_field), true);
        let mut outer = ListBuilder::new(outer_field, inner);

        outer.values().append_values(|v| v.append_slice(&[1, 2]));
        outer.values().append_values(|v| v.append_value(3));
        outer.append(true);
        outer.append_list_null();

        let array = outer.finish();
        assert_eq!(array.len(), 2);
        assert_eq!(array.value_length(0), Some(2));
        assert!(array.get(1).is_none());
    }

    #[test]
    fn fixed_size_list_builder_round_trip() {
        let mut points = FixedSizeListBuilder::try_new(
            Field::new("xyz", DataType::Float32, false),
            3,
            Float32Builder::new(),
        )
        .unwrap();
        points.values().append_slice(&[1.0, 2.0, 3.0]);
        points.append(true).unwrap();
        points.append_run_null();
        points.values().append_slice(&[4.0, 5.0, 6.0]);
        points.append(true).unwrap();

        let array = points.finish();
        assert_eq!(array.len(), 3);
        assert_eq!(array.value_size(), 3);
        assert_eq!(array.null_count(), 1);
        assert_eq!(array.values().len(), 9);
        let first = array.get(0).unwrap();
        assert_eq!(
            first
                .as_any()
                .downcast_ref::<Float32Array>()
                .map(|a| a.values().to_vec()),
            Some(vec![1.0, 2.0, 3.0])
        );
        assert!(array.get(1).is_none());
    }

    #[test]
    fn fixed_size_list_builder_rejects_short_runs() {
        let mut points = FixedSizeListBuilder::try_new(
            Field::new("xyz", DataType::Float32, false),
            3,
            Float32Builder::new(),
        )
        .unwrap();
        points.values().append_slice(&[1.0, 2.0]);
        assert_eq!(
            points.append(true).unwrap_err(),
            DataError::ChildLengthMismatch {
                expected: 3,
                actual: 2
            }
        );
        assert_eq!(points.len(), 0, "the failed close changed nothing");
        points.values().append_value(3.0);
        points.append(true).unwrap();
        assert_eq!(points.finish().len(), 1);
    }

    #[test]
    fn fixed_size_list_builder_trims_a_partial_trailing_run() {
        let mut points = FixedSizeListBuilder::try_new(
            Field::new("xyz", DataType::Float32, false),
            2,
            Float32Builder::new(),
        )
        .unwrap();
        points.values().append_slice(&[1.0, 2.0]);
        points.append(true).unwrap();
        points.values().append_value(3.0); // never closed
        let array = points.finish();
        assert_eq!(array.len(), 1);
        assert_eq!(array.values().len(), 2);
    }

    #[test]
    fn fixed_size_list_builder_validates_its_arguments() {
        assert!(
            FixedSizeListBuilder::try_new(
                Field::new("x", DataType::Float32, false),
                0,
                Float32Builder::new()
            )
            .is_err()
        );
        assert!(
            FixedSizeListBuilder::try_new(
                Field::new("x", DataType::Int32, false),
                2,
                Float32Builder::new()
            )
            .is_err()
        );
    }

    #[test]
    fn fixed_size_list_builder_finish_cloned_and_clear() {
        let mut points = FixedSizeListBuilder::try_new(
            Field::new("pair", DataType::Int32, false),
            2,
            Int32Builder::new(),
        )
        .unwrap();
        points.values().append_slice(&[1, 2]);
        points.append(true).unwrap();
        let snapshot = points.finish_cloned();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(points.len(), 1);
        points.clear();
        assert!(points.is_empty());
        assert!(points.finish().is_empty());
    }

    #[test]
    fn struct_builder_round_trip() {
        let fields = vec![
            Field::new("id", DataType::Int32, false),
            Field::new("name", DataType::Utf8, true),
        ];
        let mut rows = StructBuilder::new(
            fields.clone(),
            vec![
                Box::new(Int32Builder::new()),
                Box::new(StringBuilder::new()),
            ],
        )
        .unwrap();
        assert_eq!(rows.num_fields(), 2);
        assert_eq!(rows.fields().len(), 2);

        for index in 0..3 {
            if let Some(builder) = rows
                .field_builder(0)
                .and_then(BuilderExt::downcast_mut::<Int32Builder>)
            {
                builder.append_value(index);
            }
            if let Some(builder) = rows
                .field_builder_by_name("name")
                .and_then(BuilderExt::downcast_mut::<StringBuilder>)
            {
                builder.append_value(format!("n{index}"));
            }
            rows.append(index != 1);
        }

        let array = rows.finish();
        assert_eq!(array.len(), 3);
        assert_eq!(array.null_count(), 1);
        assert_eq!(
            array.column(0).map(|c| c.len()),
            Some(3),
            "columns match the row count"
        );
        let names = array
            .column_by_name("name")
            .and_then(|c| c.as_any().downcast_ref::<StringArray>())
            .map(|a| a.get(2));
        assert_eq!(names, Some(Some("n2")));
        assert!(rows.is_empty());
    }

    #[test]
    fn struct_builder_pads_untouched_children() {
        let fields = vec![
            Field::new("a", DataType::Int32, true),
            Field::new("b", DataType::Int32, true),
        ];
        let mut rows = StructBuilder::new(
            fields,
            vec![Box::new(Int32Builder::new()), Box::new(Int32Builder::new())],
        )
        .unwrap();
        if let Some(builder) = rows
            .field_builder(0)
            .and_then(BuilderExt::downcast_mut::<Int32Builder>)
        {
            builder.append_value(1);
        }
        rows.append(true);
        rows.append_row_null();

        let array = rows.finish();
        assert_eq!(array.len(), 2);
        for column in array.columns() {
            assert_eq!(column.len(), 2);
        }
        assert_eq!(array.columns()[1].null_count(), 2, "b was never written");
    }

    #[test]
    fn struct_builder_validates_its_arguments() {
        let fields = vec![Field::new("a", DataType::Int32, true)];
        assert!(matches!(
            StructBuilder::new(fields.clone(), Vec::new()),
            Err(DataError::ColumnCountMismatch { .. })
        ));
        assert!(matches!(
            StructBuilder::new(fields, vec![Box::new(StringBuilder::new())]),
            Err(DataError::TypeMismatch { .. })
        ));
    }

    #[test]
    fn struct_builder_finish_cloned_and_clear() {
        let fields = vec![Field::new("a", DataType::Int32, true)];
        let mut rows = StructBuilder::new(fields, vec![Box::new(Int32Builder::new())]).unwrap();
        rows.append(true);
        let snapshot = rows.finish_cloned();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(rows.len(), 1);
        rows.clear();
        assert!(rows.is_empty());
        assert_eq!(rows.finish().len(), 0);
    }

    #[test]
    fn struct_builder_zero_fields() {
        let mut rows = StructBuilder::new(Vec::new(), Vec::new()).unwrap();
        rows.append(true);
        rows.append(true);
        let array = rows.finish();
        assert_eq!(array.len(), 2);
        assert_eq!(array.num_columns(), 0);
    }

    #[test]
    fn nested_builders_compose_through_the_dynamic_interface() {
        let mut boxed: Box<dyn ArrayBuilder> =
            Box::new(ListBuilder::new(item_field(), Int32Builder::new()));
        boxed.reserve(4);
        boxed.append_nulls(2);
        assert_eq!(boxed.len(), 2);
        assert_eq!(boxed.data_type(), DataType::list(item_field()));
        let array = boxed.finish_array();
        assert_eq!(array.len(), 2);
        assert_eq!(array.null_count(), 2);
    }
}
