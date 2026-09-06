//! [`PrimitiveBuilder`] — the row-at-a-time writer for
//! [`crate::array::PrimitiveArray`].
//!
//! Values go straight into the final 64-byte aligned allocation, so `finish`
//! is a move rather than a copy: there is no intermediate `Vec<T>` to memcpy
//! out of.
//!
//! ```
//! use astrs_data::array::Array;
//! use astrs_data::builder::{ArrayBuilder, Float32Builder};
//!
//! let mut b = Float32Builder::with_capacity(3);
//! b.append_value(1.0);
//! b.append_null();
//! b.append_slice(&[2.0, 3.0]);
//!
//! let array = b.finish();
//! assert_eq!(array.len(), 4);
//! assert_eq!(array.null_count(), 1);
//! assert_eq!(array.values(), &[1.0, 0.0, 2.0, 3.0]);
//! ```

use std::any::Any;
use std::marker::PhantomData;

use crate::array::{ArrayRef, PrimitiveArray};
use crate::buffer::scalar::{extend_scalars, push_scalar};
use crate::buffer::{AlignedBuf, Buffer, ScalarBuffer};
use crate::builder::{ArrayBuilder, ValidityBuilder};
use crate::datatype::{ArrowNativeType, DataType, F16};

/// Row-at-a-time writer producing a [`PrimitiveArray`].
#[derive(Debug)]
pub struct PrimitiveBuilder<T: ArrowNativeType> {
    /// Type the finished array reports. Normally `T::DATA_TYPE`; the temporal
    /// builders override it.
    data_type: DataType,
    /// Values written so far, already in their final layout.
    values: AlignedBuf,
    /// Lazily materialised validity.
    validity: ValidityBuilder,
    /// `PrimitiveBuilder<T>` produces a `PrimitiveArray<T>`.
    marker: PhantomData<T>,
}

/// Builds an `Int8` column.
pub type Int8Builder = PrimitiveBuilder<i8>;
/// Builds an `Int16` column.
pub type Int16Builder = PrimitiveBuilder<i16>;
/// Builds an `Int32` column.
pub type Int32Builder = PrimitiveBuilder<i32>;
/// Builds an `Int64` column.
pub type Int64Builder = PrimitiveBuilder<i64>;
/// Builds a `UInt8` column.
pub type UInt8Builder = PrimitiveBuilder<u8>;
/// Builds a `UInt16` column.
pub type UInt16Builder = PrimitiveBuilder<u16>;
/// Builds a `UInt32` column.
pub type UInt32Builder = PrimitiveBuilder<u32>;
/// Builds a `UInt64` column.
pub type UInt64Builder = PrimitiveBuilder<u64>;
/// Builds a `Float16` column.
pub type Float16Builder = PrimitiveBuilder<F16>;
/// Builds a `Float32` column.
pub type Float32Builder = PrimitiveBuilder<f32>;
/// Builds a `Float64` column.
pub type Float64Builder = PrimitiveBuilder<f64>;

impl<T: ArrowNativeType> PrimitiveBuilder<T> {
    /// An empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(0)
    }

    /// An empty builder with room for `capacity` values.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self::with_type_and_capacity(T::DATA_TYPE, capacity)
    }

    /// An empty builder that reports `data_type` instead of `T::DATA_TYPE`.
    ///
    /// Crate-internal: the temporal builders use it to reuse the `i64` layout.
    pub(crate) fn with_type_and_capacity(data_type: DataType, capacity: usize) -> Self {
        debug_assert_eq!(data_type.primitive_width(), Some(T::WIDTH));
        Self {
            data_type,
            values: AlignedBuf::with_capacity(capacity.saturating_mul(T::WIDTH)),
            validity: ValidityBuilder::with_capacity(capacity),
            marker: PhantomData,
        }
    }

    /// Appends one value.
    #[inline]
    pub fn append_value(&mut self, value: T) {
        push_scalar(&mut self.values, value);
        self.validity.append(true);
    }

    /// Appends a value or a null.
    #[inline]
    pub fn append_option(&mut self, value: Option<T>) {
        match value {
            Some(value) => self.append_value(value),
            None => ArrayBuilder::append_null(self),
        }
    }

    /// Appends every value in `values`, none of them null.
    ///
    /// One bulk copy rather than `values.len()` individual appends.
    pub fn append_slice(&mut self, values: &[T]) {
        extend_scalars(&mut self.values, values);
        self.validity.append_n(values.len(), true);
    }

    /// Appends `count` copies of `value`.
    pub fn append_n(&mut self, count: usize, value: T) {
        self.values.reserve(count.saturating_mul(T::WIDTH));
        for _ in 0..count {
            push_scalar(&mut self.values, value);
        }
        self.validity.append_n(count, true);
    }

    /// Seals the column, resetting the builder.
    #[must_use]
    pub fn finish(&mut self) -> PrimitiveArray<T> {
        let values = std::mem::take(&mut self.values);
        let validity = self.validity.finish();
        Self::assemble(self.data_type.clone(), values, validity)
    }

    /// Seals the column without resetting the builder.
    #[must_use]
    pub fn finish_cloned(&self) -> PrimitiveArray<T> {
        Self::assemble(
            self.data_type.clone(),
            self.values.clone(),
            self.validity.finish_cloned(),
        )
    }

    /// Drops every appended value, keeping the allocation.
    pub fn clear(&mut self) {
        self.values.clear();
        self.validity.clear();
    }

    /// Shared tail of `finish` and `finish_cloned`.
    fn assemble(
        data_type: DataType,
        values: AlignedBuf,
        validity: Option<crate::buffer::Bitmap>,
    ) -> PrimitiveArray<T> {
        let buffer = Buffer::from(values);
        let scalars = ScalarBuffer::<T>::from_buffer_lossy(&buffer);
        PrimitiveArray::from_parts(data_type, scalars, validity)
    }
}

impl<T: ArrowNativeType> Default for PrimitiveBuilder<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: ArrowNativeType> ArrayBuilder for PrimitiveBuilder<T> {
    fn len(&self) -> usize {
        self.validity.len()
    }

    fn data_type(&self) -> DataType {
        self.data_type.clone()
    }

    fn append_null(&mut self) {
        push_scalar(&mut self.values, T::default());
        self.validity.append(false);
    }

    fn append_nulls(&mut self, count: usize) {
        self.values.extend_zeroed(count.saturating_mul(T::WIDTH));
        self.validity.append_n(count, false);
    }

    fn reserve(&mut self, additional: usize) {
        self.values.reserve(additional.saturating_mul(T::WIDTH));
    }

    fn finish_array(&mut self) -> ArrayRef {
        std::sync::Arc::new(self.finish())
    }

    fn finish_array_cloned(&self) -> ArrayRef {
        std::sync::Arc::new(self.finish_cloned())
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl<T: ArrowNativeType> Extend<T> for PrimitiveBuilder<T> {
    fn extend<I: IntoIterator<Item = T>>(&mut self, iter: I) {
        for value in iter {
            self.append_value(value);
        }
    }
}

impl<T: ArrowNativeType> Extend<Option<T>> for PrimitiveBuilder<T> {
    fn extend<I: IntoIterator<Item = Option<T>>>(&mut self, iter: I) {
        for value in iter {
            self.append_option(value);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::ALIGNMENT;
    use crate::array::Array;

    #[test]
    fn empty_builder_produces_an_empty_array() {
        let mut builder = Int32Builder::new();
        assert!(builder.is_empty());
        let array = builder.finish();
        assert!(array.is_empty());
        assert_eq!(array.null_count(), 0);
        assert!(array.validity().is_none());
    }

    #[test]
    fn round_trip_without_nulls() {
        let mut builder = Int32Builder::with_capacity(4);
        for value in 0..4 {
            builder.append_value(value);
        }
        assert_eq!(builder.len(), 4);
        let array = builder.finish();
        assert_eq!(array.values(), &[0, 1, 2, 3]);
        assert!(
            array.validity().is_none(),
            "no nulls means no validity buffer"
        );
        assert!(builder.is_empty(), "finish resets the builder");
    }

    #[test]
    fn round_trip_with_nulls() {
        let mut builder = Int64Builder::new();
        builder.append_value(1);
        builder.append_null();
        builder.append_option(Some(3));
        builder.append_option(None);
        let array = builder.finish();
        assert_eq!(array.len(), 4);
        assert_eq!(array.null_count(), 2);
        assert_eq!(
            array.iter().collect::<Vec<_>>(),
            vec![Some(1), None, Some(3), None]
        );
        assert_eq!(array.values(), &[1, 0, 3, 0]);
    }

    #[test]
    fn all_null_column() {
        let mut builder = UInt8Builder::new();
        builder.append_nulls(5);
        let array = builder.finish();
        assert_eq!(array.len(), 5);
        assert_eq!(array.null_count(), 5);
        assert_eq!(array.values(), &[0; 5]);
    }

    #[test]
    fn bulk_appends() {
        let mut builder = Float64Builder::new();
        builder.append_slice(&[1.0, 2.0, 3.0]);
        builder.append_n(2, 9.0);
        builder.append_null();
        builder.append_slice(&[]);
        let array = builder.finish();
        assert_eq!(array.len(), 6);
        assert_eq!(array.null_count(), 1);
        assert_eq!(array.values(), &[1.0, 2.0, 3.0, 9.0, 9.0, 0.0]);
    }

    #[test]
    fn values_stay_aligned_however_they_are_appended() {
        for len in [1usize, 7, 64, 1000] {
            let mut builder = Int64Builder::new();
            for value in 0..len {
                builder.append_value(value as i64);
            }
            let array = builder.finish();
            assert_eq!(array.values().as_ptr() as usize % ALIGNMENT, 0, "len {len}");
            assert_eq!(array.len(), len);
        }
    }

    #[test]
    fn finish_cloned_keeps_the_builder() {
        let mut builder = Int32Builder::new();
        builder.append_value(1);
        builder.append_null();
        let snapshot = builder.finish_cloned();
        assert_eq!(snapshot.len(), 2);
        assert_eq!(builder.len(), 2);
        builder.append_value(3);
        let final_array = builder.finish();
        assert_eq!(final_array.len(), 3);
        assert_eq!(snapshot.len(), 2, "the snapshot is independent");
    }

    #[test]
    fn clear_drops_everything() {
        let mut builder = Int32Builder::new();
        builder.append_slice(&[1, 2, 3]);
        builder.append_null();
        builder.clear();
        assert!(builder.is_empty());
        let array = builder.finish();
        assert!(array.is_empty());
        assert!(array.validity().is_none(), "the null was cleared too");
    }

    #[test]
    fn reuse_after_finish() {
        let mut builder = Int32Builder::new();
        builder.append_slice(&[1, 2]);
        let first = builder.finish();
        builder.append_slice(&[3, 4, 5]);
        let second = builder.finish();
        assert_eq!(first.values(), &[1, 2]);
        assert_eq!(second.values(), &[3, 4, 5]);
    }

    #[test]
    fn extend_from_iterators() {
        let mut builder = Int32Builder::new();
        builder.extend([1, 2, 3]);
        builder.extend([Some(4), None]);
        let array = builder.finish();
        assert_eq!(array.len(), 5);
        assert_eq!(array.null_count(), 1);
    }

    #[test]
    fn float16_builder() {
        let mut builder = Float16Builder::new();
        builder.append_value(F16::from_f32(1.5));
        builder.append_null();
        let array = builder.finish();
        assert_eq!(array.data_type(), &DataType::Float16);
        assert_eq!(array.get(0).map(F16::to_f32), Some(1.5));
        assert_eq!(array.get(1), None);
    }

    #[test]
    fn every_width_builds() {
        macro_rules! check {
            ($builder:ty, $value:expr, $data_type:expr) => {{
                let mut builder = <$builder>::new();
                builder.append_value($value);
                let array = builder.finish();
                assert_eq!(array.data_type(), &$data_type);
                assert_eq!(array.get(0), Some($value));
            }};
        }
        check!(Int8Builder, 1i8, DataType::Int8);
        check!(Int16Builder, 1i16, DataType::Int16);
        check!(Int32Builder, 1i32, DataType::Int32);
        check!(Int64Builder, 1i64, DataType::Int64);
        check!(UInt8Builder, 1u8, DataType::UInt8);
        check!(UInt16Builder, 1u16, DataType::UInt16);
        check!(UInt32Builder, 1u32, DataType::UInt32);
        check!(UInt64Builder, 1u64, DataType::UInt64);
        check!(Float32Builder, 1.5f32, DataType::Float32);
        check!(Float64Builder, 1.5f64, DataType::Float64);
    }

    #[test]
    fn large_column_round_trips() {
        let mut builder = Int32Builder::with_capacity(50_000);
        for value in 0..50_000i32 {
            builder.append_option((value % 7 != 0).then_some(value));
        }
        let array = builder.finish();
        assert_eq!(array.len(), 50_000);
        assert_eq!(array.null_count(), 50_000_usize.div_ceil(7));
        assert_eq!(array.get(1), Some(1));
        assert_eq!(array.get(7), None);
        assert_eq!(array.get(49_999), Some(49_999));
    }
}
