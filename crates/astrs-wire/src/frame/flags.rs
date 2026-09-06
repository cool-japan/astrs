//! The `flags` header byte: CRC presence and payload compression.
//!
//! Blueprint §7.1 assigns bit 0 to "crc32c trailer present", bit 1 to lz4 and
//! bit 2 to zstd. Bits 3..7 are reserved and **must be zero**: a decoder that
//! ignored them could half-understand a frame from a newer peer, which is
//! exactly the silent-drift failure mode append-only evolution exists to
//! prevent.
//!
//! The compression bits are defined and negotiated here; the actual
//! compression is applied by `astrs-transport` at route setup (§6.4), which
//! owns the oxiarc codecs and the 16 KiB threshold policy. This crate reads
//! and writes the bits, validates that at most one is set, and never
//! compresses anything itself.

use core::fmt;
use core::ops::{BitAnd, BitOr, BitOrAssign, Sub, SubAssign};

use oxicode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use crate::error::WireError;

/// Which compression codec a frame's payload was encoded with.
///
/// Also used at route setup to record the negotiated codec for a route
/// (blueprint §6.4).
///
/// # Examples
///
/// ```
/// use astrs_wire::{Compression, FrameFlags};
///
/// let flags = FrameFlags::CRC.with_compression(Compression::Zstd);
/// assert!(flags.has_crc());
/// assert_eq!(flags.compression(), Compression::Zstd);
/// ```
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    Encode,
    Decode,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Compression {
    /// The payload is stored verbatim.
    #[default]
    #[oxicode(variant = 0)]
    None,
    /// The payload is lz4-compressed (`oxiarc-lz4`).
    #[oxicode(variant = 1)]
    Lz4,
    /// The payload is zstd-compressed (`oxiarc-zstd`).
    #[oxicode(variant = 2)]
    Zstd,
}

impl Compression {
    /// Every codec this build knows, in variant order.
    pub const ALL: &'static [Self] = &[Self::None, Self::Lz4, Self::Zstd];

    /// A stable, lower-case name for manifests, logs and metrics labels.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::Compression;
    ///
    /// assert_eq!(Compression::Lz4.as_str(), "lz4");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Lz4 => "lz4",
            Self::Zstd => "zstd",
        }
    }

    /// Whether this codec actually transforms the payload.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::Compression;
    ///
    /// assert!(!Compression::None.is_enabled());
    /// assert!(Compression::Zstd.is_enabled());
    /// ```
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        !matches!(self, Self::None)
    }
}

impl fmt::Display for Compression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The `flags` byte of a frame header.
///
/// A hand-rolled bit set — the workspace dependency policy (§18.1) has no
/// `bitflags` crate and this is eight bits with three meanings.
///
/// # Examples
///
/// ```
/// use astrs_wire::{Compression, FrameFlags};
///
/// let flags = FrameFlags::CRC | FrameFlags::LZ4;
/// assert_eq!(flags.bits(), 0b0000_0011);
/// assert!(flags.contains(FrameFlags::CRC));
/// assert_eq!(flags.compression(), Compression::Lz4);
///
/// let plain = flags - FrameFlags::LZ4;
/// assert_eq!(plain.compression(), Compression::None);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrameFlags(u8);

impl FrameFlags {
    /// No flags set: no checksum, no compression.
    pub const EMPTY: Self = Self(0);
    /// Bit 0 — a 4-byte little-endian CRC-32C trailer follows the payload.
    pub const CRC: Self = Self(0b0000_0001);
    /// Bit 1 — the payload is lz4-compressed.
    pub const LZ4: Self = Self(0b0000_0010);
    /// Bit 2 — the payload is zstd-compressed.
    pub const ZSTD: Self = Self(0b0000_0100);

    /// The bits this build defines. Everything else is reserved.
    pub const KNOWN_MASK: u8 = 0b0000_0111;
    /// The two compression bits, as a mask.
    pub const COMPRESSION_MASK: u8 = 0b0000_0110;

    /// The raw byte.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::FrameFlags;
    ///
    /// assert_eq!(FrameFlags::ZSTD.bits(), 4);
    /// ```
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Parses a `flags` byte, rejecting reserved and contradictory bits.
    ///
    /// # Errors
    ///
    /// - [`WireError::ReservedFlags`] if any bit outside
    ///   [`FrameFlags::KNOWN_MASK`] is set.
    /// - [`WireError::ConflictingCompression`] if both compression bits are
    ///   set.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{FrameFlags, WireError};
    ///
    /// assert_eq!(FrameFlags::from_bits(0b0000_0101)?, FrameFlags::CRC | FrameFlags::ZSTD);
    /// assert!(matches!(
    ///     FrameFlags::from_bits(0b1000_0000),
    ///     Err(WireError::ReservedFlags { .. })
    /// ));
    /// assert!(matches!(
    ///     FrameFlags::from_bits(0b0000_0110),
    ///     Err(WireError::ConflictingCompression)
    /// ));
    /// # Ok::<(), astrs_wire::WireError>(())
    /// ```
    pub const fn from_bits(bits: u8) -> Result<Self, WireError> {
        let reserved = bits & !Self::KNOWN_MASK;
        if reserved != 0 {
            return Err(WireError::ReservedFlags { bits, reserved });
        }
        if bits & Self::COMPRESSION_MASK == Self::COMPRESSION_MASK {
            return Err(WireError::ConflictingCompression);
        }
        Ok(Self(bits))
    }

    /// Parses a `flags` byte, silently discarding reserved bits.
    ///
    /// Provided for diagnostics and tooling (a frame dumper that wants to
    /// describe a frame it would otherwise refuse). The framing code paths
    /// always use [`FrameFlags::from_bits`].
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::FrameFlags;
    ///
    /// assert_eq!(FrameFlags::from_bits_truncate(0b1111_0001), FrameFlags::CRC);
    /// ```
    #[must_use]
    pub const fn from_bits_truncate(bits: u8) -> Self {
        Self(bits & Self::KNOWN_MASK)
    }

    /// Whether every bit of `other` is set in `self`.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::FrameFlags;
    ///
    /// let flags = FrameFlags::CRC | FrameFlags::LZ4;
    /// assert!(flags.contains(FrameFlags::CRC));
    /// assert!(!flags.contains(FrameFlags::ZSTD));
    /// assert!(flags.contains(FrameFlags::EMPTY));
    /// ```
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether no bits are set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether the frame carries a CRC-32C trailer.
    #[must_use]
    pub const fn has_crc(self) -> bool {
        self.contains(Self::CRC)
    }

    /// Returns these flags with the CRC bit set or cleared.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::FrameFlags;
    ///
    /// assert!(FrameFlags::EMPTY.with_crc(true).has_crc());
    /// assert!(!FrameFlags::CRC.with_crc(false).has_crc());
    /// ```
    #[must_use]
    pub const fn with_crc(self, enabled: bool) -> Self {
        if enabled {
            Self(self.0 | Self::CRC.0)
        } else {
            Self(self.0 & !Self::CRC.0)
        }
    }

    /// The compression codec these flags select.
    ///
    /// A byte with both compression bits set can never reach this method
    /// through [`FrameFlags::from_bits`]; if one is constructed directly the
    /// lz4 bit wins, which is the conservative reading (lz4 is the lower bit
    /// and the cheaper codec to attempt).
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{Compression, FrameFlags};
    ///
    /// assert_eq!(FrameFlags::EMPTY.compression(), Compression::None);
    /// assert_eq!(FrameFlags::LZ4.compression(), Compression::Lz4);
    /// assert_eq!(FrameFlags::ZSTD.compression(), Compression::Zstd);
    /// ```
    #[must_use]
    pub const fn compression(self) -> Compression {
        if self.0 & Self::LZ4.0 != 0 {
            Compression::Lz4
        } else if self.0 & Self::ZSTD.0 != 0 {
            Compression::Zstd
        } else {
            Compression::None
        }
    }

    /// Returns these flags with the compression bits replaced.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{Compression, FrameFlags};
    ///
    /// let flags = FrameFlags::CRC.with_compression(Compression::Lz4);
    /// assert_eq!(flags.bits(), 0b0000_0011);
    /// let flags = flags.with_compression(Compression::None);
    /// assert_eq!(flags.bits(), 0b0000_0001);
    /// ```
    #[must_use]
    pub const fn with_compression(self, compression: Compression) -> Self {
        let cleared = self.0 & !Self::COMPRESSION_MASK;
        match compression {
            Compression::None => Self(cleared),
            Compression::Lz4 => Self(cleared | Self::LZ4.0),
            Compression::Zstd => Self(cleared | Self::ZSTD.0),
        }
    }
}

impl BitOr for FrameFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for FrameFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for FrameFlags {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

impl Sub for FrameFlags {
    type Output = Self;

    /// Set difference: removes every bit of `rhs` from `self`.
    fn sub(self, rhs: Self) -> Self {
        Self(self.0 & !rhs.0)
    }
}

impl SubAssign for FrameFlags {
    fn sub_assign(&mut self, rhs: Self) {
        self.0 &= !rhs.0;
    }
}

impl fmt::Display for FrameFlags {
    /// Renders as a `|`-separated list of set bits, or `-` when empty.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::FrameFlags;
    ///
    /// assert_eq!((FrameFlags::CRC | FrameFlags::LZ4).to_string(), "crc|lz4");
    /// assert_eq!(FrameFlags::EMPTY.to_string(), "-");
    /// ```
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("-");
        }
        let mut first = true;
        for (flag, name) in [(Self::CRC, "crc"), (Self::LZ4, "lz4"), (Self::ZSTD, "zstd")] {
            if self.contains(flag) {
                if !first {
                    f.write_str("|")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        // Any reserved bit that got in through `FrameFlags(..)` internals is
        // still reported, so a diagnostic dump never lies by omission.
        let reserved = self.0 & !Self::KNOWN_MASK;
        if reserved != 0 {
            if !first {
                f.write_str("|")?;
            }
            write!(f, "reserved(0x{reserved:02x})")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn bit_positions_match_the_specification() {
        assert_eq!(FrameFlags::CRC.bits(), 0b0000_0001);
        assert_eq!(FrameFlags::LZ4.bits(), 0b0000_0010);
        assert_eq!(FrameFlags::ZSTD.bits(), 0b0000_0100);
        assert_eq!(FrameFlags::EMPTY.bits(), 0);
        assert_eq!(FrameFlags::default(), FrameFlags::EMPTY);
    }

    #[test]
    fn from_bits_accepts_every_valid_combination() {
        let valid = [
            0b0000_0000,
            0b0000_0001,
            0b0000_0010,
            0b0000_0011,
            0b0000_0100,
            0b0000_0101,
        ];
        for bits in valid {
            let flags = FrameFlags::from_bits(bits).unwrap();
            assert_eq!(flags.bits(), bits);
        }
    }

    #[test]
    fn from_bits_rejects_every_reserved_bit() {
        for bit in 3..8u8 {
            let bits = 1u8 << bit;
            match FrameFlags::from_bits(bits) {
                Err(WireError::ReservedFlags { bits: b, reserved }) => {
                    assert_eq!(b, bits);
                    assert_eq!(reserved, bits);
                }
                other => panic!("bit {bit} should be reserved, got {other:?}"),
            }
        }
    }

    #[test]
    fn from_bits_rejects_both_compression_bits() {
        assert!(matches!(
            FrameFlags::from_bits(FrameFlags::COMPRESSION_MASK),
            Err(WireError::ConflictingCompression)
        ));
        assert!(matches!(
            FrameFlags::from_bits(FrameFlags::COMPRESSION_MASK | 1),
            Err(WireError::ConflictingCompression)
        ));
    }

    #[test]
    fn exhaustive_byte_sweep_partitions_valid_and_invalid() {
        let mut accepted = 0;
        for bits in 0u8..=255 {
            match FrameFlags::from_bits(bits) {
                Ok(flags) => {
                    accepted += 1;
                    assert_eq!(flags.bits(), bits);
                    assert!(bits & !FrameFlags::KNOWN_MASK == 0);
                }
                Err(WireError::ReservedFlags { .. }) => {
                    assert_ne!(bits & !FrameFlags::KNOWN_MASK, 0);
                }
                Err(WireError::ConflictingCompression) => {
                    assert_eq!(
                        bits & FrameFlags::COMPRESSION_MASK,
                        FrameFlags::COMPRESSION_MASK
                    );
                    assert_eq!(bits & !FrameFlags::KNOWN_MASK, 0);
                }
                other => panic!("unexpected result for {bits:#010b}: {other:?}"),
            }
        }
        // 0b000, 0b001, 0b010, 0b011, 0b100, 0b101 — six legal bytes.
        assert_eq!(accepted, 6);
    }

    #[test]
    fn truncate_keeps_only_known_bits() {
        assert_eq!(
            FrameFlags::from_bits_truncate(0xFF).bits(),
            FrameFlags::KNOWN_MASK
        );
        assert_eq!(FrameFlags::from_bits_truncate(0x08), FrameFlags::EMPTY);
    }

    #[test]
    fn set_operations_behave() {
        let both = FrameFlags::CRC | FrameFlags::ZSTD;
        assert!(both.contains(FrameFlags::CRC));
        assert!(both.contains(FrameFlags::ZSTD));
        assert!(!both.contains(FrameFlags::LZ4));
        assert_eq!(both & FrameFlags::CRC, FrameFlags::CRC);
        assert_eq!(both - FrameFlags::ZSTD, FrameFlags::CRC);

        let mut acc = FrameFlags::EMPTY;
        acc |= FrameFlags::LZ4;
        assert_eq!(acc, FrameFlags::LZ4);
        acc -= FrameFlags::LZ4;
        assert!(acc.is_empty());
    }

    #[test]
    fn compression_accessors_round_trip() {
        for &compression in Compression::ALL {
            let flags = FrameFlags::CRC.with_compression(compression);
            assert_eq!(flags.compression(), compression);
            assert!(flags.has_crc());
            // Setting compression must not disturb the CRC bit.
            assert_eq!(flags.with_crc(false).compression(), compression);
        }
    }

    #[test]
    fn compression_bits_are_mutually_exclusive_by_construction() {
        let flags = FrameFlags::LZ4.with_compression(Compression::Zstd);
        assert_eq!(flags.bits(), FrameFlags::ZSTD.bits());
        assert_eq!(flags.compression(), Compression::Zstd);
    }

    #[test]
    fn display_lists_set_bits() {
        assert_eq!(FrameFlags::EMPTY.to_string(), "-");
        assert_eq!(FrameFlags::CRC.to_string(), "crc");
        assert_eq!((FrameFlags::CRC | FrameFlags::ZSTD).to_string(), "crc|zstd");
        assert_eq!(
            FrameFlags::from_bits_truncate(0xFF).to_string(),
            "crc|lz4|zstd"
        );
    }

    #[test]
    fn compression_names_and_flags() {
        assert_eq!(Compression::None.to_string(), "none");
        assert_eq!(Compression::Lz4.to_string(), "lz4");
        assert_eq!(Compression::Zstd.to_string(), "zstd");
        assert!(!Compression::None.is_enabled());
        assert!(Compression::Lz4.is_enabled());
        assert!(Compression::Zstd.is_enabled());
        assert_eq!(Compression::default(), Compression::None);
    }

    #[test]
    fn compression_round_trips_through_the_codec() {
        use crate::codec::round_trip;

        for &compression in Compression::ALL {
            assert_eq!(round_trip(&compression).unwrap(), compression);
        }
    }
}
