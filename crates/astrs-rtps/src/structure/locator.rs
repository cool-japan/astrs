//! Transport addresses: [`Locator`] and [`LocatorList`].
//!
//! A locator is RTPS's transport-independent address (OMG DDSI-RTPS 2.3
//! §9.4.2.11): a `kind` naming the transport, a `port`, and sixteen address
//! octets whose meaning the kind decides.
//!
//! ```text
//! +--------+--------+--------+--------+
//! |        kind (long)                |
//! +--------+--------+--------+--------+
//! |        port (unsigned long)       |
//! +--------+--------+--------+--------+
//! ~        address (octet[16])        ~
//! +--------+--------+--------+--------+
//! ```
//!
//! `kind` and `port` follow the stream's byte order; the sixteen address
//! octets do not, because they are an octet sequence. For a UDPv4 locator the
//! address is **left-padded with twelve zero octets**, the four IPv4 octets
//! last — the layout that lets a UDPv6 address occupy the same field.
//!
//! AstRS addresses UDPv4 (including loopback, §10.2). Every other kind
//! decodes and re-encodes verbatim, because a discovery sample must survive a
//! round trip through a participant that cannot *use* every locator its peer
//! advertised; asking such a locator for a socket address is what produces
//! [`RtpsError::UnsupportedLocator`].
//!
//! ```
//! use std::net::{Ipv4Addr, SocketAddr};
//!
//! use astrs_rtps::structure::{Locator, LocatorKind};
//!
//! let locator = Locator::udpv4(Ipv4Addr::LOCALHOST, 7410);
//! assert_eq!(locator.kind(), LocatorKind::UdpV4);
//! assert!(locator.is_loopback());
//! assert_eq!(
//!     locator.socket_addr()?,
//!     SocketAddr::from(([127, 0, 0, 1], 7410)),
//! );
//! // Twelve zero octets, then the address.
//! assert_eq!(locator.address()[12..], [127, 0, 0, 1]);
//! # Ok::<(), astrs_rtps::RtpsError>(())
//! ```

use core::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use astrs_cdr::{CdrDeserialize, CdrReader, CdrResult, CdrSerialize, CdrType, CdrWriter};

use crate::error::{RtpsError, RtpsResult};

/// Octets a [`Locator`] occupies on the wire.
pub const LOCATOR_LEN: usize = 24;

/// Octets of address a locator carries.
pub const LOCATOR_ADDRESS_LEN: usize = 16;

/// The largest number of entries [`LocatorList::read`] accepts.
///
/// Each entry costs 24 octets, so a datagram already bounds the count; this
/// ceiling makes the bound explicit rather than incidental. Real discovery
/// samples advertise a handful.
pub const MAX_LOCATORS: usize = 256;

/// `LOCATOR_PORT_INVALID` (§9.4.2.11).
pub const LOCATOR_PORT_INVALID: u32 = 0;

/// `LOCATOR_ADDRESS_INVALID` (§9.4.2.11): sixteen zero octets.
pub const LOCATOR_ADDRESS_INVALID: [u8; LOCATOR_ADDRESS_LEN] = [0; LOCATOR_ADDRESS_LEN];

/// Which transport a [`Locator`] addresses (§9.4.2.11).
///
/// The wire form is a signed 32-bit value; anything outside the four defined
/// values decodes into [`LocatorKind::Other`] and survives a round trip, so a
/// vendor transport a peer advertises is forwarded rather than dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum LocatorKind {
    /// `LOCATOR_KIND_INVALID` (`-1`): this locator addresses nothing.
    Invalid,
    /// `LOCATOR_KIND_RESERVED` (`0`).
    Reserved,
    /// `LOCATOR_KIND_UDPv4` (`1`): the four low address octets are an IPv4
    /// address.
    UdpV4,
    /// `LOCATOR_KIND_UDPv6` (`2`): all sixteen address octets are an IPv6
    /// address.
    UdpV6,
    /// A kind outside the four the specification defines.
    Other(i32),
}

impl LocatorKind {
    /// `LOCATOR_KIND_INVALID`.
    pub const INVALID: i32 = -1;
    /// `LOCATOR_KIND_RESERVED`.
    pub const RESERVED: i32 = 0;
    /// `LOCATOR_KIND_UDPv4`.
    pub const UDPV4: i32 = 1;
    /// `LOCATOR_KIND_UDPv6`.
    pub const UDPV6: i32 = 2;

    /// Classify a raw `kind` value.
    #[must_use]
    pub const fn from_raw(kind: i32) -> Self {
        match kind {
            Self::INVALID => Self::Invalid,
            Self::RESERVED => Self::Reserved,
            Self::UDPV4 => Self::UdpV4,
            Self::UDPV6 => Self::UdpV6,
            other => Self::Other(other),
        }
    }

    /// The raw `kind` value.
    #[must_use]
    pub const fn raw(self) -> i32 {
        match self {
            Self::Invalid => Self::INVALID,
            Self::Reserved => Self::RESERVED,
            Self::UdpV4 => Self::UDPV4,
            Self::UdpV6 => Self::UDPV6,
            Self::Other(raw) => raw,
        }
    }

    /// True for the two kinds that name an IP transport.
    #[must_use]
    pub const fn is_ip(self) -> bool {
        matches!(self, Self::UdpV4 | Self::UdpV6)
    }

    /// True for the kind AstRS's transport can actually send to.
    #[must_use]
    pub const fn is_supported(self) -> bool {
        matches!(self, Self::UdpV4)
    }
}

impl fmt::Display for LocatorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid => f.write_str("INVALID"),
            Self::Reserved => f.write_str("RESERVED"),
            Self::UdpV4 => f.write_str("UDPv4"),
            Self::UdpV6 => f.write_str("UDPv6"),
            Self::Other(raw) => write!(f, "kind({raw})"),
        }
    }
}

/// A transport address (§9.4.2.11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Locator {
    kind: i32,
    port: u32,
    address: [u8; LOCATOR_ADDRESS_LEN],
}

impl Locator {
    /// `LOCATOR_INVALID`: kind `-1`, port `0`, sixteen zero octets.
    pub const INVALID: Self = Self {
        kind: LocatorKind::INVALID,
        port: LOCATOR_PORT_INVALID,
        address: LOCATOR_ADDRESS_INVALID,
    };

    /// Build a locator from its three raw fields.
    ///
    /// The escape hatch that lets a decoded vendor locator be rebuilt
    /// unchanged; prefer [`Locator::udpv4`] for anything AstRS sends.
    #[must_use]
    pub const fn from_raw(kind: i32, port: u32, address: [u8; LOCATOR_ADDRESS_LEN]) -> Self {
        Self {
            kind,
            port,
            address,
        }
    }

    /// A UDPv4 locator: the address in the four low octets, twelve zeros
    /// before it.
    #[must_use]
    pub const fn udpv4(address: Ipv4Addr, port: u16) -> Self {
        let octets = address.octets();
        Self {
            kind: LocatorKind::UDPV4,
            port: port as u32,
            address: [
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, octets[0], octets[1], octets[2], octets[3],
            ],
        }
    }

    /// A UDPv6 locator.
    ///
    /// AstRS does not open IPv6 sockets in 0.1.0, but a peer's IPv6 locators
    /// must survive discovery, so the constructor exists and
    /// [`Locator::socket_addr`] refuses it explicitly rather than by
    /// accident.
    #[must_use]
    pub const fn udpv6(address: Ipv6Addr, port: u16) -> Self {
        Self {
            kind: LocatorKind::UDPV6,
            port: port as u32,
            address: address.octets(),
        }
    }

    /// The locator for a `std::net` socket address.
    #[must_use]
    pub fn from_socket_addr(address: SocketAddr) -> Self {
        match address {
            SocketAddr::V4(v4) => Self::udpv4(*v4.ip(), v4.port()),
            SocketAddr::V6(v6) => Self::udpv6(*v6.ip(), v6.port()),
        }
    }

    /// The classified kind.
    #[must_use]
    pub const fn kind(self) -> LocatorKind {
        LocatorKind::from_raw(self.kind)
    }

    /// The raw `kind` field.
    #[must_use]
    pub const fn kind_raw(self) -> i32 {
        self.kind
    }

    /// The port, as the 32-bit field carries it.
    ///
    /// RTPS declares the field `unsigned long`; UDP can only use the low
    /// sixteen bits, which is what [`Locator::udp_port`] returns.
    #[must_use]
    pub const fn port(self) -> u32 {
        self.port
    }

    /// The port as a UDP port number, or `None` when the field does not fit
    /// sixteen bits.
    #[must_use]
    pub const fn udp_port(self) -> Option<u16> {
        if self.port <= u16::MAX as u32 {
            Some(self.port as u16)
        } else {
            None
        }
    }

    /// The sixteen address octets.
    #[must_use]
    pub const fn address(&self) -> &[u8; LOCATOR_ADDRESS_LEN] {
        &self.address
    }

    /// The IPv4 address, for a UDPv4 locator.
    #[must_use]
    pub const fn ipv4(self) -> Option<Ipv4Addr> {
        match self.kind() {
            LocatorKind::UdpV4 => Some(Ipv4Addr::new(
                self.address[12],
                self.address[13],
                self.address[14],
                self.address[15],
            )),
            _ => None,
        }
    }

    /// The IPv6 address, for a UDPv6 locator.
    #[must_use]
    pub const fn ipv6(self) -> Option<Ipv6Addr> {
        match self.kind() {
            LocatorKind::UdpV6 => Some(Ipv6Addr::from_octets(self.address)),
            _ => None,
        }
    }

    /// The IP address, whichever family this locator names.
    #[must_use]
    pub fn ip(self) -> Option<IpAddr> {
        match self.kind() {
            LocatorKind::UdpV4 => self.ipv4().map(IpAddr::V4),
            LocatorKind::UdpV6 => self.ipv6().map(IpAddr::V6),
            _ => None,
        }
    }

    /// The socket address a datagram would be sent to.
    ///
    /// # Errors
    ///
    /// - [`RtpsError::UnsupportedLocator`] for a kind AstRS's transport
    ///   cannot address — including UDPv6, which is a 0.2 feature.
    /// - [`RtpsError::UnknownLocatorKind`] when the port does not fit the
    ///   sixteen bits UDP allows.
    pub fn socket_addr(self) -> RtpsResult<SocketAddr> {
        if !self.kind().is_supported() {
            return Err(RtpsError::UnsupportedLocator { kind: self.kind });
        }
        let port = self
            .udp_port()
            .ok_or(RtpsError::UnknownLocatorKind { kind: self.kind })?;
        let address = self
            .ipv4()
            .ok_or(RtpsError::UnsupportedLocator { kind: self.kind })?;
        Ok(SocketAddr::V4(SocketAddrV4::new(address, port)))
    }

    /// The socket address for any IP locator, UDPv6 included.
    ///
    /// The behavior half uses this for logging and for the day IPv6 arrives;
    /// [`Locator::socket_addr`] is the one the send path calls, because it
    /// refuses what the transport cannot do.
    #[must_use]
    pub fn ip_socket_addr(self) -> Option<SocketAddr> {
        let port = self.udp_port()?;
        match self.ip()? {
            IpAddr::V4(address) => Some(SocketAddr::V4(SocketAddrV4::new(address, port))),
            IpAddr::V6(address) => Some(SocketAddr::V6(SocketAddrV6::new(address, port, 0, 0))),
        }
    }

    /// True for [`Locator::INVALID`].
    #[must_use]
    pub fn is_invalid(self) -> bool {
        self == Self::INVALID
    }

    /// True when the address is a loopback address — the self-interop path
    /// §10.2 requires.
    #[must_use]
    pub fn is_loopback(self) -> bool {
        match self.ip() {
            Some(IpAddr::V4(address)) => address.is_loopback(),
            Some(IpAddr::V6(address)) => address.is_loopback(),
            None => false,
        }
    }

    /// True when the address is a multicast group.
    #[must_use]
    pub fn is_multicast(self) -> bool {
        match self.ip() {
            Some(IpAddr::V4(address)) => address.is_multicast(),
            Some(IpAddr::V6(address)) => address.is_multicast(),
            None => false,
        }
    }

    /// True when the address is the unspecified one (`0.0.0.0`, `::`).
    ///
    /// A peer that advertises it is saying "use the address you received this
    /// datagram from", which is the behavior half's job to resolve.
    #[must_use]
    pub fn is_unspecified(self) -> bool {
        match self.ip() {
            Some(IpAddr::V4(address)) => address.is_unspecified(),
            Some(IpAddr::V6(address)) => address.is_unspecified(),
            None => false,
        }
    }

    /// The same locator with a different port.
    #[must_use]
    pub const fn with_port(mut self, port: u32) -> Self {
        self.port = port;
        self
    }
}

impl Default for Locator {
    /// [`Locator::INVALID`].
    fn default() -> Self {
        Self::INVALID
    }
}

impl fmt::Display for Locator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.ip_socket_addr() {
            Some(address) => write!(f, "{address}"),
            None => write!(f, "{}:[{:02x?}]:{}", self.kind(), self.address, self.port),
        }
    }
}

impl From<SocketAddr> for Locator {
    fn from(address: SocketAddr) -> Self {
        Self::from_socket_addr(address)
    }
}

impl From<SocketAddrV4> for Locator {
    fn from(address: SocketAddrV4) -> Self {
        Self::udpv4(*address.ip(), address.port())
    }
}

impl CdrType for Locator {
    const MIN_SERIALIZED_SIZE: usize = LOCATOR_LEN;
}

impl CdrSerialize for Locator {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_i32(self.kind)?;
        writer.write_u32(self.port)?;
        writer.write_octets(&self.address);
        Ok(())
    }
}

impl<'de> CdrDeserialize<'de> for Locator {
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        let kind = reader.read_i32()?;
        let port = reader.read_u32()?;
        let octets = reader.read_octets(LOCATOR_ADDRESS_LEN)?;
        let mut address = [0_u8; LOCATOR_ADDRESS_LEN];
        address.copy_from_slice(octets);
        Ok(Self {
            kind,
            port,
            address,
        })
    }
}

/// A `sequence<Locator>`: a four-octet count, then the locators (§9.4.2.12).
///
/// This is the shape a `LocatorList` takes inside an `INFO_REPLY`
/// submessage. Inside a discovery parameter list a locator list is *not*
/// this — there each locator is its own repeated `PID_*_LOCATOR` parameter —
/// which is why this type stays in the message model rather than being
/// woven into `ParameterList`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct LocatorList {
    locators: Vec<Locator>,
}

impl LocatorList {
    /// An empty list.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            locators: Vec::new(),
        }
    }

    /// A list holding exactly one locator.
    #[must_use]
    pub fn single(locator: Locator) -> Self {
        Self {
            locators: Vec::from([locator]),
        }
    }

    /// Append a locator.
    pub fn push(&mut self, locator: Locator) {
        self.locators.push(locator);
    }

    /// The locators, in wire order.
    #[must_use]
    pub fn as_slice(&self) -> &[Locator] {
        &self.locators
    }

    /// Iterate over the locators.
    pub fn iter(&self) -> core::slice::Iter<'_, Locator> {
        self.locators.iter()
    }

    /// How many locators the list holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.locators.len()
    }

    /// True when the list holds no locators.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.locators.is_empty()
    }

    /// Every locator AstRS's transport can address, in wire order.
    pub fn addressable(&self) -> impl Iterator<Item = &Locator> {
        self.locators
            .iter()
            .filter(|locator| locator.kind().is_supported())
    }

    /// Octets the wire form occupies: the count plus 24 per locator.
    #[must_use]
    pub fn serialized_len(&self) -> usize {
        4 + LOCATOR_LEN * self.locators.len()
    }

    /// Read a list from a submessage body.
    ///
    /// The declared count is checked against the octets that remain *before*
    /// anything is allocated (`astrs-cdr`'s
    /// [`read_sequence_len`](astrs_cdr::CdrReader::read_sequence_len) does
    /// the arithmetic), and then against [`MAX_LOCATORS`].
    ///
    /// # Errors
    ///
    /// - [`RtpsError::TooManyLocators`] when the count exceeds
    ///   [`MAX_LOCATORS`].
    /// - [`RtpsError::Cdr`] when the count cannot fit the remaining octets or
    ///   the body ends inside a locator.
    pub fn read(reader: &mut CdrReader<'_>) -> RtpsResult<Self> {
        let count = reader.read_sequence_len(LOCATOR_LEN, "locator list")?;
        if count > MAX_LOCATORS {
            return Err(RtpsError::TooManyLocators {
                declared: count as u64,
                maximum: MAX_LOCATORS,
            });
        }
        let mut locators = Vec::with_capacity(count);
        for _ in 0..count {
            locators.push(Locator::deserialize(reader)?);
        }
        Ok(Self { locators })
    }

    /// Write a list into a submessage body.
    ///
    /// # Errors
    ///
    /// [`RtpsError::Cdr`] when the count does not fit a CDR `unsigned long`.
    pub fn write(&self, writer: &mut CdrWriter) -> RtpsResult<()> {
        writer.write_sequence_len(self.locators.len())?;
        for locator in &self.locators {
            locator.serialize(writer)?;
        }
        Ok(())
    }
}

impl From<Vec<Locator>> for LocatorList {
    fn from(locators: Vec<Locator>) -> Self {
        Self { locators }
    }
}

impl FromIterator<Locator> for LocatorList {
    fn from_iter<T: IntoIterator<Item = Locator>>(iter: T) -> Self {
        Self {
            locators: iter.into_iter().collect(),
        }
    }
}

impl IntoIterator for LocatorList {
    type Item = Locator;
    type IntoIter = std::vec::IntoIter<Locator>;

    fn into_iter(self) -> Self::IntoIter {
        self.locators.into_iter()
    }
}

impl<'a> IntoIterator for &'a LocatorList {
    type Item = &'a Locator;
    type IntoIter = core::slice::Iter<'a, Locator>;

    fn into_iter(self) -> Self::IntoIter {
        self.locators.iter()
    }
}

impl fmt::Display for LocatorList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[")?;
        for (index, locator) in self.locators.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{locator}")?;
        }
        f.write_str("]")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use astrs_cdr::{Encoding, to_vec_headerless};

    use super::*;

    #[test]
    fn a_udpv4_locator_left_pads_its_address_with_twelve_zeros() {
        let locator = Locator::udpv4(Ipv4Addr::new(192, 168, 1, 10), 7412);
        assert_eq!(locator.kind(), LocatorKind::UdpV4);
        assert_eq!(locator.kind_raw(), 1);
        assert_eq!(locator.port(), 7412);
        assert_eq!(locator.udp_port(), Some(7412));
        assert_eq!(
            locator.address(),
            &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 192, 168, 1, 10]
        );
        assert_eq!(locator.ipv4(), Some(Ipv4Addr::new(192, 168, 1, 10)));
        assert_eq!(locator.ipv6(), None);
        assert_eq!(
            locator.socket_addr().expect("addressable"),
            SocketAddr::from(([192, 168, 1, 10], 7412))
        );
        assert_eq!(locator.to_string(), "192.168.1.10:7412");
    }

    #[test]
    fn the_invalid_locator_is_all_the_invalid_values_at_once() {
        assert!(Locator::INVALID.is_invalid());
        assert_eq!(Locator::INVALID.kind(), LocatorKind::Invalid);
        assert_eq!(Locator::INVALID.port(), 0);
        assert_eq!(Locator::INVALID.address(), &[0_u8; 16]);
        assert_eq!(Locator::default(), Locator::INVALID);
        assert_eq!(
            Locator::INVALID.socket_addr(),
            Err(RtpsError::UnsupportedLocator { kind: -1 })
        );
        assert_eq!(Locator::INVALID.ip(), None);
        assert!(!Locator::INVALID.is_loopback());
        assert!(!Locator::INVALID.is_multicast());
        assert!(!Locator::INVALID.is_unspecified());
        assert_eq!(
            Locator::INVALID.to_string(),
            "INVALID:[[00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00, 00]]:0"
        );
    }

    #[test]
    fn loopback_multicast_and_unspecified_are_recognised() {
        assert!(Locator::udpv4(Ipv4Addr::LOCALHOST, 1).is_loopback());
        assert!(Locator::udpv4(Ipv4Addr::new(239, 255, 0, 1), 1).is_multicast());
        assert!(Locator::udpv4(Ipv4Addr::UNSPECIFIED, 1).is_unspecified());
        assert!(Locator::udpv6(Ipv6Addr::LOCALHOST, 1).is_loopback());
        assert!(Locator::udpv6(Ipv6Addr::UNSPECIFIED, 1).is_unspecified());
    }

    #[test]
    fn udpv6_decodes_and_prints_but_is_not_addressable_yet() {
        let locator = Locator::udpv6(Ipv6Addr::LOCALHOST, 7400);
        assert_eq!(locator.kind(), LocatorKind::UdpV6);
        assert!(locator.kind().is_ip());
        assert!(!locator.kind().is_supported());
        assert_eq!(locator.ipv6(), Some(Ipv6Addr::LOCALHOST));
        assert_eq!(locator.ipv4(), None);
        assert_eq!(
            locator.socket_addr(),
            Err(RtpsError::UnsupportedLocator { kind: 2 })
        );
        assert_eq!(
            locator.ip_socket_addr(),
            Some(SocketAddr::V6(SocketAddrV6::new(
                Ipv6Addr::LOCALHOST,
                7400,
                0,
                0
            )))
        );
    }

    #[test]
    fn a_vendor_kind_survives_a_round_trip_without_being_addressable() {
        let locator = Locator::from_raw(0x4000_0001, 42, [7; 16]);
        assert_eq!(locator.kind(), LocatorKind::Other(0x4000_0001));
        assert_eq!(locator.kind().raw(), 0x4000_0001);
        assert!(!locator.kind().is_ip());
        assert_eq!(
            locator.socket_addr(),
            Err(RtpsError::UnsupportedLocator { kind: 0x4000_0001 })
        );
        assert_eq!(locator.kind().to_string(), "kind(1073741825)");

        let bytes = to_vec_headerless(&locator, Encoding::ROS2).expect("encode");
        let mut reader = CdrReader::with_encoding(&bytes, Encoding::ROS2);
        assert_eq!(Locator::deserialize(&mut reader).expect("decode"), locator);
    }

    #[test]
    fn a_port_above_sixteen_bits_is_refused_rather_than_truncated() {
        let locator = Locator::udpv4(Ipv4Addr::LOCALHOST, 1).with_port(0x0001_0000);
        assert_eq!(locator.udp_port(), None);
        assert_eq!(
            locator.socket_addr(),
            Err(RtpsError::UnknownLocatorKind { kind: 1 })
        );
        assert_eq!(locator.ip_socket_addr(), None);
    }

    #[test]
    fn a_locator_is_twenty_four_octets_kind_port_address() {
        // §9.4.2.11: kind and port take the stream's byte order, the address
        // octets do not. Little-endian kind 1 is 01 00 00 00.
        let locator = Locator::udpv4(Ipv4Addr::new(10, 0, 0, 1), 0x1cea);
        let bytes = to_vec_headerless(&locator, Encoding::ROS2).expect("encode");
        assert_eq!(bytes.len(), LOCATOR_LEN);
        assert_eq!(
            bytes,
            [
                0x01, 0x00, 0x00, 0x00, // kind = 1, little-endian
                0xea, 0x1c, 0x00, 0x00, // port = 7402, little-endian
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // twelve pad octets
                10, 0, 0, 1, // the IPv4 address
            ]
        );

        let big = to_vec_headerless(&locator, Encoding::new(astrs_cdr::EncapsulationKind::CdrBe))
            .expect("encode");
        assert_eq!(
            big,
            [
                0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x1c, 0xea, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                10, 0, 0, 1,
            ]
        );
    }

    #[test]
    fn socket_addresses_convert_both_ways() {
        let v4 = SocketAddr::from(([127, 0, 0, 1], 7410));
        assert_eq!(Locator::from(v4).socket_addr().expect("addressable"), v4);
        assert_eq!(
            Locator::from(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 7410)),
            Locator::udpv4(Ipv4Addr::LOCALHOST, 7410)
        );
        let v6 = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::LOCALHOST, 7410, 0, 0));
        assert_eq!(Locator::from(v6).ip_socket_addr(), Some(v6));
    }

    #[test]
    fn an_empty_locator_list_is_a_zero_count() {
        let list = LocatorList::new();
        assert!(list.is_empty());
        assert_eq!(list.len(), 0);
        assert_eq!(list.serialized_len(), 4);
        assert_eq!(list.to_string(), "[]");

        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        list.write(&mut writer).expect("write");
        assert_eq!(writer.finish(), [0, 0, 0, 0]);
    }

    #[test]
    fn a_locator_list_round_trips_and_reports_what_is_addressable() {
        let list = LocatorList::from_iter([
            Locator::udpv4(Ipv4Addr::LOCALHOST, 7410),
            Locator::udpv6(Ipv6Addr::LOCALHOST, 7410),
            Locator::udpv4(Ipv4Addr::new(239, 255, 0, 1), 7400),
        ]);
        assert_eq!(list.len(), 3);
        assert_eq!(list.addressable().count(), 2);
        assert_eq!(list.serialized_len(), 4 + 3 * 24);
        assert_eq!(list.iter().count(), 3);
        assert_eq!((&list).into_iter().count(), 3);
        assert_eq!(list.clone().into_iter().count(), 3);
        assert_eq!(list.as_slice().len(), 3);

        let mut writer = CdrWriter::headerless(Encoding::ROS2);
        list.write(&mut writer).expect("write");
        let bytes = writer.finish();
        assert_eq!(bytes.len(), list.serialized_len());

        let mut reader = CdrReader::with_encoding(&bytes, Encoding::ROS2);
        assert_eq!(LocatorList::read(&mut reader).expect("read"), list);
        assert_eq!(reader.remaining(), 0);

        let mut single = LocatorList::single(Locator::INVALID);
        single.push(Locator::udpv4(Ipv4Addr::LOCALHOST, 1));
        assert_eq!(single.len(), 2);
        assert_eq!(LocatorList::from(Vec::from([Locator::INVALID])).len(), 1);
    }

    #[test]
    fn a_hostile_locator_count_is_refused_before_anything_is_allocated() {
        // A four-octet count of 0xffff_ffff with nothing behind it.
        let bytes = [0xff, 0xff, 0xff, 0xff];
        let mut reader = CdrReader::with_encoding(&bytes, Encoding::ROS2);
        let error = LocatorList::read(&mut reader).expect_err("refused");
        assert!(matches!(error, RtpsError::Cdr(_)));
    }

    #[test]
    fn a_count_above_the_ceiling_is_refused_even_when_the_octets_are_there() {
        // 257 locators would need 6168 octets; provide them, so only the
        // explicit ceiling can reject the list.
        let declared = (MAX_LOCATORS + 1) as u32;
        let mut bytes = Vec::from(declared.to_le_bytes());
        bytes.resize(4 + LOCATOR_LEN * (MAX_LOCATORS + 1), 0);
        let mut reader = CdrReader::with_encoding(&bytes, Encoding::ROS2);
        assert_eq!(
            LocatorList::read(&mut reader).map(|_| ()),
            Err(RtpsError::TooManyLocators {
                declared: u64::from(declared),
                maximum: MAX_LOCATORS,
            })
        );
    }
}
