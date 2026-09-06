//! [`Message`]: a twenty-octet header and the submessages behind it.
//!
//! # Decoding
//!
//! ```text
//! offset 0    Header (20)              -- version and sender
//! offset 20   SubmessageHeader (4)     -- id, flags, octetsToNextHeader
//!             body (octetsToNextHeader)
//!             SubmessageHeader (4)
//!             body
//!             ...
//! ```
//!
//! Three rules make the loop above total, and all three are enforced here
//! rather than in the individual submessages, because only this layer knows
//! where the message ends:
//!
//! 1. **Every submessage starts four-octet aligned** (§8.3.3). A sender whose
//!    `octetsToNextHeader` leaves the next header at an odd offset gets
//!    [`RtpsError::MisalignedSubmessage`] — parsing stops rather than
//!    mis-reading everything that follows.
//! 2. **`octetsToNextHeader == 0` has two meanings** (§8.3.3.2.3), resolved
//!    by [`SubmessageHeader::body_extent`].
//! 3. **An unknown `submessageId` is skipped, not fatal** (§8.3.4.1). It
//!    arrives as [`Submessage::Opaque`], so the message still decodes and the
//!    octets survive a re-encode.
//!
//! # Encoding
//!
//! [`Message::encode`] is the exact inverse, with one thing it refuses:
//! a submessage whose body is not a multiple of four cannot be followed by
//! another, because there would be no way to put the next header on a
//! four-octet boundary without lengthening the body. That is
//! [`RtpsError::UnalignedBody`], and it can only be reached by a `DATA` or
//! `DATA_FRAG` whose payload was not padded — `astrs-cdr` pads its own, and
//! records the pad count in the encapsulation options, so a payload AstRS
//! built never trips it.
//!
//! The contract this buys is the one the property tests assert: **whenever
//! `encode` succeeds, `decode` of its output reproduces the message
//! exactly.**
//!
//! ```
//! use astrs_rtps::messages::{Data, DataPayload, Header, Message, SerializedPayload};
//! use astrs_rtps::structure::{EntityId, EntityKind, GuidPrefix, SequenceNumber};
//!
//! let message = Message::new(Header::new(GuidPrefix::new([1; 12]))).with(Data::new(
//!     EntityId::UNKNOWN,
//!     EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
//!     SequenceNumber::FIRST,
//!     DataPayload::Data(SerializedPayload::from_cdr(&7_i32)?),
//! ));
//!
//! let datagram = message.encode()?;
//! assert_eq!(datagram.len(), 20 + 4 + 28);
//! assert_eq!(Message::decode(&datagram)?, message);
//! # Ok::<(), astrs_rtps::RtpsError>(())
//! ```

use core::fmt;

use astrs_cdr::CdrWriter;

use crate::error::{RtpsError, RtpsResult};
use crate::messages::flags::body_encoding;
use crate::messages::header::{
    BodyExtent, HEADER_LEN, Header, SUBMESSAGE_ALIGNMENT, SUBMESSAGE_HEADER_LEN, SubmessageHeader,
};
use crate::messages::submessage::Submessage;
use crate::structure::guid::GuidPrefix;

/// The largest number of submessages [`Message::decode`] accepts.
///
/// The smallest submessage is a four-octet header with an empty body, so a
/// 64 KiB datagram cannot hold more than 16 383 of them; this ceiling makes
/// that bound explicit instead of incidental, and keeps a transport with a
/// larger MTU from turning a crafted datagram into an unbounded `Vec`.
pub const MAX_SUBMESSAGES: usize = 16_384;

/// The largest payload a UDPv4 datagram can carry.
///
/// `65535 - 20 (IP header) - 8 (UDP header)`.
pub const MAX_UDP_PAYLOAD: usize = 65_507;

/// A conservative per-datagram budget for a path with an Ethernet MTU.
///
/// 1500 octets of MTU less a 20-octet IPv4 header and an 8-octet UDP header
/// is 1472; AstRS leaves further room for a tunnel or a VLAN tag rather than
/// discovering the path MTU the hard way. The behavior half fragments a
/// sample that does not fit.
pub const DEFAULT_DATAGRAM_BUDGET: usize = 1_400;

/// An RTPS message: one header, then submessages.
#[derive(Debug, Clone, PartialEq)]
pub struct Message<'a> {
    /// Who sent it, and in what version.
    pub header: Header,
    /// The submessages, in wire order.
    pub submessages: Vec<Submessage<'a>>,
}

impl<'a> Message<'a> {
    /// An empty message with the given header.
    #[must_use]
    pub const fn new(header: Header) -> Self {
        Self {
            header,
            submessages: Vec::new(),
        }
    }

    /// An empty message from an AstRS participant.
    #[must_use]
    pub const fn from_participant(guid_prefix: GuidPrefix) -> Self {
        Self::new(Header::new(guid_prefix))
    }

    /// Append a submessage, taking `self` by value so a message can be built
    /// in one expression.
    #[must_use]
    pub fn with(mut self, submessage: impl Into<Submessage<'a>>) -> Self {
        self.submessages.push(submessage.into());
        self
    }

    /// Append a submessage in place.
    pub fn push(&mut self, submessage: impl Into<Submessage<'a>>) {
        self.submessages.push(submessage.into());
    }

    /// How many submessages the message carries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.submessages.len()
    }

    /// True when the message carries no submessages.
    ///
    /// Legal on the wire — a bare header is a valid, if useless, RTPS
    /// message — and the shape a builder starts from.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.submessages.is_empty()
    }

    /// Iterate over the submessages.
    pub fn iter(&self) -> core::slice::Iter<'_, Submessage<'a>> {
        self.submessages.iter()
    }

    /// Octets the encoded message occupies.
    #[must_use]
    pub fn serialized_len(&self) -> usize {
        HEADER_LEN
            + self
                .submessages
                .iter()
                .map(Submessage::serialized_len)
                .sum::<usize>()
    }

    /// Check every submessage's §8.3.7 validity clauses.
    ///
    /// Decoding deliberately does *not* call this: §8.3.4.1 asks a receiver
    /// to discard the offending **submessage**, not the datagram, so the
    /// behavior half validates each one as it dispatches it and keeps going.
    /// This is the whole-message form, for tests and for a strict mode.
    ///
    /// # Errors
    ///
    /// The first failure, with the index of the submessage that produced it
    /// unavailable — use [`Message::iter`] and
    /// [`Submessage::validate`] when the index matters.
    pub fn validate(&self) -> RtpsResult<()> {
        for submessage in &self.submessages {
            submessage.validate()?;
        }
        Ok(())
    }

    /// Detach every borrowed field from the input buffer.
    #[must_use]
    pub fn into_owned(self) -> Message<'static> {
        Message {
            header: self.header,
            submessages: self
                .submessages
                .into_iter()
                .map(Submessage::into_owned)
                .collect(),
        }
    }

    /// Encode the whole message.
    ///
    /// # Errors
    ///
    /// - [`RtpsError::UnalignedBody`] when a submessage that is not last has
    ///   a body length that is not a multiple of four. See the [module
    ///   documentation](self).
    /// - [`RtpsError::EmptyBodyNotLast`] when a submessage that is not last
    ///   has an empty body on a kind where `octetsToNextHeader == 0` would be
    ///   read as "to the end of the message".
    /// - [`RtpsError::TooLong`] when a body exceeds 65 535 octets and is not
    ///   last.
    /// - [`RtpsError::Cdr`] from a submessage's own writer.
    pub fn encode(&self) -> RtpsResult<Vec<u8>> {
        let mut output = Vec::with_capacity(self.serialized_len());
        self.encode_into(&mut output)?;
        Ok(output)
    }

    /// Encode the message onto the end of `output`.
    ///
    /// The form the send path uses, so one buffer can be reused across
    /// datagrams.
    ///
    /// # Errors
    ///
    /// Those of [`Message::encode`].
    pub fn encode_into(&self, output: &mut Vec<u8>) -> RtpsResult<()> {
        self.header.encode_into(output);
        let last = self.submessages.len().saturating_sub(1);
        for (index, submessage) in self.submessages.iter().enumerate() {
            let is_last = index == last;
            let mut writer = CdrWriter::headerless(body_encoding(submessage.endianness()));
            submessage.write_body(&mut writer)?;
            let body = writer.finish();
            if !is_last {
                if !body.len().is_multiple_of(SUBMESSAGE_ALIGNMENT) {
                    return Err(RtpsError::UnalignedBody {
                        id: submessage.id().raw(),
                        body_len: body.len(),
                    });
                }
                if body.is_empty() && !submessage.id().allows_empty_body() {
                    return Err(RtpsError::EmptyBodyNotLast {
                        id: submessage.id().raw(),
                    });
                }
            }
            let octets = SubmessageHeader::octets_for(submessage.id(), body.len(), is_last)?;
            SubmessageHeader::new(submessage.id(), submessage.flags(), octets).encode_into(output);
            output.extend_from_slice(&body);
        }
        Ok(())
    }

    /// Decode a whole datagram.
    ///
    /// # Errors
    ///
    /// - Those of [`Header::decode`].
    /// - [`RtpsError::MisalignedSubmessage`],
    ///   [`RtpsError::SubmessageOverrun`], [`RtpsError::Truncated`] from the
    ///   framing.
    /// - [`RtpsError::TooManySubmessages`] past [`MAX_SUBMESSAGES`].
    /// - Whatever a submessage's own reader returns.
    pub fn decode(bytes: &'a [u8]) -> RtpsResult<Self> {
        let (header, rest) = Header::decode_prefix(bytes)?;
        let mut submessages = Vec::new();
        for submessage in SubmessageIter::new(rest, HEADER_LEN) {
            if submessages.len() == MAX_SUBMESSAGES {
                return Err(RtpsError::TooManySubmessages {
                    maximum: MAX_SUBMESSAGES,
                });
            }
            submessages.push(submessage?);
        }
        Ok(Self {
            header,
            submessages,
        })
    }

    /// Decode the header and hand back a lazy iterator over the submessages.
    ///
    /// The receive path's door: it never allocates a `Vec`, so a datagram
    /// that turns out to be addressed elsewhere costs one header parse.
    ///
    /// # Errors
    ///
    /// Those of [`Header::decode`].
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_rtps::messages::{Header, Message};
    /// use astrs_rtps::structure::GuidPrefix;
    ///
    /// let datagram = Message::new(Header::new(GuidPrefix::new([1; 12]))).encode()?;
    /// let (header, mut submessages) = Message::scan(&datagram)?;
    /// assert_eq!(header.guid_prefix, GuidPrefix::new([1; 12]));
    /// assert!(submessages.next().is_none());
    /// # Ok::<(), astrs_rtps::RtpsError>(())
    /// ```
    pub fn scan(bytes: &'a [u8]) -> RtpsResult<(Header, SubmessageIter<'a>)> {
        let (header, rest) = Header::decode_prefix(bytes)?;
        Ok((header, SubmessageIter::new(rest, HEADER_LEN)))
    }
}

impl<'a> IntoIterator for Message<'a> {
    type Item = Submessage<'a>;
    type IntoIter = std::vec::IntoIter<Submessage<'a>>;

    fn into_iter(self) -> Self::IntoIter {
        self.submessages.into_iter()
    }
}

impl<'m, 'a> IntoIterator for &'m Message<'a> {
    type Item = &'m Submessage<'a>;
    type IntoIter = core::slice::Iter<'m, Submessage<'a>>;

    fn into_iter(self) -> Self::IntoIter {
        self.submessages.iter()
    }
}

impl fmt::Display for Message<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}", self.header)?;
        for submessage in &self.submessages {
            writeln!(f, "  {submessage}")?;
        }
        Ok(())
    }
}

/// A lazy walk over the submessages of a datagram.
///
/// Yields `Err` exactly once, at the first framing fault, and then stops:
/// once the alignment is lost there is nothing meaningful left to read.
#[derive(Debug, Clone)]
pub struct SubmessageIter<'a> {
    rest: &'a [u8],
    offset: usize,
    stopped: bool,
}

impl<'a> SubmessageIter<'a> {
    /// Walk the submessages in `rest`, which starts at `offset` octets into
    /// the datagram.
    ///
    /// The offset is what makes the four-octet alignment check meaningful, so
    /// it is required rather than assumed.
    #[must_use]
    pub const fn new(rest: &'a [u8], offset: usize) -> Self {
        Self {
            rest,
            offset,
            stopped: false,
        }
    }

    /// Octets not yet consumed.
    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.rest.len()
    }

    /// The offset the next submessage header starts at.
    #[must_use]
    pub const fn offset(&self) -> usize {
        self.offset
    }

    fn step(&mut self) -> RtpsResult<Submessage<'a>> {
        if !self.offset.is_multiple_of(SUBMESSAGE_ALIGNMENT) {
            return Err(RtpsError::MisalignedSubmessage {
                offset: self.offset,
            });
        }
        let header = SubmessageHeader::decode(self.rest)?;
        // `decode` succeeded, so at least four octets are present.
        let after_header = self.rest.get(SUBMESSAGE_HEADER_LEN..).unwrap_or(&[]);
        let body_len = match header.body_extent(after_header.len())? {
            BodyExtent::Exact(len) => len,
            BodyExtent::ToEndOfMessage => after_header.len(),
        };
        // `body_extent` already bounded `len` by `after_header.len()`.
        let body = after_header.get(..body_len).unwrap_or(after_header);
        let submessage = Submessage::read(&header, body)?;
        self.rest = after_header.get(body_len..).unwrap_or(&[]);
        self.offset += SUBMESSAGE_HEADER_LEN + body_len;
        Ok(submessage)
    }
}

impl<'a> Iterator for SubmessageIter<'a> {
    type Item = RtpsResult<Submessage<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.stopped || self.rest.is_empty() {
            return None;
        }
        let step = self.step();
        if step.is_err() {
            self.stopped = true;
        }
        Some(step)
    }
}

/// Builds a message up to a datagram budget.
///
/// The behavior half's tool for packing submessages into one datagram: it
/// tracks how many octets are left and refuses the submessage that would
/// overflow, so the caller can start a new message instead of discovering the
/// problem at `sendto` time.
///
/// ```
/// use astrs_rtps::messages::{Header, MessageBuilder, Pad};
/// use astrs_rtps::structure::GuidPrefix;
///
/// let mut builder = MessageBuilder::with_budget(Header::new(GuidPrefix::UNKNOWN), 32);
/// assert_eq!(builder.remaining(), 12); // 32 less the 20-octet header
/// assert!(builder.try_push(Pad::zeros(4)));  // 8 octets
/// assert!(!builder.try_push(Pad::zeros(8))); // 12 would not fit
/// assert_eq!(builder.build().len(), 1);
/// ```
#[derive(Debug, Clone)]
pub struct MessageBuilder<'a> {
    message: Message<'a>,
    budget: usize,
    used: usize,
}

impl<'a> MessageBuilder<'a> {
    /// A builder with no budget at all.
    #[must_use]
    pub const fn new(header: Header) -> Self {
        Self {
            message: Message::new(header),
            budget: usize::MAX,
            used: HEADER_LEN,
        }
    }

    /// A builder that will not exceed `budget` octets in total, header
    /// included.
    #[must_use]
    pub const fn with_budget(header: Header, budget: usize) -> Self {
        Self {
            message: Message::new(header),
            budget,
            used: HEADER_LEN,
        }
    }

    /// A builder with [`DEFAULT_DATAGRAM_BUDGET`].
    #[must_use]
    pub const fn for_datagram(header: Header) -> Self {
        Self::with_budget(header, DEFAULT_DATAGRAM_BUDGET)
    }

    /// Octets still available.
    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.budget.saturating_sub(self.used)
    }

    /// Octets the message occupies so far.
    #[must_use]
    pub const fn used(&self) -> usize {
        self.used
    }

    /// How many submessages have been accepted.
    #[must_use]
    pub fn len(&self) -> usize {
        self.message.len()
    }

    /// True when nothing has been accepted yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.message.is_empty()
    }

    /// True when `submessage` would fit the remaining budget.
    #[must_use]
    pub fn fits(&self, submessage: &Submessage<'a>) -> bool {
        submessage.serialized_len() <= self.remaining()
    }

    /// Append `submessage` when it fits, and report whether it did.
    pub fn try_push(&mut self, submessage: impl Into<Submessage<'a>>) -> bool {
        let submessage = submessage.into();
        if !self.fits(&submessage) {
            return false;
        }
        self.used += submessage.serialized_len();
        self.message.push(submessage);
        true
    }

    /// Append `submessage` whether or not it fits.
    ///
    /// For the first submessage of a message, which must go somewhere even
    /// when it is larger than the budget — that is what fragmentation is for,
    /// and the decision belongs to the caller.
    pub fn push(&mut self, submessage: impl Into<Submessage<'a>>) {
        let submessage = submessage.into();
        self.used += submessage.serialized_len();
        self.message.push(submessage);
    }

    /// The message built so far.
    #[must_use]
    pub fn build(self) -> Message<'a> {
        self.message
    }

    /// Encode the message built so far.
    ///
    /// # Errors
    ///
    /// Those of [`Message::encode`].
    pub fn encode(&self) -> RtpsResult<Vec<u8>> {
        self.message.encode()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use astrs_cdr::Endianness;

    use super::*;
    use crate::messages::data::{Data, DataPayload};
    use crate::messages::flags::SubmessageFlags;
    use crate::messages::heartbeat::Heartbeat;
    use crate::messages::info::{InfoDestination, InfoTimestamp};
    use crate::messages::kind::SubmessageId;
    use crate::messages::payload::SerializedPayload;
    use crate::messages::submessage::{Opaque, Pad};
    use crate::structure::guid::{EntityId, EntityKind};
    use crate::structure::sequence::SequenceNumber;
    use crate::structure::time::Time;

    const PREFIX: GuidPrefix = GuidPrefix::new([0x41, 0x53, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);

    fn writer_id() -> EntityId {
        EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY)
    }

    fn sample(sn: i64) -> Data<'static> {
        Data::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::new(sn),
            DataPayload::Data(SerializedPayload::from_cdr(&(sn as u32)).expect("encode")),
        )
    }

    #[test]
    fn an_empty_message_is_a_bare_header() {
        let message = Message::from_participant(PREFIX);
        assert!(message.is_empty());
        assert_eq!(message.len(), 0);
        assert_eq!(message.serialized_len(), HEADER_LEN);
        let bytes = message.encode().expect("encode");
        assert_eq!(bytes, Header::new(PREFIX).to_bytes());
        assert_eq!(Message::decode(&bytes).expect("decode"), message);
    }

    #[test]
    fn a_message_lays_its_submessages_out_back_to_back() {
        let message = Message::from_participant(PREFIX)
            .with(InfoTimestamp::at(Time::new(7, 0)))
            .with(sample(1));
        assert_eq!(message.serialized_len(), 20 + (4 + 8) + (4 + 28));
        let bytes = message.encode().expect("encode");
        assert_eq!(bytes.len(), message.serialized_len());

        // INFO_TS header at offset 20, DATA header at offset 32.
        assert_eq!(&bytes[20..24], &[0x09, 0x01, 0x08, 0x00]);
        assert_eq!(&bytes[32..36], &[0x15, 0x05, 0x1c, 0x00]);
        assert_eq!(Message::decode(&bytes).expect("decode"), message);
    }

    #[test]
    fn the_last_submessage_may_declare_zero_and_run_to_the_end() {
        let mut message = Message::from_participant(PREFIX);
        // A payload larger than the octetsToNextHeader field can express.
        message.push(Data::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::FIRST,
            DataPayload::Data(SerializedPayload::new(vec![0_u8; 70_000])),
        ));
        let bytes = message.encode().expect("encode");
        assert_eq!(&bytes[22..24], &[0x00, 0x00], "octetsToNextHeader is zero");
        assert_eq!(Message::decode(&bytes).expect("decode"), message);

        // The same submessage anywhere else cannot be expressed.
        let crowded = message.clone().with(Pad::empty());
        assert_eq!(
            crowded.encode().map(|_| ()),
            Err(RtpsError::TooLong {
                id: 0x15,
                length: 70_020,
                maximum: 65_535,
            })
        );
    }

    #[test]
    fn a_body_that_is_not_four_octet_aligned_may_only_be_last() {
        // A three-octet payload: legal as the last submessage, impossible
        // anywhere else, because the next header could not land on a
        // four-octet boundary.
        let ragged = Data::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::FIRST,
            DataPayload::Data(SerializedPayload::new(vec![1_u8, 2, 3])),
        );
        let last = Message::from_participant(PREFIX).with(ragged.clone());
        let bytes = last.encode().expect("last is fine");
        assert_eq!(Message::decode(&bytes).expect("decode"), last);

        let followed = last.with(Heartbeat::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::FIRST,
            SequenceNumber::FIRST,
            1,
        ));
        assert_eq!(
            followed.encode().map(|_| ()),
            Err(RtpsError::UnalignedBody {
                id: 0x15,
                body_len: 23,
            })
        );
    }

    #[test]
    fn a_cdr_payload_is_padded_so_the_alignment_rule_never_bites() {
        // A one-octet CDR value is five octets before padding; astrs-cdr pads
        // it to eight and records the count in the encapsulation options, so
        // the DATA can be followed by anything.
        let payload = SerializedPayload::from_cdr(&1_u8).expect("encode");
        assert_eq!(payload.len(), 8);
        assert!(payload.is_aligned());
        let message = Message::from_participant(PREFIX)
            .with(Data::new(
                EntityId::UNKNOWN,
                writer_id(),
                SequenceNumber::FIRST,
                DataPayload::Data(payload),
            ))
            .with(Pad::empty());
        let bytes = message.encode().expect("encode");
        assert_eq!(Message::decode(&bytes).expect("decode"), message);
    }

    #[test]
    fn a_misaligned_next_header_stops_the_walk() {
        // Hand-built: a PAD declaring a two-octet body, then another header.
        let mut bytes = Vec::from(Header::new(PREFIX).to_bytes());
        bytes.extend_from_slice(&[0x01, 0x01, 0x02, 0x00, 0xaa, 0xbb]);
        bytes.extend_from_slice(&[0x01, 0x01, 0x00, 0x00]);
        assert_eq!(
            Message::decode(&bytes).map(|_| ()),
            Err(RtpsError::MisalignedSubmessage { offset: 26 })
        );
    }

    #[test]
    fn a_length_past_the_end_of_the_datagram_is_refused() {
        let mut bytes = Vec::from(Header::new(PREFIX).to_bytes());
        bytes.extend_from_slice(&[0x07, 0x01, 0xff, 0x00]);
        bytes.extend_from_slice(&[0_u8; 8]);
        assert_eq!(
            Message::decode(&bytes).map(|_| ()),
            Err(RtpsError::SubmessageOverrun {
                id: 0x07,
                declared: 255,
                available: 8,
            })
        );
    }

    #[test]
    fn a_trailing_partial_submessage_header_is_a_truncation() {
        let mut bytes = Vec::from(Header::new(PREFIX).to_bytes());
        bytes.extend_from_slice(&[0x01, 0x01, 0x00]);
        let error = Message::decode(&bytes).expect_err("short");
        assert!(error.is_truncation());
    }

    #[test]
    fn a_pad_with_a_zero_length_does_not_swallow_the_rest_of_the_message() {
        // §8.3.3.2.3's exemption: a zero on PAD means an empty body, so the
        // INFO_DST after it is still parsed.
        let message = Message::from_participant(PREFIX)
            .with(Pad::empty())
            .with(InfoDestination::new(PREFIX));
        let bytes = message.encode().expect("encode");
        assert_eq!(&bytes[20..24], &[0x01, 0x01, 0x00, 0x00]);
        let decoded = Message::decode(&bytes).expect("decode");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded, message);
    }

    #[test]
    fn an_info_ts_with_the_invalidate_flag_has_a_zero_length_body() {
        let message = Message::from_participant(PREFIX)
            .with(InfoTimestamp::invalidate())
            .with(sample(3));
        let bytes = message.encode().expect("encode");
        assert_eq!(&bytes[20..24], &[0x09, 0x03, 0x00, 0x00]);
        assert_eq!(Message::decode(&bytes).expect("decode"), message);
    }

    #[test]
    fn an_empty_body_that_cannot_be_expressed_is_refused_rather_than_corrupted() {
        // An Opaque with an empty body would be written with
        // octetsToNextHeader == 0, which §8.3.3.2.3 reads as "to the end of
        // the message" for every kind but PAD and INFO_TS — so the DATA after
        // it would vanish on decode.
        let empty = Opaque::new(
            SubmessageId::from_raw(0x7b),
            SubmessageFlags::LITTLE_ENDIAN,
            Vec::new(),
        );
        let last = Message::from_participant(PREFIX).with(empty.clone());
        let bytes = last.encode().expect("last is fine");
        assert_eq!(Message::decode(&bytes).expect("decode"), last);

        let followed = last.with(sample(1));
        assert_eq!(
            followed.encode().map(|_| ()),
            Err(RtpsError::EmptyBodyNotLast { id: 0x7b })
        );

        // PAD and INFO_TS are exempt, so an empty one of those is fine.
        let padded = Message::from_participant(PREFIX)
            .with(Pad::empty())
            .with(InfoTimestamp::invalidate())
            .with(sample(1));
        let bytes = padded.encode().expect("PAD and INFO_TS may be empty");
        assert_eq!(Message::decode(&bytes).expect("decode").len(), 3);
    }

    #[test]
    fn an_unknown_submessage_id_is_carried_rather_than_dropped() {
        let message = Message::from_participant(PREFIX)
            .with(Opaque::new(
                SubmessageId::from_raw(0x7a),
                SubmessageFlags::LITTLE_ENDIAN,
                vec![1_u8, 2, 3, 4],
            ))
            .with(sample(2));
        let bytes = message.encode().expect("encode");
        let decoded = Message::decode(&bytes).expect("decode");
        assert_eq!(decoded, message);
        assert_eq!(decoded.iter().count(), 2);
        assert_eq!(decoded.submessages[0].id().raw(), 0x7a);
    }

    #[test]
    fn the_lazy_scan_yields_the_same_submessages_as_decode() {
        let message = Message::from_participant(PREFIX)
            .with(InfoTimestamp::at(Time::new(1, 0)))
            .with(sample(1))
            .with(Heartbeat::new(
                EntityId::UNKNOWN,
                writer_id(),
                SequenceNumber::FIRST,
                SequenceNumber::FIRST,
                1,
            ));
        let bytes = message.encode().expect("encode");
        let (header, iterator) = Message::scan(&bytes).expect("scan");
        assert_eq!(header, message.header);
        let scanned: Vec<Submessage<'_>> = iterator
            .map(|result| result.expect("well formed"))
            .collect();
        assert_eq!(scanned, message.submessages);
    }

    #[test]
    fn the_lazy_scan_stops_at_the_first_framing_fault() {
        let mut bytes = Vec::from(Header::new(PREFIX).to_bytes());
        bytes.extend_from_slice(&[0x01, 0x01, 0x00, 0x00]); // a good PAD
        bytes.extend_from_slice(&[0x07, 0x01, 0xf0, 0x00]); // a bad HEARTBEAT
        let (_, iterator) = Message::scan(&bytes).expect("scan");
        let results: Vec<RtpsResult<Submessage<'_>>> = iterator.collect();
        assert_eq!(results.len(), 2);
        assert!(results[0].is_ok());
        assert!(results[1].is_err());
    }

    #[test]
    fn the_iterator_tracks_its_offset_for_the_alignment_check() {
        let message = Message::from_participant(PREFIX).with(Pad::zeros(4));
        let bytes = message.encode().expect("encode");
        let (_, mut iterator) = Message::scan(&bytes).expect("scan");
        assert_eq!(iterator.offset(), HEADER_LEN);
        assert_eq!(iterator.remaining(), 8);
        assert!(iterator.next().is_some());
        assert_eq!(iterator.offset(), HEADER_LEN + 8);
        assert_eq!(iterator.remaining(), 0);
        assert!(iterator.next().is_none());
    }

    #[test]
    fn a_big_endian_message_round_trips() {
        let message = Message::from_participant(PREFIX)
            .with(InfoTimestamp::at(Time::new(9, 0)).with_endianness(Endianness::Big))
            .with(sample(4).with_endianness(Endianness::Big));
        let bytes = message.encode().expect("encode");
        assert_eq!(&bytes[20..24], &[0x09, 0x00, 0x00, 0x08]);
        assert_eq!(Message::decode(&bytes).expect("decode"), message);
    }

    #[test]
    fn validation_walks_every_submessage() {
        let good = Message::from_participant(PREFIX).with(sample(1));
        assert_eq!(good.validate(), Ok(()));
        let bad = Message::from_participant(PREFIX).with(sample(0));
        assert!(bad.validate().is_err());
        // Decoding does not validate: §8.3.4.1 discards the submessage, not
        // the datagram.
        let bytes = bad.encode().expect("encode");
        assert!(Message::decode(&bytes).is_ok());
    }

    #[test]
    fn messages_iterate_by_value_and_by_reference() {
        let message = Message::from_participant(PREFIX)
            .with(Pad::empty())
            .with(Pad::zeros(4));
        assert_eq!((&message).into_iter().count(), 2);
        assert_eq!(message.iter().count(), 2);
        assert_eq!(message.clone().into_owned().len(), 2);
        assert_eq!(message.into_iter().count(), 2);
    }

    #[test]
    fn the_display_form_lists_the_submessages() {
        let message = Message::from_participant(PREFIX).with(InfoDestination::new(PREFIX));
        let text = message.to_string();
        assert!(text.starts_with("RTPS 2.3 vendor 41.53"));
        assert!(text.contains("  INFO_DST 41.53"));
    }

    #[test]
    fn the_builder_refuses_what_would_not_fit_and_accepts_what_would() {
        let mut builder = MessageBuilder::with_budget(Header::new(PREFIX), 40);
        assert!(builder.is_empty());
        assert_eq!(builder.used(), HEADER_LEN);
        assert_eq!(builder.remaining(), 20);
        assert!(builder.try_push(Pad::zeros(4))); // 8 octets
        assert_eq!(builder.remaining(), 12);
        assert!(builder.try_push(Pad::zeros(8))); // 12 octets, exactly fits
        assert_eq!(builder.remaining(), 0);
        assert!(!builder.try_push(Pad::empty()));
        assert_eq!(builder.len(), 2);

        let bytes = builder.encode().expect("encode");
        assert_eq!(bytes.len(), 40);
        let message = builder.build();
        assert_eq!(message.len(), 2);
        assert_eq!(Message::decode(&bytes).expect("decode"), message);
    }

    #[test]
    fn the_builder_can_be_forced_past_its_budget_for_a_first_submessage() {
        let mut builder = MessageBuilder::for_datagram(Header::new(PREFIX));
        assert_eq!(builder.remaining(), DEFAULT_DATAGRAM_BUDGET - HEADER_LEN);
        let big = Data::new(
            EntityId::UNKNOWN,
            writer_id(),
            SequenceNumber::FIRST,
            DataPayload::Data(SerializedPayload::new(vec![0_u8; 4_000])),
        );
        assert!(!builder.fits(&Submessage::Data(big.clone())));
        assert!(!builder.try_push(big.clone()));
        builder.push(big);
        assert_eq!(builder.len(), 1);
        assert_eq!(builder.remaining(), 0);

        let unbounded = MessageBuilder::new(Header::new(PREFIX));
        assert_eq!(unbounded.remaining(), usize::MAX - HEADER_LEN);
    }

    #[test]
    fn a_crafted_datagram_cannot_produce_an_unbounded_submessage_vector() {
        // 20 000 empty PADs is above the ceiling; the decoder stops.
        let mut bytes = Vec::from(Header::new(PREFIX).to_bytes());
        for _ in 0..(MAX_SUBMESSAGES + 1) {
            bytes.extend_from_slice(&[0x01, 0x01, 0x00, 0x00]);
        }
        assert_eq!(
            Message::decode(&bytes).map(|_| ()),
            Err(RtpsError::TooManySubmessages {
                maximum: MAX_SUBMESSAGES,
            })
        );
    }

    #[test]
    fn the_udp_payload_ceiling_is_the_ipv4_one() {
        assert_eq!(MAX_UDP_PAYLOAD, 65_535 - 20 - 8);
        const _: () = assert!(DEFAULT_DATAGRAM_BUDGET < MAX_UDP_PAYLOAD);
    }
}
