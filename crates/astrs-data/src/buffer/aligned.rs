//! [`AlignedBuf`] — the 64-byte aligned owned byte buffer every AstRS column
//! is built on.
//!
//! # Why not `Vec<u8>`
//!
//! The blueprint (§6.1, §6.2) requires that a payload mapped out of a shared
//! memory slot be *directly* usable as a SIMD source and that buffer bodies be
//! padded to 64 bytes per the Arrow specification. `Vec<u8>` guarantees only
//! `align_of::<u8>() == 1`, so an Arrow body written from a `Vec` would need a
//! realigning copy on every hop. `AlignedBuf` allocates through
//! [`std::alloc`] with an explicit [`Layout`] so the start address is always
//! `ALIGNMENT`-aligned and the capacity is always a whole number of 64-byte
//! lines.
//!
//! # Safety model
//!
//! This module is one of exactly two places in `astrs-data` that contain
//! `unsafe` (the other is [`crate::buffer::scalar`]). Every `unsafe` block
//! below is justified against this invariant set, which
//! A private `assert_invariants` helper re-checks all of these in debug
//! builds:
//!
//! 1. **Alignment.** `ptr` is always aligned to [`ALIGNMENT`], allocated or
//!    dangling.
//! 2. **Empty state.** `capacity == 0` implies `ptr` is the dangling sentinel
//!    (address `ALIGNMENT`, never dereferenced) and no allocation is owned.
//! 3. **Allocated state.** `capacity > 0` implies `ptr` came from
//!    [`std::alloc::alloc`], [`std::alloc::alloc_zeroed`] or
//!    [`std::alloc::realloc`] with `Layout::from_size_align(capacity,
//!    ALIGNMENT)`, and `capacity % ALIGNMENT == 0`.
//! 4. **Initialisation.** `len <= capacity`, and bytes `[0, len)` are
//!    initialised.
//! 5. **Uniqueness.** The allocation is owned by exactly one `AlignedBuf`; it
//!    is shared only after being frozen into a [`crate::Buffer`], which hands
//!    out `&[u8]` and never `&mut [u8]`.
//!
//! Invariants 4 and 5 are what make `unsafe impl Send`/`Sync` sound: the
//! buffer behaves exactly like a `Box<[u8]>` with a stricter alignment.
//!
//! # Failure behaviour
//!
//! The infallible API mirrors `Vec`: allocation failure calls
//! [`std::alloc::handle_alloc_error`] and capacity overflow aborts. Callers
//! that must survive both have `try_` counterparts returning
//! [`DataError::AllocationFailed`] / [`DataError::CapacityOverflow`].

use std::alloc::{Layout, alloc, alloc_zeroed, dealloc, handle_alloc_error, realloc};
use std::fmt;
use std::mem::MaybeUninit;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

use crate::error::DataError;

/// Alignment, in bytes, that every [`AlignedBuf`] allocation satisfies.
///
/// 64 bytes is the Arrow specification's recommended buffer alignment and the
/// cache-line/AVX-512 width AstRS targets (blueprint §6.1).
pub const ALIGNMENT: usize = 64;

/// Smallest non-zero allocation, in bytes. One alignment line.
const MIN_CAPACITY: usize = ALIGNMENT;

/// Rounds `len` up to the next multiple of [`ALIGNMENT`].
///
/// This is the Arrow body-padding rule: every buffer in an IPC body starts on
/// a 64-byte boundary, so each buffer's on-wire length is padded up.
///
/// Returns [`None`] when the rounded value would overflow `usize`.
///
/// ```
/// use astrs_data::buffer::pad_to_alignment;
///
/// assert_eq!(pad_to_alignment(0), Some(0));
/// assert_eq!(pad_to_alignment(1), Some(64));
/// assert_eq!(pad_to_alignment(64), Some(64));
/// assert_eq!(pad_to_alignment(65), Some(128));
/// assert_eq!(pad_to_alignment(usize::MAX), None);
/// ```
#[inline]
#[must_use]
pub const fn pad_to_alignment(len: usize) -> Option<usize> {
    len.checked_next_multiple_of(ALIGNMENT)
}

/// Number of padding bytes Arrow requires after a buffer of `len` bytes.
///
/// ```
/// use astrs_data::buffer::padding_for;
///
/// assert_eq!(padding_for(0), 0);
/// assert_eq!(padding_for(1), 63);
/// assert_eq!(padding_for(64), 0);
/// ```
#[inline]
#[must_use]
pub const fn padding_for(len: usize) -> usize {
    let rem = len % ALIGNMENT;
    if rem == 0 { 0 } else { ALIGNMENT - rem }
}

/// The dangling, never-dereferenced pointer used by empty buffers.
///
/// Its address is [`ALIGNMENT`], so invariant 1 holds even when no allocation
/// exists and `slice::from_raw_parts(ptr, 0)` stays sound.
#[inline]
const fn dangling() -> NonNull<u8> {
    // SAFETY: `ALIGNMENT` is a non-zero constant, so the address is non-null.
    // The pointer carries no provenance and is never dereferenced; it only
    // ever backs zero-length slices, for which any non-null aligned address is
    // valid.
    unsafe { NonNull::new_unchecked(std::ptr::without_provenance_mut(ALIGNMENT)) }
}

/// Reports an unrecoverable allocation problem the way the standard library
/// does, then aborts. Never unwinds, so no `Drop` runs on a half-built buffer.
#[cold]
#[inline(never)]
fn alloc_abort(error: &DataError) -> ! {
    if let DataError::AllocationFailed { bytes } = *error
        && let Ok(layout) = Layout::from_size_align(bytes, ALIGNMENT)
    {
        handle_alloc_error(layout)
    }
    eprintln!("astrs-data: fatal buffer error: {error}");
    std::process::abort()
}

/// A 64-byte aligned, growable, owned byte buffer.
///
/// `AlignedBuf` is the mutable half of the buffer story: builders write into
/// it, then freeze it into an immutable, `Arc`-shared [`crate::Buffer`] window
/// that arrays slice for free.
///
/// ```
/// use astrs_data::{ALIGNMENT, AlignedBuf};
///
/// let mut buf = AlignedBuf::with_capacity(10);
/// buf.extend_from_slice(b"astrs");
/// assert_eq!(buf.as_slice(), b"astrs");
/// assert_eq!(buf.as_ptr() as usize % ALIGNMENT, 0);
///
/// // Padding a body to the Arrow rule is one call.
/// buf.pad_to_alignment();
/// assert_eq!(buf.len(), 64);
/// ```
pub struct AlignedBuf {
    /// Start of the allocation; see invariants 1–3.
    ptr: NonNull<u8>,
    /// Initialised prefix length; see invariant 4.
    len: usize,
    /// Allocated size in bytes, a multiple of [`ALIGNMENT`] when non-zero.
    capacity: usize,
}

// SAFETY: `AlignedBuf` uniquely owns its allocation (invariant 5) and exposes
// it only through `&self`/`&mut self`, exactly like `Box<[u8]>`. Bytes have no
// interior mutability and no thread affinity, so sending the owner to another
// thread and sharing `&AlignedBuf` are both sound.
unsafe impl Send for AlignedBuf {}
// SAFETY: see the `Send` justification; `&AlignedBuf` only yields `&[u8]`.
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    /// Creates an empty buffer that owns no allocation.
    ///
    /// ```
    /// use astrs_data::AlignedBuf;
    ///
    /// let buf = AlignedBuf::new();
    /// assert!(buf.is_empty());
    /// assert_eq!(buf.capacity(), 0);
    /// ```
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            ptr: dangling(),
            len: 0,
            capacity: 0,
        }
    }

    /// Creates an empty buffer able to hold at least `capacity` bytes.
    ///
    /// The real capacity is rounded up to a multiple of [`ALIGNMENT`].
    ///
    /// # Aborts
    ///
    /// On allocation failure or capacity overflow, exactly like `Vec`.
    ///
    /// ```
    /// use astrs_data::AlignedBuf;
    ///
    /// let buf = AlignedBuf::with_capacity(10);
    /// assert_eq!(buf.len(), 0);
    /// assert_eq!(buf.capacity(), 64);
    /// ```
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        match Self::try_with_capacity(capacity) {
            Ok(buf) => buf,
            Err(error) => alloc_abort(&error),
        }
    }

    /// Fallible [`AlignedBuf::with_capacity`].
    ///
    /// ```
    /// use astrs_data::AlignedBuf;
    ///
    /// assert!(AlignedBuf::try_with_capacity(128).is_ok());
    /// assert!(AlignedBuf::try_with_capacity(usize::MAX).is_err());
    /// ```
    pub fn try_with_capacity(capacity: usize) -> Result<Self, DataError> {
        let mut buf = Self::new();
        if capacity > 0 {
            buf.try_grow_to(capacity)?;
        }
        buf.assert_invariants();
        Ok(buf)
    }

    /// Creates a buffer of `len` zero bytes.
    ///
    /// Uses [`std::alloc::alloc_zeroed`], so on most platforms the pages come
    /// pre-zeroed from the kernel and no memset happens.
    ///
    /// ```
    /// use astrs_data::AlignedBuf;
    ///
    /// let buf = AlignedBuf::zeroed(3);
    /// assert_eq!(buf.as_slice(), &[0, 0, 0]);
    /// ```
    #[must_use]
    pub fn zeroed(len: usize) -> Self {
        match Self::try_zeroed(len) {
            Ok(buf) => buf,
            Err(error) => alloc_abort(&error),
        }
    }

    /// Fallible [`AlignedBuf::zeroed`].
    pub fn try_zeroed(len: usize) -> Result<Self, DataError> {
        if len == 0 {
            return Ok(Self::new());
        }
        let capacity = Self::layout_capacity(len)?;
        let layout = Self::layout_for(capacity)?;
        // SAFETY: `capacity >= MIN_CAPACITY > 0`, so the layout has non-zero
        // size — the only precondition of `alloc_zeroed`.
        let raw = unsafe { alloc_zeroed(layout) };
        let Some(ptr) = NonNull::new(raw) else {
            return Err(DataError::AllocationFailed { bytes: capacity });
        };
        let buf = Self { ptr, len, capacity };
        buf.assert_invariants();
        Ok(buf)
    }

    /// Creates a buffer holding a copy of `bytes`.
    ///
    /// ```
    /// use astrs_data::AlignedBuf;
    ///
    /// let buf = AlignedBuf::from_slice(b"hello");
    /// assert_eq!(buf.as_slice(), b"hello");
    /// ```
    #[must_use]
    pub fn from_slice(bytes: &[u8]) -> Self {
        let mut buf = Self::with_capacity(bytes.len());
        buf.extend_from_slice(bytes);
        buf
    }

    /// Number of initialised bytes.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` when the buffer holds no bytes.
    #[inline]
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of bytes the current allocation can hold.
    #[inline]
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Raw pointer to the first byte. Always [`ALIGNMENT`]-aligned.
    ///
    /// The pointer stays valid until the buffer grows or is dropped.
    #[inline]
    #[must_use]
    pub const fn as_ptr(&self) -> *const u8 {
        self.ptr.as_ptr().cast_const()
    }

    /// Mutable raw pointer to the first byte. Always [`ALIGNMENT`]-aligned.
    #[inline]
    #[must_use]
    pub const fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr.as_ptr()
    }

    /// The initialised bytes.
    #[inline]
    #[must_use]
    pub const fn as_slice(&self) -> &[u8] {
        // SAFETY: invariants 1 and 4 — `ptr` is non-null and aligned, and the
        // first `len` bytes are initialised and owned by `self`. The lifetime
        // is tied to `&self`, so no mutation can race with the borrow.
        unsafe { std::slice::from_raw_parts(self.as_ptr(), self.len) }
    }

    /// The initialised bytes, mutably.
    #[inline]
    #[must_use]
    pub const fn as_mut_slice(&mut self) -> &mut [u8] {
        let len = self.len;
        // SAFETY: as `as_slice`, plus `&mut self` proves unique access
        // (invariant 5).
        unsafe { std::slice::from_raw_parts_mut(self.as_mut_ptr(), len) }
    }

    /// The uninitialised tail between `len` and `capacity`.
    ///
    /// Combine with [`AlignedBuf::set_len`] to fill a buffer without paying
    /// for a zeroing pass.
    ///
    /// ```
    /// use astrs_data::AlignedBuf;
    ///
    /// let mut buf = AlignedBuf::with_capacity(4);
    /// let spare = buf.spare_capacity_mut();
    /// spare[0].write(1);
    /// spare[1].write(2);
    /// // SAFETY: two bytes were just initialised.
    /// unsafe { buf.set_len(2) };
    /// assert_eq!(buf.as_slice(), &[1, 2]);
    /// ```
    #[inline]
    pub const fn spare_capacity_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        let spare = self.capacity - self.len;
        let len = self.len;
        // SAFETY: bytes `[len, capacity)` lie inside the allocation
        // (invariants 3 and 4) and are owned exclusively by `self`. They may be
        // uninitialised, which is exactly what `MaybeUninit<u8>` models.
        unsafe { std::slice::from_raw_parts_mut(self.as_mut_ptr().add(len).cast(), spare) }
    }

    /// Overrides the initialised length.
    ///
    /// # Safety
    ///
    /// * `new_len` must not exceed [`AlignedBuf::capacity`].
    /// * All bytes in `[0, new_len)` must be initialised.
    ///
    /// Shrinking is always sound; growing is only sound after the caller has
    /// written the bytes, e.g. through [`AlignedBuf::spare_capacity_mut`].
    #[inline]
    pub const unsafe fn set_len(&mut self, new_len: usize) {
        debug_assert!(new_len <= self.capacity);
        self.len = new_len;
    }

    /// Ensures room for `additional` more bytes, growing geometrically.
    ///
    /// # Aborts
    ///
    /// On allocation failure or capacity overflow.
    ///
    /// ```
    /// use astrs_data::AlignedBuf;
    ///
    /// let mut buf = AlignedBuf::new();
    /// buf.reserve(100);
    /// assert!(buf.capacity() >= 100);
    /// ```
    pub fn reserve(&mut self, additional: usize) {
        if let Err(error) = self.try_reserve(additional) {
            alloc_abort(&error);
        }
    }

    /// Fallible [`AlignedBuf::reserve`].
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), DataError> {
        let Some(required) = self.len.checked_add(additional) else {
            return Err(DataError::CapacityOverflow {
                requested: additional,
            });
        };
        if required <= self.capacity {
            return Ok(());
        }
        self.try_grow_to(required)
    }

    /// Appends one byte.
    #[inline]
    pub fn push(&mut self, byte: u8) {
        if self.len == self.capacity {
            self.reserve(1);
        }
        // SAFETY: the branch above guarantees `len < capacity`, so `ptr + len`
        // is inside the allocation and writable (invariants 3 and 5).
        unsafe { self.as_mut_ptr().add(self.len).write(byte) };
        self.len += 1;
    }

    /// Appends a byte slice.
    ///
    /// ```
    /// use astrs_data::AlignedBuf;
    ///
    /// let mut buf = AlignedBuf::new();
    /// buf.extend_from_slice(b"ast");
    /// buf.extend_from_slice(b"rs");
    /// assert_eq!(buf.as_slice(), b"astrs");
    /// ```
    pub fn extend_from_slice(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.reserve(bytes.len());
        // SAFETY: `reserve` guarantees `capacity - len >= bytes.len()`, so the
        // destination range lies inside the allocation. Source and destination
        // cannot overlap: `bytes` borrows immutably while `self` is borrowed
        // mutably, so they are distinct allocations.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.as_mut_ptr().add(self.len),
                bytes.len(),
            );
        }
        self.len += bytes.len();
    }

    /// Appends `count` zero bytes.
    ///
    /// ```
    /// use astrs_data::AlignedBuf;
    ///
    /// let mut buf = AlignedBuf::from_slice(&[1]);
    /// buf.extend_zeroed(2);
    /// assert_eq!(buf.as_slice(), &[1, 0, 0]);
    /// ```
    pub fn extend_zeroed(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        self.reserve(count);
        // SAFETY: `reserve` guarantees the range `[len, len + count)` lies
        // inside the allocation and is exclusively owned.
        unsafe { self.as_mut_ptr().add(self.len).write_bytes(0, count) };
        self.len += count;
    }

    /// Grows or shrinks to `new_len`, filling new bytes with `value`.
    ///
    /// ```
    /// use astrs_data::AlignedBuf;
    ///
    /// let mut buf = AlignedBuf::from_slice(&[1, 2, 3]);
    /// buf.resize(5, 9);
    /// assert_eq!(buf.as_slice(), &[1, 2, 3, 9, 9]);
    /// buf.resize(2, 0);
    /// assert_eq!(buf.as_slice(), &[1, 2]);
    /// ```
    pub fn resize(&mut self, new_len: usize, value: u8) {
        if new_len <= self.len {
            self.len = new_len;
            return;
        }
        let additional = new_len - self.len;
        if value == 0 {
            self.extend_zeroed(additional);
            return;
        }
        self.reserve(additional);
        // SAFETY: `reserve` guarantees the range `[len, new_len)` lies inside
        // the allocation and is exclusively owned.
        unsafe {
            self.as_mut_ptr()
                .add(self.len)
                .write_bytes(value, additional)
        };
        self.len = new_len;
    }

    /// Shortens the buffer, keeping the first `new_len` bytes. No-op when
    /// `new_len` is already at or beyond the current length.
    #[inline]
    pub const fn truncate(&mut self, new_len: usize) {
        if new_len < self.len {
            self.len = new_len;
        }
    }

    /// Drops every byte, keeping the allocation for reuse.
    #[inline]
    pub const fn clear(&mut self) {
        self.len = 0;
    }

    /// Appends zero bytes until the length is a multiple of [`ALIGNMENT`].
    ///
    /// This is the Arrow body-padding rule (blueprint §6.1): each buffer in an
    /// IPC body is followed by padding so the next buffer starts 64-byte
    /// aligned.
    ///
    /// ```
    /// use astrs_data::AlignedBuf;
    ///
    /// let mut buf = AlignedBuf::from_slice(&[7; 65]);
    /// buf.pad_to_alignment();
    /// assert_eq!(buf.len(), 128);
    /// assert_eq!(buf.as_slice()[65..], [0; 63]);
    /// ```
    pub fn pad_to_alignment(&mut self) {
        let padding = padding_for(self.len);
        if padding > 0 {
            self.extend_zeroed(padding);
        }
    }

    /// Releases capacity beyond the padded length.
    ///
    /// The result still satisfies every invariant: the retained capacity is a
    /// multiple of [`ALIGNMENT`] and at least `len`.
    ///
    /// ```
    /// use astrs_data::AlignedBuf;
    ///
    /// let mut buf = AlignedBuf::with_capacity(4096);
    /// buf.extend_from_slice(b"tiny");
    /// buf.shrink_to_fit();
    /// assert_eq!(buf.capacity(), 64);
    /// assert_eq!(buf.as_slice(), b"tiny");
    /// ```
    pub fn shrink_to_fit(&mut self) {
        let Some(target) = pad_to_alignment(self.len) else {
            return;
        };
        if target >= self.capacity {
            return;
        }
        if target == 0 {
            let old = self.capacity;
            self.capacity = 0;
            let ptr = std::mem::replace(&mut self.ptr, dangling());
            if let Ok(layout) = Layout::from_size_align(old, ALIGNMENT) {
                // SAFETY: invariant 3 — `ptr` came from this exact layout and
                // is not referenced anywhere else (invariant 5).
                unsafe { dealloc(ptr.as_ptr(), layout) };
            }
            self.assert_invariants();
            return;
        }
        let (Ok(old), Ok(_new)) = (
            Layout::from_size_align(self.capacity, ALIGNMENT),
            Layout::from_size_align(target, ALIGNMENT),
        ) else {
            return;
        };
        // SAFETY: `ptr` was allocated with `old` (invariant 3), `target` is
        // non-zero, and `target` rounded up to `ALIGNMENT` is a valid layout
        // size because `Layout::from_size_align` accepted it above.
        let raw = unsafe { realloc(self.ptr.as_ptr(), old, target) };
        if let Some(ptr) = NonNull::new(raw) {
            self.ptr = ptr;
            self.capacity = target;
        }
        self.assert_invariants();
    }

    /// Reports the alignment the start address actually has, as a power of two.
    ///
    /// Always at least [`ALIGNMENT`]; useful in tests and diagnostics.
    ///
    /// ```
    /// use astrs_data::{ALIGNMENT, AlignedBuf};
    ///
    /// let buf = AlignedBuf::with_capacity(1);
    /// assert!(buf.address_alignment() >= ALIGNMENT);
    /// ```
    #[must_use]
    pub fn address_alignment(&self) -> usize {
        let addr = self.as_ptr() as usize;
        if addr == 0 {
            return ALIGNMENT;
        }
        1usize << addr.trailing_zeros().min(usize::BITS - 1)
    }

    /// Debug-only re-check of the module's invariant set.
    ///
    /// Compiles to nothing in release builds.
    #[inline]
    pub(crate) fn assert_invariants(&self) {
        debug_assert!(self.len <= self.capacity, "invariant 4: len <= capacity");
        debug_assert_eq!(
            self.as_ptr() as usize % ALIGNMENT,
            0,
            "invariant 1: 64-byte aligned start address"
        );
        debug_assert!(
            self.capacity.is_multiple_of(ALIGNMENT),
            "invariant 3: capacity is a whole number of alignment lines"
        );
        debug_assert!(
            self.capacity > 0 || self.as_ptr() as usize == ALIGNMENT,
            "invariant 2: empty buffers hold the dangling sentinel"
        );
    }

    /// Rounds a byte count up to an allocation capacity.
    fn layout_capacity(required: usize) -> Result<usize, DataError> {
        pad_to_alignment(required.max(MIN_CAPACITY)).ok_or(DataError::CapacityOverflow {
            requested: required,
        })
    }

    /// Builds the layout for an already-rounded capacity.
    fn layout_for(capacity: usize) -> Result<Layout, DataError> {
        Layout::from_size_align(capacity, ALIGNMENT).map_err(|_| DataError::CapacityOverflow {
            requested: capacity,
        })
    }

    /// Grows the allocation so it holds at least `required` bytes.
    ///
    /// Growth is geometric (doubling) so repeated `push`/`extend_from_slice`
    /// stays amortised O(1), and the result is always alignment-rounded.
    fn try_grow_to(&mut self, required: usize) -> Result<(), DataError> {
        if required <= self.capacity {
            return Ok(());
        }
        let doubled = self.capacity.saturating_mul(2);
        let capacity = Self::layout_capacity(required.max(doubled))?;
        let layout = Self::layout_for(capacity)?;

        let raw = if self.capacity == 0 {
            // SAFETY: `capacity >= MIN_CAPACITY > 0`, so the layout has
            // non-zero size.
            unsafe { alloc(layout) }
        } else {
            let old = Self::layout_for(self.capacity)?;
            // SAFETY: invariant 3 — `ptr` came from `old`. `capacity` is
            // non-zero and `Layout::from_size_align` above proved that it
            // rounds to a valid layout size, which is exactly what `realloc`
            // requires.
            unsafe { realloc(self.ptr.as_ptr(), old, capacity) }
        };

        let Some(ptr) = NonNull::new(raw) else {
            return Err(DataError::AllocationFailed { bytes: capacity });
        };
        self.ptr = ptr;
        self.capacity = capacity;
        self.assert_invariants();
        Ok(())
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        if self.capacity == 0 {
            return;
        }
        let Ok(layout) = Layout::from_size_align(self.capacity, ALIGNMENT) else {
            // Unreachable given invariant 3; leaking beats deallocating with a
            // layout that does not match the allocation.
            return;
        };
        // SAFETY: invariant 3 — the pointer came from this exact layout — and
        // invariant 5 — nothing else references the allocation, because `Drop`
        // takes `&mut self` on the sole owner.
        unsafe { dealloc(self.ptr.as_ptr(), layout) };
    }
}

impl Clone for AlignedBuf {
    /// Deep-copies the initialised bytes into a fresh allocation.
    ///
    /// The clone's capacity is the *padded length*, not the source capacity:
    /// cloning a buffer with a large speculative reserve must not duplicate
    /// that reserve.
    ///
    /// ```
    /// use astrs_data::AlignedBuf;
    ///
    /// let mut a = AlignedBuf::with_capacity(4096);
    /// a.extend_from_slice(b"xy");
    /// let b = a.clone();
    /// assert_eq!(b.as_slice(), b"xy");
    /// assert_eq!(b.capacity(), 64);
    /// assert_ne!(a.as_ptr(), b.as_ptr());
    /// ```
    fn clone(&self) -> Self {
        Self::from_slice(self.as_slice())
    }
}

impl Default for AlignedBuf {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl DerefMut for AlignedBuf {
    #[inline]
    fn deref_mut(&mut self) -> &mut [u8] {
        self.as_mut_slice()
    }
}

impl AsRef<[u8]> for AlignedBuf {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl AsMut<[u8]> for AlignedBuf {
    #[inline]
    fn as_mut(&mut self) -> &mut [u8] {
        self.as_mut_slice()
    }
}

impl PartialEq for AlignedBuf {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl Eq for AlignedBuf {}

impl std::hash::Hash for AlignedBuf {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_slice().hash(state);
    }
}

impl fmt::Debug for AlignedBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const PREVIEW: usize = 16;
        let preview = &self.as_slice()[..self.len.min(PREVIEW)];
        f.debug_struct("AlignedBuf")
            .field("len", &self.len)
            .field("capacity", &self.capacity)
            .field(
                "aligned",
                &(self.as_ptr() as usize).is_multiple_of(ALIGNMENT),
            )
            .field("head", &preview)
            .finish_non_exhaustive()
    }
}

impl From<&[u8]> for AlignedBuf {
    #[inline]
    fn from(bytes: &[u8]) -> Self {
        Self::from_slice(bytes)
    }
}

impl From<Vec<u8>> for AlignedBuf {
    #[inline]
    fn from(bytes: Vec<u8>) -> Self {
        Self::from_slice(&bytes)
    }
}

impl<const N: usize> From<[u8; N]> for AlignedBuf {
    #[inline]
    fn from(bytes: [u8; N]) -> Self {
        Self::from_slice(&bytes)
    }
}

impl Extend<u8> for AlignedBuf {
    fn extend<I: IntoIterator<Item = u8>>(&mut self, iter: I) {
        let iter = iter.into_iter();
        let (lower, _) = iter.size_hint();
        self.reserve(lower);
        for byte in iter {
            self.push(byte);
        }
    }
}

impl<'a> Extend<&'a u8> for AlignedBuf {
    fn extend<I: IntoIterator<Item = &'a u8>>(&mut self, iter: I) {
        self.extend(iter.into_iter().copied());
    }
}

impl FromIterator<u8> for AlignedBuf {
    fn from_iter<I: IntoIterator<Item = u8>>(iter: I) -> Self {
        let mut buf = Self::new();
        buf.extend(iter);
        buf
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    const fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn buffer_is_send_and_sync() {
        assert_send_sync::<AlignedBuf>();
    }

    #[test]
    fn empty_buffer_owns_nothing() {
        let buf = AlignedBuf::new();
        assert_eq!(buf.len(), 0);
        assert_eq!(buf.capacity(), 0);
        assert!(buf.is_empty());
        assert_eq!(buf.as_slice(), &[] as &[u8]);
        assert_eq!(buf.as_ptr() as usize % ALIGNMENT, 0);
    }

    #[test]
    fn every_allocation_is_64_byte_aligned() {
        for size in [1usize, 2, 7, 63, 64, 65, 127, 128, 1000, 4096, 100_000] {
            let buf = AlignedBuf::with_capacity(size);
            assert_eq!(
                buf.as_ptr() as usize % ALIGNMENT,
                0,
                "capacity {size} produced a misaligned start address"
            );
            assert!(buf.capacity() >= size);
            assert_eq!(buf.capacity() % ALIGNMENT, 0);
        }
    }

    #[test]
    fn alignment_survives_growth() {
        let mut buf = AlignedBuf::new();
        let mut seen = Vec::new();
        for i in 0..5000u32 {
            buf.push((i % 251) as u8);
            assert_eq!(buf.as_ptr() as usize % ALIGNMENT, 0);
            seen.push((i % 251) as u8);
        }
        assert_eq!(buf.as_slice(), seen.as_slice());
    }

    #[test]
    fn growth_is_geometric() {
        let mut buf = AlignedBuf::new();
        buf.reserve(1);
        assert_eq!(buf.capacity(), MIN_CAPACITY);
        buf.resize(MIN_CAPACITY, 1);
        buf.push(2);
        assert_eq!(buf.capacity(), MIN_CAPACITY * 2);
    }

    #[test]
    fn reserve_is_idempotent_when_capacity_suffices() {
        let mut buf = AlignedBuf::with_capacity(256);
        let ptr = buf.as_ptr();
        buf.reserve(10);
        assert_eq!(buf.as_ptr(), ptr);
    }

    #[test]
    fn zeroed_allocates_initialised_bytes() {
        let buf = AlignedBuf::zeroed(100);
        assert_eq!(buf.len(), 100);
        assert!(buf.as_slice().iter().all(|b| *b == 0));
        assert_eq!(buf.as_ptr() as usize % ALIGNMENT, 0);
        assert_eq!(AlignedBuf::zeroed(0).capacity(), 0);
    }

    #[test]
    fn clone_deep_copies_and_drops_slack() {
        let mut original = AlignedBuf::with_capacity(8192);
        original.extend_from_slice(b"astrs-data");
        let copy = original.clone();
        assert_eq!(copy.as_slice(), original.as_slice());
        assert_ne!(copy.as_ptr(), original.as_ptr());
        assert_eq!(copy.capacity(), ALIGNMENT);
        assert_eq!(copy.as_ptr() as usize % ALIGNMENT, 0);
        drop(original);
        assert_eq!(copy.as_slice(), b"astrs-data");
    }

    #[test]
    fn clone_of_empty_stays_empty() {
        let empty = AlignedBuf::new();
        let copy = empty.clone();
        assert_eq!(copy.capacity(), 0);
        assert!(copy.is_empty());
    }

    #[test]
    fn mutation_through_deref_mut() {
        let mut buf = AlignedBuf::from_slice(&[1, 2, 3]);
        buf[1] = 9;
        assert_eq!(buf.as_slice(), &[1, 9, 3]);
        buf.as_mut_slice()[2] = 8;
        assert_eq!(&*buf, &[1, 9, 8]);
    }

    #[test]
    fn resize_grows_and_shrinks() {
        let mut buf = AlignedBuf::from_slice(&[1, 2, 3]);
        buf.resize(6, 7);
        assert_eq!(buf.as_slice(), &[1, 2, 3, 7, 7, 7]);
        buf.resize(1, 0);
        assert_eq!(buf.as_slice(), &[1]);
        buf.resize(3, 0);
        assert_eq!(buf.as_slice(), &[1, 0, 0]);
    }

    #[test]
    fn truncate_and_clear() {
        let mut buf = AlignedBuf::from_slice(&[1, 2, 3, 4]);
        buf.truncate(10);
        assert_eq!(buf.len(), 4);
        buf.truncate(2);
        assert_eq!(buf.as_slice(), &[1, 2]);
        let capacity = buf.capacity();
        buf.clear();
        assert!(buf.is_empty());
        assert_eq!(buf.capacity(), capacity);
    }

    #[test]
    fn padding_follows_the_arrow_rule() {
        assert_eq!(padding_for(0), 0);
        assert_eq!(padding_for(1), 63);
        assert_eq!(padding_for(63), 1);
        assert_eq!(padding_for(64), 0);
        assert_eq!(padding_for(65), 63);

        let mut buf = AlignedBuf::from_slice(&[1; 3]);
        buf.pad_to_alignment();
        assert_eq!(buf.len(), 64);
        assert_eq!(&buf.as_slice()[..3], &[1, 1, 1]);
        assert!(buf.as_slice()[3..].iter().all(|b| *b == 0));
        buf.pad_to_alignment();
        assert_eq!(buf.len(), 64, "padding an aligned buffer is a no-op");
    }

    #[test]
    fn pad_to_alignment_helper_reports_overflow() {
        assert_eq!(pad_to_alignment(0), Some(0));
        assert_eq!(pad_to_alignment(usize::MAX), None);
        assert_eq!(pad_to_alignment(usize::MAX - 10), None);
        // The largest value that still rounds without overflowing.
        assert_eq!(pad_to_alignment(usize::MAX - 63), Some(usize::MAX - 63));
    }

    #[test]
    fn shrink_to_fit_keeps_data_and_alignment() {
        let mut buf = AlignedBuf::with_capacity(16_384);
        buf.extend_from_slice(&[3; 70]);
        buf.shrink_to_fit();
        assert_eq!(buf.capacity(), 128);
        assert_eq!(buf.len(), 70);
        assert!(buf.as_slice().iter().all(|b| *b == 3));
        assert_eq!(buf.as_ptr() as usize % ALIGNMENT, 0);
    }

    #[test]
    fn shrink_to_fit_releases_everything_when_empty() {
        let mut buf = AlignedBuf::with_capacity(4096);
        buf.shrink_to_fit();
        assert_eq!(buf.capacity(), 0);
        assert_eq!(buf.as_ptr() as usize, ALIGNMENT);
        // The buffer must remain usable after releasing its allocation.
        buf.extend_from_slice(b"reuse");
        assert_eq!(buf.as_slice(), b"reuse");
    }

    #[test]
    fn spare_capacity_and_set_len() {
        let mut buf = AlignedBuf::with_capacity(8);
        {
            let spare = buf.spare_capacity_mut();
            assert_eq!(spare.len(), ALIGNMENT);
            for (i, slot) in spare.iter_mut().take(4).enumerate() {
                slot.write(i as u8);
            }
        }
        // SAFETY: four bytes were just initialised.
        unsafe { buf.set_len(4) };
        assert_eq!(buf.as_slice(), &[0, 1, 2, 3]);
        // SAFETY: shrinking is always sound.
        unsafe { buf.set_len(2) };
        assert_eq!(buf.as_slice(), &[0, 1]);
    }

    #[test]
    fn try_paths_report_capacity_overflow() {
        assert_eq!(
            AlignedBuf::try_with_capacity(usize::MAX),
            Err(DataError::CapacityOverflow {
                requested: usize::MAX
            })
        );
        assert!(AlignedBuf::try_zeroed(usize::MAX).is_err());

        let mut buf = AlignedBuf::from_slice(&[1, 2, 3]);
        assert_eq!(
            buf.try_reserve(usize::MAX),
            Err(DataError::CapacityOverflow {
                requested: usize::MAX
            })
        );
        assert_eq!(buf.as_slice(), &[1, 2, 3], "a failed reserve is a no-op");
    }

    #[test]
    fn extend_and_collect() {
        let mut buf: AlignedBuf = (0u8..10).collect();
        assert_eq!(buf.len(), 10);
        buf.extend([100u8, 101]);
        buf.extend(&[102u8]);
        assert_eq!(&buf.as_slice()[10..], &[100, 101, 102]);
    }

    #[test]
    fn conversions() {
        assert_eq!(AlignedBuf::from(&b"abc"[..]).as_slice(), b"abc");
        assert_eq!(AlignedBuf::from(vec![1u8, 2]).as_slice(), &[1, 2]);
        assert_eq!(AlignedBuf::from([9u8; 3]).as_slice(), &[9, 9, 9]);
    }

    #[test]
    fn equality_and_hash_use_contents_only() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let a = AlignedBuf::from_slice(b"same");
        let mut b = AlignedBuf::with_capacity(4096);
        b.extend_from_slice(b"same");
        assert_eq!(a, b);

        let hash = |buf: &AlignedBuf| {
            let mut hasher = DefaultHasher::new();
            buf.hash(&mut hasher);
            hasher.finish()
        };
        assert_eq!(hash(&a), hash(&b));
        assert_ne!(a, AlignedBuf::from_slice(b"other"));
    }

    #[test]
    fn debug_reports_alignment() {
        let buf = AlignedBuf::from_slice(&[1, 2, 3]);
        let rendered = format!("{buf:?}");
        assert!(rendered.contains("aligned: true"), "{rendered}");
        assert!(rendered.contains("len: 3"), "{rendered}");
    }

    #[test]
    fn address_alignment_is_at_least_the_constant() {
        assert!(AlignedBuf::new().address_alignment() >= ALIGNMENT);
        assert!(AlignedBuf::with_capacity(1000).address_alignment() >= ALIGNMENT);
    }

    #[test]
    fn extend_from_empty_slice_is_a_noop() {
        let mut buf = AlignedBuf::new();
        buf.extend_from_slice(&[]);
        assert_eq!(buf.capacity(), 0);
        buf.extend_zeroed(0);
        assert_eq!(buf.capacity(), 0);
    }

    #[test]
    fn large_allocation_round_trips() {
        let mut buf = AlignedBuf::zeroed(1 << 20);
        assert_eq!(buf.len(), 1 << 20);
        buf.as_mut_slice()[(1 << 20) - 1] = 42;
        assert_eq!(buf.as_slice()[(1 << 20) - 1], 42);
        assert_eq!(buf.as_ptr() as usize % ALIGNMENT, 0);
    }
}
