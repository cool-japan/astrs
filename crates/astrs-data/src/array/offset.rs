//! [`OffsetSizeTrait`] — the `i32`/`i64` switch behind the `Binary`/
//! `LargeBinary` and `Utf8`/`LargeUtf8` pairs, plus shared offset validation.
//!
//! Arrow's variable-length layouts differ only in the width of their offset
//! buffer, so `astrs-data` writes each family once, generic over the offset
//! type, and exposes the two instantiations as aliases. The `Large` variants
//! exist because a 4 KiB × 1080p image column overflows `i32` offsets after
//! ~700 frames — a real limit for robotics payloads.
//!
//! ```
//! use astrs_data::array::{Array, BinaryArray, LargeBinaryArray, OffsetSizeTrait};
//!
//! assert!(!<i32 as OffsetSizeTrait>::IS_LARGE);
//! assert!(<i64 as OffsetSizeTrait>::IS_LARGE);
//! assert_eq!(BinaryArray::from_opt_iter([Some(&b"x"[..])]).len(), 1);
//! assert_eq!(LargeBinaryArray::from_opt_iter([Some(&b"x"[..])]).len(), 1);
//! ```

use crate::datatype::{ArrowNativeType, DataType};
use crate::error::{DataError, Result};

/// The offset width of a variable-length array: `i32` or `i64`.
///
/// Sealed through [`ArrowNativeType`], so the set stays exactly these two.
pub trait OffsetSizeTrait: ArrowNativeType + Ord {
    /// `true` for the 64-bit (`Large`) variants.
    const IS_LARGE: bool;

    /// The additive identity, the value every offset buffer starts at.
    const ZERO: Self;

    /// The `Binary`/`LargeBinary` type for this offset width.
    const BINARY_TYPE: DataType;

    /// The `Utf8`/`LargeUtf8` type for this offset width.
    const UTF8_TYPE: DataType;

    /// Converts to `usize`, or `None` when negative or too large for the host.
    fn to_usize(self) -> Option<usize>;

    /// Converts from `usize`, or `None` when the value does not fit.
    fn from_usize(value: usize) -> Option<Self>;

    /// Widens to `i64` for uniform error reporting.
    fn as_i64(self) -> i64;
}

impl OffsetSizeTrait for i32 {
    const IS_LARGE: bool = false;
    const ZERO: Self = 0;
    const BINARY_TYPE: DataType = DataType::Binary;
    const UTF8_TYPE: DataType = DataType::Utf8;

    #[inline]
    fn to_usize(self) -> Option<usize> {
        usize::try_from(self).ok()
    }

    #[inline]
    fn from_usize(value: usize) -> Option<Self> {
        Self::try_from(value).ok()
    }

    #[inline]
    fn as_i64(self) -> i64 {
        i64::from(self)
    }
}

impl OffsetSizeTrait for i64 {
    const IS_LARGE: bool = true;
    const ZERO: Self = 0;
    const BINARY_TYPE: DataType = DataType::LargeBinary;
    const UTF8_TYPE: DataType = DataType::LargeUtf8;

    #[inline]
    fn to_usize(self) -> Option<usize> {
        usize::try_from(self).ok()
    }

    #[inline]
    fn from_usize(value: usize) -> Option<Self> {
        Self::try_from(value).ok()
    }

    #[inline]
    fn as_i64(self) -> i64 {
        self
    }
}

/// Validates an offset buffer against a value region.
///
/// This is the check that makes every `value(i)` accessor *total*: after it
/// passes, `offsets[i] .. offsets[i + 1]` is guaranteed to be a valid range
/// inside a `values_len`-byte region for every `i < len`. Clippy's `panic`
/// lint cannot see slice-indexing panics, so this validation is the only thing
/// standing between a malformed payload and a runtime abort.
///
/// # Errors
///
/// * [`DataError::OffsetBufferLength`] — not exactly `len + 1` entries.
/// * [`DataError::NegativeOffset`] — an entry below zero.
/// * [`DataError::NonMonotonicOffsets`] — an entry below its predecessor.
/// * [`DataError::OffsetOutOfBounds`] — the last entry past `values_len`.
///
/// ```
/// use astrs_data::array::validate_offsets;
///
/// assert!(validate_offsets(&[0i32, 2, 5], 5).is_ok());
/// assert!(validate_offsets(&[0i32, 2, 5], 4).is_err());
/// assert!(validate_offsets(&[0i32, 5, 2], 5).is_err());
/// assert!(validate_offsets(&[] as &[i32], 0).is_err(), "needs at least one entry");
/// ```
pub fn validate_offsets<O: OffsetSizeTrait>(offsets: &[O], values_len: usize) -> Result<usize> {
    let Some((&first, rest)) = offsets.split_first() else {
        return Err(DataError::OffsetBufferLength {
            expected: 1,
            actual: 0,
        });
    };
    if first < O::ZERO {
        return Err(DataError::NegativeOffset {
            index: 0,
            offset: first.as_i64(),
        });
    }
    let mut previous = first;
    for (position, &offset) in rest.iter().enumerate() {
        let index = position + 1;
        if offset < O::ZERO {
            return Err(DataError::NegativeOffset {
                index,
                offset: offset.as_i64(),
            });
        }
        if offset < previous {
            return Err(DataError::NonMonotonicOffsets {
                index,
                offset: offset.as_i64(),
            });
        }
        previous = offset;
    }
    let last = previous.to_usize().ok_or(DataError::OffsetOutOfBounds {
        index: offsets.len() - 1,
        offset: usize::MAX,
        values_len,
    })?;
    if last > values_len {
        return Err(DataError::OffsetOutOfBounds {
            index: offsets.len() - 1,
            offset: last,
            values_len,
        });
    }
    Ok(offsets.len() - 1)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn offset_width_constants() {
        const { assert!(!<i32 as OffsetSizeTrait>::IS_LARGE) };
        const { assert!(<i64 as OffsetSizeTrait>::IS_LARGE) };
        assert_eq!(<i32 as OffsetSizeTrait>::BINARY_TYPE, DataType::Binary);
        assert_eq!(<i64 as OffsetSizeTrait>::BINARY_TYPE, DataType::LargeBinary);
        assert_eq!(<i32 as OffsetSizeTrait>::UTF8_TYPE, DataType::Utf8);
        assert_eq!(<i64 as OffsetSizeTrait>::UTF8_TYPE, DataType::LargeUtf8);
    }

    #[test]
    fn conversions_are_range_checked() {
        assert_eq!(OffsetSizeTrait::to_usize(5i32), Some(5));
        assert_eq!(OffsetSizeTrait::to_usize(-1i32), None);
        assert_eq!(<i32 as OffsetSizeTrait>::from_usize(5), Some(5));
        assert_eq!(<i32 as OffsetSizeTrait>::from_usize(usize::MAX), None);
        assert_eq!(<i64 as OffsetSizeTrait>::from_usize(1 << 40), Some(1 << 40));
        assert_eq!(OffsetSizeTrait::as_i64(-3i32), -3);
        assert_eq!(OffsetSizeTrait::as_i64(-3i64), -3);
    }

    #[test]
    fn valid_offsets_report_the_array_length() {
        assert_eq!(validate_offsets(&[0i32], 0).unwrap(), 0);
        assert_eq!(validate_offsets(&[0i32, 0, 0], 0).unwrap(), 2);
        assert_eq!(validate_offsets(&[0i32, 3, 3, 7], 7).unwrap(), 3);
        assert_eq!(validate_offsets(&[0i64, 4], 100).unwrap(), 1);
        // Offsets need not start at zero: a sliced array keeps its window.
        assert_eq!(validate_offsets(&[5i32, 9], 9).unwrap(), 1);
    }

    #[test]
    fn empty_offset_buffer_is_rejected() {
        assert_eq!(
            validate_offsets(&[] as &[i32], 0).unwrap_err(),
            DataError::OffsetBufferLength {
                expected: 1,
                actual: 0
            }
        );
    }

    #[test]
    fn negative_offsets_are_rejected() {
        assert_eq!(
            validate_offsets(&[-1i32, 0], 0).unwrap_err(),
            DataError::NegativeOffset {
                index: 0,
                offset: -1
            }
        );
        assert_eq!(
            validate_offsets(&[0i32, -1], 0).unwrap_err(),
            DataError::NegativeOffset {
                index: 1,
                offset: -1
            }
        );
    }

    #[test]
    fn non_monotonic_offsets_are_rejected() {
        assert_eq!(
            validate_offsets(&[0i32, 5, 2], 5).unwrap_err(),
            DataError::NonMonotonicOffsets {
                index: 2,
                offset: 2
            }
        );
        assert!(validate_offsets(&[0i32, 5, 5], 5).is_ok(), "equal is fine");
    }

    #[test]
    fn offsets_past_the_value_region_are_rejected() {
        assert_eq!(
            validate_offsets(&[0i32, 9], 4).unwrap_err(),
            DataError::OffsetOutOfBounds {
                index: 1,
                offset: 9,
                values_len: 4
            }
        );
        assert!(
            validate_offsets(&[0i32, 4], 4).is_ok(),
            "exactly at the end"
        );
    }
}
