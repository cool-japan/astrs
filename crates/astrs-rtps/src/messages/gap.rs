//! `GAP`: the sequence numbers a writer will never deliver.
//!
//! ```text
//!  GAP (§8.3.7.4)
//! +---------------------------+
//! |         readerId          |
//! |         writerId          |
//! |         gapStart (8)      |
//! ~      gapList (12..44)     ~
//! +---------------------------+
//! ```
//!
//! A `GAP` names two sets at once, and a reader must treat both as
//! irrelevant:
//!
//! 1. The **contiguous run** `gapStart ..= gapList.bitmapBase - 1`.
//! 2. The **individual numbers** whose bits are set in `gapList`.
//!
//! Without it a reliable reader would wait forever for a sample the writer
//! filtered out, dropped from its history, or never wrote — a content filter
//! rejecting a sample, a `KEEP_LAST` history discarding an old one, a
//! sequence number burned on an instance the reader does not subscribe to.
//!
//! [`Gap::irrelevant`] iterates both sets in one ascending sequence, which is
//! what a reader's state machine actually wants.
//!
//! ```
//! use astrs_rtps::messages::Gap;
//! use astrs_rtps::structure::{EntityId, EntityKind, SequenceNumber, SequenceNumberSet};
//!
//! // "3, 4 and 5 are gone, and so is 8."
//! let gap = Gap::new(
//!     EntityId::UNKNOWN,
//!     EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
//!     SequenceNumber::new(3),
//!     SequenceNumberSet::from_numbers(SequenceNumber::new(6), [SequenceNumber::new(8)])?,
//! );
//! gap.validate()?;
//! let gone: Vec<i64> = gap.irrelevant().map(SequenceNumber::value).collect();
//! assert_eq!(gone, [3, 4, 5, 8]);
//! # Ok::<(), astrs_rtps::RtpsError>(())
//! ```

use core::fmt;

use astrs_cdr::{CdrDeserialize, CdrReader, CdrSerialize, CdrWriter, Endianness};

use crate::error::RtpsResult;
use crate::messages::flags::{self, Extension, SubmessageFlags, body_encoding};
use crate::messages::header::SubmessageHeader;
use crate::structure::guid::EntityId;
use crate::structure::sequence::{SequenceNumber, SequenceNumberSet};

/// Octets a `GAP` occupies besides its `gapList`.
///
/// `readerId` + `writerId` + `gapStart`.
pub const GAP_FIXED_LEN: usize = 16;

/// The `GAP` submessage (§8.3.7.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gap {
    /// Byte order of the body.
    pub endianness: Endianness,
    /// The reader addressed, or [`EntityId::UNKNOWN`] for all of them.
    pub reader_id: EntityId,
    /// The writer declaring the gap.
    pub writer_id: EntityId,
    /// First number of the contiguous irrelevant run.
    pub gap_start: SequenceNumber,
    /// The end of the run (its `bitmapBase`) and the scattered numbers after
    /// it.
    pub gap_list: SequenceNumberSet,
    /// What this peer said that this build does not interpret.
    ///
    /// The RTPS 2.3 group-info fields land here; see
    /// [`flags`].
    pub extension: Extension,
}

impl Gap {
    /// The flag bits this build interprets for `GAP`.
    pub const DEFINED_FLAGS: u8 = flags::ENDIANNESS;

    /// A little-endian gap.
    #[must_use]
    pub const fn new(
        reader_id: EntityId,
        writer_id: EntityId,
        gap_start: SequenceNumber,
        gap_list: SequenceNumberSet,
    ) -> Self {
        Self {
            endianness: Endianness::Little,
            reader_id,
            writer_id,
            gap_start,
            gap_list,
            extension: Extension::EMPTY,
        }
    }

    /// A gap covering exactly the contiguous run `start ..= end`.
    ///
    /// The common case: a writer that dropped a stretch of its history. The
    /// `gapList` is the empty set based one past `end`, which is what makes
    /// the run alone the whole message.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_rtps::messages::Gap;
    /// use astrs_rtps::structure::{EntityId, SequenceNumber};
    ///
    /// let gap = Gap::contiguous(
    ///     EntityId::UNKNOWN,
    ///     EntityId::UNKNOWN,
    ///     SequenceNumber::new(2),
    ///     SequenceNumber::new(4),
    /// );
    /// let gone: Vec<i64> = gap.irrelevant().map(SequenceNumber::value).collect();
    /// assert_eq!(gone, [2, 3, 4]);
    /// ```
    #[must_use]
    pub fn contiguous(
        reader_id: EntityId,
        writer_id: EntityId,
        start: SequenceNumber,
        end: SequenceNumber,
    ) -> Self {
        Self::new(
            reader_id,
            writer_id,
            start,
            SequenceNumberSet::new(end.next()),
        )
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
        GAP_FIXED_LEN + self.gap_list.serialized_len() + self.extension.trailing_len()
    }

    /// One past the last number of the contiguous run: `gapList.bitmapBase`.
    #[must_use]
    pub fn contiguous_end(&self) -> SequenceNumber {
        self.gap_list.base()
    }

    /// Every sequence number this gap declares irrelevant, ascending.
    ///
    /// The contiguous run first, then the numbers the bitmap names.
    pub fn irrelevant(&self) -> impl Iterator<Item = SequenceNumber> + '_ {
        let start = self.gap_start.value();
        let end = self.gap_list.base().value();
        (start..end)
            .map(SequenceNumber::new)
            .chain(self.gap_list.iter())
    }

    /// True when `number` is one of the numbers this gap declares irrelevant.
    #[must_use]
    pub fn covers(&self, number: SequenceNumber) -> bool {
        (self.gap_start <= number && number < self.gap_list.base())
            || self.gap_list.contains(number)
    }

    /// Check the validity clauses of §8.3.7.4.3.
    ///
    /// # Errors
    ///
    /// - [`RtpsError::InvalidSequenceNumber`](crate::RtpsError::InvalidSequenceNumber)
    ///   when `gapStart` is not strictly positive.
    /// - [`RtpsError::InvalidSequenceNumberSet`](crate::RtpsError::InvalidSequenceNumberSet)
    ///   when `gapList` is invalid.
    pub fn validate(&self) -> RtpsResult<()> {
        self.gap_start.check_valid("GAP gapStart")?;
        self.gap_list.validate("GAP gapList")
    }

    /// Write the body.
    ///
    /// # Errors
    ///
    /// [`RtpsError::Cdr`](crate::RtpsError::Cdr) from a field write.
    pub fn write_body(&self, writer: &mut CdrWriter) -> RtpsResult<()> {
        self.reader_id.serialize(writer)?;
        self.writer_id.serialize(writer)?;
        self.gap_start.serialize(writer)?;
        self.gap_list.write(writer)?;
        writer.write_octets(&self.extension.trailing);
        Ok(())
    }

    /// Read a body against the flags its header carried.
    ///
    /// # Errors
    ///
    /// - [`RtpsError::InvalidSequenceNumberSet`](crate::RtpsError::InvalidSequenceNumberSet)
    ///   when `gapList` declares more than 256 bits.
    /// - [`RtpsError::Cdr`](crate::RtpsError::Cdr) when the body ends inside
    ///   a field.
    pub fn read(header: &SubmessageHeader, body: &[u8]) -> RtpsResult<Self> {
        let endianness = header.endianness();
        let mut reader = CdrReader::with_encoding(body, body_encoding(endianness));
        let reader_id = EntityId::deserialize(&mut reader)?;
        let writer_id = EntityId::deserialize(&mut reader)?;
        let gap_start = SequenceNumber::deserialize(&mut reader)?;
        let gap_list = SequenceNumberSet::read(&mut reader, "GAP gapList")?;
        Ok(Self {
            endianness,
            reader_id,
            writer_id,
            gap_start,
            gap_list,
            extension: Extension {
                reserved_flags: header.flags.reserved(Self::DEFINED_FLAGS),
                trailing: reader.peek_remaining().to_vec(),
            },
        })
    }
}

impl fmt::Display for Gap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "GAP {} -> {} run [{}, {}) plus {} scattered",
            self.writer_id,
            self.reader_id,
            self.gap_start,
            self.contiguous_end(),
            self.gap_list.len()
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
    use crate::structure::guid::EntityKind;

    fn writer_id() -> EntityId {
        EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY)
    }

    #[test]
    fn a_contiguous_gap_is_a_run_and_an_empty_bitmap() {
        let gap = Gap::contiguous(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::new(2),
            SequenceNumber::new(4),
        );
        assert_eq!(gap.contiguous_end(), SequenceNumber::new(5));
        assert_eq!(gap.flags().raw(), 0x01);
        assert_eq!(gap.body_len(), 16 + 12);
        assert_eq!(gap.validate(), Ok(()));

        let gone: Vec<i64> = gap.irrelevant().map(SequenceNumber::value).collect();
        assert_eq!(gone, [2, 3, 4]);
        for number in [2_i64, 3, 4] {
            assert!(gap.covers(SequenceNumber::new(number)));
        }
        assert!(!gap.covers(SequenceNumber::new(1)));
        assert!(!gap.covers(SequenceNumber::new(5)));

        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        gap.write_body(&mut writer).expect("write");
        assert_eq!(
            writer.finish(),
            [
                0x00, 0x00, 0x00, 0x00, // readerId = ENTITYID_UNKNOWN
                0x00, 0x00, 0x01, 0x03, // writerId
                0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, // gapStart = 2
                0x00, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00, // bitmapBase = 5
                0x00, 0x00, 0x00, 0x00, // numBits = 0
            ]
        );
        assert_eq!(
            gap.to_string(),
            "GAP 00.00.01.03 -> ENTITYID_UNKNOWN run [2, 5) plus 0 scattered"
        );
    }

    #[test]
    fn a_gap_names_a_run_and_scattered_numbers_at_once() {
        let gap = Gap::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::new(3),
            SequenceNumberSet::from_numbers(
                SequenceNumber::new(6),
                [SequenceNumber::new(8), SequenceNumber::new(11)],
            )
            .expect("in window"),
        );
        let gone: Vec<i64> = gap.irrelevant().map(SequenceNumber::value).collect();
        assert_eq!(gone, [3, 4, 5, 8, 11]);
        assert!(gap.covers(SequenceNumber::new(5)));
        assert!(!gap.covers(SequenceNumber::new(6)));
        assert!(gap.covers(SequenceNumber::new(8)));
        assert!(!gap.covers(SequenceNumber::new(9)));
        assert_eq!(gap.gap_list.len(), 2);
        assert_eq!(gap.body_len(), 16 + 16);
    }

    #[test]
    fn an_empty_run_is_legal_when_the_bitmap_carries_everything() {
        // gapStart == bitmapBase: the run is empty.
        let gap = Gap::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::new(9),
            SequenceNumberSet::from_numbers(SequenceNumber::new(9), [SequenceNumber::new(10)])
                .expect("in window"),
        );
        let gone: Vec<i64> = gap.irrelevant().map(SequenceNumber::value).collect();
        assert_eq!(gone, [10]);
        assert_eq!(gap.validate(), Ok(()));
    }

    #[test]
    fn validity_covers_both_the_start_and_the_list() {
        let mut gap = Gap::contiguous(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::ZERO,
            SequenceNumber::new(3),
        );
        assert_eq!(
            gap.validate(),
            Err(RtpsError::InvalidSequenceNumber {
                value: 0,
                context: "GAP gapStart",
            })
        );
        gap.gap_start = SequenceNumber::FIRST;
        gap.gap_list = SequenceNumberSet::new(SequenceNumber::ZERO);
        assert_eq!(
            gap.validate(),
            Err(RtpsError::InvalidSequenceNumberSet {
                reason: SetDefect::BaseNotPositive,
                context: "GAP gapList",
            })
        );
    }

    #[test]
    fn a_gap_round_trips_in_both_byte_orders() {
        for endianness in [Endianness::Little, Endianness::Big] {
            let gap = Gap::new(
                EntityId::UNKNOWN,
                writer_id(),
                SequenceNumber::new(0x0000_0001_0000_0002),
                SequenceNumberSet::from_numbers(
                    SequenceNumber::new(0x0000_0001_0000_0010),
                    [SequenceNumber::new(0x0000_0001_0000_0020)],
                )
                .expect("in window"),
            )
            .with_endianness(endianness);
            let mut writer = CdrWriter::headerless(body_encoding(endianness));
            gap.write_body(&mut writer).expect("write");
            let body = writer.finish();
            assert_eq!(body.len(), gap.body_len());
            let header = SubmessageHeader::new(SubmessageId::Gap, gap.flags(), 0);
            assert_eq!(Gap::read(&header, &body).expect("read"), gap);
        }
    }

    #[test]
    fn a_version_extension_after_the_gap_list_is_preserved() {
        let gap = Gap::contiguous(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::FIRST,
            SequenceNumber::new(2),
        );
        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        gap.write_body(&mut writer).expect("write");
        let mut body = writer.finish();
        body.extend_from_slice(&[0x5a; 16]);

        let header = SubmessageHeader::new(
            SubmessageId::Gap,
            SubmessageFlags::new(flags::ENDIANNESS | 0x08),
            0,
        );
        let decoded = Gap::read(&header, &body).expect("read");
        assert_eq!(decoded.extension.reserved_flags, 0x08);
        assert_eq!(decoded.extension.trailing.len(), 16);
        assert_eq!(decoded.body_len(), body.len());

        let mut again = CdrWriter::headerless(Encoding::ROS2);
        decoded.write_body(&mut again).expect("write");
        assert_eq!(again.finish(), body);
    }

    #[test]
    fn a_truncated_gap_is_a_truncation() {
        let header = SubmessageHeader::new(SubmessageId::Gap, SubmessageFlags::LITTLE_ENDIAN, 0);
        let error = Gap::read(&header, &[0_u8; 20]).expect_err("short");
        assert!(error.is_truncation());
    }
}
