//! [`PrimitiveArray`] — the fixed-width scalar column, generic over
//! [`ArrowNativeType`].
//!
//! One implementation covers eleven Arrow types (`Int8..Int64`,
//! `UInt8..UInt64`, `Float16/32/64`) plus the two nanosecond temporal types,
//! which reuse the `i64` layout through
//! [`PrimitiveArray::try_new_with_type`]. Layout is the Arrow standard: an
//! optional validity bitmap and a tightly packed values buffer, both sliced by
//! window rather than copied.
//!
//! ```
//! use astrs_data::array::{Array, Int32Array};
//!
//! let values = Int32Array::from_opt_iter([Some(1), None, Some(3)]);
//! assert_eq!(values.len(), 3);
//! assert_eq!(values.null_count(), 1);
//! assert_eq!(values.get(0), Some(1));
//! assert_eq!(values.get(1), None);
//! assert_eq!(values.values(), &[1, 0, 3], "null slots hold the default value");
//!
//! let tail = values.slice(1, 2);
//! assert_eq!(tail.iter().collect::<Vec<_>>(), vec![None, Some(3)]);
//! ```

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use crate::array::iter::{ArrayAccessor, ArrayIter};
use crate::array::{Array, ArrayRef, check_validity, clamp_window, debug_array, validity_eq};
use crate::buffer::{Bitmap, ScalarBuffer};
use crate::datatype::{ArrowNativeType, DataType, F16};
use crate::error::{DataError, Result};
use crate::sealed::Sealed;

/// A column of fixed-width scalars with an optional validity bitmap.
///
/// See the [module documentation](self) for the layout.
#[derive(Clone)]
pub struct PrimitiveArray<T: ArrowNativeType> {
    /// Declared type. Normally `T::DATA_TYPE`, but `Timestamp` and `Duration`
    /// reuse the `i64` layout with their own type.
    data_type: DataType,
    /// The values. Its element count *is* the array's length.
    values: ScalarBuffer<T>,
    /// Validity, or `None` when every slot is valid.
    validity: Option<Bitmap>,
}

/// `Int8` column.
pub type Int8Array = PrimitiveArray<i8>;
/// `Int16` column.
pub type Int16Array = PrimitiveArray<i16>;
/// `Int32` column.
pub type Int32Array = PrimitiveArray<i32>;
/// `Int64` column.
pub type Int64Array = PrimitiveArray<i64>;
/// `UInt8` column.
pub type UInt8Array = PrimitiveArray<u8>;
/// `UInt16` column.
pub type UInt16Array = PrimitiveArray<u16>;
/// `UInt32` column.
pub type UInt32Array = PrimitiveArray<u32>;
/// `UInt64` column.
pub type UInt64Array = PrimitiveArray<u64>;
/// `Float16` column, stored as [`F16`].
pub type Float16Array = PrimitiveArray<F16>;
/// `Float32` column.
pub type Float32Array = PrimitiveArray<f32>;
/// `Float64` column.
pub type Float64Array = PrimitiveArray<f64>;

impl<T: ArrowNativeType> PrimitiveArray<T> {
    /// Builds an array from a values buffer and an optional validity bitmap.
    ///
    /// # Errors
    ///
    /// [`DataError::ValidityLengthMismatch`] when the bitmap does not cover
    /// exactly one bit per value.
    ///
    /// ```
    /// use astrs_data::array::{Array, Int32Array};
    /// use astrs_data::{Bitmap, ScalarBuffer};
    ///
    /// let values = ScalarBuffer::from_slice(&[1, 2, 3]);
    /// let validity: Bitmap = [true, false, true].into_iter().collect();
    /// let array = Int32Array::try_new(values, Some(validity))?;
    /// assert_eq!(array.null_count(), 1);
    /// # Ok::<(), astrs_data::DataError>(())
    /// ```
    pub fn try_new(values: ScalarBuffer<T>, validity: Option<Bitmap>) -> Result<Self> {
        Self::try_new_with_type(T::DATA_TYPE, values, validity)
    }

    /// Builds an array that reports a data type other than `T::DATA_TYPE`.
    ///
    /// Only the types whose value width matches `T` are accepted, which is how
    /// [`crate::array::TimestampArray`] and [`crate::array::DurationArray`]
    /// reuse the `i64` layout.
    ///
    /// # Errors
    ///
    /// * [`DataError::TypeMismatch`] when `data_type`'s value width differs
    ///   from `T`'s.
    /// * [`DataError::ValidityLengthMismatch`] when the bitmap length is wrong.
    pub fn try_new_with_type(
        data_type: DataType,
        values: ScalarBuffer<T>,
        validity: Option<Bitmap>,
    ) -> Result<Self> {
        if data_type.primitive_width() != Some(T::WIDTH) || !data_type.is_primitive() {
            return Err(DataError::type_mismatch(T::DATA_TYPE, data_type));
        }
        check_validity(validity.as_ref(), values.len())?;
        Ok(Self {
            data_type,
            values,
            validity,
        })
    }

    /// Builds an array with no nulls.
    ///
    /// ```
    /// use astrs_data::array::{Array, Float64Array};
    ///
    /// let array = Float64Array::from_values([1.0, 2.5]);
    /// assert_eq!(array.null_count(), 0);
    /// assert!(array.validity().is_none());
    /// ```
    #[must_use]
    pub fn from_values(values: impl IntoIterator<Item = T>) -> Self {
        let values: Vec<T> = values.into_iter().collect();
        Self {
            data_type: T::DATA_TYPE,
            values: ScalarBuffer::from_slice(&values),
            validity: None,
        }
    }

    /// Builds an array from optional values, null slots holding `T::default()`.
    #[must_use]
    pub fn from_opt_iter(values: impl IntoIterator<Item = Option<T>>) -> Self {
        let iter = values.into_iter();
        let (lower, _) = iter.size_hint();
        let mut slots = Vec::with_capacity(lower);
        let mut validity = crate::buffer::BitmapBuilder::with_capacity(lower);
        let mut any_null = false;
        for value in iter {
            match value {
                Some(value) => {
                    slots.push(value);
                    validity.append(true);
                }
                None => {
                    slots.push(T::default());
                    validity.append(false);
                    any_null = true;
                }
            }
        }
        Self {
            data_type: T::DATA_TYPE,
            values: ScalarBuffer::from_slice(&slots),
            validity: any_null.then(|| validity.finish()),
        }
    }

    /// An all-null array of `len` slots.
    #[must_use]
    pub fn new_null(len: usize) -> Self {
        Self {
            data_type: T::DATA_TYPE,
            values: ScalarBuffer::zeroed(len),
            validity: Some(Bitmap::new_unset(len)),
        }
    }

    /// Assembles the parts without revalidating.
    ///
    /// Safe, and crate-internal: [`crate::builder::PrimitiveBuilder`] fixes the
    /// data type at construction and keeps the validity bitmap in step with
    /// the values, so revalidating on every `finish` would be pure overhead.
    pub(crate) fn from_parts(
        data_type: DataType,
        values: ScalarBuffer<T>,
        validity: Option<Bitmap>,
    ) -> Self {
        debug_assert_eq!(data_type.primitive_width(), Some(T::WIDTH));
        debug_assert!(
            validity
                .as_ref()
                .is_none_or(|bits| bits.len() == values.len())
        );
        Self {
            data_type,
            values,
            validity,
        }
    }

    /// The raw values, including the placeholder in every null slot.
    ///
    /// This is the SIMD-ready view the blueprint's zero-copy path exists for:
    /// the slice starts at the array's window and is 64-byte aligned whenever
    /// the window starts on an alignment boundary.
    #[inline]
    #[must_use]
    pub fn values(&self) -> &[T] {
        self.values.as_slice()
    }

    /// The values buffer.
    #[inline]
    #[must_use]
    pub const fn values_buffer(&self) -> &ScalarBuffer<T> {
        &self.values
    }

    /// The raw slot value at `index`, ignoring validity.
    ///
    /// Returns `None` only when `index` is out of range.
    #[inline]
    #[must_use]
    pub fn value(&self, index: usize) -> Option<T> {
        self.values.get(index)
    }

    /// The logical value at `index`: `None` when the slot is null or out of
    /// range.
    #[inline]
    #[must_use]
    pub fn get(&self, index: usize) -> Option<T> {
        if self.is_valid_index(index) {
            self.values.get(index)
        } else {
            None
        }
    }

    /// Iterates over the logical values, `None` for nulls.
    #[inline]
    pub fn iter(&self) -> ArrayIter<&Self> {
        ArrayIter::new(self)
    }

    /// Iterates over the non-null values, skipping nulls entirely.
    pub fn values_iter(&self) -> impl Iterator<Item = T> + '_ {
        self.iter().flatten()
    }

    /// A zero-copy sub-range, clamped to the array (the crate-wide slicing
    /// convention).
    #[must_use]
    pub fn slice(&self, offset: usize, len: usize) -> Self {
        let (offset, len) = clamp_window(self.len(), offset, len);
        Self {
            data_type: self.data_type.clone(),
            values: self.values.slice(offset, len),
            validity: self.validity.as_ref().map(|bits| bits.slice(offset, len)),
        }
    }

    /// Checked [`PrimitiveArray::slice`].
    ///
    /// # Errors
    ///
    /// [`DataError::SliceOutOfBounds`] when the window leaves the array.
    pub fn try_slice(&self, offset: usize, len: usize) -> Result<Self> {
        if offset.saturating_add(len) > self.len() {
            return Err(DataError::SliceOutOfBounds {
                offset,
                len,
                available: self.len(),
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
        check_validity(validity.as_ref(), self.len())?;
        Ok(Self {
            data_type: self.data_type.clone(),
            values: self.values.clone(),
            validity,
        })
    }

    /// Whether `index` is inside the array and not null.
    #[inline]
    fn is_valid_index(&self, index: usize) -> bool {
        index < self.values.len() && self.validity.as_ref().is_none_or(|bits| bits.value(index))
    }
}

impl<T: ArrowNativeType> Sealed for PrimitiveArray<T> {}

impl<T: ArrowNativeType> Array for PrimitiveArray<T> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn data_type(&self) -> &DataType {
        &self.data_type
    }

    fn len(&self) -> usize {
        self.values.len()
    }

    fn validity(&self) -> Option<&Bitmap> {
        self.validity.as_ref()
    }

    fn slice(&self, offset: usize, len: usize) -> ArrayRef {
        Arc::new(Self::slice(self, offset, len))
    }

    fn buffer_memory_size(&self) -> usize {
        self.values.inner().backing_len()
            + self
                .validity
                .as_ref()
                .map_or(0, |bits| bits.buffer().backing_len())
    }

    fn equals(&self, other: &dyn Array) -> bool {
        let Some(other) = other.as_any().downcast_ref::<Self>() else {
            return false;
        };
        if self.data_type != other.data_type || self.len() != other.len() {
            return false;
        }
        if !validity_eq(self.validity.as_ref(), other.validity.as_ref(), self.len()) {
            return false;
        }
        // Null slots may hold arbitrary bytes, so only valid slots count.
        (0..self.len()).all(|index| match (self.get(index), other.get(index)) {
            (Some(a), Some(b)) => a.value_eq(&b),
            (None, None) => true,
            _ => false,
        })
    }
}

impl<T: ArrowNativeType> ArrayAccessor for &PrimitiveArray<T> {
    type Item = T;

    #[inline]
    fn accessor_len(&self) -> usize {
        self.values.len()
    }

    #[inline]
    fn accessor_get(&self, index: usize) -> Option<T> {
        PrimitiveArray::get(self, index)
    }
}

impl<T: ArrowNativeType> PartialEq for PrimitiveArray<T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.equals(other)
    }
}

impl<T: ArrowNativeType> FromIterator<T> for PrimitiveArray<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self::from_values(iter)
    }
}

impl<T: ArrowNativeType> FromIterator<Option<T>> for PrimitiveArray<T> {
    fn from_iter<I: IntoIterator<Item = Option<T>>>(iter: I) -> Self {
        Self::from_opt_iter(iter)
    }
}

impl<T: ArrowNativeType, const N: usize> From<[T; N]> for PrimitiveArray<T> {
    fn from(values: [T; N]) -> Self {
        Self::from_values(values)
    }
}

impl<T: ArrowNativeType> From<Vec<T>> for PrimitiveArray<T> {
    fn from(values: Vec<T>) -> Self {
        Self::from_values(values)
    }
}

impl<'a, T: ArrowNativeType> IntoIterator for &'a PrimitiveArray<T> {
    type Item = Option<T>;
    type IntoIter = ArrayIter<&'a PrimitiveArray<T>>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<T: ArrowNativeType> fmt::Debug for PrimitiveArray<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        debug_array(
            f,
            "PrimitiveArray",
            &self.data_type,
            self.len(),
            self.null_count(),
            self.iter(),
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::ALIGNMENT;

    #[test]
    fn empty_array() {
        let array = Int32Array::from_values([] as [i32; 0]);
        assert!(array.is_empty());
        assert_eq!(array.len(), 0);
        assert_eq!(array.null_count(), 0);
        assert_eq!(array.values(), &[] as &[i32]);
        assert_eq!(array.get(0), None);
        assert_eq!(array.value(0), None);
        assert!(array.is_null(0), "out of range reads as null");
        assert_eq!(array.iter().count(), 0);
        assert_eq!(array.data_type(), &DataType::Int32);
    }

    #[test]
    fn no_null_array() {
        let array = Int64Array::from_values([10, 20, 30]);
        assert_eq!(array.len(), 3);
        assert_eq!(array.null_count(), 0);
        assert!(array.validity().is_none());
        assert_eq!(array.values(), &[10, 20, 30]);
        for index in 0..3 {
            assert!(array.is_valid(index));
            assert_eq!(array.get(index), Some((index as i64 + 1) * 10));
        }
        assert_eq!(array.get(3), None);
        assert_eq!(
            array.iter().collect::<Vec<_>>(),
            vec![Some(10), Some(20), Some(30)]
        );
    }

    #[test]
    fn all_null_array() {
        let array = UInt16Array::new_null(4);
        assert_eq!(array.len(), 4);
        assert_eq!(array.null_count(), 4);
        assert_eq!(array.values(), &[0, 0, 0, 0]);
        for index in 0..4 {
            assert!(array.is_null(index));
            assert_eq!(array.get(index), None);
            assert_eq!(array.value(index), Some(0), "raw slot is still readable");
        }
        assert_eq!(array.iter().flatten().count(), 0);
    }

    #[test]
    fn mixed_nulls() {
        let array = Float64Array::from_opt_iter([Some(1.5), None, Some(-2.0), None]);
        assert_eq!(array.len(), 4);
        assert_eq!(array.null_count(), 2);
        assert_eq!(array.get(0), Some(1.5));
        assert_eq!(array.get(1), None);
        assert_eq!(array.value(1), Some(0.0), "null slot holds the default");
        assert_eq!(array.values_iter().collect::<Vec<_>>(), vec![1.5, -2.0]);
    }

    #[test]
    fn from_opt_iter_without_nulls_skips_the_bitmap() {
        let array = Int8Array::from_opt_iter([Some(1), Some(2)]);
        assert!(
            array.validity().is_none(),
            "no nulls means no validity buffer"
        );
        assert_eq!(array.null_count(), 0);
    }

    #[test]
    fn slicing_is_zero_copy_and_clamping() {
        let array = Int32Array::from_opt_iter((0..20).map(|i| (i % 3 != 0).then_some(i)));
        let window = array.slice(5, 10);
        assert_eq!(window.len(), 10);
        for index in 0..10 {
            assert_eq!(window.get(index), array.get(index + 5));
        }
        assert_eq!(array.slice(18, 99).len(), 2);
        assert_eq!(array.slice(99, 5).len(), 0);
        assert_eq!(array.slice(0, 0).len(), 0);

        // The window points into the same allocation.
        assert_eq!(
            window.values_buffer().inner().backing_len(),
            array.values_buffer().inner().backing_len()
        );
    }

    #[test]
    fn nested_slices_compose() {
        let array = Int32Array::from_values(0..32);
        let a = array.slice(4, 20);
        let b = a.slice(6, 5);
        assert_eq!(b.values(), &[10, 11, 12, 13, 14]);
    }

    #[test]
    fn try_slice_reports_out_of_range() {
        let array = Int32Array::from_values([1, 2, 3]);
        assert_eq!(array.try_slice(1, 2).unwrap().len(), 2);
        assert_eq!(
            array.try_slice(2, 2).unwrap_err(),
            DataError::SliceOutOfBounds {
                offset: 2,
                len: 2,
                available: 3
            }
        );
    }

    #[test]
    fn values_are_64_byte_aligned() {
        for len in [1usize, 3, 16, 100, 1000] {
            let array = Int64Array::from_values((0..len).map(|i| i as i64));
            assert_eq!(
                array.values().as_ptr() as usize % ALIGNMENT,
                0,
                "len {len} lost the alignment"
            );
        }
    }

    #[test]
    fn validity_length_is_checked() {
        let values = ScalarBuffer::from_slice(&[1i32, 2, 3]);
        assert!(Int32Array::try_new(values.clone(), None).is_ok());
        assert!(Int32Array::try_new(values.clone(), Some(Bitmap::new_set(3))).is_ok());
        assert_eq!(
            Int32Array::try_new(values, Some(Bitmap::new_set(2))).unwrap_err(),
            DataError::ValidityLengthMismatch {
                array_len: 3,
                validity_len: 2
            }
        );
    }

    #[test]
    fn with_validity_replaces_the_bitmap() {
        let array = Int32Array::from_values([1, 2, 3]);
        let masked = array
            .with_validity(Some([true, false, true].into_iter().collect()))
            .unwrap();
        assert_eq!(masked.null_count(), 1);
        assert_eq!(array.null_count(), 0, "the original is untouched");
        assert!(array.with_validity(Some(Bitmap::new_set(9))).is_err());
        assert_eq!(masked.with_validity(None).unwrap().null_count(), 0);
    }

    #[test]
    fn typed_constructor_accepts_matching_widths_only() {
        let values = ScalarBuffer::from_slice(&[1i64, 2]);
        assert!(Int64Array::try_new_with_type(DataType::Timestamp, values.clone(), None).is_ok());
        assert!(Int64Array::try_new_with_type(DataType::Duration, values.clone(), None).is_ok());
        assert!(Int64Array::try_new_with_type(DataType::Float64, values.clone(), None).is_ok());
        assert!(Int64Array::try_new_with_type(DataType::Int32, values.clone(), None).is_err());
        assert!(Int64Array::try_new_with_type(DataType::Utf8, values, None).is_err());
    }

    #[test]
    fn equality_ignores_null_slot_contents() {
        let a = Int32Array::from_opt_iter([Some(1), None, Some(3)]);
        let b = Int32Array::try_new(
            ScalarBuffer::from_slice(&[1, 999, 3]),
            Some([true, false, true].into_iter().collect()),
        )
        .unwrap();
        assert_eq!(a, b, "the garbage in the null slot must not matter");
        assert_ne!(a, Int32Array::from_opt_iter([Some(1), Some(2), Some(3)]));
        assert_ne!(a, Int32Array::from_opt_iter([Some(1), None]));
    }

    #[test]
    fn equality_treats_absent_validity_as_all_valid() {
        let bare = Int32Array::from_values([1, 2]);
        let masked = Int32Array::try_new(
            ScalarBuffer::from_slice(&[1, 2]),
            Some([true, true].into_iter().collect()),
        )
        .unwrap();
        assert_eq!(bare, masked);
    }

    #[test]
    fn equality_across_data_types_is_false() {
        let stamps = Int64Array::try_new_with_type(
            DataType::Timestamp,
            ScalarBuffer::from_slice(&[1, 2]),
            None,
        )
        .unwrap();
        assert_ne!(stamps, Int64Array::from_values([1, 2]));
    }

    #[test]
    fn nan_arrays_are_equal_to_themselves() {
        let array = Float32Array::from_values([f32::NAN, 1.0]);
        assert_eq!(array, array.clone());
        assert_eq!(
            Float64Array::from_values([f64::NAN]),
            Float64Array::from_values([f64::NAN])
        );
        assert_eq!(
            Float16Array::from_values([F16::NAN]),
            Float16Array::from_values([F16::NAN])
        );
        assert_eq!(
            Float32Array::from_values([0.0]),
            Float32Array::from_values([-0.0]),
            "IEEE signed zeros still compare equal"
        );
    }

    #[test]
    fn sliced_arrays_compare_by_logical_content() {
        let full = Int32Array::from_opt_iter([Some(9), Some(1), None, Some(3), Some(9)]);
        let window = full.slice(1, 3);
        assert_eq!(window, Int32Array::from_opt_iter([Some(1), None, Some(3)]));
    }

    #[test]
    fn float16_columns_round_trip() {
        let array = Float16Array::from_opt_iter([
            Some(F16::from_f32(1.5)),
            None,
            Some(F16::from_f32(-0.25)),
        ]);
        assert_eq!(array.data_type(), &DataType::Float16);
        assert_eq!(array.get(0).map(F16::to_f32), Some(1.5));
        assert_eq!(array.get(1), None);
        assert_eq!(array.get(2).map(F16::to_f32), Some(-0.25));
        assert_eq!(array.values().len(), 3);
    }

    #[test]
    fn every_native_width_builds_an_array() {
        assert_eq!(Int8Array::from_values([1i8]).data_type(), &DataType::Int8);
        assert_eq!(
            Int16Array::from_values([1i16]).data_type(),
            &DataType::Int16
        );
        assert_eq!(
            Int32Array::from_values([1i32]).data_type(),
            &DataType::Int32
        );
        assert_eq!(
            Int64Array::from_values([1i64]).data_type(),
            &DataType::Int64
        );
        assert_eq!(UInt8Array::from_values([1u8]).data_type(), &DataType::UInt8);
        assert_eq!(
            UInt16Array::from_values([1u16]).data_type(),
            &DataType::UInt16
        );
        assert_eq!(
            UInt32Array::from_values([1u32]).data_type(),
            &DataType::UInt32
        );
        assert_eq!(
            UInt64Array::from_values([1u64]).data_type(),
            &DataType::UInt64
        );
        assert_eq!(
            Float32Array::from_values([1.0f32]).data_type(),
            &DataType::Float32
        );
        assert_eq!(
            Float64Array::from_values([1.0f64]).data_type(),
            &DataType::Float64
        );
    }

    #[test]
    fn conversions_and_collect() {
        let from_array: Int32Array = [1, 2, 3].into();
        assert_eq!(from_array.len(), 3);
        let from_vec: Int32Array = vec![1, 2].into();
        assert_eq!(from_vec.len(), 2);
        let collected: Int32Array = (0..4).collect();
        assert_eq!(collected.values(), &[0, 1, 2, 3]);
        let opt_collected: Int32Array = [Some(1), None].into_iter().collect();
        assert_eq!(opt_collected.null_count(), 1);
        assert_eq!((&collected).into_iter().count(), 4);
    }

    #[test]
    fn buffer_memory_size_counts_both_buffers() {
        let bare = Int64Array::from_values([1, 2, 3]);
        assert!(bare.buffer_memory_size() >= 24);
        let masked = bare
            .with_validity(Some([true, false, true].into_iter().collect()))
            .unwrap();
        assert!(masked.buffer_memory_size() > bare.buffer_memory_size());
    }

    #[test]
    fn debug_output_is_readable() {
        let array = Int32Array::from_opt_iter([Some(1), None, Some(3)]);
        let rendered = format!("{array:?}");
        assert!(rendered.contains("Int32"), "{rendered}");
        assert!(rendered.contains("len=3"), "{rendered}");
        assert!(rendered.contains("nulls=1"), "{rendered}");
        assert!(rendered.contains("null"), "{rendered}");

        let long = Int32Array::from_values(0..50);
        assert!(format!("{long:?}").contains('…'));
    }

    #[test]
    fn trait_object_slice_matches_the_inherent_one() {
        let array = Int32Array::from_values([1, 2, 3, 4]);
        let sliced = Array::slice(&array, 1, 2);
        assert_eq!(sliced.len(), 2);
        assert_eq!(
            sliced
                .as_any()
                .downcast_ref::<Int32Array>()
                .map(Int32Array::values),
            Some(&[2, 3][..])
        );
    }
}
