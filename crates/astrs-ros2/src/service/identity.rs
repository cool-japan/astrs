//! [`SampleIdentity`]: the twenty-four octets that make a reply belong to a
//! request.
//!
//! # The problem
//!
//! A ROS 2 service is two topics, not a channel. Every client of
//! `/add_two_ints` publishes on `rq/add_two_intsRequest` and *every* client
//! subscribes to `rr/add_two_intsReply` — so a reply arrives at every client
//! of that service, not only the one that asked. Something in the sample has
//! to say which request it answers.
//!
//! # The layout
//!
//! ```text
//!   ┌──────────────────┬────────────────────────┬──────────────────┐
//!   │ CDR header (4 B) │ SampleIdentity (24 B)  │ body …           │
//!   └──────────────────┴────────────────────────┴──────────────────┘
//!                        ├── writer GUID (16) ──┤
//!                                    ├ sequence (i64, 8) ┤
//! ```
//!
//! The client fills it with its own request-writer GUID and a
//! client-local sequence number; the server **echoes it verbatim** into the
//! reply. A client then keeps a reply if and only if `writer_guid` is its
//! own, and matches it to a pending call by `sequence_number`.
//!
//! # Two decisions worth stating
//!
//! - **The sequence number is a plain CDR `int64`**, not the RTPS
//!   `SequenceNumber_t` wire form (a `[high: i32][low: u32]` pair). This is
//!   the payload-embedded convention, and the two forms disagree on octet
//!   order for every value above `2³²`.
//! - **The sequence number is client-local, not the RTPS one.** It has to
//!   be: the identity is written *into* the payload, and the RTPS sequence
//!   number is only assigned when the payload is handed to the writer.
//!   [`WriterHandle::write`](astrs_rtps::behavior::WriterHandle::write)
//!   returns it afterwards, which is one call too late.
//!
//! # Provenance
//!
//! This layout follows `~/work/oxictl`'s `protocol/dds/api/service`
//! (blueprint §19.2 names it as a reference core), whose module docs record
//! the same three facts: 24 octets, GUID first, plain `int64` sequence.

use core::fmt;

use astrs_cdr::{CdrReader, CdrResult, CdrWriter};
use astrs_rtps::structure::{GUID_LEN, Guid};

/// Octets a sample identity occupies in the body.
pub const SAMPLE_IDENTITY_LEN: usize = GUID_LEN + 8;

/// The correlation header at the start of every service request and reply.
///
/// Also the public identity of an in-flight request: a server hands one back
/// with every request it takes, and hands it in again to answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct SampleIdentity {
    /// The requesting client's request-writer GUID, as sixteen octets.
    pub writer_guid: [u8; GUID_LEN],
    /// The client-local request sequence number.
    pub sequence_number: i64,
}

/// What a service server calls the identity of a request it must answer.
///
/// The same twenty-four octets; the alias exists because "the identity of
/// the sample" and "the thing you pass back to `send_response`" are
/// different ideas at a call site.
pub type RequestId = SampleIdentity;

impl SampleIdentity {
    /// The all-zero identity: no client, no sequence.
    pub const UNKNOWN: Self = Self {
        writer_guid: [0; GUID_LEN],
        sequence_number: 0,
    };

    /// Build an identity from its two halves.
    #[must_use]
    pub const fn new(writer_guid: [u8; GUID_LEN], sequence_number: i64) -> Self {
        Self {
            writer_guid,
            sequence_number,
        }
    }

    /// Build an identity from a client's request-writer GUID.
    #[must_use]
    pub const fn from_guid(guid: Guid, sequence_number: i64) -> Self {
        Self {
            writer_guid: guid.to_bytes(),
            sequence_number,
        }
    }

    /// The GUID inside, when the octets name a real endpoint.
    ///
    /// `None` for the all-zero GUID, matching
    /// [`Gid::guid`](astrs_rtps::discovery::Gid::guid): sixteen zero octets
    /// are `ENTITYID_UNKNOWN` on the null prefix, which is "nobody" rather
    /// than a GUID that happens to be zero.
    #[must_use]
    pub fn guid(&self) -> Option<Guid> {
        let guid = Guid::from_slice(&self.writer_guid)?;
        if guid.is_unknown() { None } else { Some(guid) }
    }

    /// True when this identity names no client at all.
    #[must_use]
    pub fn is_unknown(&self) -> bool {
        *self == Self::UNKNOWN
    }

    /// True when this identity was written by `guid`.
    ///
    /// The predicate a client applies to every reply on the shared `rr/`
    /// topic.
    #[must_use]
    pub fn was_written_by(&self, guid: Guid) -> bool {
        self.writer_guid == guid.to_bytes()
    }

    /// Write the twenty-four octets at the writer's current position.
    ///
    /// # Errors
    ///
    /// Whatever the writer reports.
    pub fn write(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_octets(&self.writer_guid);
        writer.write_i64(self.sequence_number)
    }

    /// Read twenty-four octets from the reader's current position.
    ///
    /// # Errors
    ///
    /// [`astrs_cdr::CdrError::Truncated`] when fewer than twenty-four octets
    /// remain.
    pub fn read(reader: &mut CdrReader<'_>) -> CdrResult<Self> {
        let octets = reader.read_octets(GUID_LEN)?;
        let mut writer_guid = [0_u8; GUID_LEN];
        // `read_octets` returns exactly `GUID_LEN` octets or errors, so the
        // copy cannot be short.
        writer_guid.copy_from_slice(octets);
        let sequence_number = reader.read_i64()?;
        Ok(Self {
            writer_guid,
            sequence_number,
        })
    }
}

impl fmt::Display for SampleIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.guid() {
            Some(guid) => write!(formatter, "{guid}#{}", self.sequence_number),
            None => write!(formatter, "GUID_UNKNOWN#{}", self.sequence_number),
        }
    }
}

/// Encode a service request or reply: header, identity, body.
///
/// # Errors
///
/// [`astrs_cdr::CdrError`] when the body will not encode.
pub fn encode_with_identity<T: astrs_cdr::CdrSerialize + ?Sized>(
    identity: SampleIdentity,
    body: &T,
) -> CdrResult<Vec<u8>> {
    let mut writer = CdrWriter::new(astrs_cdr::Encoding::ROS2);
    identity.write(&mut writer)?;
    writer.serialize(body)?;
    Ok(writer.finish())
}

/// Decode a service request or reply into its identity and its body.
///
/// # Errors
///
/// [`astrs_cdr::CdrError`] when the octets are short or the body will not
/// decode.
pub fn decode_with_identity<'de, T: astrs_cdr::CdrDeserialize<'de>>(
    payload: &'de [u8],
) -> CdrResult<(SampleIdentity, T)> {
    let mut reader = CdrReader::new(payload)?;
    let identity = SampleIdentity::read(&mut reader)?;
    let body = reader.deserialize::<T>()?;
    Ok((identity, body))
}

/// Read only the identity from a request or reply, leaving the body encoded.
///
/// What a client's reply dispatcher needs: it has to decide *whose* reply
/// this is before it knows whether decoding the body is worth the work, and
/// a reply for another client must not be able to fail this call by carrying
/// a body this client cannot parse.
///
/// # Errors
///
/// [`astrs_cdr::CdrError::Truncated`] when the payload is shorter than the
/// header plus twenty-four octets.
pub fn peek_identity(payload: &[u8]) -> CdrResult<SampleIdentity> {
    let mut reader = CdrReader::new(payload)?;
    SampleIdentity::read(&mut reader)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use astrs_rtps::structure::{EntityId, EntityKind, GuidPrefix, VendorId};

    fn guid(seed: u8) -> Guid {
        Guid::new(
            GuidPrefix::vendor_scoped(VendorId::ASTRS, [seed; 10]),
            EntityId::user_defined(1, EntityKind::USER_WRITER_NO_KEY),
        )
    }

    #[test]
    fn an_identity_is_exactly_twenty_four_octets() {
        assert_eq!(SAMPLE_IDENTITY_LEN, 24);
        let mut writer = CdrWriter::headerless(astrs_cdr::Encoding::ROS2.plain());
        SampleIdentity::from_guid(guid(1), 7)
            .write(&mut writer)
            .expect("write");
        assert_eq!(writer.finish().len(), 24);
    }

    #[test]
    fn the_guid_comes_first_and_the_sequence_is_a_plain_int64() {
        let mut writer = CdrWriter::headerless(astrs_cdr::Encoding::ROS2.plain());
        SampleIdentity::from_guid(guid(2), 1)
            .write(&mut writer)
            .expect("write");
        let octets = writer.finish();
        assert_eq!(&octets[..GUID_LEN], &guid(2).to_bytes());
        assert_eq!(
            &octets[GUID_LEN..],
            &[0x01, 0, 0, 0, 0, 0, 0, 0],
            "a plain little-endian int64, not the RTPS [high][low] pair"
        );
    }

    #[test]
    fn a_large_sequence_number_is_not_the_rtps_form() {
        // The value that separates the two encodings: as an RTPS
        // `SequenceNumber_t` this would be `[high=1][low=0]` — `01 00 00 00
        // 00 00 00 00` — and as a plain int64 it is the reverse.
        let mut writer = CdrWriter::headerless(astrs_cdr::Encoding::ROS2.plain());
        SampleIdentity::from_guid(guid(3), 1_i64 << 32)
            .write(&mut writer)
            .expect("write");
        let octets = writer.finish();
        assert_eq!(&octets[GUID_LEN..], &[0, 0, 0, 0, 0x01, 0, 0, 0]);
    }

    #[test]
    fn an_identity_round_trips() {
        for sequence in [0_i64, 1, -1, i64::MAX, i64::MIN] {
            let identity = SampleIdentity::from_guid(guid(4), sequence);
            let mut writer = CdrWriter::headerless(astrs_cdr::Encoding::ROS2.plain());
            identity.write(&mut writer).expect("write");
            let octets = writer.finish();
            let mut reader = CdrReader::with_encoding(&octets, astrs_cdr::Encoding::ROS2.plain());
            assert_eq!(SampleIdentity::read(&mut reader).expect("read"), identity);
        }
    }

    #[test]
    fn an_identity_recovers_its_guid() {
        let identity = SampleIdentity::from_guid(guid(5), 3);
        assert_eq!(identity.guid(), Some(guid(5)));
        assert!(identity.was_written_by(guid(5)));
        assert!(!identity.was_written_by(guid(6)));
        assert!(!identity.is_unknown());
        assert!(SampleIdentity::UNKNOWN.is_unknown());
    }

    #[test]
    fn a_request_encodes_the_identity_ahead_of_the_body() {
        use crate::msg::example_interfaces::AddTwoIntsRequest;
        let identity = SampleIdentity::from_guid(guid(7), 11);
        let body = AddTwoIntsRequest { a: 2, b: 40 };
        let payload = encode_with_identity(identity, &body).expect("encode");

        assert_eq!(peek_identity(&payload).expect("peek"), identity);
        let (read_identity, read_body) =
            decode_with_identity::<AddTwoIntsRequest>(&payload).expect("decode");
        assert_eq!(read_identity, identity);
        assert_eq!(read_body, body);
    }

    #[test]
    fn the_identity_sits_immediately_after_the_encapsulation_header() {
        use crate::msg::example_interfaces::AddTwoIntsRequest;
        let identity = SampleIdentity::from_guid(guid(8), 5);
        let payload =
            encode_with_identity(identity, &AddTwoIntsRequest { a: 1, b: 1 }).expect("encode");
        assert_eq!(&payload[4..20], &guid(8).to_bytes());
        assert_eq!(
            payload.len(),
            4 + SAMPLE_IDENTITY_LEN + 16,
            "two int64 fields follow the identity, both already 8-aligned"
        );
    }

    #[test]
    fn a_truncated_payload_is_refused_rather_than_misread() {
        let short = [0_u8; 8];
        assert!(peek_identity(&short).is_err());
        assert!(decode_with_identity::<i64>(&short).is_err());
    }

    #[test]
    fn peeking_survives_a_body_this_reader_cannot_parse() {
        // A reply for another client, carrying a body of a type this client
        // does not know: peeking must still tell it whose reply it is.
        let identity = SampleIdentity::from_guid(guid(9), 2);
        let mut writer = CdrWriter::new(astrs_cdr::Encoding::ROS2);
        identity.write(&mut writer).expect("write");
        writer.write_octets(&[0xff; 3]);
        let payload = writer.finish();
        assert_eq!(peek_identity(&payload).expect("peek"), identity);
    }

    #[test]
    fn the_display_form_names_the_client_and_the_sequence() {
        let identity = SampleIdentity::from_guid(guid(10), 42);
        assert!(identity.to_string().ends_with("#42"), "{identity}");
        assert!(
            SampleIdentity::UNKNOWN
                .to_string()
                .starts_with("GUID_UNKNOWN"),
            "the all-zero identity is not a GUID"
        );
    }

    #[test]
    fn identities_order_by_client_then_sequence() {
        let mut identities = [
            SampleIdentity::from_guid(guid(2), 5),
            SampleIdentity::from_guid(guid(1), 9),
            SampleIdentity::from_guid(guid(1), 2),
        ];
        identities.sort_unstable();
        assert_eq!(identities[0], SampleIdentity::from_guid(guid(1), 2));
        assert_eq!(identities[1], SampleIdentity::from_guid(guid(1), 9));
        assert_eq!(identities[2], SampleIdentity::from_guid(guid(2), 5));
    }
}
