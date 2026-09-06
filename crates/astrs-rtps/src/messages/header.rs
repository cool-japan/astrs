//! The two headers: the twenty-octet message header and the four-octet
//! submessage header.
//!
//! ```text
//!  Message
//! +----------------+----------------+----------------+----------------+
//! |  'R'   'T'   'P'   'S'          | major | minor  |   vendorId     |
//! +----------------+----------------+----------------+----------------+
//! |                    guidPrefix (12 octets)                         |
//! +----------------+----------------+----------------+----------------+
//! |  submessageId  |     flags      |    octetsToNextHeader           |
//! +----------------+----------------+----------------+----------------+
//! ~                        submessage body                            ~
//! +----------------+----------------+----------------+----------------+
//! ~                        further submessages                        ~
//! +----------------+----------------+----------------+----------------+
//! ```
//!
//! Nothing in the message header takes a byte order: the protocol id is four
//! ASCII octets, the version and vendor id are octet pairs, and the GUID
//! prefix is twelve octets. The submessage header's `octetsToNextHeader` is
//! the first field in an RTPS message that *does* — and it takes its own
//! submessage's [`EndiannessFlag`](crate::messages::flags::ENDIANNESS), which
//! is the reason bit 0 of the flags octet is read before anything else
//! (OMG DDSI-RTPS 2.3 §8.3.3).
//!
//! ```
//! use astrs_rtps::messages::Header;
//! use astrs_rtps::structure::{GuidPrefix, ProtocolVersion, VendorId};
//!
//! let header = Header::new(GuidPrefix::new([7; 12]));
//! assert_eq!(header.version, ProtocolVersion::V2_3);
//! assert_eq!(header.vendor_id, VendorId::ASTRS);
//! assert_eq!(header.to_bytes()[..4], *b"RTPS");
//! assert_eq!(Header::decode(&header.to_bytes())?, header);
//! # Ok::<(), astrs_rtps::RtpsError>(())
//! ```

use core::fmt;

use astrs_cdr::Endianness;

use crate::error::{RtpsError, RtpsResult};
use crate::messages::flags::SubmessageFlags;
use crate::messages::kind::SubmessageId;
use crate::structure::guid::{GUID_PREFIX_LEN, Guid, GuidPrefix};
use crate::structure::protocol::{
    PROTOCOL_ID, PROTOCOL_VERSION_LEN, ProtocolVersion, VENDOR_ID_LEN, VendorId,
};

/// Octets an RTPS message [`Header`] occupies.
pub const HEADER_LEN: usize = 20;

/// Octets a [`SubmessageHeader`] occupies.
pub const SUBMESSAGE_HEADER_LEN: usize = 4;

/// The alignment every submessage starts on (§8.3.3).
pub const SUBMESSAGE_ALIGNMENT: usize = 4;

/// The largest body length `octetsToNextHeader` can declare.
pub const MAX_OCTETS_TO_NEXT_HEADER: usize = u16::MAX as usize;

/// The twenty octets at the front of every RTPS message (§8.3.3.1).
///
/// The header answers "who is talking, and in what dialect". Every submessage
/// after it inherits the [`Header::guid_prefix`] as the participant its
/// `writerId` and `readerId` belong to, until an
/// [`InfoSource`](crate::messages::InfoSource) or
/// [`InfoDestination`](crate::messages::InfoDestination) says otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Header {
    /// The RTPS version the sender speaks.
    pub version: ProtocolVersion,
    /// The sender's implementation.
    pub vendor_id: VendorId,
    /// The sending participant.
    pub guid_prefix: GuidPrefix,
}

impl Header {
    /// The header AstRS puts on a message: RTPS 2.3, vendor `AS`, and the
    /// given participant.
    #[must_use]
    pub const fn new(guid_prefix: GuidPrefix) -> Self {
        Self {
            version: ProtocolVersion::CURRENT,
            vendor_id: VendorId::ASTRS,
            guid_prefix,
        }
    }

    /// A header with every field stated.
    ///
    /// Used by the golden-packet tests, which must reproduce a specific
    /// sender exactly, and by anything that forwards a peer's message.
    #[must_use]
    pub const fn with_parts(
        version: ProtocolVersion,
        vendor_id: VendorId,
        guid_prefix: GuidPrefix,
    ) -> Self {
        Self {
            version,
            vendor_id,
            guid_prefix,
        }
    }

    /// The GUID of the sending participant.
    #[must_use]
    pub const fn participant_guid(self) -> Guid {
        self.guid_prefix.participant_guid()
    }

    /// The twenty wire octets.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; HEADER_LEN] {
        let version = self.version.to_bytes();
        let vendor = self.vendor_id.to_bytes();
        let prefix = self.guid_prefix.to_bytes();
        [
            PROTOCOL_ID[0],
            PROTOCOL_ID[1],
            PROTOCOL_ID[2],
            PROTOCOL_ID[3],
            version[0],
            version[1],
            vendor[0],
            vendor[1],
            prefix[0],
            prefix[1],
            prefix[2],
            prefix[3],
            prefix[4],
            prefix[5],
            prefix[6],
            prefix[7],
            prefix[8],
            prefix[9],
            prefix[10],
            prefix[11],
        ]
    }

    /// Append the twenty wire octets to `output`.
    pub fn encode_into(self, output: &mut Vec<u8>) {
        output.extend_from_slice(&self.to_bytes());
    }

    /// Read a header from the front of `bytes`.
    ///
    /// The version check is the §8.6 one: any 2.x is processed, anything else
    /// is refused. See [`ProtocolVersion::is_understood`].
    ///
    /// # Errors
    ///
    /// - [`RtpsError::Truncated`] when fewer than twenty octets are present.
    /// - [`RtpsError::BadProtocolId`] when the first four are not `"RTPS"`.
    /// - [`RtpsError::UnsupportedProtocolVersion`] for a major version other
    ///   than 2.
    pub fn decode(bytes: &[u8]) -> RtpsResult<Self> {
        Self::decode_prefix(bytes).map(|(header, _)| header)
    }

    /// [`Header::decode`], also returning the octets that follow it.
    ///
    /// # Errors
    ///
    /// Those of [`Header::decode`].
    pub fn decode_prefix(bytes: &[u8]) -> RtpsResult<(Self, &[u8])> {
        let head = bytes.get(..HEADER_LEN).ok_or(RtpsError::Truncated {
            needed: HEADER_LEN,
            available: bytes.len(),
            context: "RTPS message header",
        })?;
        let mut protocol = [0_u8; 4];
        protocol.copy_from_slice(&head[..4]);
        if protocol != PROTOCOL_ID {
            return Err(RtpsError::BadProtocolId { found: protocol });
        }

        let mut version_octets = [0_u8; PROTOCOL_VERSION_LEN];
        version_octets.copy_from_slice(&head[4..4 + PROTOCOL_VERSION_LEN]);
        let version = ProtocolVersion::from_bytes(version_octets).check_understood()?;

        let mut vendor_octets = [0_u8; VENDOR_ID_LEN];
        vendor_octets.copy_from_slice(&head[6..6 + VENDOR_ID_LEN]);

        let mut prefix_octets = [0_u8; GUID_PREFIX_LEN];
        prefix_octets.copy_from_slice(&head[8..8 + GUID_PREFIX_LEN]);

        let header = Self {
            version,
            vendor_id: VendorId::new(vendor_octets),
            guid_prefix: GuidPrefix::new(prefix_octets),
        };
        // `head` is exactly HEADER_LEN long, so the remainder always exists.
        Ok((header, bytes.get(HEADER_LEN..).unwrap_or(&[])))
    }

    /// True when `bytes` could be an RTPS message: long enough, and starting
    /// with the protocol id.
    ///
    /// The cheap pre-filter a receive loop applies before it commits to
    /// parsing a datagram that landed on a shared port.
    #[must_use]
    pub fn looks_like_rtps(bytes: &[u8]) -> bool {
        bytes.len() >= HEADER_LEN && bytes.starts_with(&PROTOCOL_ID)
    }
}

impl fmt::Display for Header {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "RTPS {} vendor {} from {}",
            self.version, self.vendor_id, self.guid_prefix
        )
    }
}

/// The four octets at the front of every submessage (§8.3.3.2).
///
/// # `octetsToNextHeader`
///
/// The field is the distance in octets from the first octet *after* this
/// header to the first octet of the next submessage's header. §8.3.3.2.3
/// gives it two readings, and which one applies depends on the submessage
/// kind:
///
/// - **Nonzero**: the body is exactly that many octets. When this is the last
///   submessage, the count reaches the end of the message.
/// - **Zero, on `PAD` or `INFO_TS`**: the body is empty, and the next
///   submessage header follows immediately. Both kinds have a legitimate
///   empty form — `PAD` with nothing to pad, `INFO_TS` with the `I` flag —
///   so zero cannot mean "to the end" for them.
/// - **Zero, on anything else**: the body runs to the end of the message, and
///   this is therefore the last submessage. This is how a payload larger than
///   65 535 octets is sent on a transport that can carry it.
///
/// [`SubmessageHeader::body_extent`] applies all three rules in one place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubmessageHeader {
    /// The submessage kind.
    pub id: SubmessageId,
    /// The flags octet, verbatim.
    pub flags: SubmessageFlags,
    /// The `octetsToNextHeader` field, verbatim.
    pub octets_to_next_header: u16,
}

/// How far a submessage body extends, once §8.3.3.2.3 has been applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BodyExtent {
    /// The body is exactly this many octets, and another submessage may
    /// follow.
    Exact(usize),
    /// The body runs to the end of the message: this is the last submessage.
    ToEndOfMessage,
}

impl SubmessageHeader {
    /// Build a header from its three fields.
    #[must_use]
    pub const fn new(id: SubmessageId, flags: SubmessageFlags, octets_to_next_header: u16) -> Self {
        Self {
            id,
            flags,
            octets_to_next_header,
        }
    }

    /// The byte order of the body this header introduces.
    #[must_use]
    pub const fn endianness(self) -> Endianness {
        self.flags.endianness()
    }

    /// The four wire octets.
    ///
    /// `octetsToNextHeader` takes this submessage's own byte order, which is
    /// why the flags octet has to be written before it can be encoded.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; SUBMESSAGE_HEADER_LEN] {
        let length = match self.flags.endianness() {
            Endianness::Little => self.octets_to_next_header.to_le_bytes(),
            Endianness::Big => self.octets_to_next_header.to_be_bytes(),
        };
        [self.id.raw(), self.flags.raw(), length[0], length[1]]
    }

    /// Append the four wire octets to `output`.
    pub fn encode_into(self, output: &mut Vec<u8>) {
        output.extend_from_slice(&self.to_bytes());
    }

    /// Read a header from the front of `bytes`.
    ///
    /// # Errors
    ///
    /// [`RtpsError::Truncated`] when fewer than four octets are present.
    pub fn decode(bytes: &[u8]) -> RtpsResult<Self> {
        let head: [u8; SUBMESSAGE_HEADER_LEN] = bytes
            .get(..SUBMESSAGE_HEADER_LEN)
            .and_then(|slice| slice.try_into().ok())
            .ok_or(RtpsError::Truncated {
                needed: SUBMESSAGE_HEADER_LEN,
                available: bytes.len(),
                context: "submessage header",
            })?;
        let flags = SubmessageFlags::new(head[1]);
        let length_octets = [head[2], head[3]];
        let octets_to_next_header = match flags.endianness() {
            Endianness::Little => u16::from_le_bytes(length_octets),
            Endianness::Big => u16::from_be_bytes(length_octets),
        };
        Ok(Self {
            id: SubmessageId::from_raw(head[0]),
            flags,
            octets_to_next_header,
        })
    }

    /// Resolve `octetsToNextHeader` against the octets that follow this
    /// header, per §8.3.3.2.3.
    ///
    /// `available` is the number of octets between the end of this header and
    /// the end of the message.
    ///
    /// # Errors
    ///
    /// [`RtpsError::SubmessageOverrun`] when a nonzero count points past the
    /// end of the message.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_rtps::messages::{BodyExtent, SubmessageFlags, SubmessageHeader, SubmessageId};
    ///
    /// let heartbeat = SubmessageHeader::new(
    ///     SubmessageId::Heartbeat,
    ///     SubmessageFlags::LITTLE_ENDIAN,
    ///     0,
    /// );
    /// // Zero on a kind that cannot be empty: the body runs to the end.
    /// assert_eq!(heartbeat.body_extent(28)?, BodyExtent::ToEndOfMessage);
    ///
    /// let pad = SubmessageHeader::new(SubmessageId::Pad, SubmessageFlags::LITTLE_ENDIAN, 0);
    /// // Zero on PAD: an empty body, and another submessage may follow.
    /// assert_eq!(pad.body_extent(28)?, BodyExtent::Exact(0));
    /// # Ok::<(), astrs_rtps::RtpsError>(())
    /// ```
    pub fn body_extent(self, available: usize) -> RtpsResult<BodyExtent> {
        if self.octets_to_next_header == 0 {
            return if self.id.allows_empty_body() {
                Ok(BodyExtent::Exact(0))
            } else {
                Ok(BodyExtent::ToEndOfMessage)
            };
        }
        let declared = usize::from(self.octets_to_next_header);
        if declared > available {
            return Err(RtpsError::SubmessageOverrun {
                id: self.id.raw(),
                declared,
                available,
            });
        }
        Ok(BodyExtent::Exact(declared))
    }

    /// The `octetsToNextHeader` value a body of `body_len` octets needs.
    ///
    /// `is_last` selects between the two encodings of a body that does not
    /// fit sixteen bits: the last submessage of a message may declare zero
    /// and run to the end, any other must fail.
    ///
    /// # Errors
    ///
    /// [`RtpsError::TooLong`] when the body exceeds
    /// [`MAX_OCTETS_TO_NEXT_HEADER`] and is not last.
    pub fn octets_for(id: SubmessageId, body_len: usize, is_last: bool) -> RtpsResult<u16> {
        if body_len <= MAX_OCTETS_TO_NEXT_HEADER {
            // A zero-length body on a kind that cannot be empty would be read
            // back as "to the end of the message". That reading is correct
            // only when nothing follows, so anywhere else it is refused --
            // and there is no such submessage: every kind but PAD and INFO_TS
            // has mandatory fields.
            return u16::try_from(body_len).map_err(|_| RtpsError::TooLong {
                id: id.raw(),
                length: body_len,
                maximum: MAX_OCTETS_TO_NEXT_HEADER,
            });
        }
        if is_last {
            return Ok(0);
        }
        Err(RtpsError::TooLong {
            id: id.raw(),
            length: body_len,
            maximum: MAX_OCTETS_TO_NEXT_HEADER,
        })
    }
}

impl fmt::Display for SubmessageHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} flags {} len {}",
            self.id, self.flags, self.octets_to_next_header
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const PREFIX: GuidPrefix = GuidPrefix::new([0x41, 0x53, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);

    #[test]
    fn the_message_header_is_twenty_octets_in_a_fixed_order() {
        let header = Header::new(PREFIX);
        assert_eq!(
            header.to_bytes(),
            [
                b'R', b'T', b'P', b'S', // protocol id
                2, 3, // version 2.3
                0x41, 0x53, // vendor "AS"
                0x41, 0x53, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, // guidPrefix
            ]
        );
        assert_eq!(header.to_bytes().len(), HEADER_LEN);
        assert_eq!(Header::decode(&header.to_bytes()).expect("decode"), header);
        assert_eq!(header.participant_guid(), PREFIX.participant_guid());
        assert_eq!(
            header.to_string(),
            "RTPS 2.3 vendor 41.53 from 41.53.01.02.03.04.05.06.07.08.09.0a"
        );
    }

    #[test]
    fn decode_prefix_hands_back_the_submessages() {
        let mut bytes = Vec::from(Header::new(PREFIX).to_bytes());
        bytes.extend_from_slice(&[0x01, 0x01, 0x00, 0x00]);
        let (header, rest) = Header::decode_prefix(&bytes).expect("decode");
        assert_eq!(header.guid_prefix, PREFIX);
        assert_eq!(rest, [0x01, 0x01, 0x00, 0x00]);

        let (_, empty) = Header::decode_prefix(&bytes[..HEADER_LEN]).expect("decode");
        assert!(empty.is_empty());
    }

    #[test]
    fn a_datagram_that_is_not_rtps_is_refused_by_its_first_four_octets() {
        let mut bytes = Vec::from(*b"HTTP");
        bytes.resize(HEADER_LEN, 0);
        assert_eq!(
            Header::decode(&bytes),
            Err(RtpsError::BadProtocolId { found: *b"HTTP" })
        );
        assert!(!Header::looks_like_rtps(&bytes));
        assert!(Header::looks_like_rtps(&Header::new(PREFIX).to_bytes()));
        assert!(!Header::looks_like_rtps(b"RTPS"));
    }

    #[test]
    fn a_short_datagram_is_a_truncation_not_a_protocol_error() {
        assert_eq!(
            Header::decode(b"RTPS\x02\x03"),
            Err(RtpsError::Truncated {
                needed: HEADER_LEN,
                available: 6,
                context: "RTPS message header",
            })
        );
        assert_eq!(
            Header::decode(&[]),
            Err(RtpsError::Truncated {
                needed: HEADER_LEN,
                available: 0,
                context: "RTPS message header",
            })
        );
    }

    #[test]
    fn any_two_x_version_is_processed_and_nothing_else_is() {
        let mut bytes = Header::new(PREFIX).to_bytes();
        for minor in [0_u8, 1, 2, 3, 4, 200] {
            bytes[5] = minor;
            assert_eq!(
                Header::decode(&bytes).expect("2.x is understood").version,
                ProtocolVersion::new(2, minor)
            );
        }
        bytes[4] = 1;
        assert_eq!(
            Header::decode(&bytes),
            Err(RtpsError::UnsupportedProtocolVersion {
                major: 1,
                minor: 200
            })
        );
    }

    #[test]
    fn a_foreign_vendor_header_round_trips_unchanged() {
        let header = Header::with_parts(
            ProtocolVersion::V2_1,
            VendorId::new([0x01, 0x0f]),
            GuidPrefix::new([0x01, 0x0f, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9]),
        );
        let bytes = header.to_bytes();
        assert_eq!(Header::decode(&bytes).expect("decode"), header);
        let mut output = Vec::new();
        header.encode_into(&mut output);
        assert_eq!(output, bytes);
    }

    #[test]
    fn a_submessage_header_takes_its_own_byte_order_for_the_length() {
        let little = SubmessageHeader::new(
            SubmessageId::Heartbeat,
            SubmessageFlags::LITTLE_ENDIAN,
            0x0120,
        );
        assert_eq!(little.to_bytes(), [0x07, 0x01, 0x20, 0x01]);
        assert_eq!(little.endianness(), Endianness::Little);

        let big = SubmessageHeader::new(SubmessageId::Heartbeat, SubmessageFlags::NONE, 0x0120);
        assert_eq!(big.to_bytes(), [0x07, 0x00, 0x01, 0x20]);
        assert_eq!(big.endianness(), Endianness::Big);

        for header in [little, big] {
            assert_eq!(
                SubmessageHeader::decode(&header.to_bytes()).expect("decode"),
                header
            );
            let mut output = Vec::new();
            header.encode_into(&mut output);
            assert_eq!(output, header.to_bytes());
        }
        assert_eq!(little.to_string(), "HEARTBEAT flags 0x01 len 288");
    }

    #[test]
    fn a_submessage_header_needs_four_octets() {
        assert_eq!(
            SubmessageHeader::decode(&[0x15, 0x01, 0x00]),
            Err(RtpsError::Truncated {
                needed: SUBMESSAGE_HEADER_LEN,
                available: 3,
                context: "submessage header",
            })
        );
    }

    #[test]
    fn zero_means_two_different_things_depending_on_the_kind() {
        // §8.3.3.2.3, all three readings.
        for id in [SubmessageId::Pad, SubmessageId::InfoTimestamp] {
            let header = SubmessageHeader::new(id, SubmessageFlags::LITTLE_ENDIAN, 0);
            assert_eq!(
                header.body_extent(100).expect("resolvable"),
                BodyExtent::Exact(0),
                "{id} with a zero length has an empty body"
            );
        }
        for id in [
            SubmessageId::Data,
            SubmessageId::Heartbeat,
            SubmessageId::from_raw(0x99),
        ] {
            let header = SubmessageHeader::new(id, SubmessageFlags::LITTLE_ENDIAN, 0);
            assert_eq!(
                header.body_extent(100).expect("resolvable"),
                BodyExtent::ToEndOfMessage,
                "{id} with a zero length runs to the end"
            );
        }
    }

    #[test]
    fn a_length_past_the_end_of_the_message_is_refused() {
        let header = SubmessageHeader::new(SubmessageId::Data, SubmessageFlags::LITTLE_ENDIAN, 40);
        assert_eq!(
            header.body_extent(40).expect("exact"),
            BodyExtent::Exact(40)
        );
        assert_eq!(
            header.body_extent(39),
            Err(RtpsError::SubmessageOverrun {
                id: 0x15,
                declared: 40,
                available: 39,
            })
        );
    }

    #[test]
    fn a_body_that_does_not_fit_the_field_may_only_be_last() {
        assert_eq!(
            SubmessageHeader::octets_for(SubmessageId::Data, 40, false).expect("fits"),
            40
        );
        assert_eq!(
            SubmessageHeader::octets_for(SubmessageId::Data, 65_535, false).expect("fits"),
            65_535
        );
        assert_eq!(
            SubmessageHeader::octets_for(SubmessageId::Data, 65_536, true).expect("last"),
            0,
            "a body past the field runs to the end of the message"
        );
        assert_eq!(
            SubmessageHeader::octets_for(SubmessageId::Data, 65_536, false),
            Err(RtpsError::TooLong {
                id: 0x15,
                length: 65_536,
                maximum: MAX_OCTETS_TO_NEXT_HEADER,
            })
        );
    }
}
