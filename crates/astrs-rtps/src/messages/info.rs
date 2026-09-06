//! The `INFO_*` family: submessages that change how the ones after them are
//! read.
//!
//! An entity submessage (`DATA`, `HEARTBEAT`, …) names a reader and a writer
//! by their four-octet entity ids alone. The twelve-octet participant prefix
//! comes from somewhere else — from the message [`Header`](super::Header), or
//! from one of these. That is what "interpreter submessage" means in
//! §8.3.7: an `INFO_*` mutates the receiver's state, and every submessage
//! after it in the same message sees the new state.
//!
//! | Submessage | What it sets | Section |
//! |---|---|---|
//! | [`InfoTimestamp`] | the source timestamp of the samples that follow | §8.3.7.9 |
//! | [`InfoSource`] | the sending participant, version and vendor | §8.3.7.8 |
//! | [`InfoDestination`] | the participant the following submessages address | §8.3.7.7 |
//! | [`InfoReply`] | where a reply should be sent | §8.3.7.6 |
//!
//! ```
//! use astrs_rtps::messages::InfoTimestamp;
//! use astrs_rtps::structure::Time;
//!
//! let stamped = InfoTimestamp::at(Time::from_unix_nanos(1_700_000_000_000_000_000));
//! assert_eq!(stamped.body_len(), 8);
//! assert_eq!(stamped.flags().raw(), 0x01);
//!
//! // The I flag says "forget the timestamp"; the body is then empty.
//! let invalidated = InfoTimestamp::invalidate();
//! assert_eq!(invalidated.body_len(), 0);
//! assert_eq!(invalidated.flags().raw(), 0x03);
//! ```
//!
//! # `INFO_REPLY_IP4`
//!
//! The thirteenth submessage id, `INFO_REPLY_IP4` (§8.3.7.6.1), is *not*
//! decoded into a typed body here. Its locators are `LocatorUDPv4_t`, a
//! compact form distinct from the twenty-four-octet
//! [`Locator`](crate::structure::Locator), and this repository carries no
//! specification-independent source for that layout — so rather than guess,
//! AstRS carries the submessage as
//! [`Submessage::Opaque`](crate::messages::Submessage::Opaque): named
//! correctly in a log line, preserved octet for octet, never mis-read. No ROS
//! 2 stack emits it.

use core::fmt;

use astrs_cdr::{CdrDeserialize, CdrReader, CdrSerialize, CdrWriter, Endianness};

use crate::error::RtpsResult;
use crate::messages::flags::{self, Extension, SubmessageFlags, body_encoding};
use crate::messages::header::SubmessageHeader;
use crate::structure::guid::{GUID_PREFIX_LEN, GuidPrefix};
use crate::structure::locator::LocatorList;
use crate::structure::protocol::{ProtocolVersion, VendorId};
use crate::structure::time::{TIME_LEN, Time};

/// Octets an `INFO_SRC` body occupies.
///
/// An unused `long`, then `protocolVersion`, `vendorId` and `guidPrefix`.
pub const INFO_SOURCE_BODY_LEN: usize = 4 + 2 + 2 + GUID_PREFIX_LEN;

/// Octets an `INFO_DST` body occupies.
pub const INFO_DESTINATION_BODY_LEN: usize = GUID_PREFIX_LEN;

/// The `INFO_TS` submessage (§8.3.7.9).
///
/// Sets the source timestamp for every `DATA` and `DATA_FRAG` that follows it
/// in the same message. The `I` (invalidate) flag inverts the meaning: the
/// submessage then has an empty body and says the samples that follow carry
/// no timestamp at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InfoTimestamp {
    /// Byte order of the body.
    pub endianness: Endianness,
    /// The timestamp, or `None` when the `I` flag is set.
    pub timestamp: Option<Time>,
    /// Flag bits this build does not interpret.
    ///
    /// `INFO_TS` has no trailing extension: with the `I` flag its body is
    /// empty by definition, and without it the body is exactly a `Time_t`.
    pub reserved_flags: u8,
}

impl InfoTimestamp {
    /// The flag bits §8.3.7.9.1 defines for `INFO_TS`.
    pub const DEFINED_FLAGS: u8 = flags::ENDIANNESS | flags::INVALIDATE;

    /// A little-endian `INFO_TS` carrying `timestamp`.
    #[must_use]
    pub const fn at(timestamp: Time) -> Self {
        Self {
            endianness: Endianness::Little,
            timestamp: Some(timestamp),
            reserved_flags: 0,
        }
    }

    /// A little-endian `INFO_TS` with the `I` flag: the samples that follow
    /// have no source timestamp.
    #[must_use]
    pub const fn invalidate() -> Self {
        Self {
            endianness: Endianness::Little,
            timestamp: None,
            reserved_flags: 0,
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
        SubmessageFlags::from_endianness(self.endianness)
            .set(flags::INVALIDATE, self.timestamp.is_none())
            .with(self.reserved_flags)
    }

    /// Octets the body occupies: eight, or zero with the `I` flag.
    #[must_use]
    pub const fn body_len(&self) -> usize {
        if self.timestamp.is_some() {
            TIME_LEN
        } else {
            0
        }
    }

    /// True when the `I` flag is set.
    #[must_use]
    pub const fn invalidates(&self) -> bool {
        self.timestamp.is_none()
    }

    /// Write the body.
    ///
    /// # Errors
    ///
    /// [`RtpsError::Cdr`](crate::RtpsError::Cdr) from a field write.
    pub fn write_body(&self, writer: &mut CdrWriter) -> RtpsResult<()> {
        if let Some(timestamp) = self.timestamp {
            timestamp.serialize(writer)?;
        }
        Ok(())
    }

    /// Read a body against the flags its header carried.
    ///
    /// # Errors
    ///
    /// [`RtpsError::Cdr`](crate::RtpsError::Cdr) when the `I` flag is clear
    /// and the body is shorter than eight octets.
    pub fn read(header: &SubmessageHeader, body: &[u8]) -> RtpsResult<Self> {
        let endianness = header.endianness();
        let timestamp = if header.flags.has(flags::INVALIDATE) {
            None
        } else {
            let mut reader = CdrReader::with_encoding(body, body_encoding(endianness));
            Some(Time::deserialize(&mut reader)?)
        };
        Ok(Self {
            endianness,
            timestamp,
            reserved_flags: header.flags.reserved(Self::DEFINED_FLAGS),
        })
    }
}

impl fmt::Display for InfoTimestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.timestamp {
            Some(timestamp) => write!(f, "INFO_TS {timestamp}"),
            None => f.write_str("INFO_TS invalidate"),
        }
    }
}

/// The `INFO_SRC` submessage (§8.3.7.8).
///
/// Restates, mid-message, everything the message [`Header`](super::Header)
/// said: the version, the vendor and the sending participant. A relay that
/// bundles submessages from several participants into one datagram uses it;
/// so does a fault-injection test, which is why the fields are `pub`.
///
/// The body begins with four unused octets. §8.3.7.8.2 requires a sender to
/// zero them and a receiver to ignore them; AstRS preserves whatever it
/// received so re-emission is exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InfoSource {
    /// Byte order of the body.
    pub endianness: Endianness,
    /// The four unused octets, as a value in the body's byte order.
    pub unused: u32,
    /// The version the following submessages were written by.
    pub version: ProtocolVersion,
    /// The vendor of the participant that wrote them.
    pub vendor_id: VendorId,
    /// The participant the following submessages come from.
    pub guid_prefix: GuidPrefix,
    /// Flag bits this build does not interpret.
    pub reserved_flags: u8,
}

impl InfoSource {
    /// The flag bits §8.3.7.8.1 defines for `INFO_SRC`.
    pub const DEFINED_FLAGS: u8 = flags::ENDIANNESS;

    /// A little-endian `INFO_SRC` naming an AstRS participant.
    #[must_use]
    pub const fn new(guid_prefix: GuidPrefix) -> Self {
        Self {
            endianness: Endianness::Little,
            unused: 0,
            version: ProtocolVersion::CURRENT,
            vendor_id: VendorId::ASTRS,
            guid_prefix,
            reserved_flags: 0,
        }
    }

    /// An `INFO_SRC` with every field stated, for relaying another
    /// participant's submessages.
    #[must_use]
    pub const fn with_parts(
        version: ProtocolVersion,
        vendor_id: VendorId,
        guid_prefix: GuidPrefix,
    ) -> Self {
        Self {
            endianness: Endianness::Little,
            unused: 0,
            version,
            vendor_id,
            guid_prefix,
            reserved_flags: 0,
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
        SubmessageFlags::from_endianness(self.endianness).with(self.reserved_flags)
    }

    /// Octets the body occupies.
    #[must_use]
    pub const fn body_len(&self) -> usize {
        INFO_SOURCE_BODY_LEN
    }

    /// The message header this `INFO_SRC` is equivalent to.
    #[must_use]
    pub const fn as_header(&self) -> super::Header {
        super::Header::with_parts(self.version, self.vendor_id, self.guid_prefix)
    }

    /// Write the body.
    ///
    /// # Errors
    ///
    /// [`RtpsError::Cdr`](crate::RtpsError::Cdr) from a field write.
    pub fn write_body(&self, writer: &mut CdrWriter) -> RtpsResult<()> {
        writer.write_u32(self.unused)?;
        self.version.serialize(writer)?;
        self.vendor_id.serialize(writer)?;
        self.guid_prefix.serialize(writer)?;
        Ok(())
    }

    /// Read a body against the flags its header carried.
    ///
    /// Unlike the message header, an `INFO_SRC` does **not** reject a
    /// version it cannot parse: the version it carries describes the
    /// submessages that follow, and §8.6 asks a receiver to skip those rather
    /// than discard the whole datagram.
    ///
    /// # Errors
    ///
    /// [`RtpsError::Cdr`](crate::RtpsError::Cdr) when the body is shorter
    /// than twenty octets.
    pub fn read(header: &SubmessageHeader, body: &[u8]) -> RtpsResult<Self> {
        let endianness = header.endianness();
        let mut reader = CdrReader::with_encoding(body, body_encoding(endianness));
        let unused = reader.read_u32()?;
        let version = ProtocolVersion::deserialize(&mut reader)?;
        let vendor_id = VendorId::deserialize(&mut reader)?;
        let guid_prefix = GuidPrefix::deserialize(&mut reader)?;
        Ok(Self {
            endianness,
            unused,
            version,
            vendor_id,
            guid_prefix,
            reserved_flags: header.flags.reserved(Self::DEFINED_FLAGS),
        })
    }
}

impl fmt::Display for InfoSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "INFO_SRC {} vendor {} from {}",
            self.version, self.vendor_id, self.guid_prefix
        )
    }
}

/// The `INFO_DST` submessage (§8.3.7.7).
///
/// Names the participant the following submessages are addressed to. A
/// receiver whose own prefix differs must ignore them, which is how a
/// multicast datagram carries traffic for one specific peer. A prefix of
/// [`GuidPrefix::UNKNOWN`] readdresses the rest of the message to everyone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InfoDestination {
    /// Byte order of the body. No field of this body is byte-order
    /// sensitive, but the flag is still transmitted and preserved.
    pub endianness: Endianness,
    /// The participant addressed.
    pub guid_prefix: GuidPrefix,
    /// Flag bits this build does not interpret.
    pub reserved_flags: u8,
}

impl InfoDestination {
    /// The flag bits §8.3.7.7.1 defines for `INFO_DST`.
    pub const DEFINED_FLAGS: u8 = flags::ENDIANNESS;

    /// A little-endian `INFO_DST` addressing `guid_prefix`.
    #[must_use]
    pub const fn new(guid_prefix: GuidPrefix) -> Self {
        Self {
            endianness: Endianness::Little,
            guid_prefix,
            reserved_flags: 0,
        }
    }

    /// An `INFO_DST` that readdresses the rest of the message to every
    /// participant.
    #[must_use]
    pub const fn to_everyone() -> Self {
        Self::new(GuidPrefix::UNKNOWN)
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
        SubmessageFlags::from_endianness(self.endianness).with(self.reserved_flags)
    }

    /// Octets the body occupies.
    #[must_use]
    pub const fn body_len(&self) -> usize {
        INFO_DESTINATION_BODY_LEN
    }

    /// True when the submessage addresses every participant.
    #[must_use]
    pub const fn is_broadcast(&self) -> bool {
        self.guid_prefix.is_unknown()
    }

    /// Write the body.
    ///
    /// # Errors
    ///
    /// [`RtpsError::Cdr`](crate::RtpsError::Cdr) from a field write.
    pub fn write_body(&self, writer: &mut CdrWriter) -> RtpsResult<()> {
        self.guid_prefix.serialize(writer)?;
        Ok(())
    }

    /// Read a body against the flags its header carried.
    ///
    /// # Errors
    ///
    /// [`RtpsError::Cdr`](crate::RtpsError::Cdr) when the body is shorter
    /// than twelve octets.
    pub fn read(header: &SubmessageHeader, body: &[u8]) -> RtpsResult<Self> {
        let endianness = header.endianness();
        let mut reader = CdrReader::with_encoding(body, body_encoding(endianness));
        let guid_prefix = GuidPrefix::deserialize(&mut reader)?;
        Ok(Self {
            endianness,
            guid_prefix,
            reserved_flags: header.flags.reserved(Self::DEFINED_FLAGS),
        })
    }
}

impl fmt::Display for InfoDestination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "INFO_DST {}", self.guid_prefix)
    }
}

/// The `INFO_REPLY` submessage (§8.3.7.6).
///
/// Overrides where a reader should send its `ACKNACK`s: the unicast list
/// always, and a multicast list too when the `M` flag is set. A participant
/// behind a NAT, or one that received a datagram on an interface it does not
/// advertise, uses it to name the address that actually works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoReply {
    /// Byte order of the body.
    pub endianness: Endianness,
    /// Where to send unicast replies.
    pub unicast_locator_list: LocatorList,
    /// Where to send multicast replies (`M` flag), when there is such a list.
    pub multicast_locator_list: Option<LocatorList>,
    /// What this peer said that this build does not interpret.
    pub extension: Extension,
}

impl InfoReply {
    /// The flag bits §8.3.7.6.1 defines for `INFO_REPLY`.
    pub const DEFINED_FLAGS: u8 = flags::ENDIANNESS | flags::MULTICAST;

    /// A little-endian `INFO_REPLY` with only a unicast list.
    #[must_use]
    pub const fn new(unicast_locator_list: LocatorList) -> Self {
        Self {
            endianness: Endianness::Little,
            unicast_locator_list,
            multicast_locator_list: None,
            extension: Extension::EMPTY,
        }
    }

    /// The same submessage with a multicast list, which sets the `M` flag.
    #[must_use]
    pub fn with_multicast(mut self, multicast_locator_list: LocatorList) -> Self {
        self.multicast_locator_list = Some(multicast_locator_list);
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
            .set(flags::MULTICAST, self.multicast_locator_list.is_some())
            .with(self.extension.reserved_flags)
    }

    /// Octets the body occupies.
    #[must_use]
    pub fn body_len(&self) -> usize {
        self.unicast_locator_list.serialized_len()
            + self
                .multicast_locator_list
                .as_ref()
                .map_or(0, LocatorList::serialized_len)
            + self.extension.trailing_len()
    }

    /// Write the body.
    ///
    /// # Errors
    ///
    /// [`RtpsError::Cdr`](crate::RtpsError::Cdr) from a field write.
    pub fn write_body(&self, writer: &mut CdrWriter) -> RtpsResult<()> {
        self.unicast_locator_list.write(writer)?;
        if let Some(multicast) = &self.multicast_locator_list {
            multicast.write(writer)?;
        }
        writer.write_octets(&self.extension.trailing);
        Ok(())
    }

    /// Read a body against the flags its header carried.
    ///
    /// # Errors
    ///
    /// - [`RtpsError::TooManyLocators`](crate::RtpsError::TooManyLocators)
    ///   when a list declares an implausible count.
    /// - [`RtpsError::Cdr`](crate::RtpsError::Cdr) when the body ends inside
    ///   a locator.
    pub fn read(header: &SubmessageHeader, body: &[u8]) -> RtpsResult<Self> {
        let endianness = header.endianness();
        let mut reader = CdrReader::with_encoding(body, body_encoding(endianness));
        let unicast_locator_list = LocatorList::read(&mut reader)?;
        let multicast_locator_list = if header.flags.has(flags::MULTICAST) {
            Some(LocatorList::read(&mut reader)?)
        } else {
            None
        };
        Ok(Self {
            endianness,
            unicast_locator_list,
            multicast_locator_list,
            extension: Extension {
                reserved_flags: header.flags.reserved(Self::DEFINED_FLAGS),
                trailing: reader.peek_remaining().to_vec(),
            },
        })
    }
}

impl fmt::Display for InfoReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "INFO_REPLY unicast {}", self.unicast_locator_list)?;
        if let Some(multicast) = &self.multicast_locator_list {
            write!(f, " multicast {multicast}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::net::Ipv4Addr;

    use astrs_cdr::Encoding;

    use super::*;
    use crate::messages::kind::SubmessageId;
    use crate::structure::locator::Locator;

    const PREFIX: GuidPrefix = GuidPrefix::new([0x41, 0x53, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);

    #[test]
    fn an_info_ts_is_a_timestamp_or_nothing_at_all() {
        let stamped = InfoTimestamp::at(Time::new(0x6543_2100, 0x8000_0000));
        assert_eq!(stamped.flags().raw(), 0x01);
        assert_eq!(stamped.body_len(), 8);
        assert!(!stamped.invalidates());

        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        stamped.write_body(&mut writer).expect("write");
        assert_eq!(
            writer.finish(),
            [0x00, 0x21, 0x43, 0x65, 0x00, 0x00, 0x00, 0x80]
        );

        let invalidated = InfoTimestamp::invalidate();
        assert_eq!(invalidated.flags().raw(), 0x03);
        assert_eq!(invalidated.body_len(), 0);
        assert!(invalidated.invalidates());
        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        invalidated.write_body(&mut writer).expect("write");
        assert!(writer.finish().is_empty());

        assert_eq!(stamped.to_string(), "INFO_TS 1698898176.500000000");
        assert_eq!(invalidated.to_string(), "INFO_TS invalidate");
    }

    #[test]
    fn an_info_ts_round_trips_in_both_byte_orders_and_both_flag_states() {
        for endianness in [Endianness::Little, Endianness::Big] {
            for submessage in [
                InfoTimestamp::at(Time::new(-3, 7)).with_endianness(endianness),
                InfoTimestamp::invalidate().with_endianness(endianness),
            ] {
                let mut writer = CdrWriter::headerless(body_encoding(endianness));
                submessage.write_body(&mut writer).expect("write");
                let body = writer.finish();
                assert_eq!(body.len(), submessage.body_len());
                let header =
                    SubmessageHeader::new(SubmessageId::InfoTimestamp, submessage.flags(), 0);
                assert_eq!(
                    InfoTimestamp::read(&header, &body).expect("read"),
                    submessage
                );
            }
        }
    }

    #[test]
    fn an_info_src_body_is_twenty_octets_starting_with_four_unused_ones() {
        let source = InfoSource::new(PREFIX);
        assert_eq!(source.flags().raw(), 0x01);
        assert_eq!(source.body_len(), INFO_SOURCE_BODY_LEN);
        assert_eq!(source.as_header().guid_prefix, PREFIX);

        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        source.write_body(&mut writer).expect("write");
        assert_eq!(
            writer.finish(),
            [
                0x00, 0x00, 0x00, 0x00, // unused
                0x02, 0x03, // version 2.3
                0x41, 0x53, // vendor "AS"
                0x41, 0x53, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, // guidPrefix
            ]
        );
        assert_eq!(
            source.to_string(),
            "INFO_SRC 2.3 vendor 41.53 from 41.53.01.02.03.04.05.06.07.08.09.0a"
        );
    }

    #[test]
    fn an_info_src_does_not_reject_a_version_it_cannot_parse() {
        // §8.6: the version describes the submessages that follow, so a
        // receiver skips those rather than discarding the whole datagram.
        let source = InfoSource::with_parts(
            ProtocolVersion::new(3, 0),
            VendorId::new([0x01, 0x0f]),
            PREFIX,
        );
        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        source.write_body(&mut writer).expect("write");
        let header = SubmessageHeader::new(SubmessageId::InfoSource, source.flags(), 0);
        let decoded = InfoSource::read(&header, &writer.finish()).expect("read");
        assert_eq!(decoded, source);
        assert_eq!(decoded.version, ProtocolVersion::new(3, 0));
        assert!(!decoded.version.is_understood());
    }

    #[test]
    fn the_unused_field_of_an_info_src_is_preserved() {
        let mut body = Vec::from([0xde_u8, 0xad, 0xbe, 0xef]);
        body.extend_from_slice(&[0x02, 0x03, 0x41, 0x53]);
        body.extend_from_slice(&PREFIX.to_bytes());
        let header =
            SubmessageHeader::new(SubmessageId::InfoSource, SubmessageFlags::LITTLE_ENDIAN, 0);
        let decoded = InfoSource::read(&header, &body).expect("read");
        assert_eq!(decoded.unused, 0xefbe_adde);
        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        decoded.write_body(&mut writer).expect("write");
        assert_eq!(writer.finish(), body);
    }

    #[test]
    fn an_info_dst_is_twelve_octets_of_prefix() {
        let destination = InfoDestination::new(PREFIX);
        assert_eq!(destination.flags().raw(), 0x01);
        assert_eq!(destination.body_len(), INFO_DESTINATION_BODY_LEN);
        assert!(!destination.is_broadcast());
        assert!(InfoDestination::to_everyone().is_broadcast());

        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        destination.write_body(&mut writer).expect("write");
        assert_eq!(writer.finish(), PREFIX.to_bytes());

        let header = SubmessageHeader::new(SubmessageId::InfoDestination, destination.flags(), 0);
        assert_eq!(
            InfoDestination::read(&header, &PREFIX.to_bytes()).expect("read"),
            destination
        );
        assert_eq!(
            destination.to_string(),
            "INFO_DST 41.53.01.02.03.04.05.06.07.08.09.0a"
        );
    }

    #[test]
    fn an_info_dst_body_is_the_same_octets_in_either_byte_order() {
        let little = InfoDestination::new(PREFIX);
        let big = InfoDestination::new(PREFIX).with_endianness(Endianness::Big);
        assert_eq!(big.flags().raw(), 0x00);

        for submessage in [little, big] {
            let mut writer = CdrWriter::headerless(body_encoding(submessage.endianness));
            submessage.write_body(&mut writer).expect("write");
            assert_eq!(writer.finish(), PREFIX.to_bytes());
        }
    }

    #[test]
    fn an_info_reply_carries_one_or_two_locator_lists() {
        let unicast = LocatorList::single(Locator::udpv4(Ipv4Addr::LOCALHOST, 7410));
        let reply = InfoReply::new(unicast.clone());
        assert_eq!(reply.flags().raw(), 0x01);
        assert_eq!(reply.body_len(), 4 + 24);

        let with_multicast = reply
            .clone()
            .with_multicast(LocatorList::single(Locator::udpv4(
                Ipv4Addr::new(239, 255, 0, 1),
                7400,
            )));
        assert_eq!(with_multicast.flags().raw(), 0x03); // E | M
        assert_eq!(with_multicast.body_len(), (4 + 24) * 2);

        for submessage in [reply, with_multicast] {
            let mut writer = CdrWriter::headerless(Encoding::ROS2);
            submessage.write_body(&mut writer).expect("write");
            let body = writer.finish();
            assert_eq!(body.len(), submessage.body_len());
            let header = SubmessageHeader::new(SubmessageId::InfoReply, submessage.flags(), 0);
            assert_eq!(InfoReply::read(&header, &body).expect("read"), submessage);
        }
    }

    #[test]
    fn an_empty_info_reply_is_a_zero_count() {
        let reply = InfoReply::new(LocatorList::new());
        assert_eq!(reply.body_len(), 4);
        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        reply.write_body(&mut writer).expect("write");
        assert_eq!(writer.finish(), [0, 0, 0, 0]);
        assert_eq!(reply.to_string(), "INFO_REPLY unicast []");
    }

    #[test]
    fn an_info_reply_prints_both_lists() {
        let reply = InfoReply::new(LocatorList::single(Locator::udpv4(
            Ipv4Addr::LOCALHOST,
            7410,
        )))
        .with_multicast(LocatorList::single(Locator::udpv4(
            Ipv4Addr::new(239, 255, 0, 1),
            7400,
        )))
        .with_endianness(Endianness::Big);
        assert_eq!(
            reply.to_string(),
            "INFO_REPLY unicast [127.0.0.1:7410] multicast [239.255.0.1:7400]"
        );

        let mut writer = CdrWriter::headerless(body_encoding(Endianness::Big));
        reply.write_body(&mut writer).expect("write");
        let body = writer.finish();
        let header = SubmessageHeader::new(SubmessageId::InfoReply, reply.flags(), 0);
        assert_eq!(InfoReply::read(&header, &body).expect("read"), reply);
    }

    #[test]
    fn a_version_extension_after_the_locator_lists_is_preserved() {
        let reply = InfoReply::new(LocatorList::new());
        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        reply.write_body(&mut writer).expect("write");
        let mut body = writer.finish();
        body.extend_from_slice(&[7, 7, 7, 7]);

        let header = SubmessageHeader::new(
            SubmessageId::InfoReply,
            SubmessageFlags::new(flags::ENDIANNESS | 0x20),
            0,
        );
        let decoded = InfoReply::read(&header, &body).expect("read");
        assert_eq!(decoded.extension.reserved_flags, 0x20);
        assert_eq!(decoded.extension.trailing, [7, 7, 7, 7]);
        assert_eq!(decoded.body_len(), body.len());
    }

    #[test]
    fn truncated_info_bodies_are_truncations() {
        let ts_header = SubmessageHeader::new(
            SubmessageId::InfoTimestamp,
            SubmessageFlags::LITTLE_ENDIAN,
            0,
        );
        assert!(
            InfoTimestamp::read(&ts_header, &[0_u8; 4])
                .expect_err("short")
                .is_truncation()
        );

        let src_header =
            SubmessageHeader::new(SubmessageId::InfoSource, SubmessageFlags::LITTLE_ENDIAN, 0);
        assert!(
            InfoSource::read(&src_header, &[0_u8; 8])
                .expect_err("short")
                .is_truncation()
        );

        let dst_header = SubmessageHeader::new(
            SubmessageId::InfoDestination,
            SubmessageFlags::LITTLE_ENDIAN,
            0,
        );
        assert!(
            InfoDestination::read(&dst_header, &[0_u8; 8])
                .expect_err("short")
                .is_truncation()
        );
    }
}
