//! [`GenericStringArray`] — the variable-length UTF-8 column, in its `Utf8`
//! (32-bit offsets) and `LargeUtf8` (64-bit offsets) forms.
//!
//! The layout is identical to [`crate::array::GenericBinaryArray`] — this type
//! *is* one, wrapped — plus one invariant: **every slot is valid UTF-8**.
//!
//! # Validating untrusted bytes
//!
//! A payload arriving over the wire is untrusted, and two independent things
//! can be wrong with it:
//!
//! 1. the concatenated value region is not valid UTF-8; or
//! 2. the region *is* valid UTF-8, but an offset lands in the middle of a
//!    multi-byte code point, so an individual slot is not.
//!
//! Checking only the first is the classic bug: `"é"` is `[0xC3, 0xA9]`, so
//! offsets `[0, 1, 2]` split a perfectly valid two-byte region into two
//! invalid slots. [`GenericStringArray::try_new`] checks both — a single
//! [`str::from_utf8`] pass over the whole region, then
//! [`str::is_char_boundary`] on every offset.
//!
//! ```
//! use astrs_data::array::{Array, StringArray};
//! use astrs_data::{Buffer, DataError, ScalarBuffer};
//!
//! // Valid region, invalid split.
//! let err = StringArray::try_new(
//!     ScalarBuffer::from_slice(&[0, 1, 2]),
//!     Buffer::from_slice("é".as_bytes()),
//!     None,
//! )
//! .unwrap_err();
//! assert!(matches!(err, DataError::OffsetNotCharBoundary { index: 1, offset: 1 }));
//!
//! // The same region, split correctly.
//! let ok = StringArray::try_new(
//!     ScalarBuffer::from_slice(&[0, 2]),
//!     Buffer::from_slice("é".as_bytes()),
//!     None,
//! )?;
//! assert_eq!(ok.get(0), Some("é"));
//! # Ok::<(), astrs_data::DataError>(())
//! ```
//!
//! # Access cost
//!
//! Value access returns `&str` through a checked [`str::from_utf8`] rather
//! than an unchecked reinterpretation. That keeps `unsafe` confined to the two
//! buffer modules ([`crate::buffer::aligned`] and [`crate::buffer::scalar`]),
//! and the check is a vectorised scan over one — usually short — value. Hot
//! paths that do not need `&str` can read the raw bytes through
//! [`GenericStringArray::value_bytes`] and skip it entirely.

use std::any::Any;
use std::fmt;
use std::sync::Arc;

use crate::array::binary::GenericBinaryArray;
use crate::array::iter::{ArrayAccessor, ArrayIter};
use crate::array::offset::OffsetSizeTrait;
use crate::array::{Array, ArrayRef, LARGE_UTF8_TYPE, UTF8_TYPE, debug_array};
use crate::buffer::{Bitmap, Buffer, ScalarBuffer};
use crate::datatype::DataType;
use crate::error::{DataError, Result};
use crate::sealed::Sealed;

/// A column of variable-length UTF-8 strings.
///
/// Use the [`StringArray`] and [`LargeStringArray`] aliases rather than naming
/// the offset parameter directly.
#[derive(Clone)]
pub struct GenericStringArray<O: OffsetSizeTrait> {
    /// The byte layout. Every slot is guaranteed to be valid UTF-8.
    inner: GenericBinaryArray<O>,
}

/// `Utf8` column — variable-length UTF-8 with 32-bit offsets.
pub type StringArray = GenericStringArray<i32>;
/// `LargeUtf8` column — variable-length UTF-8 with 64-bit offsets.
pub type LargeStringArray = GenericStringArray<i64>;

/// Checks that every slot the offsets carve out of `values` is valid UTF-8.
///
/// Assumes the offsets themselves have already passed
/// [`crate::array::validate_offsets`].
///
/// # Errors
///
/// * [`DataError::InvalidUtf8`] — the covered region is not valid UTF-8.
/// * [`DataError::OffsetNotCharBoundary`] — an offset splits a code point.
fn validate_utf8<O: OffsetSizeTrait>(offsets: &[O], values: &[u8]) -> Result<()> {
    let (Some(first), Some(last)) = (offsets.first(), offsets.last()) else {
        return Ok(());
    };
    let start = first.to_usize().unwrap_or(0);
    let end = last.to_usize().unwrap_or(0);
    let Some(region) = values.get(start..end) else {
        return Err(DataError::OffsetOutOfBounds {
            index: offsets.len().saturating_sub(1),
            offset: end,
            values_len: values.len(),
        });
    };
    let text = std::str::from_utf8(region).map_err(|error| DataError::InvalidUtf8 {
        valid_up_to: start + error.valid_up_to(),
    })?;
    for (index, offset) in offsets.iter().enumerate() {
        let absolute = offset.to_usize().unwrap_or(0);
        let relative = absolute.saturating_sub(start);
        if !text.is_char_boundary(relative) {
            return Err(DataError::OffsetNotCharBoundary {
                index,
                offset: absolute,
            });
        }
    }
    Ok(())
}

impl<O: OffsetSizeTrait> GenericStringArray<O> {
    /// Builds an array from its three buffers, validating offsets *and* UTF-8.
    ///
    /// This is the constructor for untrusted bytes.
    ///
    /// # Errors
    ///
    /// * The offset errors listed on [`crate::array::validate_offsets`].
    /// * [`DataError::InvalidUtf8`] and [`DataError::OffsetNotCharBoundary`].
    /// * [`DataError::ValidityLengthMismatch`] when the bitmap length is wrong.
    pub fn try_new(
        offsets: ScalarBuffer<O>,
        values: Buffer,
        validity: Option<Bitmap>,
    ) -> Result<Self> {
        let inner = GenericBinaryArray::try_new(offsets, values, validity)?;
        validate_utf8(inner.value_offsets(), inner.value_data().as_slice())?;
        Ok(Self { inner })
    }

    /// Builds an array from its three buffers **without validating**.
    ///
    /// # Safety
    ///
    /// This constructor is `unsafe` by contract, not by memory model: the
    /// accessors stay memory-safe whatever the bytes say, returning `None` for
    /// a slot that is not valid UTF-8. Skipping validation turns a decode
    /// error into a silent `None`, so only use it on the trusted zero-copy
    /// path — bytes this crate produced, or bytes a caller has already
    /// validated.
    ///
    /// Callers must guarantee the offsets satisfy
    /// [`crate::array::validate_offsets`] and that every slot they carve out is
    /// valid UTF-8.
    #[must_use]
    pub unsafe fn new_unchecked(
        offsets: ScalarBuffer<O>,
        values: Buffer,
        validity: Option<Bitmap>,
    ) -> Self {
        Self::from_parts(offsets, values, validity)
    }

    /// Assembles the parts without revalidating. See
    /// [`GenericBinaryArray::from_parts`].
    pub(crate) fn from_parts(
        offsets: ScalarBuffer<O>,
        values: Buffer,
        validity: Option<Bitmap>,
    ) -> Self {
        debug_assert!(validate_utf8(offsets.as_slice(), values.as_slice()).is_ok());
        Self {
            inner: GenericBinaryArray::from_parts(offsets, values, validity),
        }
    }

    /// Builds an array with no nulls.
    ///
    /// ```
    /// use astrs_data::array::{Array, StringArray};
    ///
    /// let names = StringArray::from_values(["lidar", "camera"]);
    /// assert_eq!(names.len(), 2);
    /// assert_eq!(names.get(1), Some("camera"));
    /// ```
    #[must_use]
    pub fn from_values<V: AsRef<str>>(values: impl IntoIterator<Item = V>) -> Self {
        Self {
            inner: GenericBinaryArray::from_values(values.into_iter().map(|value| BytesOf(value))),
        }
    }

    /// Builds an array from optional strings.
    #[must_use]
    pub fn from_opt_iter<V: AsRef<str>>(values: impl IntoIterator<Item = Option<V>>) -> Self {
        Self {
            inner: GenericBinaryArray::from_opt_iter(
                values.into_iter().map(|value| value.map(BytesOf)),
            ),
        }
    }

    /// An all-null array of `len` slots.
    #[must_use]
    pub fn new_null(len: usize) -> Self {
        Self {
            inner: GenericBinaryArray::new_null(len),
        }
    }

    /// The offset buffer, `len + 1` entries.
    #[inline]
    #[must_use]
    pub fn value_offsets(&self) -> &[O] {
        self.inner.value_offsets()
    }

    /// The offset buffer as a shareable typed window.
    ///
    /// See [`GenericBinaryArray::offsets_buffer`]: the IPC encoder clones this
    /// window instead of copying the offsets out of a slice.
    ///
    /// ```
    /// use astrs_data::array::StringArray;
    ///
    /// let array = StringArray::from_values(["ab", "c"]);
    /// assert_eq!(array.offsets_buffer().as_slice(), &[0, 2, 3]);
    /// ```
    #[inline]
    #[must_use]
    pub const fn offsets_buffer(&self) -> &ScalarBuffer<O> {
        self.inner.offsets_buffer()
    }

    /// The flat value region every slot points into.
    #[inline]
    #[must_use]
    pub const fn value_data(&self) -> &Buffer {
        self.inner.value_data()
    }

    /// The raw slot bytes at `index`, ignoring validity and skipping the UTF-8
    /// check.
    ///
    /// The cheapest accessor: no validation at all.
    #[inline]
    #[must_use]
    pub fn value_bytes(&self, index: usize) -> Option<&[u8]> {
        self.inner.value(index)
    }

    /// The raw slot text at `index`, ignoring validity.
    ///
    /// Returns `None` when `index` is out of range (or, on the
    /// [`Self::new_unchecked`] path only, when the slot is not valid UTF-8).
    #[inline]
    #[must_use]
    pub fn value(&self, index: usize) -> Option<&str> {
        std::str::from_utf8(self.inner.value(index)?).ok()
    }

    /// The logical value at `index`: `None` when the slot is null or out of
    /// range.
    #[inline]
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&str> {
        std::str::from_utf8(self.inner.get(index)?).ok()
    }

    /// Byte length of slot `index` — *not* the character count.
    #[inline]
    #[must_use]
    pub fn value_length(&self, index: usize) -> Option<usize> {
        self.inner.value_length(index)
    }

    /// Total bytes the values occupy, across the whole array.
    #[must_use]
    pub fn total_value_bytes(&self) -> usize {
        self.inner.total_value_bytes()
    }

    /// Iterates over the logical values, `None` for nulls.
    #[inline]
    pub fn iter(&self) -> ArrayIter<&Self> {
        ArrayIter::new(self)
    }

    /// A zero-copy sub-range, clamped to the array (the crate-wide slicing
    /// convention).
    ///
    /// Slicing preserves the UTF-8 invariant: it narrows the offset buffer
    /// without changing any offset, and every retained offset was already
    /// checked to be a character boundary.
    #[must_use]
    pub fn slice(&self, offset: usize, len: usize) -> Self {
        Self {
            inner: self.inner.slice(offset, len),
        }
    }

    /// Checked [`GenericStringArray::slice`].
    ///
    /// # Errors
    ///
    /// [`DataError::SliceOutOfBounds`] when the window leaves the array.
    pub fn try_slice(&self, offset: usize, len: usize) -> Result<Self> {
        Ok(Self {
            inner: self.inner.try_slice(offset, len)?,
        })
    }

    /// Returns a copy with a different validity bitmap.
    ///
    /// # Errors
    ///
    /// [`DataError::ValidityLengthMismatch`] when the bitmap length is wrong.
    pub fn with_validity(&self, validity: Option<Bitmap>) -> Result<Self> {
        Ok(Self {
            inner: self.inner.with_validity(validity)?,
        })
    }

    /// Reinterprets the column as bytes, dropping the UTF-8 guarantee.
    ///
    /// Free: the two types share a representation.
    #[must_use]
    pub fn into_binary(self) -> GenericBinaryArray<O> {
        self.inner
    }

    /// The byte view of this column.
    #[inline]
    #[must_use]
    pub const fn as_binary(&self) -> &GenericBinaryArray<O> {
        &self.inner
    }
}

/// Adapts an `AsRef<str>` to the `AsRef<[u8]>` the binary builder wants.
struct BytesOf<V: AsRef<str>>(V);

impl<V: AsRef<str>> AsRef<[u8]> for BytesOf<V> {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.0.as_ref().as_bytes()
    }
}

impl<O: OffsetSizeTrait> TryFrom<GenericBinaryArray<O>> for GenericStringArray<O> {
    type Error = DataError;

    /// Validates the bytes, then reinterprets them as text without copying.
    fn try_from(inner: GenericBinaryArray<O>) -> Result<Self> {
        validate_utf8(inner.value_offsets(), inner.value_data().as_slice())?;
        Ok(Self { inner })
    }
}

impl<O: OffsetSizeTrait> Sealed for GenericStringArray<O> {}

impl<O: OffsetSizeTrait> Array for GenericStringArray<O> {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn data_type(&self) -> &DataType {
        if O::IS_LARGE {
            &LARGE_UTF8_TYPE
        } else {
            &UTF8_TYPE
        }
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

impl<'a, O: OffsetSizeTrait> ArrayAccessor for &'a GenericStringArray<O> {
    type Item = &'a str;

    #[inline]
    fn accessor_len(&self) -> usize {
        self.inner.len()
    }

    #[inline]
    fn accessor_get(&self, index: usize) -> Option<&'a str> {
        let array: &'a GenericStringArray<O> = self;
        array.get(index)
    }
}

impl<O: OffsetSizeTrait> PartialEq for GenericStringArray<O> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

impl<O: OffsetSizeTrait> Eq for GenericStringArray<O> {}

impl<O: OffsetSizeTrait, V: AsRef<str>> FromIterator<Option<V>> for GenericStringArray<O> {
    fn from_iter<I: IntoIterator<Item = Option<V>>>(iter: I) -> Self {
        Self::from_opt_iter(iter)
    }
}

impl<'a, O: OffsetSizeTrait> IntoIterator for &'a GenericStringArray<O> {
    type Item = Option<&'a str>;
    type IntoIter = ArrayIter<&'a GenericStringArray<O>>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl<O: OffsetSizeTrait> fmt::Debug for GenericStringArray<O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        debug_array(
            f,
            if O::IS_LARGE {
                "LargeStringArray"
            } else {
                "StringArray"
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
        let array = StringArray::from_values([] as [&str; 0]);
        assert!(array.is_empty());
        assert_eq!(array.null_count(), 0);
        assert_eq!(array.get(0), None);
        assert_eq!(array.iter().count(), 0);
        assert_eq!(array.data_type(), &DataType::Utf8);
        assert_eq!(array.total_value_bytes(), 0);
    }

    #[test]
    fn no_null_array() {
        let array = StringArray::from_values(["lidar", "", "camera"]);
        assert_eq!(array.len(), 3);
        assert_eq!(array.null_count(), 0);
        assert!(array.validity().is_none());
        assert_eq!(array.get(0), Some("lidar"));
        assert_eq!(array.get(1), Some(""));
        assert_eq!(array.get(2), Some("camera"));
        assert_eq!(array.get(3), None);
        assert_eq!(array.value_length(0), Some(5));
        assert_eq!(array.value_bytes(2), Some(&b"camera"[..]));
        assert_eq!(array.value_data().as_slice(), b"lidarcamera");
    }

    #[test]
    fn all_null_array() {
        let array = LargeStringArray::new_null(3);
        assert_eq!(array.len(), 3);
        assert_eq!(array.null_count(), 3);
        assert_eq!(array.data_type(), &DataType::LargeUtf8);
        for index in 0..3 {
            assert!(array.is_null(index));
            assert_eq!(array.get(index), None);
            assert_eq!(array.value(index), Some(""));
        }
    }

    #[test]
    fn mixed_nulls() {
        let array = StringArray::from_opt_iter([Some("a"), None, Some("ccc"), None]);
        assert_eq!(array.len(), 4);
        assert_eq!(array.null_count(), 2);
        assert_eq!(
            array.iter().collect::<Vec<_>>(),
            vec![Some("a"), None, Some("ccc"), None]
        );
        assert_eq!(array.total_value_bytes(), 4);
    }

    #[test]
    fn multibyte_text_round_trips() {
        let values = ["日本語", "é", "🤖", "mixed 日本 text"];
        let array = StringArray::from_values(values);
        for (index, expected) in values.iter().enumerate() {
            assert_eq!(array.get(index), Some(*expected));
            assert_eq!(array.value_length(index), Some(expected.len()));
        }
        assert_eq!(array.get(2).map(str::chars).map(Iterator::count), Some(1));
    }

    #[test]
    fn valid_region_with_a_split_code_point_is_rejected() {
        // "é" is two bytes; an offset at 1 lands mid-code-point even though
        // the whole region is valid UTF-8.
        let err = StringArray::try_new(
            ScalarBuffer::from_slice(&[0, 1, 2]),
            Buffer::from_slice("é".as_bytes()),
            None,
        )
        .unwrap_err();
        assert_eq!(
            err,
            DataError::OffsetNotCharBoundary {
                index: 1,
                offset: 1
            }
        );

        // A four-byte emoji split three ways.
        let emoji = "🤖".as_bytes();
        for split in 1..4 {
            let err = StringArray::try_new(
                ScalarBuffer::from_slice(&[0, split, 4]),
                Buffer::from_slice(emoji),
                None,
            )
            .unwrap_err();
            assert!(
                matches!(err, DataError::OffsetNotCharBoundary { .. }),
                "split {split} accepted"
            );
        }
    }

    #[test]
    fn invalid_bytes_are_rejected() {
        let err = StringArray::try_new(
            ScalarBuffer::from_slice(&[0, 2]),
            Buffer::from_slice(&[0xff, 0xfe]),
            None,
        )
        .unwrap_err();
        assert_eq!(err, DataError::InvalidUtf8 { valid_up_to: 0 });

        let err = StringArray::try_new(
            ScalarBuffer::from_slice(&[0, 4]),
            Buffer::from_slice(b"ok\xff\xfe"),
            None,
        )
        .unwrap_err();
        assert_eq!(err, DataError::InvalidUtf8 { valid_up_to: 2 });

        // A truncated multi-byte sequence at the very end.
        let mut truncated = "日".as_bytes().to_vec();
        truncated.pop();
        assert!(matches!(
            StringArray::try_new(
                ScalarBuffer::from_slice(&[0, 2]),
                Buffer::from_slice(&truncated),
                None
            ),
            Err(DataError::InvalidUtf8 { .. })
        ));
    }

    #[test]
    fn validation_reports_offsets_relative_to_the_buffer() {
        // A sliced array's region starts partway into the value buffer; the
        // reported `valid_up_to` must be an absolute buffer offset.
        let values = Buffer::from_slice(b"good\xffbad");
        let err =
            StringArray::try_new(ScalarBuffer::from_slice(&[4, 8]), values, None).unwrap_err();
        assert_eq!(err, DataError::InvalidUtf8 { valid_up_to: 4 });
    }

    #[test]
    fn offsets_are_still_validated() {
        let values = Buffer::from_slice(b"abc");
        assert!(matches!(
            StringArray::try_new(ScalarBuffer::from_slice(&[0, 9]), values.clone(), None),
            Err(DataError::OffsetOutOfBounds { .. })
        ));
        assert!(matches!(
            StringArray::try_new(ScalarBuffer::from_slice(&[0, 3, 1]), values, None),
            Err(DataError::NonMonotonicOffsets { .. })
        ));
    }

    #[test]
    fn slicing_preserves_the_utf8_invariant() {
        let array = StringArray::from_values(["日本語", "é", "plain", "🤖"]);
        for offset in 0..4 {
            for len in 0..=(4 - offset) {
                let window = array.slice(offset, len);
                assert_eq!(window.len(), len);
                for index in 0..len {
                    assert_eq!(window.get(index), array.get(offset + index));
                }
                // Rebuilding from the sliced buffers must pass validation.
                assert!(
                    StringArray::try_new(
                        ScalarBuffer::from_slice(window.value_offsets()),
                        window.value_data().clone(),
                        None,
                    )
                    .is_ok(),
                    "offset {offset} len {len}"
                );
            }
        }
    }

    #[test]
    fn slicing_clamps() {
        let array = StringArray::from_values(["a", "b", "c"]);
        assert_eq!(array.slice(2, 99).len(), 1);
        assert_eq!(array.slice(99, 1).len(), 0);
        assert_eq!(array.try_slice(1, 2).unwrap().len(), 2);
        assert!(array.try_slice(2, 2).is_err());
    }

    #[test]
    fn unchecked_constructor_matches_the_checked_one() {
        let offsets = ScalarBuffer::from_slice(&[0i32, 3, 6]);
        let values = Buffer::from_slice(b"abcdef");
        let checked = StringArray::try_new(offsets.clone(), values.clone(), None).unwrap();
        // SAFETY: the same buffers just passed validation above.
        let unchecked = unsafe { StringArray::new_unchecked(offsets, values, None) };
        assert_eq!(checked, unchecked);
        assert_eq!(unchecked.get(1), Some("def"));
    }

    #[test]
    fn binary_conversions_are_free() {
        let text = StringArray::from_opt_iter([Some("a"), None, Some("bb")]);
        let bytes = text.clone().into_binary();
        assert_eq!(bytes.get(2), Some(&b"bb"[..]));
        assert_eq!(bytes.null_count(), 1);
        let back = StringArray::try_from(bytes).unwrap();
        assert_eq!(back, text);
        assert_eq!(text.as_binary().len(), 3);

        let invalid = crate::array::BinaryArray::from_values([&[0xffu8, 0xfe][..]]);
        assert!(StringArray::try_from(invalid).is_err());
    }

    #[test]
    fn both_offset_widths_work() {
        let small = StringArray::from_values(["x"]);
        let large = LargeStringArray::from_values(["x"]);
        assert_eq!(small.data_type(), &DataType::Utf8);
        assert_eq!(large.data_type(), &DataType::LargeUtf8);
        assert_eq!(small.get(0), large.get(0));
        let small_ref: ArrayRef = Arc::new(small);
        let large_ref: ArrayRef = Arc::new(large);
        assert_ne!(small_ref, large_ref, "different data types");
    }

    #[test]
    fn equality_is_by_logical_content() {
        let a = StringArray::from_opt_iter([Some("aa"), None, Some("cc")]);
        let padded = StringArray::from_opt_iter([Some("zz"), Some("aa"), None, Some("cc")]);
        assert_eq!(a, padded.slice(1, 3));
        assert_ne!(a, StringArray::from_values(["aa", "cc"]));

        let text_ref: ArrayRef = Arc::new(a.clone());
        let bytes_ref: ArrayRef = Arc::new(a.into_binary());
        assert_ne!(text_ref, bytes_ref, "Utf8 never equals Binary");
    }

    #[test]
    fn with_validity_replaces_the_bitmap() {
        let array = StringArray::from_values(["a", "b"]);
        let masked = array
            .with_validity(Some([true, false].into_iter().collect()))
            .unwrap();
        assert_eq!(masked.null_count(), 1);
        assert_eq!(masked.get(1), None);
        assert_eq!(masked.value(1), Some("b"), "the bytes are still there");
        assert!(array.with_validity(Some(Bitmap::new_set(9))).is_err());
    }

    #[test]
    fn collects_from_iterators() {
        let collected: StringArray = [Some("a".to_owned()), None].into_iter().collect();
        assert_eq!(collected.len(), 2);
        assert_eq!(collected.null_count(), 1);
        let owned = StringArray::from_values(vec!["one".to_owned(), "two".to_owned()]);
        assert_eq!(owned.get(0), Some("one"));
        assert_eq!((&owned).into_iter().count(), 2);
    }

    #[test]
    fn debug_output_names_the_offset_width() {
        let small = StringArray::from_opt_iter([Some("a"), None]);
        let rendered = format!("{small:?}");
        assert!(rendered.starts_with("StringArray"), "{rendered}");
        assert!(rendered.contains("nulls=1"), "{rendered}");
        assert!(
            format!("{:?}", LargeStringArray::from_values(["a"])).starts_with("LargeStringArray")
        );
    }

    #[test]
    fn empty_offsets_pass_validation_vacuously() {
        assert!(validate_utf8(&[] as &[i32], b"anything").is_ok());
        assert!(validate_utf8(&[0i32], b"").is_ok());
    }

    #[test]
    fn large_text_column() {
        let values: Vec<String> = (0..5_000).map(|i| format!("node-{i}-日本語")).collect();
        let array = LargeStringArray::from_values(&values);
        assert_eq!(array.len(), 5_000);
        assert_eq!(array.get(4_999), Some(values[4_999].as_str()));
        let window = array.slice(2_500, 100);
        assert_eq!(window.get(0), Some(values[2_500].as_str()));
    }
}
