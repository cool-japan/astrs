//! The [`Submessage`] enum: one value for every shape a submessage takes.
//!
//! Decoding a submessage is a two-step affair, and the split matters. The
//! four-octet [`SubmessageHeader`] is read first, because it carries the
//! `EndiannessFlag` that every field of the body depends on and the
//! `octetsToNextHeader` that says where the body ends. Only then is the body
//! handed to the kind's own reader, which sees a slice of exactly the right
//! length and a byte order it can trust.
//!
//! ```
//! use astrs_rtps::messages::{Submessage, SubmessageId, InfoDestination};
//! use astrs_rtps::structure::GuidPrefix;
//!
//! let submessage = Submessage::InfoDestination(InfoDestination::new(GuidPrefix::new([3; 12])));
//! assert_eq!(submessage.id(), SubmessageId::InfoDestination);
//! assert_eq!(submessage.body_len(), 12);
//! assert_eq!(submessage.serialized_len(), 16);
//! assert!(submessage.is_interpreter());
//! ```
//!
//! # What is *not* decoded
//!
//! Two things arrive as [`Submessage::Opaque`], octets preserved and never
//! interpreted:
//!
//! - **Any unassigned or vendor-specific `submessageId`.** §8.3.4.1 requires
//!   a receiver to skip these and carry on with the next submessage, and
//!   keeping the octets means a forwarded message is byte-identical to the
//!   one that arrived.
//! - **`INFO_REPLY_IP4`.** Its `LocatorUDPv4_t` layout has no
//!   specification-independent source in this repository, so AstRS declines
//!   to guess. See [`info`](crate::messages::info).

use core::fmt;

use astrs_cdr::{CdrWriter, Endianness};

use crate::error::RtpsResult;
use crate::messages::acknack::{AckNack, NackFrag};
use crate::messages::data::{Data, DataFrag};
use crate::messages::flags::{self, SubmessageFlags, body_encoding};
use crate::messages::gap::Gap;
use crate::messages::header::{SUBMESSAGE_HEADER_LEN, SubmessageHeader};
use crate::messages::heartbeat::{Heartbeat, HeartbeatFrag};
use crate::messages::info::{InfoDestination, InfoReply, InfoSource, InfoTimestamp};
use crate::messages::kind::SubmessageId;

/// The `PAD` submessage (§8.3.7.11).
///
/// Exists to occupy space, which is why its body is octets and nothing else.
/// A `PAD` whose body is empty is written with `octetsToNextHeader == 0`, and
/// that zero means "empty" rather than "to the end of the message" — `PAD`
/// is one of the two kinds §8.3.3.2.3 exempts from the usual reading.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Pad {
    /// Byte order flag, preserved. No field of a `PAD` body is byte-order
    /// sensitive.
    pub endianness: Endianness,
    /// The padding octets themselves.
    pub padding: Vec<u8>,
    /// Flag bits this build does not interpret.
    pub reserved_flags: u8,
}

impl Pad {
    /// The flag bits §8.3.7.11.1 defines for `PAD`.
    pub const DEFINED_FLAGS: u8 = flags::ENDIANNESS;

    /// A `PAD` with no body at all.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            endianness: Endianness::Little,
            padding: Vec::new(),
            reserved_flags: 0,
        }
    }

    /// A `PAD` carrying `count` zero octets.
    ///
    /// A `PAD` costs its four-octet header plus its body, so the smallest
    /// space it can occupy is four octets and every larger one is a multiple
    /// of four — it cannot insert one, two or three octets of alignment.
    #[must_use]
    pub fn zeros(count: usize) -> Self {
        Self {
            endianness: Endianness::Little,
            padding: vec![0; count],
            reserved_flags: 0,
        }
    }

    /// The flags octet this submessage encodes to.
    #[must_use]
    pub const fn flags(&self) -> SubmessageFlags {
        SubmessageFlags::from_endianness(self.endianness).with(self.reserved_flags)
    }

    /// Octets the body occupies.
    #[must_use]
    pub fn body_len(&self) -> usize {
        self.padding.len()
    }

    /// Write the body.
    ///
    /// # Errors
    ///
    /// Never; the signature matches the other submessages so the dispatch in
    /// [`Submessage::write_body`] stays uniform.
    pub fn write_body(&self, writer: &mut CdrWriter) -> RtpsResult<()> {
        writer.write_octets(&self.padding);
        Ok(())
    }

    /// Read a body against the flags its header carried.
    ///
    /// # Errors
    ///
    /// Never; every octet of a `PAD` body is padding.
    pub fn read(header: &SubmessageHeader, body: &[u8]) -> RtpsResult<Self> {
        Ok(Self {
            endianness: header.endianness(),
            padding: body.to_vec(),
            reserved_flags: header.flags.reserved(Self::DEFINED_FLAGS),
        })
    }
}

impl Default for Pad {
    /// [`Pad::empty`]: a little-endian `PAD` with no body.
    fn default() -> Self {
        Self::empty()
    }
}

impl fmt::Display for Pad {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PAD {} octet(s)", self.padding.len())
    }
}

/// A submessage this build does not decode, kept octet for octet.
///
/// See the [module documentation](self) for what ends up here and why.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Opaque {
    /// The `submessageId` as received.
    pub id: SubmessageId,
    /// The flags octet as received.
    pub flags: SubmessageFlags,
    /// The body, verbatim.
    pub body: Vec<u8>,
}

impl Opaque {
    /// Wrap a body under a raw id and flags octet.
    #[must_use]
    pub fn new(id: SubmessageId, flags: SubmessageFlags, body: impl Into<Vec<u8>>) -> Self {
        Self {
            id,
            flags,
            body: body.into(),
        }
    }

    /// Octets the body occupies.
    #[must_use]
    pub fn body_len(&self) -> usize {
        self.body.len()
    }

    /// True when a receiver that does nothing further with this submessage is
    /// behaving correctly.
    ///
    /// Two cases, and they arrive here for different reasons:
    ///
    /// - **An unassigned or vendor id.** §8.3.4.1 *requires* it to be skipped.
    /// - **A DDS-Security id (`0x30`–`0x34`).** Skipping is what a participant
    ///   with no keys must do; one that has keys hands the octets to
    ///   [`crate::security`] instead, and that path never consults this.
    ///
    /// `INFO_REPLY_IP4` is the exception in the other direction: its id *is*
    /// assigned and AstRS simply does not decode its body, so a caller that
    /// understands `LocatorUDPv4_t` may still act on it.
    #[must_use]
    pub const fn must_be_ignored(&self) -> bool {
        !self.id.is_known() || self.id.is_security()
    }
}

impl fmt::Display for Opaque {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} flags {} ({} opaque octets)",
            self.id,
            self.flags,
            self.body.len()
        )
    }
}

/// One RTPS submessage.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Submessage<'a> {
    /// A sample, a key, or a sequence number being skipped.
    Data(Data<'a>),
    /// Fragments of a sample.
    DataFrag(DataFrag<'a>),
    /// What a writer holds.
    Heartbeat(Heartbeat),
    /// What fragments a writer holds.
    HeartbeatFrag(HeartbeatFrag),
    /// What a reader has and has not received.
    AckNack(AckNack),
    /// What fragments a reader has not received.
    NackFrag(NackFrag),
    /// Sequence numbers that will never arrive.
    Gap(Gap),
    /// The timestamp of the samples that follow.
    InfoTimestamp(InfoTimestamp),
    /// The participant the samples that follow come from.
    InfoSource(InfoSource),
    /// The participant the submessages that follow are addressed to.
    InfoDestination(InfoDestination),
    /// Where replies should be sent.
    InfoReply(InfoReply),
    /// Alignment filler.
    Pad(Pad),
    /// A submessage this build does not decode.
    Opaque(Opaque),
}

impl<'a> Submessage<'a> {
    /// The `submessageId` this submessage encodes to.
    #[must_use]
    pub const fn id(&self) -> SubmessageId {
        match self {
            Self::Data(_) => SubmessageId::Data,
            Self::DataFrag(_) => SubmessageId::DataFrag,
            Self::Heartbeat(_) => SubmessageId::Heartbeat,
            Self::HeartbeatFrag(_) => SubmessageId::HeartbeatFrag,
            Self::AckNack(_) => SubmessageId::AckNack,
            Self::NackFrag(_) => SubmessageId::NackFrag,
            Self::Gap(_) => SubmessageId::Gap,
            Self::InfoTimestamp(_) => SubmessageId::InfoTimestamp,
            Self::InfoSource(_) => SubmessageId::InfoSource,
            Self::InfoDestination(_) => SubmessageId::InfoDestination,
            Self::InfoReply(_) => SubmessageId::InfoReply,
            Self::Pad(_) => SubmessageId::Pad,
            Self::Opaque(opaque) => opaque.id,
        }
    }

    /// The flags octet this submessage encodes to.
    #[must_use]
    pub fn flags(&self) -> SubmessageFlags {
        match self {
            Self::Data(body) => body.flags(),
            Self::DataFrag(body) => body.flags(),
            Self::Heartbeat(body) => body.flags(),
            Self::HeartbeatFrag(body) => body.flags(),
            Self::AckNack(body) => body.flags(),
            Self::NackFrag(body) => body.flags(),
            Self::Gap(body) => body.flags(),
            Self::InfoTimestamp(body) => body.flags(),
            Self::InfoSource(body) => body.flags(),
            Self::InfoDestination(body) => body.flags(),
            Self::InfoReply(body) => body.flags(),
            Self::Pad(body) => body.flags(),
            Self::Opaque(opaque) => opaque.flags,
        }
    }

    /// The byte order of the body.
    #[must_use]
    pub fn endianness(&self) -> Endianness {
        self.flags().endianness()
    }

    /// Octets the body occupies, the four-octet header excluded.
    #[must_use]
    pub fn body_len(&self) -> usize {
        match self {
            Self::Data(body) => body.body_len(),
            Self::DataFrag(body) => body.body_len(),
            Self::Heartbeat(body) => body.body_len(),
            Self::HeartbeatFrag(body) => body.body_len(),
            Self::AckNack(body) => body.body_len(),
            Self::NackFrag(body) => body.body_len(),
            Self::Gap(body) => body.body_len(),
            Self::InfoTimestamp(body) => body.body_len(),
            Self::InfoSource(body) => body.body_len(),
            Self::InfoDestination(body) => body.body_len(),
            Self::InfoReply(body) => body.body_len(),
            Self::Pad(body) => body.body_len(),
            Self::Opaque(opaque) => opaque.body_len(),
        }
    }

    /// Octets the submessage occupies in a message, header included.
    #[must_use]
    pub fn serialized_len(&self) -> usize {
        SUBMESSAGE_HEADER_LEN + self.body_len()
    }

    /// True when the body length is a multiple of four, and another
    /// submessage may therefore follow this one (§8.3.3).
    #[must_use]
    pub fn is_aligned(&self) -> bool {
        self.body_len().is_multiple_of(4)
    }

    /// True for the submessages that address a specific reader and writer.
    #[must_use]
    pub const fn is_entity(&self) -> bool {
        self.id().is_entity_submessage()
    }

    /// True for the `INFO_*` family, which changes how the submessages after
    /// it are read.
    #[must_use]
    pub const fn is_interpreter(&self) -> bool {
        self.id().is_interpreter_submessage()
    }

    /// The `DATA` inside, if this is one.
    #[must_use]
    pub const fn as_data(&self) -> Option<&Data<'a>> {
        match self {
            Self::Data(body) => Some(body),
            _ => None,
        }
    }

    /// The `DATA_FRAG` inside, if this is one.
    #[must_use]
    pub const fn as_data_frag(&self) -> Option<&DataFrag<'a>> {
        match self {
            Self::DataFrag(body) => Some(body),
            _ => None,
        }
    }

    /// The `HEARTBEAT` inside, if this is one.
    #[must_use]
    pub const fn as_heartbeat(&self) -> Option<&Heartbeat> {
        match self {
            Self::Heartbeat(body) => Some(body),
            _ => None,
        }
    }

    /// The `ACKNACK` inside, if this is one.
    #[must_use]
    pub const fn as_acknack(&self) -> Option<&AckNack> {
        match self {
            Self::AckNack(body) => Some(body),
            _ => None,
        }
    }

    /// The `GAP` inside, if this is one.
    #[must_use]
    pub const fn as_gap(&self) -> Option<&Gap> {
        match self {
            Self::Gap(body) => Some(body),
            _ => None,
        }
    }

    /// Check the §8.3.7 validity clauses of whichever kind this is.
    ///
    /// The kinds with no clauses — the `INFO_*` family, `PAD`, an `Opaque` —
    /// always pass.
    ///
    /// # Errors
    ///
    /// Whatever the kind's own `validate` returns.
    pub fn validate(&self) -> RtpsResult<()> {
        match self {
            Self::Data(body) => body.validate(),
            Self::DataFrag(body) => body.validate(),
            Self::Heartbeat(body) => body.validate(),
            Self::HeartbeatFrag(body) => body.validate(),
            Self::AckNack(body) => body.validate(),
            Self::NackFrag(body) => body.validate(),
            Self::Gap(body) => body.validate(),
            Self::InfoTimestamp(_)
            | Self::InfoSource(_)
            | Self::InfoDestination(_)
            | Self::InfoReply(_)
            | Self::Pad(_)
            | Self::Opaque(_) => Ok(()),
        }
    }

    /// Write the body — everything after the four-octet submessage header.
    ///
    /// # Errors
    ///
    /// Whatever the kind's own `write_body` returns.
    pub fn write_body(&self, writer: &mut CdrWriter) -> RtpsResult<()> {
        match self {
            Self::Data(body) => body.write_body(writer),
            Self::DataFrag(body) => body.write_body(writer),
            Self::Heartbeat(body) => body.write_body(writer),
            Self::HeartbeatFrag(body) => body.write_body(writer),
            Self::AckNack(body) => body.write_body(writer),
            Self::NackFrag(body) => body.write_body(writer),
            Self::Gap(body) => body.write_body(writer),
            Self::InfoTimestamp(body) => body.write_body(writer),
            Self::InfoSource(body) => body.write_body(writer),
            Self::InfoDestination(body) => body.write_body(writer),
            Self::InfoReply(body) => body.write_body(writer),
            Self::Pad(body) => body.write_body(writer),
            Self::Opaque(opaque) => {
                writer.write_octets(&opaque.body);
                Ok(())
            }
        }
    }

    /// The body octets on their own, for a caller assembling a datagram by
    /// hand.
    ///
    /// # Errors
    ///
    /// Those of [`Submessage::write_body`].
    pub fn encode_body(&self) -> RtpsResult<Vec<u8>> {
        let mut writer = CdrWriter::headerless(body_encoding(self.endianness()));
        self.write_body(&mut writer)?;
        Ok(writer.finish())
    }

    /// Decode a submessage body against the header that introduced it.
    ///
    /// `body` must already be exactly the octets `octetsToNextHeader`
    /// delimited; resolving that is [`Message`](crate::messages::Message)'s
    /// job, because only it knows where the message ends.
    ///
    /// # Errors
    ///
    /// Whatever the kind's own `read` returns.
    pub fn read(header: &SubmessageHeader, body: &'a [u8]) -> RtpsResult<Self> {
        Ok(match header.id {
            SubmessageId::Data => Self::Data(Data::read(header, body)?),
            SubmessageId::DataFrag => Self::DataFrag(DataFrag::read(header, body)?),
            SubmessageId::Heartbeat => Self::Heartbeat(Heartbeat::read(header, body)?),
            SubmessageId::HeartbeatFrag => Self::HeartbeatFrag(HeartbeatFrag::read(header, body)?),
            SubmessageId::AckNack => Self::AckNack(AckNack::read(header, body)?),
            SubmessageId::NackFrag => Self::NackFrag(NackFrag::read(header, body)?),
            SubmessageId::Gap => Self::Gap(Gap::read(header, body)?),
            SubmessageId::InfoTimestamp => Self::InfoTimestamp(InfoTimestamp::read(header, body)?),
            SubmessageId::InfoSource => Self::InfoSource(InfoSource::read(header, body)?),
            SubmessageId::InfoDestination => {
                Self::InfoDestination(InfoDestination::read(header, body)?)
            }
            SubmessageId::InfoReply => Self::InfoReply(InfoReply::read(header, body)?),
            SubmessageId::Pad => Self::Pad(Pad::read(header, body)?),
            SubmessageId::InfoReplyIp4
            | SubmessageId::SecureBody
            | SubmessageId::SecurePrefix
            | SubmessageId::SecurePostfix
            | SubmessageId::SecureRtpsPrefix
            | SubmessageId::SecureRtpsPostfix
            | SubmessageId::Unknown(_) => Self::Opaque(Opaque::new(header.id, header.flags, body)),
        })
    }

    /// Detach every borrowed field from the input buffer.
    #[must_use]
    pub fn into_owned(self) -> Submessage<'static> {
        match self {
            Self::Data(body) => Submessage::Data(body.into_owned()),
            Self::DataFrag(body) => Submessage::DataFrag(body.into_owned()),
            Self::Heartbeat(body) => Submessage::Heartbeat(body),
            Self::HeartbeatFrag(body) => Submessage::HeartbeatFrag(body),
            Self::AckNack(body) => Submessage::AckNack(body),
            Self::NackFrag(body) => Submessage::NackFrag(body),
            Self::Gap(body) => Submessage::Gap(body),
            Self::InfoTimestamp(body) => Submessage::InfoTimestamp(body),
            Self::InfoSource(body) => Submessage::InfoSource(body),
            Self::InfoDestination(body) => Submessage::InfoDestination(body),
            Self::InfoReply(body) => Submessage::InfoReply(body),
            Self::Pad(body) => Submessage::Pad(body),
            Self::Opaque(opaque) => Submessage::Opaque(opaque),
        }
    }
}

impl fmt::Display for Submessage<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Data(body) => write!(f, "{body}"),
            Self::DataFrag(body) => write!(f, "{body}"),
            Self::Heartbeat(body) => write!(f, "{body}"),
            Self::HeartbeatFrag(body) => write!(f, "{body}"),
            Self::AckNack(body) => write!(f, "{body}"),
            Self::NackFrag(body) => write!(f, "{body}"),
            Self::Gap(body) => write!(f, "{body}"),
            Self::InfoTimestamp(body) => write!(f, "{body}"),
            Self::InfoSource(body) => write!(f, "{body}"),
            Self::InfoDestination(body) => write!(f, "{body}"),
            Self::InfoReply(body) => write!(f, "{body}"),
            Self::Pad(body) => write!(f, "{body}"),
            Self::Opaque(opaque) => write!(f, "{opaque}"),
        }
    }
}

macro_rules! submessage_from {
    ($variant:ident, $body:ty) => {
        impl<'a> From<$body> for Submessage<'a> {
            fn from(body: $body) -> Self {
                Self::$variant(body)
            }
        }
    };
}

submessage_from!(Data, Data<'a>);
submessage_from!(DataFrag, DataFrag<'a>);
submessage_from!(Heartbeat, Heartbeat);
submessage_from!(HeartbeatFrag, HeartbeatFrag);
submessage_from!(AckNack, AckNack);
submessage_from!(NackFrag, NackFrag);
submessage_from!(Gap, Gap);
submessage_from!(InfoTimestamp, InfoTimestamp);
submessage_from!(InfoSource, InfoSource);
submessage_from!(InfoDestination, InfoDestination);
submessage_from!(InfoReply, InfoReply);
submessage_from!(Pad, Pad);
submessage_from!(Opaque, Opaque);

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::messages::data::FragmentGeometry;
    use crate::messages::payload::SerializedPayload;
    use crate::structure::guid::{EntityId, EntityKind, GuidPrefix};
    use crate::structure::sequence::{SequenceNumber, SequenceNumberSet};
    use crate::structure::time::Time;

    fn every_kind() -> Vec<Submessage<'static>> {
        let writer = EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY);
        let reader = EntityId::user_defined(2, EntityKind::USER_READER_NO_KEY);
        Vec::from([
            Submessage::Data(crate::messages::data::Data::new(
                reader,
                writer,
                SequenceNumber::FIRST,
                crate::messages::data::DataPayload::Data(
                    SerializedPayload::from_cdr(&1_u32).expect("encode"),
                ),
            )),
            Submessage::DataFrag(DataFrag::new(
                reader,
                writer,
                SequenceNumber::FIRST,
                FragmentGeometry::new(crate::structure::fragment::FragmentNumber::FIRST, 1, 8, 8),
                SerializedPayload::new(vec![0_u8; 8]),
            )),
            Submessage::Heartbeat(Heartbeat::new(
                reader,
                writer,
                SequenceNumber::FIRST,
                SequenceNumber::new(3),
                1,
            )),
            Submessage::HeartbeatFrag(HeartbeatFrag::new(
                reader,
                writer,
                SequenceNumber::FIRST,
                crate::structure::fragment::FragmentNumber::FIRST,
                1,
            )),
            Submessage::AckNack(AckNack::new(
                reader,
                writer,
                SequenceNumberSet::new(SequenceNumber::FIRST),
                1,
            )),
            Submessage::NackFrag(NackFrag::new(
                reader,
                writer,
                SequenceNumber::FIRST,
                crate::structure::fragment::FragmentNumberSet::new(
                    crate::structure::fragment::FragmentNumber::FIRST,
                ),
                1,
            )),
            Submessage::Gap(Gap::contiguous(
                reader,
                writer,
                SequenceNumber::FIRST,
                SequenceNumber::new(2),
            )),
            Submessage::InfoTimestamp(InfoTimestamp::at(Time::new(1, 2))),
            Submessage::InfoSource(InfoSource::new(GuidPrefix::new([1; 12]))),
            Submessage::InfoDestination(InfoDestination::new(GuidPrefix::new([2; 12]))),
            Submessage::InfoReply(InfoReply::new(crate::structure::locator::LocatorList::new())),
            Submessage::Pad(Pad::zeros(4)),
            Submessage::Opaque(Opaque::new(
                SubmessageId::from_raw(0x90),
                SubmessageFlags::LITTLE_ENDIAN,
                vec![1_u8, 2, 3, 4],
            )),
        ])
    }

    #[test]
    fn every_variant_round_trips_through_its_own_reader() {
        for submessage in every_kind() {
            let body = submessage.encode_body().expect("encode");
            assert_eq!(
                body.len(),
                submessage.body_len(),
                "{submessage}: body_len disagrees with the octets written"
            );
            assert_eq!(submessage.serialized_len(), body.len() + 4, "{submessage}");
            let header = SubmessageHeader::new(
                submessage.id(),
                submessage.flags(),
                u16::try_from(body.len()).expect("small"),
            );
            let decoded = Submessage::read(&header, &body).expect("read");
            assert_eq!(decoded, submessage, "{submessage} did not round-trip");
            assert_eq!(decoded.into_owned(), submessage);
        }
    }

    #[test]
    fn every_variant_reports_its_own_id_and_family() {
        for submessage in every_kind() {
            let id = submessage.id();
            assert_eq!(submessage.is_entity(), id.is_entity_submessage());
            assert_eq!(submessage.is_interpreter(), id.is_interpreter_submessage());
            assert_eq!(submessage.endianness(), submessage.flags().endianness());
            assert!(
                submessage.is_aligned(),
                "{submessage}: every constructed submessage should be aligned"
            );
        }
    }

    #[test]
    fn the_accessors_return_only_their_own_variant() {
        let submessages = every_kind();
        assert_eq!(
            submessages.iter().filter(|s| s.as_data().is_some()).count(),
            1
        );
        assert_eq!(
            submessages
                .iter()
                .filter(|s| s.as_data_frag().is_some())
                .count(),
            1
        );
        assert_eq!(
            submessages
                .iter()
                .filter(|s| s.as_heartbeat().is_some())
                .count(),
            1
        );
        assert_eq!(
            submessages
                .iter()
                .filter(|s| s.as_acknack().is_some())
                .count(),
            1
        );
        assert_eq!(
            submessages.iter().filter(|s| s.as_gap().is_some()).count(),
            1
        );
    }

    #[test]
    fn validation_dispatches_to_the_kind_that_has_clauses() {
        for submessage in every_kind() {
            assert_eq!(submessage.validate(), Ok(()), "{submessage}");
        }
        let broken = Submessage::Heartbeat(Heartbeat::new(
            EntityId::UNKNOWN,
            EntityId::UNKNOWN,
            SequenceNumber::ZERO,
            SequenceNumber::ZERO,
            1,
        ));
        assert!(broken.validate().is_err());
    }

    #[test]
    fn an_unassigned_id_is_kept_opaque_and_must_be_ignored() {
        let header = SubmessageHeader::new(
            SubmessageId::from_raw(0x42),
            SubmessageFlags::LITTLE_ENDIAN,
            4,
        );
        let decoded = Submessage::read(&header, &[9_u8, 8, 7, 6]).expect("read");
        let Submessage::Opaque(opaque) = &decoded else {
            unreachable!("an unassigned id decodes opaque")
        };
        assert!(opaque.must_be_ignored());
        assert_eq!(opaque.body, [9, 8, 7, 6]);
        assert_eq!(decoded.encode_body().expect("encode"), [9, 8, 7, 6]);
        assert_eq!(
            opaque.to_string(),
            "UNKNOWN(0x42) flags 0x01 (4 opaque octets)"
        );
    }

    #[test]
    fn info_reply_ip4_is_assigned_but_still_opaque() {
        let header = SubmessageHeader::new(
            SubmessageId::InfoReplyIp4,
            SubmessageFlags::LITTLE_ENDIAN,
            8,
        );
        let decoded = Submessage::read(&header, &[0_u8; 8]).expect("read");
        let Submessage::Opaque(opaque) = &decoded else {
            unreachable!("INFO_REPLY_IP4 is not decoded")
        };
        assert!(
            !opaque.must_be_ignored(),
            "the id is assigned, just not decoded"
        );
        assert_eq!(decoded.id(), SubmessageId::InfoReplyIp4);
        assert_eq!(decoded.body_len(), 8);
    }

    #[test]
    fn a_pad_carries_its_padding_and_can_be_empty() {
        let empty = Pad::empty();
        assert_eq!(empty.body_len(), 0);
        assert_eq!(empty.flags().raw(), 0x01);
        assert_eq!(empty.to_string(), "PAD 0 octet(s)");
        assert_eq!(Pad::default().padding, Vec::<u8>::new());

        let filler = Pad::zeros(8);
        assert_eq!(filler.body_len(), 8);
        let header = SubmessageHeader::new(SubmessageId::Pad, filler.flags(), 8);
        assert_eq!(Pad::read(&header, &[0_u8; 8]).expect("read"), filler);
    }

    #[test]
    fn the_from_impls_cover_every_body_type() {
        let submessage: Submessage<'_> = Pad::empty().into();
        assert_eq!(submessage.id(), SubmessageId::Pad);
        let submessage: Submessage<'_> = InfoDestination::new(GuidPrefix::UNKNOWN).into();
        assert_eq!(submessage.id(), SubmessageId::InfoDestination);
        let submessage: Submessage<'_> = Opaque::new(
            SubmessageId::from_raw(0xf0),
            SubmessageFlags::NONE,
            Vec::new(),
        )
        .into();
        assert_eq!(submessage.id().raw(), 0xf0);
    }
}
