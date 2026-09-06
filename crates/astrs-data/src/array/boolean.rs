//! [`BooleanArray`] — the bit-packed `Bool` column.
//!
//! Unlike every other fixed-width type, `Bool` stores one *bit* per value, so
//! its values buffer is a [`Bitmap`] rather than a [`crate::ScalarBuffer`].
//! Both buffers — values and validity — use the same LSB-first numbering, and
//! both slice by bit offset, so slicing a boolean column stays free.
//!
//! ```
//! use astrs_data::array::{Array, BooleanArray};
//!
//! let flags = BooleanArray::from_opt_iter([Some(true), None, Some(false)]);
//! assert_eq!(flags.len(), 3);
//! assert_eq!(flags.null_count(), 1);
//! assert_eq!(flags.get(0), Some(true));
//! assert_eq!(flags.get(1), None);
//! assert_eq!(flags.true_count(), 1);
//! ```

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use crate::array::iter::{ArrayAccessor, ArrayIter};
use crate::array::{
    Array, ArrayRef, BOOL_TYPE, check_validity, clamp_window, debug_array, validity_eq,
};
use crate::buffer::{Bitmap, BitmapBuilder};
use crate::datatype::DataType;
use crate::error::{DataError, Result};
use crate::sealed::Sealed;

/// A bit-packed column of booleans with an optional validity bitmap.
#[derive(Clone)]
pub struct BooleanArray {
    /// One bit per value. Its length *is* the array's length.
    values: Bitmap,
    /// Validity, or `None` when every slot is valid.
    validity: Option<Bitmap>,
}

impl BooleanArray {
    /// Builds an array from a values bitmap and an optional validity bitmap.
    ///
    /// # Errors
    ///
    /// [`DataError::ValidityLengthMismatch`] when the two bitmaps disagree on
    /// length.
    ///
    /// ```
    /// use astrs_data::array::{Array, BooleanArray};
    /// use astrs_data::Bitmap;
    ///
    /// let values: Bitmap = [true, false, true].into_iter().collect();
    /// let validity: Bitmap = [true, true, false].into_iter().collect();
    /// let flags = BooleanArray::new(values, Some(validity))?;
    /// assert_eq!(flags.null_count(), 1);
    /// # Ok::<(), astrs_data::DataError>(())
    /// ```
    pub fn new(values: Bitmap, validity: Option<Bitmap>) -> Result<Self> {
        check_validity(validity.as_ref(), values.len())?;
        Ok(Self { values, validity })
    }

    /// Builds an array with no nulls.
    #[must_use]
    pub fn from_values(values: impl IntoIterator<Item = bool>) -> Self {
        Self {
            values: values.into_iter().collect(),
            validity: None,
        }
    }

    /// Builds an array from optional values, null slots holding `false`.
    #[must_use]
    pub fn from_opt_iter(values: impl IntoIterator<Item = Option<bool>>) -> Self {
        let iter = values.into_iter();
        let (lower, _) = iter.size_hint();
        let mut slots = BitmapBuilder::with_capacity(lower);
        let mut validity = BitmapBuilder::with_capacity(lower);
        let mut any_null = false;
        for value in iter {
            slots.append(value.unwrap_or(false));
            validity.append(value.is_some());
            any_null |= value.is_none();
        }
        Self {
            values: slots.finish(),
            validity: any_null.then(|| validity.finish()),
        }
    }

    /// An all-null array of `len` slots.
    #[must_use]
    pub fn new_null(len: usize) -> Self {
        Self {
            values: Bitmap::new_unset(len),
            validity: Some(Bitmap::new_unset(len)),
        }
    }

    /// The values bitmap, including the placeholder in every null slot.
    #[inline]
    #[must_use]
    pub const fn values(&self) -> &Bitmap {
        &self.values
    }

    /// The raw slot value at `index`, ignoring validity.
    ///
    /// Returns `None` only when `index` is out of range.
    #[inline]
    #[must_use]
    pub fn value(&self, index: usize) -> Option<bool> {
        self.values.get(index)
    }

    /// The logical value at `index`: `None` when the slot is null or out of
    /// range.
    #[inline]
    #[must_use]
    pub fn get(&self, index: usize) -> Option<bool> {
        if self.is_valid_index(index) {
            self.values.get(index)
        } else {
            None
        }
    }

    /// Number of non-null slots holding `true`.
    ///
    /// ```
    /// use astrs_data::array::BooleanArray;
    ///
    /// let flags = BooleanArray::from_opt_iter([Some(true), None, Some(true), Some(false)]);
    /// assert_eq!(flags.true_count(), 2);
    /// assert_eq!(flags.false_count(), 1);
    /// ```
    #[must_use]
    pub fn true_count(&self) -> usize {
        match &self.validity {
            None => self.values.count_set(),
            Some(validity) => match self.values.and(validity) {
                Ok(both) => both.count_set(),
                // Unreachable: the constructor guarantees equal lengths.
                Err(_) => 0,
            },
        }
    }

    /// Number of non-null slots holding `false`.
    #[must_use]
    pub fn false_count(&self) -> usize {
        self.len() - self.null_count() - self.true_count()
    }

    /// Iterates over the logical values, `None` for nulls.
    #[inline]
    pub fn iter(&self) -> ArrayIter<&Self> {
        ArrayIter::new(self)
    }

    /// A zero-copy sub-range, clamped to the array (the crate-wide slicing
    /// convention).
    #[must_use]
    pub fn slice(&self, offset: usize, len: usize) -> Self {
        let (offset, len) = clamp_window(self.len(), offset, len);
        Self {
            values: self.values.slice(offset, len),
            validity: self.validity.as_ref().map(|bits| bits.slice(offset, len)),
        }
    }

    /// Checked [`BooleanArray::slice`].
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
        Self::new(self.values.clone(), validity)
    }

    /// Whether `index` is inside the array and not null.
    #[inline]
    fn is_valid_index(&self, index: usize) -> bool {
        index < self.values.len() && self.validity.as_ref().is_none_or(|bits| bits.value(index))
    }
}

impl Sealed for BooleanArray {}

impl Array for BooleanArray {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn data_type(&self) -> &DataType {
        &BOOL_TYPE
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
        self.values.buffer().backing_len()
            + self
                .validity
                .as_ref()
                .map_or(0, |bits| bits.buffer().backing_len())
    }

    fn equals(&self, other: &dyn Array) -> bool {
        let Some(other) = other.as_any().downcast_ref::<Self>() else {
            return false;
        };
        if self.len() != other.len()
            || !validity_eq(self.validity.as_ref(), other.validity.as_ref(), self.len())
        {
            return false;
        }
        (0..self.len()).all(|index| self.get(index) == other.get(index))
    }
}

impl ArrayAccessor for &BooleanArray {
    type Item = bool;

    #[inline]
    fn accessor_len(&self) -> usize {
        self.values.len()
    }

    #[inline]
    fn accessor_get(&self, index: usize) -> Option<bool> {
        BooleanArray::get(self, index)
    }
}

impl PartialEq for BooleanArray {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.equals(other)
    }
}

impl Eq for BooleanArray {}

impl FromIterator<bool> for BooleanArray {
    fn from_iter<I: IntoIterator<Item = bool>>(iter: I) -> Self {
        Self::from_values(iter)
    }
}

impl FromIterator<Option<bool>> for BooleanArray {
    fn from_iter<I: IntoIterator<Item = Option<bool>>>(iter: I) -> Self {
        Self::from_opt_iter(iter)
    }
}

impl<const N: usize> From<[bool; N]> for BooleanArray {
    fn from(values: [bool; N]) -> Self {
        Self::from_values(values)
    }
}

impl<'a> IntoIterator for &'a BooleanArray {
    type Item = Option<bool>;
    type IntoIter = ArrayIter<&'a BooleanArray>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl fmt::Debug for BooleanArray {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        debug_array(
            f,
            "BooleanArray",
            &BOOL_TYPE,
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

    #[test]
    fn empty_array() {
        let flags = BooleanArray::from_values([] as [bool; 0]);
        assert!(flags.is_empty());
        assert_eq!(flags.null_count(), 0);
        assert_eq!(flags.true_count(), 0);
        assert_eq!(flags.false_count(), 0);
        assert_eq!(flags.get(0), None);
        assert_eq!(flags.iter().count(), 0);
        assert_eq!(flags.data_type(), &DataType::Bool);
    }

    #[test]
    fn no_null_array() {
        let flags = BooleanArray::from_values([true, false, true, true]);
        assert_eq!(flags.len(), 4);
        assert_eq!(flags.null_count(), 0);
        assert!(flags.validity().is_none());
        assert_eq!(flags.true_count(), 3);
        assert_eq!(flags.false_count(), 1);
        assert_eq!(flags.get(1), Some(false));
        assert_eq!(flags.value(1), Some(false));
        assert_eq!(flags.get(9), None);
    }

    #[test]
    fn all_null_array() {
        let flags = BooleanArray::new_null(5);
        assert_eq!(flags.len(), 5);
        assert_eq!(flags.null_count(), 5);
        assert_eq!(flags.true_count(), 0);
        assert_eq!(flags.false_count(), 0);
        for index in 0..5 {
            assert!(flags.is_null(index));
            assert_eq!(flags.get(index), None);
            assert_eq!(flags.value(index), Some(false));
        }
    }

    #[test]
    fn mixed_nulls_and_counts() {
        let flags = BooleanArray::from_opt_iter([Some(true), None, Some(false), Some(true), None]);
        assert_eq!(flags.len(), 5);
        assert_eq!(flags.null_count(), 2);
        assert_eq!(flags.true_count(), 2);
        assert_eq!(flags.false_count(), 1);
        assert_eq!(
            flags.iter().collect::<Vec<_>>(),
            vec![Some(true), None, Some(false), Some(true), None]
        );
    }

    #[test]
    fn true_count_ignores_values_under_nulls() {
        // The null slot carries a `true` bit that must not be counted.
        let values: Bitmap = [true, true, true].into_iter().collect();
        let validity: Bitmap = [true, false, true].into_iter().collect();
        let flags = BooleanArray::new(values, Some(validity)).unwrap();
        assert_eq!(flags.true_count(), 2);
        assert_eq!(flags.false_count(), 0);
        assert_eq!(flags.null_count(), 1);
    }

    #[test]
    fn from_opt_iter_without_nulls_skips_the_bitmap() {
        let flags = BooleanArray::from_opt_iter([Some(true), Some(false)]);
        assert!(flags.validity().is_none());
    }

    #[test]
    fn slicing_handles_bit_offsets() {
        let source: Vec<Option<bool>> = (0..40)
            .map(|i| (i % 5 != 0).then_some(i % 2 == 0))
            .collect();
        let flags = BooleanArray::from_opt_iter(source.clone());
        for offset in [0usize, 1, 7, 8, 9, 30] {
            let window = flags.slice(offset, 5);
            assert_eq!(window.len(), 5.min(40 - offset));
            for index in 0..window.len() {
                assert_eq!(window.get(index), source[offset + index], "offset {offset}");
            }
        }
        assert_eq!(flags.slice(38, 99).len(), 2);
        assert_eq!(flags.slice(99, 1).len(), 0);
    }

    #[test]
    fn try_slice_reports_out_of_range() {
        let flags = BooleanArray::from_values([true, false, true]);
        assert_eq!(flags.try_slice(1, 2).unwrap().len(), 2);
        assert_eq!(
            flags.try_slice(2, 2).unwrap_err(),
            DataError::SliceOutOfBounds {
                offset: 2,
                len: 2,
                available: 3
            }
        );
    }

    #[test]
    fn validity_length_is_checked() {
        let values: Bitmap = [true, false].into_iter().collect();
        assert!(BooleanArray::new(values.clone(), None).is_ok());
        assert!(BooleanArray::new(values.clone(), Some(Bitmap::new_set(2))).is_ok());
        assert_eq!(
            BooleanArray::new(values, Some(Bitmap::new_set(3))).unwrap_err(),
            DataError::ValidityLengthMismatch {
                array_len: 2,
                validity_len: 3
            }
        );
    }

    #[test]
    fn equality_ignores_null_slot_contents() {
        let a = BooleanArray::from_opt_iter([Some(true), None, Some(false)]);
        let b = BooleanArray::new(
            [true, true, false].into_iter().collect(),
            Some([true, false, true].into_iter().collect()),
        )
        .unwrap();
        assert_eq!(a, b);
        assert_ne!(a, BooleanArray::from_values([true, true, false]));
        assert_ne!(a, BooleanArray::from_opt_iter([Some(true), None]));
    }

    #[test]
    fn sliced_arrays_compare_by_logical_content() {
        let full =
            BooleanArray::from_opt_iter([Some(false), Some(true), None, Some(false), Some(true)]);
        let window = full.slice(1, 3);
        assert_eq!(
            window,
            BooleanArray::from_opt_iter([Some(true), None, Some(false)])
        );
    }

    #[test]
    fn with_validity_replaces_the_bitmap() {
        let flags = BooleanArray::from_values([true, true]);
        let masked = flags
            .with_validity(Some([true, false].into_iter().collect()))
            .unwrap();
        assert_eq!(masked.null_count(), 1);
        assert_eq!(masked.true_count(), 1);
        assert!(flags.with_validity(Some(Bitmap::new_set(9))).is_err());
    }

    #[test]
    fn conversions_and_collect() {
        let from_array: BooleanArray = [true, false].into();
        assert_eq!(from_array.len(), 2);
        let collected: BooleanArray = (0..4).map(|i| i % 2 == 0).collect();
        assert_eq!(collected.true_count(), 2);
        let opt_collected: BooleanArray = [Some(true), None].into_iter().collect();
        assert_eq!(opt_collected.null_count(), 1);
        assert_eq!((&collected).into_iter().count(), 4);
    }

    #[test]
    fn debug_output_is_readable() {
        let flags = BooleanArray::from_opt_iter([Some(true), None]);
        let rendered = format!("{flags:?}");
        assert!(rendered.contains("Bool"), "{rendered}");
        assert!(rendered.contains("nulls=1"), "{rendered}");
        assert!(rendered.contains("true"), "{rendered}");
    }

    #[test]
    fn buffer_memory_size_counts_both_bitmaps() {
        let flags = BooleanArray::from_values([true; 1000]);
        assert!(flags.buffer_memory_size() >= 125);
        let masked = flags.with_validity(Some(Bitmap::new_set(1000))).unwrap();
        assert!(masked.buffer_memory_size() > flags.buffer_memory_size());
    }

    #[test]
    fn large_boolean_column() {
        let flags = BooleanArray::from_values((0..10_000).map(|i| i % 7 == 0));
        assert_eq!(flags.len(), 10_000);
        assert_eq!(flags.true_count(), 10_000_usize.div_ceil(7));
        let window = flags.slice(3, 9_000);
        assert_eq!(window.len(), 9_000);
        assert_eq!(window.get(4), flags.get(7));
    }
}
