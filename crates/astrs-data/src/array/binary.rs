//! [`GenericBinaryArray`] — the variable-length byte column, in its `Binary`
//! (32-bit offsets) and `LargeBinary` (64-bit offsets) forms.
//!
//! # Layout
//!
//! ```text
//!   offsets  [o0, o1, o2, … , oN]     N + 1 entries, non-decreasing
//!   values   ├──v0──┼───v1───┼─v2─┤   one flat byte region
//!   validity  1      0        1        optional, one bit per value
//! ```
//!
//! Slicing narrows the *offset* buffer and leaves the value region alone, so a
//! sliced array's first offset is not zero. That is legal Arrow and it is what
//! keeps slicing `O(1)`: no bytes move and no offsets are rewritten.
//! [`validate_offsets`] therefore accepts any non-decreasing, in-range offset
//! sequence rather than insisting on a leading zero.
//!
//! ```
//! use astrs_data::array::{Array, BinaryArray};
//!
//! let blobs = BinaryArray::from_opt_iter([Some(&b"aa"[..]), None, Some(&b"cccc"[..])]);
//! assert_eq!(blobs.len(), 3);
//! assert_eq!(blobs.get(0), Some(&b"aa"[..]));
//! assert_eq!(blobs.get(1), None);
//! assert_eq!(blobs.value_length(2), Some(4));
//!
//! let tail = blobs.slice(2, 1);
//! assert_eq!(tail.get(0), Some(&b"cccc"[..]));
//! ```

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use crate::array::iter::{ArrayAccessor, ArrayIter};
use crate::array::offset::{OffsetSizeTrait, validate_offsets};
use crate::array::{
    Array, ArrayRef, BINARY_TYPE, LARGE_BINARY_TYPE, check_validity, clamp_window, debug_array,
    validity_eq,
};
use crate::buffer::{Bitmap, Buffer, ScalarBuffer};
use crate::datatype::DataType;
use crate::error::{DataError, Result};
use crate::sealed::Sealed;

/// A column of variable-length byte strings.
///
/// Use the [`BinaryArray`] and [`LargeBinaryArray`] aliases rather than naming
/// the offset parameter directly.
#[derive(Clone)]
pub struct GenericBinaryArray<O: OffsetSizeTrait> {
    /// `len + 1` non-decreasing offsets into `values`.
    offsets: ScalarBuffer<O>,
    /// The flat byte region every value points into.
    values: Buffer,
    /// Validity, or `None` when every slot is valid.
    validity: Option<Bitmap>,
}

/// `Binary` column — variable-length bytes with 32-bit offsets.
pub type BinaryArray = GenericBinaryArray<i32>;
/// `LargeBinary` column — variable-length bytes with 64-bit offsets.
pub type LargeBinaryArray = GenericBinaryArray<i64>;

impl<O: OffsetSizeTrait> GenericBinaryArray<O> {
    /// Builds an array from its three buffers, validating the offsets.
    ///
    /// # Errors
    ///
    /// * The offset errors listed on [`validate_offsets`].
    /// * [`DataError::ValidityLengthMismatch`] when the bitmap length is wrong.
    ///
    /// ```
    /// use astrs_data::array::{Array, BinaryArray};
    /// use astrs_data::{Buffer, ScalarBuffer};
    ///
    /// let array = BinaryArray::try_new(
    ///     ScalarBuffer::from_slice(&[0, 2, 5]),
    ///     Buffer::from_slice(b"aabbb"),
    ///     None,
    /// )?;
    /// assert_eq!(array.get(1), Some(&b"bbb"[..]));
    /// # Ok::<(), astrs_data::DataError>(())
    /// ```
    pub fn try_new(
        offsets: ScalarBuffer<O>,
        values: Buffer,
        validity: Option<Bitmap>,
    ) -> Result<Self> {
        let len = validate_offsets(offsets.as_slice(), values.len())?;
        check_validity(validity.as_ref(), len)?;
        Ok(Self {
            offsets,
            values,
            validity,
        })
    }

    /// Builds an array from its three buffers **without validating offsets**.
    ///
    /// # Safety
    ///
    /// This constructor is `unsafe` by contract, not by memory model: the
    /// accessors stay memory-safe whatever the offsets say (they use checked
    /// slicing and return `None`). Skipping validation means a malformed
    /// buffer silently produces `None` values instead of a decode error, which
    /// hides corruption. Only use it for offsets this crate itself produced —
    /// for example when re-wrapping buffers taken out of another array.
    ///
    /// Callers must guarantee that `offsets` holds `len + 1` non-decreasing,
    /// non-negative entries, all within `values.len()`, and that `validity`
    /// (if present) covers exactly `len` slots.
    #[must_use]
    pub unsafe fn new_unchecked(
        offsets: ScalarBuffer<O>,
        values: Buffer,
        validity: Option<Bitmap>,
    ) -> Self {
        Self::from_parts(offsets, values, validity)
    }

    /// Assembles the parts without revalidating.
    ///
    /// Safe, and crate-internal: the builders construct their own offsets and
    /// cannot produce an invalid layout, so revalidating on every `finish`
    /// would be pure overhead. The `debug_assert` keeps that claim honest in
    /// test builds.
    pub(crate) fn from_parts(
        offsets: ScalarBuffer<O>,
        values: Buffer,
        validity: Option<Bitmap>,
    ) -> Self {
        debug_assert!(validate_offsets(offsets.as_slice(), values.len()).is_ok());
        debug_assert!(
            validity
                .as_ref()
                .is_none_or(|bits| bits.len() + 1 == offsets.len())
        );
        Self {
            offsets,
            values,
            validity,
        }
    }

    /// Builds an array with no nulls from any iterator of byte slices.
    ///
    /// ```
    /// use astrs_data::array::{Array, LargeBinaryArray};
    ///
    /// let array = LargeBinaryArray::from_values([&b"one"[..], &b"two"[..]]);
    /// assert_eq!(array.len(), 2);
    /// assert_eq!(array.null_count(), 0);
    /// ```
    #[must_use]
    pub fn from_values<V: AsRef<[u8]>>(values: impl IntoIterator<Item = V>) -> Self {
        Self::build(values.into_iter().map(Some))
    }

    /// Builds an array from optional byte slices.
    #[must_use]
    pub fn from_opt_iter<V: AsRef<[u8]>>(values: impl IntoIterator<Item = Option<V>>) -> Self {
        Self::build(values.into_iter())
    }

    /// An all-null array of `len` slots.
    #[must_use]
    pub fn new_null(len: usize) -> Self {
        Self {
            offsets: ScalarBuffer::zeroed(len + 1),
            values: Buffer::new(),
            validity: Some(Bitmap::new_unset(len)),
        }
    }

    /// Shared constructor behind [`Self::from_values`] and
    /// [`Self::from_opt_iter`].
    fn build<V: AsRef<[u8]>>(values: impl Iterator<Item = Option<V>>) -> Self {
        let (lower, _) = values.size_hint();
        let mut offsets = Vec::with_capacity(lower + 1);
        let mut data = crate::buffer::AlignedBuf::new();
        let mut validity = crate::buffer::BitmapBuilder::with_capacity(lower);
        let mut any_null = false;
        offsets.push(O::ZERO);
        for value in values {
            match value {
                Some(value) => {
                    data.extend_from_slice(value.as_ref());
                    validity.append(true);
                }
                None => {
                    validity.append(false);
                    any_null = true;
                }
            }
            // Saturating: an offset overflow can only happen at 2 GiB with
            // 32-bit offsets, and `try_new` would reject the result anyway.
            offsets.push(O::from_usize(data.len()).unwrap_or(O::ZERO));
        }
        Self {
            offsets: ScalarBuffer::from_slice(&offsets),
            values: Buffer::from(data),
            validity: any_null.then(|| validity.finish()),
        }
    }

    /// The offset buffer, `len + 1` entries.
    #[inline]
    #[must_use]
    pub fn value_offsets(&self) -> &[O] {
        self.offsets.as_slice()
    }

    /// The offset buffer as a shareable typed window.
    ///
    /// [`GenericBinaryArray::value_offsets`] hands out a slice, which the IPC
    /// encoder cannot put on the wire without copying it. This accessor lets
    /// it clone the window instead — one atomic increment, no bytes moved.
    ///
    /// ```
    /// use astrs_data::array::BinaryArray;
    ///
    /// let array = BinaryArray::from_values([&b"ab"[..], &b"c"[..]]);
    /// assert_eq!(array.offsets_buffer().as_slice(), &[0, 2, 3]);
    /// ```
    #[inline]
    #[must_use]
    pub const fn offsets_buffer(&self) -> &ScalarBuffer<O> {
        &self.offsets
    }

    /// The flat value region every slot points into.
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
        let start = self.offsets.get(index)?.to_usize()?;
        let end = self.offsets.get(index + 1)?.to_usize()?;
        self.values.as_slice().get(start..end)
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

    /// Byte length of slot `index`, ignoring validity.
    #[inline]
    #[must_use]
    pub fn value_length(&self, index: usize) -> Option<usize> {
        let start = self.offsets.get(index)?.to_usize()?;
        let end = self.offsets.get(index + 1)?.to_usize()?;
        end.checked_sub(start)
    }

    /// Total bytes the values occupy, across the whole array.
    #[must_use]
    pub fn total_value_bytes(&self) -> usize {
        let offsets = self.offsets.as_slice();
        match (offsets.first(), offsets.last()) {
            (Some(first), Some(last)) => {
                let start = first.to_usize().unwrap_or(0);
                let end = last.to_usize().unwrap_or(0);
                end.saturating_sub(start)
            }
            _ => 0,
        }
    }

    /// Iterates over the logical values, `None` for nulls.
    #[inline]
    pub fn iter(&self) -> ArrayIter<&Self> {
        ArrayIter::new(self)
    }

    /// A zero-copy sub-range, clamped to the array (the crate-wide slicing
    /// convention).
    ///
    /// Only the offset buffer is narrowed; the value region is shared as-is.
    #[must_use]
    pub fn slice(&self, offset: usize, len: usize) -> Self {
        let (offset, len) = clamp_window(self.len(), offset, len);
        Self {
            offsets: self.offsets.slice(offset, len + 1),
            values: self.values.clone(),
            validity: self.validity.as_ref().map(|bits| bits.slice(offset, len)),
        }
    }

    /// Checked [`GenericBinaryArray::slice`].
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
            offsets: self.offsets.clone(),
            values: self.values.clone(),
            validity,
        })
    }

    /// Whether `index` is inside the array and not null.
    #[inline]
    fn is_valid_index(&self, index: usize) -> bool {
        index < self.len() && self.validity.as_ref().is_none_or(|bits| bits.value(index))
    }
}

impl<O: OffsetSizeTrait> Sealed for GenericBinaryArray<O> {}

impl<O: OffsetSizeTrait> Array for GenericBinaryArray<O> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn data_type(&self) -> &DataType {
        if O::IS_LARGE {
            &LARGE_BINARY_TYPE
        } else {
            &BINARY_TYPE
        }
    }

    fn len(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }

    fn validity(&self) -> Option<&Bitmap> {
        self.validity.as_ref()
    }

    fn slice(&self, offset: usize, len: usize) -> ArrayRef {
        Arc::new(Self::slice(self, offset, len))
    }

    fn buffer_memory_size(&self) -> usize {
        self.offsets.inner().backing_len()
            + self.values.backing_len()
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

impl<'a, O: OffsetSizeTrait> ArrayAccessor for &'a GenericBinaryArray<O> {
    type Item = &'a [u8];

    #[inline]
    fn accessor_len(&self) -> usize {
        Array::len(*self)
    }

    #[inline]
    fn accessor_get(&self, index: usize) -> Option<&'a [u8]> {
        let array: &'a GenericBinaryArray<O> = self;
        array.get(index)
    }
}

impl<O: OffsetSizeTrait> PartialEq for GenericBinaryArray<O> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.equals(other)
    }
}

impl<O: OffsetSizeTrait> Eq for GenericBinaryArray<O> {}

impl<O: OffsetSizeTrait, V: AsRef<[u8]>> FromIterator<Option<V>> for GenericBinaryArray<O> {
    fn from_iter<I: IntoIterator<Item = Option<V>>>(iter: I) -> Self {
        Self::from_opt_iter(iter)
    }
}

impl<'a, O: OffsetSizeTrait> IntoIterator for &'a GenericBinaryArray<O> {
    type Item = Option<&'a [u8]>;
    type IntoIter = ArrayIter<&'a GenericBinaryArray<O>>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<O: OffsetSizeTrait> fmt::Debug for GenericBinaryArray<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        debug_array(
            f,
            if O::IS_LARGE {
                "LargeBinaryArray"
            } else {
                "BinaryArray"
            },
            self.data_type(),
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
        let array = BinaryArray::from_values([] as [&[u8]; 0]);
        assert!(array.is_empty());
        assert_eq!(array.len(), 0);
        assert_eq!(array.null_count(), 0);
        assert_eq!(array.value_offsets(), &[0]);
        assert_eq!(array.get(0), None);
        assert_eq!(array.value_length(0), None);
        assert_eq!(array.total_value_bytes(), 0);
        assert_eq!(array.iter().count(), 0);
    }

    #[test]
    fn no_null_array() {
        let array = BinaryArray::from_values([&b"aa"[..], &b""[..], &b"cccc"[..]]);
        assert_eq!(array.len(), 3);
        assert_eq!(array.null_count(), 0);
        assert!(array.validity().is_none());
        assert_eq!(array.value_offsets(), &[0, 2, 2, 6]);
        assert_eq!(array.get(0), Some(&b"aa"[..]));
        assert_eq!(array.get(1), Some(&b""[..]));
        assert_eq!(array.get(2), Some(&b"cccc"[..]));
        assert_eq!(array.get(3), None);
        assert_eq!(array.value_length(1), Some(0));
        assert_eq!(array.total_value_bytes(), 6);
        assert_eq!(array.value_data().as_slice(), b"aacccc");
    }

    #[test]
    fn all_null_array() {
        let array = LargeBinaryArray::new_null(3);
        assert_eq!(array.len(), 3);
        assert_eq!(array.null_count(), 3);
        assert_eq!(array.data_type(), &DataType::LargeBinary);
        for index in 0..3 {
            assert!(array.is_null(index));
            assert_eq!(array.get(index), None);
            assert_eq!(array.value(index), Some(&b""[..]));
        }
    }

    #[test]
    fn mixed_nulls() {
        let array =
            BinaryArray::from_opt_iter([Some(&b"one"[..]), None, Some(&b"three"[..]), None]);
        assert_eq!(array.len(), 4);
        assert_eq!(array.null_count(), 2);
        assert_eq!(array.get(0), Some(&b"one"[..]));
        assert_eq!(array.get(1), None);
        assert_eq!(array.value(1), Some(&b""[..]), "null slots span zero bytes");
        assert_eq!(array.get(2), Some(&b"three"[..]));
        assert_eq!(array.total_value_bytes(), 8);
        assert_eq!(
            array.iter().flatten().collect::<Vec<_>>(),
            vec![&b"one"[..], &b"three"[..]]
        );
    }

    #[test]
    fn slicing_narrows_offsets_only() {
        let array = BinaryArray::from_values([&b"a"[..], &b"bb"[..], &b"ccc"[..], &b"dddd"[..]]);
        let window = array.slice(1, 2);
        assert_eq!(window.len(), 2);
        assert_eq!(window.get(0), Some(&b"bb"[..]));
        assert_eq!(window.get(1), Some(&b"ccc"[..]));
        assert_eq!(
            window.value_offsets(),
            &[1, 3, 6],
            "a sliced array keeps the original offsets"
        );
        assert_eq!(
            window.value_data().as_slice(),
            array.value_data().as_slice(),
            "the value region is shared, not rebuilt"
        );
        assert_eq!(window.total_value_bytes(), 5);
    }

    #[test]
    fn slicing_clamps_and_composes() {
        let array = BinaryArray::from_values((0..20).map(|i| vec![b'x'; i]));
        assert_eq!(array.slice(18, 99).len(), 2);
        assert_eq!(array.slice(99, 1).len(), 0);
        assert_eq!(array.slice(0, 0).len(), 0);
        let nested = array.slice(4, 10).slice(3, 4);
        assert_eq!(nested.len(), 4);
        assert_eq!(nested.value_length(0), Some(7));
        assert_eq!(
            array.try_slice(19, 2).unwrap_err(),
            DataError::SliceOutOfBounds {
                offset: 19,
                len: 2,
                available: 20
            }
        );
    }

    #[test]
    fn sliced_arrays_revalidate() {
        let array = BinaryArray::from_values([&b"a"[..], &b"bb"[..], &b"ccc"[..]]);
        let window = array.slice(1, 2);
        // Rebuilding from the sliced buffers must pass validation: offsets that
        // do not start at zero are legal.
        let rebuilt = BinaryArray::try_new(
            ScalarBuffer::from_slice(window.value_offsets()),
            window.value_data().clone(),
            None,
        )
        .unwrap();
        assert_eq!(rebuilt, window);
    }

    #[test]
    fn try_new_validates_offsets() {
        let values = Buffer::from_slice(b"abcdef");
        assert!(
            BinaryArray::try_new(ScalarBuffer::from_slice(&[0, 3, 6]), values.clone(), None)
                .is_ok()
        );
        assert!(matches!(
            BinaryArray::try_new(ScalarBuffer::from_slice(&[0, 9]), values.clone(), None),
            Err(DataError::OffsetOutOfBounds { .. })
        ));
        assert!(matches!(
            BinaryArray::try_new(ScalarBuffer::from_slice(&[0, 5, 2]), values.clone(), None),
            Err(DataError::NonMonotonicOffsets { .. })
        ));
        assert!(matches!(
            BinaryArray::try_new(ScalarBuffer::from_slice(&[-1, 2]), values.clone(), None),
            Err(DataError::NegativeOffset { .. })
        ));
        assert!(matches!(
            BinaryArray::try_new(
                ScalarBuffer::from_slice(&[] as &[i32]),
                values.clone(),
                None
            ),
            Err(DataError::OffsetBufferLength { .. })
        ));
        assert!(matches!(
            BinaryArray::try_new(
                ScalarBuffer::from_slice(&[0, 3]),
                values,
                Some(Bitmap::new_set(5))
            ),
            Err(DataError::ValidityLengthMismatch { .. })
        ));
    }

    #[test]
    fn unchecked_constructor_matches_the_checked_one() {
        let offsets = ScalarBuffer::from_slice(&[0i32, 2, 5]);
        let values = Buffer::from_slice(b"aabbb");
        let checked = BinaryArray::try_new(offsets.clone(), values.clone(), None).unwrap();
        // SAFETY: the same buffers just passed validation above.
        let unchecked = unsafe { BinaryArray::new_unchecked(offsets, values, None) };
        assert_eq!(checked, unchecked);
    }

    #[test]
    fn both_offset_widths_work() {
        let small = BinaryArray::from_values([&b"x"[..]]);
        let large = LargeBinaryArray::from_values([&b"x"[..]]);
        assert_eq!(small.data_type(), &DataType::Binary);
        assert_eq!(large.data_type(), &DataType::LargeBinary);
        assert_eq!(small.get(0), large.get(0));
        assert_eq!(small.value_offsets().len(), 2);
        assert_eq!(large.value_offsets().len(), 2);
    }

    #[test]
    fn equality_ignores_null_slot_contents_and_offset_bases() {
        let a = BinaryArray::from_opt_iter([Some(&b"aa"[..]), None, Some(&b"cc"[..])]);
        let padded = BinaryArray::from_opt_iter([
            Some(&b"zz"[..]),
            Some(&b"aa"[..]),
            None,
            Some(&b"cc"[..]),
        ]);
        assert_eq!(a, padded.slice(1, 3));
        assert_ne!(a, BinaryArray::from_values([&b"aa"[..], &b"cc"[..]]));
        assert_ne!(
            a,
            BinaryArray::from_opt_iter([Some(&b"aa"[..]), Some(&b"bb"[..]), Some(&b"cc"[..])])
        );
    }

    #[test]
    fn equality_across_offset_widths_is_false() {
        let small: ArrayRef = Arc::new(BinaryArray::from_values([&b"x"[..]]));
        let large: ArrayRef = Arc::new(LargeBinaryArray::from_values([&b"x"[..]]));
        assert_ne!(small, large);
    }

    #[test]
    fn with_validity_replaces_the_bitmap() {
        let array = BinaryArray::from_values([&b"a"[..], &b"b"[..]]);
        let masked = array
            .with_validity(Some([true, false].into_iter().collect()))
            .unwrap();
        assert_eq!(masked.null_count(), 1);
        assert_eq!(masked.get(1), None);
        assert!(array.with_validity(Some(Bitmap::new_set(9))).is_err());
    }

    #[test]
    fn collects_from_iterators() {
        let collected: BinaryArray = [Some(b"a".to_vec()), None].into_iter().collect();
        assert_eq!(collected.len(), 2);
        assert_eq!(collected.null_count(), 1);
        let owned = BinaryArray::from_values(vec![vec![1u8, 2], vec![3]]);
        assert_eq!(owned.get(0), Some(&[1u8, 2][..]));
        assert_eq!((&owned).into_iter().count(), 2);
    }

    #[test]
    fn debug_output_names_the_offset_width() {
        let small = BinaryArray::from_opt_iter([Some(&b"a"[..]), None]);
        let rendered = format!("{small:?}");
        assert!(rendered.starts_with("BinaryArray"), "{rendered}");
        assert!(rendered.contains("nulls=1"), "{rendered}");

        let large = LargeBinaryArray::from_values([&b"a"[..]]);
        assert!(format!("{large:?}").starts_with("LargeBinaryArray"));
    }

    #[test]
    fn buffer_memory_size_counts_three_buffers() {
        let array = BinaryArray::from_values([&b"abcdef"[..]; 100]);
        let bare = array.buffer_memory_size();
        assert!(bare >= 600);
        let masked = array.with_validity(Some(Bitmap::new_set(100))).unwrap();
        assert!(masked.buffer_memory_size() > bare);
    }

    #[test]
    fn large_values_round_trip() {
        let big = vec![7u8; 100_000];
        let array = LargeBinaryArray::from_values([big.as_slice(), b"tail"]);
        assert_eq!(array.value_length(0), Some(100_000));
        assert_eq!(array.get(1), Some(&b"tail"[..]));
        assert_eq!(array.total_value_bytes(), 100_004);
    }
}
