//! [`TimestampArray`] and [`DurationArray`] — the two nanosecond temporal
//! columns.
//!
//! Both are `i64` columns with their own [`DataType`]. Blueprint §6.1 pins the
//! unit to nanoseconds and drops the timezone, because AstRS timestamps come
//! from the HLC in `astrs-time`, which is nanosecond-resolution and UTC by
//! construction. That leaves the two types distinguished only by *meaning*:
//!
//! * `Timestamp` — nanoseconds **since the Unix epoch**, an instant.
//! * `Duration` — a nanosecond **count**, an interval.
//!
//! Keeping them as separate Rust types (rather than aliases for
//! `PrimitiveArray<i64>`) is what lets the graph's type checker reject an edge
//! that feeds a duration into an instant port. Internally each wraps a
//! [`PrimitiveArray<i64>`] built through
//! [`PrimitiveArray::try_new_with_type`], so the buffer layout is byte-for-byte
//! the Arrow `Timestamp(NANOSECOND, None)` / `Duration(NANOSECOND)` layout.
//!
//! ```
//! use astrs_data::array::{Array, DurationArray, TimestampArray};
//!
//! let stamps = TimestampArray::from_nanos([1_000_000_000, 1_500_000_000]);
//! assert_eq!(stamps.get(0), Some(1_000_000_000));
//! assert_eq!(stamps.value_as_secs_f64(1), Some(1.5));
//!
//! let gaps = DurationArray::from_nanos([500_000_000]);
//! assert_eq!(gaps.value_as_secs_f64(0), Some(0.5));
//! assert_eq!(gaps.value_as_duration(0), Some(std::time::Duration::from_millis(500)));
//! ```

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use crate::array::iter::{ArrayAccessor, ArrayIter};
use crate::array::primitive::{Int64Array, PrimitiveArray};
use crate::array::{Array, ArrayRef, DURATION_TYPE, TIMESTAMP_TYPE, debug_array};
use crate::buffer::{Bitmap, ScalarBuffer};
use crate::datatype::DataType;
use crate::error::Result;
use crate::sealed::Sealed;

/// Nanoseconds in one second.
const NANOS_PER_SEC: f64 = 1_000_000_000.0;

/// Generates the shared body of [`TimestampArray`] and [`DurationArray`].
///
/// The two types are identical apart from their `DataType`, their name and the
/// documentation of what the number means, so the implementation is written
/// once and instantiated twice. Writing it out twice by hand would be 400
/// lines of duplicated logic with two places for a bug to hide.
macro_rules! temporal_array {
    ($name:ident, $data_type:expr, $static_type:ident, $doc:literal, $unit_doc:literal) => {
        #[doc = $doc]
        ///
        /// A nanosecond `i64` column; see the [module documentation](self).
        #[derive(Clone)]
        pub struct $name {
            /// The `i64` values, carrying this type's `DataType`.
            inner: Int64Array,
        }

        impl $name {
            /// Builds an array from a values buffer and an optional validity
            /// bitmap.
            ///
            /// # Errors
            ///
            /// [`ValidityLengthMismatch`](crate::DataError::ValidityLengthMismatch)
            /// when the bitmap does not cover exactly one bit per value.
            pub fn try_new(values: ScalarBuffer<i64>, validity: Option<Bitmap>) -> Result<Self> {
                Ok(Self {
                    inner: PrimitiveArray::try_new_with_type($data_type, values, validity)?,
                })
            }

            #[doc = concat!("Builds an array with no nulls from ", $unit_doc, ".")]
            #[must_use]
            pub fn from_nanos(values: impl IntoIterator<Item = i64>) -> Self {
                let values: Vec<i64> = values.into_iter().collect();
                Self {
                    inner: PrimitiveArray::try_new_with_type(
                        $data_type,
                        ScalarBuffer::from_slice(&values),
                        None,
                    )
                    .unwrap_or_else(|_| PrimitiveArray::new_null(0)),
                }
            }

            #[doc = concat!("Builds an array from optional ", $unit_doc, ".")]
            #[must_use]
            pub fn from_opt_nanos(values: impl IntoIterator<Item = Option<i64>>) -> Self {
                let base = Int64Array::from_opt_iter(values);
                let validity = base.validity().cloned();
                Self {
                    inner: PrimitiveArray::try_new_with_type(
                        $data_type,
                        base.values_buffer().clone(),
                        validity,
                    )
                    .unwrap_or_else(|_| PrimitiveArray::new_null(0)),
                }
            }

            /// An all-null array of `len` slots.
            #[must_use]
            pub fn new_null(len: usize) -> Self {
                Self {
                    inner: PrimitiveArray::try_new_with_type(
                        $data_type,
                        ScalarBuffer::zeroed(len),
                        Some(Bitmap::new_unset(len)),
                    )
                    .unwrap_or_else(|_| PrimitiveArray::new_null(len)),
                }
            }

            /// The raw nanosecond values, including the placeholder in every
            /// null slot.
            #[inline]
            #[must_use]
            pub fn values(&self) -> &[i64] {
                self.inner.values()
            }

            /// The values buffer.
            #[inline]
            #[must_use]
            pub const fn values_buffer(&self) -> &ScalarBuffer<i64> {
                self.inner.values_buffer()
            }

            /// The raw slot value at `index`, ignoring validity.
            ///
            /// Returns `None` only when `index` is out of range.
            #[inline]
            #[must_use]
            pub fn value(&self, index: usize) -> Option<i64> {
                self.inner.value(index)
            }

            /// The logical value at `index`: `None` when the slot is null or
            /// out of range.
            #[inline]
            #[must_use]
            pub fn get(&self, index: usize) -> Option<i64> {
                self.inner.get(index)
            }

            /// The value at `index` in seconds, as a float.
            ///
            /// Lossy beyond 2^53 nanoseconds (about 104 days); use
            /// [`Self::get`] when exactness matters.
            #[must_use]
            pub fn value_as_secs_f64(&self, index: usize) -> Option<f64> {
                // The precision loss past 2^53 ns is the documented contract of
                // this accessor, not an oversight: callers who need exactness
                // are pointed at `get`, which returns the raw integer.
                #[allow(clippy::cast_precision_loss)]
                self.get(index).map(|nanos| nanos as f64 / NANOS_PER_SEC)
            }

            /// The value at `index` as a [`std::time::Duration`].
            ///
            /// Returns `None` for a negative value, which `Duration` cannot
            /// represent.
            #[must_use]
            pub fn value_as_duration(&self, index: usize) -> Option<std::time::Duration> {
                let nanos = self.get(index)?;
                u64::try_from(nanos)
                    .ok()
                    .map(std::time::Duration::from_nanos)
            }

            /// Iterates over the logical values, `None` for nulls.
            #[inline]
            pub fn iter(&self) -> ArrayIter<&Self> {
                ArrayIter::new(self)
            }

            /// A zero-copy sub-range, clamped to the array (the crate-wide
            /// slicing convention).
            #[must_use]
            pub fn slice(&self, offset: usize, len: usize) -> Self {
                Self {
                    inner: self.inner.slice(offset, len),
                }
            }

            /// Checked [`Self::slice`].
            ///
            /// # Errors
            ///
            /// [`SliceOutOfBounds`](crate::DataError::SliceOutOfBounds) when
            /// the window leaves the array.
            pub fn try_slice(&self, offset: usize, len: usize) -> Result<Self> {
                Ok(Self {
                    inner: self.inner.try_slice(offset, len)?,
                })
            }

            /// Returns a copy with a different validity bitmap.
            ///
            /// # Errors
            ///
            /// [`ValidityLengthMismatch`](crate::DataError::ValidityLengthMismatch)
            /// when the bitmap length is wrong.
            pub fn with_validity(&self, validity: Option<Bitmap>) -> Result<Self> {
                Ok(Self {
                    inner: self.inner.with_validity(validity)?,
                })
            }

            /// The underlying `i64` column, keeping this type's `DataType`.
            #[inline]
            #[must_use]
            pub const fn as_primitive(&self) -> &Int64Array {
                &self.inner
            }

            /// Reinterprets a plain `Int64` column as this temporal type.
            ///
            /// Free: the two share a buffer layout.
            ///
            /// # Errors
            ///
            /// [`TypeMismatch`](crate::DataError::TypeMismatch) when the
            /// source is not an `Int64`, `Timestamp` or `Duration` column.
            pub fn from_primitive(values: &Int64Array) -> Result<Self> {
                Self::try_new(values.values_buffer().clone(), values.validity().cloned())
            }
        }

        impl Sealed for $name {}

        impl Array for $name {
            fn as_any(&self) -> &dyn Any {
                self
            }

            fn data_type(&self) -> &DataType {
                &$static_type
            }

            fn len(&self) -> usize {
                self.inner.len()
            }

            fn validity(&self) -> Option<&Bitmap> {
                self.inner.validity()
            }

            fn slice(&self, offset: usize, len: usize) -> ArrayRef {
                Arc::new(Self::slice(self, offset, len))
            }

            fn buffer_memory_size(&self) -> usize {
                self.inner.buffer_memory_size()
            }

            fn equals(&self, other: &dyn Array) -> bool {
                other
                    .as_any()
                    .downcast_ref::<Self>()
                    .is_some_and(|other| self.inner.equals(&other.inner))
            }
        }

        impl<'a> ArrayAccessor for &'a $name {
            type Item = i64;

            #[inline]
            fn accessor_len(&self) -> usize {
                self.inner.len()
            }

            #[inline]
            fn accessor_get(&self, index: usize) -> Option<i64> {
                $name::get(self, index)
            }
        }

        impl PartialEq for $name {
            #[inline]
            fn eq(&self, other: &Self) -> bool {
                self.inner == other.inner
            }
        }

        impl Eq for $name {}

        impl FromIterator<i64> for $name {
            fn from_iter<I: IntoIterator<Item = i64>>(iter: I) -> Self {
                Self::from_nanos(iter)
            }
        }

        impl FromIterator<Option<i64>> for $name {
            fn from_iter<I: IntoIterator<Item = Option<i64>>>(iter: I) -> Self {
                Self::from_opt_nanos(iter)
            }
        }

        impl<'a> IntoIterator for &'a $name {
            type Item = Option<i64>;
            type IntoIter = ArrayIter<&'a $name>;

            fn into_iter(self) -> Self::IntoIter {
                self.iter()
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                debug_array(
                    f,
                    stringify!($name),
                    &$static_type,
                    self.len(),
                    self.null_count(),
                    self.iter(),
                )
            }
        }
    };
}

temporal_array!(
    TimestampArray,
    DataType::Timestamp,
    TIMESTAMP_TYPE,
    "Nanoseconds since the Unix epoch, timezone-less.",
    "nanosecond instants"
);

temporal_array!(
    DurationArray,
    DataType::Duration,
    DURATION_TYPE,
    "A nanosecond interval count.",
    "nanosecond intervals"
);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::DataError;
    use crate::array::IntoArrayRef;

    #[test]
    fn empty_arrays() {
        let stamps = TimestampArray::from_nanos([]);
        assert!(stamps.is_empty());
        assert_eq!(stamps.get(0), None);
        assert_eq!(stamps.data_type(), &DataType::Timestamp);

        let gaps = DurationArray::from_nanos([]);
        assert!(gaps.is_empty());
        assert_eq!(gaps.data_type(), &DataType::Duration);
    }

    #[test]
    fn no_null_arrays() {
        let stamps = TimestampArray::from_nanos([0, 1_000_000_000, -1_000_000_000]);
        assert_eq!(stamps.len(), 3);
        assert_eq!(stamps.null_count(), 0);
        assert!(stamps.validity().is_none());
        assert_eq!(stamps.get(0), Some(0));
        assert_eq!(stamps.get(1), Some(1_000_000_000));
        assert_eq!(stamps.get(3), None);
        assert_eq!(stamps.values(), &[0, 1_000_000_000, -1_000_000_000]);
    }

    #[test]
    fn all_null_arrays() {
        let stamps = TimestampArray::new_null(3);
        assert_eq!(stamps.len(), 3);
        assert_eq!(stamps.null_count(), 3);
        for index in 0..3 {
            assert!(stamps.is_null(index));
            assert_eq!(stamps.get(index), None);
            assert_eq!(stamps.value(index), Some(0));
        }

        let gaps = DurationArray::new_null(2);
        assert_eq!(gaps.null_count(), 2);
        assert_eq!(gaps.data_type(), &DataType::Duration);
    }

    #[test]
    fn mixed_nulls() {
        let stamps = TimestampArray::from_opt_nanos([Some(1), None, Some(3)]);
        assert_eq!(stamps.len(), 3);
        assert_eq!(stamps.null_count(), 1);
        assert_eq!(stamps.data_type(), &DataType::Timestamp);
        assert_eq!(
            stamps.iter().collect::<Vec<_>>(),
            vec![Some(1), None, Some(3)]
        );
        assert_eq!(stamps.value_as_secs_f64(1), None);
    }

    #[test]
    fn seconds_and_std_duration_conversions() {
        let gaps = DurationArray::from_nanos([1_500_000_000, 0, -1, 500_000_000]);
        assert_eq!(gaps.value_as_secs_f64(0), Some(1.5));
        assert_eq!(gaps.value_as_secs_f64(1), Some(0.0));
        assert_eq!(gaps.value_as_secs_f64(2), Some(-1e-9));
        assert_eq!(gaps.value_as_secs_f64(9), None);
        assert_eq!(
            gaps.value_as_duration(0),
            Some(std::time::Duration::from_millis(1_500))
        );
        assert_eq!(
            gaps.value_as_duration(3),
            Some(std::time::Duration::from_millis(500))
        );
        assert_eq!(gaps.value_as_duration(2), None, "negative has no Duration");
        assert_eq!(gaps.value_as_duration(9), None);
    }

    #[test]
    fn slicing_preserves_the_data_type() {
        let stamps = TimestampArray::from_opt_nanos((0..20).map(|i| (i % 4 != 0).then_some(i)));
        let window = stamps.slice(5, 10);
        assert_eq!(window.len(), 10);
        assert_eq!(window.data_type(), &DataType::Timestamp);
        for index in 0..10 {
            assert_eq!(window.get(index), stamps.get(index + 5));
        }
        assert_eq!(stamps.slice(18, 99).len(), 2);
        assert_eq!(stamps.slice(99, 1).len(), 0);
        assert!(stamps.try_slice(19, 2).is_err());
        assert_eq!(stamps.try_slice(1, 2).unwrap().len(), 2);
    }

    #[test]
    fn timestamps_never_equal_durations() {
        let stamps: ArrayRef = TimestampArray::from_nanos([1, 2]).into_array_ref();
        let gaps: ArrayRef = DurationArray::from_nanos([1, 2]).into_array_ref();
        let plain: ArrayRef = Int64Array::from_values([1, 2]).into_array_ref();
        assert_ne!(stamps, gaps);
        assert_ne!(stamps, plain);
        assert_ne!(gaps, plain);
        assert_eq!(stamps, TimestampArray::from_nanos([1, 2]).into_array_ref());
    }

    #[test]
    fn primitive_conversions_are_free() {
        let plain = Int64Array::from_opt_iter([Some(1), None, Some(3)]);
        let stamps = TimestampArray::from_primitive(&plain).unwrap();
        assert_eq!(stamps.data_type(), &DataType::Timestamp);
        assert_eq!(stamps.null_count(), 1);
        assert_eq!(stamps.get(2), Some(3));
        assert_eq!(stamps.as_primitive().data_type(), &DataType::Timestamp);

        let gaps = DurationArray::from_primitive(&plain).unwrap();
        assert_eq!(gaps.data_type(), &DataType::Duration);
    }

    #[test]
    fn try_new_checks_validity_length() {
        let values = ScalarBuffer::from_slice(&[1i64, 2]);
        assert!(TimestampArray::try_new(values.clone(), None).is_ok());
        assert!(TimestampArray::try_new(values.clone(), Some(Bitmap::new_set(2))).is_ok());
        assert!(matches!(
            TimestampArray::try_new(values, Some(Bitmap::new_set(3))),
            Err(DataError::ValidityLengthMismatch { .. })
        ));
    }

    #[test]
    fn with_validity_replaces_the_bitmap() {
        let stamps = TimestampArray::from_nanos([1, 2]);
        let masked = stamps
            .with_validity(Some([true, false].into_iter().collect()))
            .unwrap();
        assert_eq!(masked.null_count(), 1);
        assert_eq!(masked.data_type(), &DataType::Timestamp);
        assert!(stamps.with_validity(Some(Bitmap::new_set(9))).is_err());
    }

    #[test]
    fn collects_from_iterators() {
        let stamps: TimestampArray = (0i64..3).collect();
        assert_eq!(stamps.len(), 3);
        let gaps: DurationArray = [Some(1i64), None].into_iter().collect();
        assert_eq!(gaps.null_count(), 1);
        assert_eq!((&stamps).into_iter().count(), 3);
    }

    #[test]
    fn debug_output_names_the_type() {
        let rendered = format!("{:?}", TimestampArray::from_opt_nanos([Some(1), None]));
        assert!(rendered.contains("TimestampArray"), "{rendered}");
        assert!(rendered.contains("Timestamp(ns)"), "{rendered}");
        assert!(rendered.contains("nulls=1"), "{rendered}");
        assert!(format!("{:?}", DurationArray::from_nanos([1])).contains("Duration(ns)"));
    }

    #[test]
    fn extreme_values_round_trip() {
        let stamps = TimestampArray::from_nanos([i64::MIN, i64::MAX, 0]);
        assert_eq!(stamps.get(0), Some(i64::MIN));
        assert_eq!(stamps.get(1), Some(i64::MAX));
        assert_eq!(stamps.value_as_duration(0), None);
        assert!(stamps.value_as_secs_f64(1).unwrap_or(0.0) > 9e9);
    }

    #[test]
    fn buffer_memory_size_is_reported() {
        let stamps = TimestampArray::from_nanos(0..1_000);
        assert!(stamps.buffer_memory_size() >= 8_000);
    }
}
