//! The ten-byte frame header of blueprint §7.1.
//!
//! ```text
//! ┌────────┬─────┬─────┬───────┬──────────┬─────────────┬─────────┐
//! │ magic  │ ver │flags│ kind  │ len:u32  │ payload     │ crc32c  │
//! │ "AS"   │ u8  │ u8  │ u16   │ LE       │ oxicode     │ u32 opt │
//! └────────┴─────┴─────┴───────┴──────────┴─────────────┴─────────┘
//!   0..2     2     3     4..6     6..10      10..10+len   optional
//! ```
//!
//! Two decisions that the diagram leaves open, fixed here and depended on by
//! the rest of the crate:
//!
//! 1. **`len` counts the payload only.** The header is fixed-width and the
//!    checksum is a trailer outside the length, so a reader knows exactly how
//!    many more bytes to pull after parsing ten.
//! 2. **The CRC covers header *and* payload.** Checksumming the payload alone
//!    would let a flipped bit in `kind` or `flags` through as a
//!    wrong-but-valid frame — precisely the corruption a checksum exists to
//!    catch.

use core::fmt;

use crate::error::{WireError, WireResult};
use crate::frame::flags::FrameFlags;
use crate::frame::kind::FrameKind;

/// The two magic bytes every frame starts with.
pub const MAGIC: [u8; 2] = *b"AS";

/// The frame-layout version this build writes and accepts.
///
/// This is the version of the *byte layout*, not the protocol: message
/// semantics are versioned by [`crate::PROTOCOL_VERSION`] and negotiated in
/// the handshake. The layout has been `1` since the first release and is
/// expected to stay there.
pub const FRAME_VERSION: u8 = 1;

/// Size of the fixed frame header in bytes: `2 + 1 + 1 + 2 + 4`.
pub const HEADER_LEN: usize = 10;

/// Size of the optional CRC-32C trailer in bytes.
pub const CRC_LEN: usize = 4;

/// Byte offset of the `ver` field within the header.
pub const VERSION_OFFSET: usize = 2;
/// Byte offset of the `flags` field within the header.
pub const FLAGS_OFFSET: usize = 3;
/// Byte offset of the `kind` field within the header.
pub const KIND_OFFSET: usize = 4;
/// Byte offset of the `len` field within the header.
pub const LEN_OFFSET: usize = 6;

/// A parsed frame header.
///
/// # Examples
///
/// ```
/// use astrs_wire::{FrameFlags, FrameHeader, FrameKind, HEADER_LEN};
///
/// let header = FrameHeader::new(FrameKind::Control, FrameFlags::CRC, 42);
/// let bytes = header.to_bytes();
/// assert_eq!(bytes.len(), HEADER_LEN);
/// assert_eq!(&bytes[..2], b"AS");
///
/// let parsed = FrameHeader::parse(&bytes)?;
/// assert_eq!(parsed, header);
/// assert_eq!(parsed.total_len(), HEADER_LEN + 42 + 4);
/// # Ok::<(), astrs_wire::WireError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FrameHeader {
    /// The frame-layout version byte.
    version: u8,
    /// The flags byte.
    flags: FrameFlags,
    /// The message family.
    kind: FrameKind,
    /// The payload length in bytes, excluding header and trailer.
    payload_len: u32,
}

impl FrameHeader {
    /// Builds a header for a payload of `payload_len` bytes.
    ///
    /// The version is always [`FRAME_VERSION`]; there is no way to write a
    /// frame claiming a layout this build does not implement.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{FrameFlags, FrameHeader, FrameKind};
    ///
    /// let header = FrameHeader::new(FrameKind::Log, FrameFlags::EMPTY, 0);
    /// assert_eq!(header.payload_len(), 0);
    /// assert_eq!(header.kind(), FrameKind::Log);
    /// ```
    #[must_use]
    pub const fn new(kind: FrameKind, flags: FrameFlags, payload_len: u32) -> Self {
        Self {
            version: FRAME_VERSION,
            flags,
            kind,
            payload_len,
        }
    }

    /// The frame-layout version byte.
    #[must_use]
    pub const fn version(&self) -> u8 {
        self.version
    }

    /// The flags byte.
    #[must_use]
    pub const fn flags(&self) -> FrameFlags {
        self.flags
    }

    /// The message family.
    #[must_use]
    pub const fn kind(&self) -> FrameKind {
        self.kind
    }

    /// The payload length in bytes, excluding header and trailer.
    #[must_use]
    pub const fn payload_len(&self) -> u32 {
        self.payload_len
    }

    /// The payload length as a `usize`.
    ///
    /// On a 16-bit target this would be lossy; AstRS targets 32- and 64-bit
    /// platforms, where `u32` always fits, and the conversion is checked so
    /// the lossy case is an error rather than a silent truncation.
    ///
    /// # Errors
    ///
    /// [`WireError::FrameTooLarge`] if the declared length does not fit in a
    /// `usize` on this platform.
    pub fn payload_len_usize(&self) -> WireResult<usize> {
        usize::try_from(self.payload_len).map_err(|_| WireError::FrameTooLarge {
            len: usize::MAX,
            max: usize::MAX,
        })
    }

    /// Whether a CRC-32C trailer follows the payload.
    #[must_use]
    pub const fn has_crc(&self) -> bool {
        self.flags.has_crc()
    }

    /// The trailer length: [`CRC_LEN`] when a checksum is present, else zero.
    #[must_use]
    pub const fn trailer_len(&self) -> usize {
        if self.has_crc() { CRC_LEN } else { 0 }
    }

    /// The total on-wire size of the frame: header + payload + trailer.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{FrameFlags, FrameHeader, FrameKind};
    ///
    /// let plain = FrameHeader::new(FrameKind::Data, FrameFlags::EMPTY, 5);
    /// assert_eq!(plain.total_len(), 10 + 5);
    ///
    /// let checked = FrameHeader::new(FrameKind::Data, FrameFlags::CRC, 5);
    /// assert_eq!(checked.total_len(), 10 + 5 + 4);
    /// ```
    #[must_use]
    pub const fn total_len(&self) -> usize {
        HEADER_LEN + self.payload_len as usize + self.trailer_len()
    }

    /// Serialises the header to its ten wire bytes.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{FrameFlags, FrameHeader, FrameKind};
    ///
    /// let bytes = FrameHeader::new(FrameKind::PeerEvent, FrameFlags::CRC, 258).to_bytes();
    /// assert_eq!(bytes, [b'A', b'S', 1, 0x01, 0x06, 0x00, 0x02, 0x01, 0x00, 0x00]);
    /// ```
    #[must_use]
    pub const fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let kind = self.kind.as_u16().to_le_bytes();
        let len = self.payload_len.to_le_bytes();
        [
            MAGIC[0],
            MAGIC[1],
            self.version,
            self.flags.bits(),
            kind[0],
            kind[1],
            len[0],
            len[1],
            len[2],
            len[3],
        ]
    }

    /// Writes the header into the first ten bytes of `dst`.
    ///
    /// Used to backfill a placeholder header once the payload length is
    /// known, which is what lets [`crate::write_message`] encode a message
    /// straight into the output buffer with no intermediate copy.
    ///
    /// # Errors
    ///
    /// [`WireError::Truncated`] if `dst` is shorter than [`HEADER_LEN`].
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{FrameFlags, FrameHeader, FrameKind, HEADER_LEN};
    ///
    /// let mut buffer = vec![0u8; HEADER_LEN];
    /// FrameHeader::new(FrameKind::Control, FrameFlags::EMPTY, 3).write_into(&mut buffer)?;
    /// assert_eq!(&buffer[..2], b"AS");
    /// # Ok::<(), astrs_wire::WireError>(())
    /// ```
    pub fn write_into(&self, dst: &mut [u8]) -> WireResult<()> {
        let available = dst.len();
        let slot = dst.get_mut(..HEADER_LEN).ok_or(WireError::Truncated {
            expected: HEADER_LEN,
            found: available,
        })?;
        slot.copy_from_slice(&self.to_bytes());
        Ok(())
    }

    /// Parses a header from the front of `bytes`.
    ///
    /// Validates the magic, the layout version, the flags byte and the kind
    /// discriminant. It deliberately does **not** validate the payload length
    /// against any limit: that check belongs to the caller, which knows the
    /// connection's [`crate::FrameLimits`], and is performed before any buffer
    /// is sized.
    ///
    /// # Errors
    ///
    /// - [`WireError::Truncated`] if fewer than [`HEADER_LEN`] bytes are
    ///   available.
    /// - [`WireError::BadMagic`] if the first two bytes are not `"AS"`.
    /// - [`WireError::UnsupportedFrameVersion`] if the layout version is not
    ///   [`FRAME_VERSION`].
    /// - [`WireError::ReservedFlags`] / [`WireError::ConflictingCompression`]
    ///   for a malformed flags byte.
    /// - [`WireError::UnknownKind`] for an unknown message family.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{FrameHeader, WireError};
    ///
    /// assert!(matches!(
    ///     FrameHeader::parse(b"NO"),
    ///     Err(WireError::Truncated { .. })
    /// ));
    /// assert!(matches!(
    ///     FrameHeader::parse(b"NOPE\0\0\0\0\0\0"),
    ///     Err(WireError::BadMagic { .. })
    /// ));
    /// ```
    pub fn parse(bytes: &[u8]) -> WireResult<Self> {
        let header = bytes.get(..HEADER_LEN).ok_or(WireError::Truncated {
            expected: HEADER_LEN,
            found: bytes.len(),
        })?;

        let found_magic = [header[0], header[1]];
        if found_magic != MAGIC {
            return Err(WireError::BadMagic {
                expected: MAGIC,
                found: found_magic,
            });
        }

        let version = header[VERSION_OFFSET];
        if version != FRAME_VERSION {
            return Err(WireError::UnsupportedFrameVersion {
                found: version,
                supported: FRAME_VERSION,
            });
        }

        let flags = FrameFlags::from_bits(header[FLAGS_OFFSET])?;
        let kind = FrameKind::from_u16(u16::from_le_bytes([
            header[KIND_OFFSET],
            header[KIND_OFFSET + 1],
        ]))?;
        let payload_len = u32::from_le_bytes([
            header[LEN_OFFSET],
            header[LEN_OFFSET + 1],
            header[LEN_OFFSET + 2],
            header[LEN_OFFSET + 3],
        ]);

        Ok(Self {
            version,
            flags,
            kind,
            payload_len,
        })
    }
}

impl fmt::Display for FrameHeader {
    /// Renders as `kind/flags/len`, e.g. `control/crc/42`.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{FrameFlags, FrameHeader, FrameKind};
    ///
    /// let header = FrameHeader::new(FrameKind::Control, FrameFlags::CRC, 42);
    /// assert_eq!(header.to_string(), "control/crc/42");
    /// ```
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.kind, self.flags, self.payload_len)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn sample() -> FrameHeader {
        FrameHeader::new(
            FrameKind::NodeEvent,
            FrameFlags::CRC | FrameFlags::LZ4,
            0x0102_0304,
        )
    }

    #[test]
    fn header_is_ten_bytes_in_the_documented_order() {
        let bytes = sample().to_bytes();
        assert_eq!(bytes.len(), HEADER_LEN);
        assert_eq!(&bytes[..2], b"AS");
        assert_eq!(bytes[VERSION_OFFSET], FRAME_VERSION);
        assert_eq!(bytes[FLAGS_OFFSET], 0b0000_0011);
        assert_eq!(&bytes[KIND_OFFSET..KIND_OFFSET + 2], &[5, 0]);
        assert_eq!(&bytes[LEN_OFFSET..], &[0x04, 0x03, 0x02, 0x01]);
    }

    #[test]
    fn round_trips_through_bytes() {
        for &kind in FrameKind::ALL {
            for bits in [0u8, 1, 2, 4, 5] {
                let flags = FrameFlags::from_bits(bits).unwrap();
                for len in [0u32, 1, 255, 256, 65_535, 65_536, u32::MAX] {
                    let header = FrameHeader::new(kind, flags, len);
                    let parsed = FrameHeader::parse(&header.to_bytes()).unwrap();
                    assert_eq!(parsed, header);
                }
            }
        }
    }

    #[test]
    fn parse_ignores_bytes_past_the_header() {
        let mut bytes = sample().to_bytes().to_vec();
        bytes.extend_from_slice(b"payload and more");
        assert_eq!(FrameHeader::parse(&bytes).unwrap(), sample());
    }

    #[test]
    fn short_input_is_truncation_not_corruption() {
        let bytes = sample().to_bytes();
        for len in 0..HEADER_LEN {
            match FrameHeader::parse(&bytes[..len]) {
                Err(WireError::Truncated { expected, found }) => {
                    assert_eq!(expected, HEADER_LEN);
                    assert_eq!(found, len);
                }
                other => panic!("expected Truncated at length {len}, got {other:?}"),
            }
        }
    }

    #[test]
    fn bad_magic_is_reported_with_both_values() {
        let mut bytes = sample().to_bytes();
        bytes[0] = b'X';
        match FrameHeader::parse(&bytes) {
            Err(WireError::BadMagic { expected, found }) => {
                assert_eq!(expected, MAGIC);
                assert_eq!(found, [b'X', b'S']);
            }
            other => panic!("expected BadMagic, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_layout_version_is_rejected() {
        let mut bytes = sample().to_bytes();
        for version in [0u8, 2, 255] {
            bytes[VERSION_OFFSET] = version;
            match FrameHeader::parse(&bytes) {
                Err(WireError::UnsupportedFrameVersion { found, supported }) => {
                    assert_eq!(found, version);
                    assert_eq!(supported, FRAME_VERSION);
                }
                other => panic!("expected UnsupportedFrameVersion, got {other:?}"),
            }
        }
    }

    #[test]
    fn reserved_flag_bits_are_rejected() {
        let mut bytes = sample().to_bytes();
        bytes[FLAGS_OFFSET] = 0b1000_0001;
        assert!(matches!(
            FrameHeader::parse(&bytes),
            Err(WireError::ReservedFlags { .. })
        ));
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let mut bytes = sample().to_bytes();
        bytes[KIND_OFFSET] = 0xFF;
        bytes[KIND_OFFSET + 1] = 0xFF;
        assert!(matches!(
            FrameHeader::parse(&bytes),
            Err(WireError::UnknownKind { found: 0xFFFF })
        ));
    }

    #[test]
    fn total_len_accounts_for_the_optional_trailer() {
        let plain = FrameHeader::new(FrameKind::Data, FrameFlags::EMPTY, 7);
        assert_eq!(plain.trailer_len(), 0);
        assert_eq!(plain.total_len(), HEADER_LEN + 7);

        let checked = FrameHeader::new(FrameKind::Data, FrameFlags::CRC, 7);
        assert_eq!(checked.trailer_len(), CRC_LEN);
        assert_eq!(checked.total_len(), HEADER_LEN + 7 + CRC_LEN);
    }

    #[test]
    fn write_into_backfills_a_placeholder() {
        let mut buffer = vec![0xAAu8; HEADER_LEN + 4];
        let header = sample();
        header.write_into(&mut buffer).unwrap();
        assert_eq!(&buffer[..HEADER_LEN], &header.to_bytes());
        assert_eq!(&buffer[HEADER_LEN..], &[0xAA; 4]);
    }

    #[test]
    fn write_into_refuses_a_short_destination() {
        let mut buffer = vec![0u8; HEADER_LEN - 1];
        match sample().write_into(&mut buffer) {
            Err(WireError::Truncated { expected, found }) => {
                assert_eq!(expected, HEADER_LEN);
                assert_eq!(found, HEADER_LEN - 1);
            }
            other => panic!("expected Truncated, got {other:?}"),
        }
    }

    #[test]
    fn payload_len_usize_matches_on_supported_targets() {
        let header = FrameHeader::new(FrameKind::Data, FrameFlags::EMPTY, u32::MAX);
        assert_eq!(header.payload_len_usize().unwrap(), u32::MAX as usize);
    }

    #[test]
    fn display_is_compact_and_informative() {
        let header = FrameHeader::new(FrameKind::Control, FrameFlags::CRC, 42);
        assert_eq!(header.to_string(), "control/crc/42");
        let header = FrameHeader::new(FrameKind::Data, FrameFlags::EMPTY, 0);
        assert_eq!(header.to_string(), "data/-/0");
    }
}
