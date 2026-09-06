//! Aligned memory: owned buffers, shared zero-copy windows, typed views and
//! validity bitmaps.
//!
//! The buffer stack has four layers, each one a thin wrapper over the last:
//!
//! | Type | Owns | Shareable | Purpose |
//! |---|---|---|---|
//! | [`AlignedBuf`] | yes | no | growable 64-byte aligned allocation; builders write here |
//! | [`Buffer`] | shared | `Arc` | immutable `(Arc<AlignedBuf>, offset, len)` window; slicing is free |
//! | [`ScalarBuffer<T>`] | shared | `Arc` | a [`Buffer`] reinterpreted as `[T]` |
//! | [`Bitmap`] | shared | `Arc` | a [`Buffer`] reinterpreted as LSB-numbered bits |
//!
//! Freezing an [`AlignedBuf`] into a [`Buffer`] is the one-way door between
//! the mutable and the shared world:
//!
//! ```
//! use astrs_data::{AlignedBuf, Buffer};
//!
//! let mut owned = AlignedBuf::new();
//! owned.extend_from_slice(b"payload");
//! let shared = Buffer::from(owned);
//!
//! // Every slice is a refcount bump — no bytes move.
//! let tail = shared.slice(3, 4);
//! assert_eq!(tail.as_slice(), b"load");
//! assert_eq!(shared.as_slice(), b"payload");
//! ```

pub mod aligned;
pub mod bitmap;
pub mod scalar;

use std::fmt;
use std::sync::Arc;

pub use crate::buffer::aligned::{ALIGNMENT, AlignedBuf, pad_to_alignment, padding_for};
pub use crate::buffer::bitmap::{
    Bitmap, BitmapBuilder, BitmapIter, count_set_bits, get_bit, set_bit,
};
pub use crate::buffer::scalar::ScalarBuffer;

use crate::datatype::ArrowNativeType;
use crate::error::{DataError, Result};

/// An immutable, reference-counted window over an [`AlignedBuf`].
///
/// `Buffer` is what arrays actually hold. Cloning it and slicing it are both
/// `O(1)` and allocation-free, which is what makes array slicing zero-copy all
/// the way down to the shared-memory ring (blueprint §6.2).
///
/// The window's start address inherits the backing allocation's alignment only
/// when `offset` is itself a multiple of the alignment; use
/// [`Buffer::address_alignment`] or [`ScalarBuffer::try_new`] when that matters.
///
/// ```
/// use astrs_data::Buffer;
///
/// let buffer = Buffer::from_slice(&[1, 2, 3, 4, 5]);
/// let middle = buffer.slice(1, 3);
/// assert_eq!(middle.as_slice(), &[2, 3, 4]);
/// assert_eq!(middle.slice(1, 99).as_slice(), &[3, 4], "slices clamp, never panic");
/// ```
#[derive(Clone)]
pub struct Buffer {
    /// The shared allocation.
    data: Arc<AlignedBuf>,
    /// Byte offset of the window inside `data`.
    offset: usize,
    /// Length of the window in bytes. `offset + len <= data.len()` always.
    len: usize,
}

impl Buffer {
    /// An empty buffer. Allocates nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::from(AlignedBuf::new())
    }

    /// Copies `bytes` into a fresh aligned allocation.
    ///
    /// ```
    /// use astrs_data::Buffer;
    ///
    /// assert_eq!(Buffer::from_slice(b"abc").as_slice(), b"abc");
    /// ```
    #[must_use]
    pub fn from_slice(bytes: &[u8]) -> Self {
        Self::from(AlignedBuf::from_slice(bytes))
    }

    /// A buffer of `len` zero bytes.
    #[must_use]
    pub fn zeroed(len: usize) -> Self {
        Self::from(AlignedBuf::zeroed(len))
    }

    /// Copies a typed slice into a fresh aligned allocation.
    ///
    /// The bytes are the host's native representation, which is the
    /// little-endian layout Arrow specifies on every platform AstRS targets.
    ///
    /// ```
    /// use astrs_data::Buffer;
    ///
    /// let buffer = Buffer::from_scalars(&[1i32, 2, 3]);
    /// assert_eq!(buffer.len(), 12);
    /// ```
    #[must_use]
    pub fn from_scalars<T: ArrowNativeType>(values: &[T]) -> Self {
        Self::from_slice(scalar::as_bytes(values))
    }

    /// Wraps an existing shared allocation without copying.
    #[must_use]
    pub fn from_arc(data: Arc<AlignedBuf>) -> Self {
        let len = data.len();
        Self {
            data,
            offset: 0,
            len,
        }
    }

    /// Length of the window in bytes.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` when the window covers no bytes.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Byte offset of this window inside the backing allocation.
    #[inline]
    #[must_use]
    pub const fn offset(&self) -> usize {
        self.offset
    }

    /// Total size of the backing allocation, in bytes.
    ///
    /// A sliced buffer keeps the whole allocation alive; this is how much
    /// memory the window is really pinning.
    #[inline]
    #[must_use]
    pub fn backing_len(&self) -> usize {
        self.data.len()
    }

    /// The bytes in the window.
    #[inline]
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        let all = self.data.as_slice();
        let end = self.offset.saturating_add(self.len);
        debug_assert!(end <= all.len(), "window invariant");
        match all.get(self.offset..end) {
            Some(window) => window,
            // Unreachable while the window invariant holds; keeps the accessor
            // total instead of panicking if a future refactor breaks it.
            None => &[],
        }
    }

    /// Pointer to the first byte of the window.
    #[inline]
    #[must_use]
    pub fn as_ptr(&self) -> *const u8 {
        self.data.as_ptr().wrapping_add(self.offset)
    }

    /// Largest power-of-two alignment the window's start address satisfies.
    ///
    /// ```
    /// use astrs_data::{ALIGNMENT, Buffer};
    ///
    /// let buffer = Buffer::from_slice(&[0; 128]);
    /// assert!(buffer.address_alignment() >= ALIGNMENT);
    /// assert!(buffer.slice(1, 4).address_alignment() < ALIGNMENT);
    /// ```
    #[must_use]
    pub fn address_alignment(&self) -> usize {
        let addr = self.as_ptr() as usize;
        if addr == 0 {
            return ALIGNMENT;
        }
        1usize << addr.trailing_zeros().min(usize::BITS - 1)
    }

    /// Returns `true` when the window's start address is a multiple of `align`.
    #[inline]
    #[must_use]
    pub fn is_aligned_to(&self, align: usize) -> bool {
        align == 0 || (self.as_ptr() as usize).is_multiple_of(align)
    }

    /// A zero-copy sub-window, clamped to the available range.
    ///
    /// The crate-wide convention: an out-of-range request yields the largest
    /// valid window rather than panicking (see [`crate::array::Array::slice`]).
    /// Use [`Buffer::try_slice`] when a request outside the range is a bug.
    #[must_use]
    pub fn slice(&self, offset: usize, len: usize) -> Self {
        let offset = offset.min(self.len);
        let len = len.min(self.len - offset);
        Self {
            data: Arc::clone(&self.data),
            offset: self.offset + offset,
            len,
        }
    }

    /// Checked [`Buffer::slice`].
    ///
    /// ```
    /// use astrs_data::{Buffer, DataError};
    ///
    /// let buffer = Buffer::from_slice(&[1, 2, 3]);
    /// assert!(buffer.try_slice(1, 2).is_ok());
    /// assert_eq!(
    ///     buffer.try_slice(2, 2).unwrap_err(),
    ///     DataError::SliceOutOfBounds { offset: 2, len: 2, available: 3 }
    /// );
    /// ```
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

    /// Number of `Buffer`/`Arc` handles sharing the backing allocation.
    #[must_use]
    pub fn share_count(&self) -> usize {
        Arc::strong_count(&self.data)
    }

    /// Reinterprets the window as a typed slice.
    ///
    /// Fails when the window is not aligned for `T` or its length is not a
    /// whole number of elements. See [`ScalarBuffer`].
    pub fn typed<T: ArrowNativeType>(&self) -> Result<ScalarBuffer<T>> {
        ScalarBuffer::try_new(self.clone())
    }

    /// Recovers the unique owner when this is the only handle and the window
    /// covers the whole allocation; otherwise gives the buffer back.
    ///
    /// Builders use this to reuse an allocation instead of copying.
    ///
    /// ```
    /// use astrs_data::{AlignedBuf, Buffer};
    ///
    /// let buffer = Buffer::from(AlignedBuf::from_slice(b"own"));
    /// let owned = buffer.into_aligned().expect("sole owner of the full window");
    /// assert_eq!(owned.as_slice(), b"own");
    ///
    /// let shared = Buffer::from_slice(b"shared");
    /// let _clone = shared.clone();
    /// assert!(shared.into_aligned().is_err(), "a second handle blocks recovery");
    /// ```
    ///
    /// # Errors
    ///
    /// Returns the original buffer when it is shared or is a partial window.
    pub fn into_aligned(self) -> core::result::Result<AlignedBuf, Self> {
        let Self { data, offset, len } = self;
        if offset != 0 || len != data.len() {
            return Err(Self { data, offset, len });
        }
        Arc::try_unwrap(data).map_err(|data| Self { data, offset, len })
    }

    /// Copies the window into a fresh allocation whose start address satisfies
    /// [`ALIGNMENT`], returning `self` untouched when it already does.
    ///
    /// Stage 2 uses this on the decode path: a buffer carved out of a mapped
    /// IPC body may start at an arbitrary offset, and the typed views require
    /// element alignment.
    ///
    /// ```
    /// use astrs_data::{ALIGNMENT, Buffer};
    ///
    /// let unaligned = Buffer::from_slice(&[0u8; 128]).slice(1, 8);
    /// let fixed = unaligned.realigned();
    /// assert!(fixed.is_aligned_to(ALIGNMENT));
    /// assert_eq!(fixed.as_slice(), unaligned.as_slice());
    /// ```
    #[must_use]
    pub fn realigned(&self) -> Self {
        if self.is_aligned_to(ALIGNMENT) {
            return self.clone();
        }
        Self::from_slice(self.as_slice())
    }
}

impl Default for Buffer {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl From<AlignedBuf> for Buffer {
    #[inline]
    fn from(buf: AlignedBuf) -> Self {
        Self::from_arc(Arc::new(buf))
    }
}

impl From<&[u8]> for Buffer {
    #[inline]
    fn from(bytes: &[u8]) -> Self {
        Self::from_slice(bytes)
    }
}

impl From<Vec<u8>> for Buffer {
    #[inline]
    fn from(bytes: Vec<u8>) -> Self {
        Self::from_slice(&bytes)
    }
}

impl AsRef<[u8]> for Buffer {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl std::ops::Deref for Buffer {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl PartialEq for Buffer {
    /// Compares window *contents*, not identity: two buffers backed by
    /// different allocations are equal when their bytes are.
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl Eq for Buffer {}

impl std::hash::Hash for Buffer {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}

impl fmt::Debug for Buffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const PREVIEW: usize = 16;
        let bytes = self.as_slice();
        f.debug_struct("Buffer")
            .field("len", &self.len)
            .field("offset", &self.offset)
            .field("shared", &self.share_count())
            .field("head", &&bytes[..bytes.len().min(PREVIEW)])
            .finish_non_exhaustive()
    }
}

impl FromIterator<u8> for Buffer {
    fn from_iter<I: IntoIterator<Item = u8>>(iter: I) -> Self {
        Self::from(AlignedBuf::from_iter(iter))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    const fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn buffer_is_send_and_sync() {
        assert_send_sync::<Buffer>();
    }

    #[test]
    fn empty_buffer() {
        let buffer = Buffer::new();
        assert!(buffer.is_empty());
        assert_eq!(buffer.len(), 0);
        assert_eq!(buffer.as_slice(), &[] as &[u8]);
        assert_eq!(buffer, Buffer::default());
    }

    #[test]
    fn slicing_is_zero_copy() {
        let buffer = Buffer::from_slice(&[0, 1, 2, 3, 4, 5, 6, 7]);
        let base = buffer.as_ptr();
        let mid = buffer.slice(2, 4);
        assert_eq!(mid.as_slice(), &[2, 3, 4, 5]);
        assert_eq!(mid.as_ptr(), base.wrapping_add(2));
        assert_eq!(mid.offset(), 2);
        assert_eq!(mid.backing_len(), 8);
        assert_eq!(buffer.share_count(), 2);
    }

    #[test]
    fn nested_slices_compose() {
        let buffer = Buffer::from_slice(&(0u8..20).collect::<Vec<_>>());
        let a = buffer.slice(5, 10);
        let b = a.slice(2, 3);
        assert_eq!(b.as_slice(), &[7, 8, 9]);
        assert_eq!(b.offset(), 7);
    }

    #[test]
    fn slices_clamp_instead_of_panicking() {
        let buffer = Buffer::from_slice(&[1, 2, 3]);
        assert_eq!(buffer.slice(0, 99).as_slice(), &[1, 2, 3]);
        assert_eq!(buffer.slice(2, 99).as_slice(), &[3]);
        assert!(buffer.slice(99, 1).is_empty());
        assert!(buffer.slice(usize::MAX, usize::MAX).is_empty());
    }

    #[test]
    fn try_slice_reports_out_of_range() {
        let buffer = Buffer::from_slice(&[1, 2, 3]);
        assert_eq!(buffer.try_slice(0, 3).unwrap().as_slice(), &[1, 2, 3]);
        assert_eq!(buffer.try_slice(3, 0).unwrap().len(), 0);
        assert_eq!(
            buffer.try_slice(1, 3).unwrap_err(),
            DataError::SliceOutOfBounds {
                offset: 1,
                len: 3,
                available: 3
            }
        );
        assert!(buffer.try_slice(usize::MAX, usize::MAX).is_err());
    }

    #[test]
    fn alignment_reporting() {
        let buffer = Buffer::from_slice(&[0; 256]);
        assert!(buffer.is_aligned_to(ALIGNMENT));
        assert!(buffer.address_alignment() >= ALIGNMENT);
        let odd = buffer.slice(1, 8);
        assert!(!odd.is_aligned_to(ALIGNMENT));
        assert_eq!(odd.address_alignment(), 1);
        assert!(buffer.slice(64, 8).is_aligned_to(ALIGNMENT));
        assert!(buffer.is_aligned_to(0));
    }

    #[test]
    fn realigned_copies_only_when_needed() {
        let buffer = Buffer::from_slice(&[7u8; 128]);
        let same = buffer.realigned();
        assert_eq!(same.as_ptr(), buffer.as_ptr(), "already aligned: no copy");

        let odd = buffer.slice(3, 10);
        let fixed = odd.realigned();
        assert!(fixed.is_aligned_to(ALIGNMENT));
        assert_ne!(fixed.as_ptr(), odd.as_ptr());
        assert_eq!(fixed.as_slice(), odd.as_slice());
    }

    #[test]
    fn into_aligned_recovers_sole_ownership() {
        let buffer = Buffer::from_slice(b"recover");
        let owned = buffer.into_aligned().expect("sole owner");
        assert_eq!(owned.as_slice(), b"recover");

        let shared = Buffer::from_slice(b"shared");
        let keep = shared.clone();
        let err = shared.into_aligned().expect_err("two handles");
        assert_eq!(err.as_slice(), b"shared");
        drop(keep);

        let windowed = Buffer::from_slice(b"window").slice(1, 3);
        assert!(windowed.into_aligned().is_err(), "partial window");
    }

    #[test]
    fn equality_is_by_content() {
        let a = Buffer::from_slice(&[1, 2, 3]);
        let b = Buffer::from_slice(&[0, 1, 2, 3]).slice(1, 3);
        assert_eq!(a, b);
        assert_ne!(a, Buffer::from_slice(&[1, 2]));
    }

    #[test]
    fn conversions_and_collect() {
        assert_eq!(Buffer::from(vec![1u8, 2]).as_slice(), &[1, 2]);
        assert_eq!(Buffer::from(&b"hi"[..]).as_slice(), b"hi");
        let collected: Buffer = (0u8..4).collect();
        assert_eq!(collected.as_slice(), &[0, 1, 2, 3]);
        assert_eq!(Buffer::zeroed(3).as_slice(), &[0, 0, 0]);
        assert_eq!(&*Buffer::from_slice(b"deref"), b"deref");
    }

    #[test]
    fn from_scalars_packs_native_bytes() {
        let buffer = Buffer::from_scalars(&[1u16, 0x0201]);
        assert_eq!(buffer.len(), 4);
        let typed = buffer.typed::<u16>().unwrap();
        assert_eq!(typed.as_slice(), &[1, 0x0201]);
    }

    #[test]
    fn debug_shows_sharing() {
        let buffer = Buffer::from_slice(&[1, 2, 3]);
        let rendered = format!("{buffer:?}");
        assert!(rendered.contains("len: 3"), "{rendered}");
        assert!(rendered.contains("shared: 1"), "{rendered}");
    }

    #[test]
    fn from_arc_shares_without_copying() {
        let arc = Arc::new(AlignedBuf::from_slice(b"arc"));
        let a = Buffer::from_arc(Arc::clone(&arc));
        let b = Buffer::from_arc(arc);
        assert_eq!(a.as_ptr(), b.as_ptr());
        assert_eq!(a.as_slice(), b"arc");
    }
}
