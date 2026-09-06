//! [`ScalarBuffer<T>`] — a [`Buffer`] reinterpreted as a slice of fixed-width
//! scalars.
//!
//! # Safety model
//!
//! This is the second (and last) module in `astrs-data` that contains
//! `unsafe`. Two invariants make the reinterpretation sound, and
//! [`ScalarBuffer::try_new`] is the only way to establish them from an
//! arbitrary window:
//!
//! 1. **Alignment.** `buffer.as_ptr()` is aligned to `align_of::<T>()`.
//! 2. **Whole elements.** `buffer.len() % size_of::<T>() == 0`.
//!
//! A third precondition comes from the type system rather than a runtime
//! check: [`ArrowNativeType`] is sealed ([`crate::sealed::Sealed`]) and
//! implemented only for `i8..i64`, `u8..u64`, `f32`, `f64` and [`crate::F16`].
//! Every one of those is a fixed-width scalar with no padding bytes, no
//! invalid bit patterns, no interior mutability and no `Drop`, so *any* byte
//! sequence of the right length and alignment is a valid `[T]`. **Adding an
//! implementation for a type that does not meet that bar would make this
//! module unsound** — the sealed bound exists to make that a deliberate,
//! reviewable act.
//!
//! ```
//! use astrs_data::{Buffer, ScalarBuffer};
//!
//! let values = ScalarBuffer::from_slice(&[10i32, 20, 30, 40]);
//! assert_eq!(values.as_slice(), &[10, 20, 30, 40]);
//!
//! // Slicing is by element and stays zero-copy.
//! let tail = values.slice(2, 2);
//! assert_eq!(tail.as_slice(), &[30, 40]);
//! # Ok::<(), astrs_data::DataError>(())
//! ```

use std::fmt;
use std::marker::PhantomData;

use crate::buffer::Buffer;
use crate::datatype::ArrowNativeType;
use crate::error::{DataError, Result};

/// Reinterprets a typed slice as its native bytes.
///
/// Used by [`Buffer::from_scalars`] and [`ScalarBuffer::from_slice`].
#[inline]
#[must_use]
pub(crate) fn as_bytes<T: ArrowNativeType>(values: &[T]) -> &[u8] {
    // SAFETY: `T: ArrowNativeType` is sealed to padding-free plain-old-data
    // scalars (see the module documentation), so every byte of `values` is
    // initialised and the region is exactly `size_of_val(values)` bytes long.
    // The returned slice borrows `values`, so nothing can mutate it meanwhile,
    // and `u8` has alignment 1 so any address works.
    unsafe {
        std::slice::from_raw_parts(values.as_ptr().cast::<u8>(), std::mem::size_of_val(values))
    }
}

/// Appends one scalar's native bytes to an aligned buffer.
///
/// The builders use this to write straight into their final allocation
/// instead of staging values in a `Vec<T>` and copying at `finish`.
#[inline]
pub(crate) fn push_scalar<T: ArrowNativeType>(buf: &mut crate::buffer::AlignedBuf, value: T) {
    buf.extend_from_slice(as_bytes(std::slice::from_ref(&value)));
}

/// Appends many scalars' native bytes to an aligned buffer.
#[inline]
pub(crate) fn extend_scalars<T: ArrowNativeType>(
    buf: &mut crate::buffer::AlignedBuf,
    values: &[T],
) {
    buf.extend_from_slice(as_bytes(values));
}

/// A shared, immutable slice of `T` backed by an aligned [`Buffer`].
///
/// This is what [`crate::array::PrimitiveArray`] stores. Cloning and slicing
/// are `O(1)`; the element type is checked once, at construction.
pub struct ScalarBuffer<T: ArrowNativeType> {
    /// The byte window. Satisfies invariants 1 and 2 of the module docs.
    buffer: Buffer,
    /// `ScalarBuffer<T>` behaves like a shared `[T]`.
    marker: PhantomData<T>,
}

impl<T: ArrowNativeType> ScalarBuffer<T> {
    /// Width of one element, in bytes.
    pub const ELEMENT_SIZE: usize = std::mem::size_of::<T>();

    /// An empty typed buffer. Allocates nothing.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buffer: Buffer::new(),
            marker: PhantomData,
        }
    }

    /// Reinterprets an existing byte window as `[T]`.
    ///
    /// # Errors
    ///
    /// * [`DataError::UnalignedBuffer`] when the window's start address is not
    ///   aligned for `T`.
    /// * [`DataError::BufferLengthNotMultiple`] when the window is not a whole
    ///   number of elements.
    ///
    /// ```
    /// use astrs_data::{Buffer, DataError, ScalarBuffer};
    ///
    /// let bytes = Buffer::from_slice(&[0u8; 64]);
    /// assert!(ScalarBuffer::<i32>::try_new(bytes.clone()).is_ok());
    ///
    /// // One byte in, and both invariants break.
    /// let skewed = bytes.slice(1, 8);
    /// assert!(matches!(
    ///     ScalarBuffer::<i32>::try_new(skewed),
    ///     Err(DataError::UnalignedBuffer { .. })
    /// ));
    ///
    /// let ragged = bytes.slice(0, 7);
    /// assert!(matches!(
    ///     ScalarBuffer::<i32>::try_new(ragged),
    ///     Err(DataError::BufferLengthNotMultiple { .. })
    /// ));
    /// ```
    pub fn try_new(buffer: Buffer) -> Result<Self> {
        let align = std::mem::align_of::<T>();
        if !buffer.is_aligned_to(align) {
            return Err(DataError::UnalignedBuffer {
                required: align,
                actual: buffer.address_alignment(),
            });
        }
        if Self::ELEMENT_SIZE == 0 || !buffer.len().is_multiple_of(Self::ELEMENT_SIZE) {
            return Err(DataError::BufferLengthNotMultiple {
                len: buffer.len(),
                width: Self::ELEMENT_SIZE,
            });
        }
        Ok(Self {
            buffer,
            marker: PhantomData,
        })
    }

    /// Reinterprets a byte window as `[T]`, copying into a fresh aligned
    /// allocation when the window does not already satisfy the invariants.
    ///
    /// The trailing bytes of a ragged window are dropped, so the caller must
    /// have already validated the element count if that matters.
    #[must_use]
    pub fn from_buffer_lossy(buffer: &Buffer) -> Self {
        let usable = buffer.len() - buffer.len() % Self::ELEMENT_SIZE.max(1);
        let trimmed = buffer.slice(0, usable);
        match Self::try_new(trimmed.clone()) {
            Ok(typed) => typed,
            Err(_) => Self {
                buffer: trimmed.realigned(),
                marker: PhantomData,
            },
        }
    }

    /// Copies a typed slice into a fresh aligned allocation.
    #[must_use]
    pub fn from_slice(values: &[T]) -> Self {
        Self {
            buffer: Buffer::from_slice(as_bytes(values)),
            marker: PhantomData,
        }
    }

    /// A buffer of `len` default-valued (all-zero-bytes) elements.
    ///
    /// ```
    /// use astrs_data::ScalarBuffer;
    ///
    /// assert_eq!(ScalarBuffer::<u32>::zeroed(3).as_slice(), &[0, 0, 0]);
    /// ```
    #[must_use]
    pub fn zeroed(len: usize) -> Self {
        Self {
            buffer: Buffer::zeroed(len.saturating_mul(Self::ELEMENT_SIZE)),
            marker: PhantomData,
        }
    }

    /// Number of elements.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        match self.buffer.len().checked_div(Self::ELEMENT_SIZE) {
            Some(count) => count,
            // Unreachable for the sealed `ArrowNativeType` set, whose every
            // member is at least one byte wide. Kept total so this `const fn`
            // carries no panicking path at all.
            None => 0,
        }
    }

    /// Returns `true` when there are no elements.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The elements.
    #[inline]
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        let len = self.len();
        if len == 0 {
            return &[];
        }
        // SAFETY: invariants 1 and 2 were established by `try_new` (or by a
        // constructor that allocated the window itself), and `T` is a
        // padding-free plain-old-data scalar, so the `len * size_of::<T>()`
        // initialised bytes of the window are a valid `[T]`. The lifetime is
        // tied to `&self`, and `Buffer` never hands out `&mut`, so no mutation
        // can race with the borrow.
        unsafe { std::slice::from_raw_parts(self.buffer.as_ptr().cast::<T>(), len) }
    }

    /// The element at `index`, or `None` when out of range.
    #[inline]
    #[must_use]
    pub fn get(&self, index: usize) -> Option<T> {
        self.as_slice().get(index).copied()
    }

    /// The underlying byte window.
    #[inline]
    #[must_use]
    pub const fn inner(&self) -> &Buffer {
        &self.buffer
    }

    /// Consumes the typed view, returning the byte window.
    #[inline]
    #[must_use]
    pub fn into_inner(self) -> Buffer {
        self.buffer
    }

    /// A zero-copy sub-range measured in *elements*, clamped to the available
    /// range (the crate-wide slicing convention).
    ///
    /// Element-granular slicing preserves both invariants, so the result never
    /// needs revalidation.
    #[must_use]
    pub fn slice(&self, offset: usize, len: usize) -> Self {
        let offset = offset.min(self.len());
        let len = len.min(self.len() - offset);
        Self {
            buffer: self
                .buffer
                .slice(offset * Self::ELEMENT_SIZE, len * Self::ELEMENT_SIZE),
            marker: PhantomData,
        }
    }

    /// Checked [`ScalarBuffer::slice`].
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

    /// Iterates over the elements by value.
    #[inline]
    pub fn iter(&self) -> std::iter::Copied<std::slice::Iter<'_, T>> {
        self.as_slice().iter().copied()
    }

    /// Copies the elements into a `Vec`.
    #[must_use]
    pub fn to_vec(&self) -> Vec<T> {
        self.as_slice().to_vec()
    }
}

impl<T: ArrowNativeType> Clone for ScalarBuffer<T> {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            buffer: self.buffer.clone(),
            marker: PhantomData,
        }
    }
}

impl<T: ArrowNativeType> Default for ScalarBuffer<T> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<T: ArrowNativeType> std::ops::Deref for ScalarBuffer<T> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T: ArrowNativeType> AsRef<[T]> for ScalarBuffer<T> {
    #[inline]
    fn as_ref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T: ArrowNativeType> From<&[T]> for ScalarBuffer<T> {
    #[inline]
    fn from(values: &[T]) -> Self {
        Self::from_slice(values)
    }
}

impl<T: ArrowNativeType> From<Vec<T>> for ScalarBuffer<T> {
    #[inline]
    fn from(values: Vec<T>) -> Self {
        Self::from_slice(&values)
    }
}

impl<T: ArrowNativeType, const N: usize> From<[T; N]> for ScalarBuffer<T> {
    #[inline]
    fn from(values: [T; N]) -> Self {
        Self::from_slice(&values)
    }
}

impl<T: ArrowNativeType> FromIterator<T> for ScalarBuffer<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let values: Vec<T> = iter.into_iter().collect();
        Self::from_slice(&values)
    }
}

impl<T: ArrowNativeType> PartialEq for ScalarBuffer<T> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl<T: ArrowNativeType + Eq> Eq for ScalarBuffer<T> {}

impl<T: ArrowNativeType> fmt::Debug for ScalarBuffer<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const PREVIEW: usize = 12;
        let values = self.as_slice();
        f.debug_struct("ScalarBuffer")
            .field("type", &std::any::type_name::<T>())
            .field("len", &values.len())
            .field("head", &&values[..values.len().min(PREVIEW)])
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::F16;
    use crate::buffer::ALIGNMENT;

    const fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn typed_buffers_are_send_and_sync() {
        assert_send_sync::<ScalarBuffer<i64>>();
        assert_send_sync::<ScalarBuffer<F16>>();
    }

    #[test]
    fn round_trips_every_native_width() {
        assert_eq!(ScalarBuffer::from_slice(&[1i8, -2]).as_slice(), &[1, -2]);
        assert_eq!(ScalarBuffer::from_slice(&[1i16, -2]).as_slice(), &[1, -2]);
        assert_eq!(ScalarBuffer::from_slice(&[1i32, -2]).as_slice(), &[1, -2]);
        assert_eq!(ScalarBuffer::from_slice(&[1i64, -2]).as_slice(), &[1, -2]);
        assert_eq!(ScalarBuffer::from_slice(&[1u8, 2]).as_slice(), &[1, 2]);
        assert_eq!(ScalarBuffer::from_slice(&[1u16, 2]).as_slice(), &[1, 2]);
        assert_eq!(ScalarBuffer::from_slice(&[1u32, 2]).as_slice(), &[1, 2]);
        assert_eq!(ScalarBuffer::from_slice(&[1u64, 2]).as_slice(), &[1, 2]);
        assert_eq!(ScalarBuffer::from_slice(&[1.5f32]).as_slice(), &[1.5]);
        assert_eq!(ScalarBuffer::from_slice(&[1.5f64]).as_slice(), &[1.5]);
        let halves = [F16::from_f32(1.5), F16::from_f32(-0.25)];
        assert_eq!(ScalarBuffer::from_slice(&halves).as_slice(), &halves);
    }

    #[test]
    fn empty_typed_buffer() {
        let empty = ScalarBuffer::<i32>::new();
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
        assert_eq!(empty.as_slice(), &[] as &[i32]);
        assert_eq!(empty, ScalarBuffer::default());
    }

    #[test]
    fn element_slicing_preserves_alignment() {
        let values = ScalarBuffer::from_slice(&(0i64..32).collect::<Vec<_>>());
        for offset in 0..8 {
            let sliced = values.slice(offset, 4);
            assert_eq!(sliced.len(), 4);
            assert_eq!(sliced.as_slice()[0], offset as i64);
            assert_eq!(
                sliced.inner().as_ptr() as usize % std::mem::align_of::<i64>(),
                0
            );
            // The element view must still validate as a typed buffer.
            assert!(ScalarBuffer::<i64>::try_new(sliced.into_inner()).is_ok());
        }
    }

    #[test]
    fn slicing_clamps() {
        let values = ScalarBuffer::from_slice(&[1u32, 2, 3]);
        assert_eq!(values.slice(1, 99).as_slice(), &[2, 3]);
        assert!(values.slice(99, 1).is_empty());
        assert_eq!(
            values.try_slice(1, 3).unwrap_err(),
            DataError::SliceOutOfBounds {
                offset: 1,
                len: 3,
                available: 3
            }
        );
        assert_eq!(values.try_slice(1, 2).unwrap().as_slice(), &[2, 3]);
    }

    #[test]
    fn try_new_rejects_misaligned_windows() {
        let bytes = Buffer::from_slice(&[0u8; 128]);
        assert!(ScalarBuffer::<u64>::try_new(bytes.clone()).is_ok());

        let err = ScalarBuffer::<u64>::try_new(bytes.slice(1, 16)).unwrap_err();
        assert!(matches!(
            err,
            DataError::UnalignedBuffer { required: 8, .. }
        ));

        let err = ScalarBuffer::<u64>::try_new(bytes.slice(0, 12)).unwrap_err();
        assert_eq!(
            err,
            DataError::BufferLengthNotMultiple { len: 12, width: 8 }
        );

        // u8 has alignment 1, so any window works.
        assert!(ScalarBuffer::<u8>::try_new(bytes.slice(3, 5)).is_ok());
    }

    #[test]
    fn from_buffer_lossy_realigns_and_trims() {
        let bytes = Buffer::from_slice(&[0u8; 128]);
        let skewed = bytes.slice(1, 15);
        let typed = ScalarBuffer::<u32>::from_buffer_lossy(&skewed);
        assert_eq!(typed.len(), 3, "15 bytes trims to 3 x u32");
        assert!(typed.inner().is_aligned_to(ALIGNMENT));

        let clean = ScalarBuffer::<u32>::from_buffer_lossy(&bytes);
        assert_eq!(clean.len(), 32);
    }

    #[test]
    fn accessors_and_iteration() {
        let values = ScalarBuffer::from_slice(&[5i32, 6, 7]);
        assert_eq!(values.get(0), Some(5));
        assert_eq!(values.get(2), Some(7));
        assert_eq!(values.get(3), None);
        assert_eq!(values.iter().sum::<i32>(), 18);
        assert_eq!(values.to_vec(), vec![5, 6, 7]);
        assert_eq!(&*values, &[5, 6, 7]);
        assert_eq!(values.as_ref(), &[5, 6, 7]);
    }

    #[test]
    fn conversions_and_collect() {
        assert_eq!(ScalarBuffer::from(vec![1u16, 2]).as_slice(), &[1, 2]);
        assert_eq!(ScalarBuffer::from([1u16, 2]).as_slice(), &[1, 2]);
        assert_eq!(ScalarBuffer::from(&[1u16, 2][..]).as_slice(), &[1, 2]);
        let collected: ScalarBuffer<i16> = (0i16..4).collect();
        assert_eq!(collected.as_slice(), &[0, 1, 2, 3]);
        assert_eq!(ScalarBuffer::<i8>::zeroed(4).as_slice(), &[0; 4]);
    }

    #[test]
    fn equality_and_debug() {
        let a = ScalarBuffer::from_slice(&[1i32, 2]);
        let b = ScalarBuffer::from_slice(&[1i32, 2]);
        assert_eq!(a, b);
        assert_ne!(a, ScalarBuffer::from_slice(&[1i32, 3]));
        let rendered = format!("{a:?}");
        assert!(rendered.contains("len: 2"), "{rendered}");
        assert!(rendered.contains("i32"), "{rendered}");
    }

    #[test]
    fn element_size_constant() {
        assert_eq!(ScalarBuffer::<i8>::ELEMENT_SIZE, 1);
        assert_eq!(ScalarBuffer::<F16>::ELEMENT_SIZE, 2);
        assert_eq!(ScalarBuffer::<f64>::ELEMENT_SIZE, 8);
    }

    #[test]
    fn large_typed_buffer_stays_aligned() {
        let values: ScalarBuffer<f64> = (0..10_000).map(f64::from).collect();
        assert_eq!(values.len(), 10_000);
        assert!(values.inner().is_aligned_to(ALIGNMENT));
        assert_eq!(values.as_slice()[9_999], 9_999.0);
    }
}
