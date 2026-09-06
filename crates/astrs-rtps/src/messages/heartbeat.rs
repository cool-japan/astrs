//! `HEARTBEAT` and `HEARTBEAT_FRAG`: what a writer says it holds.
//!
//! ```text
//!  HEARTBEAT (§8.3.7.5)              HEARTBEAT_FRAG (§8.3.7.6)
//! +---------------------------+     +---------------------------+
//! |         readerId          |     |         readerId          |
//! |         writerId          |     |         writerId          |
//! |         firstSN (8)       |     |         writerSN (8)      |
//! |         lastSN (8)        |     |    lastFragmentNum        |
//! |         count             |     |         count             |
//! +---------------------------+     +---------------------------+
//!            28 octets                        24 octets
//! ```
//!
//! A `HEARTBEAT` announces the range `[firstSN, lastSN]` a reliable writer
//! still has available. A reader compares it against what it received and
//! answers with an [`AckNack`](crate::messages::AckNack) — unless the `F`
//! (final) flag says no answer is wanted, or the `L` (liveliness) flag says
//! the heartbeat exists only to assert that the writer is alive.
//!
//! `lastSN` is allowed to be exactly `firstSN - 1`, and that is how a writer
//! with an empty history announces itself: `firstSN = 1`, `lastSN = 0`
//! (§8.3.7.5.3).
//!
//! `HEARTBEAT_FRAG` does the same job one level down, for a sample being
//! delivered in fragments: it names the highest fragment of `writerSN` the
//! writer has available, and a reader answers with a
//! [`NackFrag`](crate::messages::NackFrag).
//!
//! ```
//! use astrs_rtps::messages::Heartbeat;
//! use astrs_rtps::structure::{ENTITYID_UNKNOWN, EntityId, EntityKind, SequenceNumber};
//!
//! let empty_history = Heartbeat::new(
//!     ENTITYID_UNKNOWN,
//!     EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
//!     SequenceNumber::FIRST,
//!     SequenceNumber::ZERO,
//!     1,
//! );
//! empty_history.validate()?;
//! assert!(empty_history.is_empty_history());
//! assert_eq!(empty_history.body_len(), 28);
//! # Ok::<(), astrs_rtps::RtpsError>(())
//! ```

use core::fmt;

use astrs_cdr::{CdrDeserialize, CdrReader, CdrSerialize, CdrWriter, Endianness};

use crate::error::{RtpsError, RtpsResult};
use crate::messages::flags::{self, Extension, SubmessageFlags, body_encoding};
use crate::messages::header::SubmessageHeader;
use crate::structure::fragment::FragmentNumber;
use crate::structure::guid::EntityId;
use crate::structure::sequence::SequenceNumber;

/// Octets the fixed fields of a `HEARTBEAT` occupy.
pub const HEARTBEAT_BODY_LEN: usize = 28;

/// Octets the fixed fields of a `HEARTBEAT_FRAG` occupy.
pub const HEARTBEAT_FRAG_BODY_LEN: usize = 24;

/// The `HEARTBEAT` submessage (§8.3.7.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Heartbeat {
    /// Byte order of the body.
    pub endianness: Endianness,
    /// `F`: no `ACKNACK` is being solicited.
    pub is_final: bool,
    /// `L`: the heartbeat asserts liveliness rather than announcing history.
    pub liveliness: bool,
    /// The reader addressed, or [`EntityId::UNKNOWN`] for all of them.
    pub reader_id: EntityId,
    /// The writer whose history is being announced.
    pub writer_id: EntityId,
    /// The lowest sequence number still available.
    pub first_sn: SequenceNumber,
    /// The highest sequence number the writer has ever written.
    pub last_sn: SequenceNumber,
    /// `Count_t` (§9.4.2.10): increments with every heartbeat, so a reader
    /// can drop one that arrived out of order.
    pub count: i32,
    /// What this peer said that this build does not interpret.
    ///
    /// The RTPS 2.3 group-info fields land here; see
    /// [`flags`].
    pub extension: Extension,
}

impl Heartbeat {
    /// The flag bits this build interprets for `HEARTBEAT`.
    pub const DEFINED_FLAGS: u8 = flags::ENDIANNESS | flags::FINAL | flags::LIVELINESS;

    /// A little-endian, non-final heartbeat.
    #[must_use]
    pub const fn new(
        reader_id: EntityId,
        writer_id: EntityId,
        first_sn: SequenceNumber,
        last_sn: SequenceNumber,
        count: i32,
    ) -> Self {
        Self {
            endianness: Endianness::Little,
            is_final: false,
            liveliness: false,
            reader_id,
            writer_id,
            first_sn,
            last_sn,
            count,
            extension: Extension::EMPTY,
        }
    }

    /// The same heartbeat with the `F` flag set: no answer wanted.
    #[must_use]
    pub const fn finalized(mut self) -> Self {
        self.is_final = true;
        self
    }

    /// The same heartbeat with the `L` flag set.
    #[must_use]
    pub const fn asserting_liveliness(mut self) -> Self {
        self.liveliness = true;
        self
    }

    /// The same heartbeat in the stated byte order.
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
            .set(flags::LIVELINESS, self.liveliness)
            .with(self.extension.reserved_flags)
    }

    /// Octets the body occupies.
    #[must_use]
    pub fn body_len(&self) -> usize {
        HEARTBEAT_BODY_LEN + self.extension.trailing_len()
    }

    /// True when the writer is announcing that it holds nothing:
    /// `lastSN == firstSN - 1`.
    #[must_use]
    pub fn is_empty_history(&self) -> bool {
        self.last_sn.value() == self.first_sn.value().saturating_sub(1)
    }

    /// How many sequence numbers the announced range covers.
    #[must_use]
    pub fn available(&self) -> u64 {
        self.last_sn
            .offset_from(self.first_sn)
            .map_or(0, |offset| offset + 1)
    }

    /// Check the validity clauses of §8.3.7.5.3.
    ///
    /// # Errors
    ///
    /// - [`RtpsError::InvalidSequenceNumber`] when `firstSN` is not strictly
    ///   positive.
    /// - [`RtpsError::InvalidHeartbeatRange`] when `lastSN` is below
    ///   `firstSN - 1`.
    pub fn validate(&self) -> RtpsResult<()> {
        self.first_sn.check_valid("HEARTBEAT firstSN")?;
        if self.last_sn.value() < self.first_sn.value().saturating_sub(1) {
            return Err(RtpsError::InvalidHeartbeatRange {
                first: self.first_sn.value(),
                last: self.last_sn.value(),
            });
        }
        Ok(())
    }

    /// Write the body.
    ///
    /// # Errors
    ///
    /// [`RtpsError::Cdr`] from a field write.
    pub fn write_body(&self, writer: &mut CdrWriter) -> RtpsResult<()> {
        self.reader_id.serialize(writer)?;
        self.writer_id.serialize(writer)?;
        self.first_sn.serialize(writer)?;
        self.last_sn.serialize(writer)?;
        writer.write_i32(self.count)?;
        writer.write_octets(&self.extension.trailing);
        Ok(())
    }

    /// Read a body against the flags its header carried.
    ///
    /// # Errors
    ///
    /// [`RtpsError::Cdr`] when the body ends inside a field.
    pub fn read(header: &SubmessageHeader, body: &[u8]) -> RtpsResult<Self> {
        let endianness = header.endianness();
        let mut reader = CdrReader::with_encoding(body, body_encoding(endianness));
        let reader_id = EntityId::deserialize(&mut reader)?;
        let writer_id = EntityId::deserialize(&mut reader)?;
        let first_sn = SequenceNumber::deserialize(&mut reader)?;
        let last_sn = SequenceNumber::deserialize(&mut reader)?;
        let count = reader.read_i32()?;
        Ok(Self {
            endianness,
            is_final: header.flags.has(flags::FINAL),
            liveliness: header.flags.has(flags::LIVELINESS),
            reader_id,
            writer_id,
            first_sn,
            last_sn,
            count,
            extension: Extension {
                reserved_flags: header.flags.reserved(Self::DEFINED_FLAGS),
                trailing: reader.peek_remaining().to_vec(),
            },
        })
    }
}

impl fmt::Display for Heartbeat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "HEARTBEAT {} -> {} [{}, {}] count {}{}{}",
            self.writer_id,
            self.reader_id,
            self.first_sn,
            self.last_sn,
            self.count,
            if self.is_final { " final" } else { "" },
            if self.liveliness { " liveliness" } else { "" },
        )
    }
}

/// The `HEARTBEAT_FRAG` submessage (§8.3.7.6).
///
/// §8.3.7.6.1 defines no flags beyond `E`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeartbeatFrag {
    /// Byte order of the body.
    pub endianness: Endianness,
    /// The reader addressed.
    pub reader_id: EntityId,
    /// The writer whose fragments are being announced.
    pub writer_id: EntityId,
    /// The sample being fragmented.
    pub writer_sn: SequenceNumber,
    /// The highest fragment of `writerSN` the writer has available.
    pub last_fragment_num: FragmentNumber,
    /// `Count_t` (§9.4.2.10).
    pub count: i32,
    /// What this peer said that this build does not interpret.
    pub extension: Extension,
}

impl HeartbeatFrag {
    /// The flag bits §8.3.7.6.1 defines for `HEARTBEAT_FRAG`.
    pub const DEFINED_FLAGS: u8 = flags::ENDIANNESS;

    /// A little-endian fragment heartbeat.
    #[must_use]
    pub const fn new(
        reader_id: EntityId,
        writer_id: EntityId,
        writer_sn: SequenceNumber,
        last_fragment_num: FragmentNumber,
        count: i32,
    ) -> Self {
        Self {
            endianness: Endianness::Little,
            reader_id,
            writer_id,
            writer_sn,
            last_fragment_num,
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
        HEARTBEAT_FRAG_BODY_LEN + self.extension.trailing_len()
    }

    /// Check the validity clauses of §8.3.7.6.3.
    ///
    /// # Errors
    ///
    /// [`RtpsError::InvalidSequenceNumber`] or
    /// [`RtpsError::InvalidFragmentNumber`] when `writerSN` or
    /// `lastFragmentNum` is not strictly positive.
    pub fn validate(&self) -> RtpsResult<()> {
        self.writer_sn.check_valid("HEARTBEAT_FRAG writerSN")?;
        self.last_fragment_num
            .check_valid("HEARTBEAT_FRAG lastFragmentNum")?;
        Ok(())
    }

    /// Write the body.
    ///
    /// # Errors
    ///
    /// [`RtpsError::Cdr`] from a field write.
    pub fn write_body(&self, writer: &mut CdrWriter) -> RtpsResult<()> {
        self.reader_id.serialize(writer)?;
        self.writer_id.serialize(writer)?;
        self.writer_sn.serialize(writer)?;
        self.last_fragment_num.serialize(writer)?;
        writer.write_i32(self.count)?;
        writer.write_octets(&self.extension.trailing);
        Ok(())
    }

    /// Read a body against the flags its header carried.
    ///
    /// # Errors
    ///
    /// [`RtpsError::Cdr`] when the body ends inside a field.
    pub fn read(header: &SubmessageHeader, body: &[u8]) -> RtpsResult<Self> {
        let endianness = header.endianness();
        let mut reader = CdrReader::with_encoding(body, body_encoding(endianness));
        let reader_id = EntityId::deserialize(&mut reader)?;
        let writer_id = EntityId::deserialize(&mut reader)?;
        let writer_sn = SequenceNumber::deserialize(&mut reader)?;
        let last_fragment_num = FragmentNumber::deserialize(&mut reader)?;
        let count = reader.read_i32()?;
        Ok(Self {
            endianness,
            reader_id,
            writer_id,
            writer_sn,
            last_fragment_num,
            count,
            extension: Extension {
                reserved_flags: header.flags.reserved(Self::DEFINED_FLAGS),
                trailing: reader.peek_remaining().to_vec(),
            },
        })
    }
}

impl fmt::Display for HeartbeatFrag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "HEARTBEAT_FRAG {} -> {} sn {} up to fragment {} count {}",
            self.writer_id, self.reader_id, self.writer_sn, self.last_fragment_num, self.count
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use astrs_cdr::Encoding;

    use super::*;
    use crate::messages::kind::SubmessageId;
    use crate::structure::guid::EntityKind;

    fn writer_id() -> EntityId {
        EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY)
    }

    fn reader_id() -> EntityId {
        EntityId::user_defined(2, EntityKind::USER_READER_NO_KEY)
    }

    #[test]
    fn a_heartbeat_body_is_twenty_eight_octets() {
        let heartbeat = Heartbeat::new(
            reader_id(),
            writer_id(),
            SequenceNumber::FIRST,
            SequenceNumber::new(5),
            7,
        );
        assert_eq!(heartbeat.flags().raw(), 0x01);
        assert_eq!(heartbeat.body_len(), HEARTBEAT_BODY_LEN);
        assert_eq!(heartbeat.available(), 5);
        assert!(!heartbeat.is_empty_history());
        assert_eq!(heartbeat.validate(), Ok(()));

        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        heartbeat.write_body(&mut writer).expect("write");
        assert_eq!(
            writer.finish(),
            [
                0x00, 0x00, 0x02, 0x04, // readerId
                0x00, 0x00, 0x01, 0x03, // writerId
                0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // firstSN = 1
                0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, // lastSN = 5
                0x07, 0x00, 0x00, 0x00, // count = 7
            ]
        );
        assert_eq!(
            heartbeat.to_string(),
            "HEARTBEAT 00.00.01.03 -> 00.00.02.04 [1, 5] count 7"
        );
    }

    #[test]
    fn a_writer_with_nothing_announces_last_below_first() {
        let empty = Heartbeat::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::FIRST,
            SequenceNumber::ZERO,
            1,
        );
        assert!(empty.is_empty_history());
        assert_eq!(empty.available(), 0);
        assert_eq!(empty.validate(), Ok(()));

        // One below that is not legal.
        let mut broken = empty.clone();
        broken.last_sn = SequenceNumber::new(-1);
        assert_eq!(
            broken.validate(),
            Err(RtpsError::InvalidHeartbeatRange { first: 1, last: -1 })
        );

        let mut zero_first = empty.clone();
        zero_first.first_sn = SequenceNumber::ZERO;
        assert_eq!(
            zero_first.validate(),
            Err(RtpsError::InvalidSequenceNumber {
                value: 0,
                context: "HEARTBEAT firstSN",
            })
        );
    }

    #[test]
    fn the_final_and_liveliness_flags_round_trip() {
        let heartbeat = Heartbeat::new(
            reader_id(),
            writer_id(),
            SequenceNumber::FIRST,
            SequenceNumber::new(3),
            2,
        )
        .finalized()
        .asserting_liveliness();
        assert_eq!(heartbeat.flags().raw(), 0x07);
        assert!(heartbeat.to_string().contains("final"));
        assert!(heartbeat.to_string().contains("liveliness"));

        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        heartbeat.write_body(&mut writer).expect("write");
        let header = SubmessageHeader::new(SubmessageId::Heartbeat, heartbeat.flags(), 0);
        assert_eq!(
            Heartbeat::read(&header, &writer.finish()).expect("read"),
            heartbeat
        );
    }

    #[test]
    fn a_big_endian_heartbeat_swaps_the_numbers_and_not_the_ids() {
        let heartbeat = Heartbeat::new(
            reader_id(),
            writer_id(),
            SequenceNumber::FIRST,
            SequenceNumber::new(0x0000_0001_0000_0002),
            0x0a0b_0c0d,
        )
        .with_endianness(Endianness::Big);
        assert_eq!(heartbeat.flags().raw(), 0x00);

        let mut writer = CdrWriter::headerless(body_encoding(Endianness::Big));
        heartbeat.write_body(&mut writer).expect("write");
        let body = writer.finish();
        assert_eq!(
            &body[..8],
            &[0x00, 0x00, 0x02, 0x04, 0x00, 0x00, 0x01, 0x03]
        );
        assert_eq!(&body[16..24], &[0, 0, 0, 1, 0, 0, 0, 2]);
        assert_eq!(&body[24..], &[0x0a, 0x0b, 0x0c, 0x0d]);

        let header = SubmessageHeader::new(SubmessageId::Heartbeat, heartbeat.flags(), 0);
        assert_eq!(Heartbeat::read(&header, &body).expect("read"), heartbeat);
    }

    #[test]
    fn a_version_extension_after_the_count_is_preserved_verbatim() {
        // A 2.3 sender with the group-info flag set appends fields this build
        // does not model. §8.6 says skip them; AstRS also keeps them, so a
        // forwarded heartbeat is octet-identical to the one received.
        let mut body = Vec::new();
        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        Heartbeat::new(
            reader_id(),
            writer_id(),
            SequenceNumber::FIRST,
            SequenceNumber::new(2),
            1,
        )
        .write_body(&mut writer)
        .expect("write");
        body.extend_from_slice(&writer.finish());
        body.extend_from_slice(&[0xaa; 32]);

        let header = SubmessageHeader::new(
            SubmessageId::Heartbeat,
            SubmessageFlags::new(flags::ENDIANNESS | 0x08),
            0,
        );
        let decoded = Heartbeat::read(&header, &body).expect("read");
        assert_eq!(decoded.extension.reserved_flags, 0x08);
        assert_eq!(decoded.extension.trailing.len(), 32);
        assert_eq!(decoded.body_len(), HEARTBEAT_BODY_LEN + 32);
        assert_eq!(decoded.flags().raw(), flags::ENDIANNESS | 0x08);

        let mut again = CdrWriter::headerless(Encoding::ROS2);
        decoded.write_body(&mut again).expect("write");
        assert_eq!(again.finish(), body);
    }

    #[test]
    fn a_truncated_heartbeat_is_a_truncation() {
        let header =
            SubmessageHeader::new(SubmessageId::Heartbeat, SubmessageFlags::LITTLE_ENDIAN, 0);
        let error = Heartbeat::read(&header, &[0_u8; 20]).expect_err("short");
        assert!(error.is_truncation());
    }

    #[test]
    fn a_heartbeat_frag_body_is_twenty_four_octets() {
        let frag = HeartbeatFrag::new(
            reader_id(),
            writer_id(),
            SequenceNumber::new(4),
            FragmentNumber::new(9),
            3,
        );
        assert_eq!(frag.flags().raw(), 0x01);
        assert_eq!(frag.body_len(), HEARTBEAT_FRAG_BODY_LEN);
        assert_eq!(frag.validate(), Ok(()));

        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        frag.write_body(&mut writer).expect("write");
        assert_eq!(
            writer.finish(),
            [
                0x00, 0x00, 0x02, 0x04, // readerId
                0x00, 0x00, 0x01, 0x03, // writerId
                0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, // writerSN = 4
                0x09, 0x00, 0x00, 0x00, // lastFragmentNum = 9
                0x03, 0x00, 0x00, 0x00, // count = 3
            ]
        );
        assert_eq!(
            frag.to_string(),
            "HEARTBEAT_FRAG 00.00.01.03 -> 00.00.02.04 sn 4 up to fragment 9 count 3"
        );
    }

    #[test]
    fn a_heartbeat_frag_needs_a_positive_sample_and_fragment() {
        let mut frag = HeartbeatFrag::new(
            reader_id(),
            writer_id(),
            SequenceNumber::ZERO,
            FragmentNumber::FIRST,
            1,
        );
        assert_eq!(
            frag.validate(),
            Err(RtpsError::InvalidSequenceNumber {
                value: 0,
                context: "HEARTBEAT_FRAG writerSN",
            })
        );
        frag.writer_sn = SequenceNumber::FIRST;
        frag.last_fragment_num = FragmentNumber::ZERO;
        assert_eq!(
            frag.validate(),
            Err(RtpsError::InvalidFragmentNumber {
                value: 0,
                context: "HEARTBEAT_FRAG lastFragmentNum",
            })
        );
    }

    #[test]
    fn a_heartbeat_frag_round_trips_in_both_byte_orders() {
        for endianness in [Endianness::Little, Endianness::Big] {
            let frag = HeartbeatFrag::new(
                reader_id(),
                writer_id(),
                SequenceNumber::new(0x0000_0003_0000_0004),
                FragmentNumber::new(0x0102_0304),
                -5,
            )
            .with_endianness(endianness);
            let mut writer = CdrWriter::headerless(body_encoding(endianness));
            frag.write_body(&mut writer).expect("write");
            let body = writer.finish();
            assert_eq!(body.len(), frag.body_len());
            let header = SubmessageHeader::new(SubmessageId::HeartbeatFrag, frag.flags(), 0);
            assert_eq!(HeartbeatFrag::read(&header, &body).expect("read"), frag);
        }
    }
}
