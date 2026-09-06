//! [`BooleanBuilder`] — the row-at-a-time writer for
//! [`crate::array::BooleanArray`].
//!
//! Both of a boolean column's buffers are bitmaps, so the builder writes one
//! bit per value into the values bitmap and (lazily) one into the validity
//! bitmap.
//!
//! ```
//! use astrs_data::array::Array;
//! use astrs_data::builder::{ArrayBuilder, BooleanBuilder};
//!
//! let mut b = BooleanBuilder::with_capacity(4);
//! b.append_value(true);
//! b.append_null();
//! b.append_slice(&[false, true]);
//!
//! let array = b.finish();
//! assert_eq!(array.len(), 4);
//! assert_eq!(array.null_count(), 1);
//! assert_eq!(array.true_count(), 2);
//! ```

use std::any::Any;
use std::sync::Arc;

use crate::array::{ArrayRef, BooleanArray};
use crate::buffer::BitmapBuilder;
use crate::builder::{ArrayBuilder, ValidityBuilder};
use crate::datatype::DataType;

/// Row-at-a-time writer producing a [`BooleanArray`].
#[derive(Debug, Default)]
pub struct BooleanBuilder {
    /// One bit per value.
    values: BitmapBuilder,
    /// Lazily materialised validity.
    validity: ValidityBuilder,
}

impl BooleanBuilder {
    /// An empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty builder with room for `capacity` values.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            values: BitmapBuilder::with_capacity(capacity),
            validity: ValidityBuilder::with_capacity(capacity),
        }
    }

    /// Appends one value.
    #[inline]
    pub fn append_value(&mut self, value: bool) {
        self.values.append(value);
        self.validity.append(true);
    }

    /// Appends a value or a null.
    #[inline]
    pub fn append_option(&mut self, value: Option<bool>) {
        match value {
            Some(value) => self.append_value(value),
            None => ArrayBuilder::append_null(self),
        }
    }

    /// Appends every value in `values`, none of them null.
    pub fn append_slice(&mut self, values: &[bool]) {
        self.values.append_slice(values);
        self.validity.append_n(values.len(), true);
    }

    /// Appends `count` copies of `value`.
    pub fn append_n(&mut self, count: usize, value: bool) {
        self.values.append_n(count, value);
        self.validity.append_n(count, true);
    }

    /// Seals the column, resetting the builder.
    #[must_use]
    pub fn finish(&mut self) -> BooleanArray {
        let values = self.values.finish();
        let validity = self.validity.finish();
        BooleanArray::new(values, validity).unwrap_or_else(|_| BooleanArray::new_null(0))
    }

    /// Seals the column without resetting the builder.
    #[must_use]
    pub fn finish_cloned(&self) -> BooleanArray {
        BooleanArray::new(self.values.finish_cloned(), self.validity.finish_cloned())
            .unwrap_or_else(|_| BooleanArray::new_null(0))
    }

    /// Drops every appended value, keeping the allocations.
    pub fn clear(&mut self) {
        self.values.clear();
        self.validity.clear();
    }
}

impl ArrayBuilder for BooleanBuilder {
    fn len(&self) -> usize {
        self.validity.len()
    }

    fn data_type(&self) -> DataType {
        DataType::Bool
    }

    fn append_null(&mut self) {
        self.values.append(false);
        self.validity.append(false);
    }

    fn append_nulls(&mut self, count: usize) {
        self.values.append_n(count, false);
        self.validity.append_n(count, false);
    }

    fn reserve(&mut self, _additional: usize) {
        // The bitmap builders grow geometrically on append; there is no
        // separate reservation step worth exposing.
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

impl Extend<bool> for BooleanBuilder {
    fn extend<I: IntoIterator<Item = bool>>(&mut self, iter: I) {
        for value in iter {
            self.append_value(value);
        }
    }
}

impl Extend<Option<bool>> for BooleanBuilder {
    fn extend<I: IntoIterator<Item = Option<bool>>>(&mut self, iter: I) {
        for value in iter {
            self.append_option(value);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::array::Array;

    #[test]
    fn empty_builder_produces_an_empty_array() {
        let mut builder = BooleanBuilder::new();
        assert!(builder.is_empty());
        let array = builder.finish();
        assert!(array.is_empty());
        assert!(array.validity().is_none());
        assert_eq!(array.data_type(), &DataType::Bool);
    }

    #[test]
    fn round_trip_without_nulls() {
        let mut builder = BooleanBuilder::with_capacity(3);
        builder.append_value(true);
        builder.append_value(false);
        builder.append_value(true);
        let array = builder.finish();
        assert_eq!(array.len(), 3);
        assert!(array.validity().is_none());
        assert_eq!(array.true_count(), 2);
        assert_eq!(array.false_count(), 1);
    }

    #[test]
    fn round_trip_with_nulls() {
        let mut builder = BooleanBuilder::new();
        builder.append_value(true);
        builder.append_null();
        builder.append_option(Some(false));
        builder.append_option(None);
        let array = builder.finish();
        assert_eq!(array.len(), 4);
        assert_eq!(array.null_count(), 2);
        assert_eq!(
            array.iter().collect::<Vec<_>>(),
            vec![Some(true), None, Some(false), None]
        );
        assert!(builder.is_empty());
    }

    #[test]
    fn all_null_column() {
        let mut builder = BooleanBuilder::new();
        builder.append_nulls(9);
        let array = builder.finish();
        assert_eq!(array.len(), 9);
        assert_eq!(array.null_count(), 9);
        assert_eq!(array.true_count(), 0);
    }

    #[test]
    fn bulk_appends_cross_byte_boundaries() {
        let mut builder = BooleanBuilder::new();
        builder.append_n(5, true);
        builder.append_slice(&[false, false, true, true, false]);
        builder.append_null();
        builder.append_n(20, false);
        let array = builder.finish();
        assert_eq!(array.len(), 31);
        assert_eq!(array.null_count(), 1);
        assert_eq!(array.true_count(), 7);
        for index in 0..5 {
            assert_eq!(array.get(index), Some(true));
        }
        assert_eq!(array.get(5), Some(false));
        assert_eq!(array.get(10), None);
        assert_eq!(array.get(11), Some(false));
    }

    #[test]
    fn finish_cloned_and_clear() {
        let mut builder = BooleanBuilder::new();
        builder.append_value(true);
        builder.append_null();
        let snapshot = builder.finish_cloned();
        assert_eq!(snapshot.len(), 2);
        assert_eq!(builder.len(), 2);
        builder.clear();
        assert!(builder.is_empty());
        assert_eq!(builder.finish().len(), 0);
        assert_eq!(snapshot.null_count(), 1);
    }

    #[test]
    fn extend_from_iterators() {
        let mut builder = BooleanBuilder::new();
        builder.extend([true, false]);
        builder.extend([Some(true), None]);
        let array = builder.finish();
        assert_eq!(array.len(), 4);
        assert_eq!(array.null_count(), 1);
        assert_eq!(array.true_count(), 2);
    }

    #[test]
    fn large_column_round_trips() {
        let mut builder = BooleanBuilder::with_capacity(20_000);
        for index in 0..20_000 {
            builder.append_option((index % 11 != 0).then_some(index % 3 == 0));
        }
        let array = builder.finish();
        assert_eq!(array.len(), 20_000);
        assert_eq!(array.null_count(), 20_000_usize.div_ceil(11));
        assert_eq!(array.get(0), None);
        assert_eq!(array.get(3), Some(true));
        assert_eq!(array.get(4), Some(false));
    }

    #[test]
    fn dynamic_interface() {
        let mut builder: Box<dyn ArrayBuilder> = Box::new(BooleanBuilder::new());
        builder.append_null();
        builder.reserve(10);
        assert_eq!(builder.data_type(), DataType::Bool);
        let cloned = builder.finish_array_cloned();
        assert_eq!(cloned.len(), 1);
        assert_eq!(builder.finish_array().len(), 1);
    }
}
