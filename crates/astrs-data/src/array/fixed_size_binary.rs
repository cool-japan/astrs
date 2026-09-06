//! [`FixedSizeBinaryArray`] — a column of equally sized byte blobs.
//!
//! No offset buffer: slot `i` is exactly `values[i * size .. (i + 1) * size]`.
//! This is the layout AstRS uses for hashes, fixed-length IDs, small
//! fixed-shape sensor records and the raw bytes of a fixed-format image tile,
//! and it is the cheapest variable-free byte layout Arrow offers.
//!
//! ```
//! use astrs_data::array::{Array, FixedSizeBinaryArray};
//!
//! let hashes = FixedSizeBinaryArray::try_from_values(4, [&b"aaaa"[..], &b"bbbb"[..]])?;
//! assert_eq!(hashes.len(), 2);
//! assert_eq!(hashes.value_size(), 4);
//! assert_eq!(hashes.get(1), Some(&b"bbbb"[..]));
//! # Ok::<(), astrs_data::DataError>(())
//! ```

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use crate::array::iter::{ArrayAccessor, ArrayIter};
use crate::array::{
    Array, ArrayRef, check_fixed_size, check_validity, clamp_window, debug_array, validity_eq,
};
use crate::buffer::{AlignedBuf, Bitmap, BitmapBuilder, Buffer};
use crate::datatype::DataType;
use crate::error::{DataError, Result};
use crate::sealed::Sealed;

/// A column of `size`-byte blobs with an optional validity bitmap.
#[derive(Clone)]
pub struct FixedSizeBinaryArray {
    /// Always `DataType::FixedSizeBinary(size)`.
    data_type: DataType,
    /// Bytes per slot. Always positive.
    size: usize,
    /// `len * size` bytes.
    values: Buffer,
    /// Logical length, `values.len() / size`.
    len: usize,
    /// Validity, or `None` when every slot is valid.
    validity: Option<Bitmap>,
}

impl FixedSizeBinaryArray {
    /// Builds an array from a flat value buffer.
    ///
    /// # Errors
    ///
    /// * [`DataError::InvalidFixedSize`] when `size` is not positive.
    /// * [`DataError::BufferLengthNotMultiple`] when the buffer is not a whole
    ///   number of slots.
    /// * [`DataError::ValidityLengthMismatch`] when the bitmap length is wrong.
    ///
    /// ```
    /// use astrs_data::array::{Array, FixedSizeBinaryArray};
    /// use astrs_data::Buffer;
    ///
    /// let array = FixedSizeBinaryArray::try_new(2, Buffer::from_slice(b"abcd"), None)?;
    /// assert_eq!(array.len(), 2);
    /// assert_eq!(array.get(0), Some(&b"ab"[..]));
    /// # Ok::<(), astrs_data::DataError>(())
    /// ```
    pub fn try_new(size: i32, values: Buffer, validity: Option<Bitmap>) -> Result<Self> {
        let width = check_fixed_size(size)?;
        if !values.len().is_multiple_of(width) {
            return Err(DataError::BufferLengthNotMultiple {
                len: values.len(),
                width,
            });
        }
        let len = values.len() / width;
        check_validity(validity.as_ref(), len)?;
        Ok(Self {
            data_type: DataType::FixedSizeBinary(size),
            size: width,
            values,
            len,
            validity,
        })
    }

    /// Builds an array from equally sized slices.
    ///
    /// # Errors
    ///
    /// * [`DataError::InvalidFixedSize`] when `size` is not positive.
    /// * [`DataError::ChildLengthMismatch`] when a value has the wrong length.
    pub fn try_from_values<V: AsRef<[u8]>>(
        size: i32,
        values: impl IntoIterator<Item = V>,
    ) -> Result<Self> {
        Self::try_from_opt_iter(size, values.into_iter().map(Some))
    }

    /// Builds an array from optional equally sized slices, null slots holding
    /// `size` zero bytes.
    ///
    /// # Errors
    ///
    /// * [`DataError::InvalidFixedSize`] when `size` is not positive.
    /// * [`DataError::ChildLengthMismatch`] when a value has the wrong length.
    ///
    /// ```
    /// use astrs_data::array::{Array, FixedSizeBinaryArray};
    ///
    /// let array = FixedSizeBinaryArray::try_from_opt_iter(2, [Some(&b"ab"[..]), None])?;
    /// assert_eq!(array.null_count(), 1);
    /// assert!(FixedSizeBinaryArray::try_from_values(2, [&b"abc"[..]]).is_err());
    /// # Ok::<(), astrs_data::DataError>(())
    /// ```
    pub fn try_from_opt_iter<V: AsRef<[u8]>>(
        size: i32,
        values: impl IntoIterator<Item = Option<V>>,
    ) -> Result<Self> {
        let width = check_fixed_size(size)?;
        let iter = values.into_iter();
        let (lower, _) = iter.size_hint();
        let mut data = AlignedBuf::with_capacity(lower.saturating_mul(width));
        let mut validity = BitmapBuilder::with_capacity(lower);
        let mut any_null = false;
        for value in iter {
            match value {
                Some(value) => {
                    let bytes = value.as_ref();
                    if bytes.len() != width {
                        return Err(DataError::ChildLengthMismatch {
                            expected: width,
                            actual: bytes.len(),
                        });
                    }
                    data.extend_from_slice(bytes);
                    validity.append(true);
                }
                None => {
                    data.extend_zeroed(width);
                    validity.append(false);
                    any_null = true;
                }
            }
        }
        let len = validity.len();
        Ok(Self {
            data_type: DataType::FixedSizeBinary(size),
            size: width,
            values: Buffer::from(data),
            len,
            validity: any_null.then(|| validity.finish()),
        })
    }

    /// An all-null array of `len` slots.
    ///
    /// # Errors
    ///
    /// [`DataError::InvalidFixedSize`] when `size` is not positive.
    pub fn new_null(size: i32, len: usize) -> Result<Self> {
        let width = check_fixed_size(size)?;
        Ok(Self {
            data_type: DataType::FixedSizeBinary(size),
            size: width,
            values: Buffer::zeroed(len.saturating_mul(width)),
            len,
            validity: Some(Bitmap::new_unset(len)),
        })
    }

    /// Assembles the parts without revalidating.
    ///
    /// Safe, and crate-internal: [`crate::builder::FixedSizeBinaryBuilder`]
    /// checks every value's width at `append`, so the finished buffer is a
    /// whole number of slots by construction.
    pub(crate) fn from_parts(size: i32, values: Buffer, validity: Option<Bitmap>) -> Self {
        let width = usize::try_from(size).unwrap_or(1).max(1);
        debug_assert_eq!(values.len() % width, 0);
        Self {
            data_type: DataType::FixedSizeBinary(size),
            size: width,
            len: values.len() / width,
            values,
            validity,
        }
    }

    /// Bytes per slot.
    #[inline]
    #[must_use]
    pub const fn value_size(&self) -> usize {
        self.size
    }

    /// The flat value buffer, `len * size` bytes.
    #[inline]
    #[must_use]
    pub const fn value_data(&self) -> &Buffer {
        &self.values
    }

    /// The raw slot bytes at `index`, ignoring validity.
    ///
    /// Returns `None` only when `index` is out of range.
    #[inline]
    #[must_use]
    pub fn value(&self, index: usize) -> Option<&[u8]> {
        if index >= self.len {
            return None;
        }
        let start = index * self.size;
        self.values.as_slice().get(start..start + self.size)
    }

    /// The logical value at `index`: `None` when the slot is null or out of
    /// range.
    #[inline]
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&[u8]> {
        if self.is_valid_index(index) {
            self.value(index)
        } else {
            None
        }
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
        let (offset, len) = clamp_window(self.len, offset, len);
        Self {
            data_type: self.data_type.clone(),
            size: self.size,
            values: self.values.slice(offset * self.size, len * self.size),
            len,
            validity: self.validity.as_ref().map(|bits| bits.slice(offset, len)),
        }
    }

    /// Checked [`FixedSizeBinaryArray::slice`].
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
            size: self.size,
            values: self.values.clone(),
            len: self.len,
            validity,
        })
    }

    /// Whether `index` is inside the array and not null.
    #[inline]
    fn is_valid_index(&self, index: usize) -> bool {
        index < self.len && self.validity.as_ref().is_none_or(|bits| bits.value(index))
    }
}

impl Sealed for FixedSizeBinaryArray {}

impl Array for FixedSizeBinaryArray {
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
        self.values.backing_len()
            + self
                .validity
                .as_ref()
                .map_or(0, |bits| bits.buffer().backing_len())
    }

    fn equals(&self, other: &dyn Array) -> bool {
        let Some(other) = other.as_any().downcast_ref::<Self>() else {
            return false;
        };
        if self.size != other.size
            || self.len != other.len
            || !validity_eq(self.validity.as_ref(), other.validity.as_ref(), self.len)
        {
            return false;
        }
        (0..self.len).all(|index| self.get(index) == other.get(index))
    }
}

impl<'a> ArrayAccessor for &'a FixedSizeBinaryArray {
    type Item = &'a [u8];

    #[inline]
    fn accessor_len(&self) -> usize {
        self.len
    }

    #[inline]
    fn accessor_get(&self, index: usize) -> Option<&'a [u8]> {
        let array: &'a FixedSizeBinaryArray = self;
        array.get(index)
    }
}

impl PartialEq for FixedSizeBinaryArray {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.equals(other)
    }
}

impl Eq for FixedSizeBinaryArray {}

impl<'a> IntoIterator for &'a FixedSizeBinaryArray {
    type Item = Option<&'a [u8]>;
    type IntoIter = ArrayIter<&'a FixedSizeBinaryArray>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl fmt::Debug for FixedSizeBinaryArray {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        debug_array(
            f,
            "FixedSizeBinaryArray",
            &self.data_type,
            self.len,
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
        let array = FixedSizeBinaryArray::try_from_values(4, [] as [&[u8]; 0]).unwrap();
        assert!(array.is_empty());
        assert_eq!(array.value_size(), 4);
        assert_eq!(array.get(0), None);
        assert_eq!(array.iter().count(), 0);
        assert_eq!(array.data_type(), &DataType::FixedSizeBinary(4));
    }

    #[test]
    fn no_null_array() {
        let array =
            FixedSizeBinaryArray::try_from_values(3, [&b"abc"[..], &b"def"[..], &b"ghi"[..]])
                .unwrap();
        assert_eq!(array.len(), 3);
        assert_eq!(array.null_count(), 0);
        assert!(array.validity().is_none());
        assert_eq!(array.get(0), Some(&b"abc"[..]));
        assert_eq!(array.get(2), Some(&b"ghi"[..]));
        assert_eq!(array.get(3), None);
        assert_eq!(array.value_data().as_slice(), b"abcdefghi");
    }

    #[test]
    fn all_null_array() {
        let array = FixedSizeBinaryArray::new_null(2, 3).unwrap();
        assert_eq!(array.len(), 3);
        assert_eq!(array.null_count(), 3);
        for index in 0..3 {
            assert!(array.is_null(index));
            assert_eq!(array.get(index), None);
            assert_eq!(array.value(index), Some(&[0u8, 0][..]));
        }
    }

    #[test]
    fn mixed_nulls() {
        let array =
            FixedSizeBinaryArray::try_from_opt_iter(2, [Some(&b"ab"[..]), None, Some(&b"cd"[..])])
                .unwrap();
        assert_eq!(array.len(), 3);
        assert_eq!(array.null_count(), 1);
        assert_eq!(array.get(1), None);
        assert_eq!(array.value(1), Some(&[0u8, 0][..]));
        assert_eq!(
            array.iter().flatten().collect::<Vec<_>>(),
            vec![&b"ab"[..], &b"cd"[..]]
        );
    }

    #[test]
    fn wrong_width_values_are_rejected() {
        assert_eq!(
            FixedSizeBinaryArray::try_from_values(2, [&b"abc"[..]]).unwrap_err(),
            DataError::ChildLengthMismatch {
                expected: 2,
                actual: 3
            }
        );
        assert_eq!(
            FixedSizeBinaryArray::try_from_values(2, [&b""[..]]).unwrap_err(),
            DataError::ChildLengthMismatch {
                expected: 2,
                actual: 0
            }
        );
    }

    #[test]
    fn invalid_sizes_are_rejected() {
        assert_eq!(
            FixedSizeBinaryArray::try_new(0, Buffer::new(), None).unwrap_err(),
            DataError::InvalidFixedSize { size: 0 }
        );
        assert!(FixedSizeBinaryArray::try_new(-4, Buffer::new(), None).is_err());
        assert!(FixedSizeBinaryArray::new_null(0, 3).is_err());
        assert!(FixedSizeBinaryArray::try_from_values(-1, [] as [&[u8]; 0]).is_err());
    }

    #[test]
    fn ragged_buffers_are_rejected() {
        assert_eq!(
            FixedSizeBinaryArray::try_new(4, Buffer::from_slice(b"abcdef"), None).unwrap_err(),
            DataError::BufferLengthNotMultiple { len: 6, width: 4 }
        );
        assert!(
            FixedSizeBinaryArray::try_new(2, Buffer::from_slice(b"abcd"), Some(Bitmap::new_set(3)))
                .is_err()
        );
    }

    #[test]
    fn slicing_is_zero_copy_and_clamping() {
        let array =
            FixedSizeBinaryArray::try_from_values(2, (0..10).map(|i| vec![i as u8, (i * 2) as u8]))
                .unwrap();
        let window = array.slice(3, 4);
        assert_eq!(window.len(), 4);
        assert_eq!(window.get(0), Some(&[3u8, 6][..]));
        assert_eq!(window.value_size(), 2);
        assert_eq!(
            window.value_data().backing_len(),
            array.value_data().backing_len(),
            "the values buffer is shared"
        );
        assert_eq!(array.slice(9, 99).len(), 1);
        assert_eq!(array.slice(99, 1).len(), 0);
        assert_eq!(array.slice(2, 3).slice(1, 1).get(0), Some(&[3u8, 6][..]));
        assert!(array.try_slice(9, 2).is_err());
    }

    #[test]
    fn equality_ignores_null_slot_contents() {
        let a = FixedSizeBinaryArray::try_from_opt_iter(2, [Some(&b"ab"[..]), None]).unwrap();
        let b = FixedSizeBinaryArray::try_new(
            2,
            Buffer::from_slice(b"abzz"),
            Some([true, false].into_iter().collect()),
        )
        .unwrap();
        assert_eq!(a, b);
        assert_ne!(
            a,
            FixedSizeBinaryArray::try_from_values(2, [&b"ab"[..]]).unwrap()
        );
        assert_ne!(
            a,
            FixedSizeBinaryArray::try_from_opt_iter(2, [Some(&b"ab"[..]), Some(&b"cd"[..])])
                .unwrap()
        );
    }

    #[test]
    fn arrays_of_different_widths_are_not_equal() {
        let two = FixedSizeBinaryArray::try_from_values(2, [&b"ab"[..]]).unwrap();
        let one = FixedSizeBinaryArray::try_from_values(1, [&b"a"[..]]).unwrap();
        assert_ne!(two, one);
    }

    #[test]
    fn with_validity_replaces_the_bitmap() {
        let array = FixedSizeBinaryArray::try_from_values(1, [&b"a"[..], &b"b"[..]]).unwrap();
        let masked = array
            .with_validity(Some([true, false].into_iter().collect()))
            .unwrap();
        assert_eq!(masked.null_count(), 1);
        assert!(array.with_validity(Some(Bitmap::new_set(9))).is_err());
    }

    #[test]
    fn debug_output_is_readable() {
        let array = FixedSizeBinaryArray::try_from_opt_iter(2, [Some(&b"ab"[..]), None]).unwrap();
        let rendered = format!("{array:?}");
        assert!(rendered.contains("FixedSizeBinary(2)"), "{rendered}");
        assert!(rendered.contains("nulls=1"), "{rendered}");
    }

    #[test]
    fn large_fixed_width_column() {
        let array =
            FixedSizeBinaryArray::try_from_values(32, (0..2_000).map(|i| vec![i as u8; 32]))
                .unwrap();
        assert_eq!(array.len(), 2_000);
        assert_eq!(array.value_data().len(), 64_000);
        assert_eq!(array.get(1_999), Some(&[(1_999u32 as u8); 32][..]));
        assert!(array.buffer_memory_size() >= 64_000);
    }
}
