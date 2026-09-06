//! The three constants that identify an RTPS speaker: the protocol id, the
//! protocol version and the vendor id.
//!
//! All three sit at the front of every [`Header`](crate::messages::Header),
//! they are all fixed-size octet sequences, and none of them is affected by
//! the byte order of anything else in the datagram — a `ProtocolVersion` is
//! `major` then `minor`, in that order, on a big-endian machine and on a
//! little-endian one alike (OMG DDSI-RTPS 2.3 §9.3.1).
//!
//! ```
//! use astrs_rtps::structure::{PROTOCOL_ID, ProtocolVersion, VendorId};
//!
//! assert_eq!(PROTOCOL_ID, *b"RTPS");
//! assert_eq!(ProtocolVersion::V2_3.to_bytes(), [2, 3]);
//! assert!(ProtocolVersion::V2_4.is_understood());
//! assert!(!ProtocolVersion::new(3, 0).is_understood());
//! assert_eq!(VendorId::ASTRS.to_bytes(), *b"AS");
//! ```

use core::fmt;

use astrs_cdr::{CdrDeserialize, CdrReader, CdrResult, CdrSerialize, CdrType, CdrWriter};

use crate::error::{RtpsError, RtpsResult};

/// The four octets every RTPS message starts with (§8.3.3.1.1).
///
/// `ProtocolId_t` is defined as the octet sequence `'R'`, `'T'`, `'P'`,
/// `'S'`; it is not a number, so it has no byte order.
pub const PROTOCOL_ID: [u8; 4] = *b"RTPS";

/// Octets a [`ProtocolVersion`] occupies on the wire.
pub const PROTOCOL_VERSION_LEN: usize = 2;

/// Octets a [`VendorId`] occupies on the wire.
pub const VENDOR_ID_LEN: usize = 2;

/// The RTPS protocol version a participant speaks (§9.3.1.1).
///
/// Two octets, `major` then `minor`. The compatibility rule AstRS applies is
/// the one §8.6 states: a receiver processes a message whose major version
/// matches its own and whose minor version is anything at all, ignoring the
/// submessages it does not recognise. A different major version is fatal,
/// because §8.6 makes no promise that the *message* framing survived it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProtocolVersion {
    /// Major version. AstRS understands only `2`.
    pub major: u8,
    /// Minor version. Any value is accepted when `major` is `2`.
    pub minor: u8,
}

impl ProtocolVersion {
    /// RTPS 1.0 — pre-DDSI, listed only so a log line can name it.
    pub const V1_0: Self = Self::new(1, 0);
    /// RTPS 2.0.
    pub const V2_0: Self = Self::new(2, 0);
    /// RTPS 2.1.
    pub const V2_1: Self = Self::new(2, 1);
    /// RTPS 2.2.
    pub const V2_2: Self = Self::new(2, 2);
    /// RTPS 2.3 — the version AstRS implements and announces.
    pub const V2_3: Self = Self::new(2, 3);
    /// RTPS 2.4 — accepted on receive; never announced.
    pub const V2_4: Self = Self::new(2, 4);

    /// The version AstRS puts in the header of every message it sends.
    pub const CURRENT: Self = Self::V2_3;

    /// The major version this implementation is built against.
    pub const SUPPORTED_MAJOR: u8 = 2;

    /// Build a version from its two octets.
    #[must_use]
    pub const fn new(major: u8, minor: u8) -> Self {
        Self { major, minor }
    }

    /// The wire form: `[major, minor]`.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; PROTOCOL_VERSION_LEN] {
        [self.major, self.minor]
    }

    /// Read the wire form.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; PROTOCOL_VERSION_LEN]) -> Self {
        Self::new(bytes[0], bytes[1])
    }

    /// True when this stack can parse messages stamped with this version.
    ///
    /// Only the major version is consulted, per §8.6.
    #[must_use]
    pub const fn is_understood(self) -> bool {
        self.major == Self::SUPPORTED_MAJOR
    }

    /// True when this version is at least `other`, comparing major first.
    ///
    /// Used to gate features a minor version introduced — the group-info
    /// flags of 2.3, for instance.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_rtps::structure::ProtocolVersion;
    ///
    /// assert!(ProtocolVersion::V2_3.is_at_least(ProtocolVersion::V2_1));
    /// assert!(!ProtocolVersion::V2_1.is_at_least(ProtocolVersion::V2_3));
    /// ```
    #[must_use]
    pub const fn is_at_least(self, other: Self) -> bool {
        self.major > other.major || (self.major == other.major && self.minor >= other.minor)
    }

    /// Reject a version this stack cannot parse.
    ///
    /// # Errors
    ///
    /// [`RtpsError::UnsupportedProtocolVersion`] when the major version is
    /// not [`ProtocolVersion::SUPPORTED_MAJOR`].
    pub const fn check_understood(self) -> RtpsResult<Self> {
        if self.is_understood() {
            Ok(self)
        } else {
            Err(RtpsError::UnsupportedProtocolVersion {
                major: self.major,
                minor: self.minor,
            })
        }
    }
}

impl Default for ProtocolVersion {
    /// [`ProtocolVersion::CURRENT`].
    fn default() -> Self {
        Self::CURRENT
    }
}

impl fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

impl From<[u8; PROTOCOL_VERSION_LEN]> for ProtocolVersion {
    fn from(bytes: [u8; PROTOCOL_VERSION_LEN]) -> Self {
        Self::from_bytes(bytes)
    }
}

impl From<ProtocolVersion> for [u8; PROTOCOL_VERSION_LEN] {
    fn from(version: ProtocolVersion) -> Self {
        version.to_bytes()
    }
}

impl CdrType for ProtocolVersion {
    const MIN_SERIALIZED_SIZE: usize = PROTOCOL_VERSION_LEN;
}

impl CdrSerialize for ProtocolVersion {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_octets(&self.to_bytes());
        Ok(())
    }
}

impl<'de> CdrDeserialize<'de> for ProtocolVersion {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        let major = reader.read_u8()?;
        let minor = reader.read_u8()?;
        Ok(Self::new(major, minor))
    }
}

/// The two-octet vendor id stamped into every message header (§9.3.1.3).
///
/// The value identifies the *implementation*, not the participant: every
/// message AstRS emits carries [`VendorId::ASTRS`], and a peer's id is what
/// lets the behavior half apply a vendor-specific workaround without
/// guessing.
///
/// Like [`ProtocolVersion`], it is an octet pair with no byte order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct VendorId([u8; VENDOR_ID_LEN]);

impl VendorId {
    /// `VENDORID_UNKNOWN` (§9.3.1.3): the participant did not say.
    pub const UNKNOWN: Self = Self([0x00, 0x00]);

    /// The id AstRS stamps into every message it sends.
    ///
    /// The two octets spell `"AS"`, matching the frame magic of
    /// `astrs-wire`. It is **not registered with the OMG**, and it is
    /// deliberately outside the `0x01, xx` range from which vendor ids have
    /// historically been assigned, so a future registration cannot collide
    /// with it. Nothing in AstRS branches on a peer's vendor id today; the
    /// field exists so peers can recognise us in a packet capture.
    pub const ASTRS: Self = Self(*b"AS");

    /// Wrap two raw octets.
    #[must_use]
    pub const fn new(octets: [u8; VENDOR_ID_LEN]) -> Self {
        Self(octets)
    }

    /// The wire form.
    #[must_use]
    pub const fn to_bytes(self) -> [u8; VENDOR_ID_LEN] {
        self.0
    }

    /// The two octets, borrowed.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; VENDOR_ID_LEN] {
        &self.0
    }

    /// True for [`VendorId::UNKNOWN`].
    #[must_use]
    pub const fn is_unknown(self) -> bool {
        self.0[0] == 0 && self.0[1] == 0
    }

    /// True when this is the AstRS id, so the behavior half can tell a
    /// loopback self-interop peer from a foreign stack.
    #[must_use]
    pub const fn is_astrs(self) -> bool {
        self.0[0] == VendorId::ASTRS.0[0] && self.0[1] == VendorId::ASTRS.0[1]
    }
}

impl fmt::Display for VendorId {
    /// `01.0f` — the dotted hexadecimal form packet captures use.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02x}.{:02x}", self.0[0], self.0[1])
    }
}

impl From<[u8; VENDOR_ID_LEN]> for VendorId {
    fn from(octets: [u8; VENDOR_ID_LEN]) -> Self {
        Self(octets)
    }
}

impl From<VendorId> for [u8; VENDOR_ID_LEN] {
    fn from(id: VendorId) -> Self {
        id.0
    }
}

impl CdrType for VendorId {
    const MIN_SERIALIZED_SIZE: usize = VENDOR_ID_LEN;
}

impl CdrSerialize for VendorId {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_octets(&self.0);
        Ok(())
    }
}

impl<'de> CdrDeserialize<'de> for VendorId {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        let first = reader.read_u8()?;
        let second = reader.read_u8()?;
        Ok(Self([first, second]))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use astrs_cdr::{Encoding, from_bytes_headerless, to_vec_headerless};

    use super::*;

    #[test]
    fn the_protocol_id_is_four_ascii_octets() {
        assert_eq!(PROTOCOL_ID, [0x52, 0x54, 0x50, 0x53]);
    }

    #[test]
    fn version_compatibility_looks_only_at_the_major() {
        assert!(ProtocolVersion::V2_0.is_understood());
        assert!(ProtocolVersion::V2_3.is_understood());
        // A 2.x minor from the future is processed, per §8.6.
        assert!(ProtocolVersion::new(2, 99).is_understood());
        assert!(!ProtocolVersion::V1_0.is_understood());
        assert_eq!(
            ProtocolVersion::new(3, 1).check_understood(),
            Err(RtpsError::UnsupportedProtocolVersion { major: 3, minor: 1 })
        );
        assert_eq!(
            ProtocolVersion::V2_3.check_understood(),
            Ok(ProtocolVersion::V2_3)
        );
    }

    #[test]
    fn versions_order_major_before_minor() {
        assert!(ProtocolVersion::V2_3.is_at_least(ProtocolVersion::V2_3));
        assert!(ProtocolVersion::V2_4.is_at_least(ProtocolVersion::V2_3));
        assert!(!ProtocolVersion::V2_2.is_at_least(ProtocolVersion::V2_3));
        assert!(ProtocolVersion::new(3, 0).is_at_least(ProtocolVersion::V2_4));
        assert!(!ProtocolVersion::V1_0.is_at_least(ProtocolVersion::V2_0));
    }

    #[test]
    fn versions_round_trip_through_their_octets() {
        for major in [0_u8, 1, 2, 255] {
            for minor in [0_u8, 3, 255] {
                let version = ProtocolVersion::new(major, minor);
                assert_eq!(ProtocolVersion::from_bytes(version.to_bytes()), version);
                let round: ProtocolVersion = <[u8; 2]>::from(version).into();
                assert_eq!(round, version);
            }
        }
        assert_eq!(ProtocolVersion::default(), ProtocolVersion::V2_3);
        assert_eq!(ProtocolVersion::V2_3.to_string(), "2.3");
    }

    #[test]
    fn a_version_is_two_cdr_octets_in_either_byte_order() {
        for encoding in [
            Encoding::ROS2,
            Encoding::new(astrs_cdr::EncapsulationKind::CdrBe),
        ] {
            let bytes = to_vec_headerless(&ProtocolVersion::V2_3, encoding).expect("encode");
            assert_eq!(bytes, [2, 3], "octet pairs have no byte order");
            let back: ProtocolVersion = from_bytes_headerless(&bytes, encoding).expect("decode");
            assert_eq!(back, ProtocolVersion::V2_3);
        }
    }

    #[test]
    fn the_astrs_vendor_id_is_the_ascii_pair_as() {
        assert_eq!(VendorId::ASTRS.to_bytes(), [0x41, 0x53]);
        assert_eq!(VendorId::ASTRS.as_bytes(), b"AS");
        assert!(VendorId::ASTRS.is_astrs());
        assert!(!VendorId::ASTRS.is_unknown());
        assert!(VendorId::UNKNOWN.is_unknown());
        assert!(!VendorId::UNKNOWN.is_astrs());
        assert_eq!(VendorId::default(), VendorId::UNKNOWN);
        // Outside the range vendor ids have historically been assigned from.
        assert_ne!(VendorId::ASTRS.to_bytes()[0], 0x01);
    }

    #[test]
    fn a_vendor_id_prints_the_way_a_capture_shows_it() {
        assert_eq!(VendorId::new([0x01, 0x0f]).to_string(), "01.0f");
        assert_eq!(VendorId::UNKNOWN.to_string(), "00.00");
    }

    #[test]
    fn a_vendor_id_round_trips_through_cdr_and_arrays() {
        let id = VendorId::new([0xde, 0xad]);
        let bytes = to_vec_headerless(&id, Encoding::ROS2).expect("encode");
        assert_eq!(bytes, [0xde, 0xad]);
        assert_eq!(
            from_bytes_headerless::<VendorId>(&bytes, Encoding::ROS2).expect("decode"),
            id
        );
        assert_eq!(VendorId::from(<[u8; 2]>::from(id)), id);
    }
}
