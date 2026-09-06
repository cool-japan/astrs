//! Fragment numbers and the bitmap set that names a window of them.
//!
//! When a sample is larger than a datagram it is split into fragments of a
//! fixed size and sent in `DATA_FRAG` submessages. Fragments are numbered
//! from **one**, and the last one is the only one allowed to be short
//! (OMG DDSI-RTPS 2.3 §8.3.7.3, §9.4.2.7).
//!
//! A [`FragmentNumberSet`] (§9.4.2.8) is the fragment-level analogue of
//! [`SequenceNumberSet`](crate::structure::SequenceNumberSet): the same
//! `base / numBits / bitmap` grammar, the same 256-bit ceiling, the same
//! most-significant-bit-first ordering. Only the base differs — an unsigned
//! 32-bit fragment number instead of a 64-bit sequence number — which makes
//! the fixed part eight octets rather than twelve.
//!
//! ```
//! use astrs_rtps::structure::{FragmentNumber, FragmentNumberSet};
//!
//! let mut missing = FragmentNumberSet::new(FragmentNumber::FIRST);
//! missing.insert(FragmentNumber::new(1))?;
//! missing.insert(FragmentNumber::new(3))?;
//! assert_eq!(missing.len(), 2);
//! // base(4) + numBits(4) + one bitmap word(4).
//! assert_eq!(missing.serialized_len(), 12);
//! # Ok::<(), astrs_rtps::RtpsError>(())
//! ```

use core::fmt;

use astrs_cdr::{CdrDeserialize, CdrReader, CdrResult, CdrSerialize, CdrType, CdrWriter};

use crate::error::{RtpsError, RtpsResult, SetDefect};
use crate::structure::sequence::{MAX_SET_BITS, MAX_SET_WORDS};

/// Octets a [`FragmentNumber`] occupies on the wire.
pub const FRAGMENT_NUMBER_LEN: usize = 4;

/// Octets the fixed part of a [`FragmentNumberSet`] occupies.
pub const FRAGMENT_SET_PREFIX_LEN: usize = FRAGMENT_NUMBER_LEN + 4;

/// A one-based index into the fragments of one sample (§9.4.2.7).
///
/// Zero is never a legal fragment number; it is what
/// [`FragmentNumber::is_valid`] rejects and what the `DATA_FRAG` and
/// `NACK_FRAG` validity clauses forbid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct FragmentNumber(u32);

impl FragmentNumber {
    /// The first fragment of every sample.
    pub const FIRST: Self = Self(1);

    /// The zero value, which no valid fragment ever carries.
    pub const ZERO: Self = Self(0);

    /// The largest representable fragment number.
    pub const MAX: Self = Self(u32::MAX);

    /// Wrap a raw number.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// The raw number.
    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }

    /// True for a number a sample may actually have: strictly positive.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.0 > 0
    }

    /// The next fragment, saturating at [`FragmentNumber::MAX`].
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// How far `self` is above `base`, or `None` when it is below.
    #[must_use]
    pub const fn offset_from(self, base: Self) -> Option<u32> {
        if self.0 < base.0 {
            None
        } else {
            Some(self.0 - base.0)
        }
    }

    /// Reject a number a validity clause forbids.
    ///
    /// # Errors
    ///
    /// [`RtpsError::InvalidFragmentNumber`] when the value is zero.
    pub const fn check_valid(self, context: &'static str) -> RtpsResult<Self> {
        if self.is_valid() {
            Ok(self)
        } else {
            Err(RtpsError::InvalidFragmentNumber {
                value: self.0,
                context,
            })
        }
    }
}

impl fmt::Display for FragmentNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<u32> for FragmentNumber {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

impl From<FragmentNumber> for u32 {
    fn from(number: FragmentNumber) -> Self {
        number.0
    }
}

impl CdrType for FragmentNumber {
    const IS_PRIMITIVE: bool = true;
    const MIN_SERIALIZED_SIZE: usize = FRAGMENT_NUMBER_LEN;
}

impl CdrSerialize for FragmentNumber {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_u32(self.0)
    }
}

impl<'de> CdrDeserialize<'de> for FragmentNumber {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        Ok(Self(reader.read_u32()?))
    }
}

/// A window of at most 256 fragment numbers, as a bitmap (§9.4.2.8).
///
/// # Wire format
///
/// ```text
/// +--------+--------+--------+--------+
/// |     bitmapBase (FragmentNumber)   |
/// +--------+--------+--------+--------+
/// |        numBits (unsigned long)    |
/// +--------+--------+--------+--------+
/// ~   bitmap[M] (M = ceil(numBits/32))~
/// +--------+--------+--------+--------+
/// ```
///
/// Bit *i* stands for `bitmapBase + i`, most significant bit of each word
/// first — identical to the sequence-number set, which is why both share the
/// [`MAX_SET_BITS`] ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FragmentNumberSet {
    base: FragmentNumber,
    num_bits: u32,
    bitmap: [u32; MAX_SET_WORDS],
}

impl FragmentNumberSet {
    /// An empty set based at `base`.
    #[must_use]
    pub const fn new(base: FragmentNumber) -> Self {
        Self {
            base,
            num_bits: 0,
            bitmap: [0; MAX_SET_WORDS],
        }
    }

    /// The base of the window.
    #[must_use]
    pub const fn base(self) -> FragmentNumber {
        self.base
    }

    /// The declared width of the window, in bits.
    #[must_use]
    pub const fn num_bits(self) -> u32 {
        self.num_bits
    }

    /// The bitmap words, of which only [`FragmentNumberSet::num_words`] are
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
        FRAGMENT_SET_PREFIX_LEN + 4 * self.num_words()
    }

    /// True when no fragment number is in the set.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.bitmap.iter().all(|word| *word == 0)
    }

    /// How many fragment numbers are in the set.
    #[must_use]
    pub fn len(self) -> usize {
        self.bitmap
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }

    /// True when `number` is in the set.
    #[must_use]
    pub fn contains(self, number: FragmentNumber) -> bool {
        let Some(bit) = number.offset_from(self.base) else {
            return false;
        };
        if bit >= self.num_bits {
            return false;
        }
        let (word, mask) = split_bit(bit);
        self.bitmap.get(word).is_some_and(|value| value & mask != 0)
    }

    /// Add `number` to the set, widening the window to reach it.
    ///
    /// # Errors
    ///
    /// [`RtpsError::InvalidFragmentNumberSet`] with
    /// [`SetDefect::BitOutOfRange`] when `number` is below the base or 256 or
    /// more above it.
    pub fn insert(&mut self, number: FragmentNumber) -> RtpsResult<()> {
        let bit = number
            .offset_from(self.base)
            .filter(|offset| *offset < MAX_SET_BITS)
            .ok_or(RtpsError::InvalidFragmentNumberSet {
                reason: SetDefect::BitOutOfRange {
                    offset: i128::from(number.value()) - i128::from(self.base.value()),
                },
                context: "FragmentNumberSet::insert",
            })?;
        let (word, mask) = split_bit(bit);
        if let Some(slot) = self.bitmap.get_mut(word) {
            *slot |= mask;
        }
        if bit >= self.num_bits {
            self.num_bits = bit + 1;
        }
        Ok(())
    }

    /// Build a set that names every fragment in `numbers`.
    ///
    /// # Errors
    ///
    /// Those of [`FragmentNumberSet::insert`].
    pub fn from_numbers(
        base: FragmentNumber,
        numbers: impl IntoIterator<Item = FragmentNumber>,
    ) -> RtpsResult<Self> {
        let mut set = Self::new(base);
        for number in numbers {
            set.insert(number)?;
        }
        Ok(set)
    }

    /// Force the declared window width, clearing anything beyond it.
    ///
    /// # Errors
    ///
    /// [`RtpsError::InvalidFragmentNumberSet`] when `num_bits` exceeds
    /// [`MAX_SET_BITS`].
    pub fn set_num_bits(&mut self, num_bits: u32) -> RtpsResult<()> {
        if num_bits > MAX_SET_BITS {
            return Err(RtpsError::InvalidFragmentNumberSet {
                reason: SetDefect::NumBitsTooLarge { declared: num_bits },
                context: "FragmentNumberSet::set_num_bits",
            });
        }
        self.num_bits = num_bits;
        self.clear_beyond_window();
        Ok(())
    }

    /// Iterate over the fragment numbers in the set, ascending.
    #[must_use]
    pub const fn iter(&self) -> FragmentNumberSetIter<'_> {
        FragmentNumberSetIter { set: self, bit: 0 }
    }

    /// The smallest fragment number in the set.
    #[must_use]
    pub fn first(self) -> Option<FragmentNumber> {
        self.iter().next()
    }

    /// Check the clause §8.3.7.11.3 states for the set inside a `NACK_FRAG`.
    ///
    /// # Errors
    ///
    /// [`RtpsError::InvalidFragmentNumberSet`] when the base is zero or the
    /// window is wider than [`MAX_SET_BITS`].
    pub const fn validate(self, context: &'static str) -> RtpsResult<()> {
        if !self.base.is_valid() {
            return Err(RtpsError::InvalidFragmentNumberSet {
                reason: SetDefect::BaseNotPositive,
                context,
            });
        }
        if self.num_bits > MAX_SET_BITS {
            return Err(RtpsError::InvalidFragmentNumberSet {
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
    /// - [`RtpsError::InvalidFragmentNumberSet`] when `numBits` exceeds 256,
    ///   checked before any bitmap word is read.
    /// - [`RtpsError::Cdr`] when the body ends inside the set.
    pub fn read(reader: &mut CdrReader<'_>, context: &'static str) -> RtpsResult<Self> {
        let base = FragmentNumber::deserialize(reader)?;
        let num_bits = reader.read_u32()?;
        if num_bits > MAX_SET_BITS {
            return Err(RtpsError::InvalidFragmentNumberSet {
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
                *word &= !(u32::MAX >> keep);
            }
        }
    }
}

impl Default for FragmentNumberSet {
    /// An empty set based at [`FragmentNumber::FIRST`].
    fn default() -> Self {
        Self::new(FragmentNumber::FIRST)
    }
}

impl fmt::Display for FragmentNumberSet {
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

impl<'a> IntoIterator for &'a FragmentNumberSet {
    type Item = FragmentNumber;
    type IntoIter = FragmentNumberSetIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Iterator over the fragment numbers a [`FragmentNumberSet`] names.
#[derive(Debug, Clone)]
pub struct FragmentNumberSetIter<'a> {
    set: &'a FragmentNumberSet,
    bit: u32,
}

impl Iterator for FragmentNumberSetIter<'_> {
    type Item = FragmentNumber;

    fn next(&mut self) -> Option<Self::Item> {
        while self.bit < self.set.num_bits {
            let bit = self.bit;
            self.bit += 1;
            let (word, mask) = split_bit(bit);
            if self.set.bitmap.get(word).is_some_and(|w| w & mask != 0) {
                return Some(FragmentNumber::new(
                    self.set.base.value().saturating_add(bit),
                ));
            }
        }
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some((self.set.num_bits - self.bit) as usize))
    }
}

/// The `(word index, mask)` pair for bit `bit`, most significant bit first.
const fn split_bit(bit: u32) -> (usize, u32) {
    ((bit / 32) as usize, 1_u32 << (31 - (bit % 32)))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use astrs_cdr::{Encoding, to_vec_headerless};

    use super::*;

    #[test]
    fn fragment_numbers_are_one_based() {
        assert!(!FragmentNumber::ZERO.is_valid());
        assert!(FragmentNumber::FIRST.is_valid());
        assert_eq!(FragmentNumber::FIRST.value(), 1);
        assert_eq!(FragmentNumber::default(), FragmentNumber::ZERO);
        assert_eq!(
            FragmentNumber::ZERO.check_valid("DATA_FRAG fragmentStartingNum"),
            Err(RtpsError::InvalidFragmentNumber {
                value: 0,
                context: "DATA_FRAG fragmentStartingNum",
            })
        );
        assert_eq!(
            FragmentNumber::FIRST.check_valid("x"),
            Ok(FragmentNumber::FIRST)
        );
        assert_eq!(FragmentNumber::MAX.next(), FragmentNumber::MAX);
        assert_eq!(FragmentNumber::new(4).next(), FragmentNumber::new(5));
        assert_eq!(u32::from(FragmentNumber::from(6_u32)), 6);
        assert_eq!(FragmentNumber::new(12).to_string(), "12");
    }

    #[test]
    fn offsets_reject_numbers_below_the_base() {
        assert_eq!(
            FragmentNumber::new(9).offset_from(FragmentNumber::new(4)),
            Some(5)
        );
        assert_eq!(
            FragmentNumber::new(3).offset_from(FragmentNumber::new(4)),
            None
        );
    }

    #[test]
    fn a_fragment_number_is_four_octets_in_the_stream_byte_order() {
        assert_eq!(
            to_vec_headerless(&FragmentNumber::new(0x0102_0304), Encoding::ROS2).expect("encode"),
            [0x04, 0x03, 0x02, 0x01]
        );
        assert_eq!(
            to_vec_headerless(
                &FragmentNumber::new(0x0102_0304),
                Encoding::new(astrs_cdr::EncapsulationKind::CdrBe)
            )
            .expect("encode"),
            [0x01, 0x02, 0x03, 0x04]
        );
    }

    #[test]
    fn an_empty_set_is_eight_octets() {
        let set = FragmentNumberSet::new(FragmentNumber::FIRST);
        assert_eq!(set.serialized_len(), 8);
        assert_eq!(set.num_words(), 0);
        assert!(set.is_empty());
        assert_eq!(set.len(), 0);
        assert_eq!(set.first(), None);
        assert_eq!(FragmentNumberSet::default().base(), FragmentNumber::FIRST);

        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        set.write(&mut writer).expect("write");
        assert_eq!(writer.finish(), [1, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn membership_and_iteration_agree() {
        let set = FragmentNumberSet::from_numbers(
            FragmentNumber::new(5),
            [
                FragmentNumber::new(5),
                FragmentNumber::new(6),
                FragmentNumber::new(37),
            ],
        )
        .expect("in window");
        assert_eq!(set.num_bits(), 33);
        assert_eq!(set.num_words(), 2);
        assert_eq!(set.serialized_len(), 8 + 8);
        assert_eq!(set.len(), 3);
        let collected: Vec<u32> = set.iter().map(FragmentNumber::value).collect();
        assert_eq!(collected, [5, 6, 37]);
        for number in &set {
            assert!(set.contains(number));
        }
        assert!(!set.contains(FragmentNumber::new(4)));
        assert!(!set.contains(FragmentNumber::new(7)));
        assert_eq!(set.first(), Some(FragmentNumber::new(5)));
        assert_eq!(set.iter().size_hint(), (0, Some(33)));
        assert_eq!(set.to_string(), "5+{5, 6, 37}/33");
        assert_eq!(set.words()[0], 0xc000_0000);
    }

    #[test]
    fn the_window_is_capped_at_two_hundred_and_fifty_six_bits() {
        let mut set = FragmentNumberSet::new(FragmentNumber::FIRST);
        set.insert(FragmentNumber::new(256)).expect("last bit");
        assert_eq!(set.num_bits(), 256);
        assert_eq!(
            set.insert(FragmentNumber::new(257)),
            Err(RtpsError::InvalidFragmentNumberSet {
                reason: SetDefect::BitOutOfRange { offset: 256 },
                context: "FragmentNumberSet::insert",
            })
        );
        assert_eq!(
            set.set_num_bits(300),
            Err(RtpsError::InvalidFragmentNumberSet {
                reason: SetDefect::NumBitsTooLarge { declared: 300 },
                context: "FragmentNumberSet::set_num_bits",
            })
        );
        set.set_num_bits(2).expect("narrower");
        assert_eq!(set.len(), 0, "bit 255 fell outside the new window");
    }

    #[test]
    fn a_set_round_trips_through_its_octets() {
        let set = FragmentNumberSet::from_numbers(
            FragmentNumber::new(2),
            [FragmentNumber::new(2), FragmentNumber::new(33)],
        )
        .expect("in window");
        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        set.write(&mut writer).expect("write");
        let bytes = writer.finish();
        assert_eq!(bytes.len(), set.serialized_len());

        let mut reader = CdrReader::with_encoding(&bytes, Encoding::ROS2);
        assert_eq!(
            FragmentNumberSet::read(&mut reader, "test").expect("read"),
            set
        );
        assert_eq!(reader.remaining(), 0);
    }

    #[test]
    fn a_declared_width_above_the_ceiling_is_refused_before_the_bitmap() {
        let bytes = [1, 0, 0, 0, 0x01, 0x01, 0, 0];
        let mut reader = CdrReader::with_encoding(&bytes, Encoding::ROS2);
        assert_eq!(
            FragmentNumberSet::read(&mut reader, "NACK_FRAG fragmentNumberState").map(|_| ()),
            Err(RtpsError::InvalidFragmentNumberSet {
                reason: SetDefect::NumBitsTooLarge { declared: 0x0101 },
                context: "NACK_FRAG fragmentNumberState",
            })
        );
    }

    #[test]
    fn validation_follows_the_nack_frag_clause() {
        assert_eq!(
            FragmentNumberSet::new(FragmentNumber::FIRST).validate("x"),
            Ok(())
        );
        assert_eq!(
            FragmentNumberSet::new(FragmentNumber::ZERO).validate("NACK_FRAG"),
            Err(RtpsError::InvalidFragmentNumberSet {
                reason: SetDefect::BaseNotPositive,
                context: "NACK_FRAG",
            })
        );
    }

    #[test]
    fn bits_past_the_declared_window_are_dropped_on_read() {
        let bytes = [1, 0, 0, 0, 4, 0, 0, 0, 0xff, 0xff, 0xff, 0xff];
        let mut reader = CdrReader::with_encoding(&bytes, Encoding::ROS2);
        let set = FragmentNumberSet::read(&mut reader, "test").expect("read");
        assert_eq!(set.len(), 4);
        assert_eq!(set.words()[0], 0xf000_0000);
    }
}
