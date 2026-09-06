//! `ACKNACK` and `NACK_FRAG`: what a reader says it is missing.
//!
//! ```text
//!  ACKNACK (§8.3.7.1)                NACK_FRAG (§8.3.7.10)
//! +---------------------------+     +---------------------------+
//! |         readerId          |     |         readerId          |
//! |         writerId          |     |         writerId          |
//! ~   readerSNState (12..44)  ~     |         writerSN (8)      |
//! |         count             |     ~ fragmentNumberState (8..40)~
//! +---------------------------+     |         count             |
//!                                   +---------------------------+
//! ```
//!
//! An `ACKNACK` says two things at once, and the split is the whole point of
//! the design: **`readerSNState.bitmapBase` is the positive acknowledgement**
//! — every sequence number below it has arrived — and **the bits set in the
//! bitmap are the negative one**, naming what the reader is still missing at
//! or above the base. A set with no bits at all is therefore the normal
//! steady-state message: "I have everything below the base, and I want
//! nothing".
//!
//! `NACK_FRAG` does the same one level down, for the fragments of a single
//! sample. It has no positive-acknowledgement reading: §8.3.7.10 defines only
//! the negative one, so a `NACK_FRAG` with an empty set requests nothing.
//!
//! ```
//! use astrs_rtps::messages::AckNack;
//! use astrs_rtps::structure::{EntityId, EntityKind, SequenceNumber, SequenceNumberSet};
//!
//! // "I have 1..=9; I am missing 10 and 12."
//! let state = SequenceNumberSet::from_numbers(
//!     SequenceNumber::new(10),
//!     [SequenceNumber::new(10), SequenceNumber::new(12)],
//! )?;
//! let acknack = AckNack::new(
//!     EntityId::user_defined(2, EntityKind::USER_READER_NO_KEY),
//!     EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
//!     state,
//!     1,
//! );
//! acknack.validate()?;
//! assert_eq!(acknack.acknowledged_through(), SequenceNumber::new(9));
//! assert_eq!(acknack.missing().count(), 2);
//! # Ok::<(), astrs_rtps::RtpsError>(())
//! ```

use core::fmt;

use astrs_cdr::{CdrDeserialize, CdrReader, CdrSerialize, CdrWriter, Endianness};

use crate::error::RtpsResult;
use crate::messages::flags::{self, Extension, SubmessageFlags, body_encoding};
use crate::messages::header::SubmessageHeader;
use crate::structure::fragment::{FragmentNumberSet, FragmentNumberSetIter};
use crate::structure::guid::EntityId;
use crate::structure::sequence::{SequenceNumber, SequenceNumberSet, SequenceNumberSetIter};

/// Octets an `ACKNACK` occupies besides its `readerSNState`.
///
/// `readerId` + `writerId` + `count`.
pub const ACKNACK_FIXED_LEN: usize = 12;

/// Octets a `NACK_FRAG` occupies besides its `fragmentNumberState`.
///
/// `readerId` + `writerId` + `writerSN` + `count`.
pub const NACK_FRAG_FIXED_LEN: usize = 20;

/// The `ACKNACK` submessage (§8.3.7.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AckNack {
    /// Byte order of the body.
    pub endianness: Endianness,
    /// `F`: the reader is not requesting a repair, only reporting state.
    pub is_final: bool,
    /// The reader sending the acknowledgement.
    pub reader_id: EntityId,
    /// The writer being acknowledged.
    pub writer_id: EntityId,
    /// Base = everything below has arrived; bits = what is missing.
    pub reader_sn_state: SequenceNumberSet,
    /// `Count_t` (§9.4.2.10): increments with every `ACKNACK` the reader
    /// sends, so a writer can drop one that arrived out of order.
    pub count: i32,
    /// What this peer said that this build does not interpret.
    pub extension: Extension,
}

impl AckNack {
    /// The flag bits §8.3.7.1.1 defines for `ACKNACK`.
    pub const DEFINED_FLAGS: u8 = flags::ENDIANNESS | flags::FINAL;

    /// A little-endian, non-final acknowledgement.
    #[must_use]
    pub const fn new(
        reader_id: EntityId,
        writer_id: EntityId,
        reader_sn_state: SequenceNumberSet,
        count: i32,
    ) -> Self {
        Self {
            endianness: Endianness::Little,
            is_final: false,
            reader_id,
            writer_id,
            reader_sn_state,
            count,
            extension: Extension::EMPTY,
        }
    }

    /// The same submessage with the `F` flag set.
    #[must_use]
    pub const fn finalized(mut self) -> Self {
        self.is_final = true;
        self
    }

    /// The same submessage in the stated byte order.
    #[must_use]
    pub const fn with_endianness(mut self, endianness: Endianness) -> Self {
        self.endianness = endianness;
        self
    }

    /// The flags octet this submessage encodes to.
    #[must_use]
    pub const fn flags(&self) -> SubmessageFlags {
        SubmessageFlags::from_endianness(self.endianness)
            .set(flags::FINAL, self.is_final)
            .with(self.extension.reserved_flags)
    }

    /// Octets the body occupies.
    #[must_use]
    pub fn body_len(&self) -> usize {
        ACKNACK_FIXED_LEN + self.reader_sn_state.serialized_len() + self.extension.trailing_len()
    }

    /// The highest sequence number the reader has received contiguously:
    /// `bitmapBase - 1`.
    #[must_use]
    pub fn acknowledged_through(&self) -> SequenceNumber {
        self.reader_sn_state.base().previous()
    }

    /// The sequence numbers the reader is asking to have resent.
    #[must_use]
    pub fn missing(&self) -> SequenceNumberSetIter<'_> {
        self.reader_sn_state.iter()
    }

    /// True when the reader is missing nothing: a pure acknowledgement.
    #[must_use]
    pub fn is_pure_ack(&self) -> bool {
        self.reader_sn_state.is_empty()
    }

    /// Check the validity clauses of §8.3.7.1.3.
    ///
    /// # Errors
    ///
    /// [`RtpsError::InvalidSequenceNumberSet`](crate::RtpsError::InvalidSequenceNumberSet)
    /// when the base is not strictly positive or the window is too wide.
    pub fn validate(&self) -> RtpsResult<()> {
        self.reader_sn_state.validate("ACKNACK readerSNState")
    }

    /// Write the body.
    ///
    /// # Errors
    ///
    /// [`RtpsError::Cdr`](crate::RtpsError::Cdr) from a field write.
    pub fn write_body(&self, writer: &mut CdrWriter) -> RtpsResult<()> {
        self.reader_id.serialize(writer)?;
        self.writer_id.serialize(writer)?;
        self.reader_sn_state.write(writer)?;
        writer.write_i32(self.count)?;
        writer.write_octets(&self.extension.trailing);
        Ok(())
    }

    /// Read a body against the flags its header carried.
    ///
    /// # Errors
    ///
    /// - [`RtpsError::InvalidSequenceNumberSet`](crate::RtpsError::InvalidSequenceNumberSet)
    ///   when the set declares more than 256 bits.
    /// - [`RtpsError::Cdr`](crate::RtpsError::Cdr) when the body ends inside
    ///   a field.
    pub fn read(header: &SubmessageHeader, body: &[u8]) -> RtpsResult<Self> {
        let endianness = header.endianness();
        let mut reader = CdrReader::with_encoding(body, body_encoding(endianness));
        let reader_id = EntityId::deserialize(&mut reader)?;
        let writer_id = EntityId::deserialize(&mut reader)?;
        let reader_sn_state = SequenceNumberSet::read(&mut reader, "ACKNACK readerSNState")?;
        let count = reader.read_i32()?;
        Ok(Self {
            endianness,
            is_final: header.flags.has(flags::FINAL),
            reader_id,
            writer_id,
            reader_sn_state,
            count,
            extension: Extension {
                reserved_flags: header.flags.reserved(Self::DEFINED_FLAGS),
                trailing: reader.peek_remaining().to_vec(),
            },
        })
    }
}

impl fmt::Display for AckNack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "ACKNACK {} -> {} acked through {} missing {} count {}{}",
            self.reader_id,
            self.writer_id,
            self.acknowledged_through(),
            self.reader_sn_state.len(),
            self.count,
            if self.is_final { " final" } else { "" },
        )
    }
}

/// The `NACK_FRAG` submessage (§8.3.7.10).
///
/// §8.3.7.10.1 defines no flags beyond `E`: a `NACK_FRAG` is always a
/// request, so there is no final form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NackFrag {
    /// Byte order of the body.
    pub endianness: Endianness,
    /// The reader sending the request.
    pub reader_id: EntityId,
    /// The writer being asked.
    pub writer_id: EntityId,
    /// The sample whose fragments are missing.
    pub writer_sn: SequenceNumber,
    /// The fragments the reader is asking to have resent.
    pub fragment_number_state: FragmentNumberSet,
    /// `Count_t` (§9.4.2.10).
    pub count: i32,
    /// What this peer said that this build does not interpret.
    pub extension: Extension,
}

impl NackFrag {
    /// The flag bits §8.3.7.10.1 defines for `NACK_FRAG`.
    pub const DEFINED_FLAGS: u8 = flags::ENDIANNESS;

    /// A little-endian fragment request.
    #[must_use]
    pub const fn new(
        reader_id: EntityId,
        writer_id: EntityId,
        writer_sn: SequenceNumber,
        fragment_number_state: FragmentNumberSet,
        count: i32,
    ) -> Self {
        Self {
            endianness: Endianness::Little,
            reader_id,
            writer_id,
            writer_sn,
            fragment_number_state,
            count,
            extension: Extension::EMPTY,
        }
    }

    /// The same submessage in the stated byte order.
    #[must_use]
    pub const fn with_endianness(mut self, endianness: Endianness) -> Self {
        self.endianness = endianness;
        self
    }

    /// The flags octet this submessage encodes to.
    #[must_use]
    pub const fn flags(&self) -> SubmessageFlags {
        SubmessageFlags::from_endianness(self.endianness).with(self.extension.reserved_flags)
    }

    /// Octets the body occupies.
    #[must_use]
    pub fn body_len(&self) -> usize {
        NACK_FRAG_FIXED_LEN
            + self.fragment_number_state.serialized_len()
            + self.extension.trailing_len()
    }

    /// The fragments the reader is asking to have resent.
    #[must_use]
    pub fn missing(&self) -> FragmentNumberSetIter<'_> {
        self.fragment_number_state.iter()
    }

    /// Check the validity clauses of §8.3.7.10.3.
    ///
    /// # Errors
    ///
    /// [`RtpsError::InvalidSequenceNumber`](crate::RtpsError::InvalidSequenceNumber)
    /// or
    /// [`RtpsError::InvalidFragmentNumberSet`](crate::RtpsError::InvalidFragmentNumberSet).
    pub fn validate(&self) -> RtpsResult<()> {
        self.writer_sn.check_valid("NACK_FRAG writerSN")?;
        self.fragment_number_state
            .validate("NACK_FRAG fragmentNumberState")
    }

    /// Write the body.
    ///
    /// # Errors
    ///
    /// [`RtpsError::Cdr`](crate::RtpsError::Cdr) from a field write.
    pub fn write_body(&self, writer: &mut CdrWriter) -> RtpsResult<()> {
        self.reader_id.serialize(writer)?;
        self.writer_id.serialize(writer)?;
        self.writer_sn.serialize(writer)?;
        self.fragment_number_state.write(writer)?;
        writer.write_i32(self.count)?;
        writer.write_octets(&self.extension.trailing);
        Ok(())
    }

    /// Read a body against the flags its header carried.
    ///
    /// # Errors
    ///
    /// - [`RtpsError::InvalidFragmentNumberSet`](crate::RtpsError::InvalidFragmentNumberSet)
    ///   when the set declares more than 256 bits.
    /// - [`RtpsError::Cdr`](crate::RtpsError::Cdr) when the body ends inside
    ///   a field.
    pub fn read(header: &SubmessageHeader, body: &[u8]) -> RtpsResult<Self> {
        let endianness = header.endianness();
        let mut reader = CdrReader::with_encoding(body, body_encoding(endianness));
        let reader_id = EntityId::deserialize(&mut reader)?;
        let writer_id = EntityId::deserialize(&mut reader)?;
        let writer_sn = SequenceNumber::deserialize(&mut reader)?;
        let fragment_number_state =
            FragmentNumberSet::read(&mut reader, "NACK_FRAG fragmentNumberState")?;
        let count = reader.read_i32()?;
        Ok(Self {
            endianness,
            reader_id,
            writer_id,
            writer_sn,
            fragment_number_state,
            count,
            extension: Extension {
                reserved_flags: header.flags.reserved(Self::DEFINED_FLAGS),
                trailing: reader.peek_remaining().to_vec(),
            },
        })
    }
}

impl fmt::Display for NackFrag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "NACK_FRAG {} -> {} sn {} missing {} fragment(s) count {}",
            self.reader_id,
            self.writer_id,
            self.writer_sn,
            self.fragment_number_state.len(),
            self.count
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use astrs_cdr::Encoding;

    use super::*;
    use crate::error::{RtpsError, SetDefect};
    use crate::messages::kind::SubmessageId;
    use crate::structure::fragment::FragmentNumber;
    use crate::structure::guid::EntityKind;

    fn writer_id() -> EntityId {
        EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY)
    }

    fn reader_id() -> EntityId {
        EntityId::user_defined(2, EntityKind::USER_READER_NO_KEY)
    }

    #[test]
    fn a_pure_acknowledgement_is_an_empty_set() {
        let acknack = AckNack::new(
            reader_id(),
            writer_id(),
            SequenceNumberSet::new(SequenceNumber::new(6)),
            4,
        )
        .finalized();
        assert!(acknack.is_pure_ack());
        assert_eq!(acknack.acknowledged_through(), SequenceNumber::new(5));
        assert_eq!(acknack.missing().count(), 0);
        assert_eq!(acknack.flags().raw(), 0x03); // E | F
        assert_eq!(acknack.body_len(), 12 + 12);
        assert_eq!(acknack.validate(), Ok(()));

        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        acknack.write_body(&mut writer).expect("write");
        assert_eq!(
            writer.finish(),
            [
                0x00, 0x00, 0x02, 0x04, // readerId
                0x00, 0x00, 0x01, 0x03, // writerId
                0x00, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, // bitmapBase = 6
                0x00, 0x00, 0x00, 0x00, // numBits = 0, no bitmap words
                0x04, 0x00, 0x00, 0x00, // count = 4
            ]
        );
        assert_eq!(
            acknack.to_string(),
            "ACKNACK 00.00.02.04 -> 00.00.01.03 acked through 5 missing 0 count 4 final"
        );
    }

    #[test]
    fn the_bitmap_names_what_is_missing_above_the_base() {
        let state = SequenceNumberSet::from_numbers(
            SequenceNumber::new(10),
            [SequenceNumber::new(10), SequenceNumber::new(12)],
        )
        .expect("in window");
        let acknack = AckNack::new(reader_id(), writer_id(), state, 1);
        assert!(!acknack.is_pure_ack());
        assert_eq!(acknack.acknowledged_through(), SequenceNumber::new(9));
        let missing: Vec<i64> = acknack.missing().map(SequenceNumber::value).collect();
        assert_eq!(missing, [10, 12]);
        assert_eq!(acknack.body_len(), 12 + 16);

        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        acknack.write_body(&mut writer).expect("write");
        let body = writer.finish();
        assert_eq!(
            &body[16..24],
            &[
                0x03, 0x00, 0x00, 0x00, // numBits = 3
                0x00, 0x00, 0x00, 0xa0, // bitmap: bits 0 and 2, little-endian
            ]
        );
        let header = SubmessageHeader::new(SubmessageId::AckNack, acknack.flags(), 0);
        assert_eq!(AckNack::read(&header, &body).expect("read"), acknack);
    }

    #[test]
    fn a_base_of_zero_fails_the_validity_clause() {
        let acknack = AckNack::new(
            reader_id(),
            writer_id(),
            SequenceNumberSet::new(SequenceNumber::ZERO),
            1,
        );
        assert_eq!(
            acknack.validate(),
            Err(RtpsError::InvalidSequenceNumberSet {
                reason: SetDefect::BaseNotPositive,
                context: "ACKNACK readerSNState",
            })
        );
    }

    #[test]
    fn an_acknack_round_trips_in_both_byte_orders() {
        for endianness in [Endianness::Little, Endianness::Big] {
            let state = SequenceNumberSet::from_numbers(
                SequenceNumber::new(1),
                [SequenceNumber::new(1), SequenceNumber::new(40)],
            )
            .expect("in window");
            let acknack =
                AckNack::new(reader_id(), writer_id(), state, -1).with_endianness(endianness);
            let mut writer = CdrWriter::headerless(body_encoding(endianness));
            acknack.write_body(&mut writer).expect("write");
            let body = writer.finish();
            assert_eq!(body.len(), acknack.body_len());
            let header = SubmessageHeader::new(SubmessageId::AckNack, acknack.flags(), 0);
            assert_eq!(AckNack::read(&header, &body).expect("read"), acknack);
        }
    }

    #[test]
    fn a_version_extension_after_the_count_is_preserved() {
        let acknack = AckNack::new(
            reader_id(),
            writer_id(),
            SequenceNumberSet::new(SequenceNumber::FIRST),
            1,
        );
        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        acknack.write_body(&mut writer).expect("write");
        let mut body = writer.finish();
        body.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]);

        let header = SubmessageHeader::new(
            SubmessageId::AckNack,
            SubmessageFlags::new(flags::ENDIANNESS | 0x80),
            0,
        );
        let decoded = AckNack::read(&header, &body).expect("read");
        assert_eq!(decoded.extension.reserved_flags, 0x80);
        assert_eq!(decoded.extension.trailing, [0x11, 0x22, 0x33, 0x44]);

        let mut again = CdrWriter::headerless(Encoding::ROS2);
        decoded.write_body(&mut again).expect("write");
        assert_eq!(again.finish(), body);
    }

    #[test]
    fn a_nack_frag_names_fragments_of_one_sample() {
        let state = FragmentNumberSet::from_numbers(
            FragmentNumber::new(2),
            [FragmentNumber::new(2), FragmentNumber::new(3)],
        )
        .expect("in window");
        let nack = NackFrag::new(reader_id(), writer_id(), SequenceNumber::new(7), state, 2);
        assert_eq!(nack.flags().raw(), 0x01);
        assert_eq!(nack.body_len(), 20 + 12);
        assert_eq!(nack.validate(), Ok(()));
        let missing: Vec<u32> = nack.missing().map(FragmentNumber::value).collect();
        assert_eq!(missing, [2, 3]);

        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        nack.write_body(&mut writer).expect("write");
        assert_eq!(
            writer.finish(),
            [
                0x00, 0x00, 0x02, 0x04, // readerId
                0x00, 0x00, 0x01, 0x03, // writerId
                0x00, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, // writerSN = 7
                0x02, 0x00, 0x00, 0x00, // bitmapBase = 2
                0x02, 0x00, 0x00, 0x00, // numBits = 2
                0x00, 0x00, 0x00, 0xc0, // bits 0 and 1
                0x02, 0x00, 0x00, 0x00, // count = 2
            ]
        );
        assert_eq!(
            nack.to_string(),
            "NACK_FRAG 00.00.02.04 -> 00.00.01.03 sn 7 missing 2 fragment(s) count 2"
        );
    }

    #[test]
    fn a_nack_frag_validates_both_the_sample_and_the_set() {
        let mut nack = NackFrag::new(
            reader_id(),
            writer_id(),
            SequenceNumber::ZERO,
            FragmentNumberSet::new(FragmentNumber::FIRST),
            1,
        );
        assert_eq!(
            nack.validate(),
            Err(RtpsError::InvalidSequenceNumber {
                value: 0,
                context: "NACK_FRAG writerSN",
            })
        );
        nack.writer_sn = SequenceNumber::FIRST;
        nack.fragment_number_state = FragmentNumberSet::new(FragmentNumber::ZERO);
        assert_eq!(
            nack.validate(),
            Err(RtpsError::InvalidFragmentNumberSet {
                reason: SetDefect::BaseNotPositive,
                context: "NACK_FRAG fragmentNumberState",
            })
        );
    }

    #[test]
    fn a_nack_frag_round_trips_in_both_byte_orders() {
        for endianness in [Endianness::Little, Endianness::Big] {
            let nack = NackFrag::new(
                reader_id(),
                writer_id(),
                SequenceNumber::new(3),
                FragmentNumberSet::from_numbers(FragmentNumber::FIRST, [FragmentNumber::new(33)])
                    .expect("in window"),
                9,
            )
            .with_endianness(endianness);
            let mut writer = CdrWriter::headerless(body_encoding(endianness));
            nack.write_body(&mut writer).expect("write");
            let body = writer.finish();
            assert_eq!(body.len(), nack.body_len());
            let header = SubmessageHeader::new(SubmessageId::NackFrag, nack.flags(), 0);
            assert_eq!(NackFrag::read(&header, &body).expect("read"), nack);
        }
    }

    #[test]
    fn a_hostile_num_bits_is_refused_before_the_bitmap_is_read() {
        let mut body = Vec::from([0_u8; 8]);
        body.extend_from_slice(&[0, 0, 0, 0, 1, 0, 0, 0]); // base = 1
        body.extend_from_slice(&[0xff, 0xff, 0xff, 0x7f]); // numBits = huge
        let header =
            SubmessageHeader::new(SubmessageId::AckNack, SubmessageFlags::LITTLE_ENDIAN, 0);
        assert_eq!(
            AckNack::read(&header, &body).map(|_| ()),
            Err(RtpsError::InvalidSequenceNumberSet {
                reason: SetDefect::NumBitsTooLarge {
                    declared: 0x7fff_ffff
                },
                context: "ACKNACK readerSNState",
            })
        );
    }
}
