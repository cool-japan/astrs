//! Shared, null-hardened conversions from a raw `(ptr, len)` pair to a Rust
//! slice or `&str`.
//!
//! Every `extern "C" fn` in this crate that reads caller-supplied bytes goes
//! through exactly one of these, so the `(NULL, 0)` idiom for "empty" and the
//! UTF-8 validation for a text argument are each written — and tested —
//! once.

use std::slice;

use crate::status::AstrsStatus;

/// Resolves a C `(ptr, len)` byte pair into a slice, accepting the standard
/// `(NULL, 0)` idiom for "empty" that this crate uses throughout for an
/// optional or genuinely zero-length argument (a payload, a type URN).
///
/// `slice::from_raw_parts` is undefined behaviour on a null pointer even for
/// length `0` — the pointer must be non-null and well-aligned regardless of
/// length — so a zero-length argument must never dereference `ptr` at all.
///
/// # Safety
///
/// When `len > 0`, `ptr` must point to `len` initialized, readable bytes,
/// valid for the duration of the returned borrow, and not mutated for as
/// long as the borrow is live.
pub(crate) unsafe fn bytes_or_empty<'a>(
    ptr: *const u8,
    len: usize,
) -> Result<&'a [u8], AstrsStatus> {
    if len == 0 {
        Ok(&[])
    } else if ptr.is_null() {
        Err(AstrsStatus::InvalidArgument)
    } else {
        Ok(unsafe { slice::from_raw_parts(ptr, len) })
    }
}

/// As [`bytes_or_empty`], then validated as UTF-8.
///
/// # Safety
///
/// As [`bytes_or_empty`].
pub(crate) unsafe fn str_or_empty<'a>(ptr: *const u8, len: usize) -> Result<&'a str, AstrsStatus> {
    let bytes = unsafe { bytes_or_empty(ptr, len) }?;
    std::str::from_utf8(bytes).map_err(|_| AstrsStatus::InvalidArgument)
}

/// A required (non-empty) `(ptr, len)` string argument — an output id, an
/// input id — where an empty or null argument is never meaningful and is
/// therefore reported as [`AstrsStatus::InvalidArgument`] rather than treated
/// as "empty".
///
/// # Safety
///
/// As [`bytes_or_empty`].
pub(crate) unsafe fn required_str<'a>(ptr: *const u8, len: usize) -> Result<&'a str, AstrsStatus> {
    if len == 0 || ptr.is_null() {
        return Err(AstrsStatus::InvalidArgument);
    }
    unsafe { str_or_empty(ptr, len) }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn bytes_or_empty_accepts_the_null_zero_idiom() {
        let data = unsafe { bytes_or_empty(std::ptr::null(), 0) }.unwrap();
        assert!(data.is_empty());
    }

    #[test]
    fn bytes_or_empty_rejects_null_with_a_nonzero_length() {
        let error = unsafe { bytes_or_empty(std::ptr::null(), 4) }.unwrap_err();
        assert_eq!(error, AstrsStatus::InvalidArgument);
    }

    #[test]
    fn bytes_or_empty_reads_a_valid_pointer() {
        let buf = [1u8, 2, 3, 4];
        let data = unsafe { bytes_or_empty(buf.as_ptr(), buf.len()) }.unwrap();
        assert_eq!(data, &buf);
    }

    #[test]
    fn str_or_empty_accepts_the_null_zero_idiom() {
        let text = unsafe { str_or_empty(std::ptr::null(), 0) }.unwrap();
        assert_eq!(text, "");
    }

    #[test]
    fn str_or_empty_rejects_invalid_utf8() {
        let bytes = [0xFFu8, 0xFE];
        let error = unsafe { str_or_empty(bytes.as_ptr(), bytes.len()) }.unwrap_err();
        assert_eq!(error, AstrsStatus::InvalidArgument);
    }

    #[test]
    fn str_or_empty_reads_valid_utf8() {
        let text = "hello";
        let read = unsafe { str_or_empty(text.as_ptr(), text.len()) }.unwrap();
        assert_eq!(read, "hello");
    }

    #[test]
    fn required_str_rejects_a_zero_length() {
        let text = "x";
        let error = unsafe { required_str(text.as_ptr(), 0) }.unwrap_err();
        assert_eq!(error, AstrsStatus::InvalidArgument);
        let error = unsafe { required_str(std::ptr::null(), 0) }.unwrap_err();
        assert_eq!(error, AstrsStatus::InvalidArgument);
    }

    #[test]
    fn required_str_accepts_a_real_string() {
        let text = "frames";
        let read = unsafe { required_str(text.as_ptr(), text.len()) }.unwrap();
        assert_eq!(read, "frames");
    }
}
