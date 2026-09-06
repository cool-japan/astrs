//! Sequence numbers and the bitmap set that names a window of them.
//!
//! A [`SequenceNumber`] counts the samples a writer has produced for a topic.
//! It is a 64-bit value transmitted as a **signed** high half and an
//! **unsigned** low half (OMG DDSI-RTPS 2.3 §9.4.2.5):
//!
//! ```text
//! +--------+--------+--------+--------+
//! |        high (long, signed)        |
//! +--------+--------+--------+--------+
//! |     low (unsigned long)           |
//! +--------+--------+--------+--------+
//! ```
//!
//! Both halves are subject to the submessage's byte order; only the *split*
//! is fixed. That split is why `SEQUENCENUMBER_UNKNOWN` — `high = -1`,
//! `low = 0` — is the negative value `-4294967296` rather than a sentinel
//! bit pattern.
//!
//! A [`SequenceNumberSet`] names a window of at most 256 sequence numbers
//! starting at a base (§9.4.2.6): the reader's "these are the ones I am
//! missing" in an `ACKNACK`, the writer's "these no longer exist" in a `GAP`.
//!
//! ```
//! use astrs_rtps::structure::{SequenceNumber, SequenceNumberSet};
//!
//! let mut missing = SequenceNumberSet::new(SequenceNumber::new(10));
//! missing.insert(SequenceNumber::new(10))?;
//! missing.insert(SequenceNumber::new(12))?;
//! assert!(missing.contains(SequenceNumber::new(12)));
//! assert!(!missing.contains(SequenceNumber::new(11)));
//! assert_eq!(missing.len(), 2);
//! // base(8) + numBits(4) + one bitmap word(4).
//! assert_eq!(missing.serialized_len(), 16);
//! # Ok::<(), astrs_rtps::RtpsError>(())
//! ```

use core::fmt;

use astrs_cdr::{CdrDeserialize, CdrReader, CdrResult, CdrSerialize, CdrType, CdrWriter};

use crate::error::{RtpsError, RtpsResult, SetDefect};

/// Octets a [`SequenceNumber`] occupies on the wire.
pub const SEQUENCE_NUMBER_LEN: usize = 8;

/// The largest number of bits a [`SequenceNumberSet`] may declare
/// (§9.4.2.6).
pub const MAX_SET_BITS: u32 = 256;

/// Thirty-two-bit words a full 256-bit bitmap occupies.
pub const MAX_SET_WORDS: usize = (MAX_SET_BITS as usize) / 32;

/// Octets the fixed part of a set occupies: `bitmapBase` plus `numBits`.
pub const SET_PREFIX_LEN: usize = SEQUENCE_NUMBER_LEN + 4;

/// A writer's per-topic sample counter (§9.4.2.5).
///
/// Sequence numbers start at one; zero and negative values only ever appear
/// as sentinels, which is what [`SequenceNumber::is_valid`] tests for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SequenceNumber(i64);

impl SequenceNumber {
    /// `SEQUENCENUMBER_UNKNOWN` — `high = -1`, `low = 0`.
    ///
    /// A `DATA` never carries it; a `HEARTBEAT` may, to say "I have nothing".
    pub const UNKNOWN: Self = Self(-(1_i64 << 32));

    /// The number before the first: `0`, the value a writer's counter holds
    /// before it has written anything.
    pub const ZERO: Self = Self(0);

    /// The first sequence number a writer may use.
    pub const FIRST: Self = Self(1);

    /// The largest representable number.
    pub const MAX: Self = Self(i64::MAX);

    /// Wrap a 64-bit value.
    #[must_use]
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    /// Rebuild a number from its two wire halves.
    #[must_use]
    pub const fn from_halves(high: i32, low: u32) -> Self {
        Self(((high as i64) << 32) | (low as i64))
    }

    /// The signed high half, as transmitted.
    #[must_use]
    pub const fn high(self) -> i32 {
        (self.0 >> 32) as i32
    }

    /// The unsigned low half, as transmitted.
    #[must_use]
    pub const fn low(self) -> u32 {
        self.0 as u32
    }

    /// The 64-bit value.
    #[must_use]
    pub const fn value(self) -> i64 {
        self.0
    }

    /// True for [`SequenceNumber::UNKNOWN`].
    #[must_use]
    pub const fn is_unknown(self) -> bool {
        self.0 == Self::UNKNOWN.0
    }

    /// True for a number a writer may actually have used: strictly positive.
    ///
    /// This is the predicate behind the §8.3.7 validity clauses that say
    /// "`writerSN.value > 0`".
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.0 > 0
    }

    /// The next number, saturating at [`SequenceNumber::MAX`].
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// The previous number, saturating at `i64::MIN`.
    #[must_use]
    pub const fn previous(self) -> Self {
        Self(self.0.saturating_sub(1))
    }

    /// This number advanced by `offset`, saturating at the ends.
    #[must_use]
    pub const fn saturating_add(self, offset: i64) -> Self {
        Self(self.0.saturating_add(offset))
    }

    /// How far `self` is above `base`, or `None` when it is below.
    ///
    /// The arithmetic is done in `i128` so a difference between the extremes
    /// cannot overflow.
    #[must_use]
    pub const fn offset_from(self, base: Self) -> Option<u64> {
        let difference = (self.0 as i128) - (base.0 as i128);
        if difference < 0 {
            return None;
        }
        if difference > u64::MAX as i128 {
            return None;
        }
        Some(difference as u64)
    }

    /// Reject a number a validity clause forbids.
    ///
    /// # Errors
    ///
    /// [`RtpsError::InvalidSequenceNumber`] when the value is not strictly
    /// positive. `context` names the field for the log line.
    pub const fn check_valid(self, context: &'static str) -> RtpsResult<Self> {
        if self.is_valid() {
            Ok(self)
        } else {
            Err(RtpsError::InvalidSequenceNumber {
                value: self.0,
                context,
            })
        }
    }
}

impl Default for SequenceNumber {
    /// [`SequenceNumber::ZERO`].
    fn default() -> Self {
        Self::ZERO
    }
}

impl fmt::Display for SequenceNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_unknown() {
            return f.write_str("SEQUENCENUMBER_UNKNOWN");
        }
        write!(f, "{}", self.0)
    }
}

impl From<i64> for SequenceNumber {
    fn from(value: i64) -> Self {
        Self(value)
    }
}

impl From<SequenceNumber> for i64 {
    fn from(number: SequenceNumber) -> Self {
        number.0
    }
}

impl CdrType for SequenceNumber {
    const MIN_SERIALIZED_SIZE: usize = SEQUENCE_NUMBER_LEN;
}

impl CdrSerialize for SequenceNumber {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_i32(self.high())?;
        writer.write_u32(self.low())
    }
}

impl<'de> CdrDeserialize<'de> for SequenceNumber {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        let high = reader.read_i32()?;
        let low = reader.read_u32()?;
        Ok(Self::from_halves(high, low))
    }
}

/// A window of at most 256 sequence numbers, as a bitmap (§9.4.2.6).
///
/// # Wire format
///
/// ```text
/// +--------+--------+--------+--------+
/// ~        bitmapBase (8 octets)      ~
/// +--------+--------+--------+--------+
/// |        numBits (unsigned long)    |
/// +--------+--------+--------+--------+
/// ~   bitmap[M] (M = ceil(numBits/32))~
/// +--------+--------+--------+--------+
/// ```
///
/// Bit *i* of the set stands for `bitmapBase + i`. Within a word the bits run
/// from the most significant down, so bit 0 is `bitmap[0] & 0x8000_0000` —
/// the ordering that makes a hex dump of the bitmap read left to right in
/// sequence-number order.
///
/// `numBits == 0` is legal and common: an `ACKNACK` with an empty set says
/// "everything below `bitmapBase` has arrived, and I am missing nothing".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SequenceNumberSet {
    base: SequenceNumber,
    num_bits: u32,
    bitmap: [u32; MAX_SET_WORDS],
}

impl SequenceNumberSet {
    /// An empty set based at `base`.
    #[must_use]
    pub const fn new(base: SequenceNumber) -> Self {
        Self {
            base,
            num_bits: 0,
            bitmap: [0; MAX_SET_WORDS],
        }
    }

    /// The base of the window.
    #[must_use]
    pub const fn base(self) -> SequenceNumber {
        self.base
    }

    /// The declared width of the window, in bits.
    ///
    /// This is *not* the number of bits that are set — see
    /// [`SequenceNumberSet::len`]. It is the width the sender transmitted, and
    /// it is preserved so that decode and re-encode reproduce the peer's
    /// octets exactly.
    #[must_use]
    pub const fn num_bits(self) -> u32 {
        self.num_bits
    }

    /// The bitmap words, of which only [`SequenceNumberSet::num_words`] are
    /// transmitted.
    #[must_use]
    pub const fn words(&self) -> &[u32; MAX_SET_WORDS] {
        &self.bitmap
    }

    /// Words the wire form carries: `ceil(numBits / 32)`.
    #[must_use]
    pub const fn num_words(self) -> usize {
        self.num_bits.div_ceil(32) as usize
    }

    /// Octets the wire form occupies.
    #[must_use]
    pub const fn serialized_len(self) -> usize {
        SET_PREFIX_LEN + 4 * self.num_words()
    }

    /// True when no sequence number is in the set.
    ///
    /// A set with a nonzero `numBits` whose every bit is clear is still
    /// empty in this sense.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.bitmap.iter().all(|word| *word == 0)
    }

    /// How many sequence numbers are in the set.
    #[must_use]
    pub fn len(self) -> usize {
        self.bitmap
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }

    /// True when `number` is in the set.
    #[must_use]
    pub fn contains(self, number: SequenceNumber) -> bool {
        let Some(bit) = number.offset_from(self.base) else {
            return false;
        };
        if bit >= u64::from(self.num_bits) {
            return false;
        }
        // `bit < num_bits <= 256`, so the index is in range.
        let (word, mask) = split_bit(bit as u32);
        self.bitmap.get(word).is_some_and(|value| value & mask != 0)
    }

    /// Add `number` to the set, widening the window to reach it.
    ///
    /// # Errors
    ///
    /// [`RtpsError::InvalidSequenceNumberSet`] with
    /// [`SetDefect::BitOutOfRange`] when `number` is below the base or 256 or
    /// more above it. Widening the window is the caller's decision, so
    /// rebasing is never done implicitly.
    pub fn insert(&mut self, number: SequenceNumber) -> RtpsResult<()> {
        let bit = number
            .offset_from(self.base)
            .filter(|offset| *offset < u64::from(MAX_SET_BITS))
            .ok_or(RtpsError::InvalidSequenceNumberSet {
                reason: SetDefect::BitOutOfRange {
                    offset: i128::from(number.value()) - i128::from(self.base.value()),
                },
                context: "SequenceNumberSet::insert",
            })?;
        let bit = bit as u32;
        let (word, mask) = split_bit(bit);
        if let Some(slot) = self.bitmap.get_mut(word) {
            *slot |= mask;
        }
        if bit >= self.num_bits {
            self.num_bits = bit + 1;
        }
        Ok(())
    }

    /// Build a set that names every number in `numbers`.
    ///
    /// # Errors
    ///
    /// Those of [`SequenceNumberSet::insert`].
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_rtps::structure::{SequenceNumber, SequenceNumberSet};
    ///
    /// let set = SequenceNumberSet::from_numbers(
    ///     SequenceNumber::new(4),
    ///     [SequenceNumber::new(4), SequenceNumber::new(9)],
    /// )?;
    /// assert_eq!(set.num_bits(), 6);
    /// assert_eq!(set.len(), 2);
    /// # Ok::<(), astrs_rtps::RtpsError>(())
    /// ```
    pub fn from_numbers(
        base: SequenceNumber,
        numbers: impl IntoIterator<Item = SequenceNumber>,
    ) -> RtpsResult<Self> {
        let mut set = Self::new(base);
        for number in numbers {
            set.insert(number)?;
        }
        Ok(set)
    }

    /// Force the declared window width, keeping the bits already set.
    ///
    /// A reader that must acknowledge a contiguous run without listing it
    /// bit by bit uses this. Bits beyond the new width are cleared, so the
    /// invariant "no bit is set at or above `numBits`" always holds.
    ///
    /// # Errors
    ///
    /// [`RtpsError::InvalidSequenceNumberSet`] when `num_bits` exceeds
    /// [`MAX_SET_BITS`].
    pub fn set_num_bits(&mut self, num_bits: u32) -> RtpsResult<()> {
        if num_bits > MAX_SET_BITS {
            return Err(RtpsError::InvalidSequenceNumberSet {
                reason: SetDefect::NumBitsTooLarge { declared: num_bits },
                context: "SequenceNumberSet::set_num_bits",
            });
        }
        self.num_bits = num_bits;
        self.clear_beyond_window();
        Ok(())
    }

    /// Iterate over the sequence numbers in the set, ascending.
    #[must_use]
    pub const fn iter(&self) -> SequenceNumberSetIter<'_> {
        SequenceNumberSetIter { set: self, bit: 0 }
    }

    /// The smallest number in the set.
    #[must_use]
    pub fn first(self) -> Option<SequenceNumber> {
        self.iter().next()
    }

    /// Check the clauses §8.3.7.1.3 and §8.3.7.4.3 state for a set that
    /// appears in an `ACKNACK` or a `GAP`.
    ///
    /// # Errors
    ///
    /// [`RtpsError::InvalidSequenceNumberSet`] when the base is not strictly
    /// positive or the window is wider than [`MAX_SET_BITS`].
    pub const fn validate(self, context: &'static str) -> RtpsResult<()> {
        if !self.base.is_valid() {
            return Err(RtpsError::InvalidSequenceNumberSet {
                reason: SetDefect::BaseNotPositive,
                context,
            });
        }
        if self.num_bits > MAX_SET_BITS {
            return Err(RtpsError::InvalidSequenceNumberSet {
                reason: SetDefect::NumBitsTooLarge {
                    declared: self.num_bits,
                },
                context,
            });
        }
        Ok(())
    }

    /// Read a set from a submessage body.
    ///
    /// # Errors
    ///
    /// - [`RtpsError::InvalidSequenceNumberSet`] when `numBits` exceeds 256.
    ///   The check happens *before* the bitmap words are read, so a hostile
    ///   `numBits` cannot drive a long loop.
    /// - [`RtpsError::Cdr`] when the body ends inside the set.
    pub fn read(reader: &mut CdrReader<'_>, context: &'static str) -> RtpsResult<Self> {
        let base = SequenceNumber::deserialize(reader)?;
        let num_bits = reader.read_u32()?;
        if num_bits > MAX_SET_BITS {
            return Err(RtpsError::InvalidSequenceNumberSet {
                reason: SetDefect::NumBitsTooLarge { declared: num_bits },
                context,
            });
        }
        let mut bitmap = [0_u32; MAX_SET_WORDS];
        let words = num_bits.div_ceil(32) as usize;
        for slot in bitmap.iter_mut().take(words) {
            *slot = reader.read_u32()?;
        }
        let mut set = Self {
            base,
            num_bits,
            bitmap,
        };
        // A sender may leave bits set past `numBits` in the last word; they
        // are not part of the set, and clearing them keeps `contains` and
        // `len` in agreement.
        set.clear_beyond_window();
        Ok(set)
    }

    /// Write a set into a submessage body.
    ///
    /// # Errors
    ///
    /// [`RtpsError::Cdr`] only — every field is fixed-size.
    pub fn write(&self, writer: &mut CdrWriter) -> RtpsResult<()> {
        self.base.serialize(writer)?;
        writer.write_u32(self.num_bits)?;
        for word in self.bitmap.iter().take(self.num_words()) {
            writer.write_u32(*word)?;
        }
        Ok(())
    }

    fn clear_beyond_window(&mut self) {
        let bits = self.num_bits as usize;
        for (index, word) in self.bitmap.iter_mut().enumerate() {
            let first_bit = index * 32;
            if first_bit >= bits {
                *word = 0;
            } else if first_bit + 32 > bits {
                let keep = bits - first_bit;
                // `keep` is 1..=31 here, so the shift is well defined.
                *word &= !(u32::MAX >> keep);
            }
        }
    }
}

impl Default for SequenceNumberSet {
    /// An empty set based at [`SequenceNumber::FIRST`], the only base that
    /// passes [`SequenceNumberSet::validate`] with no samples known.
    fn default() -> Self {
        Self::new(SequenceNumber::FIRST)
    }
}

impl fmt::Display for SequenceNumberSet {
    /// `base+{a, b, c}/numBits`, the form the receive-path logs use.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}+{{", self.base)?;
        for (index, number) in self.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{number}")?;
        }
        write!(f, "}}/{}", self.num_bits)
    }
}

impl<'a> IntoIterator for &'a SequenceNumberSet {
    type Item = SequenceNumber;
    type IntoIter = SequenceNumberSetIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Iterator over the sequence numbers a [`SequenceNumberSet`] names.
#[derive(Debug, Clone)]
pub struct SequenceNumberSetIter<'a> {
    set: &'a SequenceNumberSet,
    bit: u32,
}

impl Iterator for SequenceNumberSetIter<'_> {
    type Item = SequenceNumber;

    fn next(&mut self) -> Option<Self::Item> {
        while self.bit < self.set.num_bits {
            let bit = self.bit;
            self.bit += 1;
            let (word, mask) = split_bit(bit);
            if self.set.bitmap.get(word).is_some_and(|w| w & mask != 0) {
                return Some(self.set.base.saturating_add(i64::from(bit)));
            }
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = (self.set.num_bits - self.bit) as usize;
        (0, Some(remaining))
    }
}

/// The `(word index, mask)` pair for bit `bit` of a bitmap.
///
/// Bit 0 is the most significant bit of word 0 (§9.4.2.6).
const fn split_bit(bit: u32) -> (usize, u32) {
    ((bit / 32) as usize, 1_u32 << (31 - (bit % 32)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use astrs_cdr::{Encoding, to_vec_headerless};

    use super::*;

    #[test]
    fn a_sequence_number_splits_into_a_signed_high_and_unsigned_low() {
        let number = SequenceNumber::new(0x0000_0007_dead_beef);
        assert_eq!(number.high(), 7);
        assert_eq!(number.low(), 0xdead_beef);
        assert_eq!(SequenceNumber::from_halves(7, 0xdead_beef), number);
        assert_eq!(number.value(), 0x0000_0007_dead_beef);
        assert_eq!(i64::from(number), 0x0000_0007_dead_beef);
        assert_eq!(SequenceNumber::from(9_i64), SequenceNumber::new(9));
    }

    #[test]
    fn the_unknown_sentinel_is_high_minus_one_low_zero() {
        assert_eq!(SequenceNumber::UNKNOWN.high(), -1);
        assert_eq!(SequenceNumber::UNKNOWN.low(), 0);
        assert_eq!(SequenceNumber::UNKNOWN.value(), -4_294_967_296);
        assert!(SequenceNumber::UNKNOWN.is_unknown());
        assert!(!SequenceNumber::UNKNOWN.is_valid());
        assert_eq!(
            SequenceNumber::UNKNOWN.to_string(),
            "SEQUENCENUMBER_UNKNOWN"
        );
    }

    #[test]
    fn validity_is_strict_positivity() {
        assert!(!SequenceNumber::ZERO.is_valid());
        assert!(SequenceNumber::FIRST.is_valid());
        assert!(SequenceNumber::MAX.is_valid());
        assert_eq!(
            SequenceNumber::ZERO.check_valid("DATA writerSN"),
            Err(RtpsError::InvalidSequenceNumber {
                value: 0,
                context: "DATA writerSN",
            })
        );
        assert_eq!(
            SequenceNumber::FIRST.check_valid("DATA writerSN"),
            Ok(SequenceNumber::FIRST)
        );
        assert_eq!(SequenceNumber::default(), SequenceNumber::ZERO);
        assert_eq!(SequenceNumber::new(3).to_string(), "3");
    }

    #[test]
    fn arithmetic_saturates_instead_of_wrapping() {
        assert_eq!(SequenceNumber::new(5).next(), SequenceNumber::new(6));
        assert_eq!(SequenceNumber::new(5).previous(), SequenceNumber::new(4));
        assert_eq!(SequenceNumber::MAX.next(), SequenceNumber::MAX);
        assert_eq!(
            SequenceNumber::new(i64::MIN).previous(),
            SequenceNumber::new(i64::MIN)
        );
        assert_eq!(
            SequenceNumber::new(10).saturating_add(5),
            SequenceNumber::new(15)
        );
    }

    #[test]
    fn offsets_are_computed_in_wide_arithmetic() {
        assert_eq!(
            SequenceNumber::new(12).offset_from(SequenceNumber::new(10)),
            Some(2)
        );
        assert_eq!(
            SequenceNumber::new(9).offset_from(SequenceNumber::new(10)),
            None
        );
        // i64::MAX - i64::MIN is 2^64 - 1: the widest offset a u64 can hold,
        // and the reason the subtraction is done in i128 rather than i64.
        assert_eq!(
            SequenceNumber::MAX.offset_from(SequenceNumber::new(i64::MIN)),
            Some(u64::MAX)
        );
        assert_eq!(
            SequenceNumber::MAX.offset_from(SequenceNumber::ZERO),
            Some(i64::MAX as u64)
        );
    }

    #[test]
    fn a_sequence_number_is_eight_octets_high_half_first() {
        // §9.4.2.5: high (long) then low (unsigned long), each in the
        // stream's byte order. 0x0000_0001_0000_0002 = high 1, low 2.
        let bytes = to_vec_headerless(&SequenceNumber::new(0x0000_0001_0000_0002), Encoding::ROS2)
            .expect("encode");
        assert_eq!(bytes, [1, 0, 0, 0, 2, 0, 0, 0]);
        let big = to_vec_headerless(
            &SequenceNumber::new(0x0000_0001_0000_0002),
            Encoding::new(astrs_cdr::EncapsulationKind::CdrBe),
        )
        .expect("encode");
        assert_eq!(big, [0, 0, 0, 1, 0, 0, 0, 2]);
    }

    #[test]
    fn an_empty_set_is_twelve_octets() {
        let set = SequenceNumberSet::new(SequenceNumber::new(1));
        assert_eq!(set.num_bits(), 0);
        assert_eq!(set.num_words(), 0);
        assert_eq!(set.serialized_len(), 12);
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);
        assert_eq!(set.first(), None);
        assert_eq!(SequenceNumberSet::default().base(), SequenceNumber::FIRST);

        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        set.write(&mut writer).expect("write");
        assert_eq!(
            writer.finish(),
            [0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0],
            "base high=0, low=1, numBits=0, no bitmap words"
        );
    }

    #[test]
    fn bit_zero_is_the_most_significant_bit_of_the_first_word() {
        let mut set = SequenceNumberSet::new(SequenceNumber::new(1));
        set.insert(SequenceNumber::new(1)).expect("in window");
        assert_eq!(set.num_bits(), 1);
        assert_eq!(set.words()[0], 0x8000_0000);

        let mut writer = CdrWriter::headerless(Encoding::new(astrs_cdr::EncapsulationKind::CdrBe));
        set.write(&mut writer).expect("write");
        assert_eq!(
            writer.finish(),
            [0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 1, 0x80, 0, 0, 0]
        );
    }

    #[test]
    fn the_window_is_capped_at_two_hundred_and_fifty_six_bits() {
        let mut set = SequenceNumberSet::new(SequenceNumber::new(1));
        set.insert(SequenceNumber::new(256)).expect("last bit");
        assert_eq!(set.num_bits(), 256);
        assert_eq!(set.num_words(), 8);
        assert_eq!(set.serialized_len(), 12 + 32);
        assert_eq!(
            set.insert(SequenceNumber::new(257)),
            Err(RtpsError::InvalidSequenceNumberSet {
                reason: SetDefect::BitOutOfRange { offset: 256 },
                context: "SequenceNumberSet::insert",
            })
        );
        assert_eq!(
            set.insert(SequenceNumber::new(0)),
            Err(RtpsError::InvalidSequenceNumberSet {
                reason: SetDefect::BitOutOfRange { offset: -1 },
                context: "SequenceNumberSet::insert",
            })
        );
    }

    #[test]
    fn membership_and_iteration_agree() {
        let set = SequenceNumberSet::from_numbers(
            SequenceNumber::new(100),
            [
                SequenceNumber::new(100),
                SequenceNumber::new(131),
                SequenceNumber::new(132),
                SequenceNumber::new(163),
            ],
        )
        .expect("in window");
        assert_eq!(set.num_bits(), 64);
        assert_eq!(set.num_words(), 2);
        assert_eq!(set.len(), 4);
        let collected: Vec<i64> = set.iter().map(SequenceNumber::value).collect();
        assert_eq!(collected, [100, 131, 132, 163]);
        for number in &set {
            assert!(set.contains(number));
        }
        assert!(!set.contains(SequenceNumber::new(99)));
        assert!(!set.contains(SequenceNumber::new(101)));
        assert!(!set.contains(SequenceNumber::new(400)));
        assert_eq!(set.first(), Some(SequenceNumber::new(100)));
        assert_eq!(set.iter().size_hint(), (0, Some(64)));
        assert_eq!(set.to_string(), "100+{100, 131, 132, 163}/64");
    }

    #[test]
    fn widening_and_narrowing_the_window_keeps_the_invariant() {
        let mut set = SequenceNumberSet::from_numbers(
            SequenceNumber::new(1),
            [SequenceNumber::new(1), SequenceNumber::new(40)],
        )
        .expect("in window");
        assert_eq!(set.num_bits(), 40);
        set.set_num_bits(8).expect("narrower");
        assert_eq!(set.num_bits(), 8);
        assert_eq!(set.len(), 1, "bit 39 was dropped with the window");
        assert!(set.contains(SequenceNumber::new(1)));
        assert!(!set.contains(SequenceNumber::new(40)));

        set.set_num_bits(256).expect("widest");
        assert_eq!(set.num_words(), 8);
        assert_eq!(
            set.set_num_bits(257),
            Err(RtpsError::InvalidSequenceNumberSet {
                reason: SetDefect::NumBitsTooLarge { declared: 257 },
                context: "SequenceNumberSet::set_num_bits",
            })
        );
    }

    #[test]
    fn a_set_round_trips_through_its_octets() {
        let set = SequenceNumberSet::from_numbers(
            SequenceNumber::new(7),
            [SequenceNumber::new(7), SequenceNumber::new(70)],
        )
        .expect("in window");
        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        set.write(&mut writer).expect("write");
        let bytes = writer.finish();
        assert_eq!(bytes.len(), set.serialized_len());

        let mut reader = CdrReader::with_encoding(&bytes, Encoding::ROS2);
        let back = SequenceNumberSet::read(&mut reader, "test").expect("read");
        assert_eq!(back, set);
        assert_eq!(reader.remaining(), 0);
    }

    #[test]
    fn a_declared_width_above_two_hundred_and_fifty_six_is_refused_before_the_bitmap() {
        // numBits = 0x0000_1000; only the four count octets follow the base,
        // so a reader that trusted the count would run off the end.
        let bytes = [0, 0, 0, 0, 1, 0, 0, 0, 0x00, 0x10, 0x00, 0x00];
        let mut reader = CdrReader::with_encoding(&bytes, Encoding::ROS2);
        assert_eq!(
            SequenceNumberSet::read(&mut reader, "ACKNACK readerSNState").map(|_| ()),
            Err(RtpsError::InvalidSequenceNumberSet {
                reason: SetDefect::NumBitsTooLarge { declared: 0x1000 },
                context: "ACKNACK readerSNState",
            })
        );
    }

    #[test]
    fn bits_a_sender_left_set_past_the_window_are_dropped() {
        // numBits = 3, but the sender set all 32 bits of the only word.
        let bytes = [0, 0, 0, 0, 1, 0, 0, 0, 3, 0, 0, 0, 0xff, 0xff, 0xff, 0xff];
        let mut reader = CdrReader::with_encoding(&bytes, Encoding::ROS2);
        let set = SequenceNumberSet::read(&mut reader, "test").expect("read");
        assert_eq!(set.len(), 3);
        assert_eq!(set.words()[0], 0xe000_0000);
        assert!(set.contains(SequenceNumber::new(3)));
        assert!(!set.contains(SequenceNumber::new(4)));
    }

    #[test]
    fn validation_follows_the_acknack_clause() {
        let good = SequenceNumberSet::new(SequenceNumber::new(1));
        assert_eq!(good.validate("ACKNACK readerSNState"), Ok(()));
        let bad = SequenceNumberSet::new(SequenceNumber::ZERO);
        assert_eq!(
            bad.validate("ACKNACK readerSNState"),
            Err(RtpsError::InvalidSequenceNumberSet {
                reason: SetDefect::BaseNotPositive,
                context: "ACKNACK readerSNState",
            })
        );
    }

    #[test]
    fn a_truncated_bitmap_is_a_cdr_truncation() {
        // numBits = 64 promises two words; only one is present.
        let bytes = [0, 0, 0, 0, 1, 0, 0, 0, 64, 0, 0, 0, 1, 0, 0, 0];
        let mut reader = CdrReader::with_encoding(&bytes, Encoding::ROS2);
        let error = SequenceNumberSet::read(&mut reader, "test").expect_err("truncated");
        assert!(error.is_truncation());
    }
}
