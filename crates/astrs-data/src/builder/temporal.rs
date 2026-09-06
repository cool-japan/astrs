//! [`TimestampBuilder`] and [`DurationBuilder`] — writers for the two
//! nanosecond temporal columns.
//!
//! Both wrap a `PrimitiveBuilder<i64>` that has been told to report the
//! temporal data type, so they share the `i64` fast path while keeping the
//! type distinction the graph's edge checker relies on.
//!
//! ```
//! use astrs_data::array::Array;
//! use astrs_data::builder::{ArrayBuilder, TimestampBuilder};
//!
//! let mut b = TimestampBuilder::with_capacity(3);
//! b.append_value(1_000_000_000);
//! b.append_null();
//! b.append_duration(std::time::Duration::from_millis(1_500));
//!
//! let array = b.finish();
//! assert_eq!(array.len(), 3);
//! assert_eq!(array.get(2), Some(1_500_000_000));
//! ```

use std::any::Any;
use std::sync::Arc;

use crate::array::{ArrayRef, DurationArray, TimestampArray};
use crate::builder::ArrayBuilder;
use crate::builder::primitive::PrimitiveBuilder;
use crate::datatype::DataType;

/// Generates the shared body of [`TimestampBuilder`] and [`DurationBuilder`].
macro_rules! temporal_builder {
    ($name:ident, $array:ident, $data_type:expr, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug)]
        pub struct $name {
            /// The `i64` writer, reporting this builder's data type.
            inner: PrimitiveBuilder<i64>,
        }

        impl $name {
            /// An empty builder.
            #[must_use]
            pub fn new() -> Self {
                Self::with_capacity(0)
            }

            /// An empty builder with room for `capacity` values.
            #[must_use]
            pub fn with_capacity(capacity: usize) -> Self {
                Self {
                    inner: PrimitiveBuilder::with_type_and_capacity($data_type, capacity),
                }
            }

            /// Appends one nanosecond value.
            #[inline]
            pub fn append_value(&mut self, nanos: i64) {
                self.inner.append_value(nanos);
            }

            /// Appends a value or a null.
            #[inline]
            pub fn append_option(&mut self, nanos: Option<i64>) {
                self.inner.append_option(nanos);
            }

            /// Appends every value in `values`, none of them null.
            pub fn append_slice(&mut self, values: &[i64]) {
                self.inner.append_slice(values);
            }

            /// Appends a [`std::time::Duration`], saturating at [`i64::MAX`]
            /// nanoseconds (about 292 years).
            pub fn append_duration(&mut self, duration: std::time::Duration) {
                let nanos = i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX);
                self.append_value(nanos);
            }

            /// Seals the column, resetting the builder.
            #[must_use]
            pub fn finish(&mut self) -> $array {
                let values = self.inner.finish();
                $array::from_primitive(&values).unwrap_or_else(|_| $array::new_null(0))
            }

            /// Seals the column without resetting the builder.
            #[must_use]
            pub fn finish_cloned(&self) -> $array {
                let values = self.inner.finish_cloned();
                $array::from_primitive(&values).unwrap_or_else(|_| $array::new_null(0))
            }

            /// Drops every appended value, keeping the allocation.
            pub fn clear(&mut self) {
                self.inner.clear();
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl ArrayBuilder for $name {
            fn len(&self) -> usize {
                self.inner.len()
            }

            fn data_type(&self) -> DataType {
                $data_type
            }

            fn append_null(&mut self) {
                ArrayBuilder::append_null(&mut self.inner);
            }

            fn append_nulls(&mut self, count: usize) {
                ArrayBuilder::append_nulls(&mut self.inner, count);
            }

            fn reserve(&mut self, additional: usize) {
                self.inner.reserve(additional);
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

        impl Extend<i64> for $name {
            fn extend<I: IntoIterator<Item = i64>>(&mut self, iter: I) {
                for value in iter {
                    self.append_value(value);
                }
            }
        }

        impl Extend<Option<i64>> for $name {
            fn extend<I: IntoIterator<Item = Option<i64>>>(&mut self, iter: I) {
                for value in iter {
                    self.append_option(value);
                }
            }
        }
    };
}

temporal_builder!(
    TimestampBuilder,
    TimestampArray,
    DataType::Timestamp,
    "Builds a `Timestamp(ns)` column — nanoseconds since the Unix epoch."
);

temporal_builder!(
    DurationBuilder,
    DurationArray,
    DataType::Duration,
    "Builds a `Duration(ns)` column — nanosecond interval counts."
);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::array::Array;

    #[test]
    fn empty_builders() {
        assert!(TimestampBuilder::new().finish_cloned().is_empty());
        assert!(DurationBuilder::default().finish_cloned().is_empty());
    }

    #[test]
    fn timestamp_round_trip() {
        let mut builder = TimestampBuilder::with_capacity(4);
        builder.append_value(1);
        builder.append_null();
        builder.append_option(Some(3));
        builder.append_option(None);
        let array = builder.finish();
        assert_eq!(array.len(), 4);
        assert_eq!(array.null_count(), 2);
        assert_eq!(array.data_type(), &DataType::Timestamp);
        assert_eq!(
            array.iter().collect::<Vec<_>>(),
            vec![Some(1), None, Some(3), None]
        );
        assert!(builder.is_empty());
    }

    #[test]
    fn duration_round_trip() {
        let mut builder = DurationBuilder::new();
        builder.append_slice(&[1, 2, 3]);
        builder.append_null();
        let array = builder.finish();
        assert_eq!(array.len(), 4);
        assert_eq!(array.data_type(), &DataType::Duration);
        assert_eq!(array.values(), &[1, 2, 3, 0]);
    }

    #[test]
    fn std_duration_appends_and_saturates() {
        let mut builder = DurationBuilder::new();
        builder.append_duration(std::time::Duration::from_millis(1_500));
        builder.append_duration(std::time::Duration::from_secs(0));
        builder.append_duration(std::time::Duration::MAX);
        let array = builder.finish();
        assert_eq!(array.get(0), Some(1_500_000_000));
        assert_eq!(array.get(1), Some(0));
        assert_eq!(array.get(2), Some(i64::MAX), "saturates rather than wraps");
    }

    #[test]
    fn all_null_column() {
        let mut builder = TimestampBuilder::new();
        builder.append_nulls(3);
        let array = builder.finish();
        assert_eq!(array.null_count(), 3);
        assert_eq!(array.values(), &[0, 0, 0]);
    }

    #[test]
    fn finish_cloned_and_clear() {
        let mut builder = TimestampBuilder::new();
        builder.append_value(7);
        let snapshot = builder.finish_cloned();
        assert_eq!(snapshot.get(0), Some(7));
        assert_eq!(builder.len(), 1);
        builder.clear();
        assert!(builder.is_empty());
        assert_eq!(builder.finish().len(), 0);
    }

    #[test]
    fn extend_from_iterators() {
        let mut builder = TimestampBuilder::new();
        builder.extend([1i64, 2]);
        builder.extend([Some(3i64), None]);
        let array = builder.finish();
        assert_eq!(array.len(), 4);
        assert_eq!(array.null_count(), 1);
    }

    #[test]
    fn dynamic_interface_keeps_the_data_type() {
        let mut builders: Vec<Box<dyn ArrayBuilder>> = vec![
            Box::new(TimestampBuilder::new()),
            Box::new(DurationBuilder::new()),
        ];
        for builder in &mut builders {
            let data_type = builder.data_type();
            builder.reserve(4);
            builder.append_null();
            let array = builder.finish_array();
            assert_eq!(array.data_type(), &data_type);
            assert_eq!(array.null_count(), 1);
        }
    }
}
