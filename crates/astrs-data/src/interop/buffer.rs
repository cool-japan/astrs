//! [`Buffer`]/[`Bitmap`] <-> arrow-rs buffer conversions.
//!
//! # The zero-copy direction: astrs -> arrow
//!
//! Every conversion in this direction is zero-copy, unconditionally — no
//! alignment fallback, no realigning copy, ever needed. Two facts about
//! `astrs-data`'s own buffer stack make that true rather than merely usual:
//!
//! 1. Raw byte windows ([`Buffer::as_slice`]) carry no alignment requirement
//!    at all — arrow-rs's own [`ArrowBuffer::from_custom_allocation`] has
//!    none either, and neither does the [`BufferSpec::VariableWidth`]/
//!    [`BufferSpec::BitMap`] slots `arrow_data::layout()` assigns to
//!    `Binary`/`Utf8`/`FixedSizeBinary` value bytes and to `Boolean` values.
//! 2. Typed windows ([`crate::ScalarBuffer<T>`]) carry `T`-alignment as a
//!    construction invariant, not a per-call check: [`AlignedBuf`]'s start
//!    address is always 64-byte aligned, `size_of::<T>()` is always a
//!    multiple of `align_of::<T>()` for every `T` this crate supports (Rust's
//!    own layout rule), and slicing is always element-granular — so every
//!    window a `ScalarBuffer<T>` can ever produce stays `T`-aligned. There is
//!    no path to an unaligned `ScalarBuffer<T>` to defend against.
//!
//! [`to_arrow_buffer`] therefore hands `arrow_data::ArrayDataBuilder::build`
//! buffers that are always already aligned; the crate still opts every
//! forward conversion into `ArrayDataBuilder::align_buffers(true)` (see
//! `crate::interop::array::to_arrow`) as a defensive repair-on-mismatch that
//! this module's own claim predicts will never fire — the round-trip tests'
//! pointer-equality assertions are what actually holds that claim honest,
//! not this doc comment.
//!
//! [`BufferSpec::VariableWidth`]: arrow_data::BufferSpec::VariableWidth
//! [`BufferSpec::BitMap`]: arrow_data::BufferSpec::BitMap
//! [`AlignedBuf`]: crate::AlignedBuf
//!
//! # The copying direction: arrow -> astrs
//!
//! There is no zero-copy path back, and not for lack of trying: an
//! `arrow_buffer::Buffer`'s backing allocation is reachable only through
//! `arrow_buffer::Bytes` and `arrow_buffer::alloc::Deallocation`, both
//! `pub(crate)` to arrow-buffer (verified against its 59.2.0 source; neither
//! name resolves from outside the crate). There is consequently no public API
//! to ask "does this buffer's owner happen to be one of our own
//! `Arc<AlignedBuf>`s" and reclaim it — even the round-trip case (an
//! `arrow_buffer::Buffer` this same module built a moment ago) cannot be
//! recognised from the outside. And even given that recognition,
//! [`crate::AlignedBuf`] has no constructor that adopts foreign memory: its
//! `Drop` unconditionally calls [`std::alloc::dealloc`] with a
//! [`std::alloc::Layout`] it computed itself (its own safety-model doc,
//! invariant 3), so wrapping memory it did not allocate would be undefined
//! behaviour on drop. Every conversion below therefore copies once, with
//! [`Buffer::from_slice`]/[`crate::AlignedBuf::from_slice`].

use std::ptr::NonNull;
use std::sync::Arc;

use arrow_buffer::{BooleanBuffer, Buffer as ArrowBuffer, NullBuffer};

use crate::Buffer;
use crate::buffer::Bitmap;

/// Compile-time proof that `crate::Buffer` satisfies arrow-buffer's
/// [`arrow_buffer::alloc::Allocation`] bound (`RefUnwindSafe + Send + Sync +
/// 'static`), which is what lets [`to_arrow_buffer`] hand a cloned `Buffer`
/// to [`ArrowBuffer::from_custom_allocation`] as the owner without any
/// `unsafe impl` of our own. `Buffer` already asserts `Send + Sync` in its
/// own test suite (`crate::buffer::tests::buffer_is_send_and_sync`);
/// `RefUnwindSafe` holds structurally (an `Arc<AlignedBuf>` plus two
/// `usize`s, and raw pointers — `AlignedBuf`'s only non-trivial field — are
/// unconditionally `RefUnwindSafe` in `core`, unwinding through a raw pointer
/// having no unwind-safety story to violate). Asserted here, at compile time,
/// so a future field addition that breaks it fails to build at this exact
/// line instead of inside `ArrayDataBuilder::build` with a confusing trait
/// bound error.
const _: fn() = || {
    const fn assert_allocation<T: arrow_buffer::alloc::Allocation>() {}
    assert_allocation::<Buffer>();
};

/// Converts a byte window to an arrow-rs [`ArrowBuffer`], without copying the
/// bytes.
///
/// See the module docs for why this holds unconditionally, not merely "when
/// aligned". `owner` is a full [`Buffer`] clone (an `Arc<AlignedBuf>` bump
/// plus two `usize`s — not a copy of the bytes it addresses), which keeps
/// exactly the same backing allocation alive that `buffer` itself keeps
/// alive; when the arrow-rs side drops the last reference to it, `Buffer`'s
/// own `Drop` chain runs exactly as it would for any other last reference.
///
/// ```
/// use astrs_data::Buffer;
/// use astrs_data::interop::to_arrow_buffer;
///
/// let buffer = Buffer::from_slice(b"astrs");
/// let arrow_buf = to_arrow_buffer(&buffer);
/// assert_eq!(arrow_buf.as_slice(), b"astrs");
/// assert_eq!(arrow_buf.as_ptr(), buffer.as_ptr(), "no copy");
/// ```
#[must_use]
pub fn to_arrow_buffer(buffer: &Buffer) -> ArrowBuffer {
    let len = buffer.len();
    let owner: Arc<Buffer> = Arc::new(buffer.clone());
    // `owner` is `Arc<Buffer>`; `Buffer: Allocation` per the assertion above,
    // so this coerces to `Arc<dyn Allocation>` without help.
    let ptr = NonNull::new(owner.as_ptr().cast_mut()).unwrap_or(NonNull::dangling());
    // SAFETY:
    // * `ptr` is `owner.as_ptr()`, which is exactly the pointer
    //   `Buffer::as_slice` already dereferences (safely) for `len` bytes —
    //   the same validity requirement `from_custom_allocation` asks for.
    // * `owner` keeps that allocation alive for as long as any clone of it
    //   is: it is a `Buffer`, whose only path to deallocation is dropping
    //   the last `Arc<AlignedBuf>` reference, which `Deallocation::Custom`'s
    //   contract ("deallocation happens on `Allocation::drop`") is exactly
    //   built around. `arrow_buffer::Buffer` holds `owner` for precisely
    //   this reason: it will not drop it before the returned `ArrowBuffer`
    //   (and every clone taken from it) is gone.
    // * Neither side ever hands out `&mut [u8]` over this memory once
    //   frozen into a `Buffer` (`crate::buffer::Buffer` only ever yields
    //   `&[u8]`; `arrow_buffer::Buffer::as_slice` is likewise `&self`), so
    //   the two read-only views cannot race.
    unsafe { ArrowBuffer::from_custom_allocation(ptr, len, owner) }
}

/// Converts an arrow-rs [`ArrowBuffer`] to a byte window, copying the bytes.
///
/// See the module docs for why a copy is unavoidable here.
///
/// ```
/// use arrow_buffer::Buffer as ArrowBuffer;
/// use astrs_data::interop::from_arrow_buffer;
///
/// let arrow_buf = ArrowBuffer::from(&b"astrs"[..]);
/// let buffer = from_arrow_buffer(&arrow_buf);
/// assert_eq!(buffer.as_slice(), b"astrs");
/// ```
#[must_use]
pub fn from_arrow_buffer(buffer: &ArrowBuffer) -> Buffer {
    Buffer::from_slice(buffer.as_slice())
}

/// Converts a validity/value [`Bitmap`] to an arrow-rs [`BooleanBuffer`],
/// without copying the bits.
///
/// Both sides use the identical bit convention (blueprint §6.1 / `crate`'s
/// own [`Bitmap`] docs): LSB-first within a byte, `1` means valid/true. The
/// conversion is therefore a raw [`to_arrow_buffer`] plus passing
/// [`Bitmap::bit_offset`]/[`Bitmap::len`] straight through as
/// [`BooleanBuffer::new`]'s `bit_offset`/`bit_len` — no bit shifting, no
/// canonicalisation, and it works for an arbitrary (not necessarily
/// byte-aligned) bit offset because `BooleanBuffer` carries one natively,
/// exactly like [`Bitmap`] does.
///
/// ```
/// use astrs_data::Bitmap;
/// use astrs_data::interop::to_arrow_boolean_buffer;
///
/// let bits: Bitmap = [true, false, true, true].into_iter().collect();
/// let arrow_bits = to_arrow_boolean_buffer(&bits);
/// assert_eq!(arrow_bits.len(), 4);
/// assert!(arrow_bits.value(0));
/// assert!(!arrow_bits.value(1));
/// ```
#[must_use]
pub fn to_arrow_boolean_buffer(bitmap: &Bitmap) -> BooleanBuffer {
    BooleanBuffer::new(
        to_arrow_buffer(bitmap.buffer()),
        bitmap.bit_offset(),
        bitmap.len(),
    )
}

/// Converts an arrow-rs [`BooleanBuffer`] to a [`Bitmap`], copying the bits.
///
/// Copies because [`BooleanBuffer::inner`] hands back an [`ArrowBuffer`], and
/// every `ArrowBuffer` -> [`Buffer`] conversion copies (see the module docs).
///
/// ```
/// use arrow_buffer::BooleanBuffer;
/// use astrs_data::interop::from_arrow_boolean_buffer;
///
/// let arrow_bits = BooleanBuffer::from(vec![true, false, true]);
/// let bits = from_arrow_boolean_buffer(&arrow_bits);
/// assert_eq!(bits.iter().collect::<Vec<_>>(), vec![true, false, true]);
/// ```
#[must_use]
pub fn from_arrow_boolean_buffer(buffer: &BooleanBuffer) -> Bitmap {
    let raw = from_arrow_buffer(buffer.inner());
    // Always succeeds: `raw` is a byte-for-byte copy of `buffer.inner()`, so
    // it satisfies the exact `offset + len <= raw.len() * 8` bound that
    // `BooleanBuffer::new` already checked (and panics on) when `buffer`
    // itself was built. The fallback exists only so this function stays
    // infallible without `unwrap`/`expect`, per this crate's panic policy.
    Bitmap::try_new(raw, buffer.offset(), buffer.len())
        .unwrap_or_else(|_| Bitmap::new_unset(buffer.len()))
}

/// Converts a validity [`Bitmap`] to an arrow-rs [`NullBuffer`], without
/// copying the bits.
///
/// ```
/// use astrs_data::Bitmap;
/// use astrs_data::interop::to_arrow_nulls;
///
/// let validity: Bitmap = [true, false, true].into_iter().collect();
/// let nulls = to_arrow_nulls(&validity);
/// assert_eq!(nulls.null_count(), 1);
/// assert!(nulls.is_null(1));
/// ```
#[must_use]
pub fn to_arrow_nulls(bitmap: &Bitmap) -> NullBuffer {
    NullBuffer::new(to_arrow_boolean_buffer(bitmap))
}

/// Converts an arrow-rs null buffer to a validity [`Bitmap`], copying the
/// bits.
///
/// Returns `None` for both `None` and an all-valid [`NullBuffer`] (`arrow-rs`
/// can represent "no nulls" either way; a present-but-empty `NullBuffer`
/// reaching here at all is unusual — `arrow_data::ArrayDataBuilder::build`
/// itself drops one before it gets this far — but handled all the same), to
/// match this crate's own "[`crate::array::Array::validity`] returns `None`
/// when every slot is valid" convention.
///
/// ```
/// use arrow_buffer::{BooleanBuffer, NullBuffer};
/// use astrs_data::interop::from_arrow_nulls;
///
/// let nulls = NullBuffer::new(BooleanBuffer::from(vec![true, false, true]));
/// let validity = from_arrow_nulls(Some(&nulls)).unwrap();
/// assert_eq!(validity.count_unset(), 1);
///
/// assert!(from_arrow_nulls(None).is_none());
/// let all_valid = NullBuffer::new(BooleanBuffer::from(vec![true, true]));
/// assert!(from_arrow_nulls(Some(&all_valid)).is_none());
/// ```
#[must_use]
pub fn from_arrow_nulls(nulls: Option<&NullBuffer>) -> Option<Bitmap> {
    let nulls = nulls?;
    if nulls.null_count() == 0 {
        return None;
    }
    Some(from_arrow_boolean_buffer(nulls.inner()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn buffer_round_trip_is_zero_copy_forward() {
        let buffer = Buffer::from_slice(b"zero-copy-arrow");
        let arrow_buf = to_arrow_buffer(&buffer);
        assert_eq!(arrow_buf.as_slice(), buffer.as_slice());
        assert_eq!(
            arrow_buf.as_ptr(),
            buffer.as_ptr(),
            "forward conversion must not move bytes"
        );

        let back = from_arrow_buffer(&arrow_buf);
        assert_eq!(back.as_slice(), buffer.as_slice());
        assert_ne!(
            back.as_ptr(),
            buffer.as_ptr(),
            "reverse conversion always copies"
        );
    }

    #[test]
    fn buffer_zero_copy_survives_dropping_the_original() {
        let buffer = Buffer::from_slice(b"kept-alive-by-owner");
        let arrow_buf = to_arrow_buffer(&buffer);
        let ptr = arrow_buf.as_ptr();
        drop(buffer);
        // The `Arc<Buffer>` owner handed to `from_custom_allocation` keeps the
        // allocation alive independent of the original `Buffer` handle.
        assert_eq!(arrow_buf.as_ptr(), ptr);
        assert_eq!(arrow_buf.as_slice(), b"kept-alive-by-owner");
    }

    #[test]
    fn empty_buffer_round_trips() {
        let buffer = Buffer::new();
        let arrow_buf = to_arrow_buffer(&buffer);
        assert!(arrow_buf.is_empty());
        assert!(from_arrow_buffer(&arrow_buf).is_empty());
    }

    #[test]
    fn sliced_buffer_converts_the_exact_window() {
        let buffer = Buffer::from_slice(&(0u8..64).collect::<Vec<_>>());
        let window = buffer.slice(5, 10);
        let arrow_buf = to_arrow_buffer(&window);
        assert_eq!(arrow_buf.as_slice(), window.as_slice());
        assert_eq!(arrow_buf.as_ptr(), window.as_ptr());
    }

    #[test]
    fn boolean_buffer_round_trip_forward_is_zero_copy() {
        let bits: Bitmap = (0..37).map(|i| i % 3 == 0).collect();
        let arrow_bits = to_arrow_boolean_buffer(&bits);
        assert_eq!(arrow_bits.len(), bits.len());
        for i in 0..bits.len() {
            assert_eq!(arrow_bits.value(i), bits.value(i), "bit {i}");
        }
        assert_eq!(arrow_bits.inner().as_ptr(), bits.buffer().as_ptr());

        let back = from_arrow_boolean_buffer(&arrow_bits);
        assert_eq!(back, bits);
    }

    #[test]
    fn boolean_buffer_round_trip_respects_a_non_byte_aligned_offset() {
        let source: Vec<bool> = (0..40).map(|i| i % 5 == 0).collect();
        let bits: Bitmap = source.iter().copied().collect();
        let window = bits.slice(11, 21);
        assert_ne!(window.bit_offset() % 8, 0, "the interesting case");

        let arrow_bits = to_arrow_boolean_buffer(&window);
        assert_eq!(arrow_bits.len(), window.len());
        for i in 0..window.len() {
            assert_eq!(arrow_bits.value(i), window.value(i), "bit {i}");
        }

        let back = from_arrow_boolean_buffer(&arrow_bits);
        assert_eq!(back, window);
    }

    #[test]
    fn nulls_round_trip_and_none_means_all_valid() {
        let validity: Bitmap = [true, false, true, true].into_iter().collect();
        let nulls = to_arrow_nulls(&validity);
        assert_eq!(nulls.null_count(), 1);
        assert!(nulls.is_null(1));
        assert!(nulls.is_valid(0));

        let back = from_arrow_nulls(Some(&nulls)).unwrap();
        assert_eq!(back, validity);

        assert!(from_arrow_nulls(None).is_none());

        let all_valid: Bitmap = [true, true, true].into_iter().collect();
        let nulls = to_arrow_nulls(&all_valid);
        assert_eq!(nulls.null_count(), 0);
        assert!(
            from_arrow_nulls(Some(&nulls)).is_none(),
            "an all-valid NullBuffer round-trips to None, matching Array::validity's convention"
        );
    }

    #[test]
    fn allocation_bound_is_satisfied() {
        const fn assert_allocation<T: arrow_buffer::alloc::Allocation>() {}
        assert_allocation::<Buffer>();
        const fn assert_ref_unwind_safe<T: std::panic::RefUnwindSafe>() {}
        assert_ref_unwind_safe::<Buffer>();
    }
}
