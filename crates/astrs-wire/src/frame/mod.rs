//! The framed codec: one frame format for every leg of the control plane.
//!
//! Blueprint §7.1 defines a single frame layout shared by CLI↔coordinator,
//! coordinator↔daemon, daemon↔node and daemon↔daemon links, whatever the
//! transport underneath. This module implements it end to end:
//!
//! - [`FrameHeader`] — the ten fixed bytes ([`header`]).
//! - [`FrameFlags`] / [`Compression`] — the flags byte ([`flags`]).
//! - [`FrameKind`] — the family discriminant ([`kind`]).
//! - [`FrameLimits`] — the size and integrity policy ([`limits`]).
//! - [`Frame`] / [`FrameView`] — an owned frame and a borrowed view of one.
//! - [`write_message`] / [`decode_frame`] — the encode and decode entry
//!   points.
//!
//! # Examples
//!
//! ```
//! use astrs_wire::{FrameFlags, FrameKind, FrameLimits, decode_frame, encode_message};
//!
//! let limits = FrameLimits::network();
//! let bytes = encode_message(FrameKind::Data, FrameFlags::CRC, &7u32, &limits)?;
//!
//! let frame = decode_frame(&bytes, &limits)?;
//! assert_eq!(frame.kind(), FrameKind::Data);
//! assert_eq!(frame.decode::<u32>()?, 7);
//! # Ok::<(), astrs_wire::WireError>(())
//! ```

pub mod flags;
pub mod header;
pub mod kind;
pub mod limits;

use crate::codec::{WireDecode, WireEncode};
use crate::crc32c::{Crc32c, crc32c};
use crate::error::{WireError, WireResult};

pub use flags::{Compression, FrameFlags};
pub use header::{
    CRC_LEN, FLAGS_OFFSET, FRAME_VERSION, FrameHeader, HEADER_LEN, KIND_OFFSET, LEN_OFFSET, MAGIC,
    VERSION_OFFSET,
};
pub use kind::FrameKind;
pub use limits::{
    DEFAULT_MAX_PAYLOAD_BYTES, FrameLimits, MAX_SUPPORTED_PAYLOAD_BYTES, MIN_MAX_PAYLOAD_BYTES,
};

/// A borrowed view of one complete frame sitting in a caller-owned buffer.
///
/// Decoding produces a `FrameView` rather than a copy: the payload is a
/// sub-slice of the input, so a router can inspect `kind` and forward the
/// bytes without ever allocating.
///
/// # Examples
///
/// ```
/// use astrs_wire::{FrameFlags, FrameKind, FrameLimits, decode_frame, encode_frame};
///
/// let limits = FrameLimits::default();
/// let bytes = encode_frame(FrameKind::Log, FrameFlags::EMPTY, b"raw", &limits)?;
/// let view = decode_frame(&bytes, &limits)?;
/// assert_eq!(view.payload(), b"raw");
/// assert_eq!(view.total_len(), bytes.len());
/// # Ok::<(), astrs_wire::WireError>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameView<'a> {
    /// The parsed header.
    header: FrameHeader,
    /// The payload bytes, exactly `header.payload_len()` long.
    payload: &'a [u8],
}

impl<'a> FrameView<'a> {
    /// The parsed header.
    #[must_use]
    pub const fn header(&self) -> FrameHeader {
        self.header
    }

    /// The message family.
    #[must_use]
    pub const fn kind(&self) -> FrameKind {
        self.header.kind()
    }

    /// The flags byte.
    #[must_use]
    pub const fn flags(&self) -> FrameFlags {
        self.header.flags()
    }

    /// The payload bytes, still encoded (and still compressed, if the flags
    /// say so).
    #[must_use]
    pub const fn payload(&self) -> &'a [u8] {
        self.payload
    }

    /// The total on-wire length of this frame, including header and trailer.
    ///
    /// A stream reader advances its buffer by exactly this much.
    #[must_use]
    pub const fn total_len(&self) -> usize {
        self.header.total_len()
    }

    /// Copies this view into an owned [`Frame`].
    #[must_use]
    pub fn to_owned_frame(&self) -> Frame {
        Frame {
            header: self.header,
            payload: self.payload.to_vec(),
        }
    }

    /// Fails unless this frame belongs to `expected`.
    ///
    /// # Errors
    ///
    /// [`WireError::KindMismatch`] when the families differ.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{FrameFlags, FrameKind, FrameLimits, decode_frame, encode_frame};
    ///
    /// let limits = FrameLimits::default();
    /// let bytes = encode_frame(FrameKind::Log, FrameFlags::EMPTY, b"", &limits)?;
    /// let view = decode_frame(&bytes, &limits)?;
    /// assert!(view.expect_kind(FrameKind::Log).is_ok());
    /// assert!(view.expect_kind(FrameKind::Data).is_err());
    /// # Ok::<(), astrs_wire::WireError>(())
    /// ```
    pub fn expect_kind(&self, expected: FrameKind) -> WireResult<()> {
        let found = self.kind();
        if found == expected {
            Ok(())
        } else {
            Err(WireError::KindMismatch { expected, found })
        }
    }

    /// Decodes the payload as `T`, rejecting trailing bytes.
    ///
    /// # Errors
    ///
    /// - [`WireError::CompressedPayload`] if the frame's flags mark the
    ///   payload as compressed — decompression belongs to `astrs-transport`,
    ///   which owns the oxiarc codecs (blueprint §6.4).
    /// - [`WireError::Codec`] or [`WireError::TrailingBytes`] from the codec.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{FrameFlags, FrameKind, FrameLimits, decode_frame, encode_message};
    ///
    /// let limits = FrameLimits::default();
    /// let bytes = encode_message(FrameKind::Control, FrameFlags::EMPTY, &1u8, &limits)?;
    /// assert_eq!(decode_frame(&bytes, &limits)?.decode::<u8>()?, 1);
    /// # Ok::<(), astrs_wire::WireError>(())
    /// ```
    pub fn decode<T: WireDecode>(&self) -> WireResult<T> {
        let compression = self.flags().compression();
        if compression.is_enabled() {
            return Err(WireError::CompressedPayload { compression });
        }
        T::decode_exact(self.payload)
    }

    /// Decodes the payload as `T` after checking the family.
    ///
    /// # Errors
    ///
    /// As [`FrameView::expect_kind`] and [`FrameView::decode`].
    pub fn decode_as<T: WireDecode>(&self, expected: FrameKind) -> WireResult<T> {
        self.expect_kind(expected)?;
        self.decode()
    }
}

/// An owned frame: a header plus its payload bytes.
///
/// Produced by the framed readers ([`crate::FrameReader`],
/// [`crate::AsyncFrameReader`]), which must own the bytes they pulled off the
/// stream.
///
/// # Examples
///
/// ```
/// use astrs_wire::{Frame, FrameFlags, FrameKind, FrameLimits};
///
/// let frame = Frame::new(FrameKind::Data, FrameFlags::CRC, b"hello".to_vec())?;
/// let bytes = frame.encode(&FrameLimits::default())?;
/// assert_eq!(bytes.len(), frame.header().total_len());
/// # Ok::<(), astrs_wire::WireError>(())
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Frame {
    /// The header, whose `payload_len` always matches `payload.len()`.
    header: FrameHeader,
    /// The payload bytes.
    payload: Vec<u8>,
}

impl Frame {
    /// Builds a frame around an owned payload.
    ///
    /// # Errors
    ///
    /// [`WireError::FrameTooLarge`] if the payload is longer than the `len`
    /// header field can describe ([`MAX_SUPPORTED_PAYLOAD_BYTES`]).
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{Frame, FrameFlags, FrameKind};
    ///
    /// let frame = Frame::new(FrameKind::Log, FrameFlags::EMPTY, vec![1, 2, 3])?;
    /// assert_eq!(frame.payload(), &[1, 2, 3]);
    /// # Ok::<(), astrs_wire::WireError>(())
    /// ```
    pub fn new(kind: FrameKind, flags: FrameFlags, payload: Vec<u8>) -> WireResult<Self> {
        let len = u32::try_from(payload.len()).map_err(|_| WireError::FrameTooLarge {
            len: payload.len(),
            max: MAX_SUPPORTED_PAYLOAD_BYTES,
        })?;
        Ok(Self {
            header: FrameHeader::new(kind, flags, len),
            payload,
        })
    }

    /// Builds a frame by encoding `message` as its payload.
    ///
    /// # Errors
    ///
    /// As [`encode_message`].
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{Frame, FrameFlags, FrameKind, FrameLimits};
    ///
    /// let frame = Frame::from_message(
    ///     FrameKind::Control,
    ///     FrameFlags::EMPTY,
    ///     &42u16,
    ///     &FrameLimits::default(),
    /// )?;
    /// assert_eq!(frame.as_view().decode::<u16>()?, 42);
    /// # Ok::<(), astrs_wire::WireError>(())
    /// ```
    pub fn from_message<T: WireEncode>(
        kind: FrameKind,
        flags: FrameFlags,
        message: &T,
        limits: &FrameLimits,
    ) -> WireResult<Self> {
        reject_compression(flags)?;
        let hint = message.encode_size_hint()?;
        limits.check_payload_len(hint)?;
        let mut payload = Vec::with_capacity(hint);
        message.encode_presized(&mut payload)?;
        Self::new(kind, flags, payload)
    }

    /// The header.
    #[must_use]
    pub const fn header(&self) -> FrameHeader {
        self.header
    }

    /// The message family.
    #[must_use]
    pub const fn kind(&self) -> FrameKind {
        self.header.kind()
    }

    /// The flags byte.
    #[must_use]
    pub const fn flags(&self) -> FrameFlags {
        self.header.flags()
    }

    /// The payload bytes.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Consumes the frame and returns its payload buffer.
    #[must_use]
    pub fn into_payload(self) -> Vec<u8> {
        self.payload
    }

    /// A borrowed view of this frame.
    #[must_use]
    pub fn as_view(&self) -> FrameView<'_> {
        FrameView {
            header: self.header,
            payload: &self.payload,
        }
    }

    /// Decodes the payload as `T`, rejecting trailing bytes.
    ///
    /// # Errors
    ///
    /// As [`FrameView::decode`].
    pub fn decode<T: WireDecode>(&self) -> WireResult<T> {
        self.as_view().decode()
    }

    /// Appends this frame's wire bytes to `out`.
    ///
    /// # Errors
    ///
    /// [`WireError::FrameTooLarge`] if the payload exceeds `limits`.
    ///
    /// # Examples
    ///
    /// ```
    /// use astrs_wire::{Frame, FrameFlags, FrameKind, FrameLimits};
    ///
    /// let frame = Frame::new(FrameKind::Data, FrameFlags::EMPTY, vec![9])?;
    /// let mut out = Vec::new();
    /// let written = frame.encode_into(&mut out, &FrameLimits::default())?;
    /// assert_eq!(written, out.len());
    /// # Ok::<(), astrs_wire::WireError>(())
    /// ```
    pub fn encode_into(&self, out: &mut Vec<u8>, limits: &FrameLimits) -> WireResult<usize> {
        write_frame(out, self.kind(), self.flags(), &self.payload, limits)
    }

    /// Encodes this frame into a fresh buffer.
    ///
    /// # Errors
    ///
    /// As [`Frame::encode_into`].
    pub fn encode(&self, limits: &FrameLimits) -> WireResult<Vec<u8>> {
        let mut out = Vec::with_capacity(self.header.total_len());
        self.encode_into(&mut out, limits)?;
        Ok(out)
    }
}

/// Rejects flags that claim a compression this crate does not apply.
///
/// Compression is negotiated per route and applied by `astrs-transport`
/// (§6.4); a caller that hands this crate an uncompressed message *and* a
/// compression flag would produce a frame no peer could read.
fn reject_compression(flags: FrameFlags) -> WireResult<()> {
    let compression = flags.compression();
    if compression.is_enabled() {
        return Err(WireError::CompressedPayload { compression });
    }
    Ok(())
}

/// Appends a complete frame with an already-encoded payload to `out`.
///
/// This is the entry point transports use once they have (possibly
/// compressed) payload bytes in hand. Callers with a message value should
/// prefer [`write_message`], which encodes straight into `out` with no
/// intermediate buffer.
///
/// Returns the number of bytes appended. On failure `out` is restored to its
/// original length.
///
/// # Errors
///
/// [`WireError::FrameTooLarge`] if the payload exceeds `limits`.
///
/// # Examples
///
/// ```
/// use astrs_wire::{FrameFlags, FrameKind, FrameLimits, write_frame};
///
/// let mut out = Vec::new();
/// let written = write_frame(
///     &mut out,
///     FrameKind::Data,
///     FrameFlags::CRC,
///     b"payload",
///     &FrameLimits::default(),
/// )?;
/// assert_eq!(written, 10 + 7 + 4);
/// # Ok::<(), astrs_wire::WireError>(())
/// ```
pub fn write_frame(
    out: &mut Vec<u8>,
    kind: FrameKind,
    flags: FrameFlags,
    payload: &[u8],
    limits: &FrameLimits,
) -> WireResult<usize> {
    limits.check_payload_len(payload.len())?;
    let len = u32::try_from(payload.len()).map_err(|_| WireError::FrameTooLarge {
        len: payload.len(),
        max: MAX_SUPPORTED_PAYLOAD_BYTES,
    })?;

    let header = FrameHeader::new(kind, flags, len);
    let header_bytes = header.to_bytes();

    out.reserve(header.total_len());
    out.extend_from_slice(&header_bytes);
    out.extend_from_slice(payload);

    if flags.has_crc() {
        let checksum = Crc32c::new().chain(&header_bytes).chain(payload).finalize();
        out.extend_from_slice(&checksum.to_le_bytes());
    }

    Ok(header.total_len())
}

/// Encodes `message` directly into `out` as a complete frame.
///
/// The header is written as a placeholder, the payload is encoded into the
/// same buffer, and the header is then backfilled with the measured length —
/// so a message never passes through an intermediate payload buffer.
///
/// Returns the number of bytes appended. On failure `out` is restored to its
/// original length.
///
/// # Errors
///
/// - [`WireError::CompressedPayload`] if `flags` request compression (this
///   crate never compresses; hand pre-compressed bytes to [`write_frame`]).
/// - [`WireError::FrameTooLarge`] if the encoded payload exceeds `limits`.
/// - [`WireError::Codec`] or [`WireError::SizeHintMismatch`] from the codec.
///
/// # Examples
///
/// ```
/// use astrs_wire::{FrameFlags, FrameKind, FrameLimits, decode_frame, write_message};
///
/// let limits = FrameLimits::default();
/// let mut out = Vec::new();
/// write_message(&mut out, FrameKind::NodeEvent, FrameFlags::CRC, &"tick".to_owned(), &limits)?;
/// assert_eq!(decode_frame(&out, &limits)?.decode::<String>()?, "tick");
/// # Ok::<(), astrs_wire::WireError>(())
/// ```
pub fn write_message<T: WireEncode>(
    out: &mut Vec<u8>,
    kind: FrameKind,
    flags: FrameFlags,
    message: &T,
    limits: &FrameLimits,
) -> WireResult<usize> {
    reject_compression(flags)?;

    // Refuse before encoding, so an oversize message is never materialised.
    let hint = message.encode_size_hint()?;
    limits.check_payload_len(hint)?;

    let start = out.len();
    out.reserve(HEADER_LEN + hint + CRC_LEN);
    out.extend_from_slice(&[0u8; HEADER_LEN]);

    let payload_len = match message.encode_presized(out) {
        Ok(len) => len,
        Err(err) => {
            out.truncate(start);
            return Err(err);
        }
    };

    // The hint is contractually exact, but the limit is re-checked against the
    // real length so a broken hint cannot smuggle an oversize frame through.
    if let Err(err) = limits.check_payload_len(payload_len) {
        out.truncate(start);
        return Err(err);
    }
    let len = match u32::try_from(payload_len) {
        Ok(len) => len,
        Err(_) => {
            out.truncate(start);
            return Err(WireError::FrameTooLarge {
                len: payload_len,
                max: MAX_SUPPORTED_PAYLOAD_BYTES,
            });
        }
    };

    let header = FrameHeader::new(kind, flags, len);
    let backfill = out
        .get_mut(start..)
        .map(|framed| header.write_into(framed))
        .unwrap_or(Err(WireError::Truncated {
            expected: HEADER_LEN,
            found: 0,
        }));
    if let Err(err) = backfill {
        out.truncate(start);
        return Err(err);
    }

    if flags.has_crc() {
        let checksum = out.get(start..start + HEADER_LEN + payload_len).map(crc32c);
        match checksum {
            Some(checksum) => out.extend_from_slice(&checksum.to_le_bytes()),
            None => {
                let found = out.len() - start;
                out.truncate(start);
                return Err(WireError::Truncated {
                    expected: HEADER_LEN + payload_len,
                    found,
                });
            }
        }
    }

    Ok(out.len() - start)
}

/// Encodes a complete frame with an already-encoded payload into a fresh
/// buffer.
///
/// # Errors
///
/// As [`write_frame`].
///
/// # Examples
///
/// ```
/// use astrs_wire::{FrameFlags, FrameKind, FrameLimits, encode_frame};
///
/// let bytes = encode_frame(FrameKind::Log, FrameFlags::EMPTY, b"x", &FrameLimits::default())?;
/// assert_eq!(bytes.len(), 11);
/// # Ok::<(), astrs_wire::WireError>(())
/// ```
pub fn encode_frame(
    kind: FrameKind,
    flags: FrameFlags,
    payload: &[u8],
    limits: &FrameLimits,
) -> WireResult<Vec<u8>> {
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len() + CRC_LEN);
    write_frame(&mut out, kind, flags, payload, limits)?;
    Ok(out)
}

/// Encodes a message as a complete frame in a fresh buffer.
///
/// # Errors
///
/// As [`write_message`].
///
/// # Examples
///
/// ```
/// use astrs_wire::{FrameFlags, FrameKind, FrameLimits, encode_message};
///
/// let bytes = encode_message(FrameKind::Data, FrameFlags::CRC, &1u8, &FrameLimits::default())?;
/// assert_eq!(bytes.len(), 10 + 1 + 4);
/// # Ok::<(), astrs_wire::WireError>(())
/// ```
pub fn encode_message<T: WireEncode>(
    kind: FrameKind,
    flags: FrameFlags,
    message: &T,
    limits: &FrameLimits,
) -> WireResult<Vec<u8>> {
    let mut out = Vec::new();
    write_message(&mut out, kind, flags, message, limits)?;
    Ok(out)
}

/// Decodes exactly one frame that fills `buf` completely.
///
/// # Errors
///
/// - [`WireError::Truncated`] if `buf` stops short of a complete frame.
/// - [`WireError::TrailingBytes`] if bytes remain after the frame — use
///   [`decode_frame_prefix`] for stream buffers that legitimately hold more.
/// - Everything [`FrameHeader::parse`] can raise.
/// - [`WireError::FrameTooLarge`] if the declared length exceeds `limits`
///   (checked before any buffer is sized).
/// - [`WireError::MissingCrc`] if `limits` require a checksum and the frame
///   carries none.
/// - [`WireError::CrcMismatch`] if the checksum does not verify.
///
/// # Examples
///
/// ```
/// use astrs_wire::{FrameFlags, FrameKind, FrameLimits, decode_frame, encode_frame};
///
/// let limits = FrameLimits::default();
/// let mut bytes = encode_frame(FrameKind::Data, FrameFlags::EMPTY, b"ab", &limits)?;
/// assert_eq!(decode_frame(&bytes, &limits)?.payload(), b"ab");
///
/// bytes.push(0);
/// assert!(decode_frame(&bytes, &limits).is_err());
/// # Ok::<(), astrs_wire::WireError>(())
/// ```
pub fn decode_frame<'a>(buf: &'a [u8], limits: &FrameLimits) -> WireResult<FrameView<'a>> {
    let view = decode_frame_prefix(buf, limits)?;
    let consumed = view.total_len();
    if consumed != buf.len() {
        return Err(WireError::TrailingBytes {
            consumed,
            trailing: buf.len() - consumed,
        });
    }
    Ok(view)
}

/// Decodes the frame at the front of `buf`, tolerating trailing bytes.
///
/// This is the primitive a buffered stream reader uses: it returns a view of
/// the first complete frame, and [`FrameView::total_len`] says how far to
/// advance.
///
/// # Errors
///
/// As [`decode_frame`], minus [`WireError::TrailingBytes`].
///
/// # Examples
///
/// ```
/// use astrs_wire::{FrameFlags, FrameKind, FrameLimits, decode_frame_prefix, write_frame};
///
/// let limits = FrameLimits::default();
/// let mut stream = Vec::new();
/// write_frame(&mut stream, FrameKind::Data, FrameFlags::EMPTY, b"one", &limits)?;
/// write_frame(&mut stream, FrameKind::Log, FrameFlags::EMPTY, b"two", &limits)?;
///
/// let first = decode_frame_prefix(&stream, &limits)?;
/// assert_eq!(first.payload(), b"one");
/// let second = decode_frame_prefix(&stream[first.total_len()..], &limits)?;
/// assert_eq!(second.payload(), b"two");
/// # Ok::<(), astrs_wire::WireError>(())
/// ```
pub fn decode_frame_prefix<'a>(buf: &'a [u8], limits: &FrameLimits) -> WireResult<FrameView<'a>> {
    let header = FrameHeader::parse(buf)?;

    // Bound the declared length *before* touching a buffer of that size.
    let payload_len = header.payload_len_usize()?;
    limits.check_payload_len(payload_len)?;

    if limits.require_crc() && !header.has_crc() {
        return Err(WireError::MissingCrc);
    }

    let total = header.total_len();
    if buf.len() < total {
        return Err(WireError::Truncated {
            expected: total,
            found: buf.len(),
        });
    }

    let payload = buf
        .get(HEADER_LEN..HEADER_LEN + payload_len)
        .ok_or(WireError::Truncated {
            expected: total,
            found: buf.len(),
        })?;

    if header.has_crc() {
        let checked = buf
            .get(..HEADER_LEN + payload_len)
            .ok_or(WireError::Truncated {
                expected: total,
                found: buf.len(),
            })?;
        let trailer = buf
            .get(HEADER_LEN + payload_len..total)
            .ok_or(WireError::Truncated {
                expected: total,
                found: buf.len(),
            })?;
        let expected = u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
        let computed = crc32c(checked);
        if expected != computed {
            return Err(WireError::CrcMismatch { expected, computed });
        }
    }

    Ok(FrameView { header, payload })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn limits() -> FrameLimits {
        FrameLimits::default()
    }

    #[test]
    fn round_trips_an_empty_payload() {
        for flags in [FrameFlags::EMPTY, FrameFlags::CRC] {
            let bytes = encode_frame(FrameKind::Control, flags, b"", &limits()).unwrap();
            assert_eq!(
                bytes.len(),
                HEADER_LEN + if flags.has_crc() { 4 } else { 0 }
            );
            let view = decode_frame(&bytes, &limits()).unwrap();
            assert!(view.payload().is_empty());
            assert_eq!(view.kind(), FrameKind::Control);
            assert_eq!(view.flags(), flags);
        }
    }

    #[test]
    fn round_trips_every_kind() {
        for &kind in FrameKind::ALL {
            let bytes = encode_frame(kind, FrameFlags::CRC, b"payload", &limits()).unwrap();
            let view = decode_frame(&bytes, &limits()).unwrap();
            assert_eq!(view.kind(), kind);
            assert_eq!(view.payload(), b"payload");
        }
    }

    #[test]
    fn write_message_and_write_frame_agree_byte_for_byte() {
        let message = vec![1u8, 2, 3, 4, 5];
        let via_message =
            encode_message(FrameKind::Data, FrameFlags::CRC, &message, &limits()).unwrap();
        let payload = message.encode_to_vec().unwrap();
        let via_frame =
            encode_frame(FrameKind::Data, FrameFlags::CRC, &payload, &limits()).unwrap();
        assert_eq!(via_message, via_frame);
    }

    #[test]
    fn write_message_appends_and_restores_on_failure() {
        let tight = limits().with_max_payload_bytes(1);
        let mut out = b"prefix".to_vec();
        let err = write_message(
            &mut out,
            FrameKind::Data,
            FrameFlags::CRC,
            &vec![0u8; 64],
            &tight,
        )
        .unwrap_err();
        assert!(matches!(err, WireError::FrameTooLarge { .. }));
        assert_eq!(out, b"prefix", "buffer must be left untouched on failure");
    }

    #[test]
    fn multiple_frames_concatenate_and_split_cleanly() {
        let mut stream = Vec::new();
        let payloads: [&[u8]; 3] = [b"", b"a", b"longer payload"];
        for (index, payload) in payloads.iter().enumerate() {
            let flags = if index % 2 == 0 {
                FrameFlags::CRC
            } else {
                FrameFlags::EMPTY
            };
            write_frame(&mut stream, FrameKind::Data, flags, payload, &limits()).unwrap();
        }

        let mut rest = stream.as_slice();
        for payload in payloads {
            let view = decode_frame_prefix(rest, &limits()).unwrap();
            assert_eq!(view.payload(), payload);
            rest = &rest[view.total_len()..];
        }
        assert!(rest.is_empty());
    }

    #[test]
    fn decode_frame_rejects_trailing_bytes() {
        let mut bytes = encode_frame(FrameKind::Log, FrameFlags::EMPTY, b"x", &limits()).unwrap();
        bytes.extend_from_slice(b"junk");
        match decode_frame(&bytes, &limits()) {
            Err(WireError::TrailingBytes { consumed, trailing }) => {
                assert_eq!(consumed, HEADER_LEN + 1);
                assert_eq!(trailing, 4);
            }
            other => panic!("expected TrailingBytes, got {other:?}"),
        }
    }

    #[test]
    fn truncation_is_detected_at_every_prefix() {
        let bytes = encode_frame(FrameKind::Data, FrameFlags::CRC, b"hello", &limits()).unwrap();
        for len in 0..bytes.len() {
            let err = decode_frame_prefix(&bytes[..len], &limits()).unwrap_err();
            assert!(err.is_incomplete(), "length {len} gave {err:?}");
        }
        assert!(decode_frame_prefix(&bytes, &limits()).is_ok());
    }

    #[test]
    fn oversize_declared_length_is_refused_before_allocation() {
        let tight = limits().with_max_payload_bytes(8);
        // Hand-build a header claiming 4 GiB of payload with nothing behind it.
        let header = FrameHeader::new(FrameKind::Data, FrameFlags::EMPTY, u32::MAX);
        let bytes = header.to_bytes();
        match decode_frame_prefix(&bytes, &tight) {
            Err(WireError::FrameTooLarge { len, max }) => {
                assert_eq!(len, u32::MAX as usize);
                assert_eq!(max, 8);
            }
            other => panic!("expected FrameTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn encode_refuses_payloads_over_the_limit() {
        let tight = limits().with_max_payload_bytes(4);
        let err = encode_frame(FrameKind::Data, FrameFlags::EMPTY, b"12345", &tight).unwrap_err();
        assert!(matches!(err, WireError::FrameTooLarge { len: 5, max: 4 }));
    }

    #[test]
    fn crc_detects_every_single_bit_flip_in_header_or_payload() {
        let bytes =
            encode_frame(FrameKind::Data, FrameFlags::CRC, b"integrity", &limits()).unwrap();
        // Skip the magic bytes and version, whose corruption is caught earlier
        // by dedicated errors; everything from `flags` on must be covered by
        // the checksum.
        for index in FLAGS_OFFSET..bytes.len() {
            for bit in 0..8u8 {
                let mut corrupted = bytes.clone();
                corrupted[index] ^= 1 << bit;
                if corrupted == bytes {
                    continue;
                }
                let result = decode_frame(&corrupted, &limits());
                assert!(
                    result.is_err(),
                    "flipping bit {bit} of byte {index} was not detected"
                );
                let err = match result {
                    Err(err) => err,
                    Ok(_) => unreachable!(),
                };
                assert!(err.is_protocol_violation(), "unexpected error {err:?}");
            }
        }
    }

    #[test]
    fn crc_mismatch_reports_both_checksums() {
        let mut bytes =
            encode_frame(FrameKind::Data, FrameFlags::CRC, b"payload", &limits()).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0xFF;
        match decode_frame(&bytes, &limits()) {
            Err(WireError::CrcMismatch { expected, computed }) => assert_ne!(expected, computed),
            other => panic!("expected CrcMismatch, got {other:?}"),
        }
    }

    #[test]
    fn network_limits_reject_a_frame_without_a_checksum() {
        let bytes = encode_frame(FrameKind::Data, FrameFlags::EMPTY, b"x", &limits()).unwrap();
        assert!(matches!(
            decode_frame(&bytes, &FrameLimits::network()),
            Err(WireError::MissingCrc)
        ));
        // The same bytes are fine on a UDS leg.
        assert!(decode_frame(&bytes, &FrameLimits::uds()).is_ok());
    }

    #[test]
    fn compression_flags_are_refused_by_the_message_encoder() {
        for compression in [Compression::Lz4, Compression::Zstd] {
            let flags = FrameFlags::EMPTY.with_compression(compression);
            let err = encode_message(FrameKind::Data, flags, &1u8, &limits()).unwrap_err();
            assert!(matches!(
                err,
                WireError::CompressedPayload { compression: c } if c == compression
            ));
        }
    }

    #[test]
    fn compressed_payloads_are_framed_but_not_decoded() {
        let flags = FrameFlags::CRC.with_compression(Compression::Zstd);
        let bytes = encode_frame(FrameKind::Data, flags, b"opaque", &limits()).unwrap();
        let view = decode_frame(&bytes, &limits()).unwrap();
        assert_eq!(view.payload(), b"opaque");
        assert!(matches!(
            view.decode::<Vec<u8>>(),
            Err(WireError::CompressedPayload {
                compression: Compression::Zstd
            })
        ));
    }

    #[test]
    fn expect_kind_matches_and_mismatches() {
        let bytes = encode_frame(FrameKind::Log, FrameFlags::EMPTY, b"", &limits()).unwrap();
        let view = decode_frame(&bytes, &limits()).unwrap();
        assert!(view.expect_kind(FrameKind::Log).is_ok());
        match view.expect_kind(FrameKind::Data) {
            Err(WireError::KindMismatch { expected, found }) => {
                assert_eq!(expected, FrameKind::Data);
                assert_eq!(found, FrameKind::Log);
            }
            other => panic!("expected KindMismatch, got {other:?}"),
        }
    }

    #[test]
    fn decode_as_checks_the_family_first() {
        let bytes = encode_message(FrameKind::Log, FrameFlags::EMPTY, &5u8, &limits()).unwrap();
        let view = decode_frame(&bytes, &limits()).unwrap();
        assert_eq!(view.decode_as::<u8>(FrameKind::Log).unwrap(), 5);
        assert!(view.decode_as::<u8>(FrameKind::Data).is_err());
    }

    #[test]
    fn owned_and_borrowed_frames_agree() {
        let frame = Frame::new(FrameKind::Data, FrameFlags::CRC, b"body".to_vec()).unwrap();
        let bytes = frame.encode(&limits()).unwrap();
        let view = decode_frame(&bytes, &limits()).unwrap();
        assert_eq!(view.to_owned_frame(), frame);
        assert_eq!(frame.as_view(), view);
        assert_eq!(frame.payload(), b"body");
        assert_eq!(frame.clone().into_payload(), b"body".to_vec());
    }

    #[test]
    fn frame_from_message_round_trips() {
        let frame = Frame::from_message(
            FrameKind::NodeRequest,
            FrameFlags::CRC,
            &"hello".to_owned(),
            &limits(),
        )
        .unwrap();
        assert_eq!(frame.decode::<String>().unwrap(), "hello");
        assert_eq!(frame.kind(), FrameKind::NodeRequest);
        assert_eq!(frame.flags(), FrameFlags::CRC);
    }

    #[test]
    fn frame_encode_into_appends() {
        let frame = Frame::new(FrameKind::Data, FrameFlags::EMPTY, vec![7]).unwrap();
        let mut out = b"head".to_vec();
        let written = frame.encode_into(&mut out, &limits()).unwrap();
        assert_eq!(written, HEADER_LEN + 1);
        assert_eq!(&out[..4], b"head");
        assert_eq!(decode_frame(&out[4..], &limits()).unwrap().payload(), &[7]);
    }

    #[test]
    fn payload_at_exactly_the_limit_is_accepted() {
        let tight = limits().with_max_payload_bytes(16);
        let payload = [0xABu8; 16];
        let bytes = encode_frame(FrameKind::Data, FrameFlags::CRC, &payload, &tight).unwrap();
        assert_eq!(decode_frame(&bytes, &tight).unwrap().payload(), &payload);
    }

    #[test]
    fn a_large_payload_round_trips() {
        let payload = vec![0x5Au8; 1 << 20];
        let bytes = encode_frame(FrameKind::Data, FrameFlags::CRC, &payload, &limits()).unwrap();
        let view = decode_frame(&bytes, &limits()).unwrap();
        assert_eq!(view.payload().len(), payload.len());
        assert_eq!(view.payload(), payload.as_slice());
    }
}
