//! The ROS 2 distribution switch: `Humble` (24-octet GID) or `Jazzy`
//! (16-octet GID).
//!
//! Two ROS 2 releases disagree about how wide a participant identifier is.
//! `rmw_dds_common/msg/Gid` was `uint8[24] data` up to and including Humble
//! and became `uint8[16] data` in Iron, which is what Jazzy ships. An RTPS
//! GUID is sixteen octets either way; the Humble form is that GUID followed
//! by eight zero octets.
//!
//! Blueprint §10.2 fixes this as a per-bridge configuration switch rather
//! than a build feature, because one process routinely bridges to two robots
//! on two distributions. [`RosCompat`] is therefore a value a participant
//! carries, and [`Gid`] is a value that knows its own width — encoding a
//! `Gid` never consults an ambient setting.
//!
//! # What this module is *not*
//!
//! It is not the ROS graph. `rmw_dds_common`'s `ros_discovery_info` topic,
//! the `rt/` `rq/` `rr/` name mangling and the `ParticipantEntitiesInfo`
//! message all belong to `astrs-ros2` (blueprint §10.4). What lives here is
//! the part RTPS itself must get right: the octets, and the width they come
//! in.
//!
//! # Example
//!
//! ```
//! use astrs_rtps::discovery::{Gid, RosCompat};
//! use astrs_rtps::structure::{ENTITYID_PARTICIPANT, Guid, GuidPrefix, VendorId};
//!
//! let guid = Guid::new(
//!     GuidPrefix::vendor_scoped(VendorId::ASTRS, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]),
//!     ENTITYID_PARTICIPANT,
//! );
//!
//! let humble = Gid::new(RosCompat::Humble, guid);
//! let jazzy = Gid::new(RosCompat::Jazzy, guid);
//!
//! assert_eq!(humble.as_slice().len(), 24);
//! assert_eq!(jazzy.as_slice().len(), 16);
//!
//! // The first sixteen octets are the same GUID; Humble simply pads.
//! assert_eq!(&humble.as_slice()[..16], jazzy.as_slice());
//! assert_eq!(&humble.as_slice()[16..], &[0; 8]);
//!
//! // And both decode back to the GUID they came from.
//! assert_eq!(humble.guid(), Some(guid));
//! assert_eq!(jazzy.guid(), Some(guid));
//! ```

use core::fmt;

use astrs_cdr::{CdrDeserialize, CdrReader, CdrResult, CdrSerialize, CdrType, CdrWriter};

use crate::structure::{GUID_LEN, Guid};

/// The widest GID any supported distribution uses.
pub const MAX_GID_LEN: usize = 24;

/// The GID width Humble and earlier use: sixteen GUID octets, eight of
/// padding.
pub const HUMBLE_GID_LEN: usize = 24;

/// The GID width Iron and later — Jazzy included — use: the GUID, exactly.
pub const JAZZY_GID_LEN: usize = GUID_LEN;

/// Which ROS 2 distribution's conventions a bridge speaks.
///
/// Per-bridge configuration, never a compile-time feature: blueprint §10.2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum RosCompat {
    /// ROS 2 Humble Hawksbill and earlier — `rmw_dds_common` `Gid` is
    /// `uint8[24]`.
    Humble,
    /// ROS 2 Jazzy Jalisco (and Iron) — `Gid` is `uint8[16]`, the RTPS GUID
    /// unpadded. The default, because it is the current long-term release.
    #[default]
    Jazzy,
}

impl RosCompat {
    /// Every variant, for a test that must cover both.
    pub const ALL: [Self; 2] = [Self::Humble, Self::Jazzy];

    /// Octets a GID occupies on this distribution.
    #[must_use]
    pub const fn gid_len(self) -> usize {
        match self {
            Self::Humble => HUMBLE_GID_LEN,
            Self::Jazzy => JAZZY_GID_LEN,
        }
    }

    /// Octets of zero padding that follow the GUID inside a GID.
    #[must_use]
    pub const fn gid_padding(self) -> usize {
        self.gid_len() - GUID_LEN
    }

    /// The distribution's name, lower-case, as a manifest spells it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Humble => "humble",
            Self::Jazzy => "jazzy",
        }
    }

    /// Parse a distribution name, case-insensitively.
    ///
    /// Iron shares Jazzy's sixteen-octet GID, so it is accepted as an alias
    /// rather than rejected.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "humble" | "galactic" | "foxy" => Some(Self::Humble),
            "jazzy" | "iron" | "rolling" => Some(Self::Jazzy),
            _ => None,
        }
    }

    /// True when this distribution pads the GUID out to twenty-four octets.
    #[must_use]
    pub const fn pads_gid(self) -> bool {
        self.gid_padding() > 0
    }

    /// Whether the distribution's `rmw` announces XCDR2 in
    /// `PID_DATA_REPRESENTATION`.
    ///
    /// Humble is XCDR1-only. Jazzy advertises both and negotiates down, which
    /// is why `astrs-cdr` carries an XCDR2 read path at all.
    #[must_use]
    pub const fn advertises_xcdr2(self) -> bool {
        matches!(self, Self::Jazzy)
    }

    /// Build a [`Gid`] for `guid` in this distribution's width.
    #[must_use]
    pub const fn gid(self, guid: Guid) -> Gid {
        Gid::new(self, guid)
    }
}

impl fmt::Display for RosCompat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// An `rmw_dds_common` GID: a GUID plus whatever padding its distribution
/// wants.
///
/// The width is part of the value, not of the ambient configuration, so a
/// `Gid` cannot be serialized at the wrong length by a caller that forgot
/// which bridge it was on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Gid {
    octets: [u8; MAX_GID_LEN],
    len: usize,
}

impl Gid {
    /// Encode `guid` at `compat`'s width, zero-padding the tail.
    #[must_use]
    pub const fn new(compat: RosCompat, guid: Guid) -> Self {
        let guid_octets = guid.to_bytes();
        let mut octets = [0_u8; MAX_GID_LEN];
        let mut index = 0;
        while index < GUID_LEN {
            octets[index] = guid_octets[index];
            index += 1;
        }
        Self {
            octets,
            len: compat.gid_len(),
        }
    }

    /// A GID of `compat`'s width whose octets are all zero.
    ///
    /// `rmw_dds_common` uses this as "no participant".
    #[must_use]
    pub const fn unknown(compat: RosCompat) -> Self {
        Self {
            octets: [0_u8; MAX_GID_LEN],
            len: compat.gid_len(),
        }
    }

    /// Read a GID from octets, taking the width from the slice.
    ///
    /// Accepts exactly the two widths a supported distribution uses; anything
    /// else is `None`, because guessing which sixteen of twenty octets are
    /// the GUID is not a thing a decoder should do.
    #[must_use]
    pub fn from_slice(octets: &[u8]) -> Option<Self> {
        if octets.len() != HUMBLE_GID_LEN && octets.len() != JAZZY_GID_LEN {
            return None;
        }
        let mut buffer = [0_u8; MAX_GID_LEN];
        buffer.get_mut(..octets.len())?.copy_from_slice(octets);
        Some(Self {
            octets: buffer,
            len: octets.len(),
        })
    }

    /// The octets, exactly as wide as the distribution wants them.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        self.octets.get(..self.len).unwrap_or(&self.octets)
    }

    /// The width, in octets.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Always false — a GID has at least sixteen octets.
    ///
    /// Present because `len` without `is_empty` is a clippy warning, and a
    /// silent `#[allow]` would hide the next one.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// The distribution this width belongs to, when it is one of the two.
    #[must_use]
    pub const fn compat(&self) -> Option<RosCompat> {
        match self.len {
            HUMBLE_GID_LEN => Some(RosCompat::Humble),
            JAZZY_GID_LEN => Some(RosCompat::Jazzy),
            _ => None,
        }
    }

    /// The GUID inside, or `None` when the GID is the all-zero "unknown".
    #[must_use]
    pub fn guid(&self) -> Option<Guid> {
        let guid = Guid::from_slice(self.octets.get(..GUID_LEN)?)?;
        if guid.is_unknown() { None } else { Some(guid) }
    }

    /// True when every octet is zero.
    #[must_use]
    pub fn is_unknown(&self) -> bool {
        self.as_slice().iter().all(|octet| *octet == 0)
    }

    /// The same participant, re-encoded at another distribution's width.
    ///
    /// What a bridge does when it forwards a Humble robot's graph to a Jazzy
    /// one.
    #[must_use]
    pub fn to_compat(&self, compat: RosCompat) -> Self {
        Self {
            octets: self.octets,
            len: compat.gid_len(),
        }
    }
}

impl fmt::Display for Gid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.guid() {
            Some(guid) => write!(formatter, "{guid}/{}", self.len),
            None => write!(formatter, "GID_UNKNOWN/{}", self.len),
        }
    }
}

impl CdrType for Gid {
    const MIN_SERIALIZED_SIZE: usize = JAZZY_GID_LEN;
    const IS_PRIMITIVE: bool = false;
}

impl CdrSerialize for Gid {
    fn serialize(&self, writer: &mut CdrWriter) -> CdrResult<()> {
        writer.write_octets(self.as_slice());
        Ok(())
    }
}

impl<'de> CdrDeserialize<'de> for Gid {
    /// Reads the **Jazzy** width.
    ///
    /// A `Gid` is a fixed-size IDL array, so the wire carries no length and a
    /// deserializer has to be told which one it is reading. The blanket impl
    /// takes the narrow form because that is the current release; a Humble
    /// peer's twenty-four octets are read with
    /// [`read_gid`](Gid::read_gid) instead, which takes the width as an
    /// argument.
    fn deserialize(reader: &mut CdrReader<'de>) -> CdrResult<Self> {
        Self::read_gid(reader, RosCompat::Jazzy)
    }
}

impl Gid {
    /// Read a GID of `compat`'s width.
    ///
    /// # Errors
    ///
    /// Whatever the reader reports when the octets run out.
    pub fn read_gid(reader: &mut CdrReader<'_>, compat: RosCompat) -> CdrResult<Self> {
        let octets = reader.read_octets(compat.gid_len())?;
        let mut buffer = [0_u8; MAX_GID_LEN];
        let width = compat.gid_len().min(MAX_GID_LEN);
        if let Some(slot) = buffer.get_mut(..width) {
            slot.copy_from_slice(octets);
        }
        Ok(Self {
            octets: buffer,
            len: width,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::structure::{ENTITYID_PARTICIPANT, GuidPrefix, VendorId};
    use astrs_cdr::Encoding;

    fn sample_guid() -> Guid {
        Guid::new(
            GuidPrefix::vendor_scoped(VendorId::ASTRS, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]),
            ENTITYID_PARTICIPANT,
        )
    }

    #[test]
    fn the_two_widths_are_the_ones_the_blueprint_fixes() {
        assert_eq!(RosCompat::Humble.gid_len(), 24);
        assert_eq!(RosCompat::Jazzy.gid_len(), 16);
        assert_eq!(RosCompat::Humble.gid_padding(), 8);
        assert_eq!(RosCompat::Jazzy.gid_padding(), 0);
        assert!(RosCompat::Humble.pads_gid());
        assert!(!RosCompat::Jazzy.pads_gid());
    }

    #[test]
    fn jazzy_is_the_default() {
        assert_eq!(RosCompat::default(), RosCompat::Jazzy);
        assert!(RosCompat::Jazzy.advertises_xcdr2());
        assert!(!RosCompat::Humble.advertises_xcdr2());
    }

    #[test]
    fn names_round_trip_and_aliases_resolve() {
        for compat in RosCompat::ALL {
            assert_eq!(RosCompat::from_name(compat.name()), Some(compat));
            assert_eq!(compat.to_string(), compat.name());
        }
        assert_eq!(RosCompat::from_name("IRON"), Some(RosCompat::Jazzy));
        assert_eq!(RosCompat::from_name(" Foxy "), Some(RosCompat::Humble));
        assert_eq!(RosCompat::from_name("dashing"), None);
    }

    #[test]
    fn humble_is_jazzy_plus_eight_zeros() {
        let guid = sample_guid();
        let humble = Gid::new(RosCompat::Humble, guid);
        let jazzy = Gid::new(RosCompat::Jazzy, guid);
        assert_eq!(humble.len(), 24);
        assert_eq!(jazzy.len(), 16);
        assert_eq!(&humble.as_slice()[..GUID_LEN], jazzy.as_slice());
        assert_eq!(&humble.as_slice()[GUID_LEN..], &[0_u8; 8]);
        assert!(!humble.is_empty());
    }

    #[test]
    fn both_widths_recover_the_guid() {
        let guid = sample_guid();
        for compat in RosCompat::ALL {
            let gid = compat.gid(guid);
            assert_eq!(gid.guid(), Some(guid), "{compat} lost the GUID");
            assert_eq!(gid.compat(), Some(compat));
            assert!(!gid.is_unknown());
        }
    }

    #[test]
    fn the_unknown_gid_has_no_guid() {
        for compat in RosCompat::ALL {
            let gid = Gid::unknown(compat);
            assert!(gid.is_unknown());
            assert_eq!(gid.guid(), None);
            assert!(gid.to_string().starts_with("GID_UNKNOWN"));
        }
    }

    #[test]
    fn from_slice_accepts_only_the_two_widths() {
        let guid = sample_guid();
        let humble = Gid::new(RosCompat::Humble, guid);
        assert_eq!(Gid::from_slice(humble.as_slice()), Some(humble));

        assert_eq!(Gid::from_slice(&[0_u8; 20]), None, "20 is neither width");
        assert_eq!(Gid::from_slice(&[0_u8; 8]), None);
        assert_eq!(Gid::from_slice(&[]), None);
    }

    #[test]
    fn re_encoding_between_distributions_keeps_the_guid() {
        let guid = sample_guid();
        let humble = Gid::new(RosCompat::Humble, guid);
        let bridged = humble.to_compat(RosCompat::Jazzy);
        assert_eq!(bridged.len(), 16);
        assert_eq!(bridged.guid(), Some(guid));
        assert_eq!(bridged.to_compat(RosCompat::Humble), humble);
    }

    #[test]
    fn each_width_serializes_to_exactly_that_many_octets() {
        let guid = sample_guid();
        for compat in RosCompat::ALL {
            let octets =
                astrs_cdr::to_vec_headerless(&compat.gid(guid), Encoding::ROS2.plain()).unwrap();
            assert_eq!(
                octets.len(),
                compat.gid_len(),
                "{compat} serialized to the wrong width"
            );
            assert_eq!(&octets[..GUID_LEN], &guid.to_bytes());
        }
    }

    #[test]
    fn reading_uses_the_width_it_is_told() {
        let guid = sample_guid();
        let humble = Gid::new(RosCompat::Humble, guid);
        let octets = astrs_cdr::to_vec_headerless(&humble, Encoding::ROS2.plain()).unwrap();

        let mut reader = astrs_cdr::CdrReader::with_encoding(&octets, Encoding::ROS2.plain());
        let read = Gid::read_gid(&mut reader, RosCompat::Humble).unwrap();
        assert_eq!(read, humble);

        // The narrow read stops after sixteen octets and still recovers the
        // GUID, because the padding is at the end.
        let mut narrow = astrs_cdr::CdrReader::with_encoding(&octets, Encoding::ROS2.plain());
        let read = Gid::read_gid(&mut narrow, RosCompat::Jazzy).unwrap();
        assert_eq!(read.len(), 16);
        assert_eq!(read.guid(), Some(guid));
    }

    #[test]
    fn display_names_the_guid_and_the_width() {
        let gid = Gid::new(RosCompat::Humble, sample_guid());
        let rendered = gid.to_string();
        assert!(rendered.ends_with("/24"), "{rendered}");
    }
}
