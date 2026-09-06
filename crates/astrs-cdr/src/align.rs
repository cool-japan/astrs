//! Alignment arithmetic, relative to the encapsulation origin.
//!
//! OMG CDR (CORBA 3.0 §15.3.1) requires every primitive to start at a stream
//! offset that is a multiple of its size, so that a receiver can read it with
//! an aligned load. The offset is counted from the **alignment origin**, not
//! from the start of the buffer:
//!
//! ```text
//! buffer:  [ 00 01 00 00 ][ member bytes ... ]
//!            ^encapsulation header
//!                          ^origin — stream position 0
//! ```
//!
//! Alignments in CDR are always powers of two in `1..=8`, so the padding is
//! a mask operation. The functions here are `const` and total: they never
//! panic, and an alignment of zero is treated as "no alignment required".
//!
//! # Natural alignments
//!
//! Reproduced from OMG CDR 15.3.1 (XCDR1) with the XCDR2 cap of OMG
//! DDS-XTypes 1.3 §7.4.3.4.1 applied by [`crate::Encoding::align_for`]:
//!
//! | IDL type | Octets | XCDR1 alignment | XCDR2 alignment |
//! |---|---:|---:|---:|
//! | `boolean`, `octet`, `char`, `int8`, `uint8` | 1 | 1 | 1 |
//! | `short`, `unsigned short`, `int16`, `uint16`, `wchar` | 2 | 2 | 2 |
//! | `long`, `unsigned long`, `float`, `enum` | 4 | 4 | 4 |
//! | `long long`, `unsigned long long`, `double` | 8 | 8 | **4** |

/// Alignment of a CDR `boolean`, `octet`, `char`, `int8` and `uint8`.
pub const ALIGN_1: usize = 1;
/// Alignment of a CDR `short`, `unsigned short` and `wchar`.
pub const ALIGN_2: usize = 2;
/// Alignment of a CDR `long`, `unsigned long`, `float` and `enum`, and of
/// every length prefix (sequence lengths, string lengths, DHEADER, EMHEADER).
pub const ALIGN_4: usize = 4;
/// Alignment of a CDR `long long`, `unsigned long long` and `double` under
/// XCDR1. XCDR2 caps this at [`ALIGN_4`].
pub const ALIGN_8: usize = 8;

/// Number of padding octets needed to bring `position` up to a multiple of
/// `alignment`.
///
/// `position` is a stream position — an offset from the alignment origin, not
/// a buffer index. An `alignment` of 0 or 1 always needs no padding.
///
/// ```
/// use astrs_cdr::align::padding_to;
///
/// // A `double` at stream position 1 needs 7 pad octets under XCDR1.
/// assert_eq!(padding_to(1, 8), 7);
/// // …and 3 under XCDR2, where the alignment is capped at 4.
/// assert_eq!(padding_to(1, 4), 3);
/// // Already-aligned positions need nothing.
/// assert_eq!(padding_to(8, 8), 0);
/// ```
#[must_use]
pub const fn padding_to(position: usize, alignment: usize) -> usize {
    if alignment <= 1 {
        return 0;
    }
    // Every CDR alignment is a power of two, so `alignment - 1` is the mask
    // of the bits that must be zero.
    let mask = alignment - 1;
    (alignment - (position & mask)) & mask
}

/// `position` rounded up to the next multiple of `alignment`, or `None` on
/// overflow.
///
/// ```
/// use astrs_cdr::align::align_up;
///
/// assert_eq!(align_up(5, 4), Some(8));
/// assert_eq!(align_up(8, 4), Some(8));
/// assert_eq!(align_up(usize::MAX, 8), None);
/// ```
#[must_use]
pub const fn align_up(position: usize, alignment: usize) -> Option<usize> {
    let pad = padding_to(position, alignment);
    position.checked_add(pad)
}

/// True when `position` already sits on an `alignment` boundary.
#[must_use]
pub const fn is_aligned(position: usize, alignment: usize) -> bool {
    padding_to(position, alignment) == 0
}

/// The natural alignment of a CDR primitive of `size` octets, before any
/// version cap.
///
/// CDR gives every primitive an alignment equal to its size, which makes this
/// the identity for the sizes CDR actually uses (1, 2, 4, 8). Anything else
/// is clamped into that set so a caller cannot smuggle in a `3`.
#[must_use]
pub const fn natural_alignment(size: usize) -> usize {
    match size {
        0 | 1 => ALIGN_1,
        2 => ALIGN_2,
        3 | 4 => ALIGN_4,
        _ => ALIGN_8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_walks_a_full_cycle_for_each_alignment() {
        for alignment in [ALIGN_1, ALIGN_2, ALIGN_4, ALIGN_8] {
            for position in 0..(alignment * 3) {
                let pad = padding_to(position, alignment);
                assert!(pad < alignment, "pad {pad} >= alignment {alignment}");
                assert_eq!(
                    (position + pad) % alignment,
                    0,
                    "position {position} + pad {pad} not aligned to {alignment}"
                );
            }
        }
    }

    #[test]
    fn padding_matches_the_hand_worked_table() {
        // Offsets 0..8 against an 8-octet alignment — the XCDR1 `double` case.
        let expected = [0_usize, 7, 6, 5, 4, 3, 2, 1, 0];
        for (position, want) in expected.into_iter().enumerate() {
            assert_eq!(padding_to(position, ALIGN_8), want, "position {position}");
        }
        // …and against 4, the XCDR2 cap for the same member.
        let expected4 = [0_usize, 3, 2, 1, 0, 3, 2, 1, 0];
        for (position, want) in expected4.into_iter().enumerate() {
            assert_eq!(padding_to(position, ALIGN_4), want, "position {position}");
        }
    }

    #[test]
    fn alignment_of_one_or_zero_never_pads() {
        for position in 0..16 {
            assert_eq!(padding_to(position, 0), 0);
            assert_eq!(padding_to(position, 1), 0);
        }
    }

    #[test]
    fn align_up_rounds_and_detects_overflow() {
        assert_eq!(align_up(0, 8), Some(0));
        assert_eq!(align_up(1, 8), Some(8));
        assert_eq!(align_up(9, 4), Some(12));
        assert_eq!(align_up(usize::MAX, 1), Some(usize::MAX));
        assert_eq!(align_up(usize::MAX, 2), None);
        assert_eq!(align_up(usize::MAX - 2, 8), None);
    }

    #[test]
    fn is_aligned_agrees_with_padding() {
        for alignment in [ALIGN_1, ALIGN_2, ALIGN_4, ALIGN_8] {
            for position in 0..32 {
                assert_eq!(
                    is_aligned(position, alignment),
                    padding_to(position, alignment) == 0
                );
            }
        }
    }

    #[test]
    fn natural_alignment_clamps_into_the_cdr_set() {
        assert_eq!(natural_alignment(0), 1);
        assert_eq!(natural_alignment(1), 1);
        assert_eq!(natural_alignment(2), 2);
        assert_eq!(natural_alignment(3), 4);
        assert_eq!(natural_alignment(4), 4);
        assert_eq!(natural_alignment(8), 8);
        assert_eq!(natural_alignment(16), 8);
    }
}
