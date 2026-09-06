//! [`Bitmap`] — the validity bitmap, and the free bit helpers it is built on.
//!
//! # Bit numbering
//!
//! AstRS follows the Arrow specification exactly: bits are numbered
//! **least-significant-bit first** inside each byte, and bytes run in
//! increasing address order. Logical bit `i` therefore lives at
//! `data[i / 8] >> (i % 8) & 1`:
//!
//! ```text
//!   logical index:  7  6  5  4  3  2  1  0    15 14 13 12 11 10  9  8
//!                 ┌──┬──┬──┬──┬──┬──┬──┬──┐ ┌──┬──┬──┬──┬──┬──┬──┬──┐
//!   byte 0/1      │b7│b6│b5│b4│b3│b2│b1│b0│ │  │  │  │  │  │  │  │b8│
//!                 └──┴──┴──┴──┴──┴──┴──┴──┘ └──┴──┴──┴──┴──┴──┴──┴──┘
//! ```
//!
//! In a *validity* bitmap `1` means **valid** and `0` means **null** — so a
//! bitmap's null count is `len - count_set()`. Bits past the logical length
//! are unspecified; every operation here masks them off rather than trusting
//! them, so a bitmap decoded from a foreign producer is safe to combine with
//! one this crate built.
//!
//! # Offsets
//!
//! A [`Bitmap`] carries a *bit* offset independent of its buffer's byte
//! offset, which is what makes slicing free: slicing a validity map by one
//! element does not move a single byte. Every accessor, comparison and binary
//! operation therefore has to work at arbitrary bit offsets — the helper
//! A private `byte_at` helper extracts eight logical bits starting anywhere
//! and is
//! the shared core of equality, the binary ops and canonicalisation.
//!
//! ```
//! use astrs_data::Bitmap;
//!
//! let bits: Bitmap = [true, false, true, true, false].into_iter().collect();
//! assert_eq!(bits.len(), 5);
//! assert_eq!(bits.count_set(), 3);
//!
//! let tail = bits.slice(2, 3);
//! assert_eq!(tail.iter().collect::<Vec<_>>(), vec![true, true, false]);
//! ```

use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};

use crate::buffer::Buffer;
use crate::buffer::aligned::AlignedBuf;
use crate::error::{DataError, Result};

/// Sentinel meaning "the set-bit count has not been computed yet".
const UNKNOWN_COUNT: i64 = -1;

/// Reads logical bit `index` from `data`, LSB-numbered.
///
/// Returns `false` when `index` is out of range, so the helper is total.
///
/// ```
/// use astrs_data::buffer::get_bit;
///
/// let data = [0b0000_0101u8];
/// assert!(get_bit(&data, 0));
/// assert!(!get_bit(&data, 1));
/// assert!(get_bit(&data, 2));
/// assert!(!get_bit(&data, 99));
/// ```
#[inline]
#[must_use]
pub fn get_bit(data: &[u8], index: usize) -> bool {
    match data.get(index / 8) {
        Some(byte) => (byte >> (index % 8)) & 1 == 1,
        None => false,
    }
}

/// Writes logical bit `index` in `data`, LSB-numbered. Out-of-range writes are
/// ignored.
///
/// ```
/// use astrs_data::buffer::{get_bit, set_bit};
///
/// let mut data = [0u8; 2];
/// set_bit(&mut data, 9, true);
/// assert_eq!(data, [0, 0b0000_0010]);
/// assert!(get_bit(&data, 9));
/// set_bit(&mut data, 9, false);
/// assert_eq!(data, [0, 0]);
/// ```
#[inline]
pub fn set_bit(data: &mut [u8], index: usize, value: bool) {
    if let Some(byte) = data.get_mut(index / 8) {
        let mask = 1u8 << (index % 8);
        if value {
            *byte |= mask;
        } else {
            *byte &= !mask;
        }
    }
}

/// Counts the set bits in the logical range `[offset, offset + len)`.
///
/// Bits outside the slice are treated as unset, so the helper is total. The
/// interior runs eight bytes at a time through `u64::count_ones`.
///
/// ```
/// use astrs_data::buffer::count_set_bits;
///
/// let data = [0xffu8, 0x0f];
/// assert_eq!(count_set_bits(&data, 0, 16), 12);
/// assert_eq!(count_set_bits(&data, 4, 8), 8);
/// assert_eq!(count_set_bits(&data, 12, 4), 0);
/// ```
#[must_use]
pub fn count_set_bits(data: &[u8], offset: usize, len: usize) -> usize {
    let available = data.len().saturating_mul(8);
    if len == 0 || offset >= available {
        return 0;
    }
    let len = len.min(available - offset);
    let end = offset + len;

    let first_byte = offset / 8;
    let last_byte = end.div_ceil(8);
    let Some(bytes) = data.get(first_byte..last_byte) else {
        return 0;
    };
    let Some((&head, rest)) = bytes.split_first() else {
        return 0;
    };

    let lead = offset % 8;
    let trail = (8 - end % 8) % 8;
    let head_mask = 0xffu8 << lead;

    let Some((&tail, middle)) = rest.split_last() else {
        // The whole range lives in one byte.
        return usize::try_from((head & head_mask & (0xffu8 >> trail)).count_ones()).unwrap_or(0);
    };

    let mut count = (head & head_mask).count_ones();
    // `as_chunks::<8>` hands back `&[u8; 8]` directly, so the word is loaded
    // without the intermediate array a `chunks_exact(8)` slice would need.
    let (words, remainder) = middle.as_chunks::<8>();
    for word in words {
        count += u64::from_le_bytes(*word).count_ones();
    }
    for byte in remainder {
        count += byte.count_ones();
    }
    count += (tail & (0xffu8 >> trail)).count_ones();
    usize::try_from(count).unwrap_or(0)
}

/// An immutable, LSB-numbered bit sequence over a shared [`Buffer`].
///
/// Used as the validity map of every nullable array and as the value buffer of
/// [`crate::array::BooleanArray`].
///
/// The set-bit count is computed lazily and cached in an atomic, so
/// `count_set` is `O(len / 64)` once and `O(1)` afterwards, while the type
/// stays `Send + Sync` with no locks.
pub struct Bitmap {
    /// Backing bytes. `buffer.len() * 8 >= offset + len` always.
    buffer: Buffer,
    /// Logical bit offset of the first bit inside `buffer`.
    offset: usize,
    /// Number of logical bits.
    len: usize,
    /// Cached `count_set`, or [`UNKNOWN_COUNT`] when not yet computed.
    set_count: AtomicI64,
}

impl Bitmap {
    /// Wraps a byte window as a bitmap.
    ///
    /// # Errors
    ///
    /// [`DataError::BufferTooSmall`] when the buffer cannot cover
    /// `offset + len` bits.
    ///
    /// ```
    /// use astrs_data::{Bitmap, Buffer};
    ///
    /// let bits = Bitmap::try_new(Buffer::from_slice(&[0b1010_1010]), 1, 7)?;
    /// assert_eq!(bits.len(), 7);
    /// assert!(bits.value(0));
    /// assert!(Bitmap::try_new(Buffer::from_slice(&[0]), 0, 9).is_err());
    /// # Ok::<(), astrs_data::DataError>(())
    /// ```
    pub fn try_new(buffer: Buffer, offset: usize, len: usize) -> Result<Self> {
        let required = offset.saturating_add(len).div_ceil(8);
        if required > buffer.len() {
            return Err(DataError::BufferTooSmall {
                required,
                actual: buffer.len(),
            });
        }
        Ok(Self {
            buffer,
            offset,
            len,
            set_count: AtomicI64::new(UNKNOWN_COUNT),
        })
    }

    /// Wraps a byte window, using every bit it holds.
    ///
    /// ```
    /// use astrs_data::{Bitmap, Buffer};
    ///
    /// assert_eq!(Bitmap::from_buffer(Buffer::from_slice(&[0, 0])).len(), 16);
    /// ```
    #[must_use]
    pub fn from_buffer(buffer: Buffer) -> Self {
        let len = buffer.len() * 8;
        Self {
            buffer,
            offset: 0,
            len,
            set_count: AtomicI64::new(UNKNOWN_COUNT),
        }
    }

    /// An all-valid bitmap of `len` bits.
    #[must_use]
    pub fn new_set(len: usize) -> Self {
        let mut buf = AlignedBuf::with_capacity(len.div_ceil(8));
        buf.resize(len.div_ceil(8), 0xff);
        Self {
            buffer: Buffer::from(buf),
            offset: 0,
            len,
            set_count: AtomicI64::new(i64::try_from(len).unwrap_or(UNKNOWN_COUNT)),
        }
    }

    /// An all-null bitmap of `len` bits.
    #[must_use]
    pub fn new_unset(len: usize) -> Self {
        Self {
            buffer: Buffer::zeroed(len.div_ceil(8)),
            offset: 0,
            len,
            set_count: AtomicI64::new(0),
        }
    }

    /// Number of logical bits.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` when the bitmap covers no bits.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Logical bit offset of the first bit inside the backing buffer.
    #[inline]
    #[must_use]
    pub const fn bit_offset(&self) -> usize {
        self.offset
    }

    /// The backing byte window.
    #[inline]
    #[must_use]
    pub const fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    /// Reads bit `index`, returning `None` when out of range.
    #[inline]
    #[must_use]
    pub fn get(&self, index: usize) -> Option<bool> {
        if index >= self.len {
            return None;
        }
        Some(get_bit(self.buffer.as_slice(), self.offset + index))
    }

    /// Reads bit `index`, treating out-of-range indices as `false`.
    ///
    /// This is the hot accessor used by every array's `is_valid`.
    #[inline]
    #[must_use]
    pub fn value(&self, index: usize) -> bool {
        index < self.len && get_bit(self.buffer.as_slice(), self.offset + index)
    }

    /// Number of set (valid) bits. Computed once, then cached.
    ///
    /// ```
    /// use astrs_data::Bitmap;
    ///
    /// let bits: Bitmap = [true, false, true].into_iter().collect();
    /// assert_eq!(bits.count_set(), 2);
    /// assert_eq!(bits.count_unset(), 1);
    /// ```
    #[must_use]
    pub fn count_set(&self) -> usize {
        let cached = self.set_count.load(Ordering::Relaxed);
        if cached >= 0 {
            return usize::try_from(cached).unwrap_or(self.len);
        }
        let counted = count_set_bits(self.buffer.as_slice(), self.offset, self.len);
        // Racing threads compute the same value, so a plain relaxed store is
        // enough — there is no ordering relationship to establish.
        self.set_count.store(
            i64::try_from(counted).unwrap_or(UNKNOWN_COUNT),
            Ordering::Relaxed,
        );
        counted
    }

    /// Number of unset (null) bits.
    #[inline]
    #[must_use]
    pub fn count_unset(&self) -> usize {
        self.len - self.count_set()
    }

    /// Returns `true` when every bit is set.
    #[inline]
    #[must_use]
    pub fn all_set(&self) -> bool {
        self.count_set() == self.len
    }

    /// Returns `true` when no bit is set.
    #[inline]
    #[must_use]
    pub fn none_set(&self) -> bool {
        self.count_set() == 0
    }

    /// A zero-copy sub-range, clamped to the available range (the crate-wide
    /// slicing convention).
    ///
    /// ```
    /// use astrs_data::Bitmap;
    ///
    /// let bits: Bitmap = (0..20).map(|i| i % 3 == 0).collect();
    /// let window = bits.slice(5, 5);
    /// assert_eq!(window.iter().collect::<Vec<_>>(), vec![false, true, false, false, true]);
    /// assert_eq!(bits.slice(18, 99).len(), 2, "slices clamp");
    /// ```
    #[must_use]
    pub fn slice(&self, offset: usize, len: usize) -> Self {
        let offset = offset.min(self.len);
        let len = len.min(self.len - offset);
        let full = offset == 0 && len == self.len;
        Self {
            buffer: self.buffer.clone(),
            offset: self.offset + offset,
            len,
            set_count: AtomicI64::new(if full {
                self.set_count.load(Ordering::Relaxed)
            } else {
                UNKNOWN_COUNT
            }),
        }
    }

    /// Checked [`Bitmap::slice`].
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

    /// Iterates over the bits, low index first.
    #[inline]
    #[must_use]
    pub fn iter(&self) -> BitmapIter<'_> {
        BitmapIter {
            bitmap: self,
            front: 0,
            back: self.len,
        }
    }

    /// Iterates over the indices of the set bits.
    ///
    /// ```
    /// use astrs_data::Bitmap;
    ///
    /// let bits: Bitmap = [false, true, false, true].into_iter().collect();
    /// assert_eq!(bits.set_indices().collect::<Vec<_>>(), vec![1, 3]);
    /// ```
    pub fn set_indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.iter()
            .enumerate()
            .filter_map(|(i, bit)| bit.then_some(i))
    }

    /// Iterates over the indices of the unset (null) bits.
    pub fn unset_indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.iter()
            .enumerate()
            .filter_map(|(i, bit)| (!bit).then_some(i))
    }

    /// Eight logical bits starting at logical index `start`, packed LSB-first.
    ///
    /// Bits past the logical end read as `0`. This is the primitive every
    /// offset-agnostic operation is built on: it turns "two bitmaps at
    /// different bit offsets" into "two byte streams".
    #[inline]
    #[must_use]
    fn byte_at(&self, start: usize) -> u8 {
        if start >= self.len {
            return 0;
        }
        let data = self.buffer.as_slice();
        let bit = self.offset + start;
        let index = bit / 8;
        let shift = bit % 8;
        let mut byte = data.get(index).copied().unwrap_or(0) >> shift;
        if shift != 0 {
            let next = data.get(index + 1).copied().unwrap_or(0);
            byte |= next << (8 - shift);
        }
        let remaining = self.len - start;
        if remaining < 8 {
            byte &= 0xffu8 >> (8 - remaining);
        }
        byte
    }

    /// Copies the bits into a fresh, zero-offset, tightly packed bitmap.
    ///
    /// Stage 2 needs this before writing a validity buffer into an IPC body:
    /// the wire format has no bit-offset field.
    ///
    /// ```
    /// use astrs_data::Bitmap;
    ///
    /// let bits: Bitmap = (0..20).map(|i| i % 2 == 0).collect();
    /// let window = bits.slice(3, 9);
    /// let packed = window.to_canonical();
    /// assert_eq!(packed.bit_offset(), 0);
    /// assert_eq!(packed.iter().collect::<Vec<_>>(), window.iter().collect::<Vec<_>>());
    /// ```
    #[must_use]
    pub fn to_canonical(&self) -> Self {
        if self.offset == 0 && self.buffer.len() == self.len.div_ceil(8) {
            return self.clone();
        }
        let byte_len = self.len.div_ceil(8);
        let mut buf = AlignedBuf::with_capacity(byte_len);
        for index in 0..byte_len {
            buf.push(self.byte_at(index * 8));
        }
        Self {
            buffer: Buffer::from(buf),
            offset: 0,
            len: self.len,
            set_count: AtomicI64::new(self.set_count.load(Ordering::Relaxed)),
        }
    }

    /// Bitwise AND of two equal-length bitmaps.
    ///
    /// This is how a parent's validity is pushed into a child's: a value is
    /// valid only when both maps say so. Operand bit offsets may differ.
    ///
    /// # Errors
    ///
    /// [`DataError::BitmapLengthMismatch`] when the lengths differ.
    ///
    /// ```
    /// use astrs_data::Bitmap;
    ///
    /// let a: Bitmap = [true, true, false, false].into_iter().collect();
    /// let b: Bitmap = [true, false, true, false].into_iter().collect();
    /// assert_eq!(a.and(&b)?.iter().collect::<Vec<_>>(), vec![true, false, false, false]);
    /// # Ok::<(), astrs_data::DataError>(())
    /// ```
    pub fn and(&self, other: &Self) -> Result<Self> {
        self.binary_op(other, |a, b| a & b)
    }

    /// Bitwise OR of two equal-length bitmaps.
    ///
    /// # Errors
    ///
    /// [`DataError::BitmapLengthMismatch`] when the lengths differ.
    pub fn or(&self, other: &Self) -> Result<Self> {
        self.binary_op(other, |a, b| a | b)
    }

    /// Bitwise XOR of two equal-length bitmaps.
    ///
    /// # Errors
    ///
    /// [`DataError::BitmapLengthMismatch`] when the lengths differ.
    pub fn xor(&self, other: &Self) -> Result<Self> {
        self.binary_op(other, |a, b| a ^ b)
    }

    /// Bitwise NOT.
    ///
    /// ```
    /// use astrs_data::Bitmap;
    ///
    /// let bits: Bitmap = [true, false, true].into_iter().collect();
    /// assert_eq!(bits.not().iter().collect::<Vec<_>>(), vec![false, true, false]);
    /// ```
    #[must_use]
    pub fn not(&self) -> Self {
        self.map_bytes(|byte| !byte)
    }

    /// Applies `op` byte-wise over the canonicalised bits, masking the tail.
    fn map_bytes(&self, op: impl Fn(u8) -> u8) -> Self {
        let byte_len = self.len.div_ceil(8);
        let mut buf = AlignedBuf::with_capacity(byte_len);
        for index in 0..byte_len {
            buf.push(op(self.byte_at(index * 8)));
        }
        mask_tail(&mut buf, self.len);
        Self {
            buffer: Buffer::from(buf),
            offset: 0,
            len: self.len,
            set_count: AtomicI64::new(UNKNOWN_COUNT),
        }
    }

    /// Shared implementation of [`Bitmap::and`], [`Bitmap::or`] and
    /// [`Bitmap::xor`], correct for arbitrary and differing bit offsets.
    fn binary_op(&self, other: &Self, op: impl Fn(u8, u8) -> u8) -> Result<Self> {
        if self.len != other.len {
            return Err(DataError::BitmapLengthMismatch {
                left: self.len,
                right: other.len,
            });
        }
        let byte_len = self.len.div_ceil(8);
        let mut buf = AlignedBuf::with_capacity(byte_len);
        for index in 0..byte_len {
            let start = index * 8;
            buf.push(op(self.byte_at(start), other.byte_at(start)));
        }
        mask_tail(&mut buf, self.len);
        Ok(Self {
            buffer: Buffer::from(buf),
            offset: 0,
            len: self.len,
            set_count: AtomicI64::new(UNKNOWN_COUNT),
        })
    }

    /// Combines two optional validity maps the way nested arrays need.
    ///
    /// `None` means "everything valid", so the result is `None` only when both
    /// inputs are `None`.
    ///
    /// # Errors
    ///
    /// [`DataError::BitmapLengthMismatch`] when both are present with
    /// different lengths.
    ///
    /// ```
    /// use astrs_data::Bitmap;
    ///
    /// let a: Bitmap = [true, false].into_iter().collect();
    /// assert!(Bitmap::intersect(None, None)?.is_none());
    /// assert_eq!(Bitmap::intersect(Some(&a), None)?, Some(a));
    /// # Ok::<(), astrs_data::DataError>(())
    /// ```
    pub fn intersect(left: Option<&Self>, right: Option<&Self>) -> Result<Option<Self>> {
        match (left, right) {
            (None, None) => Ok(None),
            (Some(only), None) | (None, Some(only)) => Ok(Some(only.clone())),
            (Some(a), Some(b)) => a.and(b).map(Some),
        }
    }

    /// Expands the bitmap so each bit is repeated `factor` times.
    ///
    /// `FixedSizeList` uses this to push a parent's validity down onto the
    /// child values it covers.
    ///
    /// ```
    /// use astrs_data::Bitmap;
    ///
    /// let parent: Bitmap = [true, false].into_iter().collect();
    /// let child = parent.repeat_each(3);
    /// assert_eq!(child.iter().collect::<Vec<_>>(), vec![true, true, true, false, false, false]);
    /// ```
    #[must_use]
    pub fn repeat_each(&self, factor: usize) -> Self {
        let mut builder = BitmapBuilder::with_capacity(self.len.saturating_mul(factor));
        for bit in self.iter() {
            builder.append_n(factor, bit);
        }
        builder.finish()
    }
}

/// Clears the bits past `len` in the final byte, so unused tail bits never
/// leak into a count or a comparison.
fn mask_tail(buf: &mut AlignedBuf, len: usize) {
    let trailing = len % 8;
    if trailing == 0 {
        return;
    }
    let last = len / 8;
    if let Some(byte) = buf.as_mut_slice().get_mut(last) {
        *byte &= 0xffu8 >> (8 - trailing);
    }
}

impl Clone for Bitmap {
    fn clone(&self) -> Self {
        Self {
            buffer: self.buffer.clone(),
            offset: self.offset,
            len: self.len,
            set_count: AtomicI64::new(self.set_count.load(Ordering::Relaxed)),
        }
    }
}

impl PartialEq for Bitmap {
    /// Compares *logical* bits, so two bitmaps at different offsets over
    /// different buffers compare equal when their bit sequences match.
    fn eq(&self, other: &Self) -> bool {
        if self.len != other.len {
            return false;
        }
        (0..self.len.div_ceil(8)).all(|index| {
            let start = index * 8;
            self.byte_at(start) == other.byte_at(start)
        })
    }
}

impl Eq for Bitmap {}

impl std::hash::Hash for Bitmap {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.len.hash(state);
        for index in 0..self.len.div_ceil(8) {
            self.byte_at(index * 8).hash(state);
        }
    }
}

impl fmt::Debug for Bitmap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const PREVIEW: usize = 64;
        let mut bits = String::with_capacity(self.len.min(PREVIEW) + 1);
        for bit in self.iter().take(PREVIEW) {
            bits.push(if bit { '1' } else { '0' });
        }
        if self.len > PREVIEW {
            bits.push('…');
        }
        f.debug_struct("Bitmap")
            .field("len", &self.len)
            .field("offset", &self.offset)
            .field("set", &self.count_set())
            .field("bits", &bits)
            .finish()
    }
}

impl FromIterator<bool> for Bitmap {
    fn from_iter<I: IntoIterator<Item = bool>>(iter: I) -> Self {
        let iter = iter.into_iter();
        let (lower, _) = iter.size_hint();
        let mut builder = BitmapBuilder::with_capacity(lower);
        for bit in iter {
            builder.append(bit);
        }
        builder.finish()
    }
}

impl<'a> IntoIterator for &'a Bitmap {
    type Item = bool;
    type IntoIter = BitmapIter<'a>;

    fn into_iter(self) -> BitmapIter<'a> {
        self.iter()
    }
}

/// Double-ended iterator over a [`Bitmap`]'s bits.
#[derive(Debug, Clone)]
pub struct BitmapIter<'a> {
    /// The bitmap being walked.
    bitmap: &'a Bitmap,
    /// Next index from the front.
    front: usize,
    /// One past the next index from the back.
    back: usize,
}

impl Iterator for BitmapIter<'_> {
    type Item = bool;

    #[inline]
    fn next(&mut self) -> Option<bool> {
        if self.front >= self.back {
            return None;
        }
        let bit = self.bitmap.value(self.front);
        self.front += 1;
        Some(bit)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.back - self.front;
        (remaining, Some(remaining))
    }
}

impl DoubleEndedIterator for BitmapIter<'_> {
    #[inline]
    fn next_back(&mut self) -> Option<bool> {
        if self.front >= self.back {
            return None;
        }
        self.back -= 1;
        Some(self.bitmap.value(self.back))
    }
}

impl ExactSizeIterator for BitmapIter<'_> {}

impl std::iter::FusedIterator for BitmapIter<'_> {}

/// Row-at-a-time writer for [`Bitmap`].
///
/// ```
/// use astrs_data::BitmapBuilder;
///
/// let mut builder = BitmapBuilder::with_capacity(8);
/// builder.append(true);
/// builder.append_n(3, false);
/// builder.append_slice(&[true, true]);
/// let bits = builder.finish();
/// assert_eq!(bits.len(), 6);
/// assert_eq!(bits.count_set(), 3);
/// ```
#[derive(Debug, Default)]
pub struct BitmapBuilder {
    /// Packed bytes; the tail bits past `len` are always zero.
    buf: AlignedBuf,
    /// Number of bits appended so far.
    len: usize,
    /// Running count of set bits, handed to the finished bitmap.
    set_count: usize,
}

impl BitmapBuilder {
    /// An empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty builder with room for `capacity` bits.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: AlignedBuf::with_capacity(capacity.div_ceil(8)),
            len: 0,
            set_count: 0,
        }
    }

    /// Number of bits appended so far.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` when nothing has been appended.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of set bits appended so far.
    #[inline]
    #[must_use]
    pub const fn count_set(&self) -> usize {
        self.set_count
    }

    /// Appends one bit.
    #[inline]
    pub fn append(&mut self, value: bool) {
        if self.len.is_multiple_of(8) {
            self.buf.push(0);
        }
        if value {
            set_bit(self.buf.as_mut_slice(), self.len, true);
            self.set_count += 1;
        }
        self.len += 1;
    }

    /// Appends `count` copies of `value`.
    pub fn append_n(&mut self, count: usize, value: bool) {
        if count == 0 {
            return;
        }
        self.buf
            .reserve((self.len + count).div_ceil(8) - self.buf.len());
        // Fill the partial byte one bit at a time, then whole bytes at once.
        let head = (8 - self.len % 8) % 8;
        let head = head.min(count);
        for _ in 0..head {
            self.append(value);
        }
        let remaining = count - head;
        if remaining == 0 {
            return;
        }
        let whole_bytes = remaining / 8;
        if whole_bytes > 0 {
            self.buf
                .resize(self.buf.len() + whole_bytes, u8::from(value) * 0xff);
            self.len += whole_bytes * 8;
            if value {
                self.set_count += whole_bytes * 8;
            }
        }
        for _ in 0..remaining % 8 {
            self.append(value);
        }
    }

    /// Appends every bit in `values`.
    pub fn append_slice(&mut self, values: &[bool]) {
        self.buf.reserve(
            (self.len + values.len())
                .div_ceil(8)
                .saturating_sub(self.buf.len()),
        );
        for value in values {
            self.append(*value);
        }
    }

    /// Appends every bit of `bitmap`.
    pub fn append_bitmap(&mut self, bitmap: &Bitmap) {
        self.buf.reserve(
            (self.len + bitmap.len())
                .div_ceil(8)
                .saturating_sub(self.buf.len()),
        );
        for bit in bitmap.iter() {
            self.append(bit);
        }
    }

    /// Overwrites bit `index`. Out-of-range writes are ignored.
    pub fn set(&mut self, index: usize, value: bool) {
        if index >= self.len {
            return;
        }
        let previous = get_bit(self.buf.as_slice(), index);
        if previous == value {
            return;
        }
        set_bit(self.buf.as_mut_slice(), index, value);
        if value {
            self.set_count += 1;
        } else {
            self.set_count -= 1;
        }
    }

    /// Reads bit `index`, or `None` when out of range.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<bool> {
        (index < self.len).then(|| get_bit(self.buf.as_slice(), index))
    }

    /// Drops every appended bit, keeping the allocation.
    pub fn clear(&mut self) {
        self.buf.clear();
        self.len = 0;
        self.set_count = 0;
    }

    /// Finishes the bitmap, resetting the builder.
    #[must_use]
    pub fn finish(&mut self) -> Bitmap {
        let buf = std::mem::take(&mut self.buf);
        let len = std::mem::take(&mut self.len);
        let set_count = std::mem::take(&mut self.set_count);
        Bitmap {
            buffer: Buffer::from(buf),
            offset: 0,
            len,
            set_count: AtomicI64::new(i64::try_from(set_count).unwrap_or(UNKNOWN_COUNT)),
        }
    }

    /// Finishes the bitmap without resetting the builder.
    #[must_use]
    pub fn finish_cloned(&self) -> Bitmap {
        Bitmap {
            buffer: Buffer::from(self.buf.clone()),
            offset: 0,
            len: self.len,
            set_count: AtomicI64::new(i64::try_from(self.set_count).unwrap_or(UNKNOWN_COUNT)),
        }
    }
}

impl Extend<bool> for BitmapBuilder {
    fn extend<I: IntoIterator<Item = bool>>(&mut self, iter: I) {
        for bit in iter {
            self.append(bit);
        }
    }
}

impl FromIterator<bool> for BitmapBuilder {
    fn from_iter<I: IntoIterator<Item = bool>>(iter: I) -> Self {
        let mut builder = Self::new();
        builder.extend(iter);
        builder
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    const fn assert_send_sync<T: Send + Sync>() {}

    fn bitmap_of(bits: &[bool]) -> Bitmap {
        bits.iter().copied().collect()
    }

    #[test]
    fn bitmap_is_send_and_sync() {
        assert_send_sync::<Bitmap>();
        assert_send_sync::<BitmapBuilder>();
    }

    #[test]
    fn lsb_numbering_matches_the_arrow_spec() {
        let bits = Bitmap::from_buffer(Buffer::from_slice(&[0b0000_0101, 0b1000_0000]));
        assert!(bits.value(0));
        assert!(!bits.value(1));
        assert!(bits.value(2));
        assert!(!bits.value(8));
        assert!(bits.value(15));
        assert_eq!(bits.count_set(), 3);
    }

    #[test]
    fn empty_bitmap() {
        let bits = bitmap_of(&[]);
        assert!(bits.is_empty());
        assert_eq!(bits.len(), 0);
        assert_eq!(bits.count_set(), 0);
        assert_eq!(bits.count_unset(), 0);
        assert!(bits.all_set(), "vacuously true");
        assert!(bits.none_set());
        assert_eq!(bits.get(0), None);
        assert!(!bits.value(0));
        assert_eq!(bits.iter().count(), 0);
    }

    #[test]
    fn all_set_and_all_unset_constructors() {
        let set = Bitmap::new_set(13);
        assert_eq!(set.len(), 13);
        assert_eq!(set.count_set(), 13);
        assert!(set.all_set());
        assert!(set.iter().all(|b| b));

        let unset = Bitmap::new_unset(13);
        assert_eq!(unset.count_set(), 0);
        assert!(unset.none_set());
        assert!(unset.iter().all(|b| !b));
    }

    #[test]
    fn try_new_validates_coverage() {
        let buffer = Buffer::from_slice(&[0xff, 0xff]);
        assert!(Bitmap::try_new(buffer.clone(), 0, 16).is_ok());
        assert!(Bitmap::try_new(buffer.clone(), 9, 7).is_ok());
        assert_eq!(
            Bitmap::try_new(buffer.clone(), 0, 17).unwrap_err(),
            DataError::BufferTooSmall {
                required: 3,
                actual: 2
            }
        );
        assert!(Bitmap::try_new(buffer, usize::MAX, 1).is_err());
    }

    #[test]
    fn slicing_is_zero_copy_and_offset_aware() {
        let source: Vec<bool> = (0..40).map(|i| i % 3 == 0).collect();
        let bits = bitmap_of(&source);
        for offset in 0..20 {
            for len in 0..=(40 - offset) {
                let window = bits.slice(offset, len);
                assert_eq!(window.len(), len);
                let expected: Vec<bool> = source[offset..offset + len].to_vec();
                assert_eq!(window.iter().collect::<Vec<_>>(), expected);
                assert_eq!(window.count_set(), expected.iter().filter(|b| **b).count());
            }
        }
    }

    #[test]
    fn slices_clamp_and_try_slice_reports() {
        let bits = bitmap_of(&[true; 10]);
        assert_eq!(bits.slice(8, 99).len(), 2);
        assert_eq!(bits.slice(99, 99).len(), 0);
        assert_eq!(
            bits.try_slice(8, 5).unwrap_err(),
            DataError::SliceOutOfBounds {
                offset: 8,
                len: 5,
                available: 10
            }
        );
        assert_eq!(bits.try_slice(8, 2).unwrap().len(), 2);
    }

    #[test]
    fn nested_slices_compose() {
        let source: Vec<bool> = (0..64).map(|i| i % 5 == 0).collect();
        let bits = bitmap_of(&source);
        let a = bits.slice(7, 40);
        let b = a.slice(11, 10);
        let expected: Vec<bool> = source[18..28].to_vec();
        assert_eq!(b.iter().collect::<Vec<_>>(), expected);
        assert_eq!(b.bit_offset(), 18);
    }

    #[test]
    fn full_slice_reuses_the_cached_count() {
        let bits = bitmap_of(&[true, false, true]);
        assert_eq!(bits.count_set(), 2);
        let full = bits.slice(0, 3);
        assert_eq!(full.count_set(), 2);
    }

    #[test]
    fn count_set_bits_handles_offsets_and_ragged_ends() {
        let data = vec![0xffu8; 9];
        assert_eq!(count_set_bits(&data, 0, 72), 72);
        assert_eq!(count_set_bits(&data, 3, 5), 5);
        assert_eq!(count_set_bits(&data, 3, 60), 60);
        assert_eq!(count_set_bits(&data, 70, 2), 2);
        assert_eq!(count_set_bits(&data, 0, 0), 0);
        assert_eq!(count_set_bits(&data, 72, 8), 0, "past the end");
        assert_eq!(count_set_bits(&data, 60, 100), 12, "clamped");
        assert_eq!(count_set_bits(&[], 0, 8), 0);

        let sparse = [0b0000_0001u8, 0b1000_0000];
        assert_eq!(count_set_bits(&sparse, 0, 16), 2);
        assert_eq!(count_set_bits(&sparse, 1, 14), 0);
        assert_eq!(count_set_bits(&sparse, 1, 15), 1);
    }

    #[test]
    fn count_set_bits_spans_many_words() {
        let data = vec![0b0101_0101u8; 100];
        assert_eq!(count_set_bits(&data, 0, 800), 400);
        assert_eq!(count_set_bits(&data, 1, 798), 399);
        assert_eq!(count_set_bits(&data, 5, 790), 395);
    }

    #[test]
    fn binary_ops_respect_differing_offsets() {
        let left_src: Vec<bool> = (0..37).map(|i| i % 2 == 0).collect();
        let right_src: Vec<bool> = (0..37).map(|i| i % 3 == 0).collect();
        // Bury each operand behind a different, non-byte-aligned offset.
        let padded_left: Vec<bool> = std::iter::repeat_n(false, 5)
            .chain(left_src.iter().copied())
            .collect();
        let padded_right: Vec<bool> = std::iter::repeat_n(true, 11)
            .chain(right_src.iter().copied())
            .collect();
        let left = bitmap_of(&padded_left).slice(5, 37);
        let right = bitmap_of(&padded_right).slice(11, 37);

        let and = left.and(&right).unwrap();
        let or = left.or(&right).unwrap();
        let xor = left.xor(&right).unwrap();
        for i in 0..37 {
            assert_eq!(and.value(i), left_src[i] && right_src[i], "and at {i}");
            assert_eq!(or.value(i), left_src[i] || right_src[i], "or at {i}");
            assert_eq!(xor.value(i), left_src[i] ^ right_src[i], "xor at {i}");
        }
        assert_eq!(and.bit_offset(), 0, "results are canonicalised");
    }

    #[test]
    fn binary_ops_reject_length_mismatch() {
        let a = bitmap_of(&[true, false]);
        let b = bitmap_of(&[true]);
        assert_eq!(
            a.and(&b).unwrap_err(),
            DataError::BitmapLengthMismatch { left: 2, right: 1 }
        );
        assert!(a.or(&b).is_err());
        assert!(a.xor(&b).is_err());
    }

    #[test]
    fn not_masks_the_tail() {
        let bits = bitmap_of(&[true, false, true]);
        let inverted = bits.not();
        assert_eq!(inverted.len(), 3);
        assert_eq!(
            inverted.iter().collect::<Vec<_>>(),
            vec![false, true, false]
        );
        assert_eq!(
            inverted.count_set(),
            1,
            "tail bits must not leak into the count"
        );
        assert_eq!(inverted.buffer().as_slice(), &[0b0000_0010]);
    }

    #[test]
    fn intersect_treats_none_as_all_valid() {
        let a = bitmap_of(&[true, false, true]);
        let b = bitmap_of(&[true, true, false]);
        assert_eq!(Bitmap::intersect(None, None).unwrap(), None);
        assert_eq!(Bitmap::intersect(Some(&a), None).unwrap(), Some(a.clone()));
        assert_eq!(Bitmap::intersect(None, Some(&b)).unwrap(), Some(b.clone()));
        assert_eq!(
            Bitmap::intersect(Some(&a), Some(&b)).unwrap(),
            Some(bitmap_of(&[true, false, false]))
        );
        let short = bitmap_of(&[true]);
        assert!(Bitmap::intersect(Some(&a), Some(&short)).is_err());
    }

    #[test]
    fn repeat_each_expands_parent_validity() {
        let parent = bitmap_of(&[true, false, true]);
        let child = parent.repeat_each(2);
        assert_eq!(
            child.iter().collect::<Vec<_>>(),
            vec![true, true, false, false, true, true]
        );
        assert_eq!(parent.repeat_each(0).len(), 0);
        assert_eq!(parent.repeat_each(1), parent);
    }

    #[test]
    fn to_canonical_repacks_offsets() {
        let source: Vec<bool> = (0..50).map(|i| i % 7 < 3).collect();
        let bits = bitmap_of(&source);
        let window = bits.slice(11, 21);
        let packed = window.to_canonical();
        assert_eq!(packed.bit_offset(), 0);
        assert_eq!(packed.len(), 21);
        assert_eq!(packed.iter().collect::<Vec<_>>(), source[11..32].to_vec());
        assert_eq!(packed.buffer().len(), 3);
        assert_eq!(packed, window);
        // Already canonical: returns a shared clone.
        let again = packed.to_canonical();
        assert_eq!(again.buffer().as_ptr(), packed.buffer().as_ptr());
    }

    #[test]
    fn equality_is_logical_not_physical() {
        let source: Vec<bool> = (0..30).map(|i| i % 4 == 1).collect();
        let direct = bitmap_of(&source[3..17]);
        let padded: Vec<bool> = std::iter::repeat_n(true, 3)
            .chain(source[3..17].iter().copied())
            .collect();
        let windowed = bitmap_of(&padded).slice(3, 14);
        assert_eq!(direct, windowed);
        assert_ne!(direct, bitmap_of(&source[3..16]));

        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let hash = |b: &Bitmap| {
            let mut h = DefaultHasher::new();
            b.hash(&mut h);
            h.finish()
        };
        assert_eq!(hash(&direct), hash(&windowed));
    }

    #[test]
    fn iterator_is_double_ended_and_exact() {
        let bits = bitmap_of(&[true, false, true, true]);
        let mut iter = bits.iter();
        assert_eq!(iter.len(), 4);
        assert_eq!(iter.next(), Some(true));
        assert_eq!(iter.next_back(), Some(true));
        assert_eq!(iter.len(), 2);
        assert_eq!(iter.collect::<Vec<_>>(), vec![false, true]);

        let reversed: Vec<bool> = bits.iter().rev().collect();
        assert_eq!(reversed, vec![true, true, false, true]);
        assert_eq!((&bits).into_iter().count(), 4);
    }

    #[test]
    fn index_iterators() {
        let bits = bitmap_of(&[false, true, false, true, true]);
        assert_eq!(bits.set_indices().collect::<Vec<_>>(), vec![1, 3, 4]);
        assert_eq!(bits.unset_indices().collect::<Vec<_>>(), vec![0, 2]);
    }

    #[test]
    fn builder_round_trips() {
        let mut builder = BitmapBuilder::new();
        assert!(builder.is_empty());
        for i in 0..100 {
            builder.append(i % 3 == 0);
        }
        assert_eq!(builder.len(), 100);
        assert_eq!(builder.count_set(), 34);
        let bits = builder.finish();
        assert_eq!(bits.len(), 100);
        assert_eq!(bits.count_set(), 34);
        for i in 0..100 {
            assert_eq!(bits.value(i), i % 3 == 0);
        }
        assert!(builder.is_empty(), "finish resets the builder");
    }

    #[test]
    fn builder_append_n_crosses_byte_boundaries() {
        for start in 0..16usize {
            for run in 0..40usize {
                let mut builder = BitmapBuilder::new();
                builder.append_n(start, false);
                builder.append_n(run, true);
                builder.append_n(3, false);
                let bits = builder.finish();
                assert_eq!(bits.len(), start + run + 3);
                assert_eq!(bits.count_set(), run, "start {start} run {run}");
                for i in 0..start {
                    assert!(!bits.value(i));
                }
                for i in start..start + run {
                    assert!(bits.value(i), "start {start} run {run} index {i}");
                }
                for i in start + run..start + run + 3 {
                    assert!(!bits.value(i));
                }
            }
        }
    }

    #[test]
    fn builder_append_slice_and_bitmap() {
        let mut builder = BitmapBuilder::with_capacity(16);
        builder.append_slice(&[true, false, true]);
        builder.append_bitmap(&bitmap_of(&[false, true]));
        let bits = builder.finish();
        assert_eq!(
            bits.iter().collect::<Vec<_>>(),
            vec![true, false, true, false, true]
        );
    }

    #[test]
    fn builder_set_updates_the_running_count() {
        let mut builder = BitmapBuilder::new();
        builder.append_slice(&[true, false, true]);
        assert_eq!(builder.count_set(), 2);
        builder.set(1, true);
        assert_eq!(builder.count_set(), 3);
        builder.set(1, true);
        assert_eq!(builder.count_set(), 3, "idempotent");
        builder.set(0, false);
        assert_eq!(builder.count_set(), 2);
        builder.set(99, true);
        assert_eq!(builder.count_set(), 2, "out-of-range writes are ignored");
        assert_eq!(builder.get(1), Some(true));
        assert_eq!(builder.get(9), None);
        let bits = builder.finish();
        assert_eq!(bits.count_set(), 2);
    }

    #[test]
    fn builder_clear_and_finish_cloned() {
        let mut builder: BitmapBuilder = [true, true, false].into_iter().collect();
        let snapshot = builder.finish_cloned();
        assert_eq!(snapshot.len(), 3);
        assert_eq!(builder.len(), 3, "finish_cloned keeps the builder");
        builder.clear();
        assert!(builder.is_empty());
        assert_eq!(builder.finish().len(), 0);
        assert_eq!(snapshot.count_set(), 2);
    }

    #[test]
    fn builder_extend() {
        let mut builder = BitmapBuilder::new();
        builder.extend([true, false]);
        builder.extend(vec![true]);
        assert_eq!(builder.finish().count_set(), 2);
    }

    #[test]
    fn debug_renders_bits() {
        let bits = bitmap_of(&[true, false, true]);
        let rendered = format!("{bits:?}");
        assert!(rendered.contains("101"), "{rendered}");
        assert!(rendered.contains("set: 2"), "{rendered}");

        let long = Bitmap::new_set(200);
        assert!(format!("{long:?}").contains('…'));
    }

    #[test]
    fn free_helpers_are_total() {
        let mut data = [0u8; 2];
        set_bit(&mut data, 0, true);
        set_bit(&mut data, 15, true);
        set_bit(&mut data, 99, true);
        assert_eq!(data, [0b0000_0001, 0b1000_0000]);
        assert!(get_bit(&data, 0));
        assert!(get_bit(&data, 15));
        assert!(!get_bit(&data, 16));
        set_bit(&mut data, 0, false);
        assert!(!get_bit(&data, 0));
    }

    #[test]
    fn cached_count_survives_cloning() {
        let bits = bitmap_of(&[true, false, true, true]);
        assert_eq!(bits.count_set(), 3);
        let copy = bits.clone();
        assert_eq!(copy.count_set(), 3);
        assert_eq!(copy, bits);
    }

    #[test]
    fn from_buffer_uses_every_bit() {
        let bits = Bitmap::from_buffer(Buffer::from_slice(&[0b0000_1111, 0]));
        assert_eq!(bits.len(), 16);
        assert_eq!(bits.count_set(), 4);
        assert_eq!(bits.buffer().len(), 2);
    }
}
